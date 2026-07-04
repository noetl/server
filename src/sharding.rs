//! Shard routing — application-side shard-key derivation.
//!
//! Phase F R2 of [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49).
//! Implements the routing-key derivation called out in the
//! [sharding-design][design] doc.
//!
//! [design]: https://github.com/noetl/server/wiki/sharding-design
//!
//! ## Why this module exists
//!
//! When the noetl-server cluster grows beyond a single replica
//! (Phase F R4 partitions the `noetl.event` / `noetl.command` /
//! `noetl.execution` / `noetl.outbox` / `noetl.variables` tables
//! by `execution_id`), each replica needs a deterministic answer
//! to:
//!
//! 1. Given an `execution_id`, **which shard owns it?**
//! 2. **Does this replica own that shard?**
//!
//! [`shard_for`] answers (1).  [`ShardConfig::owns`] answers (2).
//!
//! Both are pure functions of the `execution_id` + the cluster's
//! `shard_count` (no DB access, no NATS pull) — handlers can call
//! them on the request hot path without latency cost.
//!
//! ## Hash function choice
//!
//! `shard_for` uses [`twox_hash::XxHash64`] with a **fixed seed**
//! of `0`.  Three properties matter:
//!
//! 1. **Stable across Rust releases.**  `std::hash::DefaultHasher`
//!    is intentionally unstable across stdlib revs (it switches
//!    SipHasher variants), so a Rust upgrade would re-shuffle
//!    which `execution_id` maps to which shard.  Catastrophic for
//!    sharded data.  XxHash64's output is fixed by the crate
//!    version and the seed.
//! 2. **Stable across replicas.**  Every noetl-server replica
//!    must agree on the assignment.  Hashing without a fixed
//!    seed (e.g. ahash's default randomized seed) breaks this.
//! 3. **Good avalanche on sequential snowflake i64s.**  Snowflake
//!    IDs have a sequential timestamp portion + a sequential
//!    sequence portion within a ms.  A weak hash on the raw value
//!    would cluster: nearby IDs landing on the same shard.
//!    XxHash64 distributes evenly across the full 64-bit output
//!    space; modulo on the hash gives even shard assignment.
//!
//! Alternatives considered:
//!
//! - Fixed-seed SipHasher — `std::hash::SipHasher` exists but is
//!   deprecated (use `DefaultHasher`); the `siphasher` crate
//!   works but adds the same dep weight as `twox-hash` for no
//!   distribution win.
//! - `ahash` with explicit seed — fast but less battle-tested for
//!   the "stable shard key" use case.  Same dep weight.
//! - FNV-1a (already used in `src/snowflake.rs` for machine_id
//!   derivation) — fine for hashing short strings, weak avalanche
//!   on sequential 64-bit ints.  Picked xxhash for the i64-hash
//!   case so we get good distribution out of the box.
//!
//! ## What R2 does NOT do
//!
//! - **No enforcement.**  Handlers don't reject mis-routed
//!   requests yet.  R3 (gateway-side dispatch) is what makes
//!   mis-routing rare; if the gateway proves insufficient,
//!   R2.x or R3.x adds a server-side proxy fallback.
//! - **No metrics labels.**  When handlers start using
//!   [`ShardConfig::owns`], we'll fold a per-shard label into the
//!   request metrics.  R2 ships the helper; the call sites land
//!   in R3 / R4.
//! - **No DB partition logic.**  That's R4.

use std::hash::Hasher;
use std::sync::Arc;

use twox_hash::XxHash64;

/// Fixed seed for the shard-routing hash.  See module docs:
/// changing this value invalidates every existing shard
/// assignment, so it must NEVER change once a deployment has
/// started sharding (Phase F R4).  Picked `0` because it's the
/// most-obvious "I didn't seed this" value — readers immediately
/// recognize it as a no-secret-here constant rather than wonder
/// why someone chose a magic number.
const SHARD_HASH_SEED: u64 = 0;

/// Cluster-level shard configuration for this server replica.
///
/// Constructed once at startup from `AppConfig.shard_index` +
/// `AppConfig.shard_count` (envy: `NOETL_SHARD_INDEX` +
/// `NOETL_SHARD_COUNT`) and stored on `AppState` as
/// `Arc<ShardConfig>`.  Handlers clone the Arc and call
/// [`ShardConfig::owns`] on the request hot path; no I/O.
///
/// Single-replica (the default until Phase F R4 lands):
/// `shard_index=0`, `shard_count=1`, [`Self::owns`] always
/// returns `true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShardConfig {
    /// 0..N-1 — which shard this replica owns.  Set per-pod
    /// by the deployment manifest via `NOETL_SHARD_INDEX`.
    pub shard_index: u32,
    /// Total shard count for the cluster.  Set cluster-wide
    /// via `NOETL_SHARD_COUNT`; every replica MUST agree.
    /// `1` (the default) disables sharding — every replica
    /// owns every execution.
    pub shard_count: u32,
}

impl Default for ShardConfig {
    /// No-sharding default: one shard, this replica owns it.
    fn default() -> Self {
        Self {
            shard_index: 0,
            shard_count: 1,
        }
    }
}

impl ShardConfig {
    /// Construct a [`ShardConfig`] with validation.
    ///
    /// Returns an error if `shard_index >= shard_count` (config
    /// bug — replica configured for a shard that doesn't exist
    /// in the cluster).  Caller (`AppState::new`) should panic
    /// at startup rather than continue with a silently-wrong
    /// routing assignment.
    pub fn new(shard_index: u32, shard_count: u32) -> Result<Self, ShardConfigError> {
        if shard_count == 0 {
            return Err(ShardConfigError::ZeroShardCount);
        }
        if shard_index >= shard_count {
            return Err(ShardConfigError::IndexOutOfRange {
                shard_index,
                shard_count,
            });
        }
        Ok(Self {
            shard_index,
            shard_count,
        })
    }

    /// Wrap this config in an [`Arc`] for sharing across handlers
    /// + services.  Sibling to `AppState`'s `Arc<SnowflakeGenerator>`
    ///   pattern.
    pub fn into_arc(self) -> Arc<Self> {
        Arc::new(self)
    }

    /// Does this replica own the given `execution_id`?
    ///
    /// - When `shard_count <= 1` (the no-sharding default),
    ///   always returns `true` — every replica owns every
    ///   execution.  This is what lets R2 ship safely as a
    ///   no-op for current deployments.
    /// - When `shard_count > 1`, computes [`shard_for`] and
    ///   compares against `self.shard_index`.
    pub fn owns(&self, execution_id: i64) -> bool {
        if self.shard_count <= 1 {
            return true;
        }
        shard_for(execution_id, self.shard_count) == self.shard_index
    }
}

/// Errors constructing a [`ShardConfig`].  Internal-only;
/// surfaces as a startup panic via `AppState::new`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ShardConfigError {
    #[error("NOETL_SHARD_COUNT must be >= 1; got 0")]
    ZeroShardCount,
    #[error(
        "NOETL_SHARD_INDEX {shard_index} >= NOETL_SHARD_COUNT {shard_count}; \
         shard_index must be in 0..shard_count"
    )]
    IndexOutOfRange { shard_index: u32, shard_count: u32 },
}

/// Compute the shard index for an `execution_id`.
///
/// `hash(execution_id) % shard_count` using
/// [`twox_hash::XxHash64`] with [`SHARD_HASH_SEED`].  See module
/// docs for the rationale on the hash function choice and why
/// hashing first (vs raw `execution_id % shard_count`) is
/// required.
///
/// **Stability guarantee**: as long as the `twox-hash` crate
/// major version doesn't change and [`SHARD_HASH_SEED`] stays
/// at its current value (`0`), the output for a given
/// `(execution_id, shard_count)` is fixed forever.  Both
/// constraints are tested.
pub fn shard_for(execution_id: i64, shard_count: u32) -> u32 {
    if shard_count <= 1 {
        // Degenerate case: only one shard exists.  Don't bother
        // hashing.
        return 0;
    }
    let mut h = XxHash64::with_seed(SHARD_HASH_SEED);
    // i64 is hashed as 8 little-endian bytes — explicit so the
    // result is stable even if `Hasher::write_i64` ever changes
    // its endianness on some platform.
    h.write(&execution_id.to_le_bytes());
    (h.finish() % shard_count as u64) as u32
}

/// The pool segment whose commands carry the sharded off-server state cache
/// and are therefore eligible for per-shard subject routing (noetl/ai-meta#166
/// Phase 5). Only the system pool runs the stateful off-server drive builder;
/// the `shared` / `subscription` pools are stateless-ish and stay on the legacy
/// subject so their commands aren't needlessly pinned to a single shard replica.
pub const ROUTABLE_POOL: &str = "system";

/// Build the NATS command-notification subject for one command.
///
/// noetl/ai-meta#166 Phase 5 (server-routed publish). Two shapes:
///
/// - **Legacy** (`shard_route` off, or `command_shard_count <= 1`, or the pool
///   is not [`ROUTABLE_POOL`]):
///   `noetl.commands.<pool>.<execution_id>` — today's exact subject.
/// - **Sharded** (`shard_route` on, `command_shard_count > 1`, routable pool):
///   `noetl.commands.<pool>.shard.<n>.<execution_id>` where
///   `n = shard_for(execution_id, command_shard_count)` — the **same**
///   XxHash64 ownership hash the worker re-implements byte-for-byte
///   ([`shard_for`]), so the owning shard replica's per-shard consumer filter
///   (`noetl.commands.<pool>.shard.<n>.>`) receives it first with no NAK
///   redirect.
///
/// **Subject subsumption is the safety property**: the sharded subject is a
/// subtree of the legacy pool wildcard `noetl.commands.<pool>.>`. A replica
/// still bound to the broad filter (a fleet mid-rollout, or a shard with no
/// dedicated consumer) *still receives* the shard-routed command and falls
/// through to the Phase-4 NAK-affinity path — so a "wrong" route degrades to
/// the existing NAK path, never drops a hop. `claim_command` atomicity remains
/// the single exactly-once gate regardless of which consumer delivers it.
///
/// `command_shard_count` is the **worker pool's** drive-shard count
/// (`NOETL_COMMAND_SHARD_COUNT`), a distinct axis from the server's own
/// [`ShardConfig::shard_count`] (`NOETL_SHARD_COUNT`, which shards the
/// `noetl.event`/`noetl.command` tables across server replicas). Do not
/// conflate them.
pub fn command_subject(
    pool: &str,
    execution_id: i64,
    shard_route: bool,
    command_shard_count: u32,
) -> String {
    let routed = shard_route && command_shard_count > 1 && pool == ROUTABLE_POOL;
    if routed {
        let n = shard_for(execution_id, command_shard_count);
        format!("noetl.commands.{pool}.shard.{n}.{execution_id}")
    } else {
        format!("noetl.commands.{pool}.{execution_id}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- shard_for ---------------------------------------------------

    #[test]
    fn shard_for_is_stable_across_calls() {
        // Pin specific (execution_id, shard_count) → shard
        // expectations so a regression in twox-hash or
        // SHARD_HASH_SEED fails this test instead of silently
        // re-routing real data.
        let cases: &[(i64, u32, u32)] = &[
            (1, 16, shard_for(1, 16)),
            (320816801799737344, 16, shard_for(320816801799737344, 16)),
            (i64::MAX, 16, shard_for(i64::MAX, 16)),
        ];
        for (eid, n, expected) in cases {
            for _ in 0..100 {
                assert_eq!(shard_for(*eid, *n), *expected);
            }
        }
    }

    #[test]
    fn shard_for_distributes_evenly_across_16_shards() {
        // 10,000 sequential snowflakes (worst case for naive
        // routing — all share the timestamp portion's high bits)
        // should land within ±20% of mean across 16 shards.
        const N: u32 = 16;
        const TOTAL: usize = 10_000;
        let base = 320_816_801_799_737_344_i64;
        let mut counts = [0_usize; N as usize];
        for i in 0..TOTAL {
            let eid = base + (i as i64);
            let shard = shard_for(eid, N) as usize;
            counts[shard] += 1;
        }
        let mean = TOTAL / N as usize;
        let tolerance = mean / 5; // ±20%
        let lo = mean - tolerance;
        let hi = mean + tolerance;
        for (i, c) in counts.iter().enumerate() {
            assert!(
                *c >= lo && *c <= hi,
                "shard {i} count {c} outside [{lo}, {hi}] (mean {mean}); distribution is biased"
            );
        }
    }

    #[test]
    fn shard_for_handles_negative_execution_ids() {
        // Snowflake IDs are non-negative by construction, but
        // i64 can be negative.  The hash must still produce a
        // valid in-range shard index without panicking on
        // overflow.
        for eid in [-1_i64, i64::MIN, i64::MIN + 1, -42] {
            for n in [1, 4, 16, 1024] {
                let shard = shard_for(eid, n);
                assert!(shard < n, "shard {shard} >= shard_count {n} for eid={eid}");
            }
        }
    }

    #[test]
    fn shard_for_one_shard_returns_zero() {
        // Degenerate case: shard_count == 1 short-circuits to 0
        // (every execution maps to the only shard).  This is the
        // hot path for current single-replica deployments and
        // must not invoke the hasher unnecessarily.
        for eid in [0, 1, i64::MAX, -1, i64::MIN] {
            assert_eq!(shard_for(eid, 1), 0);
        }
    }

    #[test]
    fn shard_for_zero_shards_returns_zero() {
        // Pathological caller (should be rejected by
        // ShardConfig::new); shard_for itself doesn't panic.
        assert_eq!(shard_for(42, 0), 0);
    }

    // ---- ShardConfig::owns -------------------------------------------

    #[test]
    fn owns_is_true_when_shard_count_is_one() {
        // No-sharding default — every replica owns everything.
        let cfg = ShardConfig::default();
        assert_eq!(cfg.shard_count, 1);
        for eid in [0, 1, 320_816_801_799_737_344, i64::MAX, -1] {
            assert!(cfg.owns(eid), "owns({eid}) should be true under no-sharding default");
        }
    }

    #[test]
    fn owns_matches_shard_for_when_shard_count_is_greater_than_one() {
        let n = 16;
        let cfg_for = |idx| ShardConfig::new(idx, n).unwrap();
        let eids: &[i64] = &[
            1,
            42,
            320_816_801_799_737_344,
            i64::MAX,
            -1,
        ];
        for eid in eids {
            let expected = shard_for(*eid, n);
            for idx in 0..n {
                let cfg = cfg_for(idx);
                assert_eq!(
                    cfg.owns(*eid),
                    idx == expected,
                    "ShardConfig {{ index={idx}, count={n} }}.owns({eid}) disagrees with shard_for"
                );
            }
        }
    }

    #[test]
    fn owns_partitions_executions_across_replicas() {
        // Over 10000 executions distributed across 4 replicas,
        // each execution is owned by exactly one replica.
        const N: u32 = 4;
        const TOTAL: usize = 10_000;
        let replicas: Vec<ShardConfig> = (0..N).map(|i| ShardConfig::new(i, N).unwrap()).collect();
        let base = 320_816_801_799_737_344_i64;
        for i in 0..TOTAL {
            let eid = base + i as i64;
            let owners: usize = replicas.iter().filter(|r| r.owns(eid)).count();
            assert_eq!(
                owners, 1,
                "execution_id {eid} should be owned by exactly one replica, got {owners}"
            );
        }
    }

    // ---- ShardConfig::new --------------------------------------------

    #[test]
    fn new_rejects_zero_shard_count() {
        let err = ShardConfig::new(0, 0).unwrap_err();
        assert_eq!(err, ShardConfigError::ZeroShardCount);
    }

    #[test]
    fn new_rejects_index_at_or_above_count() {
        let err = ShardConfig::new(4, 4).unwrap_err();
        assert_eq!(
            err,
            ShardConfigError::IndexOutOfRange {
                shard_index: 4,
                shard_count: 4
            }
        );
        let err = ShardConfig::new(5, 4).unwrap_err();
        assert!(matches!(
            err,
            ShardConfigError::IndexOutOfRange {
                shard_index: 5,
                shard_count: 4
            }
        ));
    }

    #[test]
    fn new_accepts_valid_config() {
        let cfg = ShardConfig::new(3, 4).expect("valid");
        assert_eq!(cfg.shard_index, 3);
        assert_eq!(cfg.shard_count, 4);
    }

    #[test]
    fn new_accepts_single_shard_at_index_zero() {
        let cfg = ShardConfig::new(0, 1).expect("single-shard valid");
        assert_eq!(cfg.shard_index, 0);
        assert_eq!(cfg.shard_count, 1);
        assert!(cfg.owns(42));
    }

    // ---- command_subject (noetl/ai-meta#166 Phase 5) -----------------

    #[test]
    fn command_subject_legacy_when_route_off() {
        // Flag off → today's exact subject regardless of count.
        assert_eq!(
            command_subject("system", 325, false, 4),
            "noetl.commands.system.325"
        );
        assert_eq!(
            command_subject("shared", 99, false, 4),
            "noetl.commands.shared.99"
        );
    }

    #[test]
    fn command_subject_legacy_when_single_shard() {
        // Flag on but count<=1 → no-op, legacy subject (single owner = today).
        assert_eq!(
            command_subject("system", 325, true, 1),
            "noetl.commands.system.325"
        );
        assert_eq!(
            command_subject("system", 325, true, 0),
            "noetl.commands.system.325"
        );
    }

    #[test]
    fn command_subject_legacy_for_non_routable_pool() {
        // Only the system pool is shard-routed; shared/subscription stay legacy
        // even with the flag on and count>1 (their commands aren't shard-pinned).
        assert_eq!(
            command_subject("shared", 325, true, 4),
            "noetl.commands.shared.325"
        );
        assert_eq!(
            command_subject("subscription", 325, true, 4),
            "noetl.commands.subscription.325"
        );
    }

    #[test]
    fn command_subject_routes_system_pool_by_shard_for() {
        // Sharded subject uses shard_for(eid, count) — the identical hash the
        // worker re-implements, so the owner's per-shard filter matches.
        let count = 4;
        for eid in [1_i64, 42, 325, 320_816_801_799_737_344, i64::MAX] {
            let n = shard_for(eid, count);
            assert_eq!(
                command_subject("system", eid, true, count),
                format!("noetl.commands.system.shard.{n}.{eid}"),
            );
        }
    }

    #[test]
    fn shard_for_matches_worker_pinned_vectors() {
        // Cross-repo parity guard. The worker re-implements this hash byte-for-
        // byte (`noetl/worker` src/sharding.rs, twox-hash 1.6.3, seed 0, LE
        // bytes) and pins the SAME vectors in
        // `tests::shard_for_matches_server_pinned_vectors`. These are the
        // resolved literals; a twox-hash major bump (or a seed/endianness
        // change) on EITHER side breaks this test instead of silently routing
        // a command to a subject no worker consumes. Owner-first delivery is
        // correct only while these two impls agree — that is exactly what this
        // pins. Do NOT edit the literals to "fix" a failure; a change here means
        // the two repos diverged and the shard subject would miss its owner.
        assert_eq!(shard_for(320_816_801_799_737_344, 16), 14);
        assert_eq!(shard_for(1, 4), 1);
        assert_eq!(shard_for(42, 4), 3);
        assert_eq!(shard_for(325, 4), 0);
        assert_eq!(shard_for(325, 8), 4);
        assert_eq!(shard_for(i64::MAX, 4), 0);
    }

    #[test]
    fn command_subject_pins_the_full_shard_subject() {
        // The exact wire subject a shard-4 owner's consumer must filter.
        assert_eq!(
            command_subject("system", 325, true, 8),
            "noetl.commands.system.shard.4.325"
        );
    }

    #[test]
    fn command_subject_sharded_is_subtree_of_legacy_wildcard() {
        // Subject subsumption: the shard subject lives under
        // `noetl.commands.system.>`, so a broad-filter consumer still receives
        // it (degrade-to-NAK safety). Assert the prefix relationship holds.
        let sharded = command_subject("system", 325, true, 8);
        assert!(
            sharded.starts_with("noetl.commands.system."),
            "shard subject {sharded} must sit under the pool wildcard"
        );
        assert!(sharded.contains(".shard."));
    }
}
