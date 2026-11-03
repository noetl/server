//! An archived execution that is touched again — noetl/ai-meta#506 P3.
//!
//! Before P3 an execution in `known_archived` was never looked at again. A late event
//! (a #504 straggler, a callback after terminal) reached `noetl.event` and a new engine
//! part while the execution still counted as satisfied for the prune floor, so once the
//! floor passed that part the late record was gone from the hot store with no copy in the
//! archive — and nothing reported it.
//!
//! Two properties carry the weight here, and each has a test that fails without it:
//! - **No action on a bloom "maybe".** The footprint over-estimates; only an exact read
//!   confirms.
//! - **No rewrite from an incomplete hot read.** If the execution's first records are
//!   already pruned, re-archiving from what is hot would replace the archive with the late
//!   records alone. It must stay pending, under the floor, instead.

use std::collections::HashSet;

use chrono::{Duration, TimeZone, Utc};
use noetl_server::services::archive_store::FakeArchiveStore;
use noetl_server::services::event_archive as ea;

fn t(h: i64) -> chrono::DateTime<chrono::Utc> {
    Utc.timestamp_opt(1_790_000_000, 0).unwrap() + Duration::hours(h)
}

fn rec(seq: u64) -> ea::ArchiveRecord {
    ea::ArchiveRecord {
        global_sequence: seq,
        event_id: Some(format!("ev-{seq}")),
        event_type: "step.completed".into(),
        body: serde_json::json!({ "seq": seq }),
    }
}

fn recs(seqs: &[u64]) -> Vec<ea::ArchiveRecord> {
    seqs.iter().copied().map(rec).collect()
}

fn cfg_on() -> ea::RetentionConfig {
    ea::RetentionConfig {
        archive_enabled: true,
        bucket: Some("b".into()),
        ..Default::default()
    }
}

fn cand(id: i64) -> ea::ArchiveCandidate {
    ea::ArchiveCandidate {
        execution_id: id,
        status: "COMPLETED".into(),
        completed_at: Some(t(0)),
        started_at: t(0),
    }
}

fn hot(id: i64, min: u64, max: u64) -> ea::HotExecution {
    ea::HotExecution {
        execution_id: id,
        min_sequence: min,
        max_sequence: max,
    }
}

fn state(mode: ea::RetouchMode, extents: &[(i64, u64, u64)]) -> ea::RetouchState {
    ea::RetouchState {
        mode,
        extents: extents.iter().map(|&(id, a, b)| (id, (a, b))).collect(),
        ..Default::default()
    }
}

fn known(ids: &[i64]) -> HashSet<i64> {
    ids.iter().copied().collect()
}

// --- detection ------------------------------------------------------------------

/// No part starting above the archived extent might hold it: certainly not re-touched, and
/// no exact read is spent. (Blooms have no false negatives.)
#[test]
fn no_part_above_the_archive_costs_no_exact_read() {
    let st = state(ea::RetouchMode::On, &[(1, 10, 20)]);
    let mut reads = 0;
    let scan = ea::detect_retouched(
        &st,
        &known(&[1]),
        |_| Some(vec![]),
        |_| {
            reads += 1;
            Some(Some((10, 20)))
        },
        32,
    );
    assert_eq!((scan.examined, scan.suspects, reads), (1, 0, 0));
    assert!(scan.confirmed.is_empty());
}

/// The suspect test is asked about the ARCHIVED max, per execution.
#[test]
fn suspects_are_asked_about_the_archived_max() {
    let st = state(ea::RetouchMode::On, &[(1, 10, 20), (2, 30, 40)]);
    let mut asked = Vec::new();
    ea::detect_retouched(
        &st,
        &known(&[1, 2]),
        |ext| {
            asked = ext.to_vec();
            Some(vec![])
        },
        |_| None,
        32,
    );
    asked.sort_unstable();
    assert_eq!(asked, vec![(1, 20), (2, 40)]);
}

/// ⭐ A bloom false positive on a newer part: a suspect, cleared by the exact read, and
/// remembered — so it does not consume a read every pass.
#[test]
fn a_bloom_false_positive_is_cleared_and_remembered() {
    let mut st = state(ea::RetouchMode::On, &[(1, 10, 20)]);
    let mut k = known(&[1]);
    let scan = ea::detect_retouched(
        &st,
        &k,
        |_| Some(vec![(1, 900)]),
        |_| Some(Some((10, 20))),
        32,
    );
    assert_eq!(scan.suspects, 1);
    assert!(
        scan.confirmed.is_empty(),
        "an exact read at the archived extent must clear it"
    );
    assert_eq!(scan.cleared, vec![(1, 900)]);
    ea::apply_retouch_scan(&scan, &mut st, &mut k);

    // Same newest part next pass: not a suspect, no read.
    let mut reads = 0;
    let again = ea::detect_retouched(
        &st,
        &k,
        |_| Some(vec![(1, 900)]),
        |_| {
            reads += 1;
            Some(Some((10, 20)))
        },
        32,
    );
    assert_eq!((again.suspects, reads), (0, 0));

    // A NEWER part may hold it: a suspect again.
    let newer = ea::detect_retouched(
        &st,
        &k,
        |_| Some(vec![(1, 950)]),
        |_| Some(Some((10, 20))),
        32,
    );
    assert_eq!(newer.suspects, 1);
}

#[test]
fn a_real_late_record_is_confirmed() {
    let st = state(ea::RetouchMode::On, &[(1, 10, 20), (2, 30, 40)]);
    let scan = ea::detect_retouched(
        &st,
        &known(&[1, 2]),
        |_| Some(vec![(1, 900)]),
        |id| Some(Some(if id == 1 { (10, 25) } else { (30, 40) })),
        32,
    );
    assert_eq!(scan.confirmed, vec![1]);
}

#[test]
fn exact_reads_are_bounded_per_pass() {
    let ids: Vec<i64> = (1..=10).collect();
    let extents: Vec<(i64, u64, u64)> = ids.iter().map(|&i| (i, 1, 2)).collect();
    let st = state(ea::RetouchMode::On, &extents);
    let mut reads = 0;
    let scan = ea::detect_retouched(
        &st,
        &known(&ids),
        |ext| Some(ext.iter().map(|&(i, _)| (i, 99)).collect()),
        |_| {
            reads += 1;
            Some(Some((1, 99)))
        },
        3,
    );
    assert_eq!((reads, scan.confirmed.len()), (3, 3));
    assert!(scan.bounded);
}

/// An engine that cannot be read concludes nothing.
#[test]
fn an_unreadable_engine_confirms_nothing() {
    let st = state(ea::RetouchMode::On, &[(1, 10, 20)]);
    let scan = ea::detect_retouched(&st, &known(&[1]), |_| None, |_| None, 32);
    assert!(scan.unreadable && scan.confirmed.is_empty());
}

// --- applying a scan --------------------------------------------------------------

/// `shadow` counts and logs, and changes nothing the floor sees.
#[test]
fn shadow_keeps_the_execution_archived() {
    let mut st = state(ea::RetouchMode::Shadow, &[(1, 10, 20)]);
    let mut k = known(&[1]);
    let scan = ea::RetouchScan {
        confirmed: vec![1],
        ..Default::default()
    };
    ea::apply_retouch_scan(&scan, &mut st, &mut k);
    assert!(k.contains(&1) && st.pending.is_empty());
}

/// `on` takes it out of the satisfied set, so the floor is pinned by it again.
#[test]
fn on_puts_a_re_touched_execution_back_under_the_floor() {
    let mut st = state(ea::RetouchMode::On, &[(1, 10, 20)]);
    let mut k = known(&[1]);
    let scan = ea::RetouchScan {
        confirmed: vec![1],
        ..Default::default()
    };
    ea::apply_retouch_scan(&scan, &mut st, &mut k);
    assert!(!k.contains(&1) && st.pending.contains(&1));

    let safe: Vec<i64> = k.iter().copied().collect();
    let floor = ea::retention_floor(&[hot(1, 10, 25), hot(2, 50, 60)], &safe);
    assert_eq!(
        floor.keep_from_sequence,
        Some(10),
        "a re-touched execution must constrain the floor until its archive is complete"
    );
}

// --- re-archive inside archive_pass ----------------------------------------------

/// ⭐ The late record is still hot along with the first ones: re-archive, complete.
#[tokio::test]
async fn a_re_touched_execution_is_archived_again_with_the_late_record() {
    let store = FakeArchiveStore::new();
    let cands = vec![cand(7)];
    let mut k = HashSet::new();
    let mut ooc = HashSet::new();
    let mut st = ea::RetouchState {
        mode: ea::RetouchMode::On,
        ..Default::default()
    };

    let first = ea::archive_pass(
        &store,
        &cfg_on(),
        &cands,
        t(100),
        &mut k,
        &mut ooc,
        &mut st,
        |_| Some(recs(&[1, 2, 3])),
    )
    .await;
    assert_eq!(first.verified, 1);
    assert_eq!(
        st.extents.get(&7),
        Some(&(1, 3)),
        "the archived extent is recorded"
    );

    // A late record arrives at sequence 10.
    let scan = ea::detect_retouched(
        &st,
        &k,
        |_| Some(vec![(7, 10)]),
        |_| Some(Some((1, 10))),
        32,
    );
    ea::apply_retouch_scan(&scan, &mut st, &mut k);
    assert!(st.pending.contains(&7));

    let second = ea::archive_pass(
        &store,
        &cfg_on(),
        &cands,
        t(100),
        &mut k,
        &mut ooc,
        &mut st,
        |_| Some(recs(&[1, 2, 3, 10])),
    )
    .await;
    assert_eq!(second.re_archived, 1, "{}", second.describe());
    assert!(k.contains(&7) && st.pending.is_empty());
    assert_eq!(st.extents.get(&7), Some(&(1, 10)));

    match ea::archive_state(&store, 7).await.unwrap() {
        ea::ArchiveState::SafeToPrune {
            record_count,
            min_sequence,
            max_sequence,
        } => assert_eq!((record_count, min_sequence, max_sequence), (4, 1, 10)),
        other => panic!("not safe to prune after re-archive: {}", other.reason()),
    }
    // Additive: the first data object is still there.
    assert!(
        store.keys().contains(&ea::data_key(7, 1, 3)),
        "the original data object must not be deleted"
    );
}

/// ⭐⭐ The execution's first records are already pruned from the hot store. Re-archiving
/// from what is hot would point the manifest at the late record alone — the cold copy
/// would lose the execution's history. It must stay pending and keep the archive as it was.
#[tokio::test]
async fn an_incomplete_hot_read_never_rewrites_the_archive() {
    let store = FakeArchiveStore::new();
    let cands = vec![cand(8)];
    let mut k = HashSet::new();
    let mut ooc = HashSet::new();
    let mut st = ea::RetouchState {
        mode: ea::RetouchMode::On,
        ..Default::default()
    };
    ea::archive_pass(
        &store,
        &cfg_on(),
        &cands,
        t(100),
        &mut k,
        &mut ooc,
        &mut st,
        |_| Some(recs(&[1, 2, 3])),
    )
    .await;

    let scan = ea::RetouchScan {
        confirmed: vec![8],
        ..Default::default()
    };
    ea::apply_retouch_scan(&scan, &mut st, &mut k);

    // Hot now holds only the late record: parts with 1..3 were dropped.
    let rep = ea::archive_pass(
        &store,
        &cfg_on(),
        &cands,
        t(100),
        &mut k,
        &mut ooc,
        &mut st,
        |_| Some(recs(&[10])),
    )
    .await;
    assert_eq!(rep.re_touch_incomplete, 1, "{}", rep.describe());
    assert_eq!(rep.re_archived, 0);
    assert!(
        st.pending.contains(&8),
        "it must stay pending, under the floor"
    );
    assert!(!k.contains(&8));
    match ea::archive_state(&store, 8).await.unwrap() {
        ea::ArchiveState::SafeToPrune {
            record_count,
            max_sequence,
            ..
        } => assert_eq!(
            (record_count, max_sequence),
            (3, 3),
            "the archive must be untouched — a rewrite here drops records 1..3 from it"
        ),
        other => panic!("{}", other.reason()),
    }
}

/// `off` (the default) records nothing, so the pass is exactly what it was before P3.
#[tokio::test]
async fn off_records_no_extents() {
    let store = FakeArchiveStore::new();
    let mut st = ea::RetouchState::default();
    ea::archive_pass(
        &store,
        &cfg_on(),
        &[cand(9)],
        t(100),
        &mut HashSet::new(),
        &mut HashSet::new(),
        &mut st,
        |_| Some(recs(&[1, 2])),
    )
    .await;
    assert!(st.extents.is_empty() && st.pending.is_empty());
}

#[test]
fn the_mode_parses_conservatively() {
    assert_eq!(ea::RetouchMode::parse(""), ea::RetouchMode::Off);
    assert_eq!(ea::RetouchMode::parse("garbage"), ea::RetouchMode::Off);
    assert_eq!(ea::RetouchMode::parse(" Shadow "), ea::RetouchMode::Shadow);
    assert_eq!(ea::RetouchMode::parse("on"), ea::RetouchMode::On);
}
