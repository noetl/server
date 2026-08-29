//! Structural guards for the execution-start catalog snapshot.
//!
//! These assert properties of the CALL SITE that no unit test on the module can
//! see: that the snapshot is fail-safe at the point it is used, and that it is
//! emitted after the event whose primacy other code depends on.

const EXECUTE: &str = include_str!("../src/handlers/execute.rs");

/// The snapshot must be emitted AFTER `playbook_started`.
///
/// `playbook_started` must remain the execution's first event: it is the chain
/// root, `get_latest_event(.., "playbook_started")` looks it up, and the
/// execute-time descriptor is seeded from it. A snapshot emitted first would
/// take the root and the chain would still walk — silently, with the wrong
/// first event.
#[test]
fn the_snapshot_is_emitted_after_playbook_started() {
    let started = EXECUTE
        .find("emit_playbook_started_event(")
        .expect("execute_one must still emit playbook_started");
    let snapshot = EXECUTE
        .find("catalog_snapshot::record(")
        .expect("execute_one must record the catalog snapshot");
    assert!(
        started < snapshot,
        "the catalog snapshot is emitted BEFORE playbook_started; that makes the \
         snapshot the chain root and displaces the event the descriptor seeding \
         and get_latest_event both depend on"
    );
}

/// The call must be fail-safe: no `?`, no `unwrap`, no `expect`.
///
/// An execution that failed to start because its audit record could not be
/// written would be strictly worse than one with no audit record. `record`
/// returns `()`, so this is already true by type — the guard exists so that a
/// future change to a fallible signature has to confront this comment rather
/// than propagate an error into the execute path unnoticed.
#[test]
fn the_snapshot_call_cannot_fail_the_execution() {
    let at = EXECUTE
        .find("catalog_snapshot::record(")
        .expect("call site must exist");
    let tail = &EXECUTE[at..];
    let end = tail.find(".await").expect("the call is awaited") + ".await".len();
    let call = &tail[..end + 2];
    for forbidden in ["?", "unwrap", "expect"] {
        assert!(
            !call.contains(forbidden),
            "the catalog snapshot call uses `{forbidden}` — a record ABOUT an \
             execution must never be a precondition FOR one:\n{call}"
        );
    }
}

/// The snapshot pins the content the parser consumed and the EFFECTIVE workload.
///
/// Passing `request.payload` instead of the merged `workload` would record what
/// was asked for rather than what ran, which is the one thing this event exists
/// to get right.
#[test]
fn the_snapshot_is_given_the_parsed_content_and_effective_workload() {
    let at = EXECUTE.find("catalog_snapshot::record(").unwrap();
    let call = &EXECUTE[at..at + 260];
    assert!(
        call.contains("&playbook_yaml"),
        "snapshot must receive the resolved content parse_playbook consumed:\n{call}"
    );
    assert!(
        call.contains("&workload"),
        "snapshot must receive the EFFECTIVE merged workload, not request.payload:\n{call}"
    );
    assert!(
        !call.contains("request.payload"),
        "snapshot was given the request payload; that records what was asked \
         for, not what ran:\n{call}"
    );
}
