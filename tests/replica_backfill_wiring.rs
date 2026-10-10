//! **The off-box replica is wired to a backfill, and says so when it is not.**
//!
//! Arming a replica substrate replicates only parts sealed *after* the attach —
//! uploads are enqueued on seal and nowhere else (noetl/ehdb#400). Measured on
//! prod 2026-10-10 from the remote's own manifest: 40 parts listed, **39 with
//! `replica_count` 1**, while every aggregate gauge read healthy
//! (`survives_node_loss 1`) because those describe the replica *set*, not the
//! parts.
//!
//! Two things this pins, both of which are about a signal rather than a
//! behaviour:
//!
//! 1. The open path calls `backfill_under_replicated` — a method that exists
//!    and is never called is the defect this codebase keeps producing.
//! 2. When the replica is armed and the backfill is **off**, the log says the
//!    remote is not a recoverable copy. Otherwise the healthy-looking gauges
//!    are the only thing an operator sees, and they are describing something
//!    else.

const SRC: &str = include_str!("../src/handlers/ehdb_embedded.rs");

/// The source with `#[cfg(test)]` modules removed, so a string that appears
/// only inside this file's own tests cannot satisfy a production-call-site
/// assertion.
fn non_test(src: &str) -> &str {
    match src.find("#[cfg(test)]") {
        Some(i) => &src[..i],
        None => src,
    }
}

#[test]
fn the_open_path_calls_the_backfill() {
    let src = non_test(SRC);
    // ⚠ Assert the extraction before asserting about it: a `non_test` helper
    // that cut at the wrong place would make every check below pass over an
    // empty string.
    assert!(
        src.len() > 10_000,
        "non_test() returned {} bytes — the slice is implausibly small, so the \
         assertions below would be computed over nothing",
        src.len()
    );
    assert!(
        src.contains("backfill_under_replicated()"),
        "nothing calls the backfill, so existing history never reaches the \
         off-box replica"
    );
    // It must be reached from the engine-open path, where the replica set is
    // assembled — not from some unrelated handler.
    let open_at = src
        .find("fn open_embedded")
        .expect("open_embedded is the engine-open path");
    let call_at = src
        .find("backfill_under_replicated()")
        .expect("checked above");
    assert!(
        call_at > open_at,
        "the backfill call must sit in the engine-open path"
    );
}

#[test]
fn an_armed_replica_without_a_backfill_says_the_remote_is_not_recoverable() {
    let src = non_test(SRC);
    assert!(
        src.contains("NOT a recoverable copy"),
        "a replica armed with the backfill off must say so — the gauges will \
         read healthy and they are describing the replica set, not the parts"
    );
    assert!(
        src.contains("NOETL_EHDB_REPLICA_BACKFILL"),
        "the warning must name the flag that turns it on"
    );
}

#[test]
fn the_backfill_flag_defaults_off() {
    // It copies existing history off-box: real egress, real object writes. That
    // is opted into, not something that happens on the first restart after a
    // bucket is configured.
    let src = non_test(SRC);
    let at = src
        .find("fn replica_backfill_enabled")
        .expect("the flag reader exists");
    let body = &src[at..(at + 400).min(src.len())];
    assert!(
        body.contains("unwrap_or(false)"),
        "the backfill flag must default OFF; body was: {body}"
    );
}
