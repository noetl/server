//! P2 of [noetl/ai-meta#459](https://github.com/noetl/ai-meta/issues/459) — archive one
//! execution to the object store in the exact layout, verifiably, and read it back.
//!
//! Proven twice: against an in-process fake (where faults can be injected) and against a
//! **real fake-gcs-server emulator** through the production `GcsBackend` HTTP client, so
//! the wire path is exercised and not merely the logic.
//!
//! The emulator test is `#[ignore]`d — it needs a server on `NOETL_TEST_GCS_ENDPOINT` —
//! and is run explicitly. It reports the population it wrote so a vacuous pass is visible.

use chrono::{TimeZone, Utc};
use noetl_server::services::archive_store::{ArchiveStore, FakeArchiveStore};
use noetl_server::services::event_archive as ea;

fn ts(h: i64) -> chrono::DateTime<chrono::Utc> {
    Utc.timestamp_opt(1_790_000_000, 0).unwrap() + chrono::Duration::hours(h)
}

fn recs(n: usize, base_seq: u64) -> Vec<ea::ArchiveRecord> {
    (0..n)
        .map(|i| ea::ArchiveRecord {
            global_sequence: base_seq + i as u64,
            event_id: 9_000 + i as i64,
            event_type: if i == n - 1 {
                "playbook.completed".into()
            } else {
                "step.completed".into()
            },
            body: serde_json::json!({"i": i, "note": "archive probe"}),
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

/// ⭐ The required layout, asserted literally. `bucket/execution_id=<id>/<files>`.
#[test]
fn the_keys_match_the_required_execution_id_partition() {
    assert_eq!(ea::execution_prefix(42), "execution_id=42/");
    assert_eq!(ea::manifest_key(42), "execution_id=42/manifest.json");
    let dk = ea::data_key(42, 7, 99);
    assert!(
        dk.starts_with("execution_id=42/"),
        "the data object must live under the execution partition: {dk}"
    );
    // Zero-padded so lexicographic listing is sequence order — GCS lists lexicographically.
    assert!(dk.contains("events-00000000000000000007-00000000000000000099"), "{dk}");
    // Negative control: a different execution never shares a prefix.
    assert!(!ea::data_key(43, 7, 99).starts_with(&ea::execution_prefix(42)));
}

/// Round-trip: what comes back set-equals what went in, in sequence order.
#[tokio::test]
async fn an_archived_execution_round_trips_by_execution_id() {
    let store = FakeArchiveStore::new();
    let records = recs(50, 1_000);
    let m = ea::archive_execution(&store, &meta(7), &records, ts(2))
        .await
        .expect("archive");

    assert_eq!(m.record_count, 50);
    assert_eq!(m.min_sequence, 1_000);
    assert_eq!(m.max_sequence, 1_049);
    assert_eq!(m.schema_version, ea::ARCHIVE_SCHEMA_VERSION);

    // Exactly two objects under the partition: data + manifest.
    let keys = store.keys();
    let under: Vec<&String> = keys
        .iter()
        .filter(|k| k.starts_with(&ea::execution_prefix(7)))
        .collect();
    assert_eq!(under.len(), 2, "expected data + manifest, got {under:?}");

    let (m2, back) = ea::read_archived(&store, 7)
        .await
        .expect("read")
        .expect("present");
    assert_eq!(m2, m, "the manifest must round-trip byte-identically");

    // ⭐ SET EQUALITY against ground truth, not a count. A count matches when the wrong 50
    // records are stored.
    let mut want: Vec<(u64, i64)> = records.iter().map(|r| (r.global_sequence, r.event_id)).collect();
    let mut got: Vec<(u64, i64)> = back.iter().map(|r| (r.global_sequence, r.event_id)).collect();
    want.sort();
    got.sort();
    assert_eq!(got, want, "the archived record set must equal the input set");

    // And ordered ascending, because a replay reads the archive in order.
    assert!(
        back.windows(2).all(|w| w[0].global_sequence <= w[1].global_sequence),
        "archived records must be in ascending sequence order"
    );
}

/// ⚠ Encoding sorts, rather than trusting the caller. An unsorted archive would be a
/// correct-looking object that replays wrong.
#[tokio::test]
async fn records_handed_over_out_of_order_are_archived_in_order() {
    let store = FakeArchiveStore::new();
    let mut records = recs(10, 500);
    records.reverse();
    ea::archive_execution(&store, &meta(8), &records, ts(2))
        .await
        .unwrap();
    let (_, back) = ea::read_archived(&store, 8).await.unwrap().unwrap();
    let seqs: Vec<u64> = back.iter().map(|r| r.global_sequence).collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    assert_eq!(seqs, sorted, "archive must be written in sequence order");
}

/// ⚠⚠ RED control: a **corrupted** data object must fail verification. The corruption flips
/// one byte and keeps the length, so only a digest can catch it — a size or presence check
/// would pass.
#[tokio::test]
async fn a_corrupt_archive_fails_verification_and_refuses_to_read() {
    let store = FakeArchiveStore::new();
    let records = recs(20, 1);
    let m = ea::archive_execution(&store, &meta(9), &records, ts(2))
        .await
        .unwrap();

    // Positive control FIRST: it verifies clean before the fault is injected, so the
    // failure below is the corruption and not a broken verifier.
    assert!(
        ea::verify_archived(&store, 9).await.unwrap().is_durable(),
        "must be durable before corruption — otherwise this test proves nothing"
    );

    store.corrupt(&m.data_key);
    match ea::verify_archived(&store, 9).await.unwrap() {
        ea::Durability::DigestMismatch { expected, actual } => {
            assert_eq!(expected, m.data_sha256);
            assert_ne!(actual, expected);
        }
        other => panic!("corruption must be a DigestMismatch, got {other:?}"),
    }
    // And the read path must ERROR, never return "absent".
    let err = ea::read_archived(&store, 9).await.unwrap_err();
    assert!(err.contains("CORRUPT"), "{err}");
}

/// ⚠⚠ RED control: a **missing data object** with the manifest present is a broken archive,
/// not an absent execution. Returning `None` here is the failure mode this codebase keeps
/// paying for — a wrong lookup answering with silence.
#[tokio::test]
async fn a_missing_data_object_is_an_error_not_an_absent_execution() {
    let store = FakeArchiveStore::new();
    let m = ea::archive_execution(&store, &meta(10), &recs(5, 1), ts(2))
        .await
        .unwrap();
    assert!(store.remove(&m.data_key), "the data object existed");

    match ea::verify_archived(&store, 10).await.unwrap() {
        ea::Durability::DataMissing { key } => assert_eq!(key, m.data_key),
        other => panic!("expected DataMissing, got {other:?}"),
    }
    let err = ea::read_archived(&store, 10).await.unwrap_err();
    assert!(err.contains("MISSING"), "{err}");
    assert!(
        err.contains("not an absent execution"),
        "the error must say it is a broken archive, not absence: {err}"
    );
}

/// A genuinely never-archived execution is `None` — the one case where silence is correct.
#[tokio::test]
async fn an_unarchived_execution_reads_as_absent() {
    let store = FakeArchiveStore::new();
    assert!(ea::read_archived(&store, 1234).await.unwrap().is_none());
    assert_eq!(
        ea::verify_archived(&store, 1234).await.unwrap(),
        ea::Durability::ManifestMissing
    );
}

/// ⚠ A failed upload must not leave a manifest claiming the data is there.
#[tokio::test]
async fn a_failed_data_upload_does_not_write_a_manifest() {
    let store = FakeArchiveStore::new();
    let records = recs(5, 1);
    let dk = ea::data_key(11, 1, 5);
    store.fail_put_for(&dk);
    let err = ea::archive_execution(&store, &meta(11), &records, ts(2))
        .await
        .unwrap_err();
    assert!(err.contains("injected put failure"), "{err}");
    assert!(
        store.is_empty(),
        "a failed data upload must leave NOTHING — a manifest without its data would \
         verify as DataMissing but still be a half-archive: {:?}",
        store.keys()
    );
}

/// ⚠ An empty record set is refused. An empty archive object would later verify clean and
/// authorise pruning events that were never copied.
#[tokio::test]
async fn an_empty_execution_is_refused_rather_than_archived_empty() {
    let store = FakeArchiveStore::new();
    let err = ea::archive_execution(&store, &meta(12), &[], ts(2))
        .await
        .unwrap_err();
    assert!(err.contains("refusing to write an empty archive"), "{err}");
    assert!(store.is_empty());
}

/// Idempotent: a second pass writes the same keys and the same bytes.
#[tokio::test]
async fn archiving_twice_is_idempotent() {
    let store = FakeArchiveStore::new();
    let records = recs(30, 77);
    let a = ea::archive_execution(&store, &meta(13), &records, ts(2)).await.unwrap();
    let keys_after_first = store.keys();
    let b = ea::archive_execution(&store, &meta(13), &records, ts(3)).await.unwrap();
    assert_eq!(a.data_key, b.data_key);
    assert_eq!(a.data_sha256, b.data_sha256, "same records => same digest");
    assert_eq!(
        store.keys(),
        keys_after_first,
        "a second pass must not create new objects"
    );
    assert!(ea::verify_archived(&store, 13).await.unwrap().is_durable());
}

/// ⚠⚠ The truncation case: the data object decodes but holds fewer records than the
/// manifest claims. Caught by the count check, which the digest check alone would miss if
/// the digest were recomputed from the truncated bytes.
#[tokio::test]
async fn a_count_mismatch_is_caught_independently_of_the_digest() {
    let store = FakeArchiveStore::new();
    let records = recs(10, 1);
    let mut m = ea::archive_execution(&store, &meta(14), &records, ts(2)).await.unwrap();
    // Rewrite the manifest claiming more records than the object holds, keeping its digest
    // honest — so only the count check can fail.
    m.record_count = 99;
    store
        .put(
            &ea::manifest_key(14),
            "application/json",
            &serde_json::to_vec(&m).unwrap(),
        )
        .await
        .unwrap();
    match ea::verify_archived(&store, 14).await.unwrap() {
        ea::Durability::CountMismatch { expected, actual } => {
            assert_eq!((expected, actual), (99, 10));
        }
        other => panic!("expected CountMismatch, got {other:?}"),
    }
}

/// Every `Durability` reason must be distinct and actionable — three identical strings
/// would satisfy a naive non-empty check while losing the distinction the enum exists for.
#[test]
fn every_durability_reason_is_distinct() {
    let v = [
        ea::Durability::Durable { record_count: 1 },
        ea::Durability::ManifestMissing,
        ea::Durability::DataMissing { key: "k".into() },
        ea::Durability::DigestMismatch { expected: "a".into(), actual: "b".into() },
        ea::Durability::CountMismatch { expected: 1, actual: 2 },
        ea::Durability::Undecodable("x".into()),
    ];
    let msgs: Vec<String> = v.iter().map(|d| d.reason()).collect();
    for (i, a) in msgs.iter().enumerate() {
        assert!(!a.is_empty());
        for b in msgs.iter().skip(i + 1) {
            assert_ne!(a, b);
        }
    }
    assert_eq!(v.iter().filter(|d| d.is_durable()).count(), 1);
}

// ---------------------------------------------------------------------------
// The real wire path, against a real GCS-compatible emulator.
// ---------------------------------------------------------------------------

/// Exercises the **production `GcsBackend` HTTP client** against fake-gcs-server, so the
/// URL shapes, upload encoding and list parsing are proven — not just the archival logic.
///
/// Run with:
///   NOETL_TEST_GCS_ENDPOINT=http://127.0.0.1:4443 NOETL_TEST_GCS_BUCKET=ehdb-archive-test \
///     cargo test --test archive_execution emulator -- --ignored --nocapture
#[tokio::test]
#[ignore = "needs a fake-gcs-server on NOETL_TEST_GCS_ENDPOINT"]
async fn the_real_gcs_client_archives_and_round_trips_against_the_emulator() {
    let endpoint = std::env::var("NOETL_TEST_GCS_ENDPOINT").expect("NOETL_TEST_GCS_ENDPOINT");
    let bucket = std::env::var("NOETL_TEST_GCS_BUCKET").expect("NOETL_TEST_GCS_BUCKET");
    let store = noetl_server::services::object_backend::GcsBackend::open_unauthenticated(
        &endpoint, &bucket,
    )
    .expect("emulator backend");

    // A distinct id per run so reruns do not read a previous run's objects.
    let id: i64 = 700_000 + (Utc::now().timestamp_subsec_micros() as i64 % 90_000);
    let records = recs(120, 10_000);

    let m = ea::archive_execution(&store, &meta(id), &records, Utc::now())
        .await
        .expect("archive to the emulator");
    println!("  wrote execution_id={id} records={} key={}", m.record_count, m.data_key);

    let d = ea::verify_archived(&store, id).await.expect("verify call");
    println!("  durability: {}", d.reason());
    assert!(d.is_durable(), "emulator archive must verify durable: {}", d.reason());

    let (m2, back) = ea::read_archived(&store, id)
        .await
        .expect("read")
        .expect("present in the emulator");
    assert_eq!(m2.data_sha256, m.data_sha256);

    // ⭐ Set equality against ground truth, and the denominator printed.
    let mut want: Vec<(u64, i64)> = records.iter().map(|r| (r.global_sequence, r.event_id)).collect();
    let mut got: Vec<(u64, i64)> = back.iter().map(|r| (r.global_sequence, r.event_id)).collect();
    want.sort();
    got.sort();
    println!("  round-tripped {} of {} records", got.len(), want.len());
    assert_eq!(got, want);
    assert!(got.len() == 120, "denominator: expected 120, got {}", got.len());

    // The listing must see exactly this execution's two objects under its prefix.
    let keys = store
        .list(&ea::execution_prefix(id), 100)
        .await
        .expect("list");
    println!("  keys under execution_id={id}/: {keys:?}");
    assert_eq!(keys.len(), 2, "data + manifest, got {keys:?}");

    // Negative control: a never-written execution must be absent, not an error and not a
    // stray hit from a shared bucket.
    let absent = ea::read_archived(&store, id + 1).await.expect("absent read");
    assert!(absent.is_none(), "an unarchived id must read as absent");
}

/// ⚠⚠ The emulator constructor must refuse a real GCS host. An unauthenticated client
/// there fails every request, and the first guess would be the credential rather than the
/// constructor — so the refusal is the guard that keeps a test helper from becoming a
/// production footgun.
#[test]
fn the_unauthenticated_constructor_refuses_real_gcs() {
    use noetl_server::services::object_backend::GcsBackend;
    for host in [
        "https://storage.googleapis.com",
        "https://storage.googleapis.com/",
        "http://googleapis.com",
        "https://foo.googleapis.com:443",
    ] {
        let e = match GcsBackend::open_unauthenticated(host, "b") {
            Err(e) => e,
            Ok(_) => panic!("{host} must be refused by the unauthenticated constructor"),
        };
        assert!(e.contains("refuses"), "{host} must be refused: {e}");
        assert!(e.contains("from_env"), "the error must name the right path: {e}");
    }
    // Positive control: an emulator endpoint IS accepted, so the refusals above measure
    // the host check and not a constructor that rejects everything.
    assert!(GcsBackend::open_unauthenticated("http://127.0.0.1:4443", "b").is_ok(), "an emulator endpoint must be accepted");
    assert!(GcsBackend::open_unauthenticated("http://fake-gcs-server:4443", "b").is_ok());
    // And empties are refused.
    assert!(GcsBackend::open_unauthenticated("", "b").is_err());
    assert!(GcsBackend::open_unauthenticated("http://x", " ").is_err());
}
