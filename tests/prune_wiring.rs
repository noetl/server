//! The floor wired to the retention call — [noetl/ai-meta#459](https://github.com/noetl/ai-meta/issues/459).
//!
//! ⚠⚠⚠ `compute_prune_floor` existed with **zero production callers** — a correct primitive
//! that deleted nothing because nothing reached it. These tests cover the wiring, and the
//! property they protect is negative: no part that could hold a live execution's events may
//! be dropped. `noetl.event` is append-only with replay as the source of truth, so a wrong
//! floor is data loss, not eviction.
//!
//! The prune callback here models a real part layout and the engine's actual droppability
//! rule (`part.max_sequence < keep_from_sequence`), so the assertions are about **which
//! records survive**, not about whether the call returned `Ok`. A floor bug that reclaims
//! nothing and a floor bug that deletes live data both return `Ok`.

use std::cell::RefCell;
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

fn set(c: Vec<ea::ArchiveCandidate>) -> ea::CandidateSet {
    ea::CandidateSet { candidates: c, complete: true }
}

fn cfg(prune: bool) -> ea::RetentionConfig {
    ea::RetentionConfig {
        retention_hours: 48,
        archive_enabled: true,
        prune_enabled: prune,
        bucket: Some("b".into()),
        interval_secs: 300,
        max_per_pass: 100,
    }
}

/// A part, as the manifest carries it: a sequence range and the executions whose records
/// actually live in it.
#[derive(Clone, Debug)]
struct Part {
    min: u64,
    max: u64,
    bytes: u64,
    holds: Vec<i64>,
}

/// The engine's real droppability rule, and nothing else.
struct FakeEngine {
    parts: RefCell<Vec<Part>>,
}

impl FakeEngine {
    fn new(parts: Vec<Part>) -> Self {
        Self { parts: RefCell::new(parts) }
    }
    /// The footprint the server derives from the manifest: min/max over the parts that may
    /// hold each execution.
    fn footprint(&self, ids: &[i64]) -> Option<Vec<ea::HotExecution>> {
        let parts = self.parts.borrow();
        let mut out = Vec::new();
        for id in ids {
            let mut lo: Option<u64> = None;
            let mut hi: u64 = 0;
            for p in parts.iter() {
                if p.holds.contains(id) {
                    lo = Some(lo.map_or(p.min, |l: u64| l.min(p.min)));
                    hi = hi.max(p.max);
                }
            }
            if let Some(lo) = lo {
                out.push(ea::HotExecution {
                    execution_id: *id,
                    min_sequence: lo,
                    max_sequence: hi,
                });
            }
        }
        Some(out)
    }
    fn prune(&self, keep_from: u64) -> Option<Result<(usize, u64), String>> {
        let mut parts = self.parts.borrow_mut();
        let before = parts.len();
        let bytes: u64 = parts.iter().filter(|p| p.max < keep_from).map(|p| p.bytes).sum();
        parts.retain(|p| p.max >= keep_from);
        Some(Ok((before - parts.len(), bytes)))
    }
    fn surviving_executions(&self) -> HashSet<i64> {
        self.parts.borrow().iter().flat_map(|p| p.holds.clone()).collect()
    }
}

/// The layout the property is tested against.
///
/// Live execution 9 sits at sequence 30 — in the MIDDLE of the log, interleaved with
/// archived executions above and below it. That is the shape that matters: a floor derived
/// from "everything archivable is archived" must still be pinned below 30 by execution 9, or
/// the part holding it is dropped.
fn layout() -> Vec<Part> {
    vec![
        Part { min: 1, max: 10, bytes: 1_000, holds: vec![1, 2] },
        Part { min: 11, max: 20, bytes: 2_000, holds: vec![3, 4] },
        Part { min: 21, max: 40, bytes: 4_000, holds: vec![5, 9] },
        Part { min: 41, max: 60, bytes: 8_000, holds: vec![6, 7] },
    ]
}

#[test]
fn a_backlog_refuses_to_prune_anything() {
    let eng = FakeEngine::new(layout());
    // 1..=7 completed long ago (archivable); only 1 and 2 are verified-archived.
    let cands: Vec<_> = (1..=7).map(|i| cand(i, "COMPLETED", Some(-100))).collect();
    let archived: HashSet<i64> = [1, 2].into_iter().collect();

    let rep = ea::prune_pass(
        &cfg(true),
        &set(cands.clone()),
        t(0),
        &archived,
        |ids| eng.footprint(ids),
        |f| eng.prune(f),
    );

    assert_eq!(
        rep.parts_dropped, 0,
        "a backlog must block pruning entirely: {}",
        rep.describe()
    );
    assert_eq!(rep.bytes_reclaimed, 0);
    assert!(
        rep.basis.as_deref().unwrap_or("").contains("refusing"),
        "the refusal must say why, got {:?}",
        rep.basis
    );
    assert_eq!(eng.parts.borrow().len(), 4, "no part may be dropped");
}

#[test]
fn the_floor_never_crosses_a_live_execution() {
    let eng = FakeEngine::new(layout());
    // 9 is RUNNING — not archivable, so it must stay hot. Everything else is archivable and
    // fully archived, which is precisely the state that unblocks the floor.
    let mut cands: Vec<_> = (1..=7).map(|i| cand(i, "COMPLETED", Some(-100))).collect();
    cands.push(cand(9, "RUNNING", None));
    let archived: HashSet<i64> = (1..=7).collect();

    let rep = ea::prune_pass(
        &cfg(true),
        &set(cands.clone()),
        t(0),
        &archived,
        |ids| eng.footprint(ids),
        |f| eng.prune(f),
    );

    let floor = rep.keep_from_sequence.expect("the floor must compute");
    assert!(
        floor <= 21,
        "the floor {floor} rose into the part holding live execution 9 (min_sequence 21) — \
         pruning below it deletes events that were never archived"
    );
    // The property, stated as surviving records rather than as a return code.
    assert!(
        eng.surviving_executions().contains(&9),
        "live execution 9 was pruned away. floor={floor} report={}",
        rep.describe()
    );
    // And it must still make progress: the two parts entirely below 9 are reclaimable.
    assert_eq!(rep.parts_dropped, 2, "expected the two parts below 21: {}", rep.describe());
    assert_eq!(
        rep.bytes_reclaimed, 3_000,
        "reclaimed bytes must be summed from the parts actually dropped, not estimated"
    );
}

#[test]
fn an_absent_footprint_refuses_rather_than_pruning_everything() {
    let eng = FakeEngine::new(layout());
    let cands: Vec<_> = (1..=7).map(|i| cand(i, "COMPLETED", Some(-100))).collect();
    let archived: HashSet<i64> = (1..=7).collect();

    let rep = ea::prune_pass(
        &cfg(true),
        &set(cands.clone()),
        t(0),
        &archived,
        |_| None, // the engine is not open
        |f| eng.prune(f),
    );

    assert_eq!(rep.parts_dropped, 0);
    assert!(
        rep.skipped.as_deref().unwrap_or("").contains("not open"),
        "an absent footprint must be refused as unknown, never read as \
         'nothing constrains': {:?}",
        rep.skipped
    );
    assert_eq!(eng.parts.borrow().len(), 4);
}

#[test]
fn the_flag_off_prunes_nothing() {
    let eng = FakeEngine::new(layout());
    let cands: Vec<_> = (1..=7).map(|i| cand(i, "COMPLETED", Some(-100))).collect();
    let archived: HashSet<i64> = (1..=7).collect();

    let rep = ea::prune_pass(
        &cfg(false),
        &set(cands.clone()),
        t(0),
        &archived,
        |ids| eng.footprint(ids),
        |f| eng.prune(f),
    );
    assert_eq!(rep.parts_dropped, 0);
    assert!(rep.skipped.is_some(), "the flag being off must be reported, not silent");
    assert_eq!(eng.parts.borrow().len(), 4);
}

#[test]
fn an_implausible_constraining_set_refuses_rather_than_truncating() {
    let eng = FakeEngine::new(layout());
    // Every candidate is RUNNING, so every one constrains.
    let n = ea::MAX_FOOTPRINT_IDS + 1;
    let cands: Vec<_> = (1..=n as i64).map(|i| cand(i, "RUNNING", None)).collect();
    let archived: HashSet<i64> = HashSet::new();

    let rep = ea::prune_pass(
        &cfg(true),
        &set(cands.clone()),
        t(0),
        &archived,
        |ids| eng.footprint(ids),
        |f| eng.prune(f),
    );
    assert_eq!(rep.parts_dropped, 0);
    assert!(
        rep.skipped.as_deref().unwrap_or("").contains("MAX_FOOTPRINT_IDS"),
        "an oversized footprint must refuse: truncating it would place the floor above \
         live data. got {:?}",
        rep.skipped
    );
}

/// The positive control. Without this, every assertion above is satisfied by a `prune_pass`
/// that never prunes under any circumstances.
#[test]
fn a_fully_archived_log_does_reclaim() {
    let eng = FakeEngine::new(layout());
    let cands: Vec<_> = [1, 2, 3, 4, 5, 6, 7, 9]
        .into_iter()
        .map(|i| cand(i, "COMPLETED", Some(-100)))
        .collect();
    let archived: HashSet<i64> = [1, 2, 3, 4, 5, 6, 7, 9].into_iter().collect();

    let rep = ea::prune_pass(
        &cfg(true),
        &set(cands.clone()),
        t(0),
        &archived,
        |ids| eng.footprint(ids),
        |f| eng.prune(f),
    );
    assert!(
        rep.parts_dropped > 0 && rep.bytes_reclaimed > 0,
        "with nothing constraining, retention must actually reclaim — otherwise the \
         negative tests above prove only that nothing ever prunes: {}",
        rep.describe()
    );
}

/// ⚠⚠ The completeness gate. A partial candidate listing makes the floor unsound rather
/// than merely conservative: an execution the listing never returned is invisible to the
/// floor, so its records can lie below it and be deleted. The paging cap is silent, so this
/// is the one signal standing between a large population and data loss.
#[test]
fn an_incomplete_candidate_listing_refuses_to_prune() {
    let eng = FakeEngine::new(layout());
    let cands: Vec<_> = (1..=7).map(|i| cand(i, "COMPLETED", Some(-100))).collect();
    let archived: std::collections::HashSet<i64> = (1..=7).collect();
    let incomplete = ea::CandidateSet { candidates: cands, complete: false };

    let rep = ea::prune_pass(
        &cfg(true),
        &incomplete,
        t(0),
        &archived,
        |ids| eng.footprint(ids),
        |f| eng.prune(f),
    );
    assert_eq!(rep.parts_dropped, 0);
    assert!(
        rep.skipped.as_deref().unwrap_or("").contains("incomplete"),
        "an incomplete listing must refuse, got {:?}",
        rep.skipped
    );
    assert_eq!(eng.parts.borrow().len(), 4, "no part may be dropped");
}

/// The all-archived case, stated as reclaimed bytes.
///
/// ⚠ This is the steady state prod reaches once the 6,230-execution backlog drains on an
/// idle log, and the first implementation returned `NoFootprint` here — so the whole prune
/// path would have reclaimed zero while every call returned `Ok`.
#[test]
fn nothing_constraining_reclaims_up_to_the_measured_tip() {
    let eng = FakeEngine::new(layout());
    let ids = [1, 2, 3, 4, 5, 6, 7, 9];
    let cands: Vec<_> = ids.iter().map(|i| cand(*i, "COMPLETED", Some(-100))).collect();
    let archived: std::collections::HashSet<i64> = ids.into_iter().collect();

    let rep = ea::prune_pass(
        &cfg(true),
        &set(cands),
        t(0),
        &archived,
        |i| eng.footprint(i),
        |f| eng.prune(f),
    );
    assert_eq!(
        rep.keep_from_sequence,
        Some(61),
        "the floor must clear the highest sequence the measured executions occupy (60), not \
         their highest FIRST sequence — the off-by-a-whole-log error `max_sequence` exists \
         for: {}",
        rep.describe()
    );
    assert_eq!(rep.parts_dropped, 4);
    assert_eq!(
        rep.bytes_reclaimed, 15_000,
        "reclaim must be summed from the parts actually dropped: {}",
        rep.describe()
    );
}
