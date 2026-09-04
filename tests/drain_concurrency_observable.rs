//! The #320 mitigation's engagement must be observable at runtime.
//!
//! **noetl/ai-meta#320.** `NOETL_EHDB_EVENTLOG_MIRROR_DRAIN_CONCURRENCY=1` was
//! applied to production as the mitigation for a 99% mirror failure rate — and
//! there was **no runtime evidence anywhere that it had taken effect**. The knob
//! is read per drain pass rather than at startup, and the ARMED log line reported
//! `capacity` / `drain_max` / `enqueue_timeout_ms` but not this one.
//!
//! The only way to check was the Deployment spec, which is a *different
//! representation* of the running process and can disagree with it. A rollback
//! knob whose engagement cannot be observed is the same defect class as the
//! incident it mitigates.

const QUEUE: &str = include_str!("../src/handlers/ehdb_eventlog_mirror_queue.rs");
const METRICS: &str = include_str!("../src/metrics.rs");

fn non_test(src: &str) -> &str {
    src.split_once("\n#[cfg(test)]").map_or(src, |(b, _)| b)
}

/// ⭐ The ARMED line reports the concurrency alongside the other three knobs.
///
/// ⚠ Mutation verified: removing `drain_concurrency` from the `info!` fails this.
#[test]
fn the_armed_log_line_reports_the_drain_concurrency() {
    let src = non_test(QUEUE);
    let at = src
        .find("async event-log mirror queue ARMED")
        .expect("the ARMED line must exist");
    // The fields precede the message in a tracing macro, so look back from it.
    let head = &src[at.saturating_sub(600)..at];
    for field in [
        "capacity",
        "drain_max",
        "enqueue_timeout_ms",
        "drain_concurrency",
    ] {
        assert!(
            head.contains(field),
            "the ARMED line omits `{field}`. Every knob that changes drain \
             behaviour has to be in it, or an operator reading the log cannot \
             tell what the process is actually doing:\n{head}"
        );
    }
}

/// ⭐ The gauge is published from the value the pass ACTUALLY used.
///
/// Setting it only at startup would let it disagree with the running behaviour,
/// because the knob is re-read per pass. That gap is exactly what left the
/// mitigation unobservable.
///
/// ⚠ Mutation verified: moving the `.set(..)` out of `deliver_pass` and into
/// `init` fails this.
#[test]
fn the_gauge_is_set_from_the_value_the_pass_used() {
    let src = non_test(QUEUE);
    let at = src
        .find("async fn deliver_pass")
        .expect("deliver_pass must exist");
    let body = &src[at..];
    let end = body.find("\n}\n").unwrap_or(body.len());
    let body = &body[..end];
    assert!(
        body.contains("ehdb_eventlog_mirror_drain_concurrency().set("),
        "deliver_pass does not publish the concurrency it used, so the gauge can \
         disagree with the running drain"
    );
    let set_at = body.find("drain_concurrency().set(").unwrap();
    let conc_at = body
        .find("let concurrency = configured_drain_concurrency();")
        .expect("the pass must resolve the concurrency");
    assert!(
        conc_at < set_at,
        "the gauge is published before the value is resolved, so it cannot be \
         reporting what this pass used"
    );
}

/// ⚠ The gauge and the drain must read the SAME source.
///
/// Two independent `env_usize` reads would let the published value and the
/// applied value drift — a gauge that is confidently wrong is worse than none,
/// because it ends an investigation early.
#[test]
fn the_published_value_and_the_applied_value_share_one_reader() {
    let src = non_test(QUEUE);
    assert_eq!(
        src.matches("env_usize(DRAIN_CONCURRENCY_ENV").count(),
        1,
        "DRAIN_CONCURRENCY is read from the env in more than one place; the \
         published value can then drift from the applied one"
    );
    assert!(
        src.contains("pub fn configured_drain_concurrency() -> usize {"),
        "the single reader must be a named, shared helper"
    );
}

/// ⚠ Pinned at the CONFIGURED value, not 0.
///
/// Unlike the counters beside it this is not a tally that starts empty: 0 is not
/// a legal concurrency. Pinning it at 0 would publish a value the drain can never
/// use, making an idle process look misconfigured.
#[test]
fn the_gauge_is_pinned_to_the_configured_value_not_zero() {
    // ⚠ Deliberately NOT `non_test` here. That helper cuts at the FIRST
    // `#[cfg(test)]`, which in `metrics.rs` is at line 248 — thousands of lines
    // before this function. Using it made this test search an empty tail and fail
    // for a reason that had nothing to do with the code. A file with inline test
    // modules needs the function extracted, not the file truncated.
    let at = METRICS
        .find("pub fn init_ehdb_eventlog_mirror_queue_series()")
        .expect("the pin fn must exist");
    let rest = &METRICS[at..];
    let end = rest.find("\n}\n").map(|e| e + 2).unwrap_or(rest.len());
    let body = &rest[..end];
    assert!(
        body.len() > 200,
        "extracted an implausibly short function body ({} bytes) — the extraction \
         is broken and any pass below would be vacuous",
        body.len()
    );
    assert!(
        body.contains("ehdb_eventlog_mirror_drain_concurrency()"),
        "the drain-concurrency gauge is not pinned, so it is ABSENT until the \
         first drain pass — and absent reads as 'no such metric', which is the \
         same silence this fixes"
    );
    assert!(
        !body.contains("ehdb_eventlog_mirror_drain_concurrency()\n        .set(0)"),
        "pinned at 0, which is not a legal concurrency"
    );
}
