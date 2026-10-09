//! [noetl/ai-meta#460](https://github.com/noetl/ai-meta/issues/460) A1 — make the RF=1
//! reality evaluate, so a 0 means *checked and fine* rather than *never checked*.
//!
//! The property under test is the one prod violates today: both copies on one device while
//! `replica_domain_violations` reads 0, because the engine's check is gated on
//! `replicas.len() >= 2` and short-circuits at RF=1.

use ehdb_l0::failure_domain::{FailureDomain, ReplicaDomain};
use noetl_server::services::replica_reality as rr;

/// ⚠⚠ These tests mutate **process-global** Prometheus gauges, and `cargo test` runs the
/// tests in one binary **in parallel**. Without this lock they race: one test seeds the
/// series while another asserts an exact value, and the result is a failure that depends on
/// timing — it passed locally and failed in CI, which is the worst version of the bug.
///
/// The repo already carries this lesson for `env::set_var` ("cargo test does not serialise
/// tests"); a shared metric registry is the same hazard wearing different clothes.
static GAUGE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take the lock, tolerating a poisoned mutex so one failing test does not cascade into
/// every other test in the file reporting a lock error instead of its own verdict.
fn serialised() -> std::sync::MutexGuard<'static, ()> {
    match GAUGE_LOCK.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn local(dev: u64, root: &str) -> ReplicaDomain {
    ReplicaDomain {
        replica: format!("r-{dev}-{root}"),
        domain: FailureDomain::LocalDevice {
            device_id: dev,
            root: root.into(),
        },
        root: Some(root.into()),
    }
}

fn remote(bucket: &str) -> ReplicaDomain {
    ReplicaDomain {
        replica: format!("r-{bucket}"),
        domain: FailureDomain::Remote {
            provider: "gcs".into(),
            bucket: bucket.into(),
        },
        root: None,
    }
}

/// ⭐⭐ The RF=1 case the server actually runs: evaluated, and honest about it.
#[test]
fn a_single_replica_is_evaluated_and_reported_as_a_single_point_of_failure() {
    let r = rr::evaluate(&[local(66320, "/data/ehdb-embedded/substrate")]);
    assert_eq!(r.replica_set_size, 1);
    assert!(!r.survives_node_loss, "a local device cannot survive node loss");
    assert!(
        r.is_single_point_of_failure(),
        "RF=1 on local storage MUST report as a single point of failure — a 0 here is the \
         defect this module exists to remove"
    );
    // ⚠ And the vacuous half is labelled as vacuous rather than presented as reassurance.
    assert!(r.domains_distinct, "one replica trivially does not collide with itself");
    assert!(
        r.describe().contains("vacuous at RF=1"),
        "the distinctness answer must be marked vacuous at RF=1: {}",
        r.describe()
    );
}

/// ⚠⚠⚠ PROD'S EXACT SHAPE: two replicas, one device. The count says 2; the guarantee is 1.
#[test]
fn two_replicas_on_one_device_are_not_two_failure_domains() {
    let r = rr::evaluate(&[
        local(66320, "/data/ehdb-embedded/substrate"),
        local(66320, "/data/ehdb-embedded/local"),
    ]);
    assert_eq!(r.replica_set_size, 2);
    assert!(
        !r.domains_distinct,
        "two replicas on device 66320 must NOT read as distinct domains"
    );
    assert!(
        !r.survives_node_loss,
        "⚠⚠ this is prod: RF reads 2 and the store still dies with the node"
    );
    assert!(r.is_single_point_of_failure());
    // ⭐ And the alert is NOT keyed on the count — which is the whole point.
    assert_eq!(
        r.replica_set_size, 2,
        "an RF<2 alert would have missed this configuration entirely"
    );
}

/// Two local devices are distinct domains and STILL die with the node — the distinction
/// `survives_node_loss` exists to draw.
#[test]
fn two_distinct_local_devices_still_do_not_survive_node_loss() {
    let r = rr::evaluate(&[local(1, "/a"), local(2, "/b")]);
    assert!(r.domains_distinct, "different devices are different domains");
    assert!(
        !r.survives_node_loss,
        "distinct devices on one node still both die with the node"
    );
    assert!(r.is_single_point_of_failure());
}

/// ⭐ The positive control: with an off-node domain the verdict flips. Without this, every
/// assertion above would also pass against a function that always reports failure.
#[test]
fn a_remote_domain_makes_the_set_survive_node_loss() {
    let r = rr::evaluate(&[local(66320, "/data"), remote("ehdb-archive")]);
    assert_eq!(r.replica_set_size, 2);
    assert!(r.domains_distinct);
    assert!(
        r.survives_node_loss,
        "a Remote domain is exactly what makes node-loss survival possible"
    );
    assert!(
        !r.is_single_point_of_failure(),
        "the alert must clear when the risk is actually gone"
    );
    // And it works even at RF=1 if that one replica is remote.
    let solo_remote = rr::evaluate(&[remote("b")]);
    assert!(solo_remote.survives_node_loss);
}

/// An empty set must not read as safe.
#[test]
fn an_empty_replica_set_is_not_safe() {
    let r = rr::evaluate(&[]);
    assert_eq!(r.replica_set_size, 0);
    assert!(!r.survives_node_loss);
    assert!(
        r.is_single_point_of_failure(),
        "no replicas must never read as survivable"
    );
}

/// ⚠ `Undeclared` and `Ephemeral` must not count as off-node. Silence is not independence.
#[test]
fn undeclared_and_ephemeral_domains_do_not_count_as_off_node() {
    for d in [
        FailureDomain::Undeclared,
        FailureDomain::Ephemeral {
            instance: "pod-1".into(),
        },
    ] {
        let r = rr::evaluate(&[ReplicaDomain {
            replica: "x".into(),
            domain: d.clone(),
            root: None,
        }]);
        assert!(
            !r.survives_node_loss,
            "{d:?} must not count as node-independent — silence is not independence"
        );
    }
}

/// Domain labels must distinguish the cases an operator needs to tell apart at a glance.
#[test]
fn the_domain_labels_are_distinct_and_carry_the_device() {
    let a = rr::domain_label(&FailureDomain::LocalDevice {
        device_id: 66320,
        root: "/data".into(),
    });
    let b = rr::domain_label(&FailureDomain::Remote {
        provider: "gcs".into(),
        bucket: "bk".into(),
    });
    let c = rr::domain_label(&FailureDomain::Ephemeral {
        instance: "i".into(),
    });
    let d = rr::domain_label(&FailureDomain::Undeclared);
    assert!(a.contains("66320"), "the device id is the thing that collides: {a}");
    for (i, x) in [&a, &b, &c, &d].iter().enumerate() {
        for y in [&a, &b, &c, &d].iter().skip(i + 1) {
            assert_ne!(x, y);
        }
    }
}

/// ⭐⭐ The series must be present and PESSIMISTIC before anything evaluates them.
///
/// ⚠⚠ Seeded to `survives_node_loss=0` / `single_point_of_failure=1`, not to 0 across the
/// board: until something has evaluated the replica set, "we do not know" must not read as
/// "safe". Seeding the alert gauge to 0 would reproduce the exact defect — a green signal
/// nobody computed.
#[test]
fn the_series_are_pinned_pessimistically_before_evaluation() {
    let _g = serialised();
    noetl_server::metrics::init_replica_reality_series();
    let text = noetl_server::metrics::gather_text().expect("render /metrics");
    for g in [
        "noetl_ehdb_replica_set_size",
        "noetl_ehdb_survives_node_loss",
        "noetl_ehdb_replica_domains_distinct",
        "noetl_ehdb_replica_single_point_of_failure",
        // ⚠ The put HISTOGRAM is deliberately not in this list. It used to be, pinned by
        // `observe(0.0)` — which is a fabricated sample, not a pin. See
        // `the_put_pin_does_not_fabricate_a_sample` below. The counter is what carries the
        // absent-vs-zero signal here.
        "noetl_object_store_put_total",
    ] {
        assert!(
            text.lines().any(|l| l.starts_with(g)),
            "{g} is ABSENT — and absence is exactly the state this work removes"
        );
    }
    assert_eq!(
        noetl_server::metrics::ehdb_replica_single_point_of_failure().get(),
        1,
        "the pre-evaluation value must be the PESSIMISTIC one: an unevaluated replica set \
         must not read as safe"
    );
    assert_eq!(noetl_server::metrics::ehdb_survives_node_loss().get(), 0);

    // And publishing a real evaluation overwrites it in both directions.
    rr::publish(&rr::evaluate(&[local(66320, "/data"), remote("b")]));
    assert_eq!(noetl_server::metrics::ehdb_survives_node_loss().get(), 1);
    assert_eq!(
        noetl_server::metrics::ehdb_replica_single_point_of_failure().get(),
        0
    );
    rr::publish(&rr::evaluate(&[local(66320, "/data")]));
    assert_eq!(
        noetl_server::metrics::ehdb_replica_single_point_of_failure().get(),
        1,
        "and back again — a stale green must not survive a worsening configuration"
    );
}

/// ⚠ The put histogram must label failures separately: a latency distribution over
/// successes only hides the case where the slow puts are the ones that fail.
#[test]
fn the_put_histogram_separates_outcomes() {
    let _g = serialised();
    noetl_server::metrics::init_replica_reality_series();
    noetl_server::metrics::observe_object_store_put("gcs", "ok", 0.012);
    noetl_server::metrics::observe_object_store_put("gcs", "failed", 3.5);
    let text = noetl_server::metrics::gather_text().unwrap();
    assert!(text.contains(r#"backend="gcs",outcome="ok""#), "missing ok series");
    assert!(
        text.contains(r#"backend="gcs",outcome="failed""#),
        "missing failed series"
    );
    // Buckets must span the range the question needs: ms to seconds.
    assert!(text.contains("le=\"0.001\""), "no 1ms bucket — cannot see a fast local put");
    assert!(text.contains("le=\"8\""), "no 8s bucket — a clipped top answers with its own ceiling");
}

/// ⚠⚠ A histogram must never be "pinned" by observing into it.
///
/// `init_replica_reality_series` used to call `.observe(0.0)` per label. `inc_by(0)` on a
/// counter is harmless; `observe(0.0)` on a histogram records a 0-second measurement that
/// was never taken. The damage lands on the one number the metric exists for: #460 is
/// deciding whether a remote put costs "tens of ms" against a ~4 ms local fsync, and a
/// planted zero pulls the low quantiles under the real floor — worst when n is small, which
/// is exactly when the decision gets made.
///
/// Measured as a delta rather than asserted by absence: these tests share a process-wide
/// registry, so another test may legitimately have made the family present already.
#[test]
fn the_put_pin_does_not_fabricate_a_sample() {
    let _g = serialised();

    let count_of = |text: &str| -> u64 {
        text.lines()
            .filter(|l| l.starts_with("noetl_object_store_put_seconds_count"))
            .filter_map(|l| l.rsplit(' ').next())
            .filter_map(|v| v.parse::<f64>().ok())
            .sum::<f64>() as u64
    };

    let before = count_of(&noetl_server::metrics::gather_text().unwrap());
    noetl_server::metrics::init_replica_reality_series();
    let after_text = noetl_server::metrics::gather_text().unwrap();
    assert_eq!(
        count_of(&after_text),
        before,
        "initialising the series added {} histogram observation(s) — a pin must not invent \
         measurements, because the resulting distribution is then wrong in the direction \
         that looks fast",
        count_of(&after_text).saturating_sub(before)
    );

    // And the counter IS pinned, so "no put has happened" stays readable at zero.
    assert!(
        after_text
            .lines()
            .any(|l| l.starts_with("noetl_object_store_put_total")),
        "the attempt counter must be present at zero — it is what makes the histogram's \
         absence interpretable without fabricating a sample"
    );
}

/// Positive control: a real put DOES move both.
#[test]
fn a_real_put_moves_the_counter_and_the_histogram() {
    let _g = serialised();
    noetl_server::metrics::init_replica_reality_series();
    let c0 = noetl_server::metrics::object_store_put_total()
        .with_label_values(&["gcs", "ok"])
        .get();
    noetl_server::metrics::observe_object_store_put("gcs", "ok", 0.042);
    let c1 = noetl_server::metrics::object_store_put_total()
        .with_label_values(&["gcs", "ok"])
        .get();
    assert_eq!(c1, c0 + 1, "the attempt counter must track real puts");
    let text = noetl_server::metrics::gather_text().unwrap();
    assert!(
        text.contains("noetl_object_store_put_seconds_count"),
        "a real observation must make the histogram present"
    );
}
