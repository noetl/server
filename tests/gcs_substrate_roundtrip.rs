//! A2 round-trip against a GCS emulator — [noetl/ai-meta#460](https://github.com/noetl/ai-meta/issues/460).
//!
//! ⚠ `#[ignore]` by default: these need a `fake-gcs-server` listening on 127.0.0.1:4443 with
//! the buckets below, so CI (which has neither) would otherwise fail or, worse, pass
//! vacuously. Run them deliberately:
//!
//! ```text
//! podman run -d --name fake-gcs-a2 -p 4443:4443 docker.io/fsouza/fake-gcs-server:latest \
//!   -scheme http -port 4443 -public-host 127.0.0.1:4443
//! curl -XPOST 'http://127.0.0.1:4443/storage/v1/b?project=test' \
//!   -H 'Content-Type: application/json' -d '{"name":"ehdb-replica"}'
//! curl -XPOST 'http://127.0.0.1:4443/storage/v1/b?project=test' \
//!   -H 'Content-Type: application/json' -d '{"name":"ehdb-empty"}'
//! cargo test --test gcs_substrate_roundtrip -- --ignored --test-threads=1
//! ```
//!
//! # What is being proved
//!
//! The unit tests in `gcs_substrate.rs` cover the domain algebra — that declaring a `Remote`
//! domain flips `survives_node_loss`. That is a statement about *metadata*. It says nothing
//! about whether any byte actually left the node.
//!
//! These tests close that gap by **recovering the log from the GCS replica alone**: a
//! cold-load against a fresh local root, with the original local substrate not in the
//! picture. If the records come back, sealed parts genuinely crossed to the remote.
//!
//! ⚠⚠ And a negative control, because the positive result has an innocent explanation that
//! would otherwise go unnoticed: a cold-load that silently read a local cache, or an
//! assertion comparing two empty vectors, looks exactly like success.

use std::sync::Arc;

use ehdb_l0::substrate::DurableSubstrate;
use ehdb_l0::{L0Config, L0EventLogEngine, LocalFsSubstrate, ReplicaTarget};
use noetl_server::services::gcs_substrate::GcsSubstrate;

const ENDPOINT: &str = "http://127.0.0.1:4443";

fn unique_dir(tag: &str) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("ehdb-a2-{tag}-{}-{n}", std::process::id()))
}

fn cfg(root: &std::path::Path) -> L0Config {
    L0Config::d1(root)
        .with_shard_count(1)
        .with_granule_size(4)
        // Small, so the appends below actually SEAL. An unsealed tail is never uploaded, so
        // a test that never seals would prove nothing while passing.
        .with_seal_max_records(8)
}

fn gcs(bucket: &str, prefix: &str) -> Arc<dyn DurableSubstrate> {
    Arc::new(GcsSubstrate::open(ENDPOINT, bucket, prefix).expect("emulator reachable"))
}

/// ⭐ The test A2 exists for: the log is recoverable from the off-box replica alone.
#[test]
#[ignore = "needs fake-gcs-server on 127.0.0.1:4443"]
fn sealed_parts_reach_gcs_and_the_log_cold_loads_from_it_alone() {
    let prefix = format!("rt-{}", std::process::id());
    let local_objects = unique_dir("objects");
    let origin_root = unique_dir("origin");
    let cold_root = unique_dir("cold");

    let local: Arc<dyn DurableSubstrate> =
        Arc::new(LocalFsSubstrate::new(&local_objects).unwrap());
    let remote = gcs("ehdb-replica", &prefix);

    let mut origin = L0EventLogEngine::open_replicated(
        cfg(&origin_root),
        vec![
            ReplicaTarget::new("replica-0", local.clone()),
            ReplicaTarget::new("replica-1-gcs", remote.clone()),
        ],
    )
    .expect("RF=2 open");

    for i in 0..48u64 {
        origin
            .append("90210", &format!("t{i}"), format!("payload-{i}"))
            .unwrap();
    }
    origin.flush_and_wait_uploads().unwrap();
    let expected = origin.replay_all().unwrap();
    assert!(
        expected.len() == 48,
        "the fixture must actually hold records, else every comparison below is between \
         empty vectors: {}",
        expected.len()
    );

    // 1. Objects physically exist in the bucket.
    let remote_parts = remote.list_prefix("parts/d1_event_log/").unwrap();
    assert!(
        !remote_parts.is_empty(),
        "no part objects in the replica bucket — nothing left the node, so the \
         `survives_node_loss` the domain algebra reports would be a claim about metadata only"
    );

    // 2. ⭐ Recovery from the remote ALONE, with a fresh local root and the original local
    //    substrate not passed in.
    let cold = L0EventLogEngine::cold_load(cfg(&cold_root), remote.clone())
        .expect("cold-load from the GCS replica");
    let recovered = cold.replay_all().unwrap();
    assert_eq!(
        recovered, expected,
        "the log did not come back from the off-box replica: {} of {} records",
        recovered.len(),
        expected.len()
    );

    drop(cold);
    drop(origin);
    let _ = std::fs::remove_dir_all(&local_objects);
    let _ = std::fs::remove_dir_all(&origin_root);
    let _ = std::fs::remove_dir_all(&cold_root);
}

/// ⚠⚠ The negative control for the test above.
///
/// A cold-load from a bucket nothing was written to must NOT produce the records. Without
/// this, the positive test is also satisfied by a cold-load that read a local directory, or
/// by `assert_eq!` comparing two empty vectors — both of which look like success.
#[test]
#[ignore = "needs fake-gcs-server on 127.0.0.1:4443"]
fn a_cold_load_from_an_untouched_bucket_recovers_nothing() {
    let cold_root = unique_dir("cold-empty");
    let empty = gcs("ehdb-empty", &format!("never-written-{}", std::process::id()));

    match L0EventLogEngine::cold_load(cfg(&cold_root), empty) {
        // Either outcome is correct — what must NOT happen is records appearing.
        Ok(engine) => {
            let recovered = engine.replay_all().unwrap_or_default();
            assert!(
                recovered.is_empty(),
                "a cold-load from an untouched prefix returned {} records; the positive test \
                 is then not evidence that anything crossed to GCS",
                recovered.len()
            );
        }
        Err(_) => {}
    }
    let _ = std::fs::remove_dir_all(&cold_root);
}

/// The substrate's own primitives, end to end against a real server: the immutability
/// contract and the short-read refusal, neither of which a mock would exercise faithfully.
#[test]
#[ignore = "needs fake-gcs-server on 127.0.0.1:4443"]
fn put_if_absent_is_atomic_and_get_range_refuses_an_over_read() {
    let s = GcsSubstrate::open(ENDPOINT, "ehdb-replica", &format!("prim-{}", std::process::id()))
        .unwrap();
    let key = "immutable/object";
    let body = b"0123456789";

    assert!(s.put_if_absent(key, body).unwrap(), "first write is new");
    assert!(
        !s.put_if_absent(key, b"different").unwrap(),
        "a second write must report ALREADY PRESENT, not overwrite — parts are immutable, \
         and silently replacing one would make a part's bytes mutable"
    );
    assert_eq!(
        s.get_all(key).unwrap(),
        body,
        "and the original bytes must survive the refused write"
    );

    // Range reads.
    assert_eq!(s.get_range(key, 2, 3).unwrap(), b"234");
    assert_eq!(s.get_range(key, 0, 10).unwrap(), body);

    // ⚠ The load-bearing refusal: an over-read must ERROR, not come back short.
    let over = s.get_range(key, 5, 100);
    assert!(
        over.is_err(),
        "an over-read returned {:?} instead of erroring — the manifest states every part's \
         length, so a short read would let a TRUNCATED object read as intact",
        over.map(|v| v.len())
    );

    // exists / get_all distinguish absent from empty.
    assert!(s.exists(key).unwrap());
    assert!(!s.exists("no/such/object").unwrap());
    assert!(
        s.get_all("no/such/object").is_err(),
        "an absent object must be NotFound, never an empty vec — conflating them would let a \
         missing part read as present-but-empty"
    );

    // delete is idempotent, and list reflects it.
    s.delete(key).unwrap();
    assert!(!s.exists(key).unwrap());
    s.delete(key).expect("deleting an absent key is not an error");
}
