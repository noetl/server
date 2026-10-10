//! The archive pass driver — [noetl/ai-meta#459](https://github.com/noetl/ai-meta/issues/459).
//!
//! ## ⚠⚠ Why this file exists
//!
//! P1–P6 shipped every primitive and **nothing called them**. A control grep against
//! `origin/main` found `archive_execution_indexed`, `archive_execution`, `verify_archived`,
//! `archive_state`, `export_floor` and `export_scan` with **zero callers** outside their own
//! module, and no spawn in `main.rs`. `ARCHIVE_ENABLED=true` would have archived nothing
//! while every flag and metric read exactly as a working deployment does.
//!
//! These tests are the reachability proof that was missing.

use chrono::{Duration, TimeZone, Utc};
use noetl_server::services::archive_store::FakeArchiveStore;
use noetl_server::services::event_archive as ea;

fn t(h: i64) -> chrono::DateTime<chrono::Utc> {
    Utc.timestamp_opt(1_790_000_000, 0).unwrap() + Duration::hours(h)
}

fn recs(n: usize, base: u64) -> Vec<ea::ArchiveRecord> {
    (0..n)
        .map(|i| ea::ArchiveRecord {
            global_sequence: base + i as u64,
            // ⚠ None is the REAL shape for shadow-written records; the tests must exercise
            // it rather than a convenient id.
            event_id: if i % 2 == 0 { None } else { Some(format!("ev-{i}")) },
            event_type: "step.completed".into(),
            body: serde_json::json!({"i": i}),
        })
        .collect()
}

fn cfg_on() -> ea::RetentionConfig {
    ea::RetentionConfig {
        archive_enabled: true,
        bucket: Some("b".into()),
        ..Default::default()
    }
}

/// Candidates: (id, status, completed_at, started_at)
fn cand(id: i64, status: &str, done_h: Option<i64>) -> ea::ArchiveCandidate {
    ea::ArchiveCandidate {
        execution_id: id,
        status: status.into(),
        completed_at: done_h.map(t),
        started_at: t(0),
    }
}

/// ⭐⭐ THE REACHABILITY PROOF: a pass actually archives, indexes and verifies.
#[tokio::test]
async fn a_pass_archives_indexes_and_verifies_an_expired_execution() {
    let store = FakeArchiveStore::new();
    let cands = vec![cand(1, "COMPLETED", Some(0)), cand(2, "FAILED", Some(1))];
    let rep = ea::archive_pass(&store, &cfg_on(), &cands, t(100), &mut Default::default(), &mut Default::default(), |id| {
        Some(recs(10, 1_000 + id as u64 * 100))
    })
    .await;

    assert_eq!(rep.skipped, None, "the pass must have run");
    assert_eq!(rep.examined, 2);
    assert_eq!(rep.archivable, 2);
    assert_eq!(rep.archived, 2, "{}", rep.describe());
    assert_eq!(rep.verified, 2, "every archive must be read back and verified");
    assert_eq!(rep.verify_failed, 0);
    assert_eq!(rep.failed, 0);

    // The objects exist in the required layout, both prefixes.
    for id in [1, 2] {
        assert!(ea::archive_state(&store, id).await.unwrap().is_safe_to_prune());
    }
    let keys = store.keys();
    assert_eq!(
        keys.iter().filter(|k| k.starts_with("execution_id=")).count(),
        4,
        "2 executions x (data + manifest): {keys:?}"
    );
    assert_eq!(keys.iter().filter(|k| k.starts_with("date=")).count(), 2);
}

/// ⚠⚠ Idempotent: a second pass archives nothing new and reports it as already-archived
/// rather than silently re-uploading.
#[tokio::test]
async fn a_second_pass_is_a_no_op_and_says_so() {
    let store = FakeArchiveStore::new();
    let cands = vec![cand(5, "COMPLETED", Some(0))];
    let first = ea::archive_pass(&store, &cfg_on(), &cands, t(100), &mut Default::default(), &mut Default::default(), |_| Some(recs(4, 1))).await;
    assert_eq!(first.archived, 1);
    let before = store.len();

    let second = ea::archive_pass(&store, &cfg_on(), &cands, t(100), &mut Default::default(), &mut Default::default(), |_| Some(recs(4, 1))).await;
    assert_eq!(second.archived, 0);
    assert_eq!(second.already_archived, 1, "{}", second.describe());
    assert_eq!(store.len(), before, "a second pass must create no objects");
}

/// ⚠⚠ Out of coverage is SKIPPED, never archived as empty. An empty archive would later
/// verify clean and authorise pruning events that were never copied.
#[tokio::test]
async fn an_execution_the_hot_store_does_not_hold_is_skipped_not_archived_empty() {
    let store = FakeArchiveStore::new();
    let cands = vec![cand(7, "COMPLETED", Some(0))];
    let rep = ea::archive_pass(&store, &cfg_on(), &cands, t(100), &mut Default::default(), &mut Default::default(), |_| Some(vec![])).await;
    assert_eq!(rep.out_of_coverage, 1, "{}", rep.describe());
    assert_eq!(rep.archived, 0);
    assert!(store.is_empty(), "nothing may be written: {:?}", store.keys());
}

/// The engine being closed stops the pass with a reason, rather than logging per candidate.
#[tokio::test]
async fn a_closed_engine_stops_the_pass_with_a_reason() {
    let store = FakeArchiveStore::new();
    let cands = vec![cand(8, "COMPLETED", Some(0))];
    let rep = ea::archive_pass(&store, &cfg_on(), &cands, t(100), &mut Default::default(), &mut Default::default(), |_| None).await;
    assert!(rep.skipped.as_deref().unwrap_or("").contains("not open"), "{rep:?}");
    assert!(store.is_empty());
}

/// ⭐ Only EXPIRED executions are archived, and a running one is never touched.
#[tokio::test]
async fn the_pass_respects_the_retention_window_and_skips_non_terminal() {
    let store = FakeArchiveStore::new();
    let cands = vec![
        cand(10, "COMPLETED", Some(0)),   // 100h old -> archivable
        cand(11, "COMPLETED", Some(99)),  // 1h old   -> within retention
        cand(12, "RUNNING", None),        // not terminal
    ];
    let rep = ea::archive_pass(&store, &cfg_on(), &cands, t(100), &mut Default::default(), &mut Default::default(), |_| Some(recs(3, 1))).await;
    assert_eq!(rep.examined, 3);
    assert_eq!(rep.archivable, 1, "{}", rep.describe());
    assert_eq!(rep.archived, 1);
    assert!(ea::archive_state(&store, 10).await.unwrap().is_safe_to_prune());
    assert_eq!(ea::archive_state(&store, 11).await.unwrap(), ea::ArchiveState::NotArchived);
    assert_eq!(ea::archive_state(&store, 12).await.unwrap(), ea::ArchiveState::NotArchived);
}

/// ⚠⚠ The flags gate it. With archiving off the pass does nothing and names the variable.
#[tokio::test]
async fn the_pass_is_a_no_op_when_the_flag_or_bucket_is_missing() {
    let store = FakeArchiveStore::new();
    let cands = vec![cand(20, "COMPLETED", Some(0))];

    let off = ea::archive_pass(&store, &ea::RetentionConfig::default(), &cands, t(100), &mut Default::default(), &mut Default::default(), |_| {
        Some(recs(3, 1))
    })
    .await;
    assert!(off.skipped.as_deref().unwrap_or("").contains("NOETL_EHDB_ARCHIVE_ENABLED"), "{off:?}");
    assert!(store.is_empty());

    let no_bucket = ea::RetentionConfig { archive_enabled: true, ..Default::default() };
    let nb = ea::archive_pass(&store, &no_bucket, &cands, t(100), &mut Default::default(), &mut Default::default(), |_| Some(recs(3, 1))).await;
    assert!(nb.skipped.as_deref().unwrap_or("").contains("NOETL_EHDB_ARCHIVE_BUCKET"), "{nb:?}");
    assert!(store.is_empty(), "a flag with no bucket must write nothing");
}

/// ⚠⚠ A write that does not verify is reported as verify_failed, not as success.
#[tokio::test]
async fn an_archive_that_does_not_verify_is_not_counted_as_success() {
    let store = FakeArchiveStore::new();
    let cands = vec![cand(30, "COMPLETED", Some(0))];
    // Make the data object vanish on read, so the write "succeeds" and verification cannot.
    store.vanish(&ea::data_key(30, 1, 3));
    let rep = ea::archive_pass(&store, &cfg_on(), &cands, t(100), &mut Default::default(), &mut Default::default(), |_| Some(recs(3, 1))).await;
    assert_eq!(rep.archived, 1, "the write itself succeeded");
    assert_eq!(rep.verified, 0, "but it must NOT count as verified: {}", rep.describe());
    assert_eq!(rep.verify_failed, 1);
    // And therefore it is not safe to prune behind.
    assert!(!ea::archive_state(&store, 30).await.unwrap().is_safe_to_prune());
}

/// `max_per_pass` bounds the work.
#[tokio::test]
async fn the_pass_is_bounded_by_max_per_pass() {
    let store = FakeArchiveStore::new();
    let cands: Vec<_> = (0..50).map(|i| cand(100 + i, "COMPLETED", Some(0))).collect();
    let cfg = ea::RetentionConfig { max_per_pass: 7, ..cfg_on() };
    let rep = ea::archive_pass(&store, &cfg, &cands, t(100), &mut Default::default(), &mut Default::default(), |_| Some(recs(2, 1))).await;
    assert_eq!(rep.archivable, 50, "all 50 are archivable");
    assert_eq!(rep.archived, 7, "but only max_per_pass are done: {}", rep.describe());
}

/// ⚠⚠ Pruning is refused LOUDLY, not silently. An operator who sets the flag and sees
/// nothing happen must be told why — a flag that appears to work while doing nothing is the
/// defect this whole program keeps finding.
///
/// ⚠⚠ This test PINNED THE STALE TEXT until 2026-10-10. It asserted
/// `note.contains("NOT implemented")` and `note.contains("nothing is deleted")` — claims
/// that stopped being true when server#516 wired the floor to `apply_retention`, so the
/// test actively held the contradiction in place and would have failed any correction. The
/// same shape as the `NoFootprint` assertion that pinned the all-archived defect.
///
/// The intent was right and is kept: the refusal must be LOUD and must name its reason.
/// Only the specific strings were wrong.
#[test]
fn the_prune_refusal_names_the_reason() {
    let note = ea::prune_readiness_note();
    // The reason an operator needs: arming the flag reclaims NOTHING until the backlog is
    // drained, because the floor refuses. Without that sentence, zero reclaimed bytes reads
    // as a broken feature.
    assert!(note.contains("floor"), "the refusal must name the floor: {note}");
    assert!(
        note.to_lowercase().contains("refuse") || note.to_lowercase().contains("until"),
        "it must say arming reclaims nothing until the backlog drains: {note}"
    );
    assert!(note.contains("459"), "it must cite where to follow up: {note}");
    // And it must not resurrect the stale claims.
    assert!(
        !note.contains("NOT implemented") && !note.contains("nothing is deleted"),
        "the note contradicts the code again: {note}"
    );
}

/// The record shape is faithful: `event_id: None` is the real value for shadow-written
/// records and must round-trip as absent, not as a fabricated id.
#[tokio::test]
async fn a_none_event_id_round_trips_as_absent() {
    let store = FakeArchiveStore::new();
    let cands = vec![cand(40, "COMPLETED", Some(0))];
    ea::archive_pass(&store, &cfg_on(), &cands, t(100), &mut Default::default(), &mut Default::default(), |_| Some(recs(4, 1))).await;
    let (_, back) = ea::read_archived(&store, 40).await.unwrap().unwrap();
    assert_eq!(back.len(), 4);
    assert!(back.iter().any(|r| r.event_id.is_none()), "None must survive the round trip");
    assert!(back.iter().any(|r| r.event_id.is_some()), "and Some must too");
}

/// ⭐⭐ THE GUARD AGAINST THE BUG THIS FILE EXISTS FOR.
///
/// Every archive primitive must have a production caller. P1–P6 shipped six of them with
/// **none**, and the only symptom was that nothing happened — no error, no absent metric,
/// every flag reading exactly as a working deployment does.
///
/// ⚠ A structural test, which is necessary and not sufficient (gating a hook says nothing
/// about what reaches it). The sufficiency is the pass tests above, which drive the real
/// `archive_pass`. This guard catches the *other* failure: the pass existing and never
/// being spawned.
#[test]
fn every_archive_primitive_has_a_production_caller() {
    let main_rs = include_str!("../src/main.rs");
    assert!(
        main_rs.contains("spawn_archive_pass("),
        "⚠⚠ main.rs does not spawn the archive pass. ARCHIVE_ENABLED would then be INERT: \
         the flag, the config and all 14 metrics would read exactly as a working \
         deployment does while nothing was archived. That is precisely the defect this \
         module was added to fix."
    );

    // And the pass itself must reach the primitives.
    let src = include_str!("../src/services/event_archive.rs");
    let pass_at = src
        .find("pub async fn archive_pass")
        .expect("the pass exists");
    let pass = &src[pass_at..];
    for needed in [
        "archive_state(",
        "archive_execution_indexed(",
        "verify_archived(",
        "export_scan(",
    ] {
        assert!(
            pass.contains(needed),
            "the pass does not call {needed} — a primitive with no caller is the bug, not \
             the fix"
        );
    }

    // ⭐ The spawn must read the records from the EMBEDDED store, which is the store that
    // gets pruned. Sourcing them from anywhere else would archive a different
    // representation and make the prune unverifiable against it.
    assert!(
        src.contains("read_archive_records("),
        "the pass must source records from the embedded engine"
    );
}

/// ⚠⚠⚠ THE BUG THIS FIX EXISTS FOR: a pass must be able to progress PAST `max_per_pass`.
///
/// The first version wrote `scan.archivable.iter().take(cfg.max_per_pass)`, which took the
/// SAME first N ids every pass. Once those were archived, every later pass re-checked them,
/// reported `archived=0 already=N`, and the backlog stopped draining at exactly N. Observed
/// on prod: three consecutive passes of `archived=0 already=100` against 6,230 archivable.
#[tokio::test]
async fn consecutive_passes_drain_the_backlog_instead_of_re_checking_the_same_head() {
    let store = FakeArchiveStore::new();
    let cands: Vec<_> = (1..=25).map(|i| cand(i, "COMPLETED", Some(0))).collect();
    let cfg = ea::RetentionConfig { max_per_pass: 10, ..cfg_on() };
    let mut known = std::collections::HashSet::new();

    let p1 = ea::archive_pass(&store, &cfg, &cands, t(100), &mut known, &mut Default::default(), |_| Some(recs(2, 1))).await;
    assert_eq!(p1.archived, 10, "pass 1: {}", p1.describe());

    let p2 = ea::archive_pass(&store, &cfg, &cands, t(100), &mut known, &mut Default::default(), |_| Some(recs(2, 1))).await;
    assert_eq!(
        p2.archived, 10,
        "⚠⚠⚠ pass 2 archived {} — the backlog is NOT draining, which is the exact prod \
         defect (archived=0 already=N forever): {}",
        p2.archived,
        p2.describe()
    );

    let p3 = ea::archive_pass(&store, &cfg, &cands, t(100), &mut known, &mut Default::default(), |_| Some(recs(2, 1))).await;
    assert_eq!(p3.archived, 5, "pass 3 finishes the tail: {}", p3.describe());

    let p4 = ea::archive_pass(&store, &cfg, &cands, t(100), &mut known, &mut Default::default(), |_| Some(recs(2, 1))).await;
    assert_eq!(p4.archived, 0, "pass 4 has nothing left");
    assert_eq!(p4.already_archived, 25);

    // ⭐ All 25 actually in the store, by set equality — not just a count of 25.
    for id in 1..=25 {
        assert!(
            ea::archive_state(&store, id).await.unwrap().is_safe_to_prune(),
            "execution {id} was counted but is not durably archived"
        );
    }
}

/// ⚠ The known-archived set must be populated only AFTER verification. A set filled on
/// write would let a later pass skip an execution whose archive never landed.
#[tokio::test]
async fn an_unverified_archive_is_not_remembered_as_done() {
    let store = FakeArchiveStore::new();
    let cands = vec![cand(90, "COMPLETED", Some(0))];
    store.vanish(&ea::data_key(90, 1, 2));
    let mut known = std::collections::HashSet::new();
    let p1 = ea::archive_pass(&store, &cfg_on(), &cands, t(100), &mut known, &mut Default::default(), |_| Some(recs(2, 1))).await;
    assert_eq!(p1.verify_failed, 1);
    assert!(
        !known.contains(&90),
        "an archive that did not verify must NOT be remembered as done — a later pass \
         would skip it and the prune floor would advance past data that never landed"
    );
}
