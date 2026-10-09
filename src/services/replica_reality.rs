//! Does the replica set actually spread risk? — [noetl/ai-meta#460](https://github.com/noetl/ai-meta/issues/460) A1.
//!
//! ## ⚠⚠ Why this exists: a green 0 that was never evaluated
//!
//! Measured on prod 2026-10-09:
//!
//! ```text
//! /data/ehdb-embedded/substrate  -> device 66320   }  the SAME device,
//! /data/ehdb-embedded/local      -> device 66320   }  /dev/nvme0n5
//!
//! ehdb_l0_replica_domain_violations 0
//! ehdb_l0_parts_under_replicated    0
//! ehdb_l0_parts_local_only          0
//! ```
//!
//! Every durability signal reads healthy for a configuration that cannot survive losing the
//! node. The reason is not a bug in those metrics: `L0Engine::open` wraps the substrate in
//! `vec![ReplicaTarget::new("replica-0", substrate)]` — a replica set of **exactly one** —
//! and the failure-domain check is gated on `if replicas.len() >= 2`, because "one replica
//! makes no spreading claim". So the check short-circuits and the healthy path pins the
//! counter at 0.
//!
//! **The 0 is therefore not a verdict. It is an unevaluated pin**, and it is
//! indistinguishable from "checked and fine" — which is the whole defect.
//!
//! ⭐ This module makes the RF=1 reality **evaluate**. It reports the replica-set size and
//! whether the set survives node loss as values that are always computed, so a 0 means
//! *checked and fine* and an RF=1 deployment is visible and alertable rather than silently
//! green.
//!
//! ⚠ It changes **nothing** about durability. It is an instrument. The work that would
//! actually give RF>1 is #460 A2/A3 and is owner-gated.

use ehdb_l0::failure_domain::{survives_node_loss, FailureDomain, ReplicaDomain};
use ehdb_l0::substrate::DurableSubstrate;

/// What a replica set actually guarantees, as opposed to what its count suggests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaReality {
    /// How many replicas the engine was opened with.
    pub replica_set_size: usize,
    /// Can the set survive losing the writer's node? Needs at least one `Remote` domain.
    pub survives_node_loss: bool,
    /// Do the replicas resolve to distinct failure domains?
    ///
    /// ⚠ `true` at RF=1 is **vacuous** — one replica trivially does not collide with
    /// itself. That is exactly why this is reported next to `replica_set_size` and never
    /// alone: "distinct domains" on a set of one is the reassuring half of the reading
    /// that caused this module to exist.
    pub domains_distinct: bool,
    /// The domains, for the log line.
    pub domains: Vec<String>,
}

impl ReplicaReality {
    /// ⭐ The single number worth alerting on: a replica set that cannot survive node loss.
    ///
    /// Deliberately **not** "RF < 2". Two replicas on one device also cannot survive node
    /// loss, and the whole lesson of this metric is that a count is not a guarantee.
    pub fn is_single_point_of_failure(&self) -> bool {
        !self.survives_node_loss
    }

    pub fn describe(&self) -> String {
        format!(
            "replica_set_size={} survives_node_loss={} domains_distinct={}{} domains=[{}]",
            self.replica_set_size,
            self.survives_node_loss,
            self.domains_distinct,
            if self.replica_set_size < 2 {
                " (vacuous at RF=1)"
            } else {
                ""
            },
            self.domains.join(", ")
        )
    }
}

/// A short label for a domain, **for display only**.
///
/// ⚠ Richer than `FailureDomain::label()` on purpose — it carries the bucket, which an
/// operator wants to see — and therefore **must not be used for collision detection**.
/// `label()` is the authoritative equality, and the two differ: `label()` treats two GCS
/// buckets as one domain and this does not.
pub fn domain_label(d: &FailureDomain) -> String {
    match d {
        FailureDomain::LocalDevice { device_id, .. } => format!("local-device:{device_id}"),
        FailureDomain::Remote { provider, bucket } => format!("remote:{provider}/{bucket}"),
        FailureDomain::Ephemeral { instance } => format!("ephemeral:{instance}"),
        FailureDomain::Undeclared => "undeclared".to_string(),
    }
}

/// Evaluate a replica set — **at any size, including one.**
///
/// ⚠⚠ The `len() >= 2` short-circuit in the engine is correct *for its own question*
/// ("do these collide?"), and wrong as the only signal, because the answer for a set of
/// one is "no collision" rather than "no risk". This asks the different question that an
/// operator actually needs answered: *can this survive losing the node?*
pub fn evaluate(domains: &[ReplicaDomain]) -> ReplicaReality {
    let labels: Vec<String> = domains.iter().map(|r| domain_label(&r.domain)).collect();
    // ⚠⚠ Distinctness is decided by `FailureDomain::label()` — ehdb's OWN definition of
    // "same domain" — not by the struct's derived `Eq`. The first cut used `Eq`, which
    // includes the `root` path, so two replicas on device 66320 with roots
    // `/data/.../substrate` and `/data/.../local` compared as DISTINCT. That is exactly
    // prod's shape, and it would have reported a reassuring "distinct domains" for two
    // copies that die together. The test caught it.
    //
    // `label()` collapses `LocalDevice` to its device id and ignores the root, which is
    // the correct collision rule — and note it also collapses `Remote` to its PROVIDER,
    // ignoring the bucket: two GCS buckets are ONE domain by this definition, because
    // their failures correlate. Any future "spread across two buckets" plan has to
    // reckon with that rather than assume two buckets are two domains.
    let mut seen = std::collections::HashSet::new();
    let mut distinct = true;
    for r in domains {
        if !seen.insert(r.domain.label()) {
            distinct = false;
        }
    }
    ReplicaReality {
        replica_set_size: domains.len(),
        survives_node_loss: survives_node_loss(domains),
        domains_distinct: distinct,
        domains: labels,
    }
}

/// Evaluate the single-substrate case the server actually runs, and publish it.
///
/// The server calls `L0Engine::open(config, substrate)`, which is RF=1 by construction. So
/// this is handed that one substrate and reports the truth about it rather than inheriting
/// the engine's "no collision" answer.
pub fn evaluate_and_publish(substrate: &dyn DurableSubstrate, replica: &str) -> ReplicaReality {
    let domain = substrate.failure_domain();
    let reality = evaluate(&[ReplicaDomain {
        replica: replica.to_string(),
        domain,
        root: None,
    }]);
    publish(&reality);
    reality
}

/// Evaluate and publish for a **replica set**, not one replica.
///
/// noetl/ai-meta#460 A2. The single-substrate form above was written when the engine was
/// opened with exactly one replica; once a second (GCS) replica can be configured, reporting
/// only `replica-0` would leave the gauges saying RF=1 on a store that now has two copies —
/// the drift this instrument exists to prevent, pointed the other way.
pub fn evaluate_and_publish_set(replicas: &[(&str, &dyn DurableSubstrate)]) -> ReplicaReality {
    let domains: Vec<ReplicaDomain> = replicas
        .iter()
        .map(|(id, s)| ReplicaDomain {
            replica: (*id).to_string(),
            domain: s.failure_domain(),
            root: None,
        })
        .collect();
    let reality = evaluate(&domains);
    publish(&reality);
    reality
}

/// Publish a reality to the gauges.
pub fn publish(r: &ReplicaReality) {
    crate::metrics::ehdb_replica_set_size().set(r.replica_set_size as i64);
    crate::metrics::ehdb_survives_node_loss().set(i64::from(r.survives_node_loss));
    crate::metrics::ehdb_replica_domains_distinct().set(i64::from(r.domains_distinct));
    crate::metrics::ehdb_replica_single_point_of_failure()
        .set(i64::from(r.is_single_point_of_failure()));
}
