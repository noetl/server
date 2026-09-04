//! Prometheus metrics surface for the NoETL control plane.
//!
//! Follows `agents/rules/observability.md` Principles 1 and 2:
//!
//! - Every substantive change ships a counter and/or histogram
//!   alongside the code (Principle 1).
//! - Counters / histograms / gauges scale; per-event INFO logs
//!   do not (Principle 2).
//!
//! The registry is global (`OnceLock<Registry>`) so any module
//! can record without threading a handle through `AppState`.
//! `gather_text()` renders the registry into the standard
//! Prometheus text exposition format used by `/metrics`.
//!
//! ## Per-endpoint conventions
//!
//! - **Counters** are named with a trailing `_total` suffix
//!   (Prometheus convention).
//! - **Histograms** are named with a unit suffix
//!   (`_seconds`, `_bytes`, etc.) — never raw.
//! - **Labels** are low-cardinality enums (`event_type`,
//!   `status`).  `execution_id` is NEVER a label (cardinality
//!   blows up the registry); it lives on tracing spans only
//!   per Principle 4.
//!
//! ## Round 1 surface
//!
//! - `noetl_events_ingested_total{event_type, status}` —
//!   counter; one increment per `POST /api/events` call.
//!   `event_type` is a meaningful breakdown (15+ values) so it
//!   warrants its own metric.
//! - `noetl_event_ingest_duration_seconds{event_type}` —
//!   histogram; the wall-clock time spent inside the handler.
//!
//! ## Round 2 surface (the other 5 write endpoints)
//!
//! The remaining Phase B POST endpoints each have a single
//! mode of operation (catalog/register = upsert, credentials =
//! upsert, keychain = set, etc.) so they share a generic pair:
//!
//! - `noetl_write_requests_total{endpoint, status}` — counter.
//! - `noetl_write_request_duration_seconds{endpoint}` —
//!   histogram.
//!
//! `endpoint` label values (low-cardinality enum):
//! - `catalog_register`
//! - `credentials_upsert`
//! - `keychain_set`
//! - `runtime_register`
//! - `runtime_heartbeat`
//!
//! See noetl/server#21 for the round breakdown.

use std::sync::OnceLock;

use prometheus::{
    Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, Opts,
    Registry, TextEncoder,
};

/// Bucket boundaries for the event-ingest histogram (seconds).
///
/// Spans the 1ms–10s range an event-ingest call could plausibly
/// take (DB write + optional engine call + result-store fallback).
/// Wider buckets at the tail capture the rare slow paths without
/// overweighting the high-percentile estimate.
const EVENT_INGEST_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Global registry — lazily initialised on first `registry()` call.
fn registry() -> &'static Registry {
    static REG: OnceLock<Registry> = OnceLock::new();
    REG.get_or_init(Registry::new)
}

/// Counter: `POST /api/events` calls bucketed by event type and status.
pub fn events_ingested_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_events_ingested_total",
                "Total events accepted by POST /api/events (incremented once per handler call, whether the body persisted or errored).",
            ),
            &["event_type", "status"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Counter: rows purged by `POST /api/internal/cleanup/purge`, bucketed by
/// the `noetl.*` table they were purged from.  Lets retention runs be
/// observed without per-delete log lines (noetl/ai-meta#96).
pub fn cleanup_rows_purged_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_cleanup_rows_purged_total",
                "Total rows deleted by the scheduled-cleanup internal endpoint, by table.",
            ),
            &["table"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record the rows purged from one table by a cleanup run.
pub fn record_cleanup_purged(table: &str, rows: u64) {
    cleanup_rows_purged_total()
        .with_label_values(&[table])
        .inc_by(rows);
}

/// Counter: worker-driven orchestrate drive events, by stage (`dispatched` —
/// the server issued the orchestrate command to the pool; `applied` — its
/// result was applied; `decode_error` — the worker's result couldn't be
/// decoded; `skipped_in_flight` — a drive was already running). The drive loop's
/// health at a glance (noetl/ai-meta#108).
pub fn orchestrate_drive_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_orchestrate_drive_total",
                "Worker-driven orchestrate drive events, by stage.",
            ),
            &["stage"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one worker-driven drive event (`dispatched` / `applied` /
/// `decode_error` / `skipped_in_flight`).
pub fn record_orchestrate_drive(stage: &str) {
    orchestrate_drive_total().with_label_values(&[stage]).inc();
}

// ── Orphaned-command guardrail sweep (zombie-exec fix; refs #154/#161/#163) ───

/// `noetl_orphan_sweep_total{outcome}` — outcomes of the orphaned-command
/// sweep ([`crate::handlers::orphan_sweep`]).  `outcome` is one of: `candidate`
/// (a RUNNING exec with an outstanding claimed command was examined),
/// `terminated` (owner worker dead → `playbook.failed` emitted), `skipped_live`
/// (owner still a live worker — never failed), `capped` (eligible orphan
/// deferred to a later tick by the rate limit), `error` (scan / emit failure).
/// Zero increments while the sweep is off (`NOETL_ORPHAN_SWEEP_ENABLED=false`).
pub fn orphan_sweep_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_orphan_sweep_total",
                "Orphaned-command guardrail sweep outcomes, by outcome.",
            ),
            &["outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one orphaned-command sweep outcome (see [`orphan_sweep_total`]).
pub fn record_orphan_sweep(outcome: &str) {
    orphan_sweep_total().with_label_values(&[outcome]).inc();
}

/// The five `outcome` values, kept in sync with the `record_orphan_sweep`
/// call sites in [`crate::handlers::orphan_sweep`] (all string literals).
pub const ORPHAN_SWEEP_OUTCOMES: [&str; 5] =
    ["candidate", "terminated", "skipped_live", "capped", "error"];

/// Materialise every [`ORPHAN_SWEEP_OUTCOMES`] series at 0.
///
/// Same reasoning as [`init_nonconvergence_sweep_series`], and it lands on the
/// same operational question.  This guardrail also emits `playbook.failed`, and
/// noetl/ai-meta#227 describes it re-issuing against permanently stalled
/// executions on an otherwise idle cluster — a loop that was detected only by
/// watching `ehdb_feed_shard_committed` climb, because these counters were
/// absent rather than zero.  "Is the guardrail doing anything, and is it
/// terminating things?" should be one scrape, not an inference from a shard
/// cursor.
/// `noetl_reconcile_giveup_total{reason}` — executions the reconcile poller
/// stopped re-driving (noetl/ai-meta#315).
///
/// `reason` is `max_noops`: the execution did not advance for
/// `NOETL_RECONCILE_MAX_NOOPS` consecutive polls.
///
/// ⚠ Pinned to 0 at startup by [`init_reconcile_giveup_series`]. A labelled
/// family is **pruned while empty** by `Registry::gather`, so without the pin a
/// healthy server and a server too old to carry this metric would both serve
/// nothing — which is the exact ambiguity that made the 2026-09-01 dispatch
/// outage invisible for hours.
pub fn reconcile_giveup_total() -> &'static prometheus::IntCounterVec {
    static M: std::sync::OnceLock<prometheus::IntCounterVec> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        let m = prometheus::IntCounterVec::new(
            prometheus::Opts::new(
                "noetl_reconcile_giveup_total",
                "Executions the reconcile poller stopped re-driving because they were not advancing (noetl/ai-meta#315).",
            ),
            &["reason"],
        )
        .expect("static counter spec must be valid");
        // ⚠ `registry()`, NOT `prometheus::default_registry()`. `gather_text()`
        // gathers from this crate's own registry, so a metric registered on the
        // default one is never scraped — it exists, it increments, and it is
        // invisible. Shipped that way in v3.99.3 (noetl/ai-meta#315): the cap
        // worked while the counter proving it could not appear.
        registry()
            .register(Box::new(m.clone()))
            .expect("counter registration must succeed");
        m
    })
}

pub const RECONCILE_GIVEUP_REASONS: &[&str] = &["max_noops"];

pub fn init_reconcile_giveup_series() {
    for reason in RECONCILE_GIVEUP_REASONS {
        reconcile_giveup_total()
            .with_label_values(&[reason])
            .inc_by(0);
    }
}

pub fn record_reconcile_giveup(reason: &str) {
    reconcile_giveup_total().with_label_values(&[reason]).inc();
}

#[cfg(test)]
mod reconcile_giveup_registry_tests {
    /// The metric must be on the registry `/metrics` actually gathers.
    ///
    /// ⚠ This is the test that was missing. v3.99.3 shipped the giveup counter
    /// registered on `prometheus::default_registry()` while `gather_text()`
    /// gathers from this crate's own `registry()`, so the series could never
    /// appear on the scrape — the cap worked and the counter proving it was
    /// invisible. Every test written for it was a pure-function test of the
    /// decision logic, which is exactly why none of them caught it.
    #[test]
    fn the_giveup_series_reaches_the_scrape() {
        super::init_reconcile_giveup_series();
        let text = super::gather_text().expect("gather");
        assert!(
            text.contains("noetl_reconcile_giveup_total"),
            "the giveup counter is not on the registry gather_text() reads, so it can \
             never be scraped — registering it is not the same as exposing it"
        );
        assert!(
            text.contains(r#"noetl_reconcile_giveup_total{reason="max_noops"} 0"#),
            "the series must be PINNED at 0: a labelled family is pruned while empty, so \
             without the pin a healthy server and one too old to carry the metric serve \
             the same nothing"
        );
    }
}

/// `noetl_db_pool_connections{state}` — the shared Postgres pool's occupancy.
///
/// ⚠ This pool had **no metric at all** until noetl/ai-meta#317. It is the one
/// resource that bounds the whole request path — with `shard_count <= 1` every
/// `/api/execute`, every read and all five background pollers draw from it —
/// and it was invisible. A pool exhaustion presents as requests hanging for the
/// full 30 s `acquire_timeout` with **no error logged**, because a waiting
/// `acquire` has nothing to say. That is indistinguishable from a network
/// problem unless you can see the pool.
///
/// `state` is `total` (connections the pool holds) or `idle` (of those, unused).
/// `total - idle` is in-flight; `total` pinned at the configured max with
/// `idle` at 0 is the exhaustion signature.
pub fn db_pool_connections() -> &'static prometheus::IntGaugeVec {
    static M: OnceLock<prometheus::IntGaugeVec> = OnceLock::new();
    M.get_or_init(|| {
        let g = prometheus::IntGaugeVec::new(
            Opts::new(
                "noetl_db_pool_connections",
                "Shared Postgres pool occupancy by state (noetl/ai-meta#317).",
            ),
            &["state"],
        )
        .expect("static gauge spec must be valid");
        // ⚠ `registry()`, not `prometheus::default_registry()` — `gather_text`
        // reads this crate's registry, and getting that wrong once already
        // shipped an invisible metric (noetl/ai-meta#315).
        registry()
            .register(Box::new(g.clone()))
            .expect("gauge registration must succeed");
        g
    })
}

pub const DB_POOL_STATES: &[&str] = &["total", "idle"];

/// Pin both series at 0 so an exhausted pool and a build without the metric are
/// distinguishable — a labelled family is pruned while empty.
/// Publishes whose retry moved off the request path, and how many later landed
/// (noetl/ai-meta#319 P3). `deferred - deferred_landed` is the number still
/// in flight or exhausted; pinned at 0 so absence is not mistaken for health.
pub fn ehdb_command_publish_deferred_total() -> &'static prometheus::IntCounter {
    static M: OnceLock<prometheus::IntCounter> = OnceLock::new();
    M.get_or_init(|| {
        let c = prometheus::IntCounter::new(
            "noetl_ehdb_command_publish_deferred_total",
            "Publishes whose retry budget continued off the request path (noetl/ai-meta#319).",
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(c.clone()))
            .expect("registration must succeed");
        c
    })
}

pub fn ehdb_command_publish_deferred_landed_total() -> &'static prometheus::IntCounter {
    static M: OnceLock<prometheus::IntCounter> = OnceLock::new();
    M.get_or_init(|| {
        let c = prometheus::IntCounter::new(
            "noetl_ehdb_command_publish_deferred_landed_total",
            "Deferred publishes that subsequently landed (noetl/ai-meta#319).",
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(c.clone()))
            .expect("registration must succeed");
        c
    })
}

pub fn record_ehdb_command_publish_deferred() {
    ehdb_command_publish_deferred_total().inc();
}
pub fn record_ehdb_command_publish_deferred_landed() {
    ehdb_command_publish_deferred_landed_total().inc();
}
pub fn init_ehdb_publish_deferred_series() {
    ehdb_command_publish_deferred_total().inc_by(0);
    ehdb_command_publish_deferred_landed_total().inc_by(0);
}

pub fn init_db_pool_series() {
    for st in DB_POOL_STATES {
        db_pool_connections().with_label_values(&[st]).set(0);
    }
}

pub fn set_db_pool(total: i64, idle: i64) {
    db_pool_connections()
        .with_label_values(&["total"])
        .set(total);
    db_pool_connections().with_label_values(&["idle"]).set(idle);
}

#[cfg(test)]
mod db_pool_metric_tests {
    /// One test, not two, deliberately.
    ///
    /// ⚠ The registry is process-global and tests run in parallel. Split across
    /// two tests these raced: one called `init_db_pool_series()` (pinning both
    /// series to 0) while the other asserted the values it had just set, and the
    /// pinning test won. Same labels, same registry, no isolation possible — so
    /// the sequence is kept inside a single test.
    #[test]
    fn the_pool_gauge_is_pinned_reaches_the_scrape_and_tracks_its_values() {
        // 1. pinned at 0 — an exhausted pool and a build without the metric must
        //    not look the same; a labelled family is pruned while empty.
        super::init_db_pool_series();
        let text = super::gather_text().expect("gather");
        assert!(
            text.contains("noetl_db_pool_connections"),
            "the pool gauge is not on the registry gather_text() reads — registering is \
             not the same as exposing"
        );
        for st in ["total", "idle"] {
            assert!(
                text.contains(&format!(r#"noetl_db_pool_connections{{state="{st}"}} 0"#)),
                "series for state={st} must be pinned at 0:\n{text}"
            );
        }

        // 2. tracks what it is given, so a renderer hard-coding 0 cannot pass.
        super::set_db_pool(25, 3);
        let text = super::gather_text().expect("gather");
        assert!(
            text.contains(r#"noetl_db_pool_connections{state="total"} 25"#),
            "{text}"
        );
        assert!(
            text.contains(r#"noetl_db_pool_connections{state="idle"} 3"#),
            "{text}"
        );

        // 3. the exhaustion signature the incident would have shown: pool at its
        //    max with nothing idle.
        super::set_db_pool(25, 0);
        let text = super::gather_text().expect("gather");
        assert!(
            text.contains(r#"noetl_db_pool_connections{state="total"} 25"#),
            "{text}"
        );
        assert!(
            text.contains(r#"noetl_db_pool_connections{state="idle"} 0"#),
            "{text}"
        );
    }
}

pub fn init_orphan_sweep_series() {
    for outcome in ORPHAN_SWEEP_OUTCOMES {
        orphan_sweep_total().with_label_values(&[outcome]).inc_by(0);
    }
}

// ── Systemic non-convergence sweep (noetl/ai-meta#227 part B) ────────────────

/// `noetl_nonconvergence_sweep_total{outcome}` — outcomes of the systemic
/// non-convergence sweep ([`crate::handlers::nonconvergence_sweep`]).  `outcome`
/// is one of: `candidate` (an execution whose watermark has not moved for the
/// grace period was examined), `terminated` (`playbook.failed` emitted),
/// `skipped_live` (an outstanding command is held by a live worker — never
/// failed), `skipped_awaiting_callback` (parked on an external callback by
/// design — never failed), `skipped_parent_active` (a parent execution is still
/// running — never failed), `capped` (eligible but deferred to a later tick by
/// the rate limit), `error` (scan / emit failure).  Zero increments while the
/// sweep is off (`NOETL_NONCONVERGENCE_SWEEP_ENABLED=false`).
///
/// The authoritative list is `Disposition::metric_label` in
/// [`crate::handlers::nonconvergence_sweep`] plus the two literals recorded at
/// the call sites; this comment had drifted from it, omitting
/// `skipped_parent_active`.  [`NONCONVERGENCE_SWEEP_OUTCOMES`] is now the
/// single list both the pin and this doc follow.
///
/// `skipped_live` and `skipped_awaiting_callback` are the negative-control
/// signals: during a drain they should account for every healthy execution the
/// sweep looked at, and any drift in `terminated` against a known target list is
/// the alarm.
pub fn nonconvergence_sweep_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_nonconvergence_sweep_total",
                "Systemic non-convergence sweep outcomes, by outcome.",
            ),
            &["outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// The six `outcome` values, pinned at 0 so they exist before the sweep runs.
///
/// The doc above says "zero increments while the sweep is off". That is true of
/// the counter and false of `/metrics`: an unincremented labelled counter has no
/// series, and `Registry::gather` prunes empty families — so a server with the
/// sweep off exposes **nothing at all** here, not zeros.
///
/// That distinction matters more on this metric than on most, because the sweep
/// **terminates executions**. "Has it failed anything?" is a question an
/// operator asks while deciding whether to enable it, and an empty answer is
/// consistent with "it has failed nothing", "the sweep is off", and "this
/// binary has no sweep". Pinning makes the first two readable and leaves only
/// absence to mean the third.
///
/// The set is closed and small, so all six can be pinned — unlike an
/// `{event_type}` label, where this technique does not apply at all and
/// `noetl_server_build_info` is the fallback.
/// Kept in sync with `Disposition::metric_label` in
/// [`crate::handlers::nonconvergence_sweep`], plus the two literals
/// (`candidate`, `error`) recorded directly at the call sites.
pub const NONCONVERGENCE_SWEEP_OUTCOMES: [&str; 7] = [
    "candidate",
    "terminated",
    "skipped_live",
    "skipped_awaiting_callback",
    "skipped_parent_active",
    "capped",
    "error",
];

/// Materialise every [`NONCONVERGENCE_SWEEP_OUTCOMES`] series at 0.
pub fn init_nonconvergence_sweep_series() {
    for outcome in NONCONVERGENCE_SWEEP_OUTCOMES {
        nonconvergence_sweep_total()
            .with_label_values(&[outcome])
            .inc_by(0);
    }
}

/// Pin the two correctness-signal series that read as *absent* when nothing has
/// gone wrong — which is the state they spend almost all their time in.
///
/// `noetl_state_build_parity_total{result}` compares the two state-build paths.
/// A `mismatch` is a correctness divergence, so the question asked of it is
/// "have there been any?" — and until now the answer to that was an empty
/// scrape, equally consistent with "none" and "the comparison is not running".
/// Both `match` and `mismatch` are pinned so the ratio is readable from the
/// first scrape.
///
/// `noetl_terminal_dedup_total{outcome}` is the noetl/ai-meta#118 guard that
/// suppresses a second terminal event for an execution — the one that would
/// otherwise orphan the chain with a NULL-`prev_event_id` second root.  It has
/// a single outcome, `suppressed`, and a healthy platform never increments it.
/// A guard that has never fired and a guard that is not deployed are the two
/// readings that must not look alike.
///
/// Both label sets come from the call sites (`handlers::events` and
/// `handlers::event_write`), all of which pass literals — not from prose.
pub fn init_parity_and_dedup_series() {
    for result in ["match", "mismatch"] {
        state_build_parity_total()
            .with_label_values(&[result])
            .inc_by(0);
    }
    terminal_dedup_total()
        .with_label_values(&["suppressed"])
        .inc_by(0);
}

/// Every `outcome` the result-tier GC records, taken from the call sites in
/// [`crate::handlers::result_tier`].
///
/// Result-tier GC **deletes** stored results, so "how much has it deleted?" is
/// a question with consequences, and until now the answer on a server that had
/// not yet run a GC pass was an empty scrape rather than zeros.
///
/// This list is taken from the source, and two separate manual passes still got
/// it wrong.  A same-line grep found six, because `skipped_unparseable`'s
/// literal is wrapped onto the line after its call.  Reading the file found
/// seven, because there are **two** GC call blocks and the second one adds
/// `skipped_unsunk`.  The eighth was found only by the test below.  A set that
/// silently omits an outcome is worse than no set at all, because the missing
/// one reads as healthy.
pub const RESULT_TIER_GC_OUTCOMES: [&str; 8] = [
    "no_op",
    "scanned",
    "deleted",
    "skipped_live",
    "skipped_grace",
    "skipped_unparseable",
    "skipped_unsunk",
    "error",
];

/// Materialise every [`RESULT_TIER_GC_OUTCOMES`] series at 0.
pub fn init_result_tier_gc_series() {
    for outcome in RESULT_TIER_GC_OUTCOMES {
        result_tier_gc_total()
            .with_label_values(&[outcome])
            .inc_by(0);
    }
}

/// Record one non-convergence sweep outcome (see [`nonconvergence_sweep_total`]).
pub fn record_nonconvergence_sweep(outcome: &str) {
    nonconvergence_sweep_total()
        .with_label_values(&[outcome])
        .inc();
}

// ── Result/state tier GC (noetl/ai-meta#104 Phase F + #166 Phase 5) ──────────

/// `noetl_result_tier_gc_objects_total{class,action}` — objects a tier-GC sweep
/// examined, by class and outcome. `class` = `result` / `state_open` /
/// `state_sealed` / `other` (noetl/ai-meta#166 Phase 5 classification); `action`
/// = `skip_live` / `skip_grace` / `skip_unparseable` / `guard_protected`
/// (open state shard held by `NOETL_STATE_SHARD_GC`) / `dead_dryrun` /
/// `deleted`. Lets an operator see how many state shards vs result objects a
/// sweep reclaims without parsing the JSON report. Zero increments when
/// `NOETL_RESULT_TIER_GC` is off (the sweep is a no-op).
pub fn result_tier_gc_objects_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_result_tier_gc_objects_total",
                "Tier-GC objects by class and action (noetl/ai-meta#104 + #166).",
            ),
            &["class", "action"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one tier-GC object outcome (see [`result_tier_gc_objects_total`]).
pub fn record_result_tier_gc_object(class: &str, action: &str) {
    result_tier_gc_objects_total()
        .with_label_values(&[class, action])
        .inc();
}

// ── Server-routed command publish (noetl/ai-meta#166 Phase 5) ────────────────

/// `noetl_command_publish_total{route,pool}` — command notifications published
/// to NATS, by routing shape. `route` = `sharded` (published to the per-shard
/// subject `noetl.commands.<pool>.shard.<n>.<eid>` under
/// `NOETL_SHARD_SUBJECT_ROUTE`) or `legacy` (the pool subject
/// `noetl.commands.<pool>.<eid>`). `pool` is the resolved pool segment
/// (`system`/`shared`/`subscription`/…). With server-routing off every publish
/// is `legacy`; the `sharded` series only materialises once the flag is on and
/// `NOETL_COMMAND_SHARD_COUNT > 1` for the system pool.
pub fn command_publish_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_command_publish_total",
                "Command notifications published to NATS, by routing shape (noetl/ai-meta#166).",
            ),
            &["route", "pool"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one command-notification publish (see [`command_publish_total`]).
pub fn record_command_publish(route: &str, pool: &str) {
    command_publish_total()
        .with_label_values(&[route, pool])
        .inc();
}

/// `noetl_ehdb_command_publish_failed_total{reason}` — command notifications the
/// server **gave up** publishing to the EHDB writer after exhausting its retry
/// window, and per-attempt failures that were retried.
///
/// `noetl_command_publish_total` counts only successes, so before this there was
/// no failure rate and a total give-up was visible in the log and nowhere else.
/// That matters on this path specifically: post-T5 the EHDB writer is the only
/// transport carrying command notifications, and a command that is never
/// published is never claimed — the execution simply stops, with no terminal
/// event to notice. noetl/ai-meta#208 was exactly that shape and ran unnoticed
/// in production for ~2.4 days.
///
/// `reason` is `gave_up` (retry window exhausted; the dispatch is lost) or
/// `attempt` (one publish attempt failed and will be retried — expected during
/// a writer restart, and only interesting as a rate).
pub fn ehdb_command_publish_failed_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_ehdb_command_publish_failed_total",
                "Command notifications the server failed to publish to the EHDB writer, by reason (noetl/ai-meta#208).",
            ),
            &["reason"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Materialise both `reason` series at 0 so they exist before the first failure.
///
/// A labelled counter has no series until it is incremented, so an unfired
/// `noetl_ehdb_command_publish_failed_total` is simply ABSENT from `/metrics`.
/// Absent is indistinguishable from three different things: nothing has failed,
/// the metric was removed, or the binary predates it.  Verified on kind against
/// a released image — the gauge showed up and this counter did not.
///
/// The `reason` values are pinned at 0 so the absence question disappears: a
/// scrape either shows 0 (healthy, current binary) or nothing (not this
/// binary).
///
/// `no_writers` and `shadow_failed` were added when the P2 dispatch triage
/// found two more ways a command fails to reach the bus, neither of which had
/// any signal.  `no_writers` is the severe one: with `NOETL_COMMAND_BUS=ehdb`
/// and no writer routes resolved, EVERY command is silently not delivered and
/// every execution stalls — previously visible only as a `tracing::warn!`.
/// ⚠ Every reason the code can record must be listed here.
///
/// A reason that is recorded but not pinned stays **ABSENT** from `/metrics`
/// until the first time it fires, while its siblings read 0 — so "it has never
/// happened" and "this build has no such metric" are the same scrape. That is
/// the absent-is-not-zero shape this repo keeps finding, and
/// `drift-audit.sh metric-pins` compares this list against the literals
/// `record_ehdb_command_publish_failed` is actually called with.
///
/// `deferred_exhausted` was missing here from the day the deferred publish path
/// shipped (noetl/ai-meta#319 P3) — the one outcome an operator would go looking
/// for after a writer outage, and the one that would have shown nothing.
pub const EHDB_COMMAND_PUBLISH_FAILED_REASONS: [&str; 5] = [
    "gave_up",
    "attempt",
    "no_writers",
    "shadow_failed",
    "deferred_exhausted",
];

pub fn init_ehdb_command_publish_failed_series() {
    for reason in EHDB_COMMAND_PUBLISH_FAILED_REASONS {
        ehdb_command_publish_failed_total()
            .with_label_values(&[reason])
            .inc_by(0);
    }
}

/// Record a failed EHDB command publish (see [`ehdb_command_publish_failed_total`]).
pub fn record_ehdb_command_publish_failed(reason: &str) {
    ehdb_command_publish_failed_total()
        .with_label_values(&[reason])
        .inc();
}

/// `noetl_event_ingest_publish_skipped_total{reason}` — events that took the
/// INSERT path instead of the publish path, and which of `should_publish`'s
/// three conditions sent them there.
///
/// `noetl_event_ingest_published_total` counts only the publish side, so a
/// server that publishes nothing exposes no series at all — and post-T5 the
/// EHDB events feed is the sole writer of the durable log, which makes "zero
/// publishes" the single most consequential state to be unable to read.
///
/// Absent, that zero has three very different causes and no way to tell them
/// apart from `/metrics`:
///
/// - `gate_off` — `NOETL_EVENT_INGEST_PUBLISH_ONLY` is unset. Expected.
/// - `no_transport` — the gate is on but there is no usable publisher, which
///   is the noetl/ai-meta#212 shape: nothing errors, executions still
///   complete, and the events feed sits at a flat cursor.
/// - `system_execution` — system-pool playbooks are deliberately exempt
///   (see [`is_system_execution`]), so a server carrying only system traffic
///   publishes nothing and is perfectly healthy.
///
/// Reading which of the three applies on production took a source dive, three
/// env lookups and a label inspection; it is one scrape with this counter.
///
/// [`is_system_execution`]: crate::handlers::event_write
pub fn event_ingest_publish_skipped_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_event_ingest_publish_skipped_total",
                "Events routed to INSERT instead of publish, by which should_publish condition failed (noetl/ai-meta#238)",
            ),
            &["reason"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Materialise all three `reason` series at 0 so they exist before the first skip.
///
/// Same argument as [`init_ehdb_command_publish_failed_series`]: a labelled
/// counter is ABSENT until incremented, and absent cannot be distinguished
/// from "the binary predates this metric". `reason` has exactly three known
/// values, so all three can be pinned and the absence question disappears.
pub fn init_event_ingest_publish_skipped_series() {
    for reason in ["gate_off", "no_transport", "system_execution"] {
        event_ingest_publish_skipped_total()
            .with_label_values(&[reason])
            .inc_by(0);
    }
}

/// Record one event that skipped the publish path
/// (see [`event_ingest_publish_skipped_total`]).
pub fn record_event_ingest_publish_skipped(reason: &str) {
    event_ingest_publish_skipped_total()
        .with_label_values(&[reason])
        .inc();
}

/// `noetl_ehdb_event_publisher_configured` — 1 when the server has a usable
/// EHDB events publisher, 0 when `NOETL_EVENT_BUS` selects EHDB but
/// `NOETL_EVENT_BUS_WRITER_ADDRS` resolved to no routes.
///
/// The zero case is not a degraded mode: post-T5 the events feed is the sole
/// writer of the durable log, so every event publish is skipped and the log
/// simply stops growing.  Until now the only signal was one `tracing::error!`
/// at startup — a line that scrolls away, on a workload nothing scrapes
/// (noetl/ai-meta#238).  A gauge is alertable and survives the boot it
/// describes.
///
/// Deliberately a gauge rather than a counter: this is boot-time state, and the
/// question an operator asks is "is it configured right now", not "how often did
/// it fail".
pub fn ehdb_event_publisher_configured() -> &'static IntGauge {
    static M: OnceLock<IntGauge> = OnceLock::new();
    M.get_or_init(|| {
        let g = IntGauge::new(
            "noetl_ehdb_event_publisher_configured",
            "1 when the EHDB events publisher has writer routes, 0 when EHDB is selected but none resolved (noetl/ai-meta#238).",
        )
        .expect("static gauge spec must be valid");
        registry()
            .register(Box::new(g.clone()))
            .expect("gauge registration must succeed");
        g
    })
}

/// Record whether the EHDB events publisher is usable (see
/// [`ehdb_event_publisher_configured`]).
pub fn set_ehdb_event_publisher_configured(configured: bool) {
    ehdb_event_publisher_configured().set(i64::from(configured));
}

/// `noetl_server_build_info{version}` — always 1; the version is the point.
///
/// A labelled metric has no series until a child exists, and `Registry::gather`
/// **prunes empty metric families**, so registering a metric is not enough to
/// make it visible. Every counter is therefore ABSENT until it first fires, and
/// absent has two very different meanings: "this has never happened" or "this
/// binary is too old to have the metric".
///
/// Pinning known label values fixes that one metric at a time
/// (see [`init_ehdb_command_publish_failed_series`]), but it only works where
/// the label set is closed — `noetl_worker_event_emit_failed_total{event_type}`
/// takes a free-form string and cannot be pinned at all.
///
/// A build-info gauge answers the question once for every metric on the
/// process: if the version here is new enough to have metric X, then X's
/// absence means it has not fired. Nothing else on `/metrics` carries the
/// version today — establishing which binary a pod ran meant reading the image
/// tag out of the Deployment, which is a different representation and can
/// disagree with what is actually running (noetl/ai-meta#238).
pub fn build_info() -> &'static IntGaugeVec {
    static M: OnceLock<IntGaugeVec> = OnceLock::new();
    M.get_or_init(|| {
        let g = IntGaugeVec::new(
            Opts::new(
                "noetl_server_build_info",
                "Always 1; the version label identifies the running binary (noetl/ai-meta#238).",
            ),
            &["version"],
        )
        .expect("static gauge spec must be valid");
        registry()
            .register(Box::new(g.clone()))
            .expect("gauge registration must succeed");
        g
    })
}

/// Publish this binary's version as [`build_info`]. Call once at startup.
pub fn init_build_info() {
    build_info()
        .with_label_values(&[env!("CARGO_PKG_VERSION")])
        .set(1);
}

// ── Off-server tail-attach accelerator (noetl/ai-meta#156) ───────────────────

/// `noetl_offserver_tail_attached_total{outcome}` — off-server drive dispatches
/// by whether the server attached a non-empty per-execution event tail
/// ([`crate::state::ChainTails`]).  `outcome` = `attached` (the dispatch carried
/// `tail_events` so the worker can advance its WAL index drain-independently),
/// `empty` (the ring held nothing for this execution — a cold dispatch falling
/// back to today's drain-served path), or `scoped_out` (the master flag is on
/// but this playbook is outside the `NOETL_OFFSERVER_TAIL_PLAYBOOK_PREFIXES`
/// allowlist — e.g. the auth path — so the drive intentionally carries no tail).
/// Zero increments when the accelerator is off (`NOETL_OFFSERVER_ATTACH_TAIL=false`).
pub fn offserver_tail_attached_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_offserver_tail_attached_total",
                "Off-server drive dispatches by tail-attach outcome (noetl/ai-meta#156).",
            ),
            &["outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// `noetl_offserver_tail_size` — distribution of the number of events the server
/// attaches to an off-server drive dispatch (noetl/ai-meta#156).  Observed only
/// when a non-empty tail is attached; the magnitude shows the tail stays O(few
/// events) rather than O(global-stream).
pub fn offserver_tail_size() -> &'static Histogram {
    static M: OnceLock<Histogram> = OnceLock::new();
    M.get_or_init(|| {
        let h = Histogram::with_opts(
            HistogramOpts::new(
                "noetl_offserver_tail_size",
                "Events attached to an off-server drive dispatch (noetl/ai-meta#156).",
            )
            .buckets(vec![1.0, 2.0, 3.0, 5.0, 8.0, 16.0, 32.0, 64.0]),
        )
        .expect("static histogram spec must be valid");
        registry()
            .register(Box::new(h.clone()))
            .expect("histogram registration must succeed");
        h
    })
}

/// Record one off-server drive dispatch's tail-attach outcome.  `n` is the number
/// of events attached (0 → the `empty` outcome; the size histogram is observed
/// only for a non-empty tail).
pub fn record_offserver_tail_attached(n: usize) {
    if n == 0 {
        offserver_tail_attached_total()
            .with_label_values(&["empty"])
            .inc();
    } else {
        offserver_tail_attached_total()
            .with_label_values(&["attached"])
            .inc();
        offserver_tail_size().observe(n as f64);
    }
}

/// Record an off-server drive dispatch whose playbook is **outside** the
/// tail-attach allowlist (`NOETL_OFFSERVER_TAIL_PLAYBOOK_PREFIXES`) while the
/// master flag is on — e.g. the auth/login path (noetl/ai-meta#156).  The drive
/// carries no tail and keeps today's drain-served behavior; this counter makes
/// the scoping observable (auth executions should land here, never in `attached`).
pub fn record_offserver_tail_scoped_out() {
    offserver_tail_attached_total()
        .with_label_values(&["scoped_out"])
        .inc();
}

// ── Terminal-event dedup (noetl/ai-meta#118) ─────────────────────────────────

/// `noetl_terminal_dedup_total{outcome}` — the event-write chokepoint's
/// exactly-one-terminal-per-execution guard.  `outcome` = `suppressed` (a
/// DUPLICATE terminal event — a straggler/duplicate finalize under off-server +
/// PUBLISH_ONLY materializer-lag on a single replica — was dropped before it
/// could reach the chain linker and orphan as a NULL-`prev_event_id` second
/// chain root).  Zero increments on a healthy run; any non-zero count is the
/// race being caught instead of forking the chain.  See
/// [`crate::state::FinalizedGuard`].
pub fn terminal_dedup_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_terminal_dedup_total",
                "Duplicate terminal events suppressed at the event-write chokepoint (noetl/ai-meta#118).",
            ),
            &["outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one terminal-event dedup decision (`outcome` = `suppressed`).
pub fn record_terminal_dedup(outcome: &str) {
    terminal_dedup_total().with_label_values(&[outcome]).inc();
}

// ── Atomic-working-item context (RFC noetl/ai-meta#115 Phase 5) ───────────────

/// `noetl_atomic_item_context_total{outcome}` — how the in-process drive sized
/// each worker-bound command context when the atomic-item-context flag is on.
/// `outcome` = `narrowed` (a minimal slice replaced the full context) |
/// `full_fallback` (the step couldn't be statically bounded, so the full
/// context shipped — conservative). Zero increments while the flag is off.
pub fn atomic_item_context_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_atomic_item_context_total",
                "Worker-bound command context sizing under the atomic-item-context flag (RFC #115 Phase 5).",
            ),
            &["outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one atomic-item-context sizing decision (`narrowed` | `full_fallback`).
pub fn record_atomic_item_context(outcome: &str) {
    atomic_item_context_total()
        .with_label_values(&[outcome])
        .inc();
}

// ── Object-store GCS auth (noetl/ai-meta#104 result tier) ────────────────────

/// `noetl_object_store_gcs_auth_total{mode, outcome}` — GCS backend bearer-token
/// acquisitions for the result tier (noetl/ai-meta#104). `mode` = `adc`
/// (Workload Identity / Application Default Credentials, the prod path) | `static`
/// (explicit `NOETL_OBJECT_STORE_GCS_TOKEN`). `outcome` = `acquired` (token
/// resolved — for `adc` this is served from gcp_auth's internal cache or a fresh
/// mint, transparently) | `error` (provider init or token fetch failed). The
/// no-auth emulator path (`mode = none`) makes no external token call, so it
/// never increments here.
pub fn object_store_gcs_auth_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_object_store_gcs_auth_total",
                "GCS result-tier bearer-token acquisitions, by mode and outcome (noetl/ai-meta#104).",
            ),
            &["mode", "outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one GCS bearer-token acquisition (`mode` = `adc` | `static`;
/// `outcome` = `acquired` | `error`).
pub fn record_object_store_gcs_auth(mode: &str, outcome: &str) {
    object_store_gcs_auth_total()
        .with_label_values(&[mode, outcome])
        .inc();
}

// ── State-build mode (RFC noetl/ai-meta#115 Phase 3) ─────────────────────────

/// `noetl_state_build_total{mode, outcome}` — how the drive built `WorkflowState`
/// for a trigger. `mode` = `chain_walk` | `event_scan`. `outcome` = `ok`
/// (built via that mode) | `fallback_cold_head` / `fallback_node_missing` /
/// `fallback_non_genesis` / `fallback_empty` (chain_walk asked for, but a guard
/// sent it to the event-scan path — correctness preserved). Watching
/// `chain_walk/ok` vs the `fallback_*` outcomes shows how often the in-memory
/// chain head served the build without a scan.
pub fn state_build_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_state_build_total",
                "Orchestrator WorkflowState builds, by mode and outcome (RFC #115 Phase 3).",
            ),
            &["mode", "outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one state build (`mode` = `chain_walk`|`event_scan`; `outcome` = `ok`
/// or a `fallback_*` reason).
pub fn record_state_build(mode: &str, outcome: &str) {
    state_build_total()
        .with_label_values(&[mode, outcome])
        .inc();
}

// ── Hot-path event reads (RFC noetl/ai-meta#115 Phase 6) ─────────────────────

/// `noetl_event_hotpath_reads_total{site, outcome}` — every execution-lifecycle
/// hot-path reader of `noetl.event` that the Phase-6 `event_read_path=audit_only`
/// flag retires (the `WHERE execution_id = $1` replay class *outside* the drive:
/// `get_catalog_id`, `inherit_parent_trace`, the subscription dedup-audit catalog
/// lookup, the container-callback existence + catalog reads).
///
/// - `site` = `get_catalog_id` | `inherit_parent_trace` | `dedup_audit_catalog`
///   | `container_callback_exists` | `container_callback_catalog`.
/// - `outcome` = `served_descriptor` (served from the in-memory execute-time
///   descriptor — **no `noetl.event` read**) | `scan` (fell back to the
///   `WHERE execution_id` scan — cold descriptor, or `event_read_path=event_scan`).
///
/// The never-scan invariant proof (RFC §7): under `event_read_path=audit_only`
/// with `state_builder=offserver`, across a full execution lifecycle the
/// `{outcome="scan"}` series stays **flat** (Δ0) while the lifecycle still
/// completes — every hot-path event read was served from a read model, and
/// `noetl.event` was scanned by nobody on the hot path.  Pairs with the
/// drive-path `noetl_state_build_event_scans_total` (which proves the drive's
/// own zero-scan) for the end-to-end guarantee.
pub fn event_hotpath_reads_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_event_hotpath_reads_total",
                "Execution-lifecycle hot-path reads of noetl.event, by site and outcome (RFC #115 Phase 6).",
            ),
            &["site", "outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one hot-path event read (`site` = the reader; `outcome` =
/// `served_descriptor` | `scan`).
pub fn record_event_hotpath_read(site: &str, outcome: &str) {
    event_hotpath_reads_total()
        .with_label_values(&[site, outcome])
        .inc();
}

/// `noetl_result_uri_accept_total{outcome}` — canonical result-URI shadow
/// acceptance (RFC noetl/ai-meta#104 Phase A). Incremented once per event whose
/// `result` carries a `reference.uri`, when `NOETL_RESULT_URI_ACCEPT=true`.
/// `outcome` is one of:
/// - `canonical` — parsed as the canonical logical Resource Locator.
/// - `legacy` — parsed as the legacy execution ref (accepted for back-compat).
/// - `malformed` — failed to parse; logged + counted, event NOT failed.
///
/// Flag-off this counter never moves (the accept hook is skipped); flag-on its
/// delta over a run is the proof the server is accepting the worker-stamped URI
/// without resolving by it yet (that is Phase C).
pub fn result_uri_accept_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_result_uri_accept_total",
                "Canonical result-URI shadow acceptances by outcome (RFC #104 Phase A).",
            ),
            &["outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one canonical result-URI acceptance outcome (`canonical` | `legacy` |
/// `malformed`).
pub fn record_result_uri_accept(outcome: &str) {
    result_uri_accept_total()
        .with_label_values(&[outcome])
        .inc();
}

/// `noetl_result_store_dual_write_total` — over-budget results written to
/// `noetl.result_store` as the **transitional fallback leg** while the Phase D
/// minting flip is on (`NOETL_RESULT_MINT_AUTHORITATIVE=true`,
/// RFC noetl/ai-meta#104 Phase D).
///
/// Under the flip the worker treats the URN → Feather/GCS tier as authoritative
/// and resolves from it first; the server keeps minting + storing `result_store`
/// so the cutover is reversible (flag-off → `result_store`-authoritative again).
/// Each such write increments this counter, making the dual-write window
/// observable. Flag-off it never moves (the dual-write is just the ordinary,
/// only store). The actual retirement of `result_store` (stopping this write) is
/// the OQ5-gated operational decision — not Phase D.
pub fn result_store_dual_write_total() -> &'static prometheus::IntCounter {
    static M: OnceLock<prometheus::IntCounter> = OnceLock::new();
    M.get_or_init(|| {
        let counter = prometheus::IntCounter::new(
            "noetl_result_store_dual_write_total",
            "result_store writes that are the dual-write fallback leg under the Phase D minting flip (RFC #104 Phase D).",
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one `result_store` dual-write (the reversible fallback leg under the
/// Phase D minting flip).
pub fn record_result_store_dual_write() {
    result_store_dual_write_total().inc();
}

/// `noetl_result_store_dual_write_skipped_total` — `PUT /api/result/{eid}`
/// requests whose `noetl.result_store` INSERT was **skipped** because the
/// dual-write was retired (`NOETL_RESULT_STORE_DUAL_WRITE=false`, RFC
/// noetl/ai-meta#104 OQ5 retirement).
///
/// The handler still mints + returns a byte-identical `ResultPutResponse` (the
/// worker's `reference` block is unchanged); only the DB row is not written. This
/// counter climbing while `noetl_result_store_put_total{status="ok"}` stays flat
/// is the on-prod signal that the store write is retired — resolution continues
/// to serve from the #104 result tier. Flag-on it never moves.
pub fn result_store_dual_write_skipped_total() -> &'static prometheus::IntCounter {
    static M: OnceLock<prometheus::IntCounter> = OnceLock::new();
    M.get_or_init(|| {
        let counter = prometheus::IntCounter::new(
            "noetl_result_store_dual_write_skipped_total",
            "result_store INSERTs skipped because the dual-write was retired (NOETL_RESULT_STORE_DUAL_WRITE=false, RFC #104 OQ5).",
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one skipped `result_store` write (the retired dual-write under
/// `NOETL_RESULT_STORE_DUAL_WRITE=false`).
pub fn record_result_store_dual_write_skipped() {
    result_store_dual_write_skipped_total().inc();
}

/// `noetl_state_build_event_scans_total` — incremented once each time the drive
/// path enters the **event-scan** state-construction block (the block that issues
/// `WHERE execution_id = $1 …` scans of `noetl.event`: the consistency `COUNT`,
/// the `event_id > $2` window, and the bounded `rebuild_state`). This is the
/// no-scan proof counter for RFC #115 tenet 3: with `NOETL_STATE_BUILD_MODE=chain_walk`
/// and no fallback, the drive never enters that block, so this counter's delta
/// over a run is **0** while `noetl_state_build_chain_hops` shows the PK-only walk
/// did the work.
pub fn state_build_event_scans_total() -> &'static prometheus::IntCounter {
    static M: OnceLock<prometheus::IntCounter> = OnceLock::new();
    M.get_or_init(|| {
        let counter = prometheus::IntCounter::new(
            "noetl_state_build_event_scans_total",
            "Times the drive entered the noetl.event-scanning state-build block (RFC #115 \
             tenet 3 no-scan proof; chain_walk keeps this at 0).",
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record that the drive used the event-scanning state-build path for a trigger.
pub fn record_state_build_event_scan() {
    state_build_event_scans_total().inc();
}

/// `noetl_state_build_chain_hops` — distribution of chain-walk depth (number of
/// `(execution_id, event_id)` PK lookups) per successful chain-walk build. Each
/// observation == the events collected by following `prev_event_id` head→root.
/// Non-zero observations are the positive evidence the PK-only walk is doing the
/// state construction.
pub fn state_build_chain_hops() -> &'static Histogram {
    static M: OnceLock<Histogram> = OnceLock::new();
    M.get_or_init(|| {
        let hist = Histogram::with_opts(
            HistogramOpts::new(
                "noetl_state_build_chain_hops",
                "Chain-walk depth (prev_event_id PK lookups) per state build (RFC #115 Phase 3).",
            )
            .buckets(vec![
                1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0,
            ]),
        )
        .expect("static histogram spec must be valid");
        registry()
            .register(Box::new(hist.clone()))
            .expect("histogram registration must succeed");
        hist
    })
}

/// Record the depth of one chain-walk build.
pub fn record_state_build_chain_hops(hops: usize) {
    state_build_chain_hops().observe(hops as f64);
}

/// `noetl_state_build_parity_total{result}` — when `NOETL_STATE_BUILD_PARITY_CHECK`
/// is on, each shadow comparison of the event-scan vs chain-walk build records
/// `match` or `mismatch`. A non-zero `mismatch` is a correctness alarm (the two
/// builders disagree for an execution) and is the parity proof's failure signal.
pub fn state_build_parity_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_state_build_parity_total",
                "Shadow event-scan vs chain-walk state-build comparisons, by result (RFC #115).",
            ),
            &["result"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one parity-check result (`match` | `mismatch`).
pub fn record_state_build_parity(result: &str) {
    state_build_parity_total()
        .with_label_values(&[result])
        .inc();
}

// ── Replica coherence (RFC noetl/ai-meta#115 program-scale / #107) ───────────

/// `noetl_replica_coherence_total{structure, op, outcome}` — every access to the
/// per-execution drive watermark ([`crate::state::ChainHeads`]) or descriptor
/// ([`crate::state::ExecDescriptor`]) under `NOETL_REPLICA_COHERENCE=nats_kv`,
/// labelled by which read model served it.  The proof series for multi-replica
/// coherence:
///
/// - `structure` = `chain_head` | `descriptor`.
/// - `op` = `link_batch` | `head` | `get` | `seed` | `mark_terminal` | `evict`.
/// - `outcome`:
///   - `kv_ok` — a KV write (head CAS / descriptor merge / evict) succeeded.
///   - `kv_remote_hit` — a `descriptor get` (or `head`) **missed the local
///     in-process map but hit the KV bucket** — i.e. another replica seeded it
///     and this replica resolved it coherently.  **This is the load-bearing
///     proof counter**: every increment is a server-built cold-fallback (an event
///     read) that the KV backing avoided when the trigger landed on a different
///     replica than the one that seeded the execution.
///   - `kv_local_hit` — both the local map and KV had it (the common
///     single-replica / same-replica case).
///   - `kv_miss` — KV authoritatively had no entry (genuinely cold: never-seeded
///     or evicted everywhere) → the caller takes the server-built fallback.
///   - `kv_unavailable` — KV unreachable / disabled / a CAS exhausted its
///     retries → degraded to the in-process map (behaves as `local`).
///
/// Under `nats_kv` with 2+ replicas and triggers landing across them, a coherent
/// run shows `kv_remote_hit > 0` (cross-replica resolves happened) while the
/// drive's `noetl_state_build_event_scans_total` and the hot-path
/// `noetl_event_hotpath_reads_total{outcome="scan"}` stay flat — coherence
/// without a single recovery scan attributable to the replica split.
pub fn replica_coherence_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_replica_coherence_total",
                "Per-execution drive watermark/descriptor accesses under NOETL_REPLICA_COHERENCE=nats_kv, by structure/op/outcome (RFC #115 program-scale).",
            ),
            &["structure", "op", "outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one replica-coherence access (`structure`/`op`/`outcome`).
pub fn record_replica_coherence(structure: &str, op: &str, outcome: &str) {
    replica_coherence_total()
        .with_label_values(&[structure, op, outcome])
        .inc();
}

// ── Execution affinity (RFC noetl/ai-meta#116) ───────────────────────────────

/// `noetl_execution_affinity_total{outcome}` — every `POST /api/events` routing
/// decision under `NOETL_EXECUTION_AFFINITY=true`.  The single-owner write-
/// ordering proof for multi-replica off-server execution.
///
/// - `owned_local` — this replica owns the execution; processed locally (the
///   common case on the owner).
/// - `forwarded_ok` — a non-owner forwarded the event to the owner and got a
///   success back.  **The load-bearing proof**: every increment is a trigger that
///   would otherwise have driven/emitted on the wrong replica (a chain-fork
///   source) and was instead funnelled to the single owner.
/// - `forwarded_terminus` — a request the peer already forwarded once landed here
///   (this replica is the owner); processed locally (loop guard).
/// - `forward_unavailable` / `forward_http_err` / `forward_decode_err` — the
///   forward failed; degraded to local processing (no event dropped). Should stay
///   0 in a healthy cluster.
pub fn execution_affinity_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_execution_affinity_total",
                "POST /api/events affinity routing decisions under NOETL_EXECUTION_AFFINITY, by outcome (RFC noetl/ai-meta#116).",
            ),
            &["outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one execution-affinity routing decision.
pub fn record_execution_affinity(outcome: &str) {
    execution_affinity_total()
        .with_label_values(&[outcome])
        .inc();
}

/// Histogram: wall-clock time spent inside the `POST /api/events` handler.
pub fn event_ingest_duration_seconds() -> &'static HistogramVec {
    static M: OnceLock<HistogramVec> = OnceLock::new();
    M.get_or_init(|| {
        let hist = HistogramVec::new(
            HistogramOpts::new(
                "noetl_event_ingest_duration_seconds",
                "Wall-clock time spent inside POST /api/events.",
            )
            .buckets(EVENT_INGEST_BUCKETS.to_vec()),
            &["event_type"],
        )
        .expect("static histogram spec must be valid");
        registry()
            .register(Box::new(hist.clone()))
            .expect("histogram registration must succeed");
        hist
    })
}

/// Histogram: one PHASE of `POST /api/events`, so the handler's total can be
/// attributed rather than guessed (noetl/ai-meta#155).
///
/// The total (`event_ingest_duration_seconds`) reads ~336ms for
/// `command.started` / `command.completed` and ~1021ms for `call.done` on prod,
/// against ~40ms in kind — three emits per playbook hop, which is most of the
/// uniform ~0.9-1.4s per-transition cadence users see. Nothing recorded WHERE
/// inside the handler that goes, and the components differ between environments
/// (prod runs the GCS object store, kind runs the Postgres one), so the split
/// has to be measured on prod rather than inferred from a kind run.
///
/// `phase` values, in execution order:
///
/// - `catalog_id`     — the `get_catalog_id` lookup.
/// - `claim_check`    — `check_already_claimed` (claim events only).
/// - `normalize`      — `normalize_event_to_row`.
/// - `emit`           — the whole `emit_event` chokepoint (sum of the four below).
/// - `emit_mirror`    — the EHDB event-log tier mirror (`mirror_rows`).
/// - `emit_should_publish` — the publish gate's catalog lookup.
/// - `emit_publish`   — publishing onto the EHDB events feed.
/// - `emit_insert`    — the authoritative Postgres INSERT.
/// - `trigger`        — `trigger_orchestrator` (dispatches the drive).
///
/// Measurement only: recording a duration changes no behaviour on this path.
pub fn event_ingest_phase_seconds() -> &'static HistogramVec {
    static M: OnceLock<HistogramVec> = OnceLock::new();
    M.get_or_init(|| {
        let hist = HistogramVec::new(
            HistogramOpts::new(
                "noetl_event_ingest_phase_seconds",
                "Wall-clock time in one phase of POST /api/events.",
            )
            .buckets(vec![
                0.001, 0.005, 0.010, 0.025, 0.050, 0.100, 0.250, 0.500, 1.0, 2.5, 5.0, 10.0,
            ]),
            &["phase"],
        )
        .expect("static histogram spec must be valid");
        registry()
            .register(Box::new(hist.clone()))
            .expect("histogram registration must succeed");
        hist
    })
}

/// Record one `POST /api/events` phase (see [`event_ingest_phase_seconds`]).
pub fn record_event_ingest_phase(phase: &str, seconds: f64) {
    event_ingest_phase_seconds()
        .with_label_values(&[phase])
        .observe(seconds);
}

/// Pin the closed `phase` label set so every series exists from process start.
///
/// `Registry::gather` prunes empty families, so an un-exercised phase would be
/// absent and indistinguishable from a binary too old to have the metric — the
/// distinction that matters when reading a prod scrape for the first time.
pub fn init_event_ingest_phase_series() {
    for phase in [
        "catalog_id",
        "claim_check",
        "normalize",
        "emit",
        "emit_mirror",
        "emit_should_publish",
        "emit_publish",
        "emit_insert",
        "trigger",
    ] {
        event_ingest_phase_seconds()
            .with_label_values(&[phase])
            .observe(0.0);
    }
}

/// Record a single `POST /api/events` outcome.
///
/// `event_type` is the wire event_type from the request
/// (`"playbook.initialized"`, `"command.claimed"`, etc.).
/// `status` is `"ok"` on the success path, `"error"` on any
/// `Err` return.  `duration_seconds` is wall-clock time
/// inside the handler.
pub fn record_event_ingest(event_type: &str, status: &str, duration_seconds: f64) {
    events_ingested_total()
        .with_label_values(&[event_type, status])
        .inc();
    event_ingest_duration_seconds()
        .with_label_values(&[event_type])
        .observe(duration_seconds);
}

// The CQRS write-path tailer's metrics were removed here
// (noetl/ai-meta#242): `noetl_event_stream_published_total`,
// `noetl_event_stream_skipped_total` and `noetl_event_stream_cursor`.
// They measured a tailer publishing onto the `noetl_events` JetStream
// stream — the skip reason was literally "payload over the NATS max" —
// and T5 deleted JetStream along with the tailer.  Nothing has called
// their recorders since, and because these are lazily registered they
// were never even present on /metrics to read as zero.

/// Counter: events published through the `emit_event` chokepoint when the
/// `NOETL_EVENT_INGEST_PUBLISH_ONLY` gate is on (noetl/ai-meta#103 phase 2d-3),
/// by event type.  This is the **producer cutover** path (the synchronous
/// INSERT replaced by a publish), so a non-zero rate here means the
/// materializer is the sole writer.  It once had a sibling on the tailer, which
/// went with JetStream at T5 (noetl/ai-meta#242).
///
/// Note the asymmetry with [`event_ingest_publish_skipped_total`]: this counter
/// has no series until the first publish, so zero publishes reads as ABSENT
/// rather than 0.  On a server carrying only system-pool traffic — which is
/// what production reads today — that absence is the healthy state, and the
/// skip counter's `reason` is what tells it apart from a missing transport.
pub fn event_ingest_published_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_event_ingest_published_total",
                "Total events published to noetl_events by the emit_event chokepoint under NOETL_EVENT_INGEST_PUBLISH_ONLY (2d-3 cutover), by event type.",
            ),
            &["event_type"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one event published through the chokepoint (gate-on path).
pub fn record_event_published(event_type: &str) {
    event_ingest_published_total()
        .with_label_values(&[event_type])
        .inc();
}

/// Counter: events mirrored onto the EHDB events feed (noetl/ai-meta#212 L1 T3).
///
/// Paired with [`record_event_published`], this is the shadow-parity signal: in
/// `NOETL_EVENT_BUS=shadow` the two counters must track each other event-for-event
/// and label-for-label.  A divergence by `event_type` localises which events are
/// missing, which a single total would hide.
fn ehdb_event_published_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_ehdb_events_published_total",
                "Total events published onto the EHDB events feed, by event type.",
            ),
            &["event_type"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

pub fn record_ehdb_event_published(event_type: &str) {
    ehdb_event_published_total()
        .with_label_values(&[event_type])
        .inc();
}

/// Counter: EHDB event publishes that failed.
///
/// In `shadow` these are swallowed so the shadow path can never take down event
/// ingest — which means this counter is the *only* place a failing shadow shows
/// up.  A silent shadow that is quietly dropping events would otherwise read as
/// perfect parity right up until the cutover.
fn ehdb_event_publish_errors_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_ehdb_events_publish_errors_total",
                "Total EHDB events-feed publish failures, by event type.",
            ),
            &["event_type"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

pub fn record_ehdb_event_publish_error(event_type: &str) {
    ehdb_event_publish_errors_total()
        .with_label_values(&[event_type])
        .inc();
}

/// Counter: executions whose `projection_snapshot` was advanced by the
/// `system/projector` playbook via `/api/internal/projection/advance`
/// (noetl/ai-meta#103 phase 2b).  No labels — one global rate.
pub fn projection_advanced_total() -> &'static prometheus::IntCounter {
    static M: OnceLock<prometheus::IntCounter> = OnceLock::new();
    M.get_or_init(|| {
        let counter = prometheus::IntCounter::new(
            "noetl_projection_advanced_total",
            "Executions whose projection_snapshot was advanced by the system/projector playbook.",
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one execution's projection advance.
pub fn record_projection_advanced(_version: i64) {
    projection_advanced_total().inc();
}

/// Counter: `noetl.event` rows materialized from the stream by the
/// `system/event_materializer` playbook via `/api/internal/events/materialize`
/// (noetl/ai-meta#103 phase 2d).  No labels — one global rate.
pub fn events_materialized_total() -> &'static prometheus::IntCounter {
    static M: OnceLock<prometheus::IntCounter> = OnceLock::new();
    M.get_or_init(|| {
        let counter = prometheus::IntCounter::new(
            "noetl_events_materialized_total",
            "noetl.event rows materialized from the noetl_events stream by the system/event_materializer playbook.",
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Counter: `noetl.event` rows written by `/api/internal/events/project` — the
/// live durable-log write path.
///
/// [`events_materialized_total`] counts the *other* sink
/// (`/api/internal/events/materialize`), which the deployed configuration does
/// not use, so it reads 0 forever and is not a usable signal for "is the durable
/// log still being written". This one tracks the path that actually runs under
/// `NOETL_EVENT_INGEST_PUBLISH_ONLY`, where the worker-side `noetl_materializer`
/// draining the events bus is the sole writer of `noetl.event`.
///
/// That makes it the ground-truth gate for the T3 events-bus cutover
/// (noetl/ai-meta#212): publish counters prove the *publisher*, this proves rows
/// are still landing in the log. `duplicates` is tracked separately because
/// at-least-once redelivery makes a non-zero duplicate count normal — collapsing
/// the two would hide a real drop behind retried writes.
pub fn events_projected_total() -> &'static prometheus::IntCounter {
    static M: OnceLock<prometheus::IntCounter> = OnceLock::new();
    M.get_or_init(|| {
        let counter = prometheus::IntCounter::new(
            "noetl_events_projected_total",
            "noetl.event rows written via /api/internal/events/project (the live materializer sink).",
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Counter: rows `/api/internal/events/project` skipped as already-present.
pub fn events_projected_duplicates_total() -> &'static prometheus::IntCounter {
    static M: OnceLock<prometheus::IntCounter> = OnceLock::new();
    M.get_or_init(|| {
        let counter = prometheus::IntCounter::new(
            "noetl_events_projected_duplicates_total",
            "Rows /api/internal/events/project skipped as duplicates (at-least-once redelivery).",
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record the outcome of one `events/project` batch.
pub fn record_events_projected(projected: u64, duplicates: u64) {
    events_projected_total().inc_by(projected);
    events_projected_duplicates_total().inc_by(duplicates);
}

/// Record a batch of materialized event rows.
pub fn record_events_materialized(rows: u64) {
    events_materialized_total().inc_by(rows);
}

/// Count of inline business step results stripped out of the permanent
/// `noetl.event` log by the permanent-log-lean strip (noetl/ai-meta#195), and
/// the total pre-strip payload bytes kept out of the append-only log.
fn permanent_log_slimmed_total() -> &'static prometheus::IntCounter {
    static M: OnceLock<prometheus::IntCounter> = OnceLock::new();
    M.get_or_init(|| {
        let counter = prometheus::IntCounter::new(
            "noetl_permanent_log_slimmed_total",
            "Inline business step results stripped from the permanent noetl.event log (noetl/ai-meta#195).",
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

fn permanent_log_slimmed_bytes_total() -> &'static prometheus::IntCounter {
    static M: OnceLock<prometheus::IntCounter> = OnceLock::new();
    M.get_or_init(|| {
        let counter = prometheus::IntCounter::new(
            "noetl_permanent_log_slimmed_bytes_total",
            "Business-payload bytes kept out of the permanent noetl.event log by the lean strip (noetl/ai-meta#195).",
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

pub fn record_permanent_log_slimmed(count: u64, bytes: u64) {
    permanent_log_slimmed_total().inc_by(count);
    permanent_log_slimmed_bytes_total().inc_by(bytes);
}

// ---------------------------------------------------------------------------
// Round 2 — generic write-endpoint surface
// ---------------------------------------------------------------------------

/// Canonical endpoint labels accepted by [`record_write_request`].
///
/// Kept as `&'static str` constants so a typo at a call site is a
/// compile error rather than a runtime drift.  Add new entries here
/// (and only here) when instrumenting future write endpoints.
pub mod endpoint {
    pub const CATALOG_REGISTER: &str = "catalog_register";
    pub const CATALOG_DELETE: &str = "catalog_delete";
    pub const CREDENTIALS_UPSERT: &str = "credentials_upsert";
    pub const KEYCHAIN_SET: &str = "keychain_set";
    pub const RUNTIME_REGISTER: &str = "runtime_register";
    pub const RUNTIME_HEARTBEAT: &str = "runtime_heartbeat";
}

/// Counter: write-endpoint dispatches bucketed by canonical
/// endpoint name and status.  Shared across the Round-2 endpoints
/// because each has a single mode of operation; per-endpoint
/// metrics would inflate the registry without adding signal.
pub fn write_requests_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_write_requests_total",
                "Total POST requests to write endpoints other than /api/events (counted once per handler call, Ok or Err).",
            ),
            &["endpoint", "status"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Histogram: wall-clock time spent inside Round-2 write
/// endpoints, bucketed by canonical endpoint label.
pub fn write_request_duration_seconds() -> &'static HistogramVec {
    static M: OnceLock<HistogramVec> = OnceLock::new();
    M.get_or_init(|| {
        let hist = HistogramVec::new(
            HistogramOpts::new(
                "noetl_write_request_duration_seconds",
                "Wall-clock time spent inside POST write endpoints (other than /api/events).",
            )
            .buckets(EVENT_INGEST_BUCKETS.to_vec()),
            &["endpoint"],
        )
        .expect("static histogram spec must be valid");
        registry()
            .register(Box::new(hist.clone()))
            .expect("histogram registration must succeed");
        hist
    })
}

/// Record a single Round-2 write-endpoint outcome.
///
/// `endpoint` should be one of the constants under
/// [`endpoint`].  `status` is `"ok"` on the success path,
/// `"error"` on any `Err` return.  `duration_seconds` is
/// wall-clock time inside the handler.
pub fn record_write_request(endpoint: &str, status: &str, duration_seconds: f64) {
    write_requests_total()
        .with_label_values(&[endpoint, status])
        .inc();
    write_request_duration_seconds()
        .with_label_values(&[endpoint])
        .observe(duration_seconds);
}

/// Counter: sealed credential responses on `GET /api/credentials/{id}/sealed`,
/// bucketed by outcome.
///
/// Secrets Wallet Phase 5b (noetl/ai-meta#61) — pairs with the `credential.seal`
/// span in `handlers::credentials::get_sealed`.  Labels:
///
/// - `status` ∈ {`ok`, `no_pubkey`, `worker_not_found`, `seal_error`,
///   `credential_error`} — the outcome bucket.
///
/// `noetl_credentials_sealed_total{status="ok"}` is the throughput counter;
/// the other label values are failure modes worth grepping in Prometheus
/// when a worker stops being able to fetch sealed credentials.  `execution_id`
/// is NOT a label (cardinality) — it lives on the matching span.
pub fn credentials_sealed_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_credentials_sealed_total",
                "GET /api/credentials/{id}/sealed calls by outcome status.",
            ),
            &["status"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// `noetl_sharding_config_parse_failed` — 1 when `NOETL_SHARDS` /
/// `NOETL_CLUSTER_DSN` failed to parse and the server fell back to single-pool.
///
/// The fallback is deliberate and safe, but it is also SILENT: an operator who
/// configured sharding gets none of it, and the only trace is one startup
/// `tracing::warn!` on a workload whose /metrics is what anyone actually
/// queries.  A gauge survives the boot it describes; a log line scrolls away.
///
/// Unlabelled, so it reads 0 on a healthy server rather than being absent.
pub fn sharding_config_parse_failed() -> &'static IntGauge {
    static M: OnceLock<IntGauge> = OnceLock::new();
    M.get_or_init(|| {
        let g = IntGauge::new(
            "noetl_sharding_config_parse_failed",
            "1 when sharding config failed to parse and single-pool fallback was used (noetl/ai-meta#238).",
        )
        .expect("static gauge spec must be valid");
        registry()
            .register(Box::new(g.clone()))
            .expect("gauge registration must succeed");
        g
    })
}

/// Record whether the sharding config parsed.  Call once at startup, with
/// `false` on the healthy path so the gauge exists either way.
pub fn set_sharding_config_parse_failed(failed: bool) {
    sharding_config_parse_failed().set(i64::from(failed));
}

/// `noetl_permanent_log_lean_stage_failed_total` — command-context stages that
/// failed, leaving the context inline.
///
/// Not a correctness failure: the context is still carried, just not staged out
/// of the event.  But it means the lean-log strip silently did not apply, so
/// the saving it exists for is not happening and nothing said so.
pub fn permanent_log_lean_stage_failed_total() -> &'static IntCounter {
    static M: OnceLock<IntCounter> = OnceLock::new();
    M.get_or_init(|| {
        let c = IntCounter::new(
            "noetl_permanent_log_lean_stage_failed_total",
            "Command-context stages that failed, leaving context inline (noetl/ai-meta#238).",
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(c.clone()))
            .expect("counter registration must succeed");
        c
    })
}

/// Register the unlabelled metrics that nothing touches at startup.
///
/// On this crate every metric is behind a `OnceLock` and is registered when its
/// ACCESSOR is first called — so "unlabelled, therefore always present at 0" is
/// false here.  It is true on the worker, whose metrics are built eagerly in
/// `WorkerMetrics::new`; assuming it transferred is how
/// `noetl_permanent_log_lean_stage_failed_total` shipped absent from a released
/// image while its unit test passed, because the test called the accessor and
/// the binary never does.
///
/// `ehdb_event_publisher_configured` and `sharding_config_parse_failed` are
/// already registered as a side effect of being SET at startup.  This covers
/// the ones with no such setter.
pub fn init_unlabelled_series() {
    // Touching the accessor is what registers it.  All of these were ABSENT
    // from a released image's /metrics until this ran — verified by scraping
    // v3.77.0 on kind and diffing the declared set against the served set: 17
    // of 67 declared metrics were present, and these 11 were the unlabelled
    // ones missing purely because nothing had called them yet.
    //
    // `state_build_event_scans_total` is the one that makes the case.  It
    // exists to prove the never-scan invariant — "this drive did zero
    // noetl.event scans" — and an ABSENT metric cannot prove zero.  It reads
    // the same as a binary without the metric at all.
    let _ = cell_registry_requests_total();
    let _ = events_materialized_total();
    let _ = events_projected_duplicates_total();
    let _ = events_projected_total();
    let _ = permanent_log_lean_stage_failed_total();
    let _ = permanent_log_slimmed_bytes_total();
    let _ = permanent_log_slimmed_total();
    let _ = projection_advanced_total();
    let _ = result_store_dual_write_skipped_total();
    let _ = result_store_dual_write_total();
    let _ = state_build_event_scans_total();
}

/// Every unlabelled metric that `init_unlabelled_series` must register.
///
/// Kept as data so the test can assert the served set rather than re-listing
/// names in two places that drift apart.
pub const UNLABELLED_STARTUP_METRICS: [&str; 11] = [
    "noetl_cell_registry_requests_total",
    "noetl_events_materialized_total",
    "noetl_events_projected_duplicates_total",
    "noetl_events_projected_total",
    "noetl_permanent_log_lean_stage_failed_total",
    "noetl_permanent_log_slimmed_bytes_total",
    "noetl_permanent_log_slimmed_total",
    "noetl_projection_advanced_total",
    "noetl_result_store_dual_write_skipped_total",
    "noetl_result_store_dual_write_total",
    "noetl_state_build_event_scans_total",
];

/// Record one failed command-context stage.
pub fn record_permanent_log_lean_stage_failed() {
    permanent_log_lean_stage_failed_total().inc();
}

/// `noetl_command_row_insert_failed_total{mode}` — `noetl.command` rows the
/// server failed to write, by whether it was a single or batch insert.
///
/// Both sites call this "non-fatal — event log is source of truth", which is
/// right about REPLAY and understates the rest: `noetl.command` is read on
/// live paths.  `handlers/container_callback.rs` resolves `catalog_id` from it
/// to route a container's resume, and `handlers/events.rs` does five further
/// lookups.  A missing row therefore does not corrupt state, but it can make a
/// later callback unable to find its catalog entry — on the same resume path
/// noetl/ai-meta#227 is about.
///
/// It is emphatically not a reason to fail the request; it is a reason to be
/// able to see that it happened, which until now nothing could.
pub fn command_row_insert_failed_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_command_row_insert_failed_total",
                "noetl.command rows that failed to insert, by mode (noetl/ai-meta#238).",
            ),
            &["mode"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// The two insert shapes.
pub const COMMAND_ROW_INSERT_MODES: [&str; 2] = ["single", "batch"];

/// Materialise every [`COMMAND_ROW_INSERT_MODES`] series at 0.
pub fn init_command_row_insert_series() {
    for mode in COMMAND_ROW_INSERT_MODES {
        command_row_insert_failed_total()
            .with_label_values(&[mode])
            .inc_by(0);
    }
}

/// Record one failed `noetl.command` insert.
pub fn record_command_row_insert_failed(mode: &str) {
    command_row_insert_failed_total()
        .with_label_values(&[mode])
        .inc();
}

/// `noetl_system_plugin_seed_total{outcome}` — built-in wasm plug-ins seeded
/// into the catalog at startup, and the ones that were skipped.
///
/// A `.wasm` file with a non-UTF8 name or that cannot be read is skipped with a
/// `tracing::warn!` and startup continues.  The plug-in is simply not in the
/// catalog, and the failure surfaces much later as a drive that cannot route to
/// `system/<name>` — far from the cause.  The startup log line reports only the
/// count that succeeded, so a run that seeded 3 of 4 is indistinguishable from
/// one that seeded 3 of 3.
pub fn system_plugin_seed_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_system_plugin_seed_total",
                "Built-in wasm plug-ins seeded at startup, by outcome (noetl/ai-meta#238).",
            ),
            &["outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Every outcome [`system_plugin_seed_total`] records.
pub const SYSTEM_PLUGIN_SEED_OUTCOMES: [&str; 3] =
    ["seeded", "skipped_non_utf8", "skipped_unreadable"];

/// Materialise every [`SYSTEM_PLUGIN_SEED_OUTCOMES`] series at 0.
pub fn init_system_plugin_seed_series() {
    for outcome in SYSTEM_PLUGIN_SEED_OUTCOMES {
        system_plugin_seed_total()
            .with_label_values(&[outcome])
            .inc_by(0);
    }
}

/// Record one plug-in seed outcome.
pub fn record_system_plugin_seed(outcome: &str) {
    system_plugin_seed_total()
        .with_label_values(&[outcome])
        .inc();
}

/// Every `outcome` [`secret_refresh_total`] records.
///
/// NOT derivable by scanning call-site literals: two of the five
/// (`succeeded`, `failed`) are assigned to a local in a `match` and passed as a
/// variable, so a literal scan finds three and would pin a set that is short by
/// two — with the missing ones absent from `/metrics` while the rest read 0.
/// Enumerated by reading `services/credential.rs` and `services/keychain.rs`,
/// and deliberately excluded from the scan-based check in
/// `playbooks/lib/pinned_sets.py` for the same reason.
pub const SECRET_REFRESH_OUTCOMES: [&str; 5] = [
    "triggered",
    "stampede_collapsed",
    "succeeded",
    "failed",
    "decision_failed",
];

/// Materialise every [`SECRET_REFRESH_OUTCOMES`] series at 0.
pub fn init_secret_refresh_series() {
    for outcome in SECRET_REFRESH_OUTCOMES {
        secret_refresh_total()
            .with_label_values(&[outcome])
            .inc_by(0);
    }
}

/// Every `status` the sealed-credential endpoint records, taken from the call
/// sites in [`crate::handlers::credentials`].
///
/// This endpoint hands a worker an X25519-sealed credential (Secrets Wallet
/// Phase 5b, noetl/ai-meta#61), so its failure modes are security-relevant:
/// `residency_violation` is a policy denial, `no_pubkey` and `worker_not_found`
/// are callers that could not be addressed, and `seal_error` is the crypto
/// path failing.  A healthy deployment that has simply not sealed anything yet
/// and a deployment where the endpoint is not reachable both rendered as an
/// empty scrape, because a labelled counter has no series until incremented.
///
/// Note the name mismatch that hid this: the metric is
/// `noetl_credentials_sealed_total` and the recorder is
/// `record_credential_seal` — singular, and not a prefix of the metric.  A
/// search keyed on the metric name finds nothing outside `metrics.rs` and
/// reads as a dead recorder.
pub const CREDENTIAL_SEAL_STATUSES: [&str; 7] = [
    "ok",
    "ok_via_broker",
    "no_pubkey",
    "worker_not_found",
    "residency_violation",
    "credential_error",
    "seal_error",
];

/// Materialise every [`CREDENTIAL_SEAL_STATUSES`] series at 0.
pub fn init_credential_seal_series() {
    for status in CREDENTIAL_SEAL_STATUSES {
        credentials_sealed_total()
            .with_label_values(&[status])
            .inc_by(0);
    }
}

/// Increment [`credentials_sealed_total`] by 1 for the given outcome.
pub fn record_credential_seal(status: &str) {
    credentials_sealed_total()
        .with_label_values(&[status])
        .inc();
}

/// Counter: synchronous auth fast-path calls (noetl/ai-meta#167 structural
/// cure).  Bucketed by `operation` (`validate` / `login`) and `outcome`
/// (`valid` / `invalid` / `authenticated` / `error`).  This is the drive-immune
/// in-process path the gateway takes under `NOETL_AUTH_SYNC=true`; the counter
/// makes it visible whether prod auth traffic is served synchronously (fast) or
/// still falling to the off-server drive (the recurring-lockout path).
pub fn auth_sync_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_auth_sync_total",
                "Synchronous in-process auth calls by operation (validate/login/check_access) and outcome.",
            ),
            &["operation", "outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Increment [`auth_sync_total`] by 1 for the given operation + outcome.
pub fn record_auth_sync(operation: &str, outcome: &str) {
    auth_sync_total()
        .with_label_values(&[operation, outcome])
        .inc();
}

/// Counter: Auth0 ID-token signature verification attempts (noetl/ai-meta#169),
/// bucketed by `mode` (`shadow` / `enforce`) and `outcome` (`success` /
/// `bad_signature` / `bad_claims` / `unknown_kid` / `malformed` /
/// `jwks_unavailable` / `no_domain`).  Shipped dark behind
/// `NOETL_AUTH_VERIFY_SIGNATURE`; the counter makes the canary observable —
/// during `shadow` rollout a nonzero non-`success` count means real prod tokens
/// would be rejected under `enforce`, so DO NOT flip until this reads
/// success-only for live traffic.
pub fn auth_jwt_verify_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_auth_jwt_verify_total",
                "Auth0 ID-token signature verification attempts by mode (shadow/enforce) and outcome.",
            ),
            &["mode", "outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Increment [`auth_jwt_verify_total`] by 1 for the given mode + outcome.
pub fn record_jwt_verify(mode: &str, outcome: &str) {
    auth_jwt_verify_total()
        .with_label_values(&[mode, outcome])
        .inc();
}

/// Counter: JWKS cache events (noetl/ai-meta#169), bucketed by `event`
/// (`cache_hit` / `cache_miss` / `unknown_kid_refresh`).  `unknown_kid_refresh`
/// is the Auth0 key-rotation signal — a spike means keys rotated and the cache
/// re-fetched to pick up the new `kid`.
pub fn auth_jwks_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_auth_jwks_total",
                "Auth0 JWKS cache events by type (cache_hit/cache_miss/unknown_kid_refresh).",
            ),
            &["event"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Increment [`auth_jwks_total`] by 1 for the given cache event.
pub fn record_jwks_event(event: &str) {
    auth_jwks_total().with_label_values(&[event]).inc();
}

/// Secrets-Wallet Phase 6a: per-resolve counter for keychain entries
/// against external secret providers.
///
/// Labels are bounded enums:
/// - `provider`: `gcp` / `aws` / `azure` / `vault` / `k8s` (the five
///   backends behind [`crate::secrets::SecretProvider`]).
/// - `region`: the secret's home region as it was resolved.  Falls back
///   to `"-"` when neither the keychain entry nor `NOETL_SERVER_REGION`
///   supplied one (legacy path; pre-6a behaviour).
/// - `status`: `ok` on a successful fetch; otherwise a failure-mode
///   label (`provider_build_error`, `provider_fetch_error`, `template_error`).
///
/// `execution_id` is NOT a label (cardinality) — it lives on the matching
/// span per [`agents/rules/observability.md`](https://github.com/noetl/ai-meta/blob/main/agents/rules/observability.md).  Region IS a label by design:
/// the cardinality is bounded (operators don't deploy into hundreds of
/// regions in practice), and per-region breakdown is exactly what an
/// operator queries when troubleshooting a residency-related outage.
pub fn secret_resolve_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_secret_resolve_total",
                "Keychain-entry resolutions against external secret providers, by \
                 provider + region + outcome.",
            ),
            &["provider", "region", "status"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Increment [`secret_resolve_total`] by 1 for the given outcome.
///
/// `region` may be empty — pass `"-"` (the convention used here) when no
/// region was supplied, to keep the label cardinality bounded.
pub fn record_secret_resolve(provider: &str, region: &str, status: &str) {
    let region_label = if region.is_empty() { "-" } else { region };
    secret_resolve_total()
        .with_label_values(&[provider, region_label, status])
        .inc();
}

/// Secrets-Wallet Phase 6b: per-`(provider, region)` provider-build counter.
///
/// `status`:
/// - `cache_hit` — the registry returned an existing entry.
/// - `ok` — a new provider was built and inserted.
/// - `error` — `build_secret_provider_for_region` failed; the cache is
///   unchanged.
///
/// Together with [`secret_resolve_total`] this answers two operator questions:
/// "Is the cache effective?" (`cache_hit / (ok + cache_hit)` ratio) and
/// "Is a region's provider down?" (`error` per-region rate).
pub fn secret_provider_build_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_secret_provider_build_total",
                "ProviderRegistry get_or_build outcomes per (provider, region).",
            ),
            &["provider", "region", "status"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Increment [`secret_provider_build_total`] by 1.
pub fn record_secret_provider_build(provider: &str, region: &str, status: &str) {
    let region_label = if region.is_empty() { "-" } else { region };
    secret_provider_build_total()
        .with_label_values(&[provider, region_label, status])
        .inc();
}

/// Secrets-Wallet Phase 6b: histogram of secret-resolve wall-clock latency,
/// keyed by `(provider, region)`.  Bucketed to span the 5 ms – 5 s range
/// where cloud secret managers and Vault clusters actually live.
///
/// `execution_id` is NOT a label — it lives on the matching `secret.resolve`
/// span per [`agents/rules/observability.md`](https://github.com/noetl/ai-meta/blob/main/agents/rules/observability.md) Principle 4.
pub fn secret_resolve_duration_seconds() -> &'static HistogramVec {
    static M: OnceLock<HistogramVec> = OnceLock::new();
    M.get_or_init(|| {
        let h = HistogramVec::new(
            HistogramOpts::new(
                "noetl_secret_resolve_duration_seconds",
                "Wall-clock seconds spent resolving one keychain entry against \
                 its provider.",
            )
            .buckets(vec![
                0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0,
            ]),
            &["provider", "region"],
        )
        .expect("static histogram spec must be valid");
        registry()
            .register(Box::new(h.clone()))
            .expect("histogram registration must succeed");
        h
    })
}

/// Observe one resolve duration on the [`secret_resolve_duration_seconds`]
/// histogram.
pub fn record_secret_resolve_duration(provider: &str, region: &str, seconds: f64) {
    let region_label = if region.is_empty() { "-" } else { region };
    secret_resolve_duration_seconds()
        .with_label_values(&[provider, region_label])
        .observe(seconds);
}

/// Secrets-Wallet Phase 6c: residency-policy gate outcomes.
///
/// Labels are bounded enums:
/// - `policy`: `none` / `advisory` / `strict` — the `KeychainDef.residency`
///   value at evaluation time.
/// - `decision`: one of `allowed_no_policy` / `allowed_same_region` /
///   `allowed_in_allowlist` / `violation_allowed` / `violation_blocked`.
///
/// `policy="strict"` + `decision="violation_blocked"` is the alert-worthy
/// combination — it means the gate refused a resolution that would have
/// crossed a residency boundary.  `policy="advisory"` +
/// `decision="violation_allowed"` is the migration-window surface for
/// finding existing cross-region flows before flipping to `strict`.
pub fn secret_residency_check_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_secret_residency_check_total",
                "Residency-policy gate outcomes per keychain-entry \
                 resolution (Secrets Wallet Phase 6c).",
            ),
            &["policy", "decision"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Increment [`secret_residency_check_total`] by 1.
pub fn record_secret_residency_check(policy: &str, decision: &str) {
    secret_residency_check_total()
        .with_label_values(&[policy, decision])
        .inc();
}

/// Secrets-Wallet Phase 6d: histogram of issuer-reported dynamic-secret
/// time-to-expiry at resolution time.
///
/// Buckets span the common cloud-token TTLs:
/// `[60, 300, 900, 3600, 14400, 43200]` seconds = 1 min / 5 min / 15 min /
/// 1 h / 4 h / 12 h.  An operator watching this dashboard sees whether
/// their fleet is hot-pathing through short-lived creds (most calls
/// landing in the 1 min – 15 min buckets) or running off long-lived ones
/// (12 h+).
///
/// No labels: the metric tells a fleet-wide story; per-credential
/// inspection lives on the matching `secret.resolve` tracing span.
pub fn secret_dynamic_ttl_seconds() -> &'static prometheus::Histogram {
    static M: OnceLock<prometheus::Histogram> = OnceLock::new();
    M.get_or_init(|| {
        let h = prometheus::Histogram::with_opts(
            HistogramOpts::new(
                "noetl_secret_dynamic_ttl_seconds",
                "Issuer-reported time-to-expiry of resolved dynamic secrets (Phase 6d).",
            )
            .buckets(vec![60.0, 300.0, 900.0, 3600.0, 14400.0, 43200.0]),
        )
        .expect("static histogram spec must be valid");
        registry()
            .register(Box::new(h.clone()))
            .expect("histogram registration must succeed");
        h
    })
}

/// Observe one issuer-reported TTL (seconds).  Caller filters to the
/// dynamic-secret case (i.e. only when `SecretValue.expires_at` was set).
pub fn record_secret_dynamic_ttl(seconds: f64) {
    secret_dynamic_ttl_seconds().observe(seconds);
}

/// Secrets-Wallet Phase 6d: counter for keychain-cache writes the
/// resolver skipped.
///
/// `reason` is a bounded enum:
/// - `already_expired` — issuer's `expires_at` already in the past or
///   within the safety margin.  Caching would store something already
///   dead.
///
/// Future 6d-follow-up reasons may include `unsupported_scope`, etc.
pub fn secret_cache_skip_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_secret_cache_skip_total",
                "Keychain-cache writes skipped by reason (Phase 6d).",
            ),
            &["reason"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Increment [`secret_cache_skip_total`] by 1.
pub fn record_secret_cache_skip(reason: &str) {
    secret_cache_skip_total().with_label_values(&[reason]).inc();
}

/// Secrets-Wallet Phase 6e: cross-region broker call outcomes.
///
/// `broker_region` is the region the request was routed to (or `"-"`
/// for diagnostics paths that don't know).  `outcome` is a bounded
/// enum:
/// - `ok` — broker sealed the response and returned it.
/// - `unreachable` — network / DNS / TLS / 5xx from the broker.
/// - `denied_by_broker` — broker rejected the request (its own region
///   gate or local policy).
/// - `wrong_region` — broker's `server_region()` didn't match the
///   requested `expected_entry_region`.
/// - `bad_pubkey` — requesting peer sent a malformed worker public key.
/// - `resolve_error` / `serialize_error` / `seal_error` — broker-side
///   pipeline errors.
///
/// `wrong_region` is the alert-worthy combination — it means a peer's
/// broker registry is out of date.
pub fn cross_region_broker_call_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_secret_broker_call_total",
                "Cross-region broker call outcomes per broker_region (Phase 6e).",
            ),
            &["broker_region", "outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Increment [`cross_region_broker_call_total`] by 1.
pub fn record_cross_region_broker_call(broker_region: &str, outcome: &str) {
    let region_label = if broker_region.is_empty() {
        "-"
    } else {
        broker_region
    };
    cross_region_broker_call_total()
        .with_label_values(&[region_label, outcome])
        .inc();
}

/// Secrets-Wallet Phase 6e: histogram of cross-region broker call
/// wall-clock latency.  Buckets span the cross-region round-trip range
/// (`[0.05, 0.1, 0.25, 0.5, 1, 2, 5]`).  Caller observes regardless of
/// outcome so a dashboard shows "broker is slow" + "broker is failing"
/// independently.
pub fn cross_region_broker_call_duration_seconds() -> &'static HistogramVec {
    static M: OnceLock<HistogramVec> = OnceLock::new();
    M.get_or_init(|| {
        let h = HistogramVec::new(
            HistogramOpts::new(
                "noetl_secret_broker_call_duration_seconds",
                "Wall-clock seconds spent in a cross-region broker call.",
            )
            .buckets(vec![0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0]),
            &["broker_region"],
        )
        .expect("static histogram spec must be valid");
        registry()
            .register(Box::new(h.clone()))
            .expect("histogram registration must succeed");
        h
    })
}

/// Observe one cross-region broker call duration.
pub fn record_cross_region_broker_call_duration(broker_region: &str, seconds: f64) {
    let region_label = if broker_region.is_empty() {
        "-"
    } else {
        broker_region
    };
    cross_region_broker_call_duration_seconds()
        .with_label_values(&[region_label])
        .observe(seconds);
}

/// Secrets-Wallet Phase 7a: wallet KEK-rotation pass outcomes.
///
/// `table` is `credential` or `keychain` (the two `noetl.*` tables that
/// hold envelope-encrypted blobs).  `status` is a bounded enum:
/// - `skipped` — record already wrapped under the current KEK version.
/// - `rewrapped` — DEK was unwrapped under the old version and re-wrapped
///   under the current.
/// - `failed_unwrap` — provider can't produce the old KEK version (key
///   compromise + delete-all rotation; operator must reseed).
/// - `failed_wrap` — provider can't issue a fresh wrap (KMS reachability).
/// - `parse_error` — stored value isn't a valid envelope (forward-only
///   contract — re-register the record).
///
/// `failed_unwrap` is the alert-worthy combination — it means the
/// rotation can't complete without operator intervention.
pub fn wallet_rotate_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_wallet_rotate_total",
                "Wallet KEK-rotation pass outcomes per table (Phase 7a).",
            ),
            &["table", "status"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Increment [`wallet_rotate_total`] by 1.
pub fn record_wallet_rotate(table: &str, status: &str) {
    wallet_rotate_total()
        .with_label_values(&[table, status])
        .inc();
}

/// Secrets-Wallet Phase 7b: secret-resolution audit-write outcomes.
///
/// Labels are bounded enums:
/// - `operation`: matches `services::secret_audit::Operation::as_str` —
///   `get_sealed` / `cross_region_broker_serve` / `resolve_keychain` /
///   `get_credential`.
/// - `outcome`: the resolver's actual outcome at audit time —
///   `ok` / `residency_violation` / `broker_unreachable` / etc.
///   (mirrors `services::secret_audit::Outcome::as_str`).
/// - `status`: what happened to the audit write itself —
///   - `written` — sink confirmed the row landed.
///   - `dropped_async` — fire-and-forget write failed (logged + dropped).
///   - `failed_strict` — `NOETL_SECRET_AUDIT_REQUIRED=true` and the
///     sink errored.  **Alert-worthy.**
pub fn secret_audit_writes_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_secret_audit_writes_total",
                "Secret-resolution audit-write outcomes (Phase 7b).",
            ),
            &["operation", "outcome", "status"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Increment [`secret_audit_writes_total`] by 1.
pub fn record_secret_audit_write(operation: &str, outcome: &str, status: &str) {
    secret_audit_writes_total()
        .with_label_values(&[operation, outcome, status])
        .inc();
}

/// Secrets-Wallet Phase 7c: token auto-renewal outcomes.
///
/// `outcome` is a bounded enum:
/// - `triggered` — refresh decision made (will spawn or collapse).
/// - `succeeded` — refresh ran and the new value landed in the cache.
/// - `failed` — refresh ran but the provider errored.  The cached
///   row is **preserved** (we don't poison the cache on a transient
///   outage; the next natural cache miss after `expires_at` re-resolves).
/// - `stampede_collapsed` — concurrent request found a refresh already
///   in flight; piggy-backed on it.
///
/// `failed` at sustained rate is alert-worthy — it means a provider
/// is unreachable AND a cached token is about to expire.
pub fn secret_refresh_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_secret_refresh_total",
                "Token auto-renewal outcomes (Phase 7c).  Aliases are NOT \
                 labeled (cardinality); per-alias detail lives on the \
                 secret.refresh tracing span.",
            ),
            &["outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Increment [`secret_refresh_total`] by 1.
pub fn record_secret_refresh(outcome: &str) {
    secret_refresh_total().with_label_values(&[outcome]).inc();
}

// ---------------------------------------------------------------------------
// Result-store metrics (noetl/ai-meta#70)
// ---------------------------------------------------------------------------

/// Counter: `PUT /api/result/{execution_id}` calls by outcome status.
///
/// `status` ∈ { `ok`, `error` }.
/// `execution_id` is NOT a label (cardinality) — it lives on the
/// `result_store.put` tracing span per
/// [`agents/rules/observability.md`](https://github.com/noetl/ai-meta/blob/main/agents/rules/observability.md) Principle 4.
pub fn result_store_put_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_result_store_put_total",
                "PUT /api/result/{execution_id} calls by outcome status.",
            ),
            &["status"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Histogram: wall-clock time spent inside `PUT /api/result/{eid}`.
pub fn result_store_put_duration_seconds() -> &'static HistogramVec {
    static M: OnceLock<HistogramVec> = OnceLock::new();
    M.get_or_init(|| {
        let h = HistogramVec::new(
            HistogramOpts::new(
                "noetl_result_store_put_duration_seconds",
                "Wall-clock seconds for PUT /api/result/{execution_id}.",
            )
            .buckets(EVENT_INGEST_BUCKETS.to_vec()),
            &["status"],
        )
        .expect("static histogram spec must be valid");
        registry()
            .register(Box::new(h.clone()))
            .expect("histogram registration must succeed");
        h
    })
}

/// Record one `PUT /api/result/{eid}` outcome.
///
/// `bytes` is the stored payload size (0 on error paths).
/// `status` is `"ok"` or `"error"`.
pub fn record_result_store_put(duration_seconds: f64, bytes: usize, status: &str) {
    result_store_put_total().with_label_values(&[status]).inc();
    result_store_put_duration_seconds()
        .with_label_values(&[status])
        .observe(duration_seconds);
    // Log bytes as a span field in the handler; Prometheus metric
    // stays label-free on bytes (unbounded cardinality for a gauge).
    let _ = bytes; // consumed for future histogram extension
}

/// Counter: `GET /api/result/resolve` calls by outcome status.
///
/// `status` ∈ { `ok`, `not_found`, `bad_request`, `error` }.
pub fn result_store_resolve_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_result_store_resolve_total",
                "GET /api/result/resolve calls by outcome status.",
            ),
            &["status"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Histogram: wall-clock time for `GET /api/result/resolve`.
pub fn result_store_resolve_duration_seconds() -> &'static HistogramVec {
    static M: OnceLock<HistogramVec> = OnceLock::new();
    M.get_or_init(|| {
        let h = HistogramVec::new(
            HistogramOpts::new(
                "noetl_result_store_resolve_duration_seconds",
                "Wall-clock seconds for GET /api/result/resolve.",
            )
            .buckets(EVENT_INGEST_BUCKETS.to_vec()),
            &["status"],
        )
        .expect("static histogram spec must be valid");
        registry()
            .register(Box::new(h.clone()))
            .expect("histogram registration must succeed");
        h
    })
}

/// Record one `GET /api/result/resolve` outcome.
pub fn record_result_store_resolve(duration_seconds: f64, status: &str) {
    result_store_resolve_total()
        .with_label_values(&[status])
        .inc();
    result_store_resolve_duration_seconds()
        .with_label_values(&[status])
        .observe(duration_seconds);
}

/// `noetl_object_store_ops_total{backend,op,outcome}` — object-store backend I/O
/// (RFC #104 Phase C). `backend` is `postgres`/`gcs`, `op` is `put`/`get`,
/// `outcome` is `ok`/`error`. The GCS-backend deltas are the proof the Feather
/// tier read/write goes through GCS under the flag.
pub fn object_store_ops_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_object_store_ops_total",
                "Object-store backend operations by backend, op, and outcome (RFC #104 Phase C).",
            ),
            &["backend", "op", "outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one object-store backend operation outcome.
pub fn record_object_store_op(backend: &str, op: &str, ok: bool) {
    object_store_ops_total()
        .with_label_values(&[backend, op, if ok { "ok" } else { "error" }])
        .inc();
}

/// `noetl_registry_ops_total{op,outcome}` — model/dataset/eval/release registry
/// operations (RFC #146 platform foundation G3). `op` is
/// `register`/`list`/`resolve`; `outcome` is `ok`/`error`. The deltas prove the
/// SLM MLOps stages' registry writes + resolves go through the server API
/// (data-access-boundary.md). Gated behind `NOETL_REGISTRY_ENABLED`, so this
/// counter is flat on default deployments.
pub fn registry_ops_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_registry_ops_total",
                "Registry operations by op and outcome (RFC #146 G3).",
            ),
            &["op", "outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one registry operation outcome.
pub fn record_registry_op(op: &str, ok: bool) {
    registry_ops_total()
        .with_label_values(&[op, if ok { "ok" } else { "error" }])
        .inc();
}

/// `noetl_cell_registry_requests_total` — `GET /api/internal/cells` hits (RFC
/// #104 Phase C). The resolve-by-URN read path consults the registry once per
/// process (cached), so this is low-volume; its delta proves the read side is
/// wired to the server-served registry rather than local env.
pub fn cell_registry_requests_total() -> &'static prometheus::IntCounter {
    static M: OnceLock<prometheus::IntCounter> = OnceLock::new();
    M.get_or_init(|| {
        let counter = prometheus::IntCounter::new(
            "noetl_cell_registry_requests_total",
            "GET /api/internal/cells requests served (RFC #104 Phase C cell endpoint registry).",
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one cell-registry request.
pub fn record_cell_registry_request() {
    cell_registry_requests_total().inc();
}

/// `noetl_result_tier_gc_total{outcome}` — result-tier GC sweep outcomes (RFC
/// #104 Phase F). `outcome` is `no_op` (gate off), `scanned`, `deleted`,
/// `skipped_live`, `skipped_grace`, `skipped_unparseable`, or `error`. The
/// `skipped_live` series is the proof the sweep never reclaims a live-referenced
/// object; `deleted` advances only when a provably-dead object is removed.
pub fn result_tier_gc_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_result_tier_gc_total",
                "Result-tier GC sweep outcomes by outcome (RFC #104 Phase F).",
            ),
            &["outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record `n` result-tier GC outcomes of a kind (a sweep records one delta per
/// outcome class, so the counter sums cleanly across sweeps).
pub fn record_result_tier_gc(outcome: &str, n: u64) {
    if n > 0 {
        result_tier_gc_total()
            .with_label_values(&[outcome])
            .inc_by(n);
    }
}

/// `noetl_sink_state_total{op}` — sink-state feed operations (noetl/ai-meta#199
/// Slice B). `op` is `mark` (a worker reported an execution pending-sink),
/// `confirm` (a worker cleared one — context sunk), or `gc_consult` (the
/// result-tier GC read the pending set for a sweep). The write-behind-cache
/// invariant's server-visible signal is observable here before the eviction gate
/// is switched on.
pub fn sink_state_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_sink_state_total",
                "Sink-state feed operations by op — mark / confirm / gc_consult (noetl/ai-meta#199).",
            ),
            &["op"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one sink-state feed operation (`mark` / `confirm` / `gc_consult`).
/// Every `op` value `noetl_sink_state_total` can take (noetl/ai-meta#248).
///
/// `release` was added with the endpoint that clears a mark WITHOUT claiming the
/// context was sunk.  Pinning matters more than usual here: the whole point of
/// the feed is that a *retained* execution is visible, so an operator asking
/// "did anything give up on a sink?" must be able to read a 0 rather than an
/// absent series.
pub const SINK_STATE_OPS: [&str; 5] = [
    "mark",
    "confirm",
    "release",
    "gc_consult",
    "gc_feed_truncated",
];

/// Materialise every [`SINK_STATE_OPS`] series at 0.
pub fn init_sink_state_series() {
    for op in SINK_STATE_OPS {
        sink_state_total().with_label_values(&[op]).inc_by(0);
    }
}

/// `noetl_catalog_delete_total{outcome}` — catalog rows removed via
/// `POST /api/catalog/delete` (noetl/ai-meta#237).
///
/// `noetl.catalog` is not event-sourced and has no replay, so a delete leaves
/// no trace anywhere else.  This counter plus the INFO log at the call site are
/// the audit trail.
pub fn catalog_delete_total() -> &'static IntCounterVec {
    static M: std::sync::OnceLock<IntCounterVec> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        let m = IntCounterVec::new(
            prometheus::Opts::new(
                "noetl_catalog_delete_total",
                "Catalog delete operations by outcome (noetl/ai-meta#237).",
            ),
            &["outcome"],
        )
        .expect("catalog_delete_total metric");
        registry()
            .register(Box::new(m.clone()))
            .expect("register catalog_delete_total");
        m
    })
}

/// Every `outcome` value `catalog_delete_total` can take.
pub const CATALOG_DELETE_OUTCOMES: [&str; 6] = [
    "deleted",
    "no_match",
    "archived",
    "already_archived",
    "restored",
    "archive_unavailable",
];

/// Materialise both [`CATALOG_DELETE_OUTCOMES`] series at 0.
///
/// A destructive endpoint whose counter is absent until the first delete cannot
/// answer "has anything been removed from the catalog?" — the question an
/// operator asks first, and the one where absent must not read as zero.
pub fn init_catalog_delete_series() {
    for outcome in CATALOG_DELETE_OUTCOMES {
        catalog_delete_total()
            .with_label_values(&[outcome])
            .inc_by(0);
    }
}

/// Record one catalog delete outcome.
pub fn record_catalog_delete(outcome: &str) {
    catalog_delete_total().with_label_values(&[outcome]).inc();
}

pub fn record_sink_state(op: &str) {
    sink_state_total().with_label_values(&[op]).inc();
}

/// Secrets-Wallet Phase 7c: histogram of token auto-renewal wall-clock
/// latency.  Buckets `[0.05, 0.1, 0.25, 0.5, 1, 2, 5]` — span the range
/// where auth round-trips actually live.  Observed regardless of
/// outcome so a dashboard surfaces "refresh is slow" + "refresh is
/// failing" independently.
pub fn secret_refresh_duration_seconds() -> &'static prometheus::Histogram {
    static M: OnceLock<prometheus::Histogram> = OnceLock::new();
    M.get_or_init(|| {
        let h = prometheus::Histogram::with_opts(
            HistogramOpts::new(
                "noetl_secret_refresh_duration_seconds",
                "Wall-clock seconds spent in one token auto-renewal (Phase 7c).",
            )
            .buckets(vec![0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0]),
        )
        .expect("static histogram spec must be valid");
        registry()
            .register(Box::new(h.clone()))
            .expect("histogram registration must succeed");
        h
    })
}

/// Observe one refresh duration.
pub fn record_secret_refresh_duration(seconds: f64) {
    secret_refresh_duration_seconds().observe(seconds);
}

// ---------------------------------------------------------------------------
// Subscription scale hardening (noetl/ai-meta#90 Phase 7)
// ---------------------------------------------------------------------------

/// Counter: executions created, bucketed by the `/api/execute` entry path
/// (`single` | `batch`) and the dedup outcome (`new` | `duplicate` | `error`).
/// Lets an operator see batch-dispatch uptake and how often the opt-in dedup
/// window is collapsing duplicates without grepping logs.
pub fn execute_outcomes_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_execute_outcomes_total",
                "Executions handled by /api/execute(/batch), bucketed by entry path and dedup outcome (noetl/ai-meta#90 Phase 7).",
            ),
            &["entry", "outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Histogram: `POST /api/execute/batch` request sizes (number of executions in
/// one HTTP round-trip).  Answers "is the runtime actually batching, and how
/// deep" — the whole point of Phase 7's batch dispatch.
pub fn execute_batch_size() -> &'static HistogramVec {
    static M: OnceLock<HistogramVec> = OnceLock::new();
    M.get_or_init(|| {
        let hist = HistogramVec::new(
            HistogramOpts::new(
                "noetl_execute_batch_size",
                "Number of executions submitted in one POST /api/execute/batch call (noetl/ai-meta#90 Phase 7).",
            )
            .buckets(vec![1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0]),
            &[],
        )
        .expect("static histogram spec must be valid");
        registry()
            .register(Box::new(hist.clone()))
            .expect("histogram registration must succeed");
        hist
    })
}

/// Record one execution outcome.  `entry` is `"single"` or `"batch"`;
/// `outcome` is `"new"` (execution created), `"duplicate"` (dedup window
/// collapsed it), or `"error"`.
pub fn record_execute_outcome(entry: &str, outcome: &str) {
    execute_outcomes_total()
        .with_label_values(&[entry, outcome])
        .inc();
}

/// Observe one batch-dispatch request size.
pub fn record_execute_batch_size(n: usize) {
    execute_batch_size()
        .with_label_values(&[])
        .observe(n as f64);
}

/// Render the global registry as Prometheus text-exposition
/// format.  Used by the `GET /metrics` handler.
pub fn gather_text() -> Result<String, prometheus::Error> {
    let encoder = TextEncoder::new();
    let metric_families = registry().gather();
    encoder.encode_to_string(&metric_families)
}

// ---------------------------------------------------------------------------
// Container Tool Callback (noetl/ai-meta#43 Round 2 — noetl/server#140)
// ---------------------------------------------------------------------------

/// Counter for in-flight container-callback emits, labelled by terminal
/// state.  Sister metric to [`container_callback_stale_total`] (stale path).
/// Together they answer "how many Job terminations did the watcher
/// observe, and what fraction matched a live execution".
pub fn container_callback_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_container_callback_total",
                "Container-tool callback receives that matched an in-flight \
                 execution and emitted a call.done event (Container Tool \
                 Callback umbrella, noetl/ai-meta#43).",
            ),
            &["state"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Increment [`container_callback_total`] by 1.
pub fn record_container_callback(state: &str) {
    container_callback_total().with_label_values(&[state]).inc();
}

/// Counter for stale container-callback receives — Job terminations the
/// watcher observed for executions that don't exist on this server.
/// **Alert-worthy when sustained** — usually means a stale watcher
/// pointing at the wrong namespace, or a Job created out-of-band.
pub fn container_callback_stale_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_container_callback_stale_total",
                "Container-tool callback receives that did NOT match any \
                 in-flight execution (stale — execution gc'd, watcher \
                 mis-namespaced, or Job created out-of-band).",
            ),
            &["state"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Increment [`container_callback_stale_total`] by 1.
pub fn record_container_callback_stale(state: &str) {
    container_callback_stale_total()
        .with_label_values(&[state])
        .inc();
}

// ===========================================================================
// EHDB cross-store parity (noetl/ai-meta#258).
//
// The signal that gates the first EHDB primary tier flip. Three families, and
// the third is the one that makes the other two mean anything.
// ===========================================================================

/// Counter: one verdict per execution compared across `noetl.event` and the
/// EHDB event-log tier.
///
/// `outcome` distinguishes agreement from every distinct way of *not knowing*
/// (see [`crate::handlers::ehdb_parity::ParityOutcome`]). That distinction is
/// the point of the metric: the signal it replaces
/// (`noetl_ehdb_eventlog_ops_total{outcome="mirrored"}`) could not tell "the
/// stores agree" from "the tier was never asked".
pub fn ehdb_crossstore_parity_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_ehdb_crossstore_parity_total",
                "Cross-store parity verdicts comparing the EHDB tier against the authoritative \
                 noetl.event log, by tier and outcome (noetl/ai-meta#258).",
            ),
            &["tier", "outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one cross-store parity verdict.
pub fn record_ehdb_crossstore_parity(tier: &str, outcome: &str) {
    ehdb_crossstore_parity_total()
        .with_label_values(&[tier, outcome])
        .inc();
}

/// Counter: divergences found, by kind.
///
/// One increment per *kind* per execution, not per offending record — the
/// question this answers is "at what rate do executions diverge, and how", and
/// counting records would let a single badly-corrupted execution dominate the
/// rate.
pub fn ehdb_crossstore_divergence_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_ehdb_crossstore_divergence_total",
                "Cross-store divergences between the EHDB tier and noetl.event, by tier and \
                 kind (noetl/ai-meta#258).",
            ),
            &["tier", "kind"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one divergence kind on one execution.
pub fn record_ehdb_crossstore_divergence(tier: &str, kind: &str) {
    ehdb_crossstore_divergence_total()
        .with_label_values(&[tier, kind])
        .inc();
}

/// Counter: events whose identifying fields matched across both stores.
///
/// The denominator. Without it a zero divergence rate has no scale — one
/// execution compared and a million are the same reading.
pub fn ehdb_crossstore_events_compared_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_ehdb_crossstore_events_compared_total",
                "Events matched across the EHDB tier and noetl.event, by tier \
                 (noetl/ai-meta#258).",
            ),
            &["tier"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Add to the matched-event count.
pub fn add_ehdb_crossstore_events_compared(tier: &str, n: u64) {
    ehdb_crossstore_events_compared_total()
        .with_label_values(&[tier])
        .inc_by(n);
}

/// Counter: the comparator's own controls — the anti-vacuity signal.
///
/// `result="unexpected"` means the comparator failed to behave on a synthetic
/// input whose answer is known: either it missed a deliberately planted
/// divergence, or it invented one on a clean pair. Either way every zero it has
/// published is void, which is a different and much worse situation than a
/// divergence.
///
/// The reading an operator wants before a tier flip is
/// `expected > 0 AND unexpected == 0` — that is what turns
/// `noetl_ehdb_crossstore_divergence_total == 0` from an absence into evidence.
pub fn ehdb_crossstore_control_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_ehdb_crossstore_control_total",
                "Cross-store comparator self-controls: synthetic inputs with known answers, by \
                 control and whether the comparator behaved (noetl/ai-meta#258).",
            ),
            &["control", "result"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one control verdict (`expected` | `unexpected`).
pub fn record_ehdb_crossstore_control(control: &str, result: &str) {
    ehdb_crossstore_control_total()
        .with_label_values(&[control, result])
        .inc();
}

/// Materialise every cross-store parity series at 0.
///
/// `Registry::gather` prunes families with no children, so a labelled counter
/// that has never been incremented is **absent** from `/metrics`, not zero. On
/// this metric that is intolerable: absence would be equally consistent with
/// "no divergence has ever been found", "the comparator is off", and "this
/// binary predates the comparator". Pinning collapses the first two into a
/// readable 0 and leaves absence to mean only the third — and
/// `noetl_server_build_info{version}` answers that.
///
/// Pinned **unconditionally**, outside any config branch. A pin placed inside
/// `if enabled` is not a pin: it would leave exactly the disabled server —
/// the one an operator is checking before turning the comparator on — showing
/// nothing.
///
/// Every label value comes from the call sites' own enums
/// ([`crate::handlers::ehdb_parity::PARITY_OUTCOMES`], `DIVERGENCE_KINDS`,
/// `CONTROL_NAMES`), not from a hand-maintained list in prose; the test below
/// asserts the enums and the pinned set cannot drift apart.
/// Counter: authoritative events the server mirrored into the event-log tier.
///
/// Labelled by outcome over a closed set, and counted in **events** rather than
/// batches: the number an operator needs is "did the tier receive everything the
/// log received", and a batch count cannot answer it.
pub fn ehdb_eventlog_mirror_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_ehdb_eventlog_mirror_total",
                "Authoritative events the SERVER mirrored into the EHDB event-log tier, by \
                 outcome (noetl/ai-meta#258).",
            ),
            &["outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Every **terminal** outcome label the server-side mirror can emit, one per
/// batch. Closed set, pinned at 0.
///
/// `dropped` is the one that matters: it is the count of authoritative events
/// that will never reach the tier, and before noetl/ai-meta#320 there was no
/// such label — a discarded batch was filed under `unavailable`, which reads
/// like a transport blip rather than data loss, and which no alert watched.
///
/// `unavailable` and `degraded` are still emitted here when retries are
/// disabled (`NOETL_EHDB_EVENTLOG_MIRROR_MAX_RETRIES=0`), because with no
/// retry the first failure *is* terminal. When retries are on, a failing
/// attempt is counted on
/// [`record_ehdb_eventlog_mirror_attempt`] instead and only the exhausted
/// batch lands here, as `dropped`.
pub const EHDB_EVENTLOG_MIRROR_OUTCOMES: [&str; 6] = [
    "mirrored",
    "recovered",
    "dropped",
    "unconfigured",
    "unavailable",
    "degraded",
];

/// Per-**attempt** failure labels, so a retry that eventually succeeds is still
/// visible. Without this the fix would hide the very instability it works
/// around: a pool flapping every ten minutes and a perfectly healthy one would
/// both read `mirrored`.
pub const EHDB_EVENTLOG_MIRROR_ATTEMPT_OUTCOMES: [&str; 2] = ["unavailable", "degraded"];

pub fn ehdb_eventlog_mirror_attempt_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let c = IntCounterVec::new(
            Opts::new(
                "noetl_ehdb_eventlog_mirror_attempt_total",
                "Individual failed mirror delivery attempts by outcome; a batch that \
                 succeeds on a retry increments this and then `mirror_total{outcome=\"recovered\"}`.",
            ),
            &["outcome"],
        )
        .expect("valid metric");
        registry().register(Box::new(c.clone())).ok();
        c
    })
}

pub fn record_ehdb_eventlog_mirror_attempt(outcome: &str, events: usize) {
    ehdb_eventlog_mirror_attempt_total()
        .with_label_values(&[outcome])
        .inc_by(events as u64);
}

pub fn record_ehdb_eventlog_mirror(outcome: &str, events: usize) {
    ehdb_eventlog_mirror_total()
        .with_label_values(&[outcome])
        .inc_by(events as u64);
}

/// Pin the server-side mirror's label set at 0.
///
/// `Registry::gather` prunes metric families with no children, so without this a
/// server that has mirrored nothing and a server that cannot mirror at all
/// produce byte-identical `/metrics` output. Pinned unconditionally — a pin
/// placed behind the mirror-source check would be absent on exactly the
/// configuration whose `unconfigured` count someone would be reading.
pub fn init_ehdb_eventlog_mirror_series() {
    for outcome in EHDB_EVENTLOG_MIRROR_OUTCOMES {
        ehdb_eventlog_mirror_total()
            .with_label_values(&[outcome])
            .inc_by(0);
    }
    for outcome in EHDB_EVENTLOG_MIRROR_ATTEMPT_OUTCOMES {
        ehdb_eventlog_mirror_attempt_total()
            .with_label_values(&[outcome])
            .inc_by(0);
    }
}

// ---------------------------------------------------------------------------
// Async mirror queue (noetl/ai-meta#155 Option 3).
// ---------------------------------------------------------------------------

/// Counter: what happened to a batch when it was handed to the mirror queue.
///
/// Deliberately a **second** family rather than more labels on
/// [`ehdb_eventlog_mirror_total`]. That one answers "did the tier receive
/// everything the log received", counted in events at delivery time; an
/// `enqueued` label on it would be counted twice for every event — once on
/// enqueue and once on delivery — and quietly destroy the only number that
/// answers the coverage question.
///
/// This family answers a different question: is the queue absorbing the load,
/// or is it pushing back? Counted in **events** for the same reason.
pub fn ehdb_eventlog_mirror_queue_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_ehdb_eventlog_mirror_queue_total",
                "Authoritative events by how the async event-log mirror queue handled them \
                 (noetl/ai-meta#155).",
            ),
            &["outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Every queue outcome. Closed set, pinned at 0.
///
/// * `enqueued` — accepted onto the queue immediately.
/// * `enqueued_after_wait` — the queue was full and the emit path **waited**
///   for room. This is backpressure working: no event was dropped and order is
///   preserved. Its rate is the signal that the drain is falling behind.
/// * `queue_full_inline` — the queue stayed full past the enqueue timeout, so
///   the batch was mirrored inline on the emit path instead. Still no drop, but
///   this is the one path that can deliver out of order relative to batches
///   still queued for the same execution, so it is its own label and it is what
///   the alert fires on.
/// * `queue_closed_inline` — no drain task (queue disabled mid-flight, or the
///   drainer is gone). Mirrored inline; the pre-Option-3 behaviour.
/// * `drained` — delivered by the drain task. The terminal success label.
/// * `shutdown_abandoned` — still queued when the process finished its
///   shutdown flush. These events never reached the tier and WILL show up as a
///   `missing_event` divergence; the label exists so the cause is attributable
///   rather than a mystery an hour later.
pub const EHDB_EVENTLOG_MIRROR_QUEUE_OUTCOMES: [&str; 6] = [
    "enqueued",
    "enqueued_after_wait",
    "queue_full_inline",
    "queue_closed_inline",
    "drained",
    "shutdown_abandoned",
];

pub fn record_ehdb_eventlog_mirror_queue(outcome: &str, events: usize) {
    ehdb_eventlog_mirror_queue_total()
        .with_label_values(&[outcome])
        .inc_by(events as u64);
}

/// Histogram: seconds from enqueue to durable in the tier.
///
/// **This is the metric the comparator's lag tolerance is checked against.**
/// The tolerance window (`NOETL_EHDB_CROSSSTORE_PARITY_LAG_TOLERANCE_SECS`)
/// tells the comparator to ignore events younger than N seconds; that is a
/// promise about how long the mirror can take, and without this histogram it
/// would be an assumption. An operator compares this histogram's tail against
/// the configured window, and the alert does it automatically.
///
/// Buckets run to 60s on purpose: the interesting readings are the slow ones,
/// and a histogram that tops out below the tolerance window cannot show a
/// breach of it.
pub fn ehdb_eventlog_mirror_lag_seconds() -> &'static Histogram {
    static M: OnceLock<Histogram> = OnceLock::new();
    M.get_or_init(|| {
        let hist = Histogram::with_opts(
            HistogramOpts::new(
                "noetl_ehdb_eventlog_mirror_lag_seconds",
                "Seconds from an authoritative event being enqueued for the async event-log \
                 mirror to it being durable in the tier (noetl/ai-meta#155).",
            )
            .buckets(vec![
                0.005, 0.010, 0.025, 0.050, 0.100, 0.250, 0.500, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
            ]),
        )
        .expect("static histogram spec must be valid");
        registry()
            .register(Box::new(hist.clone()))
            .expect("histogram registration must succeed");
        hist
    })
}

pub fn observe_ehdb_eventlog_mirror_lag(seconds: f64) {
    ehdb_eventlog_mirror_lag_seconds().observe(seconds);
}

/// Gauge: authoritative events enqueued for the mirror and not yet durable.
///
/// The coverage number. `mirror_lag_seconds` only records events that *made
/// it*, so a queue that has stopped draining entirely publishes no lag
/// observations at all — its histogram goes quiet, which reads exactly like an
/// idle healthy system. This gauge is what distinguishes the two, and it is why
/// the alert watches both.
pub fn ehdb_eventlog_mirror_pending_events() -> &'static IntGauge {
    static M: OnceLock<IntGauge> = OnceLock::new();
    M.get_or_init(|| {
        let g = IntGauge::new(
            "noetl_ehdb_eventlog_mirror_pending_events",
            "Authoritative events enqueued for the async event-log mirror and not yet durable \
             in the tier (noetl/ai-meta#155).",
        )
        .expect("static gauge spec must be valid");
        registry()
            .register(Box::new(g.clone()))
            .expect("gauge registration must succeed");
        g
    })
}

/// Gauge: batches sitting on the mirror queue.
pub fn ehdb_eventlog_mirror_queue_depth() -> &'static IntGauge {
    static M: OnceLock<IntGauge> = OnceLock::new();
    M.get_or_init(|| {
        let g = IntGauge::new(
            "noetl_ehdb_eventlog_mirror_queue_depth",
            "Batches currently on the async event-log mirror queue (noetl/ai-meta#155).",
        )
        .expect("static gauge spec must be valid");
        registry()
            .register(Box::new(g.clone()))
            .expect("gauge registration must succeed");
        g
    })
}

/// Gauge: 1 when the async mirror queue is armed in this process, 0 otherwise.
///
/// Without it, "the flag is off" and "the flag is on but the drain task never
/// started" produce the same scrape: an idle queue with zero depth. That is the
/// inert-and-silent shape this codebase has shipped three times already
/// (noetl/ai-meta#242, #266), so the arming is published rather than inferred.
pub fn ehdb_eventlog_mirror_async_enabled() -> &'static IntGauge {
    static M: OnceLock<IntGauge> = OnceLock::new();
    M.get_or_init(|| {
        let g = IntGauge::new(
            "noetl_ehdb_eventlog_mirror_async_enabled",
            "1 when the async event-log mirror queue is armed in this process, 0 when the \
             mirror is inline (noetl/ai-meta#155).",
        )
        .expect("static gauge spec must be valid");
        registry()
            .register(Box::new(g.clone()))
            .expect("gauge registration must succeed");
        g
    })
}

/// Gauge: how many executions one drain pass delivers concurrently.
///
/// ⚠ This exists because I shipped the knob without it. `DRAIN_CONCURRENCY` is
/// read **per pass** rather than at startup, and the ARMED log line reports
/// `capacity` / `drain_max` / `enqueue_timeout_ms` but not this — so after the
/// noetl/ai-meta#320 mitigation was applied there was **no runtime evidence
/// anywhere** that the serial drain was actually in effect. The value could only
/// be inferred from the Deployment spec, which is a different representation and
/// one that can disagree with the running process.
///
/// A rollback knob whose engagement cannot be observed is the same defect class
/// as the incident it mitigates.
pub fn ehdb_eventlog_mirror_drain_concurrency() -> &'static IntGauge {
    static M: OnceLock<IntGauge> = OnceLock::new();
    M.get_or_init(|| {
        let g = IntGauge::new(
            "noetl_ehdb_eventlog_mirror_drain_concurrency",
            "Executions one async event-log mirror drain pass delivers concurrently; 1 is the \
             serial drain (noetl/ai-meta#320).",
        )
        .expect("static gauge spec must be valid");
        registry()
            .register(Box::new(g.clone()))
            .expect("gauge registration must succeed");
        g
    })
}

/// Pin the async-mirror label set and gauges at 0.
///
/// Unconditional, and not inside the flag check: a pin behind the flag would be
/// absent on exactly the configuration whose `queue_full_inline` count someone
/// would be reading during an incident.
pub fn init_ehdb_eventlog_mirror_queue_series() {
    for outcome in EHDB_EVENTLOG_MIRROR_QUEUE_OUTCOMES {
        ehdb_eventlog_mirror_queue_total()
            .with_label_values(&[outcome])
            .inc_by(0);
    }
    ehdb_eventlog_mirror_pending_events().set(0);
    ehdb_eventlog_mirror_queue_depth().set(0);
    ehdb_eventlog_mirror_async_enabled().set(0);
    // Pinned to the CONFIGURED value, not 0: unlike the others this is not a
    // count that starts empty — 0 is not a legal concurrency, so pinning it at 0
    // would publish a value the drain can never use and make an un-armed process
    // look misconfigured rather than idle.
    ehdb_eventlog_mirror_drain_concurrency()
        .set(crate::handlers::ehdb_eventlog_mirror_queue::configured_drain_concurrency() as i64);
    // An unlabelled Histogram is still a family with one child, so it is not
    // pruned — but observing nothing leaves every bucket at 0, which is what we
    // want it to read as.
    let _ = ehdb_eventlog_mirror_lag_seconds();
}

/// Counter: mirror-expected authoritative events the comparator held back
/// because they were inside the lag tolerance window (noetl/ai-meta#155).
///
/// The window's cost, published. `divergence_total == 0` under a tolerance
/// window means "the comparable events agreed" — this number says how much was
/// not comparable, and it is the difference between a clean verdict and a
/// comparator that has been taught to look away.
pub fn ehdb_crossstore_pending_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_ehdb_crossstore_pending_total",
                "Mirror-expected authoritative events held back by the cross-store parity lag \
                 tolerance window (noetl/ai-meta#155).",
            ),
            &["tier"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Gauge: the comparator's configured lag tolerance window, in seconds.
///
/// **Published so the alert does not have to hardcode it.** The rule that
/// matters — "is the mirror's real lag inside the window the comparator was
/// told to allow" — compares two numbers, and one of them lives in a
/// deployment env var. A threshold copied into a PromQL expression is a
/// representation of that env var (`agents/rules/representation-drift.md`),
/// true only until someone tunes one side; publishing it makes the comparison
/// read the value itself.
///
/// 0 means no tolerance, which is also the signal that the async mirror is not
/// meant to be on — the alert uses that to stay silent rather than to divide by
/// an implicit assumption.
pub fn ehdb_crossstore_parity_lag_tolerance_seconds() -> &'static IntGauge {
    static M: OnceLock<IntGauge> = OnceLock::new();
    M.get_or_init(|| {
        let g = IntGauge::new(
            "noetl_ehdb_crossstore_parity_lag_tolerance_seconds",
            "The cross-store parity comparator's configured lag tolerance window in seconds; 0 \
             means no tolerance (noetl/ai-meta#155).",
        )
        .expect("static gauge spec must be valid");
        registry()
            .register(Box::new(g.clone()))
            .expect("gauge registration must succeed");
        g
    })
}

pub fn set_ehdb_crossstore_parity_lag_tolerance(seconds: u64) {
    ehdb_crossstore_parity_lag_tolerance_seconds().set(seconds as i64);
}

pub fn add_ehdb_crossstore_pending(tier: &str, n: u64) {
    ehdb_crossstore_pending_total()
        .with_label_values(&[tier])
        .inc_by(n);
}

/// Counter: authoritative projection snapshots the server mirrored into the
/// EHDB projection tier (noetl/ai-meta#265).
///
/// A separate family from the event log's, not a `tier` label on it: the two
/// mirrors arm from different variables and cut over independently, and one
/// family would make a dashboard panel for tier 1 silently start including
/// tier 2.
pub fn ehdb_projection_mirror_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_ehdb_projection_mirror_total",
                "Authoritative orchestrator snapshots the SERVER mirrored into the EHDB \
                 projection tier, by outcome (noetl/ai-meta#265).",
            ),
            &["outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Every outcome label the projection mirror can emit. Closed set, pinned at 0.
pub const EHDB_PROJECTION_MIRROR_OUTCOMES: [&str; 4] =
    ["mirrored", "unconfigured", "unavailable", "degraded"];

/// Outcomes of serving a Secret-Manager-sourced credential (noetl/ai-meta#267):
/// `cache_hit`, `fetched`, `fetch_error`, `not_an_object`.
///
/// All four are emitted as series on first use of the path. `Registry::gather`
/// prunes empty families, so an absent series is indistinguishable from a binary
/// predating the metric — which matters most for `fetch_error`, the one an
/// operator would go looking for.
pub fn sm_sourced_credential_total() -> &'static IntCounterVec {
    static M: std::sync::OnceLock<IntCounterVec> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        let m = IntCounterVec::new(
            prometheus::Opts::new(
                "noetl_sm_sourced_credential_total",
                "Serving a credential whose value is sourced from a secret manager, by outcome (noetl/ai-meta#267).",
            ),
            &["outcome"],
        )
        .expect("sm_sourced_credential_total metric");
        for o in ["cache_hit", "fetched", "fetch_error", "not_an_object"] {
            m.with_label_values(&[o]).reset();
        }
        registry()
            .register(Box::new(m.clone()))
            .expect("register sm_sourced_credential_total");
        m
    })
}

/// Record one SM-sourced credential serve outcome.
pub fn record_sm_sourced_credential(outcome: &str) {
    sm_sourced_credential_total()
        .with_label_values(&[outcome])
        .inc();
}

pub fn record_ehdb_projection_mirror(outcome: &str) {
    ehdb_projection_mirror_total()
        .with_label_values(&[outcome])
        .inc();
}

/// Counter: the projection comparator's own controls — its anti-vacuity signal.
///
/// A separate family from the event-log comparator's `control` counter because
/// the two suites have different control names, and sharing would produce a
/// family whose label set is the union of two closed sets — i.e. not closed, and
/// half of it permanently 0 on each tier.
pub fn ehdb_projection_control_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_ehdb_projection_control_total",
                "Projection cross-store comparator self-controls: synthetic inputs with known \
                 answers, by control and whether the comparator behaved (noetl/ai-meta#265).",
            ),
            &["control", "result"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

pub fn record_ehdb_projection_control(control: &str, result: &str) {
    ehdb_projection_control_total()
        .with_label_values(&[control, result])
        .inc();
}

/// Materialise every projection-tier series at 0.
///
/// Unconditional, outside any config branch — a pin inside `if enabled` would
/// leave exactly the disabled server, the one an operator checks before turning
/// the mirror on, showing nothing.
///
/// The cross-store families are shared with the event log and already carry a
/// `tier` label, so this seeds `tier="projection"` on each of them using the
/// PROJECTION comparator's own enums. Reusing the event log's kind list would
/// pin series this tier can never emit and leave its real ones absent.
pub fn init_ehdb_projection_series() {
    use crate::handlers::ehdb_projection_parity::{
        CONTROL_NAMES, DIVERGENCE_KINDS, PARITY_OUTCOMES, TIER,
    };
    for outcome in EHDB_PROJECTION_MIRROR_OUTCOMES {
        ehdb_projection_mirror_total()
            .with_label_values(&[outcome])
            .inc_by(0);
    }
    for outcome in PARITY_OUTCOMES {
        ehdb_crossstore_parity_total()
            .with_label_values(&[TIER, outcome.as_str()])
            .inc_by(0);
    }
    for kind in DIVERGENCE_KINDS {
        ehdb_crossstore_divergence_total()
            .with_label_values(&[TIER, kind.as_str()])
            .inc_by(0);
    }
    ehdb_crossstore_events_compared_total()
        .with_label_values(&[TIER])
        .inc_by(0);
    for control in CONTROL_NAMES {
        for result in ["expected", "unexpected"] {
            ehdb_projection_control_total()
                .with_label_values(&[control, result])
                .inc_by(0);
        }
    }
    // #265 B1 — the read path and the coverage denominator. Pinned from the
    // modules' OWN enums, not from a copy of them, so a reason added there
    // without a label here fails the guard test rather than going absent on
    // exactly the value someone is checking.
    for outcome in crate::handlers::ehdb_projection_read::READ_OUTCOMES {
        ehdb_projection_read_total()
            .with_label_values(&[outcome])
            .inc_by(0);
    }
    for o in ["materialized", "refused"] {
        ehdb_projection_materialize_total()
            .with_label_values(&[o])
            .inc_by(0);
    }
    for v in crate::handlers::ehdb_projection_fold::REFOLD_VERDICTS {
        ehdb_projection_refold_total()
            .with_label_values(&[v])
            .inc_by(0);
    }
    for r in crate::handlers::ehdb_projection_fold::REFOLD_REFUSALS {
        ehdb_projection_refold_refusal_total()
            .with_label_values(&[r])
            .inc_by(0);
    }
    // #307 — the recovery coverage counter. Every (source, outcome) pair is
    // pinned, because the value being read here is a ZERO and an absent series
    // would be indistinguishable from one. Pinned from the fold module's own
    // arrays so a new refusal reason cannot go missing on exactly the label an
    // operator is checking.
    for source in crate::handlers::ehdb_projection_fold::RECOVERY_FOLD_SOURCES {
        ehdb_recovery_fold_total()
            .with_label_values(&[source, "folded"])
            .inc_by(0);
        for r in crate::handlers::ehdb_projection_fold::REFOLD_REFUSALS {
            ehdb_recovery_fold_total()
                .with_label_values(&[source, r])
                .inc_by(0);
        }
    }
    for o in crate::handlers::catalog_read::OUTCOMES {
        catalog_relation_read_total()
            .with_label_values(&["get_latest", o])
            .inc_by(0);
    }
    // ⚠ Pinned: `relation` reading 0 must be distinguishable from the family
    // being absent, which is the whole evidence question for the cutover.
    for b in crate::handlers::catalog_read::SERVED_BY {
        catalog_read_served_total()
            .with_label_values(&[b])
            .inc_by(0);
    }
    for m in crate::handlers::catalog_log::MODES {
        for o in crate::handlers::catalog_log::OUTCOMES {
            catalog_log_total().with_label_values(&[m, o]).inc_by(0);
        }
    }
    for m in crate::handlers::catalog_snapshot::MODES {
        for o in crate::handlers::catalog_snapshot::OUTCOMES {
            catalog_snapshot_total()
                .with_label_values(&[m, o])
                .inc_by(0);
        }
    }
    for mode in crate::handlers::ehdb_projection_fold::RECOVERY_SOURCES {
        ehdb_recovery_source_info()
            .with_label_values(&[mode])
            .set(i64::from(
                crate::handlers::ehdb_projection_fold::recovery_source().as_str() == mode,
            ));
    }
    for outcome in EHDB_PROJECTION_SNAPSHOT_GATE_OUTCOMES {
        ehdb_projection_snapshot_gate_total()
            .with_label_values(&[outcome])
            .inc_by(0);
    }
    // #265 G3 — the async mirror queue. Pinned UNCONDITIONALLY, outside any
    // config branch: server#315 pinned the event log's publish-skip reasons
    // inside `if event_bus_mode.publishes_ehdb()` and left them absent on
    // exactly the configuration whose reason someone would be reading. The
    // configuration an operator checks before turning this on is the one with
    // it off.
    for outcome in EHDB_PROJECTION_MIRROR_QUEUE_OUTCOMES {
        ehdb_projection_mirror_queue_total()
            .with_label_values(&[outcome])
            .inc_by(0);
    }
    ehdb_projection_mirror_pending().set(0);
    ehdb_projection_mirror_queue_depth().set(0);
    ehdb_projection_mirror_async_enabled().set(0);
    // An unlabelled Histogram is a family with one child, so it is not pruned;
    // observing nothing leaves every bucket at 0, which is what it should read.
    let _ = ehdb_projection_mirror_lag_seconds();
    // The pending counter is the shared cross-store family, so the projection
    // tier's series must be seeded too — otherwise `{tier="projection"}` is
    // absent until the first held-back verdict, and absent reads as "this build
    // has no tolerance window".
    ehdb_crossstore_pending_total()
        .with_label_values(&[TIER])
        .inc_by(0);
}

/// Counter: how each orchestrator snapshot read resolved (noetl/ai-meta#265 B1).
///
/// The read-serve counterpart of `ehdb_projection_mirror_total`. One label,
/// `outcome`, whose closed set is `served_tier` + `disabled` + one value per
/// demote reason.
///
/// **Every demote reason is its own label, deliberately.** A single `demoted`
/// value would make the case that means "the tier is dangerous"
/// (`version_ahead` — it claims to have folded an event that does not exist)
/// indistinguishable from the case that means "nothing has been mirrored for
/// this execution yet" (`missing`), which on a freshly armed mirror is every
/// execution. Those two want opposite operator responses.
pub fn ehdb_projection_read_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_ehdb_projection_read_total",
                "Orchestrator snapshot reads by how they resolved: served from the EHDB \
                 projection tier, or demoted to noetl.projection_snapshot with a reason \
                 (noetl/ai-meta#265).",
            ),
            &["outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

pub fn record_ehdb_projection_read(outcome: &str) {
    ehdb_projection_read_total()
        .with_label_values(&[outcome])
        .inc();
}

/// Counter: what the orchestrator's snapshot-write gate decided
/// (noetl/ai-meta#265 G4) — **the coverage denominator**.
///
/// This exists because of a measured finding: a 13-event execution completes
/// and `noetl.projection_snapshot` gets **no row**. The orchestrator's
/// self-write needs a throttled consistency COUNT to have landed in the same
/// pass (`total == Some(applied_count)`), and on a short execution the two never
/// coincide. So the projection tier can only ever hold what the incumbent
/// wrote, and the incumbent writes sparsely.
///
/// Without a denominator, a shadow soak reporting "0 divergences" is reporting
/// about a population the mirror was never offered — the vacuous pass this whole
/// design was shaped to avoid. This counter is that denominator: `written`
/// against the `skipped_*` reasons is the fraction of eligible passes that ever
/// produced a row to mirror.
///
/// Counted at the **gate**, not at the writer, because the writer by definition
/// only sees the passes that got through.
pub fn ehdb_projection_snapshot_gate_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_ehdb_projection_snapshot_gate_total",
                "Orchestrator snapshot-write decisions by outcome; `written` over the total is \
                 the projection tier's coverage denominator (noetl/ai-meta#265).",
            ),
            &["outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Every snapshot-gate outcome. Closed set, pinned at 0.
///
/// * `written` — the gate passed and a row was upserted.
/// * `skipped_unsettled` — the consistency COUNT had not landed in this pass
///   (`total != applied_count`). **The dominant one on short executions**, and
///   the finding that gates any soak.
/// * `skipped_interval` — settled, but fewer than the snapshot interval's worth
///   of ids since the last one.
/// * `skipped_projector_owns` — the projector owns the snapshot in this
///   deployment, so the orchestrator does not write it.
/// * `skipped_no_state` — nothing folded to write.
pub const EHDB_PROJECTION_SNAPSHOT_GATE_OUTCOMES: [&str; 5] = [
    "written",
    "skipped_unsettled",
    "skipped_interval",
    "skipped_projector_owns",
    "skipped_no_state",
];

pub fn record_ehdb_projection_snapshot_gate(outcome: &str) {
    ehdb_projection_snapshot_gate_total()
        .with_label_values(&[outcome])
        .inc();
}

/// Counter: what happened to a snapshot when it was handed to the projection
/// mirror queue (noetl/ai-meta#265 G3).
///
/// A **second** family rather than more labels on `ehdb_projection_mirror_total`.
/// That one answers "did the tier receive what the incumbent stored", counted at
/// delivery; an `enqueued` label on it would be counted twice for every snapshot
/// — once on enqueue, once on delivery — and destroy the only number that
/// answers the coverage question. This family answers a different one: is the
/// queue absorbing the load, or pushing back?
pub fn ehdb_projection_mirror_queue_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_ehdb_projection_mirror_queue_total",
                "Authoritative snapshots by how the async projection mirror queue handled \
                 them (noetl/ai-meta#265 G3).",
            ),
            &["outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Every queue outcome. Closed set, pinned at 0.
///
/// * `enqueued` — accepted immediately.
/// * `enqueued_after_wait` — the queue was full and the writer **waited** for
///   room. Backpressure working: nothing lost, order preserved. Its rate is the
///   signal that the drain is falling behind.
/// * `queue_full_inline` — full past the enqueue timeout, so the snapshot was
///   mirrored inline. Still no drop, but this is the one path that can deliver
///   out of order relative to snapshots still queued for the same execution, so
///   it is its own label and it is what an alert fires on.
/// * `queue_closed_inline` — no drain task. Mirrored inline; the pre-G3 path.
/// * `drained` — delivered by the drain task. The terminal success label.
/// * `shutdown_abandoned` — still queued when the process finished its flush.
///   These never reached the tier and WILL show as a projection divergence; the
///   label exists so the cause is attributable rather than a mystery an hour later.
pub const EHDB_PROJECTION_MIRROR_QUEUE_OUTCOMES: [&str; 6] = [
    "enqueued",
    "enqueued_after_wait",
    "queue_full_inline",
    "queue_closed_inline",
    "drained",
    "shutdown_abandoned",
];

pub fn record_ehdb_projection_mirror_queue(outcome: &str) {
    ehdb_projection_mirror_queue_total()
        .with_label_values(&[outcome])
        .inc();
}

/// Histogram: seconds from acceptance to durable in the projection tier.
///
/// **The metric the comparator's lag tolerance is checked against.** The window
/// tells the comparator to forgive a tier that is behind by less than N seconds;
/// that is a promise about how long this can take, and without the histogram it
/// would be an assumption. Buckets run to 60 s so a breach of a large window is
/// still visible — a histogram topping out below the tolerance cannot show one.
pub fn ehdb_projection_mirror_lag_seconds() -> &'static Histogram {
    static M: OnceLock<Histogram> = OnceLock::new();
    M.get_or_init(|| {
        let hist = Histogram::with_opts(
            HistogramOpts::new(
                "noetl_ehdb_projection_mirror_lag_seconds",
                "Seconds from a snapshot being accepted for the projection mirror to it \
                 being durable in the tier (noetl/ai-meta#265 G3).",
            )
            .buckets(vec![
                0.005, 0.010, 0.025, 0.050, 0.100, 0.250, 0.500, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
            ]),
        )
        .expect("static histogram spec must be valid");
        registry()
            .register(Box::new(hist.clone()))
            .expect("histogram registration must succeed");
        hist
    })
}

pub fn observe_ehdb_projection_mirror_lag(seconds: f64) {
    ehdb_projection_mirror_lag_seconds().observe(seconds);
}

/// Gauge: snapshots accepted for the mirror and not yet durable.
///
/// The coverage number for the queue. `mirror_lag_seconds` only records
/// snapshots that MADE it, so a queue that has stopped draining publishes no
/// observations at all — its histogram goes quiet, which reads exactly like an
/// idle healthy system. This gauge is what distinguishes the two.
pub fn ehdb_projection_mirror_pending() -> &'static IntGauge {
    static M: OnceLock<IntGauge> = OnceLock::new();
    M.get_or_init(|| {
        let g = IntGauge::new(
            "noetl_ehdb_projection_mirror_pending_snapshots",
            "Snapshots accepted for the async projection mirror and not yet durable in the \
             tier (noetl/ai-meta#265 G3).",
        )
        .expect("static gauge spec must be valid");
        registry()
            .register(Box::new(g.clone()))
            .expect("gauge registration must succeed");
        g
    })
}

/// Gauge: snapshots sitting on the queue.
pub fn ehdb_projection_mirror_queue_depth() -> &'static IntGauge {
    static M: OnceLock<IntGauge> = OnceLock::new();
    M.get_or_init(|| {
        let g = IntGauge::new(
            "noetl_ehdb_projection_mirror_queue_depth",
            "Snapshots currently on the async projection mirror queue (noetl/ai-meta#265 G3).",
        )
        .expect("static gauge spec must be valid");
        registry()
            .register(Box::new(g.clone()))
            .expect("gauge registration must succeed");
        g
    })
}

/// Gauge: 1 when the async projection mirror is ARMED in this process, 0 otherwise.
///
/// Reads 0 in three distinguishable-by-log situations: the flag is off, the flag
/// is on but the drain never started, and the flag is on but the pairing check
/// REFUSED it (async with a zero tolerance window). Without this gauge all three
/// look like an idle queue, and the third — the one that means someone set half
/// a pair — is the one worth finding.
pub fn ehdb_projection_mirror_async_enabled() -> &'static IntGauge {
    static M: OnceLock<IntGauge> = OnceLock::new();
    M.get_or_init(|| {
        let g = IntGauge::new(
            "noetl_ehdb_projection_mirror_async_enabled",
            "1 when the async projection mirror queue is armed in this process, 0 when the \
             mirror is inline or the pairing check refused to arm it (noetl/ai-meta#265 G3).",
        )
        .expect("static gauge spec must be valid");
        registry()
            .register(Box::new(g.clone()))
            .expect("gauge registration must succeed");
        g
    })
}

/// Gauge: the PROJECTION comparator's configured lag tolerance, in seconds.
///
/// Its own gauge rather than a reading of the event log's: the two windows are
/// separate variables that cut over independently, and one gauge would make an
/// alert for tier 1 silently start reading tier 2's tuning. Published so a rule
/// does not have to hardcode the threshold — a number copied into PromQL is a
/// representation of an env var, true until someone tunes one side.
pub fn ehdb_projection_parity_lag_tolerance_seconds() -> &'static IntGauge {
    static M: OnceLock<IntGauge> = OnceLock::new();
    M.get_or_init(|| {
        let g = IntGauge::new(
            "noetl_ehdb_projection_parity_lag_tolerance_seconds",
            "The projection cross-store comparator's configured lag tolerance in seconds; 0 \
             means no tolerance (noetl/ai-meta#265 G3).",
        )
        .expect("static gauge spec must be valid");
        registry()
            .register(Box::new(g.clone()))
            .expect("gauge registration must succeed");
        g
    })
}

pub fn set_ehdb_projection_parity_lag_tolerance(seconds: u64) {
    ehdb_projection_parity_lag_tolerance_seconds().set(seconds as i64);
}

/// Counter: event rows whose `created_at` decoded as neither `timestamptz` nor
/// `timestamp`, so the loader substituted `Utc::now()` (ai-meta#265 Phase 1).
///
/// Pinned at 0 and published because the previous behaviour — an unconditional
/// `unwrap_or_else(Utc::now)` — made an EVERY-ROW decode failure look exactly
/// like normal operation for as long as it existed. A fallback nobody can count
/// is a fallback nobody discovers.
pub fn event_created_at_fallback_total() -> &'static IntCounter {
    static M: OnceLock<IntCounter> = OnceLock::new();
    M.get_or_init(|| {
        let c = IntCounter::new(
            "noetl_event_created_at_fallback_total",
            "Event rows whose created_at decoded as neither timestamptz nor timestamp, so the \
             loader used the current time instead (noetl/ai-meta#265).",
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(c.clone()))
            .expect("counter registration must succeed");
        c
    })
}

pub fn record_event_created_at_fallback() {
    event_created_at_fallback_total().inc();
}

/// Counter: Phase-2 re-fold comparator verdicts (ai-meta#265).
///
/// Ground truth is a fresh fold of the WAL spine, not a Postgres row — so
/// unlike the A4 comparator this counts a comparison that was actually
/// recomputed rather than a stored number compared with itself.
pub fn ehdb_projection_refold_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let c = IntCounterVec::new(
            Opts::new(
                "noetl_ehdb_projection_refold_total",
                "Projection records compared against a fresh fold of the WAL spine, by verdict \
                 (noetl/ai-meta#265).",
            ),
            &["verdict"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(c.clone()))
            .expect("counter registration must succeed");
        c
    })
}

pub fn record_ehdb_projection_refold(verdict: &str) {
    ehdb_projection_refold_total()
        .with_label_values(&[verdict])
        .inc();
}

/// Counter: WAL-spine materialisations into the projection tier
/// (ai-meta#265 Phase 3).
pub fn ehdb_projection_materialize_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let c = IntCounterVec::new(
            Opts::new(
                "noetl_ehdb_projection_materialize_total",
                "Executions materialised into the projection tier from the WAL spine, by outcome \
                 (noetl/ai-meta#265).",
            ),
            &["outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(c.clone()))
            .expect("counter registration must succeed");
        c
    })
}

pub fn record_ehdb_projection_materialize(outcome: &str) {
    ehdb_projection_materialize_total()
        .with_label_values(&[outcome])
        .inc();
}

pub fn init_ehdb_crossstore_series() {
    // Unconditional pin: absent and zero are different answers, and this one's
    // whole value is being readable as 0 on a healthy binary.
    event_created_at_fallback_total().inc_by(0);
    use crate::handlers::ehdb_parity::{CONTROL_NAMES, DIVERGENCE_KINDS, PARITY_OUTCOMES, TIER};
    for outcome in PARITY_OUTCOMES {
        ehdb_crossstore_parity_total()
            .with_label_values(&[TIER, outcome.as_str()])
            .inc_by(0);
    }
    for kind in DIVERGENCE_KINDS {
        ehdb_crossstore_divergence_total()
            .with_label_values(&[TIER, kind.as_str()])
            .inc_by(0);
    }
    ehdb_crossstore_pending_total()
        .with_label_values(&[TIER])
        .inc_by(0);
    ehdb_crossstore_parity_lag_tolerance_seconds().set(0);
    ehdb_crossstore_events_compared_total()
        .with_label_values(&[TIER])
        .inc_by(0);
    for control in CONTROL_NAMES {
        for result in ["expected", "unexpected"] {
            ehdb_crossstore_control_total()
                .with_label_values(&[control, result])
                .inc_by(0);
        }
    }
}

/// Where each bootstrap secret came from (noetl/ai-meta#267 Tier 2) — labelled
/// by `var` and `source` (`file` / `env` / `file_unusable`).  Records the
/// PROVENANCE, never the value.
///
/// Every variable emits on every boot, so all sources exist as series from the
/// first scrape.  `Registry::gather` prunes empty families, so an absent series
/// would be indistinguishable from a binary that predates the metric.
pub fn secret_source_total() -> &'static IntCounterVec {
    static M: std::sync::OnceLock<IntCounterVec> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        let m = IntCounterVec::new(
            prometheus::Opts::new(
                "noetl_secret_source_total",
                "Where a bootstrap secret was read from at startup: file (CSI mount) or env (secretKeyRef). Provenance only, never the value (noetl/ai-meta#267).",
            ),
            &["var", "source"],
        )
        .expect("secret_source_total metric");
        registry()
            .register(Box::new(m.clone()))
            .expect("register secret_source_total");
        m
    })
}

/// Record one bootstrap secret's provenance.  `source` is `file`, `env` or
/// `file_unusable`.
pub fn record_secret_source(var: &str, source: &str) {
    secret_source_total()
        .with_label_values(&[var, source])
        .inc();
}

/// Outcomes of the router-level internal-auth gate (noetl/ai-meta#303), labelled
/// by `group` (`credentials` / `keychain` / `internal`), `outcome`
/// (`valid` / `missing` / `malformed` / `mismatch` / `unconfigured`) and the
/// `mode` the gate was running in.
///
/// The whole point of the shadow mode is to answer "who would break if we
/// enforced this?", so an absent series is the one thing that must not be
/// ambiguous. `Registry::gather` prunes empty families, so every group x outcome
/// pair is pinned at 0 by [`init_internal_auth_series`] at startup: a 0 means
/// "this has not happened", never "this binary predates the metric".
pub fn internal_auth_total() -> &'static IntCounterVec {
    static M: std::sync::OnceLock<IntCounterVec> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        let m = IntCounterVec::new(
            prometheus::Opts::new(
                "noetl_internal_auth_total",
                "Router-level internal API token gate outcomes by route group and mode (noetl/ai-meta#303). In shadow mode nothing is rejected; a non-valid outcome is what WOULD be rejected under enforce.",
            ),
            &["group", "outcome", "mode"],
        )
        .expect("internal_auth_total metric");
        registry()
            .register(Box::new(m.clone()))
            .expect("register internal_auth_total");
        m
    })
}

/// Record one gate decision.
pub fn record_internal_auth(group: &str, outcome: &str, mode: &str) {
    internal_auth_total()
        .with_label_values(&[group, outcome, mode])
        .inc();
}

/// Pin every group x outcome series at 0, for the current mode, unconditionally
/// at startup.
///
/// Pinned for the mode the process is actually running in. Pinning all three
/// modes would be worse than useless: it would show `mode="enforce"` series on a
/// server running in shadow, which reads as "enforcement is on".
pub fn init_internal_auth_series(mode: &str) {
    for g in crate::auth_gate::GROUPS {
        for o in crate::auth_gate::Outcome::ALL {
            internal_auth_total().with_label_values(&[g, o.as_str(), mode]);
        }
    }
}

/// Why a projection re-fold refused, when it refused (noetl/ai-meta#303 follow-up).
///
/// `noetl_ehdb_projection_refold_total{verdict="spine_refused"}` says a
/// comparison did not happen; this says why. Prod reports 4 of 4 refolds refused
/// with no way to distinguish drain lag from corruption, which are opposite
/// operational responses.
pub fn ehdb_projection_refold_refusal_total() -> &'static IntCounterVec {
    static M: std::sync::OnceLock<IntCounterVec> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        let m = IntCounterVec::new(
            prometheus::Opts::new(
                "noetl_ehdb_projection_refold_refusal_total",
                "Reason a projection re-fold refused: no_events / source_unavailable / unparseable / fold_failed / spine_incomplete.",
            ),
            &["reason"],
        )
        .expect("ehdb_projection_refold_refusal_total metric");
        registry()
            .register(Box::new(m.clone()))
            .expect("register ehdb_projection_refold_refusal_total");
        m
    })
}

/// Record one refusal reason.
pub fn record_ehdb_projection_refold_refusal(reason: &str) {
    ehdb_projection_refold_refusal_total()
        .with_label_values(&[reason])
        .inc();
}

/// Which source answered a recovery fold, and how (noetl/ai-meta#307).
///
/// **This is the counter that was structurally 0.** Recovery folded only from
/// the worker's in-memory spine, which holds in-flight executions and evicts
/// them on completion — while the comparator, by its nature, asks about
/// *completed* ones. So coverage was ~0 by construction rather than by failure,
/// and nothing said so except a zero, which reads identically to "healthy and
/// quiet".
///
/// `source="tier"` going non-zero is the whole point: it means a completed
/// execution was refolded from the durable log.
pub fn ehdb_recovery_fold_total() -> &'static IntCounterVec {
    static M: std::sync::OnceLock<IntCounterVec> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        let m = IntCounterVec::new(
            prometheus::Opts::new(
                "noetl_ehdb_recovery_fold_total",
                "Recovery fold attempts by source (spine / tier) and outcome (folded, or the refusal reason).",
            ),
            &["source", "outcome"],
        )
        .expect("ehdb_recovery_fold_total metric");
        registry()
            .register(Box::new(m.clone()))
            .expect("register ehdb_recovery_fold_total");
        m
    })
}

/// Record one recovery-fold attempt.
pub fn record_ehdb_recovery_fold(source: &str, outcome: &str) {
    ehdb_recovery_fold_total()
        .with_label_values(&[source, outcome])
        .inc();
}

/// The configured recovery source, as a gauge (noetl/ai-meta#307).
///
/// Without this, "the tier never folded" and "the tier fallback is switched
/// off" are the same evidence — both are an absent `source="tier"` series. That
/// ambiguity is precisely what made the original zero unreadable, so the mode
/// is published rather than inferred from the counters it governs.
pub fn ehdb_recovery_source_info() -> &'static IntGaugeVec {
    static M: std::sync::OnceLock<IntGaugeVec> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        let m = IntGaugeVec::new(
            prometheus::Opts::new(
                "noetl_ehdb_recovery_source_info",
                "Configured recovery fold source; 1 on the active mode, 0 on the others.",
            ),
            &["mode"],
        )
        .expect("ehdb_recovery_source_info metric");
        registry()
            .register(Box::new(m.clone()))
            .expect("register ehdb_recovery_source_info");
        m
    })
}

/// Catalog relation reads compared against the incumbent (RFC step 3 §5.1).
pub fn catalog_read_served_total() -> &'static IntCounterVec {
    static M: std::sync::OnceLock<IntCounterVec> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        let m = IntCounterVec::new(
            prometheus::Opts::new(
                "noetl_catalog_read_served_total",
                "Catalog path resolutions by which source answered (incumbent Postgres vs the folded relation).",
            ),
            &["served_by"],
        )
        .expect("catalog_read_served_total");
        registry().register(Box::new(m.clone())).ok();
        m
    })
}

pub fn record_catalog_read_served(by: &str) {
    catalog_read_served_total().with_label_values(&[by]).inc();
}

pub fn catalog_relation_read_total() -> &'static IntCounterVec {
    static M: std::sync::OnceLock<IntCounterVec> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        let m = IntCounterVec::new(
            prometheus::Opts::new(
                "noetl_catalog_relation_read_total",
                "Catalog relation reads by operation and outcome (agree/disagree/fold_missing/...).",
            ),
            &["operation", "outcome"],
        )
        .expect("catalog_relation_read_total metric");
        registry()
            .register(Box::new(m.clone()))
            .expect("register catalog_relation_read_total");
        m
    })
}

/// Record one relation-vs-incumbent comparison.
pub fn record_catalog_relation_read(operation: &str, outcome: &str) {
    catalog_relation_read_total()
        .with_label_values(&[operation, outcome])
        .inc();
}

/// Catalog-log records, by mode and outcome (noetl/ai-meta#311 step 2).
pub fn catalog_log_total() -> &'static IntCounterVec {
    static M: std::sync::OnceLock<IntCounterVec> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        let m = IntCounterVec::new(
            prometheus::Opts::new(
                "noetl_catalog_log_total",
                "Catalog registrations recorded to the catalog tier, by mode and outcome.",
            ),
            &["mode", "outcome"],
        )
        .expect("catalog_log_total metric");
        registry()
            .register(Box::new(m.clone()))
            .expect("register catalog_log_total");
        m
    })
}

/// Record one catalog-log attempt.
pub fn record_catalog_log(mode: &str, outcome: &str) {
    catalog_log_total()
        .with_label_values(&[mode, outcome])
        .inc();
}

/// Execution-start catalog snapshots, by mode and outcome.
///
/// `mode="off",outcome="disabled"` is the normal reading until the feature is
/// switched on; every pair is pinned at 0 so "not recording" and "binary predates
/// the metric" are distinguishable.
pub fn catalog_snapshot_total() -> &'static IntCounterVec {
    static M: std::sync::OnceLock<IntCounterVec> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        let m = IntCounterVec::new(
            prometheus::Opts::new(
                "noetl_catalog_snapshot_total",
                "Execution-start catalog snapshots by mode (off/digest/full) and outcome.",
            ),
            &["mode", "outcome"],
        )
        .expect("catalog_snapshot_total metric");
        registry()
            .register(Box::new(m.clone()))
            .expect("register catalog_snapshot_total");
        m
    })
}

/// Record one snapshot attempt.
pub fn record_catalog_snapshot(mode: &str, outcome: &str) {
    catalog_snapshot_total()
        .with_label_values(&[mode, outcome])
        .inc();
}

/// Whether `GET /api/executions` served a capped page (noetl/ai-meta#255).
///
/// Both label values are pinned at 0 at startup: the interesting question is
/// "is anything hitting the cap?", and an absent `capped="true"` series would be
/// indistinguishable from a binary that predates the metric.
pub fn executions_limit_total() -> &'static IntCounterVec {
    static M: std::sync::OnceLock<IntCounterVec> = std::sync::OnceLock::new();
    M.get_or_init(|| {
        let m = IntCounterVec::new(
            prometheus::Opts::new(
                "noetl_executions_limit_total",
                "GET /api/executions requests by whether the page size was capped at MAX_LIMIT (noetl/ai-meta#255).",
            ),
            &["capped"],
        )
        .expect("executions_limit_total metric");
        registry()
            .register(Box::new(m.clone()))
            .expect("register executions_limit_total");
        m
    })
}

/// Record one list request.
pub fn record_executions_limit(capped: bool) {
    executions_limit_total()
        .with_label_values(&[if capped { "true" } else { "false" }])
        .inc();
}

/// Pin both outcomes, unconditionally.
pub fn init_executions_limit_series() {
    for v in ["true", "false"] {
        executions_limit_total().with_label_values(&[v]).inc_by(0);
    }
}

#[cfg(test)]
mod tests {

    /// Serialises the tests that mutate PROCESS-GLOBAL gauges.
    ///
    /// `cargo test` runs tests on a thread pool and does **not** serialise them,
    /// so two tests that both `set_ehdb_event_publisher_configured` race: one
    /// sets it false while the other is asserting it reads 1. That produced a
    /// flake on roughly one run in three — green often enough to look like noise
    /// and to survive review, which is the worst frequency for a failing test.
    ///
    /// The lock is poison-tolerant: a panic in one test must fail that test, not
    /// cascade into every other test that takes this lock.
    static GLOBAL_GAUGE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_global_gauges() -> std::sync::MutexGuard<'static, ()> {
        GLOBAL_GAUGE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    use super::*;
    // The registry is process-global, so all tests share state.
    // We assert on the rendered text after at least one observation
    // — the test order is `serial_test`-coordinated by the global
    // registry's internal locks (counters are thread-safe).

    #[test]
    fn registry_initializes_once() {
        let a = registry() as *const Registry;
        let b = registry() as *const Registry;
        assert_eq!(a, b, "registry() must return the same instance");
    }

    #[test]
    fn counter_increments_by_label_set() {
        events_ingested_total()
            .with_label_values(&["test.counter_increments", "ok"])
            .inc();
        events_ingested_total()
            .with_label_values(&["test.counter_increments", "ok"])
            .inc();
        let value = events_ingested_total()
            .with_label_values(&["test.counter_increments", "ok"])
            .get();
        assert!(value >= 2, "expected at least 2 increments, got {value}");
    }

    #[test]
    fn histogram_observes_duration() {
        event_ingest_duration_seconds()
            .with_label_values(&["test.histogram_observes"])
            .observe(0.123);
        // We can't read the histogram value directly via the public
        // API, but we can confirm the gathered output mentions it.
        let text = gather_text().expect("gather_text must succeed");
        assert!(
            text.contains("test.histogram_observes"),
            "expected histogram label in text:\n{text}"
        );
    }

    #[test]
    fn gather_text_contains_metric_names() {
        record_event_ingest("test.gather_text", "ok", 0.05);
        let text = gather_text().expect("gather_text must succeed");
        assert!(
            text.contains("noetl_events_ingested_total"),
            "expected counter name in text:\n{text}"
        );
        assert!(
            text.contains("noetl_event_ingest_duration_seconds"),
            "expected histogram name in text:\n{text}"
        );
    }

    #[test]
    fn record_event_ingest_handles_both_statuses() {
        record_event_ingest("test.both_statuses", "ok", 0.01);
        record_event_ingest("test.both_statuses", "error", 0.02);
        let text = gather_text().expect("gather_text must succeed");
        assert!(text.contains("test.both_statuses"));
        // Both label sets should be present.
        assert!(
            text.contains("status=\"ok\""),
            "expected status=ok label in text:\n{text}"
        );
        assert!(
            text.contains("status=\"error\""),
            "expected status=error label in text:\n{text}"
        );
    }

    // --- Round 2: generic write-request metrics ---

    #[test]
    fn write_request_counter_increments_by_label_set() {
        record_write_request("test.write.counter", "ok", 0.01);
        record_write_request("test.write.counter", "ok", 0.02);
        let value = write_requests_total()
            .with_label_values(&["test.write.counter", "ok"])
            .get();
        assert!(value >= 2, "expected at least 2 increments, got {value}");
    }

    #[test]
    fn write_request_metric_names_appear_in_text() {
        record_write_request("test.write.text", "ok", 0.05);
        let text = gather_text().expect("gather_text must succeed");
        assert!(
            text.contains("noetl_write_requests_total"),
            "expected counter name in text:\n{text}"
        );
        assert!(
            text.contains("noetl_write_request_duration_seconds"),
            "expected histogram name in text:\n{text}"
        );
        assert!(text.contains("endpoint=\"test.write.text\""));
    }

    #[test]
    fn endpoint_constants_are_used_consistently() {
        // Compile-time check: the constants exist and resolve.
        let names = [
            endpoint::CATALOG_REGISTER,
            endpoint::CREDENTIALS_UPSERT,
            endpoint::KEYCHAIN_SET,
            endpoint::RUNTIME_REGISTER,
            endpoint::RUNTIME_HEARTBEAT,
        ];
        // Sanity: they're all distinct and non-empty.
        assert_eq!(
            names.iter().collect::<std::collections::HashSet<_>>().len(),
            names.len()
        );
        assert!(names.iter().all(|n| !n.is_empty()));
    }

    /// noetl/ai-meta#208 — a publish that gives up must be COUNTABLE, not only
    /// loggable.  `noetl_command_publish_total` counts successes, so without
    /// this counter there is no failure rate and a total give-up is invisible
    /// outside the log.
    #[test]
    fn ehdb_publish_failures_are_counted_by_reason() {
        record_ehdb_command_publish_failed("gave_up");
        record_ehdb_command_publish_failed("attempt");
        record_ehdb_command_publish_failed("attempt");

        let text = gather_text().expect("gather metrics text");
        assert!(
            text.contains("noetl_ehdb_command_publish_failed_total"),
            "the counter must appear in /metrics output"
        );
        // Scoped to this metric's own lines so the assertion cannot pass on
        // some other metric that happens to carry the same label.
        let lines: Vec<&str> = text
            .lines()
            .filter(|l| l.starts_with("noetl_ehdb_command_publish_failed_total{"))
            .collect();
        assert!(
            lines.iter().any(|l| l.contains("reason=\"gave_up\"")),
            "gave_up series missing; got {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("reason=\"attempt\"")),
            "attempt series missing; got {lines:?}"
        );
    }

    /// noetl/ai-meta#238 — "the durable log is being dropped on the floor" must
    /// be a metric, not only a startup log line.  A gauge because the operator
    /// question is "is it configured now", and because a boot-time error scrolls
    /// away while a gauge survives.
    #[test]
    fn the_event_publisher_configured_gauge_tracks_both_states() {
        let _guard = lock_global_gauges();
        set_ehdb_event_publisher_configured(false);
        let text = gather_text().expect("gather metrics text");
        let line = text
            .lines()
            .find(|l| l.starts_with("noetl_ehdb_event_publisher_configured "))
            .expect("gauge must appear in /metrics");
        assert!(
            line.ends_with(" 0"),
            "unconfigured must read 0; got {line:?}"
        );

        set_ehdb_event_publisher_configured(true);
        let text = gather_text().expect("gather metrics text");
        let line = text
            .lines()
            .find(|l| l.starts_with("noetl_ehdb_event_publisher_configured "))
            .expect("gauge must appear in /metrics");
        assert!(line.ends_with(" 1"), "configured must read 1; got {line:?}");
    }

    /// A labelled counter is ABSENT from /metrics until first incremented, so an
    /// unfired failure counter cannot be told apart from a removed metric or an
    /// older binary.  Verified on kind: the gauge appeared, this counter did not.
    #[test]
    fn publish_failure_series_exist_at_zero_before_any_failure() {
        init_ehdb_command_publish_failed_series();
        let text = gather_text().expect("gather metrics text");
        let lines: Vec<&str> = text
            .lines()
            .filter(|l| l.starts_with("noetl_ehdb_command_publish_failed_total{"))
            .collect();
        for reason in ["gave_up", "attempt"] {
            assert!(
                lines
                    .iter()
                    .any(|l| l.contains(&format!("reason=\"{reason}\""))),
                "{reason} series must exist before any failure; got {lines:?}"
            );
        }
    }

    /// The publish-skip reasons carry the same absent-until-fired problem, and
    /// on this path the zero is the *normal* reading: a server carrying only
    /// system-pool traffic never publishes, and must be distinguishable from
    /// one whose transport is missing.
    #[test]
    fn publish_skip_series_exist_at_zero_before_any_skip() {
        init_event_ingest_publish_skipped_series();
        let text = gather_text().expect("gather metrics text");
        let lines: Vec<&str> = text
            .lines()
            .filter(|l| l.starts_with("noetl_event_ingest_publish_skipped_total{"))
            .collect();
        for reason in ["gate_off", "no_transport", "system_execution"] {
            assert!(
                lines
                    .iter()
                    .any(|l| l.contains(&format!("reason=\"{reason}\""))),
                "{reason} series must exist before any skip; got {lines:?}"
            );
        }
    }

    /// Calling ONLY the startup inits must register every unlabelled metric.
    ///
    /// This is the test that was missing.  The previous one called
    /// `permanent_log_lean_stage_failed_total()` directly to "touch its
    /// registration" — which registered it, so the assertion passed while the
    /// released binary served a `/metrics` without it.  Every metric here is
    /// behind a `OnceLock` and registers when its ACCESSOR runs; nothing in the
    /// binary called that one.  Caught by scraping a released image on kind, not
    /// by any test.
    ///
    /// So this test deliberately touches no accessor: it runs what `main` runs
    /// and then asks what a scrape would show.
    #[test]
    fn startup_inits_alone_register_every_unlabelled_metric() {
        let _guard = lock_global_gauges();
        init_unlabelled_series();
        set_sharding_config_parse_failed(false);
        set_ehdb_event_publisher_configured(false);

        let text = gather_text().expect("gather metrics text");
        let mut want: Vec<&str> = UNLABELLED_STARTUP_METRICS.to_vec();
        // These two register as a side effect of being SET at startup.
        want.push("noetl_sharding_config_parse_failed");
        want.push("noetl_ehdb_event_publisher_configured");
        for name in want {
            assert!(
                text.lines().any(|l| l.starts_with(&format!("{name} "))),
                "{name} must be registered by startup alone — a metric only \
                 registers when its accessor runs, and the binary must be the \
                 thing that runs it"
            );
        }

        // main must call it; wiring the init and forgetting the call is the
        // same bug one level up.
        assert!(
            include_str!("main.rs").contains("init_unlabelled_series()"),
            "main must invoke init_unlabelled_series"
        );
        // The list and the function must not drift apart: every name in the
        // const has to actually be registered by the call above, and the count
        // has to match the number of accessor touches in the function body.
        let me = include_str!("metrics.rs");
        let body_start = me
            .find("pub fn init_unlabelled_series() {")
            .expect("init must exist");
        let body = &me[body_start..body_start + me[body_start..].find("\n}").unwrap()];
        assert_eq!(
            body.matches("let _ = ").count(),
            UNLABELLED_STARTUP_METRICS.len(),
            "every metric listed in UNLABELLED_STARTUP_METRICS must be touched by the init"
        );
    }

    /// The remaining P2 server signals must all be readable with no activity.
    ///
    /// Two are unlabelled on purpose.  `sharding_config_parse_failed` is set on
    /// BOTH startup arms so it reads 0 on a healthy server rather than being
    /// absent — a silent single-pool fallback is precisely the case where an
    /// absent gauge and a healthy one must not look alike.
    #[test]
    fn p2_server_signals_are_present_at_zero() {
        init_command_row_insert_series();
        set_sharding_config_parse_failed(false);
        let text = gather_text().expect("gather metrics text");

        for mode in COMMAND_ROW_INSERT_MODES {
            assert!(
                text.lines()
                    .any(|l| l.starts_with("noetl_command_row_insert_failed_total{")
                        && l.contains(&format!("mode=\"{mode}\""))),
                "{mode} must be pinned at 0"
            );
        }
        let g = text
            .lines()
            .find(|l| l.starts_with("noetl_sharding_config_parse_failed "))
            .expect("the sharding gauge must be present even when parsing succeeded");
        assert!(g.ends_with(" 0"), "healthy startup must read 0; got {g:?}");

        // Both startup arms must set the gauge, or the healthy path leaves it
        // absent and the fallback stays as invisible as it was.
        let main_rs = include_str!("main.rs");
        assert_eq!(
            main_rs.matches("set_sharding_config_parse_failed(").count(),
            2,
            "both the ok and err arms must set the gauge"
        );
    }

    /// Every way a command fails to reach the EHDB bus must be counted, and all
    /// four reasons readable at 0.
    ///
    /// `no_writers` is the severe one: with `NOETL_COMMAND_BUS=ehdb` and no
    /// writer routes resolved, every command is silently not delivered and
    /// every execution stalls.  Before this it was a `tracing::warn!` and
    /// nothing else, on a path where nothing else errors.
    #[test]
    fn ehdb_publish_failure_reasons_cover_every_undelivered_path() {
        init_ehdb_command_publish_failed_series();
        let text = gather_text().expect("gather metrics text");
        for reason in EHDB_COMMAND_PUBLISH_FAILED_REASONS {
            assert!(
                text.lines().any(
                    |l| l.starts_with("noetl_ehdb_command_publish_failed_total{")
                        && l.contains(&format!("reason=\"{reason}\""))
                ),
                "{reason} must be pinned at 0"
            );
        }
        // Both dispatch-side sites in execute.rs must record.
        let src = include_str!("handlers/execute.rs");
        for reason in ["no_writers", "shadow_failed"] {
            assert!(
                src.contains(&format!("record_ehdb_command_publish_failed(\"{reason}\")")),
                "{reason} must be recorded at its dispatch site"
            );
        }
        // The stale claim must not come back: NATS stopped being authoritative
        // at T5, so a message saying it is would mislead exactly when someone
        // is debugging an undelivered command.
        assert!(
            !src.contains("EHDB shadow command publish failed (NATS authoritative)"),
            "the pre-T5 'NATS authoritative' wording must stay corrected"
        );
    }

    /// Both P1 server sets must be readable at 0, and every plug-in outcome
    /// must be instrumented.
    ///
    /// The plug-in one matters because the startup log reports only the count
    /// that succeeded: seeding 3 of 4 and 3 of 3 look identical there.  The
    /// secret-refresh one is checked for COMPLETENESS here rather than by the
    /// scan in `pinned_sets.py`, because two of its five outcomes are passed as
    /// a variable and a literal scan under-reports it by exactly those two.
    #[test]
    fn plugin_seed_and_secret_refresh_series_exist() {
        init_system_plugin_seed_series();
        init_secret_refresh_series();
        let text = gather_text().expect("gather metrics text");
        for outcome in SYSTEM_PLUGIN_SEED_OUTCOMES {
            assert!(
                text.lines()
                    .any(|l| l.starts_with("noetl_system_plugin_seed_total{")
                        && l.contains(&format!("outcome=\"{outcome}\""))),
                "{outcome} must be pinned"
            );
        }
        for outcome in SECRET_REFRESH_OUTCOMES {
            assert!(
                text.lines()
                    .any(|l| l.starts_with("noetl_secret_refresh_total{")
                        && l.contains(&format!("outcome=\"{outcome}\""))),
                "{outcome} must be pinned"
            );
        }

        // Every skip path in the seeder must record, or a missing plug-in stays
        // as silent as it was before.
        let src = include_str!("system_plugins.rs");
        assert_eq!(
            src.matches("record_system_plugin_seed(").count(),
            3,
            "all three seed outcomes must be instrumented"
        );
        assert_eq!(
            src.matches("tracing::warn!").count(),
            src.matches("record_system_plugin_seed(\"skipped").count(),
            "every warn! in the seeder must have a matching skipped_* counter"
        );
    }

    /// Same guard as the GC one, on the security-relevant path.  Reuses the
    /// after-the-call extraction rather than same-line matching, because that
    /// is the variant that has already been caught missing a literal.
    #[test]
    fn credential_seal_literals_are_all_pinned() {
        let src = include_str!("handlers/credentials.rs");
        let call = "record_credential_seal(";
        let mut found: Vec<&str> = Vec::new();
        let mut rest = src;
        while let Some(i) = rest.find(call) {
            rest = &rest[i + call.len()..];
            if let Some(q1) = rest.find('"') {
                let after = &rest[q1 + 1..];
                if let Some(q2) = after.find('"') {
                    found.push(&after[..q2]);
                }
            }
        }
        assert!(
            found.len() >= 8,
            "extraction found only {} call site(s) — must fail loudly, not vacuously; got {found:?}",
            found.len()
        );
        for lit in &found {
            assert!(
                CREDENTIAL_SEAL_STATUSES.contains(lit),
                "record_credential_seal(\"{lit}\") is recorded but not pinned"
            );
        }
        // The security-relevant denial must never quietly drop out of the set.
        assert!(
            CREDENTIAL_SEAL_STATUSES.contains(&"residency_violation"),
            "residency_violation is a policy denial and must stay pinned"
        );
    }

    /// Every outcome literal at a call site must be pinned.
    ///
    /// The source is embedded with `include_str!`, so this reads the real calls
    /// rather than a doc comment.  The extraction deliberately looks for the
    /// next quoted string AFTER the call rather than on the same line: one of
    /// these seven, `skipped_unparseable`, is wrapped onto the following line,
    /// and a same-line pattern silently drops it — which is how a pinned set
    /// ends up one short while looking complete.
    #[test]
    fn result_tier_gc_literals_are_all_pinned() {
        let src = include_str!("handlers/result_tier.rs");
        let call = "record_result_tier_gc(";
        let mut found: Vec<&str> = Vec::new();
        let mut rest = src;
        while let Some(i) = rest.find(call) {
            rest = &rest[i + call.len()..];
            if let Some(q1) = rest.find('"') {
                let after = &rest[q1 + 1..];
                if let Some(q2) = after.find('"') {
                    found.push(&after[..q2]);
                }
            }
        }
        assert!(
            found.len() >= 8,
            "extraction found only {} literal(s) — a broken parser must fail here, \
             not pass vacuously; got {found:?}",
            found.len()
        );
        for lit in &found {
            assert!(
                RESULT_TIER_GC_OUTCOMES.contains(lit),
                "record_result_tier_gc(\"{lit}\") is recorded but not pinned"
            );
        }
        assert!(
            found.contains(&"skipped_unparseable"),
            "the wrapped literal must be found — if it is not, the extraction \
             regressed to same-line matching; got {found:?}"
        );
    }

    /// A correctness signal that reads as absent when nothing is wrong cannot
    /// be told apart from one that is not running.  Both must read 0.
    #[test]
    fn parity_and_dedup_series_exist_at_zero() {
        init_parity_and_dedup_series();
        let text = gather_text().expect("gather metrics text");
        for want in [
            "noetl_state_build_parity_total{result=\"match\"}",
            "noetl_state_build_parity_total{result=\"mismatch\"}",
            "noetl_terminal_dedup_total{outcome=\"suppressed\"}",
        ] {
            let line = text
                .lines()
                .find(|l| l.starts_with(want))
                .unwrap_or_else(|| panic!("{want} must be pinned"));
            assert!(
                line.trim_end().ends_with(" 0"),
                "{want} must read 0 before anything happens; got {line:?}"
            );
        }
    }

    /// The orphan guardrail also emits `playbook.failed`, and noetl/ai-meta#227
    /// describes it looping against stalled executions while these counters were
    /// absent — so the loop was found by watching a shard cursor climb.
    #[test]
    fn orphan_sweep_outcome_series_exist_at_zero() {
        init_orphan_sweep_series();
        let text = gather_text().expect("gather metrics text");
        let lines: Vec<&str> = text
            .lines()
            .filter(|l| l.starts_with("noetl_orphan_sweep_total{"))
            .collect();
        for outcome in ORPHAN_SWEEP_OUTCOMES {
            assert!(
                lines
                    .iter()
                    .any(|l| l.contains(&format!("outcome=\"{outcome}\""))),
                "{outcome} series must exist before any sweep; got {lines:?}"
            );
        }
    }

    /// noetl/ai-meta#237 — a foreign-key violation SOFT-DELETES; it does not error.
    ///
    /// `noetl.event` holds an FK onto `noetl.catalog` and the event log is
    /// append-only, so an entry with execution history can never be hard
    /// deleted. It used to surface as a bare 500, then briefly as a 409.
    /// Neither did what the caller asked.
    ///
    /// Now the FK violation is the *signal* to archive instead: the entry is
    /// marked `archived_at`, stops resolving by path, drops out of `list`, and
    /// is restorable with one call. The caller's intent — make this go away —
    /// is satisfied, reversibly, without destroying anything.
    ///
    /// Asserts the mapping at the call site; the sqlx error type cannot be
    /// constructed in a unit test without a live database.
    #[test]
    fn catalog_delete_falls_back_to_archive_on_fk_violation() {
        let full = include_str!("services/catalog.rs");
        let src = full.split_once("\n#[cfg(test)]").map_or(full, |(b, _)| b);
        assert!(
            src.contains("Some(\"23503\")"),
            "the FK violation SQLSTATE must still be matched explicitly"
        );
        assert!(
            src.contains("archive_catalog_entries"),
            "an FK violation must fall back to archiving, not error out"
        );
        // What the FK branch must DO, stated positively. An earlier version of
        // this guard asserted "no AppError::Conflict in the branch", which was a
        // proxy for the same idea — and it became wrong the moment the archive
        // path grew a legitimate nested Conflict for the missing-column case.
        // Assert the behaviour, not the absence of a symbol.
        let fk = src.find("Some(\"23503\")").expect("the FK branch exists");
        let fk_branch = &src[fk..];
        let archive_at = fk_branch
            .find("archive_catalog_entries")
            .expect("the FK branch must archive");
        let ok_at = fk_branch
            .find("return Ok(CatalogDeleteResponse")
            .expect("the FK branch must return success once archived");
        assert!(
            archive_at < ok_at,
            "the FK branch must archive BEFORE reporting success"
        );

        // Retirement must be undoable, or soft delete is just a slower hard delete.
        assert!(
            src.contains("restore_catalog_entries"),
            "restore must exist for archive to be reversible"
        );
    }

    /// noetl/ai-meta#237 — startup DDL that can fail on a PERMISSIONS boundary
    /// must not be fatal.
    ///
    /// `noetl.catalog` is not owned by the role the server connects as (kind:
    /// owner `demo`, connection `noetl`), and Postgres requires ownership to
    /// ALTER. The first cut used `?`, so the server exited on boot and
    /// crash-looped — caught in kind against the released image, before it
    /// reached production, where it would have taken the control plane down.
    ///
    /// The column is OPTIONAL: it only enables soft delete, and nothing else
    /// depends on it. A server that cannot add it must still start.
    #[test]
    fn optional_startup_ddl_is_not_fatal() {
        let m = include_str!("main.rs");
        let src = m.split_once("\n#[cfg(test)]").map_or(m, |(b, _)| b);
        let i = src
            .find("ensure_archived_column")
            .expect("the archived_at DDL runs at startup");
        // Look at the statement around the call, not the whole file.
        let start = src[..i].rfind('\n').unwrap_or(0);
        let end = src[i..].find(";\n").map(|j| i + j).unwrap_or(src.len());
        let stmt = &src[start..end];
        assert!(
            !stmt.contains(".await?"),
            "the archived_at DDL must not use `?` — a permissions failure would \
             crash-loop the server on boot; statement was:\n{stmt}"
        );
        assert!(
            src[start..].starts_with("\n    if let Err(") || stmt.contains("if let Err("),
            "the DDL failure must be handled and logged, not propagated"
        );
    }

    /// A missing `archived_at` must produce an ACTIONABLE error, not a 500.
    #[test]
    fn missing_archived_column_reports_the_operator_action() {
        let full = include_str!("services/catalog.rs");
        let src = full.split_once("\n#[cfg(test)]").map_or(full, |(b, _)| b);
        assert!(
            src.contains("Some(\"42703\")"),
            "undefined_column must be matched so the message can be specific"
        );
        assert!(
            src.contains("ALTER TABLE noetl.catalog ADD COLUMN"),
            "the error must name the exact statement an operator has to run"
        );
    }

    /// noetl/ai-meta#237 — the archived predicate must be CONDITIONAL, never
    /// hard-coded.
    ///
    /// The server cannot create `archived_at` (the table is owned by another
    /// role), so a database may legitimately not have it. Hard-coding
    /// `archived_at IS NULL` into resolution took **production** down for ~90
    /// seconds: every execute-by-path returned 500 with
    /// `column "archived_at" does not exist`.
    ///
    /// Feature degrading is fine. Resolution breaking is not. Every read path
    /// that filters archived rows must interpolate `archived_filter()`, which is
    /// the empty string when the column is absent — making the query
    /// byte-identical to pre-soft-delete.
    #[test]
    fn archived_predicate_is_conditional_on_every_read_path() {
        fn non_test(src: &str) -> &str {
            src.split_once("\n#[cfg(test)]").map_or(src, |(b, _)| b)
        }
        let q = non_test(include_str!("db/queries/catalog.rs"));
        let ex = non_test(include_str!("handlers/execute.rs"));

        // The execute path is the one that broke prod.
        assert!(
            ex.contains("archived_filter()"),
            "resolve_catalog must interpolate archived_filter(), not hard-code the predicate"
        );
        assert!(
            !ex.contains("AND archived_at IS NULL \\"),
            "resolve_catalog must not hard-code the predicate"
        );

        // Every SELECT in the queries module must go through the helper.  The
        // remaining literal uses are the UPDATEs in archive/restore, which only
        // run once the column exists.
        fn body<'a>(src: &'a str, name: &str) -> &'a str {
            let s = src
                .find(&format!("pub async fn {name}("))
                .unwrap_or_else(|| panic!("{name}"));
            let e = src[s..].find("\n}").map(|i| s + i).unwrap_or(src.len());
            &src[s..e]
        }
        for name in [
            "get_catalog_latest",
            "get_catalog_by_path_version",
            "get_catalog_all_versions",
            "list_catalog_entries",
        ] {
            let b = body(q, name);
            assert!(
                b.contains("archived_filter()"),
                "{name} must use archived_filter(); a hard-coded predicate breaks it on a \
                 database without the column"
            );
            assert!(
                !b.contains("AND archived_at IS NULL"),
                "{name} still hard-codes the predicate"
            );
        }

        // And the invariant that survives from before: version numbering must
        // still COUNT archived rows, so it must not filter at all.
        assert!(
            !body(q, "get_next_version").contains("archived"),
            "get_next_version must count archived versions, or re-registering reuses a version"
        );
    }

    /// Detection must not depend on the ALTER succeeding.
    ///
    /// `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` fails with "must be owner" for
    /// a non-owner **even when the column already exists**, so its result says
    /// nothing about presence. If detection keyed off the ALTER, an operator
    /// adding the column out of band would never turn the feature on.
    #[test]
    fn archived_column_detection_is_independent_of_the_alter() {
        let q = include_str!("db/queries/catalog.rs");
        let src = q.split_once("\n#[cfg(test)]").map_or(q, |(b, _)| b);
        let f = src
            .find("pub async fn ensure_archived_column")
            .expect("ensure_archived_column exists");
        let body = &src[f..];
        assert!(
            body.contains("information_schema.columns"),
            "presence must be detected from information_schema, not inferred from the ALTER"
        );
        let alter_at = body.find("ALTER TABLE").expect("attempts the ALTER");
        let detect_at = body.find("information_schema").expect("detects");
        assert!(
            alter_at < detect_at,
            "detect AFTER attempting the ALTER, so a successful add is seen too"
        );
    }

    /// noetl/ai-meta#237 — both catalog-delete outcomes must be SERVED at 0.
    ///
    /// A destructive endpoint whose counter appears only after the first delete
    /// cannot answer "has anything been removed from the catalog?", and
    /// `noetl.catalog` has no replay, so that counter plus the INFO log are the
    /// entire audit trail. Absent must not read as zero here.
    ///
    /// Touches no recorder — calling one would create the series and mask the
    /// defect.
    #[test]
    fn catalog_delete_outcomes_are_served_at_zero() {
        init_catalog_delete_series();
        let text = gather_text().expect("gather metrics text");
        for outcome in CATALOG_DELETE_OUTCOMES {
            let want = format!("noetl_catalog_delete_total{{outcome=\"{outcome}\"}}");
            let line = text
                .lines()
                .find(|l| l.starts_with(&want))
                .unwrap_or_else(|| panic!("{want} must be pinned"));
            assert!(
                line.trim_end().ends_with(" 0"),
                "{want} must read 0 before any delete; got {line:?}"
            );
        }
    }

    /// The pinned set must cover the outcomes the service actually records.
    #[test]
    fn catalog_delete_pinned_outcomes_match_the_call_site() {
        let full = include_str!("services/catalog.rs");
        let src = full.split_once("\n#[cfg(test)]").map_or(full, |(b, _)| b);

        // Direction 1: everything pinned is actually recorded.
        for outcome in CATALOG_DELETE_OUTCOMES {
            assert!(
                src.contains(&format!("\"{outcome}\"")),
                "{outcome} is pinned but the service never records it"
            );
        }

        // Direction 2 — the one that matters, and the one a set-iterating test
        // cannot check: every literal the service passes to
        // `record_catalog_delete` must be pinned. Shrinking the pinned array
        // also shrinks a test that loops over it, so without this a dropped
        // outcome reintroduces the absent-series bug for that value alone while
        // the rest read 0 and look complete.
        let call = src
            .find("record_catalog_delete(")
            .expect("the service must record a delete outcome");
        let region = &src[call..call + 200];
        let recorded: Vec<&str> = region
            .match_indices('"')
            .collect::<Vec<_>>()
            .chunks(2)
            .filter_map(|c| match c {
                [(a, _), (b, _)] => Some(&region[a + 1..*b]),
                _ => None,
            })
            .collect();
        assert!(
            !recorded.is_empty(),
            "could not extract the recorded outcome literals from {region:?}"
        );
        for r in &recorded {
            assert!(
                CATALOG_DELETE_OUTCOMES.contains(r),
                "the service records {r:?} but it is not in CATALOG_DELETE_OUTCOMES, \
                 so that series is absent from /metrics until it first fires"
            );
        }
    }

    /// noetl/ai-meta#237 — the delete endpoint must be auth-gated.
    ///
    /// `register` on the same router is ungated, so "match the neighbours" would
    /// have shipped an unauthenticated destructive endpoint. This asserts the
    /// handler takes the internal-token extractor, which fails closed (503 when
    /// the server has no token, 403 on a bad one).
    ///
    /// Fails on any refactor that drops the extractor — which would compile
    /// perfectly well and silently open the surface.
    #[test]
    fn catalog_delete_handler_requires_the_internal_token() {
        let full = include_str!("handlers/catalog.rs");
        let src = full.split_once("\n#[cfg(test)]").map_or(full, |(b, _)| b);
        let start = src
            .find("pub async fn delete(")
            .expect("delete handler exists");
        let sig = &src[start..start + 400];
        assert!(
            sig.contains("RequireInternalApiToken"),
            "the delete handler must take the internal-token extractor; signature was:\n{sig}"
        );
    }

    /// The only `DELETE FROM noetl.catalog` in the crate must be the one the
    /// service calls — the data-access boundary this endpoint exists to honour.
    ///
    /// If a second raw delete appears anywhere, the endpoint stops being the
    /// single supported removal path and the audit trail has a hole.
    #[test]
    fn catalog_delete_has_exactly_one_raw_sql_site() {
        let q = include_str!("db/queries/catalog.rs");
        // Two statements live inside `delete_catalog_entries` — the
        // version-scoped delete and the whole-path delete. What matters is that
        // NONE live outside it: everything before that function must be free of
        // raw catalog deletes, or the endpoint is no longer the single removal
        // path and the audit trail has a hole.
        let before = q
            .split_once("pub async fn delete_catalog_entries")
            .map(|(b, _)| b)
            .expect("delete_catalog_entries exists");
        assert_eq!(
            before.matches("DELETE FROM noetl.catalog").count(),
            0,
            "a raw catalog DELETE exists outside delete_catalog_entries"
        );
        assert!(
            q.matches("DELETE FROM noetl.catalog").count() >= 1,
            "delete_catalog_entries must actually delete"
        );
    }

    /// noetl/ai-meta#248 — every `op` the code actually records must be pinned.
    ///
    /// An op that is recorded but not pinned is absent from `/metrics` until it
    /// first fires, so "nothing has given up on a sink" and "this build cannot
    /// tell you" look identical. Scans the non-test source of every call site so
    /// the guard cannot match its own literals.
    #[test]
    fn sink_state_pinned_ops_cover_every_call_site() {
        fn non_test(src: &str) -> &str {
            src.split_once("\n#[cfg(test)]").map_or(src, |(b, _)| b)
        }
        let sources = [
            non_test(include_str!("handlers/sink_state.rs")),
            non_test(include_str!("handlers/result_tier.rs")),
            non_test(include_str!("services/result_tier_gc.rs")),
        ];
        let mut found = std::collections::BTreeSet::new();
        for src in sources {
            let mut rest = src;
            while let Some(k) = rest.find("record_sink_state(\"") {
                rest = &rest[k + "record_sink_state(\"".len()..];
                if let Some(end) = rest.find('"') {
                    found.insert(rest[..end].to_string());
                }
            }
        }
        assert!(!found.is_empty(), "the scan found no call sites at all");
        for op in &found {
            assert!(
                SINK_STATE_OPS.contains(&op.as_str()),
                "op {op:?} is recorded but not in SINK_STATE_OPS, so it is absent \
                 from /metrics until it first fires"
            );
        }
        for pinned in SINK_STATE_OPS {
            assert!(
                found.contains(pinned),
                "op {pinned:?} is pinned but nothing records it"
            );
        }
    }

    /// The pinned ops must be SERVED at 0 without touching a recorder.
    #[test]
    fn sink_state_ops_are_served_at_zero() {
        init_sink_state_series();
        let text = gather_text().expect("gather metrics text");
        for op in SINK_STATE_OPS {
            let want = format!("noetl_sink_state_total{{op=\"{op}\"}}");
            let line = text
                .lines()
                .find(|l| l.starts_with(&want))
                .unwrap_or_else(|| panic!("{want} must be pinned"));
            assert!(
                line.trim_end().ends_with(" 0"),
                "{want} must read 0 before anything happens; got {line:?}"
            );
        }
    }

    /// The sweep terminates executions, so "it has failed nothing" and "there
    /// is no sweep in this binary" must not look the same on `/metrics`.
    #[test]
    fn sweep_outcome_series_exist_at_zero_before_any_sweep() {
        init_nonconvergence_sweep_series();
        let text = gather_text().expect("gather metrics text");
        let lines: Vec<&str> = text
            .lines()
            .filter(|l| l.starts_with("noetl_nonconvergence_sweep_total{"))
            .collect();
        for outcome in NONCONVERGENCE_SWEEP_OUTCOMES {
            assert!(
                lines
                    .iter()
                    .any(|l| l.contains(&format!("outcome=\"{outcome}\""))),
                "{outcome} series must exist before any sweep; got {lines:?}"
            );
        }
        assert!(
            lines
                .iter()
                .any(|l| l.contains("outcome=\"terminated\"") && l.trim_end().ends_with(" 0")),
            "terminated must read 0, not be absent; got {lines:?}"
        );
    }

    /// The gauge must carry the crate version, and must be present after init —
    /// its whole purpose is to be readable when every other metric is absent.
    #[test]
    fn build_info_publishes_the_crate_version() {
        init_build_info();
        let text = gather_text().expect("gather metrics text");
        let line = text
            .lines()
            .find(|l| l.starts_with("noetl_server_build_info{"))
            .expect("build_info series must exist after init");
        assert!(
            line.contains(&format!("version=\"{}\"", env!("CARGO_PKG_VERSION"))),
            "build_info must carry the crate version; got {line:?}"
        );
        assert!(line.ends_with(" 1"), "build_info must read 1; got {line:?}");
    }

    /// A recorded skip must land on its own `reason` series, not merge into a
    /// neighbouring one — the three are only useful because they separate a
    /// fault (`no_transport`) from two healthy states.
    #[test]
    fn recorded_skip_increments_only_its_own_reason() {
        init_event_ingest_publish_skipped_series();
        let before = event_ingest_publish_skipped_total()
            .with_label_values(&["no_transport"])
            .get();
        record_event_ingest_publish_skipped("system_execution");
        assert_eq!(
            event_ingest_publish_skipped_total()
                .with_label_values(&["no_transport"])
                .get(),
            before,
            "recording one reason must not move another"
        );
        assert!(
            event_ingest_publish_skipped_total()
                .with_label_values(&["system_execution"])
                .get()
                > 0,
            "the recorded reason must have moved"
        );
    }

    /// The async-mirror surface must reach `/metrics` before anything mirrors.
    ///
    /// The gauge is the load-bearing one. Without it, "the async flag is off"
    /// and "the flag is on but the drain task never started" are the same
    /// scrape — an idle queue at depth 0 — which is the inert-and-silent shape
    /// this codebase has shipped three times (noetl/ai-meta#242, #266).
    #[test]
    fn async_mirror_series_are_pinned_into_the_rendered_output() {
        init_ehdb_eventlog_mirror_queue_series();
        let text = gather_text().expect("gather must succeed");
        for expected in [
            "noetl_ehdb_eventlog_mirror_queue_total{outcome=\"enqueued\"} 0",
            "noetl_ehdb_eventlog_mirror_queue_total{outcome=\"queue_full_inline\"} 0",
            "noetl_ehdb_eventlog_mirror_queue_total{outcome=\"drained\"} 0",
            "noetl_ehdb_eventlog_mirror_queue_total{outcome=\"shutdown_abandoned\"} 0",
            "noetl_ehdb_eventlog_mirror_pending_events 0",
            "noetl_ehdb_eventlog_mirror_async_enabled 0",
        ] {
            assert!(
                text.contains(expected),
                "pinned async-mirror series missing from /metrics: {expected}"
            );
        }
        // The lag histogram is unlabelled, so it is never pruned — but it must
        // still be registered, and a bucket line is the proof.
        assert!(
            text.contains("noetl_ehdb_eventlog_mirror_lag_seconds_bucket"),
            "the lag histogram must render; it is what the parity window is checked against"
        );
        assert!(
            !text.contains("noetl_ehdb_eventlog_mirror_queue_total{outcome=\"dropped\""),
            "positive control: an unpinned outcome must be absent — and `dropped` is not a \
             policy this queue has (noetl/ai-meta#155)"
        );
    }

    /// The pin must reach `/metrics`, not just the counter.
    ///
    /// This asserts the property that actually matters and that a
    /// `.with_label_values(..).get()` check would *not* catch: `Registry::gather`
    /// prunes empty families, so the question is whether the series survives
    /// rendering. The negative half is the point — an unpinned label value must
    /// be absent, or this test would pass on a build with no pinning at all.
    #[test]
    fn crossstore_series_are_pinned_into_the_rendered_output() {
        init_ehdb_crossstore_series();
        let text = gather_text().expect("gather must succeed");

        for expected in [
            "noetl_ehdb_crossstore_parity_total{outcome=\"match\",tier=\"eventlog\"} 0",
            "noetl_ehdb_crossstore_parity_total{outcome=\"ehdb_unavailable\",tier=\"eventlog\"} 0",
            "noetl_ehdb_crossstore_divergence_total{kind=\"missing_event\",tier=\"eventlog\"} 0",
            "noetl_ehdb_crossstore_divergence_total{kind=\"unidentified\",tier=\"eventlog\"} 0",
            "noetl_ehdb_crossstore_events_compared_total{tier=\"eventlog\"} 0",
            "noetl_ehdb_crossstore_control_total{control=\"identical\",result=\"expected\"} 0",
            "noetl_ehdb_crossstore_control_total{control=\"order\",result=\"unexpected\"} 0",
            // noetl/ai-meta#155 — the lag-tolerance surface. `pending_mirror`
            // and the two lag controls are absent-by-default series on a
            // deployment that never turns the window on, which is exactly the
            // case where an operator needs to read them as 0 rather than as
            // "this binary predates the window".
            "noetl_ehdb_crossstore_parity_total{outcome=\"pending_mirror\",tier=\"eventlog\"} 0",
            "noetl_ehdb_crossstore_pending_total{tier=\"eventlog\"} 0",
            "noetl_ehdb_crossstore_parity_lag_tolerance_seconds 0",
            "noetl_ehdb_crossstore_control_total{control=\"lag_within_window\",result=\"expected\"} 0",
            "noetl_ehdb_crossstore_control_total{control=\"lag_beyond_window\",result=\"unexpected\"} 0",
        ] {
            assert!(
                text.contains(expected),
                "pinned series missing from /metrics: {expected}"
            );
        }

        // Positive control for the assertion itself: a label value nobody pins
        // must NOT be present. Without this, the loop above would pass against
        // an implementation that emitted every conceivable series.
        assert!(
            !text.contains("noetl_ehdb_crossstore_divergence_total{kind=\"not_a_kind\""),
            "an unpinned label value must not appear — the check above proves nothing otherwise"
        );
    }
}
