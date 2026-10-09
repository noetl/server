//! P4 of [noetl/ai-meta#459](https://github.com/noetl/ai-meta/issues/459) — the hot copy is
//! pruned ONLY after the archive is verified durable and indexed.
//!
//! The load-bearing property is a **negative** one: a still-running execution's early
//! events must survive a prune pass. So the data-loss case is the test, not a footnote.
//!
//! Driven against a REAL `L0Engine` on a real `LocalFsSubstrate` — real parts, real
//! `apply_retention` — because the whole point of this phase is that `apply_retention`
//! had zero callers in the server and was built-and-dormant.

use chrono::{Duration, TimeZone, Utc};
use ehdb_l0::dataset::{D1EventLog, EventRecord};
use ehdb_l0::engine::{L0Config, L0Engine};
use ehdb_l0::substrate::LocalFsSubstrate;
use noetl_server::services::archive_store::FakeArchiveStore;
use noetl_server::services::event_archive as ea;
use std::sync::Arc;

fn dir(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!(
        "p4-{tag}-{}-{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn ts(h: i64) -> chrono::DateTime<chrono::Utc> {
    Utc.timestamp_opt(1_790_000_000, 0).unwrap() + Duration::hours(h)
}

fn arec(seq: u64, id: i64) -> ea::ArchiveRecord {
    ea::ArchiveRecord {
        global_sequence: seq,
        event_id: id,
        event_type: "step.completed".into(),
        body: serde_json::json!({"seq": seq}),
    }
}

fn meta(id: i64) -> ea::ExecutionMeta {
    ea::ExecutionMeta {
        execution_id: id,
        status: "COMPLETED".into(),
        started_at: ts(0),
        completed_at: Some(ts(1)),
    }
}

// ---------------------------------------------------------------------------
// The floor: the bridge between per-execution archival and per-part pruning.
// ---------------------------------------------------------------------------

/// ⚠⚠⚠ THE DATA-LOSS CASE, proven impossible. A still-running execution whose earliest
/// events sit in the OLDEST part must pin the floor below that part, so the part is not
/// droppable — even though every other execution in it is archived.
#[test]
fn a_live_execution_pins_the_floor_below_its_earliest_events() {
    let hot = vec![
        ea::HotExecution { execution_id: 1, min_sequence: 100, max_sequence: 100+1000 }, // archived
        ea::HotExecution { execution_id: 2, min_sequence: 150, max_sequence: 150+1000 }, // archived
        // ⚠ Started early, STILL RUNNING: its first events are in the oldest part.
        ea::HotExecution { execution_id: 3, min_sequence: 120, max_sequence: 120+1000 },
        ea::HotExecution { execution_id: 4, min_sequence: 900, max_sequence: 900+1000 }, // archived
    ];
    let d = ea::retention_floor(&hot, &[1, 2, 4]);
    assert_eq!(
        d.keep_from_sequence,
        Some(120),
        "the floor must sit at the LIVE execution's earliest sequence"
    );
    assert_eq!(d.blocked_by.as_ref().unwrap().execution_id, 3);
    assert_eq!(d.hot_examined, 4);
    assert_eq!(d.safe_count, 3);

    // The part holding sequences 100..=200 contains execution 3's early events.
    assert!(
        !d.part_is_droppable(100, 200),
        "⚠⚠ a part containing a LIVE execution's events must never be droppable — this is \
         the data-loss case"
    );
    // A part strictly below the floor holds only archived executions.
    assert!(d.part_is_droppable(100, 119));
    // A part straddling the floor is kept WHOLE — drop-partition never splits a part.
    assert!(!d.part_is_droppable(119, 121));

    // Positive control: with execution 3 archived too, the same part becomes droppable —
    // so the refusal above is the live execution and not a function that always says no.
    let d2 = ea::retention_floor(&hot, &[1, 2, 3, 4]);
    assert!(d2.blocked_by.is_none());
    assert!(
        d2.part_is_droppable(100, 200),
        "once everything is archived the part must be droppable"
    );
}

/// With nothing archived, the answer is "prune nothing" — not "prune everything".
#[test]
fn nothing_archived_means_prune_nothing() {
    let hot = vec![
        ea::HotExecution { execution_id: 1, min_sequence: 10, max_sequence: 10+1000 },
        ea::HotExecution { execution_id: 2, min_sequence: 20, max_sequence: 20+1000 },
    ];
    let d = ea::retention_floor(&hot, &[]);
    assert_eq!(d.keep_from_sequence, Some(10));
    assert!(!d.part_is_droppable(1, 9999));
    assert!(!d.part_is_droppable(10, 10));
    assert_eq!(d.blocked_by.unwrap().execution_id, 1);
}

/// ⚠ An empty hot set must not be read as "everything is prunable".
#[test]
fn an_empty_hot_set_drops_nothing() {
    let d = ea::retention_floor(&[], &[]);
    assert!(!d.part_is_droppable(0, u64::MAX / 2));
    assert_eq!(d.hot_examined, 0);
}

/// ⭐ The pinning execution is surfaced, because one stuck execution blocks ALL reclamation
/// while every other counter reads healthy.
#[test]
fn the_pinning_execution_is_named_in_the_decision() {
    let hot = vec![
        ea::HotExecution { execution_id: 77, min_sequence: 5, max_sequence: 5+1000 },
        ea::HotExecution { execution_id: 88, min_sequence: 50_000, max_sequence: 60_000 },
    ];
    let d = ea::retention_floor(&hot, &[88]);
    let s = d.describe();
    assert!(s.contains("pinned by execution 77"), "{s}");
    assert!(s.contains("sequence 5"), "{s}");
    assert!(s.contains("1 of 2"), "the denominator must be in the message: {s}");
}

// ---------------------------------------------------------------------------
// The gate: an archive must be durable AND indexed before a prune.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_indexed_durable_archive_is_safe_to_prune() {
    let store = FakeArchiveStore::new();
    let recs: Vec<_> = (0..10).map(|i| arec(100 + i, 1 + i as i64)).collect();
    ea::archive_execution_indexed(&store, &meta(5), &recs, Utc::now()).await.unwrap();
    match ea::archive_state(&store, 5).await.unwrap() {
        ea::ArchiveState::SafeToPrune { record_count } => assert_eq!(record_count, 10),
        other => panic!("expected SafeToPrune, got {other:?}"),
    }
}

/// ⚠⚠ RED: a corrupt archive must NOT be safe to prune. The byte flip keeps the length, so
/// only the digest can catch it.
#[tokio::test]
async fn a_corrupt_archive_is_never_safe_to_prune() {
    let store = FakeArchiveStore::new();
    let recs: Vec<_> = (0..8).map(|i| arec(1 + i, 1 + i as i64)).collect();
    let (m, _) = ea::archive_execution_indexed(&store, &meta(6), &recs, Utc::now()).await.unwrap();

    // Positive control first.
    assert!(ea::archive_state(&store, 6).await.unwrap().is_safe_to_prune());

    store.corrupt(&m.data_key);
    let st = ea::archive_state(&store, 6).await.unwrap();
    assert!(!st.is_safe_to_prune(), "corrupt archive must block the prune");
    assert!(matches!(
        st,
        ea::ArchiveState::NotDurable(ea::Durability::DigestMismatch { .. })
    ), "got {st:?}");
}

/// ⚠⚠ RED: a vanished data object must NOT be safe to prune.
#[tokio::test]
async fn a_vanished_data_object_is_never_safe_to_prune() {
    let store = FakeArchiveStore::new();
    let recs: Vec<_> = (0..4).map(|i| arec(1 + i, 1 + i as i64)).collect();
    let (m, _) = ea::archive_execution_indexed(&store, &meta(7), &recs, Utc::now()).await.unwrap();
    store.vanish(&m.data_key);
    let st = ea::archive_state(&store, 7).await.unwrap();
    assert!(!st.is_safe_to_prune());
    assert!(matches!(st, ea::ArchiveState::NotDurable(ea::Durability::DataMissing { .. })));
}

/// ⚠⚠ RED: durable but NOT indexed must block the prune. Pruning behind it would leave
/// data that exists and cannot be found by date.
#[tokio::test]
async fn a_durable_but_unindexed_archive_blocks_the_prune() {
    let store = FakeArchiveStore::new();
    let recs: Vec<_> = (0..6).map(|i| arec(1 + i, 1 + i as i64)).collect();
    // Archive WITHOUT the date reference.
    ea::archive_execution(&store, &meta(8), &recs, Utc::now()).await.unwrap();
    assert!(
        ea::verify_archived(&store, 8).await.unwrap().is_durable(),
        "the data is durable — only the index is missing"
    );
    let st = ea::archive_state(&store, 8).await.unwrap();
    assert!(!st.is_safe_to_prune(), "unindexed must block: {}", st.reason());
    assert!(matches!(st, ea::ArchiveState::DurableButUnindexed { .. }));
    assert!(st.reason().contains("unfindable by date"), "{}", st.reason());

    // And writing the reference makes it safe — the control that this is the index and not
    // something else.
    let m: ea::ArchiveManifest = serde_json::from_slice(
        &noetl_server::services::archive_store::ArchiveStore::get(&store, &ea::manifest_key(8))
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    ea::write_date_reference(&store, &m).await.unwrap();
    assert!(ea::archive_state(&store, 8).await.unwrap().is_safe_to_prune());
}

#[tokio::test]
async fn an_unarchived_execution_is_not_safe_to_prune() {
    let store = FakeArchiveStore::new();
    assert_eq!(
        ea::archive_state(&store, 999).await.unwrap(),
        ea::ArchiveState::NotArchived
    );
}

// ---------------------------------------------------------------------------
// End to end against a REAL L0 engine: archive, verify, prune, measure.
// ---------------------------------------------------------------------------

fn erec(seq: u64, execution: &str) -> EventRecord {
    EventRecord::new(seq, execution, format!("tx-{seq}"), format!("{{\"seq\":{seq}}}"))
}

/// ⭐⭐ The whole phase, end to end on a real engine: archive the expired executions, verify
/// durability, compute the floor, call the real `apply_retention`, and MEASURE what came
/// back — while a still-running execution's early events survive.
#[tokio::test]
async fn archived_executions_are_reclaimed_while_a_live_execution_survives() {
    let root = dir("e2e");
    let sub = Arc::new(LocalFsSubstrate::new(root.join("substrate").to_string_lossy().to_string()).unwrap());
    let mut e = L0Engine::<D1EventLog>::open(
        L0Config::for_dataset("d1_event_log", root.join("local").to_string_lossy().to_string())
            // Small parts so the 100-record workload produces SEVERAL, which is what makes
            // "a part entirely below the floor" vs "a part straddling it" distinguishable.
            .with_seal_max_records(12),
        sub,
    )
    .expect("open engine");

    // Three executions. "live" interleaves with the others from the very first sequence,
    // so an old part necessarily contains its events.
    let mut seq = 1u64;
    let mut per_exec: std::collections::BTreeMap<&str, Vec<u64>> = Default::default();
    for round in 0..40 {
        for ex in ["done-a", "live", "done-b"] {
            // the live execution keeps appending for the whole range
            if ex == "live" && (10..30).contains(&round) {
                continue;
            }
            e.append_record(erec(seq, ex)).expect("append");
            per_exec.entry(ex).or_default().push(seq);
            seq += 1;
        }
    }
    // Force the open part out so the manifest has sealed parts to prune.
    e.flush_and_wait_uploads().ok();
    e.seal_aged_parts().ok();
    let parts_before = e.manifest_snapshot().parts.len();
    let bytes_before: u64 = e.manifest_snapshot().parts.iter().map(|p| p.byte_size).sum();
    println!("  BEFORE: parts={parts_before} bytes={bytes_before}");
    assert!(parts_before >= 1, "the engine must have produced parts to prune");

    // Archive the two terminal executions (not "live").
    let store = FakeArchiveStore::new();
    let mut safe = Vec::new();
    for (i, ex) in ["done-a", "done-b"].iter().enumerate() {
        let seqs = &per_exec[*ex];
        let recs: Vec<ea::ArchiveRecord> = seqs
            .iter()
            .enumerate()
            .map(|(j, s)| arec(*s, j as i64))
            .collect();
        let id = 1000 + i as i64;
        ea::archive_execution_indexed(&store, &meta(id), &recs, Utc::now()).await.unwrap();
        assert!(ea::archive_state(&store, id).await.unwrap().is_safe_to_prune());
        safe.push(id);
    }

    // The floor, over the hot footprint.
    let hot = vec![
        ea::HotExecution { execution_id: 1000, min_sequence: per_exec["done-a"][0], max_sequence: *per_exec["done-a"].last().unwrap() },
        ea::HotExecution { execution_id: 1001, min_sequence: per_exec["done-b"][0], max_sequence: *per_exec["done-b"].last().unwrap() },
        ea::HotExecution { execution_id: 1002, min_sequence: per_exec["live"][0], max_sequence: *per_exec["live"].last().unwrap() },
    ];
    let live_min = per_exec["live"][0];

    // ⚠⚠ With "live" unarchived the floor sits at its earliest sequence, so NOTHING below
    // it that could hold live data is droppable.
    let pinned = ea::retention_floor(&hot, &safe);
    println!("  pinned floor: {}", pinned.describe());
    assert_eq!(pinned.keep_from_sequence, Some(live_min));
    assert_eq!(pinned.blocked_by.as_ref().unwrap().execution_id, 1002);

    let dropped_pinned = e.apply_retention(pinned.keep_from_sequence.unwrap()).expect("retention");
    let after_pinned = e.manifest_snapshot().parts.len();
    println!("  after PINNED retention: dropped={dropped_pinned} parts={after_pinned}");

    // ⭐ The decisive assertion: every one of the live execution's events is still readable.
    let live_seqs: std::collections::BTreeSet<u64> = per_exec["live"].iter().copied().collect();
    let still: std::collections::BTreeSet<u64> = e
        .read_index_after("live", 0)
        .expect("read live")
        .into_iter()
        .map(|r| r.global_sequence)
        .collect();
    println!("  live events: {} of {} survive", still.len(), live_seqs.len());
    assert_eq!(
        still, live_seqs,
        "⚠⚠⚠ a pruned part destroyed events of a STILL-RUNNING execution — this is the \
         data-loss case the floor exists to prevent"
    );

    // Now archive "live" too and re-run: with everything archived the floor rises and the
    // parts become droppable. This is the positive control — it proves the earlier refusal
    // was the live execution, not an inert prune.
    let lrecs: Vec<ea::ArchiveRecord> = per_exec["live"]
        .iter()
        .enumerate()
        .map(|(j, s)| arec(*s, j as i64))
        .collect();
    ea::archive_execution_indexed(&store, &meta(1002), &lrecs, Utc::now()).await.unwrap();
    let all_safe = vec![1000, 1001, 1002];
    let open = ea::retention_floor(&hot, &all_safe);
    println!("  open floor: {}", open.describe());
    assert!(open.blocked_by.is_none());

    let dropped_open = e.apply_retention(open.keep_from_sequence.unwrap()).expect("retention");
    let parts_after = e.manifest_snapshot().parts.len();
    let bytes_after: u64 = e.manifest_snapshot().parts.iter().map(|p| p.byte_size).sum();
    println!(
        "  AFTER: dropped={dropped_open} parts={parts_before}->{parts_after} \
         bytes={bytes_before}->{bytes_after} reclaimed={}",
        bytes_before.saturating_sub(bytes_after)
    );

    // ⭐ MEASURE the reclaim, with the denominator. A "0 reclaimed" must be distinguishable
    // from "nothing was droppable".
    assert!(
        dropped_open > 0 || dropped_pinned > 0,
        "no part was ever dropped: parts_before={parts_before}, so this test measured \
         nothing and would pass against a prune that does nothing"
    );
    assert!(
        bytes_after < bytes_before,
        "bytes must actually come back: {bytes_before} -> {bytes_after}"
    );

    // And the archive still serves the pruned execution.
    let (_, back) = ea::read_archived(&store, 1000).await.unwrap().expect("archived");
    let want: std::collections::BTreeSet<u64> = per_exec["done-a"].iter().copied().collect();
    let got: std::collections::BTreeSet<u64> = back.iter().map(|r| r.global_sequence).collect();
    assert_eq!(got, want, "the archive must serve what the hot store no longer holds");
}
