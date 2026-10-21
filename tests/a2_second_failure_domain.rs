//! A2 reaches a GENUINE second failure domain — [noetl/ai-meta#460](https://github.com/noetl/ai-meta/issues/460).
//!
//! ⚠⚠ The claim being tested is narrow and the easy mistake is to prove the wrong thing.
//! "Two replicas" is not redundancy. Prod already had two copies of every part and was still
//! a single point of failure, because both lived on the same PVC — `local` 1,240,316 KiB and
//! `substrate` 1,295,824 KiB on one disk, which the 2026-10-10 prune freed *both* halves of.
//!
//! So these tests use the REAL substrates (`LocalFsSubstrate` + `GcsSubstrate`), not stubs,
//! and carry a negative control: two local substrates on the same device must be reported as
//! **one** domain and still a single point of failure. Without that control, a test that
//! simply counts to two passes on exactly the configuration prod already had.
//!
//! `#[ignore]` by default: needs a `fake-gcs-server` on 127.0.0.1:4443. See
//! `gcs_substrate_roundtrip.rs` for the run command.

use std::sync::Arc;

use ehdb_l0::failure_domain::FailureDomain;
use ehdb_l0::substrate::DurableSubstrate;
use ehdb_l0::{L0Config, L0EventLogEngine, LocalFsSubstrate, ReplicaTarget};
use noetl_server::services::gcs_substrate::GcsSubstrate;
use noetl_server::services::replica_reality as rr;

const ENDPOINT: &str = "http://127.0.0.1:4443";

fn unique_dir(tag: &str) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("ehdb-a2d-{tag}-{}-{n}", std::process::id()))
}

/// ⚠⚠⚠ The negative control, and the most important test here.
///
/// This is EXACTLY prod's shape before A2: two durable copies, one disk. If the domain
/// algebra cannot tell this apart from real redundancy, then every "RF=2" claim is
/// meaningless.
#[test]
fn two_local_substrates_on_one_device_are_ONE_domain_and_still_a_spof() {
    let a = unique_dir("same-dev-a");
    let b = unique_dir("same-dev-b");
    let sa = LocalFsSubstrate::new(&a).unwrap();
    let sb = LocalFsSubstrate::new(&b).unwrap();

    // Different paths — and that is the trap. The device is what matters.
    assert_ne!(a, b);
    match (sa.failure_domain(), sb.failure_domain()) {
        (
            FailureDomain::LocalDevice { device_id: da, .. },
            FailureDomain::LocalDevice { device_id: db, .. },
        ) => assert_eq!(da, db, "fixture: both temp dirs must be on one device"),
        other => panic!("expected two LocalDevice domains, got {other:?}"),
    }

    let r = rr::evaluate_and_publish_set(&[
        ("replica-0", &sa as &dyn DurableSubstrate),
        ("replica-1", &sb as &dyn DurableSubstrate),
    ]);
    assert_eq!(r.replica_set_size, 2, "there really are two replicas");
    assert!(
        !r.domains_distinct,
        "⚠⚠⚠ two copies on ONE device were reported as distinct failure domains. That is \
         prod's pre-A2 shape, and calling it redundant is the drift this instrument exists \
         to catch: {}",
        r.describe()
    );
    assert!(
        !r.survives_node_loss,
        "two copies on one device cannot survive losing that node: {}",
        r.describe()
    );
    assert!(r.is_single_point_of_failure(), "{}", r.describe());

    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
}

/// ⭐ And the positive: local + GCS, with the REAL substrates, is two domains.
#[test]
#[ignore = "needs fake-gcs-server on 127.0.0.1:4443"]
fn local_plus_gcs_is_a_genuine_second_domain() {
    let local_dir = unique_dir("objects");
    let local = LocalFsSubstrate::new(&local_dir).unwrap();
    let remote =
        GcsSubstrate::open(ENDPOINT, "ehdb-replica", &format!("dom-{}", std::process::id()))
            .unwrap();

    // The declarations, read off the real implementations.
    assert!(
        matches!(local.failure_domain(), FailureDomain::LocalDevice { .. }),
        "LocalFsSubstrate must declare a LocalDevice domain"
    );
    match remote.failure_domain() {
        FailureDomain::Remote { provider, .. } => assert_eq!(provider, "gcs"),
        other => panic!("GcsSubstrate must declare Remote, got {other:?}"),
    }

    let r = rr::evaluate_and_publish_set(&[
        ("replica-0", &local as &dyn DurableSubstrate),
        ("replica-1-gcs", &remote as &dyn DurableSubstrate),
    ]);
    assert!(r.domains_distinct, "{}", r.describe());
    assert!(
        r.survives_node_loss,
        "a local + remote set must survive node loss: {}",
        r.describe()
    );
    assert!(!r.is_single_point_of_failure(), "{}", r.describe());
    // Asserted on the gauges too, not only the struct — the gauges are what an operator reads.
    assert_eq!(noetl_server::metrics::ehdb_survives_node_loss().get(), 1);
    assert_eq!(noetl_server::metrics::ehdb_replica_domains_distinct().get(), 1);
    assert_eq!(
        noetl_server::metrics::ehdb_replica_single_point_of_failure().get(),
        0
    );

    let _ = std::fs::remove_dir_all(&local_dir);
}

/// ⭐⭐ The whole thing through a REAL engine: RF=2 opens, sealed parts land in BOTH
/// substrates, and ehdb's own replica-domain validation accepts the set.
#[test]
#[ignore = "needs fake-gcs-server on 127.0.0.1:4443"]
fn an_rf2_engine_puts_sealed_parts_in_both_domains() {
    let prefix = format!("rf2-{}", std::process::id());
    let local_dir = unique_dir("objects");
    let origin_root = unique_dir("origin");

    let local: Arc<dyn DurableSubstrate> = Arc::new(LocalFsSubstrate::new(&local_dir).unwrap());
    let remote: Arc<dyn DurableSubstrate> =
        Arc::new(GcsSubstrate::open(ENDPOINT, "ehdb-replica", &prefix).unwrap());

    // ⚠ `open_replicated` runs ehdb's FORMAT_VERSION gate on EVERY replica and, at
    // len() >= 2, its replica-domain check. If this open succeeds, ehdb itself has accepted
    // the pair as independent — not just our own gauge.
    let cfg = L0Config::d1(&origin_root)
        .with_shard_count(1)
        .with_seal_max_records(8)
        .with_require_distinct_domains(true);
    let mut e = L0EventLogEngine::open_replicated(
        cfg,
        vec![
            ReplicaTarget::new("replica-0", local.clone()),
            ReplicaTarget::new("replica-1-gcs", remote.clone()),
        ],
    )
    .expect("RF=2 open with require_distinct_domains must be ACCEPTED for local+gcs");

    for i in 0..32u64 {
        e.append("9100", &format!("t{i}"), format!("payload-{i}")).unwrap();
    }
    e.flush_and_wait_uploads().unwrap();
    let expected = e.replay_all().unwrap();
    assert_eq!(expected.len(), 32, "fixture must hold records");

    // ⚠⚠ Assert the remote replica is actually REMOTE, here and not only in the test above.
    //
    // A RED control exposed this gap: falsifying `GcsSubstrate::failure_domain()` to a
    // `LocalDevice` left this test PASSING, because the fabricated device id differed from
    // the temp dir's and the pair still counted as "distinct". So "parts landed in both
    // substrates" and "the second substrate is off-node" are separate claims, and this test
    // was only making the first.
    //
    // ⚠ Note also that ehdb's `require_distinct_domains` compares DEVICE IDS, so two
    // different local disks satisfy it — distinct domains is a weaker property than
    // surviving node loss, which needs a `Remote`. Opening successfully is therefore not
    // evidence of off-node durability on its own.
    match remote.failure_domain() {
        FailureDomain::Remote { provider, .. } => assert_eq!(
            provider, "gcs",
            "the second replica must declare a Remote domain; a distinct LOCAL device would \
             satisfy the engine's open check while still dying with the node"
        ),
        other => panic!(
            "the second replica is not Remote ({other:?}) — the engine accepted the set, but \
             accepting it only proves the domains differ, not that either is off-node"
        ),
    }
    let reality = rr::evaluate_and_publish_set(&[
        ("replica-0", local.as_ref()),
        ("replica-1-gcs", remote.as_ref()),
    ]);
    assert!(
        reality.survives_node_loss,
        "the RF=2 engine's own replica set must survive node loss: {}",
        reality.describe()
    );

    // Sealed parts present in BOTH domains.
    let lp = local.list_prefix("parts/d1_event_log/").unwrap();
    let rp = remote.list_prefix("parts/d1_event_log/").unwrap();
    assert!(!lp.is_empty(), "no sealed parts in the LOCAL domain: {lp:?}");
    assert!(
        !rp.is_empty(),
        "⚠⚠ no sealed parts in the REMOTE domain. The engine opened RF=2 and reported two \
         replicas while writing to one — which is the claim this test exists to refuse: {rp:?}"
    );
    assert_eq!(
        lp.len(),
        rp.len(),
        "the two domains hold different part counts (local {} vs remote {}) — a partial \
         second copy is not a second copy",
        lp.len(),
        rp.len()
    );

    // And the manifest reports the parts as replicated to two places.
    let m = e.manifest_snapshot();
    assert!(!m.parts.is_empty());
    let min_replicas = m.parts.iter().map(|p| p.replicas.len()).min().unwrap_or(0);
    assert!(
        min_replicas >= 2,
        "a part records only {min_replicas} replica location(s); `survives_node_loss` is \
         decided from this list, so a part with one entry is not covered whatever the engine \
         was opened with"
    );

    drop(e);
    let _ = std::fs::remove_dir_all(&local_dir);
    let _ = std::fs::remove_dir_all(&origin_root);
}
