//! Application state for the NoETL Control Plane server.
//!
//! This module defines the shared application state that is
//! passed to all handlers via Axum's state management.

use crate::config::AppConfig;
use crate::db::{DbPool, DbPoolMap};
use crate::sharding::ShardConfig;
use crate::snowflake::{derive_machine_id, SnowflakeGenerator};
use std::sync::Arc;

/// Shared application state.
///
/// This struct holds all shared resources that handlers need access to.
/// It is wrapped in an `Arc` and passed to handlers via Axum's state.
#[derive(Clone)]
pub struct AppState {
    /// Legacy database connection pool.
    ///
    /// In single-pool fallback mode (Phase F R4-1's
    /// `NOETL_SHARDS` empty), this IS the only pool — every
    /// handler that hasn't migrated to [`Self::pools`] uses it.
    /// In sharded mode, `db` is the cluster-wide pool (the
    /// always-master pool for catalog / credential / keychain /
    /// runtime / etc.) so handlers that read cluster-wide tables
    /// keep working without R4-3 touching them.
    ///
    /// Phase F R4-3 migrates per-execution call sites to
    /// `self.pools.pool_for(execution_id)`.  Until that round
    /// lands, every handler reads from `db` regardless of which
    /// table they touch — which is correct in fallback mode
    /// (one pool everywhere) and incorrect-but-tolerated in
    /// sharded mode (per-execution tables would still go to the
    /// cluster master; this is why the kind validation in R4-5
    /// only fires after R4-3 ships).
    pub db: DbPool,

    /// Sharded pool map — N per-shard pools + 1 cluster pool.
    ///
    /// Phase F R4-2 of
    /// [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)
    /// added this.  Use [`DbPoolMap::pool_for`] for per-execution
    /// tables (`event`, `command`, `execution`, `outbox`,
    /// `transient`, `stage`, `frame`, `projection`,
    /// `projection_snapshot`, `result_ref`) and
    /// [`DbPoolMap::cluster`] for cluster-wide tables
    /// (`catalog`, `credential`, `keychain`, `runtime`,
    /// `schedule`, `resource`, `manifest`, `manifest_part`).
    ///
    /// In single-pool fallback mode (NOETL_SHARDS empty), every
    /// accessor returns the same pool as [`Self::db`] — handlers
    /// that opt into `pools` get bit-identical behaviour to the
    /// legacy path.
    pub pools: DbPoolMap,

    /// Application configuration
    pub config: Arc<AppConfig>,

    /// NATS client (optional)
    pub nats: Option<Arc<async_nats::Client>>,

    /// Application-side snowflake ID generator.  Phase F R1.5 of
    /// [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)
    /// moved id generation out of the DB-side `noetl.snowflake_id()`
    /// function into this generator so (a) spans see ids before the
    /// DB round-trip, (b) retries stay idempotent, (c) the upcoming
    /// sharded layout (R4) can pin `machine_id` per pod via the
    /// deployment manifest.  See `src/snowflake.rs` for the id
    /// layout and migration rationale.
    pub snowflake: Arc<SnowflakeGenerator>,

    /// Shard routing configuration.  Phase F R2 of
    /// [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)
    /// added this.  Single-shard default (no enforcement) when
    /// `NOETL_SHARD_INDEX` + `NOETL_SHARD_COUNT` are unset, so
    /// current deployments continue working unchanged.  See
    /// `src/sharding.rs` for the hash-function choice and the
    /// routing semantics; the cross-component design lives on
    /// the [noetl/server wiki sharding-design page](https://github.com/noetl/server/wiki/sharding-design).
    pub shard: Arc<ShardConfig>,

    /// Server start time for uptime calculation
    pub start_time: std::time::Instant,

    /// Per-execution orchestrator state cache (noetl/ai-meta#100 perf).  The
    /// orchestrator advances a cached `WorkflowState` by applying only NEW
    /// events each trigger instead of reloading + replaying the whole event
    /// log (the per-trigger O(n) rebuild + the concurrent-completion memory
    /// spike were the scaling bottleneck — a high-concurrency run OOM'd the
    /// server).  The per-execution lock inside also serialises a single
    /// execution's concurrent completion triggers.
    pub orch_cache: Arc<OrchStateCache>,

    /// Per-execution chain head for the one-level event chain (RFC #115 Phase
    /// 2, noetl/ai-meta#115 §4).  The event-write chokepoint reads + advances it
    /// to stamp each row's `prev_event_id`, so per-execution events form a
    /// walkable singly-linked list.  See [`ChainHeads`].
    pub chain_heads: Arc<ChainHeads>,

    /// CQRS write-path publisher (noetl/ai-meta#103 phase 2d-3).  Lazily built
    /// on first use from [`Self::nats`] so the `emit_event` chokepoint can
    /// publish to the `noetl_events` stream when
    /// `config.event_ingest_publish_only` is on.  `OnceCell` so the gate-off
    /// (default) path never builds it and the gate-on path ensures the stream
    /// exactly once.  `None` inner stays uninitialised until the first publish.
    pub event_stream_publisher: Arc<tokio::sync::OnceCell<crate::nats::EventStreamPublisher>>,
}

/// Cached orchestrator state for one execution, advanced incrementally.
#[derive(Default)]
pub struct ExecOrchState {
    /// The reconstructed workflow state (None until first built).
    pub state: Option<crate::engine::state::WorkflowState>,
    /// Highest event_id applied to `state`.
    pub last_event_id: i64,
    /// Number of events applied — compared against the live event COUNT to
    /// detect a straggler (an event with id <= last_event_id inserted late);
    /// on mismatch the cache falls back to a full rebuild for correctness.
    pub applied_count: i64,
    /// The `playbook_started` event's meta (pool segment + W3C trace routing),
    /// cached so follow-up command dispatch needn't reload the first event.
    pub routing_meta: Option<serde_json::Value>,
    /// Highest `event_id` folded into the last persisted
    /// `projection_snapshot` (noetl/ai-meta#101 block b).  A rebuild loads
    /// that snapshot + only events newer than this, so the rebuild cost is
    /// bounded by the snapshot interval instead of the whole (growing) event
    /// log — which is what OOM'd the server at scale.
    pub snapshot_version: i64,
    /// Last time the O(events) consistency `COUNT(*)` ran for this execution
    /// (noetl/ai-meta#101 block b throughput).  That count grows with the log
    /// (≈27ms at 60k events) and would dominate the hot path if run on every
    /// trigger, so it's throttled; between checks the incremental apply + the
    /// immediate `trigger_event_id`-straggler handling carry correctness.  Not
    /// serialized (in-memory only).
    pub last_count_check: Option<std::time::Instant>,
    /// Worker-driven drive (noetl/ai-meta#108 slice 3): true while an
    /// `system/orchestrate` command is dispatched to the pool but its result
    /// has not yet been applied.  Set when the scheduler dispatches, cleared
    /// when the completion is applied.  Serialises drives per execution so two
    /// near-simultaneous triggers (or the reconcile poller) don't dispatch two
    /// orchestrate commands → double-issue the same next commands.  In-memory
    /// only; a server restart re-derives the drive from the event log.
    pub orchestrate_in_flight: bool,
}


/// Per-execution chain head for the one-level event chain (RFC #115 Phase 2,
/// noetl/ai-meta#115 §4).  Maps `execution_id → event_id of the last event
/// appended to the chain`.  The event-write chokepoint
/// ([`crate::handlers::event_write::emit_events`]) reads the head as the next
/// row's `prev_event_id`, then advances it — so the per-execution events form a
/// singly-linked list walkable pointer-by-pointer, no `noetl.event` scan.
///
/// In-memory only and **no DB read on the hot path**: a cold slot (execution's
/// first event, or an execution whose earlier events were handled by a
/// different replica / before a restart) yields `None`, i.e. a chain root.
/// This shares the locality assumption the existing [`OrchStateCache`] already
/// relies on — an execution's drive is serialised on one replica — so the chain
/// is continuous for the single-writer / single-replica topology the kind gate
/// validates.  Restart-spanning repair is a Phase 3 builder concern (it
/// re-walks from a durable head); nothing reads `prev_event_id` yet, so a chain
/// restart here is additive metadata, never a regression.
#[derive(Default)]
pub struct ChainHeads {
    map: std::sync::Mutex<std::collections::HashMap<i64, i64>>,
}

impl ChainHeads {
    /// Stamp the chain link for a batch of event ids emitted (in order) for one
    /// execution, advancing the head as it goes.  Returns, for each id, the
    /// `prev_event_id` it should carry (`None` for a chain root).  One short
    /// `std::Mutex` critical section, no `await` held — safe to call from any
    /// chokepoint path without lock-ordering hazards against
    /// [`OrchStateCache`]'s per-execution `tokio::Mutex`.
    ///
    /// An empty `event_ids` is a no-op returning an empty vec.
    pub fn link_batch(&self, execution_id: i64, event_ids: &[i64]) -> Vec<Option<i64>> {
        let mut map = self.map.lock().unwrap();
        let mut head = map.get(&execution_id).copied();
        let mut prevs = Vec::with_capacity(event_ids.len());
        for &eid in event_ids {
            prevs.push(head);
            head = Some(eid);
        }
        if let Some(h) = head {
            map.insert(execution_id, h);
        }
        prevs
    }

    /// Drop a terminal execution's head (frees memory).  Called alongside
    /// [`OrchStateCache::evict`] on the terminal-event paths.
    pub fn evict(&self, execution_id: i64) {
        self.map.lock().unwrap().remove(&execution_id);
    }

    /// Current head for an execution, if any.  Test/diagnostic helper.
    pub fn head(&self, execution_id: i64) -> Option<i64> {
        self.map.lock().unwrap().get(&execution_id).copied()
    }
}

/// Per-execution orchestrator state cache.  The outer `std::Mutex` guards a
/// short get-or-insert; the inner per-execution `tokio::Mutex` is held across
/// the orchestrator's DB round-trips so a single execution's triggers serialise
/// (different executions never contend).
#[derive(Default)]
pub struct OrchStateCache {
    map: std::sync::Mutex<
        std::collections::HashMap<i64, Arc<tokio::sync::Mutex<ExecOrchState>>>,
    >,
}

impl OrchStateCache {
    /// Get (or create) the per-execution state slot.
    pub fn entry(&self, execution_id: i64) -> Arc<tokio::sync::Mutex<ExecOrchState>> {
        self.map
            .lock()
            .unwrap()
            .entry(execution_id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(ExecOrchState::default())))
            .clone()
    }

    /// Drop a terminal execution's cached state (frees memory).
    pub fn evict(&self, execution_id: i64) {
        self.map.lock().unwrap().remove(&execution_id);
    }

    /// Snapshot of the currently-cached (i.e. non-terminal, not-yet-evicted)
    /// execution ids.  The background reconcile poller iterates these to
    /// force-advance any execution that got stuck — e.g. a cursor that missed a
    /// non-triggering straggler and stopped emitting events, so no trigger would
    /// otherwise retry.  Cheap: a short lock + clone of the keys.
    pub fn active_executions(&self) -> Vec<i64> {
        self.map.lock().unwrap().keys().copied().collect()
    }
}

impl AppState {
    /// Create a new application state.
    ///
    /// # Arguments
    ///
    /// * `db` - Legacy database connection pool (kept for handlers
    ///   not yet migrated to [`Self::pools`] — see field doc).
    /// * `pools` - Sharded pool map.  In single-pool fallback
    ///   mode this is constructed from the same `db` connection
    ///   via [`DbPoolMap::new`] with an empty [`crate::config::ShardingConfig`];
    ///   callers should use [`AppState::new_legacy`] when they
    ///   don't have a separate `ShardingConfig` to pass.
    /// * `config` - Application configuration
    /// * `nats` - Optional NATS client
    ///
    /// Reads `server_machine_id` from `config.server_machine_id`
    /// (envy: `NOETL_SERVER_MACHINE_ID`).  When unset, derives a
    /// 10-bit id from the process hostname via FNV-1a — fine for
    /// local dev; the deployment manifest should set the env var
    /// explicitly per replica in production.
    ///
    /// # Returns
    ///
    /// A new `AppState` instance.
    ///
    /// # Panics
    ///
    /// Panics if the configured `server_machine_id` exceeds the
    /// 10-bit max (1023).  The caller should validate at
    /// config-load time; this is the last-resort guard.
    pub fn new(
        db: DbPool,
        pools: DbPoolMap,
        config: AppConfig,
        nats: Option<async_nats::Client>,
    ) -> Self {
        let machine_id = config.server_machine_id.unwrap_or_else(|| {
            let hostname = std::env::var("HOSTNAME")
                .or_else(|_| std::env::var("COMPUTERNAME"))
                .unwrap_or_else(|_| "noetl-server-local".to_string());
            derive_machine_id(&hostname)
        });
        let snowflake = SnowflakeGenerator::new(machine_id)
            .expect("server_machine_id must fit in 10 bits; validate config at startup");
        tracing::info!(
            machine_id = snowflake.machine_id(),
            source = if config.server_machine_id.is_some() {
                "NOETL_SERVER_MACHINE_ID"
            } else {
                "derived from HOSTNAME"
            },
            "Snowflake generator initialized"
        );

        // Phase F R2: shard configuration.  Single-shard default
        // (no enforcement) when neither env var is set — that
        // keeps current single-replica deployments working
        // without any change.  Validation: shard_index <
        // shard_count must hold; startup panics otherwise so we
        // fail fast on a config bug rather than silently
        // mis-routing requests.
        let shard_count = config.shard_count.unwrap_or(1);
        let shard_index = config.shard_index.unwrap_or(0);
        let shard = ShardConfig::new(shard_index, shard_count).unwrap_or_else(|e| {
            panic!("invalid shard config (NOETL_SHARD_INDEX / NOETL_SHARD_COUNT): {e}")
        });
        tracing::info!(
            shard_index = shard.shard_index,
            shard_count = shard.shard_count,
            sharding_enabled = shard.shard_count > 1,
            source = if config.shard_index.is_some() || config.shard_count.is_some() {
                "NOETL_SHARD_INDEX / NOETL_SHARD_COUNT"
            } else {
                "default (no sharding)"
            },
            "Shard configuration initialized"
        );

        Self {
            db,
            pools,
            config: Arc::new(config),
            nats: nats.map(Arc::new),
            snowflake: Arc::new(snowflake),
            shard: Arc::new(shard),
            start_time: std::time::Instant::now(),
            orch_cache: Arc::new(OrchStateCache::default()),
            chain_heads: Arc::new(ChainHeads::default()),
            event_stream_publisher: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    /// Convenience constructor for tests + paths that haven't
    /// loaded a [`ShardingConfig`] yet.  Wraps the legacy `db`
    /// pool in a single-pool [`DbPoolMap`] (no per-shard pools,
    /// no separate cluster pool — the same `db` handle covers
    /// every accessor).
    ///
    /// `main.rs` uses the full [`AppState::new`] with a pool map
    /// built from [`ShardingConfig::from_env`] so the production
    /// path honors `NOETL_SHARDS` if set.  Test code that
    /// already has a `DbPool` in hand uses this shim.
    pub fn new_legacy(
        db: DbPool,
        config: AppConfig,
        nats: Option<async_nats::Client>,
    ) -> Self {
        let pools = DbPoolMap::from_single_pool(db.clone());
        Self::new(db, pools, config, nats)
    }

    /// Get the server uptime in seconds.
    pub fn uptime_seconds(&self) -> u64 {
        self.start_time.elapsed().as_secs()
    }

    /// Check if NATS is configured and connected.
    pub fn has_nats(&self) -> bool {
        self.nats.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::ChainHeads;

    // Note: Full tests require a database connection
    // These are placeholder tests for documentation

    #[test]
    fn test_uptime() {
        // AppState::new requires a real DB pool, so we can't easily test here
        // This is a documentation placeholder
    }

    // RFC #115 §4: the per-execution chain head turns a stream of emitted event
    // ids into a walkable singly-linked list.
    #[test]
    fn chain_head_links_first_event_is_root() {
        let heads = ChainHeads::default();
        // First batch: the execution's first event has no predecessor (root).
        let prevs = heads.link_batch(7, &[100]);
        assert_eq!(prevs, vec![None], "execution's first event is a chain root");
        assert_eq!(heads.head(7), Some(100));
    }

    #[test]
    fn chain_head_links_batch_in_order() {
        let heads = ChainHeads::default();
        // A multi-row batch (e.g. a cursor fan-out's N command.issued events)
        // links each row to the previous one in order; the first to the head.
        let prevs = heads.link_batch(7, &[10, 11, 12]);
        assert_eq!(prevs, vec![None, Some(10), Some(11)]);
        assert_eq!(heads.head(7), Some(12), "head advances to the last id");
        // A following batch continues the chain from the advanced head.
        let prevs = heads.link_batch(7, &[20, 21]);
        assert_eq!(prevs, vec![Some(12), Some(20)]);
        assert_eq!(heads.head(7), Some(21));
    }

    #[test]
    fn chain_head_is_per_execution() {
        let heads = ChainHeads::default();
        heads.link_batch(1, &[10, 11]);
        heads.link_batch(2, &[90]);
        // Distinct executions never share a head.
        assert_eq!(heads.head(1), Some(11));
        assert_eq!(heads.head(2), Some(90));
        let p1 = heads.link_batch(1, &[12]);
        let p2 = heads.link_batch(2, &[91]);
        assert_eq!(p1, vec![Some(11)]);
        assert_eq!(p2, vec![Some(90)]);
    }

    #[test]
    fn chain_head_walk_reconstructs_full_sequence_no_gaps() {
        // Property the kind validation asserts in SQL: walking prev_event_id
        // from the head reconstructs the full per-execution sequence with no
        // gaps.  Model it over the in-memory linker.
        let heads = ChainHeads::default();
        let ids = [5_i64, 6, 7, 8, 9];
        let prevs = heads.link_batch(42, &ids);
        // Build the prev map the chain would persist.
        let mut prev_of = std::collections::HashMap::new();
        for (id, prev) in ids.iter().zip(&prevs) {
            prev_of.insert(*id, *prev);
        }
        // Walk backward from the head.
        let mut walked = Vec::new();
        let mut cur = heads.head(42);
        while let Some(id) = cur {
            walked.push(id);
            cur = prev_of.get(&id).copied().flatten();
        }
        walked.reverse();
        assert_eq!(walked, ids, "pointer walk == full emit sequence, no gaps");
    }

    #[test]
    fn chain_head_evict_drops_state() {
        let heads = ChainHeads::default();
        heads.link_batch(7, &[10]);
        assert_eq!(heads.head(7), Some(10));
        heads.evict(7);
        assert_eq!(heads.head(7), None, "evicted execution starts a fresh chain");
        // After eviction the next event is treated as a root again.
        let prevs = heads.link_batch(7, &[20]);
        assert_eq!(prevs, vec![None]);
    }

    #[test]
    fn chain_head_empty_batch_is_noop() {
        let heads = ChainHeads::default();
        assert_eq!(heads.link_batch(7, &[]), Vec::<Option<i64>>::new());
        assert_eq!(heads.head(7), None);
    }
}
