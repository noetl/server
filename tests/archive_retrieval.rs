//! P5 of [noetl/ai-meta#459](https://github.com/noetl/ai-meta/issues/459) — retrieval:
//! hot first, archive on miss, and a pruned execution still fully readable.
//!
//! ## ⚠⚠ Scope, verified rather than assumed
//!
//! The server's embedded EHDB store is a **shadow**. Verified against `origin/main`: the
//! only readers of `engine()` are the shadow writer and the parity verifier, and
//! `GET /api/executions/{id}` serves events from Postgres `noetl.event`, which is
//! append-only and never purged. So pruning the embedded store does **not** make history
//! unreadable through the existing API.
//!
//! These tests therefore prove what the fall-through actually guarantees: that EHDB-format
//! history survives a prune and is readable by execution id and by date, and that a pruned
//! execution never reads as an empty success.

use chrono::{Duration, TimeZone, Utc};
use noetl_server::services::archive_store::FakeArchiveStore;
use noetl_server::services::event_archive as ea;

fn ts(h: i64) -> chrono::DateTime<chrono::Utc> {
    Utc.timestamp_opt(1_790_000_000, 0).unwrap() + Duration::hours(h)
}

fn recs(n: usize, base: u64) -> Vec<ea::ArchiveRecord> {
    (0..n)
        .map(|i| ea::ArchiveRecord {
            global_sequence: base + i as u64,
            event_id: Some(format!("ev-{i}")),
            event_type: "step.completed".into(),
            body: serde_json::json!({"i": i}),
        })
        .collect()
}

fn meta(id: i64) -> ea::ExecutionMeta {
    ea::ExecutionMeta {
        execution_id: id,
        status: "COMPLETED".into(),
        started_at: ts(0),
        completed_at: Some(ts(1)),
    }
}

/// While the hot store still has it, the hot store answers — the archive is not consulted.
#[tokio::test]
async fn a_hot_execution_is_served_from_the_hot_store() {
    let store = FakeArchiveStore::new();
    let hot = recs(12, 1);
    ea::archive_execution_indexed(&store, &meta(1), &hot, Utc::now()).await.unwrap();

    let r = ea::read_with_fallthrough(&store, 1, || Some(hot.clone())).await.unwrap();
    assert_eq!(r.source, ea::ReadSource::Hot { records: 12 });
    assert_eq!(r.records.len(), 12);
}

/// ⭐⭐ THE PHASE: an archived-then-pruned execution is still fully readable, and the
/// records are set-equal to what the hot store held before the prune.
#[tokio::test]
async fn a_pruned_execution_is_still_fully_readable_from_the_archive() {
    let store = FakeArchiveStore::new();
    let before_prune = recs(64, 9_000);
    ea::archive_execution_indexed(&store, &meta(2), &before_prune, Utc::now())
        .await
        .unwrap();

    // The prune happened: the hot store is open and holds NOTHING for this execution.
    let r = ea::read_with_fallthrough(&store, 2, || Some(Vec::new())).await.unwrap();

    match &r.source {
        ea::ReadSource::Archive { records, data_key, .. } => {
            assert_eq!(*records, 64);
            assert!(data_key.starts_with("execution_id=2/"), "{data_key}");
        }
        other => panic!("a pruned execution must be served from the archive, got {other:?}"),
    }

    // ⭐ Set equality against the pre-prune set, not a count.
    let mut want: Vec<(u64, Option<String>)> =
        before_prune.iter().map(|x| (x.global_sequence, x.event_id.clone())).collect();
    let mut got: Vec<(u64, Option<String>)> = r.records.iter().map(|x| (x.global_sequence, x.event_id.clone())).collect();
    want.sort();
    got.sort();
    assert_eq!(got, want, "the archive must return exactly the pre-prune event set");
    // And byte-for-byte on the bodies, so a lossy encode would fail here.
    assert_eq!(r.records, {
        let mut s = before_prune.clone();
        s.sort_by(|a, b| (a.global_sequence, &a.event_id).cmp(&(b.global_sequence, &b.event_id)));
        s
    });
}

/// ⚠⚠ The failure this function exists to prevent: an open-but-empty hot store must be a
/// MISS that falls through, never an answer. Treating it as "the execution has no events"
/// would turn every pruned execution into a silent empty success.
#[tokio::test]
async fn an_empty_hot_result_falls_through_instead_of_answering_empty() {
    let store = FakeArchiveStore::new();
    ea::archive_execution_indexed(&store, &meta(3), &recs(5, 1), Utc::now()).await.unwrap();

    // Hot returns Some(empty) — the post-prune state.
    let r = ea::read_with_fallthrough(&store, 3, || Some(Vec::new())).await.unwrap();
    assert!(
        matches!(r.source, ea::ReadSource::Archive { .. }),
        "Some(empty) from hot must fall through, got {:?}",
        r.source
    );
    assert_eq!(r.records.len(), 5);

    // Hot returns None (engine closed) — also a fall-through.
    let r2 = ea::read_with_fallthrough(&store, 3, || None).await.unwrap();
    assert!(matches!(r2.source, ea::ReadSource::Archive { .. }));
}

/// A never-existing id is NotFound — the one case where "nothing" is the right answer.
#[tokio::test]
async fn a_never_existing_execution_is_not_found_not_an_empty_success() {
    let store = FakeArchiveStore::new();
    let r = ea::read_with_fallthrough(&store, 404_404, || Some(Vec::new())).await.unwrap();
    assert_eq!(r.source, ea::ReadSource::NotFound);
    assert!(r.records.is_empty());
    // ⭐ And it is DISTINGUISHABLE from the pruned case above: different variant, so a
    // caller cannot confuse "pruned, here it is" with "no such execution".
    assert_ne!(r.source.label(), "archive");
    assert_eq!(r.source.label(), "not_found");
}

/// ⚠⚠ RED CONTROL on the fall-through: break the archive read and confirm it is DETECTED,
/// not silently empty. A corrupt archive behind a pruned hot store is data loss, and
/// reporting it as absence would hide that behind a clean 404.
#[tokio::test]
async fn a_broken_archive_behind_a_pruned_hot_store_errors_rather_than_reading_empty() {
    let store = FakeArchiveStore::new();
    let (m, _) = ea::archive_execution_indexed(&store, &meta(4), &recs(9, 1), Utc::now())
        .await
        .unwrap();

    // Positive control FIRST: it reads cleanly from the archive before the fault, so the
    // failure below is the fault and not a broken reader.
    let ok = ea::read_with_fallthrough(&store, 4, || Some(Vec::new())).await.unwrap();
    assert!(matches!(ok.source, ea::ReadSource::Archive { .. }));

    // Fault 1 — the data object vanishes while the manifest remains.
    store.vanish(&m.data_key);
    let err = ea::read_with_fallthrough(&store, 4, || Some(Vec::new()))
        .await
        .expect_err("a missing data object must be an ERROR, not NotFound");
    assert!(err.contains("MISSING"), "{err}");
    assert!(
        err.contains("not an absent execution"),
        "the error must distinguish a broken archive from absence: {err}"
    );

    // Fault 2 — corruption (one byte flipped, length preserved, so only a digest catches it).
    let store2 = FakeArchiveStore::new();
    let (m2, _) = ea::archive_execution_indexed(&store2, &meta(5), &recs(9, 1), Utc::now())
        .await
        .unwrap();
    store2.corrupt(&m2.data_key);
    let err2 = ea::read_with_fallthrough(&store2, 5, || Some(Vec::new()))
        .await
        .expect_err("corruption must be an ERROR");
    assert!(err2.contains("CORRUPT"), "{err2}");
}

/// The three read sources must serialise distinguishably — a caller reads this, not the
/// enum.
#[test]
fn the_three_read_sources_serialise_distinguishably() {
    let v = [
        ea::ReadSource::Hot { records: 3 },
        ea::ReadSource::Archive {
            records: 3,
            data_key: "execution_id=1/x".into(),
            archived_at: ts(0),
        },
        ea::ReadSource::NotFound,
    ];
    let json: Vec<String> = v.iter().map(|s| serde_json::to_string(s).unwrap()).collect();
    for (i, a) in json.iter().enumerate() {
        for b in json.iter().skip(i + 1) {
            assert_ne!(a, b);
        }
    }
    assert!(json[0].contains("\"source\":\"hot\""), "{}", json[0]);
    assert!(json[1].contains("\"source\":\"archive\""), "{}", json[1]);
    assert!(json[2].contains("\"source\":\"not_found\""), "{}", json[2]);
}

/// ⚠ A malformed date must be rejected, not turned into a prefix that matches nothing and
/// returns a clean empty — a quiet day that never existed.
#[test]
fn the_date_validator_refuses_anything_that_is_not_an_iso_date() {
    use noetl_server::handlers::archive_read::is_iso_date;
    assert!(is_iso_date("2026-09-03"));
    assert!(is_iso_date("2024-02-29"), "a real leap day must be accepted");
    for bad in [
        "", "2026-9-3", "2026/09/03", "26-09-03", "2026-09-03T00:00:00Z",
        "2026-13-01", "2026-02-30", "2023-02-29", "yyyy-mm-dd", "2026-09-032",
    ] {
        assert!(!is_iso_date(bad), "{bad:?} must be refused");
    }
}

// ---------------------------------------------------------------------------
// End to end over the real emulator: archive -> prune -> read back by id and by date.
// ---------------------------------------------------------------------------

/// ⭐⭐ The whole retrieval phase against a real fake-gcs-server, through the production
/// `GcsBackend` client: archive several executions, simulate the prune (hot returns empty),
/// read each back by execution id, and discover them by date — then fetch each discovered
/// id, which is the by-date path used from the read side.
#[tokio::test]
#[ignore = "needs a fake-gcs-server on NOETL_TEST_GCS_ENDPOINT"]
async fn retrieval_round_trips_by_id_and_by_date_against_the_emulator() {
    let endpoint = std::env::var("NOETL_TEST_GCS_ENDPOINT").expect("endpoint");
    let bucket = std::env::var("NOETL_TEST_GCS_BUCKET").expect("bucket");
    let store = noetl_server::services::object_backend::GcsBackend::open_unauthenticated(
        &endpoint, &bucket,
    )
    .unwrap();

    // A unique day + id block per run.
    let tag = (Utc::now().timestamp_subsec_micros() % 27 + 1) as i64;
    let day = format!("2028-03-{tag:02}");
    let started = Utc
        .with_ymd_and_hms(2028, 3, tag as u32, 8, 0, 0)
        .unwrap();

    let mut truth: std::collections::BTreeMap<i64, Vec<(u64, Option<String>)>> = Default::default();
    for k in 0..6i64 {
        let id = 900_000 + tag * 1_000 + k;
        let r = recs(20 + k as usize, 50_000 + k as u64 * 1_000);
        let m = ea::ExecutionMeta {
            execution_id: id,
            status: if k == 3 { "FAILED".into() } else { "COMPLETED".into() },
            started_at: started,
            completed_at: Some(started + Duration::hours(1)),
        };
        ea::archive_execution_indexed(&store, &m, &r, Utc::now())
            .await
            .expect("archive+index");
        truth.insert(id, r.iter().map(|x| (x.global_sequence, x.event_id.clone())).collect());
    }
    println!("  archived {} executions for date={day}", truth.len());

    // 1. Retrieval BY EXECUTION ID, with the hot store pruned (returns empty).
    let mut served_from_archive = 0usize;
    for (id, want) in &truth {
        let r = ea::read_with_fallthrough(&store, *id, || Some(Vec::new()))
            .await
            .expect("fallthrough read");
        match &r.source {
            ea::ReadSource::Archive { records, .. } => {
                served_from_archive += 1;
                assert_eq!(*records, want.len());
            }
            other => panic!("execution {id} must be served from the archive, got {other:?}"),
        }
        let mut got: Vec<(u64, Option<String>)> =
            r.records.iter().map(|x| (x.global_sequence, x.event_id.clone())).collect();
        let mut w = want.clone();
        got.sort();
        w.sort();
        assert_eq!(got, w, "execution {id} must round-trip set-equal");
    }
    println!("  served from archive: {served_from_archive} of {}", truth.len());
    assert_eq!(served_from_archive, truth.len());

    // 2. Discovery BY DATE, then fetch each discovered id — the read-side path.
    let listing = ea::list_executions_for_date(&store, &day, 1_000).await.expect("list");
    println!(
        "  date={day} keys_seen={} ids={} unrecognised={} truncated={}",
        listing.keys_seen,
        listing.execution_ids.len(),
        listing.unrecognised,
        listing.truncated
    );
    let want_ids: Vec<i64> = truth.keys().copied().collect();
    assert_eq!(listing.execution_ids, want_ids, "by-date set equality");
    assert_eq!(listing.unrecognised, 0);

    let mut fetched = 0usize;
    for id in &listing.execution_ids {
        let r = ea::read_with_fallthrough(&store, *id, || Some(Vec::new()))
            .await
            .expect("fetch a discovered id");
        assert!(matches!(r.source, ea::ReadSource::Archive { .. }));
        assert_eq!(r.records.len(), truth[id].len());
        fetched += 1;
    }
    println!("  fetched {fetched} of {} discovered ids", listing.execution_ids.len());
    assert_eq!(fetched, want_ids.len());

    // 3. Negative controls over HTTP: an unarchived id is NotFound, and a day with nothing
    // archived lists empty — neither is an error and neither is a stray hit.
    let absent = ea::read_with_fallthrough(&store, 999_999_999, || Some(Vec::new()))
        .await
        .expect("absent read must not error");
    assert_eq!(absent.source, ea::ReadSource::NotFound);
    let quiet = ea::list_executions_for_date(&store, "2028-12-31", 100).await.unwrap();
    println!("  control day 2028-12-31: keys_seen={}", quiet.keys_seen);
    assert!(quiet.execution_ids.is_empty());
}
