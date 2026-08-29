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

// ---- catalog read-source ladder (RFC step 3 §5) ----

/// The ladder must be WIRED, or flipping the flag would do nothing.
///
/// An unreachable mode is the failure this codebase keeps finding: a flag that
/// is set, documented, and read by nothing. `NOETL_CATALOG_READ_SOURCE=tier`
/// must be a cutover, not a no-op.
#[test]
fn the_catalog_read_ladder_is_wired_into_resolution() {
    assert!(
        EXECUTE.contains("catalog_read::mode()"),
        "resolve_catalog does not consult the read-source mode; the flag would be inert"
    );
    assert!(
        EXECUTE.contains("catalog_read::compare_latest("),
        "the comparison is never performed, so `verify` would measure nothing"
    );
}

/// ⭐ The relation's answer is NOT served here.
///
/// This is the held decision. `verify` must resolve from Postgres, and the
/// cutover must be a deliberate, separate change — not something that arrives
/// by accident in a refactor.
#[test]
fn resolution_still_returns_the_incumbent_answer() {
    let at = EXECUTE
        .find("catalog_read::mode()")
        .expect("the ladder must be wired");
    let tail = &EXECUTE[at..];
    let end = tail.find("} else {").expect("the by-path branch must end");
    let block = &tail[..end];
    assert!(
        block.contains("Ok((entry.0, entry.1))"),
        "resolution no longer returns the incumbent tuple — the read-cutover may \
         have been taken by accident:\n{block}"
    );
    for forbidden in ["rel.get_latest", "relation.get_latest", "e.version)"] {
        assert!(
            !block.contains(forbidden),
            "the relation's answer is being used to resolve; that is the held \
             read-cutover, not a staging step: found `{forbidden}`"
        );
    }
}
