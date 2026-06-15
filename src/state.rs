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
    // Note: Full tests require a database connection
    // These are placeholder tests for documentation

    #[test]
    fn test_uptime() {
        // AppState::new requires a real DB pool, so we can't easily test here
        // This is a documentation placeholder
    }
}
