//! P3 of [noetl/ai-meta#459](https://github.com/noetl/ai-meta/issues/459) — discovery by
//! date. One reference object per execution under `date=YYYY-MM-DD/`, so a `list(prefix)`
//! returns exactly the executions that STARTED that date.
//!
//! ⭐⭐ The deviation from a single per-day manifest file is deliberate and is the point of
//! this phase: GCS has no append, so one object per day would be read-modify-write and two
//! concurrent archives would lose an entry with nothing reporting it. The concurrency test
//! below is the proof.

use chrono::{Duration, TimeZone, Utc};
use noetl_server::services::archive_store::{ArchiveStore, FakeArchiveStore};
use noetl_server::services::event_archive as ea;

fn at(day: &str, hour: i64) -> chrono::DateTime<chrono::Utc> {
    let d: Vec<i64> = day.split('-').map(|p| p.parse().unwrap()).collect();
    Utc.with_ymd_and_hms(d[0] as i32, d[1] as u32, d[2] as u32, 0, 0, 0)
        .unwrap()
        + Duration::hours(hour)
}

fn recs(n: usize) -> Vec<ea::ArchiveRecord> {
    (0..n)
        .map(|i| ea::ArchiveRecord {
            global_sequence: 1 + i as u64,
            event_id: Some(format!("ev-{i}")),
            event_type: "step.completed".into(),
            body: serde_json::json!({"i": i}),
        })
        .collect()
}

fn meta(id: i64, started: chrono::DateTime<chrono::Utc>) -> ea::ExecutionMeta {
    ea::ExecutionMeta {
        execution_id: id,
        status: "COMPLETED".into(),
        started_at: started,
        completed_at: Some(started + Duration::hours(1)),
    }
}

#[test]
fn the_reference_keys_match_the_required_date_partition() {
    assert_eq!(ea::date_prefix("2026-09-03"), "date=2026-09-03/");
    assert_eq!(
        ea::date_ref_key("2026-09-03", 42),
        "date=2026-09-03/execution_id=42.json"
    );
    // The date is the START date, in UTC.
    assert_eq!(ea::start_date(at("2026-09-03", 23)), "2026-09-03");
    assert_eq!(ea::start_date(at("2026-09-03", 24)), "2026-09-04");
}

/// ⭐ Set equality against ground truth. Three dates, interleaved ids, and the listing for
/// each must equal exactly the executions that started that day — not a count, and not a
/// superset.
#[tokio::test]
async fn discovery_by_date_returns_exactly_the_executions_that_started_that_date() {
    let store = FakeArchiveStore::new();
    // Ground truth, deliberately interleaved so an id-ordering bug cannot pass by luck.
    let truth: Vec<(&str, i64)> = vec![
        ("2026-09-01", 101),
        ("2026-09-02", 201),
        ("2026-09-01", 102),
        ("2026-09-03", 301),
        ("2026-09-02", 202),
        ("2026-09-01", 103),
        ("2026-09-03", 302),
    ];
    for (day, id) in &truth {
        ea::archive_execution_indexed(&store, &meta(*id, at(day, 9)), &recs(3), Utc::now())
            .await
            .expect("archive+index");
    }

    for day in ["2026-09-01", "2026-09-02", "2026-09-03"] {
        let mut want: Vec<i64> = truth.iter().filter(|(d, _)| *d == day).map(|(_, i)| *i).collect();
        want.sort_unstable();
        let listing = ea::list_executions_for_date(&store, day, 1000).await.unwrap();
        assert_eq!(
            listing.execution_ids, want,
            "{day}: listing must set-equal ground truth"
        );
        // ⭐ Denominator reported, and nothing unrecognised fell through silently.
        assert_eq!(listing.keys_seen, want.len());
        assert_eq!(listing.unrecognised, 0);
        assert!(!listing.truncated);
    }

    // Negative control: a day with nothing archived is EMPTY, and that is distinguishable
    // from a day that was never asked about because `keys_seen` is 0 too.
    let empty = ea::list_executions_for_date(&store, "2026-09-04", 1000).await.unwrap();
    assert!(empty.execution_ids.is_empty());
    assert_eq!(empty.keys_seen, 0);
}

/// ⚠⚠ The concurrency property that justifies the layout. Many executions archiving for the
/// SAME date must all survive — which a single appended per-day object could not guarantee
/// without locking, because GCS has no append.
#[tokio::test]
async fn many_executions_on_one_date_all_survive_concurrent_archiving() {
    use std::sync::Arc;
    let store = Arc::new(FakeArchiveStore::new());
    let day = "2026-09-03";
    let ids: Vec<i64> = (5_000..5_060).collect();

    let mut set = tokio::task::JoinSet::new();
    for id in ids.clone() {
        let s = store.clone();
        set.spawn(async move {
            ea::archive_execution_indexed(&*s, &meta(id, at(day, 12)), &recs(2), Utc::now())
                .await
                .map(|_| id)
        });
    }
    let mut done = Vec::new();
    while let Some(r) = set.join_next().await {
        done.push(r.expect("task").expect("archive"));
    }
    done.sort_unstable();
    assert_eq!(done, ids, "every concurrent archive must have succeeded");

    let listing = ea::list_executions_for_date(&*store, day, 10_000).await.unwrap();
    assert_eq!(
        listing.execution_ids, ids,
        "⚠⚠ the by-date index LOST entries under concurrency — {} of {} survived",
        listing.execution_ids.len(),
        ids.len()
    );
}

/// Idempotent: re-archiving the same execution rewrites one reference, never a duplicate.
#[tokio::test]
async fn re_indexing_the_same_execution_does_not_duplicate_the_reference() {
    let store = FakeArchiveStore::new();
    let m = meta(77, at("2026-09-05", 3));
    ea::archive_execution_indexed(&store, &m, &recs(4), Utc::now()).await.unwrap();
    let first = ea::list_executions_for_date(&store, "2026-09-05", 100).await.unwrap();
    ea::archive_execution_indexed(&store, &m, &recs(4), Utc::now()).await.unwrap();
    let second = ea::list_executions_for_date(&store, "2026-09-05", 100).await.unwrap();
    assert_eq!(first.execution_ids, vec![77]);
    assert_eq!(second.execution_ids, vec![77], "a re-index must not duplicate");
    assert_eq!(second.keys_seen, 1);
}

/// ⚠ A stray object under the date prefix must not become a phantom execution id.
#[tokio::test]
async fn a_stray_object_under_the_prefix_is_counted_not_invented() {
    let store = FakeArchiveStore::new();
    ea::archive_execution_indexed(&store, &meta(9, at("2026-09-06", 1)), &recs(2), Utc::now())
        .await
        .unwrap();
    // Something else writes under the same prefix — a derived daily roll-up, say.
    store
        .put("date=2026-09-06/index.json", "application/json", b"{}")
        .await
        .unwrap();
    store
        .put("date=2026-09-06/execution_id=notanumber.json", "application/json", b"{}")
        .await
        .unwrap();

    let l = ea::list_executions_for_date(&store, "2026-09-06", 100).await.unwrap();
    assert_eq!(l.execution_ids, vec![9], "only real references become ids");
    assert_eq!(l.keys_seen, 3, "the denominator must include what was skipped");
    assert_eq!(l.unrecognised, 2, "skipped keys must be COUNTED, not silently dropped");
}

/// ⚠ A truncated listing must say so. Silence would read exactly like a quiet day.
#[tokio::test]
async fn a_capped_listing_reports_that_it_was_truncated() {
    let store = FakeArchiveStore::new();
    for id in 0..10 {
        ea::archive_execution_indexed(&store, &meta(id, at("2026-09-07", 2)), &recs(1), Utc::now())
            .await
            .unwrap();
    }
    let full = ea::list_executions_for_date(&store, "2026-09-07", 100).await.unwrap();
    assert_eq!(full.execution_ids.len(), 10);
    assert!(!full.truncated);

    let capped = ea::list_executions_for_date(&store, "2026-09-07", 4).await.unwrap();
    assert_eq!(capped.execution_ids.len(), 4);
    assert!(
        capped.truncated,
        "a listing that hit its cap must report truncation, or a short list reads as a quiet day"
    );
}

#[test]
fn the_key_parser_refuses_shapes_it_should_not_interpret() {
    assert_eq!(
        ea::execution_id_from_date_key("date=2026-09-03/execution_id=42.json"),
        Some(42)
    );
    for bad in [
        "date=2026-09-03/index.json",
        "date=2026-09-03/execution_id=.json",
        "date=2026-09-03/execution_id=abc.json",
        "date=2026-09-03/execution_id=42.txt",
        "execution_id=42/manifest.json",
        "",
        "date=2026-09-03/",
    ] {
        assert_eq!(
            ea::execution_id_from_date_key(bad),
            None,
            "{bad:?} must not parse as an execution id"
        );
    }
    // Negative ids are real snowflake-adjacent values; do not reject them accidentally.
    assert_eq!(
        ea::execution_id_from_date_key("date=2026-09-03/execution_id=-7.json"),
        Some(-7)
    );
}

/// The reference carries enough to decide whether to fetch the execution.
#[tokio::test]
async fn the_reference_round_trips_with_the_fields_a_caller_needs() {
    let store = FakeArchiveStore::new();
    let m = meta(555, at("2026-09-08", 6));
    let (manifest, key) = ea::archive_execution_indexed(&store, &m, &recs(7), Utc::now())
        .await
        .unwrap();
    assert_eq!(key, "date=2026-09-08/execution_id=555.json");
    let r = ea::read_date_reference(&store, "2026-09-08", 555)
        .await
        .unwrap()
        .expect("present");
    assert_eq!(r.execution_id, 555);
    assert_eq!(r.record_count, 7);
    assert_eq!(r.status, "COMPLETED");
    assert_eq!(r.data_key, manifest.data_key);
    assert_eq!(r.started_at, m.started_at);
    // Absent reads as absent.
    assert!(ea::read_date_reference(&store, "2026-09-08", 556).await.unwrap().is_none());
}

/// The real client, against the emulator: the date index works over HTTP too.
#[tokio::test]
#[ignore = "needs a fake-gcs-server on NOETL_TEST_GCS_ENDPOINT"]
async fn the_date_index_works_against_the_emulator() {
    let endpoint = std::env::var("NOETL_TEST_GCS_ENDPOINT").expect("endpoint");
    let bucket = std::env::var("NOETL_TEST_GCS_BUCKET").expect("bucket");
    let store = noetl_server::services::object_backend::GcsBackend::open_unauthenticated(
        &endpoint, &bucket,
    )
    .unwrap();

    // A unique day per run so a rerun does not read the previous run's references.
    let tag = Utc::now().timestamp_subsec_micros() % 28 + 1;
    let day = format!("2027-01-{tag:02}");
    let ids: Vec<i64> = (0..12).map(|i| 800_000 + tag as i64 * 100 + i).collect();
    for id in &ids {
        ea::archive_execution_indexed(&store, &meta(*id, at(&day, 10)), &recs(3), Utc::now())
            .await
            .expect("archive+index to the emulator");
    }
    let listing = ea::list_executions_for_date(&store, &day, 1000).await.unwrap();
    println!(
        "  date={day} keys_seen={} ids={} unrecognised={} truncated={}",
        listing.keys_seen,
        listing.execution_ids.len(),
        listing.unrecognised,
        listing.truncated
    );
    let mut want = ids.clone();
    want.sort_unstable();
    assert_eq!(listing.execution_ids, want, "set equality over HTTP");
    assert_eq!(listing.unrecognised, 0);

    // Negative control: a day nothing was written for must be empty, not a stray hit.
    let other = ea::list_executions_for_date(&store, "2027-02-28", 1000).await.unwrap();
    println!("  control day 2027-02-28: keys_seen={}", other.keys_seen);
    assert!(other.execution_ids.is_empty());
}
