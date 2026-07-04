//! Deterministic owner-first delivery proof for server-routed shard publish
//! (noetl/ai-meta#166 Phase 5, leg 1).
//!
//! Phase 4 validated the *worker-side* NAK affinity with a deterministic
//! competing-consumer harness (`noetl/worker` `tests/affinity_multi_replica.rs`)
//! rather than a live 2-pod StatefulSet, because a real per-pod
//! `NOETL_SHARD_INDEX` is an ops change out of a code PR's scope. This test
//! mirrors that approach for the *server-side* publish: it models JetStream
//! subject-filter matching and drives the **real** `command_subject` /
//! `shard_for` to prove, without a cluster:
//!
//! 1. **Owner-first, zero misroute** — under per-shard consumer filters
//!    (`noetl.commands.system.shard.<i>.>`), every routed command matches
//!    **exactly one** consumer: the owner `shard_for(eid)`. No command lands on
//!    a non-owner (the redirect the Phase-4 shared consumer paid ≈1/drive at 2
//!    shards drops to 0).
//! 2. **Subject subsumption (degrade-to-NAK safety)** — every routed command
//!    *also* matches the broad pool filter `noetl.commands.system.>`, so a
//!    replica still on the broad filter (fleet mid-rollout / a shard with no
//!    live consumer) still receives it and falls through to the Phase-4 NAK
//!    path. A wrong route degrades to NAK, never drops a hop.
//! 3. **Legacy equivalence (flag off)** — with routing off, the subject is the
//!    exact `noetl.commands.system.<eid>` the broad filter serves and no
//!    per-shard filter matches, i.e. byte-identical to today.

use noetl_server::sharding::{command_subject, shard_for};

/// Minimal model of NATS subject-token matching for the two wildcard forms the
/// worker's `NATS_FILTER_SUBJECT` uses: a literal token, `*` (exactly one
/// token), and a trailing `>` (one-or-more remaining tokens). This is the
/// documented JetStream filter semantics — the property under test is that our
/// *subjects* land where our *filters* expect, not that async-nats implements
/// matching (it does).
fn subject_matches_filter(subject: &str, filter: &str) -> bool {
    let subj: Vec<&str> = subject.split('.').collect();
    let filt: Vec<&str> = filter.split('.').collect();
    let mut i = 0;
    while i < filt.len() {
        match filt[i] {
            ">" => return i < subj.len(), // `>` matches one-or-more remaining tokens
            _ if i >= subj.len() => return false,
            "*" => {}                     // matches exactly this one token
            tok if tok == subj[i] => {}   // literal token match
            _ => return false,
        }
        i += 1;
    }
    i == subj.len()
}

const N: u32 = 4; // model a 4-shard system pool
const POOL: &str = "system";

/// A representative spread of snowflake-shaped execution ids.
fn sample_eids() -> Vec<i64> {
    let base = 320_816_801_799_737_344_i64;
    (0..2_000).map(|i| base + i as i64).collect()
}

fn per_shard_filter(i: u32) -> String {
    format!("noetl.commands.{POOL}.shard.{i}.>")
}

fn broad_filter() -> String {
    format!("noetl.commands.{POOL}.>")
}

#[test]
fn routed_command_lands_on_exactly_the_owner_shard() {
    for eid in sample_eids() {
        let subject = command_subject(POOL, eid, true, N);
        let owner = shard_for(eid, N);

        // Exactly one per-shard consumer receives it, and it is the owner.
        let mut receivers: Vec<u32> = Vec::new();
        for i in 0..N {
            if subject_matches_filter(&subject, &per_shard_filter(i)) {
                receivers.push(i);
            }
        }
        assert_eq!(
            receivers,
            vec![owner],
            "eid {eid} subject {subject} must match exactly the owner shard {owner}"
        );
    }
}

#[test]
fn owner_first_means_zero_redirects_across_the_fleet() {
    // Aggregate proof: over the whole sample, the count of (command, non-owner
    // consumer) deliveries is 0. Under the Phase-4 shared consumer this count
    // was ≈ (fleet-1)/fleet × drives (the redirect tax); server-routing drives
    // it to exactly 0.
    let mut misroutes = 0usize;
    let mut owner_hits = 0usize;
    for eid in sample_eids() {
        let subject = command_subject(POOL, eid, true, N);
        let owner = shard_for(eid, N);
        for i in 0..N {
            if subject_matches_filter(&subject, &per_shard_filter(i)) {
                if i == owner {
                    owner_hits += 1;
                } else {
                    misroutes += 1;
                }
            }
        }
    }
    assert_eq!(misroutes, 0, "no command may land on a non-owner shard");
    assert_eq!(owner_hits, sample_eids().len(), "every command reaches its owner");
}

#[test]
fn routed_command_is_subsumed_by_the_broad_pool_filter() {
    // Degrade-to-NAK safety: a broad-filter replica still receives every
    // shard-routed command, so flipping server-routing on before the fleet
    // switches to per-shard filters is behaviour == Phase-4 (NAK steering),
    // never a dropped hop.
    for eid in sample_eids() {
        let subject = command_subject(POOL, eid, true, N);
        assert!(
            subject_matches_filter(&subject, &broad_filter()),
            "routed subject {subject} must still match the broad pool filter"
        );
    }
}

#[test]
fn legacy_subject_matches_broad_filter_and_no_per_shard_filter() {
    // Flag off → exact today's subject: broad filter serves it, no per-shard
    // filter matches (byte-identical to pre-Phase-5).
    for eid in sample_eids() {
        let subject = command_subject(POOL, eid, false, N);
        assert_eq!(subject, format!("noetl.commands.{POOL}.{eid}"));
        assert!(subject_matches_filter(&subject, &broad_filter()));
        for i in 0..N {
            assert!(
                !subject_matches_filter(&subject, &per_shard_filter(i)),
                "legacy subject {subject} must NOT match per-shard filter {i}"
            );
        }
    }
}

#[test]
fn non_system_pool_is_never_shard_routed() {
    // The shared/subscription pools stay on the legacy subject even with the
    // flag on — their commands aren't shard-pinned.
    for pool in ["shared", "subscription"] {
        for eid in [1_i64, 42, 320_816_801_799_737_344] {
            let subject = command_subject(pool, eid, true, N);
            assert_eq!(subject, format!("noetl.commands.{pool}.{eid}"));
            assert!(!subject.contains(".shard."));
        }
    }
}

#[test]
fn subject_matches_filter_model_is_correct() {
    // Guard the model itself so the proofs above rest on correct matching.
    assert!(subject_matches_filter("noetl.commands.system.shard.2.325", "noetl.commands.system.>"));
    assert!(subject_matches_filter(
        "noetl.commands.system.shard.2.325",
        "noetl.commands.system.shard.2.>"
    ));
    assert!(!subject_matches_filter(
        "noetl.commands.system.shard.2.325",
        "noetl.commands.system.shard.3.>"
    ));
    assert!(subject_matches_filter("noetl.commands.system.325", "noetl.commands.system.>"));
    // `>` requires at least one trailing token.
    assert!(!subject_matches_filter("noetl.commands.system", "noetl.commands.system.>"));
    // `*` matches exactly one token.
    assert!(subject_matches_filter("noetl.commands.system.325", "noetl.commands.*.325"));
    assert!(!subject_matches_filter("noetl.commands.system.a.325", "noetl.commands.*.325"));
}
