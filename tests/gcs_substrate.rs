//! The off-box replica substrate — [noetl/ai-meta#460](https://github.com/noetl/ai-meta/issues/460) A2.
//!
//! ⚠⚠ What these tests establish is narrow and worth stating precisely, because the
//! overstated version of this claim is dangerous:
//!
//! - configuring a GCS replica makes the engine's replica set span two failure domains, so
//!   `survives_node_loss` becomes **true for sealed parts**;
//! - the unsealed tail is still RF=1, and replication is asynchronous, so recovery is a
//!   **bounded-loss consistent prefix** — NOT "no data loss".
//!
//! Byte-level round-trip I/O is proved against an emulator on kind, not here: these tests
//! construct the substrate (which performs no network I/O) and exercise the domain algebra
//! and key mapping, which is where the silent-wrong answers live.

use std::sync::Arc;

use ehdb_l0::failure_domain::FailureDomain;
use ehdb_l0::DurableSubstrate;
use noetl_server::services::gcs_substrate::{normalise_prefix, GcsSubstrate};
use noetl_server::services::replica_reality as rr;

/// A substrate with an explicit local device, standing in for the PVC replica.
#[derive(Debug)]
struct LocalDevice(u64);
impl DurableSubstrate for LocalDevice {
    fn failure_domain(&self) -> FailureDomain {
        FailureDomain::LocalDevice {
            device_id: self.0,
            root: "/data/ehdb".into(),
        }
    }
    fn put_if_absent(&self, _k: &str, _b: &[u8]) -> ehdb_core::Result<bool> {
        unimplemented!("domain algebra only")
    }
    fn put_overwrite(&self, _k: &str, _b: &[u8]) -> ehdb_core::Result<()> {
        unimplemented!()
    }
    fn get_range(&self, _k: &str, _o: u64, _l: u64) -> ehdb_core::Result<Vec<u8>> {
        unimplemented!()
    }
    fn get_all(&self, _k: &str) -> ehdb_core::Result<Vec<u8>> {
        unimplemented!()
    }
    fn exists(&self, _k: &str) -> ehdb_core::Result<bool> {
        unimplemented!()
    }
    fn list_prefix(&self, _p: &str) -> ehdb_core::Result<Vec<String>> {
        unimplemented!()
    }
    fn delete(&self, _k: &str) -> ehdb_core::Result<()> {
        unimplemented!()
    }
}

/// Construction performs no network I/O, so a non-Google endpoint is enough to build one.
fn gcs(bucket: &str) -> GcsSubstrate {
    GcsSubstrate::open("http://127.0.0.1:4443", bucket, "ehdb").expect("construct")
}

#[test]
fn the_substrate_declares_a_remote_domain_with_its_bucket() {
    let s = gcs("replica-bucket");
    match s.failure_domain() {
        FailureDomain::Remote { provider, bucket } => {
            assert_eq!(provider, "gcs");
            assert_eq!(bucket, "replica-bucket");
        }
        other => panic!(
            "must declare Remote — Undeclared is REFUSED by the replica-domain check, and \
             anything else would make an off-box copy read as on-node: {other:?}"
        ),
    }
}

/// ⭐ The property A2 exists for.
#[test]
fn a_gcs_replica_makes_the_set_survive_node_loss() {
    let local = LocalDevice(66320);
    let remote = gcs("replica-bucket");

    // RF=1, prod's current state.
    let before = rr::evaluate_and_publish_set(&[("replica-0", &local as &dyn DurableSubstrate)]);
    assert!(
        before.is_single_point_of_failure(),
        "a lone local replica must report as a single point of failure: {}",
        before.describe()
    );
    assert!(!before.survives_node_loss);

    // RF=2 across domains.
    let after = rr::evaluate_and_publish_set(&[
        ("replica-0", &local as &dyn DurableSubstrate),
        ("replica-1-gcs", &remote as &dyn DurableSubstrate),
    ]);
    assert!(
        after.survives_node_loss,
        "a local + remote set must survive node loss: {}",
        after.describe()
    );
    assert!(after.domains_distinct);
    assert!(!after.is_single_point_of_failure());
    assert_eq!(after.replica_set_size, 2);

    // And the gauges moved, not just the struct.
    assert_eq!(noetl_server::metrics::ehdb_survives_node_loss().get(), 1);
    assert_eq!(
        noetl_server::metrics::ehdb_replica_single_point_of_failure().get(),
        0
    );
}

/// ⚠⚠ Two GCS buckets are ONE failure domain — but they DO survive node loss.
///
/// These two facts are independent, and the first draft of this test conflated them: it
/// asserted that two buckets are "still a single point of failure", which is false.
///
/// - `survives_node_loss` asks whether any copy is off **this node**. Two remote buckets
///   both are, so losing the node loses neither — and `is_single_point_of_failure()` is
///   defined as exactly `!survives_node_loss`, so it is correctly false.
/// - `domains_distinct` asks whether the replicas fail **independently**.
///   `FailureDomain::label()` collapses `Remote` to its provider and ignores the bucket, so
///   two GCS buckets are one domain: a provider-wide outage takes both.
///
/// So the gauge that would catch a "spread across two buckets" plan is `domains_distinct`,
/// NOT the single-point-of-failure one. Writing that down is the point of this test, because
/// reading redundancy off the wrong gauge is how a correlated replica set reads as safe.
#[test]
fn two_gcs_buckets_survive_node_loss_but_are_not_independent() {
    let a = gcs("bucket-a");
    let b = gcs("bucket-b");
    let r = rr::evaluate_and_publish_set(&[
        ("replica-0", &a as &dyn DurableSubstrate),
        ("replica-1", &b as &dyn DurableSubstrate),
    ]);
    assert!(
        !r.domains_distinct,
        "two GCS buckets must NOT count as independent domains: {}",
        r.describe()
    );
    assert!(
        r.survives_node_loss,
        "but they are both off-node, so node loss is survived: {}",
        r.describe()
    );
    // Stated explicitly so the asymmetry cannot be mistaken for an oversight.
    assert!(!r.is_single_point_of_failure());
}

/// The key mapping. A prefix that silently lost or doubled its separator would write to a
/// different path than the operator configured — and because reads use the same function,
/// the mismatch is invisible from inside the process: writes and reads would agree with each
/// other while disagreeing with the bucket.
#[test]
fn the_prefix_is_normalised_to_exactly_one_separator() {
    assert_eq!(normalise_prefix(""), "");
    assert_eq!(normalise_prefix("   "), "");
    assert_eq!(normalise_prefix("/"), "");
    assert_eq!(normalise_prefix("ehdb"), "ehdb/");
    assert_eq!(normalise_prefix("/ehdb"), "ehdb/");
    assert_eq!(normalise_prefix("ehdb/"), "ehdb/");
    assert_eq!(normalise_prefix("/ehdb/"), "ehdb/");
    assert_eq!(normalise_prefix("a/b"), "a/b/");
}

/// An empty bucket is refused at construction rather than producing a substrate that writes
/// to `https://storage.googleapis.com/upload/storage/v1/b//o` and fails per-object later.
#[test]
fn an_empty_bucket_is_refused() {
    assert!(GcsSubstrate::open("http://127.0.0.1:4443", "", "p").is_err());
    assert!(GcsSubstrate::open("http://127.0.0.1:4443", "   ", "p").is_err());
}

/// A zero-length range must not become a malformed `bytes=o-(o-1)` header.
///
/// ⚠ This is the one I/O-shaped case testable without a server: it must answer WITHOUT a
/// request, so it returns cleanly against an endpoint where nothing is listening.
#[test]
fn a_zero_length_range_is_answered_without_a_request() {
    let s = gcs("b");
    let got = s.get_range("any/key", 100, 0).expect("len 0 must not error");
    assert!(got.is_empty());
}

/// And the negative control for the test above: a NON-zero range against the same dead
/// endpoint must fail. Without this, the zero-length test passes for the wrong reason — a
/// substrate that answered every range with an empty vec would satisfy it.
#[test]
fn a_nonzero_range_against_a_dead_endpoint_fails() {
    let s = gcs("b");
    let r = s.get_range("any/key", 0, 8);
    assert!(
        r.is_err(),
        "a real range request to a dead endpoint must error, not return empty bytes"
    );
}

/// The substrate is usable as `Arc<dyn DurableSubstrate>`, which is what `ReplicaTarget`
/// takes. A compile-level check, but the injection does not exist without it.
#[test]
fn it_is_object_safe_as_a_replica_target() {
    let s: Arc<dyn DurableSubstrate> = Arc::new(gcs("b"));
    let t = ehdb_l0::ReplicaTarget::new("replica-1-gcs", s);
    assert_eq!(t.id, "replica-1-gcs");
}
