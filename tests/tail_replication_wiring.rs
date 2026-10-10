//! B3 — off-box replication of the unsealed tail
//! ([noetl/ai-meta#460](https://github.com/noetl/ai-meta/issues/460)).
//!
//! ⚠⚠ Same two-halves trap B2 has, and ehdb's doc on `replicate_tail` states
//! it: the config flag replicates nothing without a driver calling it on a
//! timer. A flag that is set while nothing ticks reads as a correct
//! configuration from every angle and moves no bytes.
//!
//! B2 bounds the window in which records live on one disk; it cannot close it,
//! because a part is local-only until it seals. These tests exist so B3 cannot
//! regress to one half, and so the number it buys is never overstated.

/// Both halves must ship together.
#[test]
fn the_tail_replicator_has_both_halves() {
    let embedded = include_str!("../src/handlers/ehdb_embedded.rs");
    let main_rs = include_str!("../src/main.rs");

    assert!(
        embedded.contains("with_tail_replication"),
        "half 1 missing: the engine is never configured for tail replication"
    );
    assert!(
        embedded.contains("prepare_tail_batches()"),
        "half 2a missing: nothing prepares a tail batch"
    );
    assert!(
        embedded.contains("upload_tail_batch("),
        "half 2a missing: nothing uploads a prepared batch"
    );
    assert!(
        embedded.contains("commit_tail_batch("),
        "half 2a missing: nothing commits the watermark, so the same records would be \
         re-sent forever"
    );
    assert!(
        embedded.contains("pub fn spawn_tail_replication_task"),
        "half 2b missing: there is no timer task to call it from"
    );
    assert!(
        main_rs.contains("spawn_tail_replication_task("),
        "⚠⚠ THE TIMER IS NEVER SPAWNED. `NOETL_EHDB_TAIL_REPLICATION` would be set, the \
         config would look correct, and not one tail record would leave the node. Same \
         shape as the B2 half-wiring, and just as invisible."
    );
}

/// The tick interval is the loss window, so the code must say so where someone
/// setting it will read it.
#[test]
fn the_tick_is_documented_as_the_loss_window() {
    let embedded = include_str!("../src/handlers/ehdb_embedded.rs");
    assert!(
        embedded.contains("IS the loss window"),
        "the interval reader must state that the tick IS the window — otherwise B3 reads \
         as 'the tail is replicated', which overstates it the way 'no data loss' would"
    );
    assert!(
        embedded.contains("NOT zero") || embedded.contains("not zero"),
        "the honest bound must be stated at the call site"
    );
}

/// ⚠ A failing replicator and an idle one both leave the batch count flat, so
/// the failure needs its own counter — and it must be pinned, or its absence
/// reads as a healthy zero.
#[test]
fn the_failure_counter_exists_and_is_pinned_unconditionally() {
    let metrics = include_str!("../src/metrics.rs");
    let embedded = include_str!("../src/handlers/ehdb_embedded.rs");

    assert!(
        metrics.contains("noetl_ehdb_tail_failed_total"),
        "no counter distinguishes a failing replicator from an idle one"
    );
    assert!(
        metrics.contains("pub fn init_tail_replication_series"),
        "the series are never pinned, so they are ABSENT until something fires and an \
         absent series reads like a healthy zero"
    );
    // The pin must not sit inside the enabled branch: the configuration whose
    // zero matters most is the one with replication OFF.
    let init_at = embedded
        .find("init_tail_replication_series()")
        .expect("the task pins the series");
    let enabled_at = embedded
        .find("let enabled = tail_replication_enabled();")
        .expect("the task reads the flag");
    assert!(
        init_at > enabled_at,
        "sanity: the pin is expected after the flag read in the same function"
    );
    // ...and before any branch on it.
    let branch_at = embedded[init_at..]
        .find("if enabled {")
        .map(|o| o + init_at)
        .expect("the task logs differently per flag state");
    assert!(
        init_at < branch_at,
        "⚠ the pin is inside a flag branch — server#315's reconcile-skip reasons were \
         pinned inside `if event_bus_mode.publishes_ehdb()` and were therefore absent on \
         exactly the configuration whose zero someone would be reading"
    );
}

/// ⚠⚠ **The remote put must NOT happen under the engine lock.**
///
/// `L0Engine::replicate_tail()` does prepare → upload → commit in one call, so
/// calling it from the driver would hold the engine across the put. That same
/// lock is taken by `shadow_append`, which runs inside `emit_events` on the
/// **live write path** — so every append colliding with a tick would wait out
/// the put: p50 77 ms, p99 234 ms measured against GCS. A durability feature
/// would have become a write-path latency regression.
///
/// ehdb's uploader thread documents the rule it follows — "the substrate writes
/// happen OUTSIDE the lock so a slow store never blocks appends/reads" — and
/// this guard keeps the tail replicator honest about it.
#[test]
fn the_driver_does_not_hold_the_engine_lock_across_the_remote_put() {
    let embedded = include_str!("../src/handlers/ehdb_embedded.rs");

    // Find the driver and look only inside it.
    let start = embedded
        .find("pub fn spawn_tail_replication_task")
        .expect("the driver exists");
    let end = embedded[start..]
        .find("\npub fn ")
        .map(|o| o + start)
        .unwrap_or(embedded.len());
    let driver = &embedded[start..end];
    // ⚠ Assert the extraction before asserting about it: a slice cut at the
    // wrong place would make the checks below pass over almost nothing.
    assert!(
        driver.len() > 800,
        "the extracted driver is only {} bytes — the checks below would be vacuous",
        driver.len()
    );

    assert!(
        !driver.contains("guard.replicate_tail()") && !driver.contains(".replicate_tail()"),
        "⚠ the driver calls the all-in-one `replicate_tail`, which performs the remote put \
         while holding the engine lock that the LIVE append path also takes"
    );
    assert!(
        driver.contains("prepare_tail_batches()") && driver.contains("upload_tail_batch("),
        "the driver must use the split API"
    );
    // The upload must sit between the two lock scopes, not inside either.
    let prep = driver.find("prepare_tail_batches()").unwrap();
    let upload = driver.find("upload_tail_batch(").unwrap();
    let commit = driver.find("commit_tail_batch(").unwrap();
    assert!(
        prep < upload && upload < commit,
        "ordering must be prepare → upload → commit (got {prep}, {upload}, {commit})"
    );
    assert!(
        driver.contains("lock released"),
        "the lock boundary must be stated where someone editing this will see it"
    );
}

/// ⚠⚠ **D2 cleanup defaults ON, which is the opposite of every other flag in
/// this module, and that asymmetry is deliberate.**
///
/// Omitting cleanup does not leave the system as it was — it leaves tail
/// objects accumulating in the replica bucket indefinitely. Whereas the
/// operation itself is safe by construction: a tail object is deleted only
/// when a **contiguous** run of durable parts already covers its whole
/// interval, so what goes is a redundant second copy of something already
/// off-box.
///
/// A default-off cleanup would mean "turn on replication" silently also means
/// "grow a bucket forever", which is the kind of coupling nobody discovers
/// until the bill or the listing gets strange.
#[test]
fn tail_cleanup_defaults_on_and_is_opt_out() {
    let embedded = include_str!("../src/handlers/ehdb_embedded.rs");
    let at = embedded
        .find("fn tail_cleanup_enabled")
        .expect("the cleanup flag reader exists");
    let body = &embedded[at..(at + 400).min(embedded.len())];
    assert!(
        body.contains("unwrap_or(false)") && body.starts_with("fn tail_cleanup_enabled")
            || body.contains('!'),
        "cleanup must default ON (opt-out), not off; body was: {body}"
    );
    // The reader must be a negation of an opt-out, not a plain opt-in.
    assert!(
        body.contains("!std::env::var"),
        "⚠ cleanup reads like an opt-IN flag. Default-off means enabling replication \
         silently also means growing the bucket forever: {body}"
    );
    assert!(
        embedded.contains("NOETL_EHDB_TAIL_CLEANUP"),
        "the flag must be nameable by an operator"
    );
}

/// The cleanup's listing and deletes are I/O, so they must run with the engine
/// lock released — the same rule the upload follows, for the same measured
/// reason (p50 77 ms / p99 234 ms, and the live append path takes that lock).
#[test]
fn the_cleanup_runs_with_the_engine_lock_released() {
    let embedded = include_str!("../src/handlers/ehdb_embedded.rs");
    let start = embedded
        .find("pub fn spawn_tail_replication_task")
        .expect("the driver exists");
    let end = embedded[start..]
        .find("\npub fn ")
        .map(|o| o + start)
        .unwrap_or(embedded.len());
    let driver = &embedded[start..end];
    assert!(
        driver.len() > 1200,
        "extracted driver is {} bytes — the checks below would be vacuous",
        driver.len()
    );

    let wm = driver
        .find("contiguous_durable_watermarks()")
        .expect("the watermark is read under the lock");
    let dropped = driver.find("drop(guard);").expect("the lock is released");
    let reclaim = driver
        .find("reclaim_superseded_tail_objects(")
        .expect("the reclaim is called");

    assert!(
        wm < dropped,
        "the watermark must be read BEFORE the lock is dropped (it reads the in-RAM manifest)"
    );
    assert!(
        dropped < reclaim,
        "⚠ the reclaim performs listing, sizing and deletes — all I/O — so it must run \
         AFTER the lock is released. Holding the engine across it would put the whole \
         object-store round trip in front of every append that collides with a tick."
    );
}

/// An operator must be able to tell a stalled reclaimer from an idle one.
#[test]
fn the_cleanup_reports_failures_and_retention_not_just_deletions() {
    let embedded = include_str!("../src/handlers/ehdb_embedded.rs");
    assert!(
        embedded.contains("reclaim.failed"),
        "delete failures must be surfaced: a reclaimer failing every delete and one with \
         nothing to delete both leave the reclaimed count flat"
    );
    assert!(
        embedded.contains("reclaim.unparsed"),
        "objects whose keys this build does not understand must be surfaced rather than \
         silently skipped — they are being left in an event-log bucket deliberately"
    );
    assert!(
        embedded.contains("retained = reclaim.retained"),
        "retention must be logged alongside deletions, so a stalled seal/upload is visible"
    );
}
