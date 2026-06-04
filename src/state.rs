//! Application state for the NoETL Control Plane server.
//!
//! This module defines the shared application state that is
//! passed to all handlers via Axum's state management.

use crate::config::AppConfig;
use crate::db::DbPool;
use crate::sharding::ShardConfig;
use crate::snowflake::{derive_machine_id, SnowflakeGenerator};
use std::sync::Arc;

/// Shared application state.
///
/// This struct holds all shared resources that handlers need access to.
/// It is wrapped in an `Arc` and passed to handlers via Axum's state.
#[derive(Clone)]
pub struct AppState {
    /// Database connection pool
    pub db: DbPool,

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
}

impl AppState {
    /// Create a new application state.
    ///
    /// # Arguments
    ///
    /// * `db` - Database connection pool
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
    pub fn new(db: DbPool, config: AppConfig, nats: Option<async_nats::Client>) -> Self {
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
            config: Arc::new(config),
            nats: nats.map(Arc::new),
            snowflake: Arc::new(snowflake),
            shard: Arc::new(shard),
            start_time: std::time::Instant::now(),
        }
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
