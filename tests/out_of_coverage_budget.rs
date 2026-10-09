//! Out-of-coverage executions must not consume the pass budget — [noetl/ai-meta#459](https://github.com/noetl/ai-meta/issues/459).
//!
//! ⚠⚠⚠ Found on prod, by a log line, after the published gauges had read healthy for hours:
//!
//! ```text
//! archive pass complete  examined=6511 archivable=6259
//!                        archived=0  already=3680  out_of_coverage=100  failed=0
//! ```
//!
//! `out_of_coverage=100` is exactly `max_per_pass`. The budget is decremented BEFORE the
//! empty-records check, so each out-of-coverage execution cost a unit — and nothing
//! remembered the answer, so every pass spent its entire budget re-deciding the same
//! unarchivable executions. `archived` sat at **0**, and the ~2,479 genuinely archivable
//! executions further down the list were never reached. The drain had stopped dead.
//!
//! It is defect server#514's shape reached by a different route: **work repeated because its
//! result was not retained.** #514 re-took the same first 100 candidates; this re-decided the
//! same first 100 unarchivable ones.
//!
//! And it blocked more than the drain. An out-of-coverage execution is `archivable` by age
//! and is never `verified_archived`, so `compute_prune_floor` counted it as backlog and
//! returned `AwaitingBacklog` **permanently** — prune could never have run at all.

use std::collections::HashSet;

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
            event_id: Some(format!("e{}", base + i as u64)),
            event_type: "command.completed".into(),
            body: serde_json::json!({"i": i}),
        })
        .collect()
}

fn cand(id: i64) -> ea::ArchiveCandidate {
    ea::ArchiveCandidate {
        execution_id: id,
        status: "COMPLETED".into(),
        completed_at: Some(t(0)),
        started_at: t(0),
    }
}

fn cfg(max_per_pass: usize) -> ea::RetentionConfig {
    ea::RetentionConfig {
        retention_hours: 48,
        archive_enabled: true,
        prune_enabled: false,
        bucket: Some("b".into()),
        interval_secs: 300,
        max_per_pass,
    }
}

/// ⭐⭐ The end-to-end property: the drain gets PAST a block of out-of-coverage executions.
///
/// Fixture mirrors prod: the first N candidates are out of coverage (the engine has no
/// records for them), the rest are archivable. With `max_per_pass == N`, the first pass can
/// do nothing but discover the dead ones — the second pass must then make real progress.
#[tokio::test]
async fn the_drain_progresses_past_a_block_of_out_of_coverage_executions() {
    let store = FakeArchiveStore::default();
    let dead: Vec<i64> = (1..=5).collect();
    let live: Vec<i64> = (100..=110).collect();
    let cands: Vec<_> = dead.iter().chain(live.iter()).map(|i| cand(*i)).collect();

    let mut archived_set: HashSet<i64> = HashSet::new();
    let mut ooc: HashSet<i64> = HashSet::new();
    let read = |id: i64| {
        if (1..=5).contains(&id) {
            Some(Vec::new()) // engine open, holds nothing: OUT OF COVERAGE
        } else {
            Some(recs(4, 1_000 + id as u64 * 10))
        }
    };

    // Budget exactly equals the dead block, which is the prod shape.
    let p1 = ea::archive_pass(&store, &cfg(5), &cands, t(100), &mut archived_set, &mut ooc, read).await;
    assert_eq!(p1.out_of_coverage, 5, "{}", p1.describe());
    assert_eq!(p1.archived, 0, "pass 1 spends its whole budget discovering the dead ones");

    // ⭐ Pass 2 must not repeat that work.
    let p2 = ea::archive_pass(&store, &cfg(5), &cands, t(100), &mut archived_set, &mut ooc, read).await;
    assert_eq!(
        p2.archived, 5,
        "⚠⚠ THE DRAIN IS BLOCKED: pass 2 archived {} instead of 5. The out-of-coverage \
         executions were re-decided and consumed the budget again, which is exactly the prod \
         failure — archived=0 forever while every gauge reads healthy. {}",
        p2.archived,
        p2.describe()
    );
    assert_eq!(p2.out_of_coverage, 5, "they are still counted, just for free");

    // And it keeps going.
    let p3 = ea::archive_pass(&store, &cfg(5), &cands, t(100), &mut archived_set, &mut ooc, read).await;
    assert_eq!(p3.archived, 5, "{}", p3.describe());
    let p4 = ea::archive_pass(&store, &cfg(5), &cands, t(100), &mut archived_set, &mut ooc, read).await;
    assert_eq!(p4.archived, 1, "the last live one; {}", p4.describe());
    assert_eq!(archived_set.len(), 11, "every live execution archived");
}

/// The budget accounting itself: a remembered out-of-coverage id costs nothing.
#[tokio::test]
async fn a_remembered_out_of_coverage_id_costs_no_budget() {
    let store = FakeArchiveStore::default();
    let cands: Vec<_> = (1..=3).map(cand).chain(std::iter::once(cand(99))).collect();
    let mut archived_set: HashSet<i64> = HashSet::new();
    let mut ooc: HashSet<i64> = HashSet::new();
    let read = |id: i64| if id < 10 { Some(Vec::new()) } else { Some(recs(2, 500)) };

    // Budget 3: pass 1 burns it on the three dead ones and never reaches 99.
    let p1 = ea::archive_pass(&store, &cfg(3), &cands, t(100), &mut archived_set, &mut ooc, read).await;
    assert_eq!(p1.out_of_coverage, 3);
    assert_eq!(p1.archived, 0, "{}", p1.describe());

    // Pass 2, same budget of 3 — the three are free now, so 99 is reached.
    let p2 = ea::archive_pass(&store, &cfg(3), &cands, t(100), &mut archived_set, &mut ooc, read).await;
    assert_eq!(p2.archived, 1, "{}", p2.describe());
    assert!(archived_set.contains(&99));
}

/// ⚠⚠ The floor half. Out-of-coverage executions must not count as backlog, or the floor
/// returns `AwaitingBacklog` forever and prune can never run.
#[test]
fn the_floor_is_not_blocked_by_out_of_coverage_executions() {
    let cands: Vec<_> = (1..=4).map(cand).collect();
    let mut scan = ea::ArchivableScan::default();
    let rows: Vec<(i64, String, Option<chrono::DateTime<chrono::Utc>>)> = cands
        .iter()
        .map(|c| (c.execution_id, c.status.clone(), c.completed_at))
        .collect();
    ea::classify_into(&mut scan, &rows, t(100), chrono::Duration::hours(48));
    assert_eq!(scan.archivable.len(), 4);

    // 1 and 2 genuinely archived; 3 and 4 are out of coverage and never will be.
    let archived: HashSet<i64> = [1, 2].into_iter().collect();
    let ooc: HashSet<i64> = [3, 4].into_iter().collect();
    let footprint = vec![
        ea::HotExecution { execution_id: 1, min_sequence: 10, max_sequence: 20 },
        ea::HotExecution { execution_id: 2, min_sequence: 21, max_sequence: 30 },
    ];

    // Archived alone: blocked, which is the bug.
    let blocked = ea::compute_prune_floor(&scan, &archived, &footprint);
    assert!(
        !blocked.may_prune(),
        "sanity: with the out-of-coverage ones counted as backlog the floor must refuse"
    );

    // Archived UNION out-of-coverage: the floor computes.
    let satisfied: HashSet<i64> = archived.union(&ooc).copied().collect();
    let ok = ea::compute_prune_floor(&scan, &satisfied, &footprint);
    assert!(
        ok.may_prune(),
        "⚠⚠ the floor is STILL blocked once out-of-coverage executions are treated as \
         satisfied — prune can then never run on prod: {}",
        ok.reason()
    );
    assert_eq!(
        ok.keep_from_sequence(),
        Some(31),
        "nothing constrains, so the floor clears the measured tip: {}",
        ok.reason()
    );
}

/// ⚠⚠⚠ The reachability guard, because the test above has a gap.
///
/// `the_floor_is_not_blocked_by_out_of_coverage_executions` builds the union **in the test**
/// and asserts `compute_prune_floor` handles it. That proves the floor function is capable —
/// it proves nothing about whether production actually passes it a unioned set. A pass that
/// hands `prune_pass` only `known_archived` would satisfy every assertion in this file and
/// still leave prune permanently blocked on prod.
///
/// So this asserts the production source performs the union, with `#[cfg(test)]` blocks
/// stripped so a test or doc comment mentioning it cannot satisfy the check, and the stripped
/// slice asserted plausible before anything is asserted about it.
#[test]
fn production_unions_out_of_coverage_into_the_floors_satisfied_set() {
    const SRC: &str = include_str!("../src/services/event_archive.rs");

    let mut prod = String::with_capacity(SRC.len());
    let mut rest = SRC;
    while let Some(at) = rest.find("#[cfg(test)]") {
        prod.push_str(&rest[..at]);
        let after = &rest[at..];
        let Some(open) = after.find('{') else { break };
        let mut depth = 0usize;
        let mut end = None;
        for (i, c) in after[open..].char_indices() {
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(open + i + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        match end {
            Some(e) => rest = &after[e..],
            None => break,
        }
    }
    prod.push_str(rest);
    assert!(
        prod.len() > SRC.len() / 2 && prod.contains("pub fn spawn_archive_pass"),
        "production slice implausible ({} of {} bytes) — the assertions below would be vacuous",
        prod.len(),
        SRC.len()
    );

    assert!(
        prod.contains("known_out_of_coverage"),
        "the pass does not track out-of-coverage executions at all"
    );
    assert!(
        prod.contains(".union(&known_out_of_coverage)"),
        "⚠⚠ the production pass does NOT union out-of-coverage into the floor's satisfied \
         set. The floor will count those executions as backlog and return AwaitingBacklog \
         forever, so prune can never run — and every other test in this file still passes."
    );
}
