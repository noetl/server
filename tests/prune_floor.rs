//! The prune floor — [noetl/ai-meta#459](https://github.com/noetl/ai-meta/issues/459).
//!
//! ⚠⚠⚠ The property under test is a **negative** one: the floor must never permit dropping
//! a part that could hold a live execution's events. `noetl.event` is append-only with
//! replay as the source of truth, so that is data loss, not eviction.
//!
//! The floor exists because `plan_retention` drops whole parts below a sort-key floor while
//! a part holds MANY executions interleaved, and `PartMeta` carries only a bloom — which
//! cannot enumerate. The way out is that the *constraining* set is bounded by the retention
//! window, not by history: on prod the pass reported `examined=6496 archivable=6230`, so
//! only **266** executions constrain.

use std::collections::HashSet;

use chrono::{Duration, TimeZone, Utc};
use noetl_server::services::event_archive as ea;

fn t(h: i64) -> chrono::DateTime<chrono::Utc> {
    Utc.timestamp_opt(1_790_000_000, 0).unwrap() + Duration::hours(h)
}

fn cand(id: i64, status: &str, done_h: Option<i64>) -> ea::ArchiveCandidate {
    ea::ArchiveCandidate {
        execution_id: id,
        status: status.into(),
        completed_at: done_h.map(t),
        started_at: t(0),
    }
}

fn hot(id: i64, min: u64, max: u64) -> ea::HotExecution {
    ea::HotExecution { execution_id: id, min_sequence: min, max_sequence: max }
}

fn scan_of(cands: &[ea::ArchiveCandidate], now_h: i64) -> ea::ArchivableScan {
    let mut s = ea::ArchivableScan::default();
    let rows: Vec<(i64, String, Option<chrono::DateTime<chrono::Utc>>)> = cands
        .iter()
        .map(|c| (c.execution_id, c.status.clone(), c.completed_at))
        .collect();
    ea::classify_into(&mut s, &rows, t(now_h), Duration::hours(48));
    s
}

/// ⚠⚠⚠ THE DATA-LOSS CASE. A live execution whose earliest events sit in the oldest part
/// must pin the floor below that part — even when every other execution is archived.
#[test]
fn a_live_execution_pins_the_floor_and_its_part_is_not_droppable() {
    let cands = vec![
        cand(1, "COMPLETED", Some(0)),  // archivable, archived
        cand(2, "COMPLETED", Some(1)),  // archivable, archived
        cand(3, "RUNNING", None),       // ⚠ LIVE — started early, still going
    ];
    let scan = scan_of(&cands, 100);
    assert_eq!(scan.archivable, vec![1, 2]);
    assert_eq!(scan.not_terminal, 1);

    let archived: HashSet<i64> = [1, 2].into_iter().collect();
    // The live execution's first event is at sequence 120, inside the part [100,200].
    let footprint = vec![hot(1, 100, 150), hot(2, 150, 180), hot(3, 120, 999)];

    let basis = ea::compute_prune_floor(&scan, &archived, &footprint);
    match &basis {
        ea::FloorBasis::Computed { keep_from_sequence, constrained_by } => {
            assert_eq!(*keep_from_sequence, 120, "the floor must sit at the LIVE execution");
            assert_eq!(*constrained_by, 1);
        }
        other => panic!("expected Computed, got {other:?}"),
    }

    let d = ea::retention_floor(&footprint, &[1, 2]);
    assert!(
        !d.part_is_droppable(100, 200),
        "⚠⚠⚠ a part holding the LIVE execution's events must never be droppable"
    );
    assert!(d.part_is_droppable(100, 119), "a part strictly below the floor may drop");
    assert!(!d.part_is_droppable(119, 121), "a part straddling the floor is kept WHOLE");

    // ⭐ Positive control: archive the live one too and the same part becomes droppable —
    // so the refusal above is the live execution, not a floor that always refuses.
    let all: HashSet<i64> = [1, 2, 3].into_iter().collect();
    let scan2 = scan_of(&[cand(1, "COMPLETED", Some(0)), cand(2, "COMPLETED", Some(1)), cand(3, "COMPLETED", Some(2))], 100);
    let basis2 = ea::compute_prune_floor(&scan2, &all, &footprint);
    assert!(matches!(basis2, ea::FloorBasis::NoFootprint), "{basis2:?}");
    let d2 = ea::retention_floor(&footprint, &[1, 2, 3]);
    assert!(d2.part_is_droppable(100, 200));
}

/// ⚠⚠ A BACKLOG blocks the floor entirely. While any archivable execution is not yet
/// verified-archived, a part may hold it, so nothing may drop.
#[test]
fn an_unarchived_backlog_refuses_to_advance_the_floor() {
    let cands: Vec<_> = (1..=50).map(|i| cand(i, "COMPLETED", Some(0))).collect();
    let scan = scan_of(&cands, 100);
    assert_eq!(scan.archivable.len(), 50);

    // 49 archived, one still pending.
    let archived: HashSet<i64> = (1..=49).collect();
    let footprint: Vec<_> = (1..=50).map(|i| hot(i, 1000 + i as u64, 2000)).collect();

    let basis = ea::compute_prune_floor(&scan, &archived, &footprint);
    assert_eq!(basis, ea::FloorBasis::AwaitingBacklog { unarchived: 1 });
    assert!(!basis.may_prune(), "a single unarchived execution must block the prune");
    assert_eq!(basis.keep_from_sequence(), None);
    assert!(basis.reason().contains("not yet verified-archived"), "{}", basis.reason());

    // ⭐ Positive control: with the last one archived, the floor computes.
    let all: HashSet<i64> = (1..=50).collect();
    let b2 = ea::compute_prune_floor(&scan, &all, &footprint);
    assert!(matches!(b2, ea::FloorBasis::NoFootprint | ea::FloorBasis::Computed { .. }), "{b2:?}");
}

/// ⚠⚠ An empty footprint must NOT be read as "everything is prunable". Absence of evidence
/// is not evidence of absence.
#[test]
fn an_empty_footprint_refuses_rather_than_pruning_everything() {
    let cands = vec![cand(1, "COMPLETED", Some(0))];
    let scan = scan_of(&cands, 100);
    let archived: HashSet<i64> = [1].into_iter().collect();
    let basis = ea::compute_prune_floor(&scan, &archived, &[]);
    assert_eq!(basis, ea::FloorBasis::NoFootprint);
    assert!(!basis.may_prune(), "no footprint must never authorise a prune");
}

/// Within-retention executions constrain too — being terminal is not enough.
#[test]
fn a_recently_completed_execution_still_constrains_the_floor() {
    let cands = vec![
        cand(1, "COMPLETED", Some(0)),   // 100h old -> archivable
        cand(2, "COMPLETED", Some(99)),  // 1h old   -> within retention, NOT archivable
    ];
    let scan = scan_of(&cands, 100);
    assert_eq!(scan.archivable, vec![1]);
    assert_eq!(scan.within_retention, 1);

    let archived: HashSet<i64> = [1].into_iter().collect();
    let footprint = vec![hot(1, 500, 600), hot(2, 50, 700)];
    match ea::compute_prune_floor(&scan, &archived, &footprint) {
        ea::FloorBasis::Computed { keep_from_sequence, .. } => assert_eq!(
            keep_from_sequence, 50,
            "the recent execution's earliest event bounds the floor"
        ),
        other => panic!("{other:?}"),
    }
}

/// ⭐ The relevant-id set is bounded by the window, not by history — the measurement that
/// makes this design affordable. Prod: 266 of 6,496.
#[test]
fn the_floor_only_needs_the_constraining_executions_not_the_whole_population() {
    // 1000 old+archived, 7 recent, 3 running.
    let mut cands: Vec<_> = (1..=1000).map(|i| cand(i, "COMPLETED", Some(0))).collect();
    cands.extend((1001..=1007).map(|i| cand(i, "COMPLETED", Some(99))));
    cands.extend((1008..=1010).map(|i| cand(i, "RUNNING", None)));
    let scan = scan_of(&cands, 100);
    assert_eq!(scan.archivable.len(), 1000);

    let archived: HashSet<i64> = (1..=1000).collect();
    let ids = ea::floor_relevant_ids(&scan, &archived, &cands);
    assert_eq!(
        ids.len(),
        10,
        "only the 7 recent + 3 running constrain; got {} of {}",
        ids.len(),
        cands.len()
    );
    assert!(ids.iter().all(|id| *id >= 1001));

    // ⚠ And an unarchived archivable one joins the set, because a part may hold it.
    let partial: HashSet<i64> = (1..=999).collect();
    let ids2 = ea::floor_relevant_ids(&scan, &partial, &cands);
    assert_eq!(ids2.len(), 11, "the one unarchived archivable execution must be included");
    assert!(ids2.contains(&1000));
}

/// Every basis must give a distinct, actionable reason.
#[test]
fn every_floor_basis_reason_is_distinct_and_actionable() {
    let v = [
        ea::FloorBasis::Computed { keep_from_sequence: 7, constrained_by: 2 },
        ea::FloorBasis::AwaitingBacklog { unarchived: 5 },
        ea::FloorBasis::NoFootprint,
    ];
    let msgs: Vec<String> = v.iter().map(|b| b.reason()).collect();
    for (i, a) in msgs.iter().enumerate() {
        assert!(!a.is_empty());
        for b in msgs.iter().skip(i + 1) {
            assert_ne!(a, b);
        }
    }
    assert_eq!(v.iter().filter(|b| b.may_prune()).count(), 1, "exactly one state permits a prune");
}
