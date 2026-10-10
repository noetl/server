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

    // Either outcome is correct — a cold-load against an untouched prefix may refuse
    // outright. What must NOT happen is records appearing.
    if let Ok(engine) = L0EventLogEngine::cold_load(cfg(&cold_root), empty) {
        let recovered = engine.replay_all().unwrap_or_default();
        assert!(
            recovered.is_empty(),
            "a cold-load from an untouched prefix returned {} records; the positive test is \
             then not evidence that anything crossed to GCS",
            recovered.len()
        );
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

/// ⭐⭐ **The prod scenario, end to end.** This is the exact operation performed on
/// `noetl-server-rust-embedded-0`: a store that has been running local-only accumulates
/// history, then an off-box GCS replica is attached.
///
/// Before the backfill the remote is in the state measured on prod — it holds the durable
/// manifest naming **every** part and the bytes of only the ones sealed since the attach.
/// That is why a cold-load from it must be attempted *before* as well as after: the "after"
/// result alone is consistent with a remote that was complete all along.
#[test]
#[ignore = "needs fake-gcs-server on 127.0.0.1:4443"]
fn a_backfill_makes_an_attached_replica_recoverable_and_it_was_not_before() {
    let prefix = format!("bf-{}", std::process::id());
    let local_objects = unique_dir("bf-objects");
    let origin_root = unique_dir("bf-origin");

    let local: Arc<dyn DurableSubstrate> =
        Arc::new(LocalFsSubstrate::new(&local_objects).unwrap());

    // --- Phase 1: history, written with NO off-box replica (prod before the attach) ---
    let mut origin = L0EventLogEngine::open_replicated(
        cfg(&origin_root),
        vec![ReplicaTarget::new("replica-0", local.clone())],
    )
    .unwrap();
    let mut expected: Vec<String> = Vec::new();
    for i in 0..24u64 {
        origin
            .append("1001", &format!("t{i}"), format!("history-{i}"))
            .unwrap();
        expected.push(format!("history-{i}"));
    }
    origin.flush_and_wait_uploads().unwrap();
    assert_eq!(origin.manifest_snapshot().parts.len(), 3, "24/8 = 3 parts");
    drop(origin);

    // --- Phase 2: attach the GCS replica, and seal ONE more part ---
    let remote = gcs("ehdb-backfill", &prefix);
    let mut attached = L0EventLogEngine::open_replicated(
        cfg(&origin_root),
        vec![
            ReplicaTarget::new("replica-0", local.clone()),
            ReplicaTarget::new("replica-1-gcs", remote.clone()),
        ],
    )
    .unwrap();
    for i in 24..32u64 {
        attached
            .append("1001", &format!("t{i}"), format!("history-{i}"))
            .unwrap();
        expected.push(format!("history-{i}"));
    }
    attached.flush_and_wait_uploads().unwrap();
    attached.refresh_state_gauges();

    // The prod shape, reproduced: the manifest names 4 parts, exactly one of which the
    // remote actually holds.
    let m = attached.manifest_snapshot();
    assert_eq!(m.parts.len(), 4);
    let two_way = m.parts.iter().filter(|p| p.replica_count() == 2).count();
    let one_way = m.parts.iter().filter(|p| p.replica_count() == 1).count();
    assert_eq!(
        (one_way, two_way),
        (3, 1),
        "3 pre-existing parts single-copy, 1 replicated — the prod 39:1 shape in miniature"
    );
    let remote_parts = remote.list_prefix("parts/d1_event_log/").unwrap();
    assert_eq!(
        remote_parts.len(),
        1,
        "the remote physically holds one part while its manifest names four"
    );

    // ⚠ The negative control, and the reason it is not optional: a cold-load from the
    // remote alone must FAIL to reproduce history at this point. Without this, the
    // post-backfill success is consistent with the remote having been complete all along.
    let pre_root = unique_dir("bf-cold-pre");
    let pre = L0EventLogEngine::cold_load_replicated(
        cfg(&pre_root),
        vec![ReplicaTarget::new("replica-1-gcs", remote.clone())],
    );
    // ⚠ Two outcomes to tell apart, because they are different claims and only one is
    // true: does the OPEN fail, or does the open succeed and the REPLAY fail? Collapsing
    // them with `unwrap_or_default()` reports "recovered 0 records", which reads as "the
    // remote is an empty log" — a claim this does not establish.
    let (pre_opened, pre_replay_err, pre_recovered): (bool, Option<String>, Vec<String>) =
        match pre {
            Err(e) => (false, Some(e.to_string()), Vec::new()),
            Ok(ref engine) => match engine.replay_all() {
                Ok(rs) => (
                    true,
                    None,
                    rs.into_iter().map(|r| r.payload).collect::<Vec<String>>(),
                ),
                Err(e) => (true, Some(e.to_string()), Vec::new()),
            },
        };
    eprintln!(
        "PRE-BACKFILL  opened={pre_opened}  replay_err={:?}  recovered={} of {}",
        pre_replay_err,
        pre_recovered.len(),
        expected.len()
    );
    // The measured prod shape: the manifest loads fine (it is complete), and the replay
    // fails on the parts whose only replica is not in this set. ⚠ The dangerous reading
    // would be an open that succeeds AND a replay that quietly returns a short log — a
    // silent partial recovery. Pin that it does not happen.
    assert!(
        pre_opened,
        "the remote's manifest is complete, so the open is expected to SUCCEED — that is \
         exactly why the bucket reads as a usable store"
    );
    assert!(
        pre_replay_err.is_some(),
        "the replay must FAIL rather than return a short log: a silent partial recovery \
         is the outcome there would be no way to notice"
    );
    assert_ne!(
        pre_recovered, expected,
        "BEFORE the backfill the remote must NOT reproduce history"
    );

    // --- Phase 3: the backfill ---
    let enqueued = attached.backfill_under_replicated().unwrap();
    assert_eq!(enqueued, 3, "the three pre-existing parts");
    attached.flush_and_wait_uploads().unwrap();
    attached.refresh_state_gauges();

    let m = attached.manifest_snapshot();
    for p in &m.parts {
        assert_eq!(p.replica_count(), 2, "part {} two-way", p.part_id);
        assert!(
            p.replicas.iter().any(|r| r.replica == "replica-1-gcs"),
            "part {} names the remote",
            p.part_id
        );
    }
    assert_eq!(attached.metrics().snapshot().parts_under_replicated, 0);
    assert_eq!(attached.metrics().snapshot().backfill_uploads, 3);

    // Counted from the bucket, not from the manifest the backfill just wrote.
    let remote_parts = remote.list_prefix("parts/d1_event_log/").unwrap();
    assert_eq!(
        remote_parts.len(),
        4,
        "every part is physically in the bucket"
    );
    drop(attached);

    // --- Phase 4: ⭐ cold-load from the remote ALONE, with the local store gone ---
    std::fs::remove_dir_all(&local_objects).unwrap();
    let cold_root = unique_dir("bf-cold-post");
    let revived = L0EventLogEngine::cold_load_replicated(
        cfg(&cold_root),
        vec![ReplicaTarget::new("replica-1-gcs", remote.clone())],
    )
    .expect("the remote alone can serve a cold load after the backfill");
    let got: Vec<String> = revived
        .replay_all()
        .unwrap()
        .into_iter()
        .map(|r| r.payload)
        .collect();
    assert_eq!(
        got, expected,
        "all 32 records — including the 24 written before the replica existed"
    );
}

/// Build a store whose records are deliberately left **unsealed**, replicating
/// the tail iff `tail`. Returns (local_root, prefix, expected_tail_payloads).
///
/// The sealed part exists only so a durable manifest does — a cold load needs
/// one. The records under test are the ones that never sealed.
fn tail_scenario(
    bucket: &str,
    tag: &str,
    tail: bool,
) -> (std::path::PathBuf, String, Vec<String>, Vec<String>) {
    let prefix = format!("{tag}-{}", std::process::id());
    let local_objects = unique_dir(&format!("{tag}-objects"));
    let origin_root = unique_dir(&format!("{tag}-origin"));

    let local: Arc<dyn DurableSubstrate> =
        Arc::new(LocalFsSubstrate::new(&local_objects).unwrap());
    let remote = gcs(bucket, &prefix);

    let mut c = cfg(&origin_root);
    if tail {
        c = c.with_tail_replication(true);
    }
    let mut origin = L0EventLogEngine::open_replicated(
        c,
        vec![
            ReplicaTarget::new("replica-0", local.clone()),
            ReplicaTarget::new("replica-1-gcs", remote.clone()),
        ],
    )
    .unwrap();

    // 8 records seal exactly one part, so a durable manifest exists.
    let mut sealed = Vec::new();
    for i in 0..8u64 {
        origin
            .append("1001", &format!("s{i}"), format!("sealed-{i}"))
            .unwrap();
        sealed.push(format!("sealed-{i}"));
    }
    origin.flush_and_wait_uploads().unwrap();
    assert_eq!(origin.manifest_snapshot().parts.len(), 1);

    // These must NOT seal — they are the subject.
    let mut tail_payloads = Vec::new();
    for i in 0..5u64 {
        origin
            .append("1001", &format!("u{i}"), format!("unsealed-{i}"))
            .unwrap();
        tail_payloads.push(format!("unsealed-{i}"));
    }
    assert_eq!(
        origin.manifest_snapshot().parts.len(),
        1,
        "the 5 must still be unsealed, or this proves nothing about the tail"
    );

    if tail {
        let rep = origin.replicate_tail().unwrap();
        assert_eq!(rep.records, 5, "all five unsealed records replicated");
        assert!(rep.failed_shards.is_empty());
    }
    drop(origin);

    // Simulate losing the node's disk entirely.
    std::fs::remove_dir_all(&local_objects).unwrap();
    (origin_root, prefix, sealed, tail_payloads)
}

/// Recover from the GCS replica alone and return the payloads, in order.
fn recover_from_gcs_alone(bucket: &str, prefix: &str, tag: &str) -> Vec<String> {
    let fresh = unique_dir(tag);
    let engine = L0EventLogEngine::cold_load_replicated(
        cfg(&fresh),
        vec![ReplicaTarget::new("replica-1-gcs", gcs(bucket, prefix))],
    )
    .expect("the GCS replica alone can serve a cold load");
    engine
        .replay_all()
        .unwrap()
        .into_iter()
        .map(|r| r.payload)
        .collect()
}

/// ⚠⚠ **B3 RED control, against real GCS.** With tail replication off, records
/// in an unsealed part are gone when the node's disk is lost — even though the
/// remote holds the sealed part and a complete manifest.
///
/// Without this, the GREEN below is unfalsifiable: a cold load that happened to
/// find the records some other way would look identical.
#[test]
#[ignore = "needs fake-gcs-server on 127.0.0.1:4443"]
fn without_tail_replication_unsealed_records_do_not_reach_gcs() {
    let (_root, prefix, sealed, tail_payloads) =
        tail_scenario("ehdb-tail-red", "b3red", false);

    // Nothing under `tail/`, counted from the bucket itself.
    let objects = gcs("ehdb-tail-red", &prefix)
        .list_prefix("tail/")
        .unwrap();
    assert!(
        objects.is_empty(),
        "tail objects exist with the flag OFF: {objects:?}"
    );

    let got = recover_from_gcs_alone("ehdb-tail-red", &prefix, "b3red-cold");
    assert_eq!(got, sealed, "only the sealed part comes back");
    for p in &tail_payloads {
        assert!(
            !got.contains(p),
            "{p} survived with replication OFF — the loss B3 prevents is not real here"
        );
    }
}

/// ⭐⭐ **The B3 acceptance proof: unsealed-tail records cold-load from the GCS
/// replica alone**, with the node's local store deleted.
#[test]
#[ignore = "needs fake-gcs-server on 127.0.0.1:4443"]
fn the_unsealed_tail_reaches_gcs_and_cold_loads_from_it_alone() {
    let (_root, prefix, sealed, tail_payloads) = tail_scenario("ehdb-tail", "b3green", true);

    // Byte-level proof from the bucket, not from a gauge: the tail object is
    // physically there, under `tail/` and NOT under `parts/`.
    let remote = gcs("ehdb-tail", &prefix);
    let tails = remote.list_prefix("tail/").unwrap();
    assert_eq!(tails.len(), 1, "one tail object in GCS: {tails:?}");
    assert!(tails[0].contains("/shard-0/"), "tail key shape: {tails:?}");
    let parts = remote.list_prefix("parts/").unwrap();
    assert_eq!(parts.len(), 1, "the one sealed part, with no tail among it");

    let got = recover_from_gcs_alone("ehdb-tail", &prefix, "b3green-cold");

    let mut expected = sealed.clone();
    expected.extend(tail_payloads.clone());
    assert_eq!(
        got, expected,
        "all 13 records — the 8 sealed AND the 5 that never sealed, recovered from GCS \
         with the node's disk gone"
    );
}
