//! The archived set survives a restart — noetl/ai-meta#506 RFC question 5.
//!
//! `known_archived` was in memory, so every restart re-verified the whole archived backlog
//! from GCS before trusting any of it — about 40 minutes on prod (2026-11-03), with the
//! prune refusing and re-touch detection waiting the whole time.

use std::collections::HashSet;

use chrono::{Duration, TimeZone, Utc};
use noetl_server::services::archive_store::FakeArchiveStore;
use noetl_server::services::event_archive as ea;

fn t(h: i64) -> chrono::DateTime<chrono::Utc> {
    Utc.timestamp_opt(1_790_000_000, 0).unwrap() + Duration::hours(h)
}

fn set(ids: &[i64]) -> HashSet<i64> {
    ids.iter().copied().collect()
}

fn snapshot() -> ea::KnownSnapshot {
    let mut retouch = ea::RetouchState::default();
    retouch.extents.insert(2, (10, 20));
    ea::KnownSnapshot::capture("bkt", &set(&[3, 1, 2]), &set(&[9, 8]), &retouch, t(0))
}

#[test]
fn a_snapshot_round_trips_and_is_deterministic() {
    let snap = snapshot();
    let ids: Vec<i64> = snap.archived.iter().map(|k| k.id).collect();
    assert_eq!(ids, vec![1, 2, 3], "sorted, so the file is deterministic");
    assert_eq!(snap.out_of_coverage, vec![8, 9]);
    let bytes = serde_json::to_vec(&snap).unwrap();
    assert_eq!(ea::KnownSnapshot::decode(&bytes, "bkt").unwrap(), snap);
}

/// "Archived" means archived in THAT bucket. A snapshot for another one is refused.
#[test]
fn a_snapshot_for_another_bucket_is_refused() {
    let bytes = serde_json::to_vec(&snapshot()).unwrap();
    let err = ea::KnownSnapshot::decode(&bytes, "other-bucket").unwrap_err();
    assert!(err.starts_with("bucket mismatch"), "{err}");
}

#[test]
fn a_wrong_version_or_garbage_is_refused() {
    let mut snap = snapshot();
    snap.version = ea::KNOWN_SNAPSHOT_VERSION + 1;
    let bytes = serde_json::to_vec(&snap).unwrap();
    assert!(ea::KnownSnapshot::decode(&bytes, "bkt").is_err());
    assert!(ea::KnownSnapshot::decode(b"{not json", "bkt").is_err());
}

/// ⭐ An entry without an extent loads WITHOUT one. A made-up (0, 0) extent would make every
/// part read as past the archive, and re-touch detection would confirm healthy executions.
#[test]
fn apply_never_invents_an_extent() {
    let snap = snapshot();
    let mut known = HashSet::new();
    let mut ooc = HashSet::new();
    let mut retouch = ea::RetouchState::default();
    snap.apply(&mut known, &mut ooc, &mut retouch);
    assert_eq!(known, set(&[1, 2, 3]));
    assert_eq!(ooc, set(&[8, 9]));
    assert_eq!(retouch.extents.len(), 1);
    assert_eq!(retouch.extents.get(&2), Some(&(10, 20)));
}

/// `shadow` measures how much of the snapshot the live re-verification has confirmed.
#[test]
fn confirmation_counts_claims_the_live_state_has_reverified() {
    assert_eq!(snapshot().confirmation(&set(&[1, 3, 77])), (2, 1));
}

#[test]
fn the_write_is_atomic_and_leaves_no_temp_file() {
    let dir = std::env::temp_dir().join(format!("noetl-known-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("archive-known.json");
    ea::write_known_snapshot(&path, &snapshot()).unwrap();
    let back = ea::KnownSnapshot::decode(&std::fs::read(&path).unwrap(), "bkt").unwrap();
    assert_eq!(back, snapshot());
    assert!(!path.with_extension("json.tmp").exists());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_mode_parses_conservatively() {
    assert_eq!(ea::KnownPersistMode::parse(""), ea::KnownPersistMode::Off);
    assert_eq!(
        ea::KnownPersistMode::parse("garbage"),
        ea::KnownPersistMode::Off
    );
    assert_eq!(
        ea::KnownPersistMode::parse(" Shadow "),
        ea::KnownPersistMode::Shadow
    );
    assert_eq!(ea::KnownPersistMode::parse("on"), ea::KnownPersistMode::On);
}

/// ⭐⭐ The point of it: under `on`, an execution loaded from the snapshot is skipped by the
/// pass WITHOUT reading the archive. The store here is empty, so a read-back would have
/// found nothing and the execution would not have counted as archived.
#[tokio::test]
async fn a_loaded_execution_is_known_without_an_archive_read() {
    let store = FakeArchiveStore::new();
    let cfg = ea::RetentionConfig {
        archive_enabled: true,
        bucket: Some("bkt".into()),
        ..Default::default()
    };
    let cands = vec![ea::ArchiveCandidate {
        execution_id: 2,
        status: "COMPLETED".into(),
        completed_at: Some(t(0)),
        started_at: t(0),
    }];
    let mut known = HashSet::new();
    let mut ooc = HashSet::new();
    let mut retouch = ea::RetouchState::default();
    snapshot().apply(&mut known, &mut ooc, &mut retouch);

    let mut reads = 0;
    let rep = ea::archive_pass(
        &store,
        &cfg,
        &cands,
        t(100),
        &mut known,
        &mut ooc,
        &mut retouch,
        |_| {
            reads += 1;
            Some(vec![])
        },
    )
    .await;
    assert_eq!(rep.already_archived, 1, "{}", rep.describe());
    assert_eq!(reads, 0, "no hot read either");
    assert!(
        store.keys().is_empty(),
        "nothing was written to the archive"
    );
}
