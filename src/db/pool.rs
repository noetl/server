//! Database connection pool management.
//!
//! Phase F R4 introduces [`DbPoolMap`] — the N+1 pool layout that
//! lets the server route per-execution queries to the per-shard
//! Postgres and cluster-wide queries to the shared master.  When
//! sharding is OFF (`NOETL_SHARDS` empty), `DbPoolMap` degenerates
//! to a single-pool wrapper that behaves identically to the
//! pre-R4 [`create_pool`] path.

use crate::config::database::{ShardConnection, ShardingConfig};
use crate::config::DatabaseConfig;
use crate::sharding::shard_for;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::sync::Arc;
use std::time::Duration;

/// Type alias for the PostgreSQL connection pool.
pub type DbPool = PgPool;

/// Create a new database connection pool.
///
/// # Arguments
///
/// * `config` - Database configuration
///
/// # Returns
///
/// A configured PostgreSQL connection pool.
///
/// # Errors
///
/// Returns an error if the connection pool cannot be created.
pub async fn create_pool(config: &DatabaseConfig) -> Result<DbPool, sqlx::Error> {
    let pool = PgPoolOptions::new()
        .max_connections(config.max_connections)
        .min_connections(config.min_connections)
        .acquire_timeout(Duration::from_secs(config.acquire_timeout))
        .connect_with(config.connect_options())
        .await?;

    tracing::info!(
        host = %config.host,
        port = %config.port,
        database = %config.database,
        max_connections = config.max_connections,
        "Database connection pool created"
    );

    Ok(pool)
}

/// Check if the database connection is healthy.
///
/// # Arguments
///
/// * `pool` - Database connection pool
///
/// # Returns
///
/// `true` if the database is reachable, `false` otherwise.
pub async fn health_check(pool: &DbPool) -> bool {
    sqlx::query("SELECT 1").execute(pool).await.is_ok()
}

/// N+1 pool layout for Phase F R4 sharding.
///
/// Holds N per-shard pools (selected by [`shard_for`]) and one
/// cluster-wide pool for the always-master tables (`catalog`,
/// `credential`, `keychain`, `runtime`, `schedule`, `resource`,
/// `manifest`, `manifest_part`).  Per-execution tables (`event`,
/// `command`, `execution`, `outbox`, `transient`, `stage`,
/// `frame`, `projection`, `projection_snapshot`, `result_ref`)
/// ride the per-shard pools.
///
/// **Single-pool fallback.**  When [`ShardingConfig::is_disabled`]
/// (i.e. `NOETL_SHARDS` was empty), the constructor builds a
/// degenerate map: one shard whose pool IS the legacy pool, and
/// `cluster` points at the same pool.  Every accessor below
/// returns that pool.  This keeps R4 dormant for current
/// single-host deployments — handlers that adopt
/// `pool_for(execution_id)` get identical behaviour until the
/// operator opts in via env vars.
///
/// Shape chosen for cheap clones: every field is an [`Arc`]-style
/// handle (sqlx's `PgPool` is already internally `Arc`'d), so
/// `Clone` is one ref-count bump per pool.
#[derive(Debug, Clone)]
pub struct DbPoolMap {
    shards: Arc<Vec<DbPool>>,
    cluster: DbPool,
    /// Cached `shards.len()` for the hot path.  `0` is impossible
    /// (the constructor always populates at least one entry); the
    /// helper methods rely on this invariant.
    shard_count: u32,
    /// True when this map was constructed in single-pool fallback
    /// mode (`NOETL_SHARDS` empty).  Distinct from
    /// `shard_count == 1` because the operator MAY opt into
    /// sharding with N=1 (cluster on its own + shard 0 on its own
    /// host); the routing math is identical to fallback but the
    /// pool topology is different.
    single_pool_mode: bool,
}

impl DbPoolMap {
    /// Build the pool map.
    ///
    /// Two modes:
    ///
    /// - **Single-pool fallback** (`sharding.is_disabled()`):
    ///   creates one pool from `legacy` and uses it for both the
    ///   `shards[0]` slot and the cluster slot.  Identical
    ///   behaviour to the pre-R4 [`create_pool`] code path.
    /// - **Sharded** (`sharding.shards` non-empty): creates one
    ///   pool per [`ShardConnection`] in `sharding.shards`, plus
    ///   a separate cluster pool from `sharding.cluster` (or from
    ///   `shards[0]` when `sharding.cluster` is `None` — useful
    ///   for single-node kind validation where one Postgres host
    ///   carries both per-execution and cluster-wide tables).
    ///
    /// Pool-tuning fields (`max_connections`, `min_connections`,
    /// `acquire_timeout`) come from the legacy `DatabaseConfig`
    /// and apply uniformly across every shard + cluster pool.
    /// Per-pool tuning is a Phase G concern.
    pub async fn new(
        legacy: &DatabaseConfig,
        sharding: &ShardingConfig,
    ) -> Result<Self, sqlx::Error> {
        if sharding.is_disabled() {
            let pool = create_pool(legacy).await?;
            tracing::info!("DbPoolMap: single-pool fallback (NOETL_SHARDS empty)");
            return Ok(Self {
                shards: Arc::new(vec![pool.clone()]),
                cluster: pool,
                shard_count: 1,
                single_pool_mode: true,
            });
        }

        let mut shard_pools = Vec::with_capacity(sharding.shards.len());
        for (idx, conn) in sharding.shards.iter().enumerate() {
            let pool = build_pool(legacy, conn).await.inspect_err(|e| {
                tracing::error!(
                    shard_index = idx,
                    host = %conn.host,
                    error = %e,
                    "DbPoolMap: failed to build shard pool"
                );
            })?;
            tracing::info!(
                shard_index = idx,
                host = %conn.host,
                port = %conn.port,
                database = %conn.database,
                "DbPoolMap: shard pool ready"
            );
            shard_pools.push(pool);
        }

        let cluster = match &sharding.cluster {
            Some(conn) => {
                let pool = build_pool(legacy, conn).await.inspect_err(|e| {
                    tracing::error!(
                        host = %conn.host,
                        error = %e,
                        "DbPoolMap: failed to build cluster pool"
                    );
                })?;
                tracing::info!(
                    host = %conn.host,
                    port = %conn.port,
                    database = %conn.database,
                    "DbPoolMap: cluster pool ready"
                );
                pool
            }
            None => {
                tracing::warn!(
                    "DbPoolMap: NOETL_CLUSTER_DSN unset; cluster-wide queries \
                     ride shard 0's pool (single-node kind topology)"
                );
                shard_pools[0].clone()
            }
        };

        let shard_count = shard_pools.len() as u32;
        Ok(Self {
            shards: Arc::new(shard_pools),
            cluster,
            shard_count,
            single_pool_mode: false,
        })
    }

    /// Build a single-pool fallback [`DbPoolMap`] from an
    /// already-created [`DbPool`].  Sync constructor for callers
    /// (tests, the legacy `main.rs` path) that already have a
    /// pool in hand and don't want to re-resolve `ShardingConfig`.
    ///
    /// The result behaves identically to the single-pool branch
    /// of [`DbPoolMap::new`]: one shard whose pool is also the
    /// cluster pool; every accessor returns `pool`.
    pub fn from_single_pool(pool: DbPool) -> Self {
        Self {
            shards: Arc::new(vec![pool.clone()]),
            cluster: pool,
            shard_count: 1,
            single_pool_mode: true,
        }
    }

    /// Number of shard pools configured.  Always `>= 1`.
    pub fn shard_count(&self) -> u32 {
        self.shard_count
    }

    /// True when this map is operating in single-pool fallback
    /// mode (`NOETL_SHARDS` was empty at construction).
    pub fn is_single_pool(&self) -> bool {
        self.single_pool_mode
    }

    /// Pool for the given `execution_id`.
    ///
    /// In single-pool fallback mode (or when `shard_count == 1`)
    /// returns the only shard pool unconditionally — no hash
    /// computation.  In sharded mode, returns
    /// `shards[shard_for(execution_id, shard_count)]`.
    ///
    /// **Stability contract**: this MUST agree with the
    /// gateway-side `shard_for` from Phase F R3a-2.  The R3b
    /// drift-guard integration test
    /// (`repos/ops/automation/development/validate-shard-drift-guard.sh`)
    /// asserts both sides compute the same `shard_index` for the
    /// same `(execution_id, shard_count)` pair.
    pub fn pool_for(&self, execution_id: i64) -> &DbPool {
        if self.shard_count <= 1 {
            return &self.shards[0];
        }
        let idx = shard_for(execution_id, self.shard_count) as usize;
        &self.shards[idx]
    }

    /// Pool for cluster-wide tables (catalog, credential,
    /// keychain, runtime, schedule, resource, manifest).
    ///
    /// In single-pool fallback mode this is the same handle as
    /// every shard pool.
    pub fn cluster(&self) -> &DbPool {
        &self.cluster
    }

    /// Iterator over every per-shard pool, in shard-index order.
    /// Used by the cluster-wide list endpoint
    /// (`GET /api/executions`) for fan-out queries against the
    /// per-execution tables — see Phase F R4-4.
    pub fn all_shards(&self) -> impl Iterator<Item = (u32, &DbPool)> {
        self.shards
            .iter()
            .enumerate()
            .map(|(idx, pool)| (idx as u32, pool))
    }
}

/// Build a pool from a [`ShardConnection`] using the legacy
/// pool-tuning fields (max/min connections + acquire timeout).
async fn build_pool(
    legacy: &DatabaseConfig,
    conn: &ShardConnection,
) -> Result<DbPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(legacy.max_connections)
        .min_connections(legacy.min_connections)
        .acquire_timeout(Duration::from_secs(legacy.acquire_timeout))
        .connect_with(conn.connect_options())
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pool_type_alias() {
        // Type alias should be PgPool
        fn _assert_type(_: DbPool) {}
    }

    // DbPoolMap behavioural tests run against real sqlx pools,
    // which need a live Postgres — they live in the kind-validate
    // rig (Phase F R4-5).  Unit tests here exercise the routing
    // math via `shard_for` directly; the wiring of pool selection
    // is small enough that a live test is the natural verification.

    #[test]
    fn pool_for_routing_math_matches_drift_guard_pairs() {
        // Pin the same (execution_id, shard_count) -> shard_index
        // mapping the R3b drift-guard asserts across sources.
        // If DbPoolMap::pool_for ever stops calling shard_for,
        // these pins still document the contract.
        assert_eq!(shard_for(1, 2), 1);
        assert_eq!(shard_for(1, 4), 1);
        assert_eq!(shard_for(1, 16), 5);
        assert_eq!(shard_for(1, 64), 21);
        assert_eq!(shard_for(1, 1024), 405);
    }

    #[test]
    fn pool_for_degenerate_shard_count_short_circuits() {
        // shard_count = 1 must return shard 0 without hashing.
        // Pin both the helper and a representative execution_id
        // to keep this honest if shard_for ever changes its
        // shard_count <= 1 short-circuit.
        assert_eq!(shard_for(42, 1), 0);
        assert_eq!(shard_for(9_999_999_999, 1), 0);
        assert_eq!(shard_for(-1, 1), 0);
    }

    // ----- DbPoolMap::from_single_pool (R4-2) ---------------------------------

    // The `from_single_pool` constructor lets `AppState::new_legacy`
    // (Phase F R4-2) wrap an already-created `DbPool` without
    // re-resolving `ShardingConfig`.  These tests don't need a
    // live Postgres — they exercise the struct shape only.
    // Building a `PgPool` without connecting requires sqlx's
    // `PgPoolOptions::connect_lazy_with`; we use that to fabricate
    // a dummy pool whose accessor identity we then verify.

    fn dummy_pool() -> DbPool {
        use sqlx::postgres::PgConnectOptions;
        PgPoolOptions::new()
            .max_connections(1)
            .connect_lazy_with(PgConnectOptions::new().host("localhost"))
    }

    #[tokio::test]
    async fn from_single_pool_marks_fallback_mode() {
        let pool = dummy_pool();
        let map = DbPoolMap::from_single_pool(pool);
        assert!(map.is_single_pool());
        assert_eq!(map.shard_count(), 1);
        // pool_for must short-circuit and not hash; the value
        // we return for any execution_id is the only pool.
        // (We don't compare the pool by identity here — sqlx
        // doesn't expose Arc internals — but we do verify
        // `shard_count() == 1` and that `all_shards()` yields
        // exactly one entry.)
        assert_eq!(map.all_shards().count(), 1);
    }

    #[tokio::test]
    async fn from_single_pool_pool_for_does_not_panic_on_negative_eid() {
        // Regression guard: `shard_for(-1, 1)` short-circuits to
        // 0; pool_for indexes into `shards[0]`.  Make sure the
        // single-pool path is safe for the i64-extreme inputs
        // the R3b drift-guard exercises.
        let map = DbPoolMap::from_single_pool(dummy_pool());
        let _ = map.pool_for(-1);
        let _ = map.pool_for(i64::MAX);
        let _ = map.pool_for(0);
    }
}
