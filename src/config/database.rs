//! Database configuration for PostgreSQL connection.

use serde::Deserialize;
use sqlx::postgres::PgConnectOptions;

/// Database configuration loaded from environment variables.
///
/// Environment variables are prefixed with `POSTGRES_`:
/// - `POSTGRES_HOST`: Database host (default: "localhost")
/// - `POSTGRES_PORT`: Database port (default: "5432")
/// - `POSTGRES_USER`: Database user
/// - `POSTGRES_PASSWORD`: Database password
/// - `POSTGRES_DATABASE`: Database name (default: "noetl")
///
/// Additional configuration:
/// - `NOETL_SCHEMA`: Database schema (default: "noetl")
/// - `DATABASE_URL`: Full connection URL (overrides individual settings)
#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    /// Database host
    #[serde(default = "default_host")]
    pub host: String,

    /// Database port
    #[serde(default = "default_port")]
    pub port: String,

    /// Database user
    #[serde(default = "default_user")]
    pub user: String,

    /// Database password
    #[serde(default)]
    pub password: String,

    /// Database name
    #[serde(default = "default_database")]
    pub database: String,

    /// Maximum connections in the pool
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,

    /// Minimum connections in the pool
    #[serde(default = "default_min_connections")]
    pub min_connections: u32,

    /// Connection acquire timeout in seconds
    #[serde(default = "default_acquire_timeout")]
    pub acquire_timeout: u64,
}

fn default_host() -> String {
    "localhost".to_string()
}

fn default_port() -> String {
    "5432".to_string()
}

fn default_user() -> String {
    "noetl".to_string()
}

fn default_database() -> String {
    "noetl".to_string()
}

/// Maximum Postgres connections this server process holds.
///
/// ⚠ **This is a shared pool.** With `shard_count <= 1` — prod's configuration —
/// [`crate::db::pool::DbPoolMap::pool_for`] returns the same handle for every
/// shard *and* for cluster tables, so every `/api/execute`, every read, and
/// every background task draw from this one budget. As of 2026-09-02 that is
/// five long-lived pollers (orchestrator reconcile on an 8 s tick, orphan
/// sweep, non-convergence sweep, cross-store parity sampler, projection parity
/// sampler) plus all request traffic.
///
/// At the previous value of **10** that left roughly five connections for
/// requests, and `POST /api/execute` — which makes several sequential
/// round-trips per call — exhausted it at **six concurrent clients**. Every
/// request then blocked in `acquire()` for the full
/// [`default_acquire_timeout`] of 30 s, which is exactly where an in-cluster
/// load run saw them die. It presented as a *collapse* rather than a plateau:
/// concurrency 1-2 fine, 4 degraded, 6 total stall with **no executions
/// started and no ERROR logged**, because a waiting `acquire` has nothing to
/// say (noetl/ai-meta#317).
///
/// **25 is chosen to match pgbouncer's `default_pool_size = 25`**, which is the
/// real ceiling: pgbouncer pools per (database, user) pair, so a server pool
/// larger than that would not buy concurrency, it would just move the queue one
/// hop downstream where it is even harder to see. `max_db_connections = 32`
/// leaves headroom above it for other users.
///
/// ⚠ **These two numbers are coupled and must move together.** Raising this
/// above pgbouncer's `default_pool_size`, or running more than one server
/// replica without raising pgbouncer, re-creates the same queue somewhere less
/// observable. Prod runs **one** replica; 25 x 1 <= 25 holds.
fn default_max_connections() -> u32 {
    25
}

fn default_min_connections() -> u32 {
    1
}

fn default_acquire_timeout() -> u64 {
    30
}

/// Per-connection prepared-statement-cache capacity, from the
/// environment.
///
/// `NOETL_PG_STATEMENT_CACHE_CAPACITY` controls sqlx's prepared-
/// statement cache.  Unset → sqlx's own default (100), preserving
/// existing behaviour for direct/session-mode Postgres (kind, a
/// dedicated server).
///
/// Set it to `0` when the server connects through a **transaction-
/// mode** connection pooler (e.g. pgbouncer `pool_mode=transaction`
/// in front of Cloud SQL — the prod path).  Under transaction
/// pooling a cached prepared statement lives on a backend
/// connection the pooler can hand to a different client mid-session,
/// producing `prepared statement "sqlx_s_N" does not exist`.
/// Capacity `0` makes sqlx use one-shot unnamed statements, which
/// are safe under transaction pooling.
fn statement_cache_capacity_from_env() -> usize {
    std::env::var("NOETL_PG_STATEMENT_CACHE_CAPACITY")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(100)
}

/// Connection options for a single Postgres host — used by the
/// per-shard + cluster-wide pools in [`ShardingConfig`].
///
/// Phase F R4 introduces this as a lightweight DSN-style holder so
/// the `DbPoolMap` can carry N+1 [`PgConnectOptions`] without the
/// rest of [`DatabaseConfig`]'s pool-tuning fields (which apply
/// uniformly across all pools).
///
/// Parsed from a single `host=...&port=...&user=...&password=...&database=...`
/// query-string-ish DSN.  See [`ShardConnection::parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardConnection {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
}

impl ShardConnection {
    /// Parse a DSN of the form
    /// `host=postgres-0;port=5432;user=noetl;password=secret;database=noetl`
    /// (semicolon-separated key=value pairs).  Order-independent.
    ///
    /// Picked semicolons (NOT `&` and NOT URL-encoded form) because
    /// the outer separator in `NOETL_SHARDS` is the comma, and we
    /// want DSN strings to be obviously distinct from URL query
    /// strings (operators copy-paste these into env files; the
    /// less ambiguity, the better).
    pub fn parse(dsn: &str) -> Result<Self, ShardConnectionError> {
        let mut host: Option<String> = None;
        let mut port: Option<u16> = None;
        let mut user: Option<String> = None;
        let mut password: Option<String> = None;
        let mut database: Option<String> = None;

        for pair in dsn.split(';').filter(|p| !p.trim().is_empty()) {
            let (key, value) = pair
                .split_once('=')
                .ok_or_else(|| ShardConnectionError::MalformedPair(pair.to_string()))?;
            let value = value.to_string();
            match key.trim() {
                "host" => host = Some(value),
                "port" => {
                    port = Some(
                        value
                            .parse()
                            .map_err(|_| ShardConnectionError::InvalidPort(value.clone()))?,
                    )
                }
                "user" => user = Some(value),
                "password" => password = Some(value),
                "database" | "dbname" => database = Some(value),
                other => return Err(ShardConnectionError::UnknownKey(other.to_string())),
            }
        }

        Ok(Self {
            host: host.ok_or(ShardConnectionError::MissingKey("host"))?,
            port: port.unwrap_or(5432),
            user: user.ok_or(ShardConnectionError::MissingKey("user"))?,
            password: password.unwrap_or_default(),
            database: database.unwrap_or_else(|| "noetl".to_string()),
        })
    }

    /// Build [`PgConnectOptions`] from this shard connection.
    pub fn connect_options(&self) -> PgConnectOptions {
        PgConnectOptions::new()
            .host(&self.host)
            .port(self.port)
            .username(&self.user)
            .password(&self.password)
            .database(&self.database)
            .statement_cache_capacity(statement_cache_capacity_from_env())
    }
}

/// Errors parsing a [`ShardConnection`] DSN.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ShardConnectionError {
    #[error("malformed key=value pair: {0}")]
    MalformedPair(String),
    #[error("missing required key: {0}")]
    MissingKey(&'static str),
    #[error("unknown key: {0}")]
    UnknownKey(String),
    #[error("invalid port: {0}")]
    InvalidPort(String),
}

/// Sharding configuration — per-shard Postgres DSNs + a separate
/// cluster-wide DSN for the always-master tables (catalog,
/// credential, keychain, runtime, schedule, resource, manifest).
///
/// Phase F R4 plumbs this through `AppState`.  When `shards` is
/// empty, the server runs in single-pool fallback mode (current
/// shape — every query goes through the legacy [`DatabaseConfig`]
/// pool).  When `shards` is non-empty, [`DbPoolMap`] holds N
/// per-shard pools picked by [`crate::sharding::shard_for`] and an
/// optional separate cluster pool; when `cluster` is `None`, the
/// cluster-wide tables ride on shard 0's pool (degenerate but
/// useful for single-node kind validation).
///
/// Parsed from env vars:
///
/// - `NOETL_SHARDS` — comma-separated list of shard DSNs.  Empty
///   string or unset → single-pool fallback.
/// - `NOETL_CLUSTER_DSN` — optional DSN for the cluster-wide pool.
#[derive(Debug, Clone, Default)]
pub struct ShardingConfig {
    /// Per-shard connections, in shard-index order.  Position N
    /// in this vec is the DSN for shard N (matching
    /// `shard_for(execution_id, shards.len()) == N`).
    pub shards: Vec<ShardConnection>,
    /// Optional cluster-wide pool DSN.  When `None`, cluster-wide
    /// queries ride on `shards[0]` (or fall back to the legacy
    /// pool if `shards` is also empty).
    pub cluster: Option<ShardConnection>,
}

impl ShardingConfig {
    /// Load sharding config from env vars.
    ///
    /// `NOETL_SHARDS` (default empty) is comma-separated; each
    /// segment is parsed via [`ShardConnection::parse`].  Empty
    /// segments are skipped — `NOETL_SHARDS=""` yields an empty
    /// `shards` vec (single-pool fallback).
    ///
    /// `NOETL_CLUSTER_DSN` (default empty) is a single DSN.
    pub fn from_env() -> Result<Self, ShardConnectionError> {
        let shards_raw = std::env::var("NOETL_SHARDS").unwrap_or_default();
        let cluster_raw = std::env::var("NOETL_CLUSTER_DSN").unwrap_or_default();

        let shards = shards_raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ShardConnection::parse)
            .collect::<Result<Vec<_>, _>>()?;

        let cluster = if cluster_raw.trim().is_empty() {
            None
        } else {
            Some(ShardConnection::parse(&cluster_raw)?)
        };

        Ok(Self { shards, cluster })
    }

    /// Number of shards configured.  `0` = single-pool fallback.
    pub fn shard_count(&self) -> u32 {
        self.shards.len() as u32
    }

    /// True when sharding is OFF — server runs in single-pool
    /// fallback mode.
    pub fn is_disabled(&self) -> bool {
        self.shards.is_empty()
    }
}

/// Schema configuration loaded separately.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct SchemaConfig {
    /// Database schema
    #[serde(default = "default_schema")]
    pub schema: String,
}

fn default_schema() -> String {
    "noetl".to_string()
}

impl DatabaseConfig {
    /// Load configuration from environment variables.
    ///
    /// Environment variables are prefixed with `POSTGRES_`.
    pub fn from_env() -> Result<Self, envy::Error> {
        envy::prefixed("POSTGRES_").from_env::<DatabaseConfig>()
    }

    /// Get PostgreSQL connection options.
    pub fn connect_options(&self) -> PgConnectOptions {
        let port: u16 = self.port.parse().unwrap_or(5432);

        PgConnectOptions::new()
            .host(&self.host)
            .port(port)
            .username(&self.user)
            .password(&self.password)
            .database(&self.database)
            .statement_cache_capacity(statement_cache_capacity_from_env())
    }

    /// Get the connection URL string.
    pub fn connection_url(&self) -> String {
        format!(
            "postgres://{}:{}@{}:{}/{}",
            self.user, self.password, self.host, self.port, self.database
        )
    }
}

impl SchemaConfig {
    /// Load schema configuration from environment variables.
    ///
    /// Environment variables are prefixed with `NOETL_`.
    #[allow(dead_code)]
    pub fn from_env() -> Result<Self, envy::Error> {
        envy::prefixed("NOETL_").from_env::<SchemaConfig>()
    }
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            user: default_user(),
            password: String::new(),
            database: default_database(),
            max_connections: default_max_connections(),
            min_connections: default_min_connections(),
            acquire_timeout: default_acquire_timeout(),
        }
    }
}

impl Default for SchemaConfig {
    fn default() -> Self {
        Self {
            schema: default_schema(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = DatabaseConfig::default();
        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, "5432");
        assert_eq!(config.database, "noetl");
    }

    #[test]
    fn test_connection_url() {
        let config = DatabaseConfig {
            password: "secret".to_string(),
            ..DatabaseConfig::default()
        };
        assert_eq!(
            config.connection_url(),
            "postgres://noetl:secret@localhost:5432/noetl"
        );
    }

    // ----- ShardConnection parsing ------------------------------------------

    #[test]
    fn shard_connection_parse_full_dsn() {
        let dsn = "host=postgres-0;port=5432;user=noetl;password=secret;database=noetl_shard0";
        let conn = ShardConnection::parse(dsn).expect("parse");
        assert_eq!(conn.host, "postgres-0");
        assert_eq!(conn.port, 5432);
        assert_eq!(conn.user, "noetl");
        assert_eq!(conn.password, "secret");
        assert_eq!(conn.database, "noetl_shard0");
    }

    #[test]
    fn shard_connection_parse_uses_defaults() {
        let dsn = "host=p0;user=noetl";
        let conn = ShardConnection::parse(dsn).expect("parse");
        assert_eq!(conn.host, "p0");
        assert_eq!(conn.port, 5432);
        assert_eq!(conn.password, "");
        assert_eq!(conn.database, "noetl");
    }

    #[test]
    fn shard_connection_parse_accepts_dbname_alias() {
        let dsn = "host=p0;user=noetl;dbname=noetl_shard1";
        let conn = ShardConnection::parse(dsn).expect("parse");
        assert_eq!(conn.database, "noetl_shard1");
    }

    #[test]
    fn shard_connection_parse_rejects_missing_host() {
        assert_eq!(
            ShardConnection::parse("user=noetl"),
            Err(ShardConnectionError::MissingKey("host"))
        );
    }

    #[test]
    fn shard_connection_parse_rejects_missing_user() {
        assert_eq!(
            ShardConnection::parse("host=p0"),
            Err(ShardConnectionError::MissingKey("user"))
        );
    }

    #[test]
    fn shard_connection_parse_rejects_unknown_key() {
        let err = ShardConnection::parse("host=p0;user=noetl;sslmode=require").unwrap_err();
        assert_eq!(err, ShardConnectionError::UnknownKey("sslmode".into()));
    }

    #[test]
    fn shard_connection_parse_rejects_malformed_pair() {
        let err = ShardConnection::parse("host=p0;bogus;user=noetl").unwrap_err();
        assert_eq!(err, ShardConnectionError::MalformedPair("bogus".into()));
    }

    #[test]
    fn shard_connection_parse_rejects_invalid_port() {
        let err = ShardConnection::parse("host=p0;port=abc;user=noetl").unwrap_err();
        assert_eq!(err, ShardConnectionError::InvalidPort("abc".into()));
    }

    #[test]
    fn shard_connection_parse_tolerates_trailing_separator() {
        let conn = ShardConnection::parse("host=p0;user=noetl;").expect("parse");
        assert_eq!(conn.host, "p0");
    }

    // ----- ShardingConfig from_env ------------------------------------------

    /// Serialises the env-mutating sharding tests against each other.
    ///
    /// `cargo test` runs tests on a thread pool; env vars are process-wide. An
    /// earlier note here said to pass `--test-threads=1` "if you add more, or
    /// guard with a mutex" — the tests already raced without either.
    /// `sharding_config_disabled_when_env_unset` asserts `NOETL_SHARDS` is
    /// absent while its three siblings are setting it, so it failed on
    /// `cfg.cluster.is_none()` in **22 of 40** filtered runs.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Restores the two env vars on drop, so a panicking test body cannot leak
    /// its values into whichever test acquires the lock next — which would turn
    /// one failure into a cascade.
    struct EnvRestore {
        shards: Option<String>,
        cluster: Option<String>,
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match &self.shards {
                Some(v) => std::env::set_var("NOETL_SHARDS", v),
                None => std::env::remove_var("NOETL_SHARDS"),
            }
            match &self.cluster {
                Some(v) => std::env::set_var("NOETL_CLUSTER_DSN", v),
                None => std::env::remove_var("NOETL_CLUSTER_DSN"),
            }
        }
    }

    fn with_env<F: FnOnce() -> R, R>(shards: Option<&str>, cluster: Option<&str>, f: F) -> R {
        // A poisoned lock means a previous test panicked; its EnvRestore still
        // ran, so recover rather than cascade a second failure.
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _restore = EnvRestore {
            shards: std::env::var("NOETL_SHARDS").ok(),
            cluster: std::env::var("NOETL_CLUSTER_DSN").ok(),
            _guard: guard,
        };

        match shards {
            Some(v) => std::env::set_var("NOETL_SHARDS", v),
            None => std::env::remove_var("NOETL_SHARDS"),
        }
        match cluster {
            Some(v) => std::env::set_var("NOETL_CLUSTER_DSN", v),
            None => std::env::remove_var("NOETL_CLUSTER_DSN"),
        }

        f()
    }

    #[test]
    fn sharding_config_disabled_when_env_unset() {
        with_env(None, None, || {
            let cfg = ShardingConfig::from_env().expect("from_env");
            assert!(cfg.is_disabled());
            assert_eq!(cfg.shard_count(), 0);
            assert!(cfg.cluster.is_none());
        });
    }

    #[test]
    fn sharding_config_disabled_on_empty_string() {
        with_env(Some(""), Some(""), || {
            let cfg = ShardingConfig::from_env().expect("from_env");
            assert!(cfg.is_disabled());
        });
    }

    #[test]
    fn sharding_config_parses_two_shards() {
        with_env(
            Some("host=p0;user=noetl,host=p1;user=noetl"),
            Some("host=pc;user=noetl"),
            || {
                let cfg = ShardingConfig::from_env().expect("from_env");
                assert_eq!(cfg.shard_count(), 2);
                assert!(!cfg.is_disabled());
                assert_eq!(cfg.shards[0].host, "p0");
                assert_eq!(cfg.shards[1].host, "p1");
                assert_eq!(cfg.cluster.as_ref().unwrap().host, "pc");
            },
        );
    }

    #[test]
    fn sharding_config_skips_empty_segments() {
        with_env(
            Some(",host=p0;user=noetl,,host=p1;user=noetl,"),
            None,
            || {
                let cfg = ShardingConfig::from_env().expect("from_env");
                assert_eq!(cfg.shard_count(), 2);
            },
        );
    }
}

#[cfg(test)]
mod pool_sizing_tests {
    use super::*;

    /// pgbouncer pools per (database, user) pair. The server pool must not
    /// exceed it, or the queue simply moves one hop downstream where it is
    /// harder to see. Live value on prod 2026-09-02.
    const PGBOUNCER_DEFAULT_POOL_SIZE: u32 = 25;

    /// Concurrent requests the pool must still serve after every long-lived
    /// background task has taken a connection.
    const MIN_REQUEST_HEADROOM: u32 = 15;

    /// Count the long-lived background tasks that draw on the shared pool.
    ///
    /// Counts CODE in main.rs, not prose — the bug was that five pollers plus
    /// request traffic exceeded a pool of ten, and the way that recurs is
    /// someone adding a sixth poller without touching this file.
    fn background_pool_consumers() -> u32 {
        let main_rs = include_str!("../main.rs");
        main_rs
            .lines()
            .map(str::trim_start)
            .filter(|l| !l.starts_with("//"))
            .filter(|l| l.contains("spawn_") && l.contains("state.clone()"))
            .count() as u32
    }

    /// ⚠ THE REGRESSION GUARD for noetl/ai-meta#317.
    ///
    /// At `max_connections = 10` with five background pollers, roughly five
    /// connections were left for requests, and `POST /api/execute` — several
    /// sequential round-trips per call — exhausted the pool at SIX concurrent
    /// clients. Every request then blocked the full 30 s `acquire_timeout`,
    /// with no execution started and no ERROR logged.
    #[test]
    fn the_pool_serves_the_background_tasks_and_still_leaves_headroom() {
        let bg = background_pool_consumers();
        assert!(
            bg > 0,
            "found no background pool consumers — the guard is anchored on \
             `spawn_*(state.clone())` in main.rs and has stopped matching, so it \
             would pass vacuously"
        );
        let need = bg + MIN_REQUEST_HEADROOM;
        assert!(
            default_max_connections() >= need,
            "pool of {} cannot serve {bg} background tasks plus {MIN_REQUEST_HEADROOM} \
             concurrent requests (needs >= {need}). This is exactly noetl/ai-meta#317: \
             requests block in acquire() for the full 30s timeout with nothing logged.",
            default_max_connections()
        );
    }

    /// The other side of the same coupling: bigger is not free.
    #[test]
    fn the_pool_does_not_exceed_the_pgbouncer_per_user_cap() {
        assert!(
            default_max_connections() <= PGBOUNCER_DEFAULT_POOL_SIZE,
            "a server pool of {} above pgbouncer's default_pool_size of {} does not buy \
             concurrency — it moves the queue one hop downstream, where there is no \
             metric at all. Raise pgbouncer first, and remember prod runs 1 replica: \
             N replicas x this value must also fit.",
            default_max_connections(),
            PGBOUNCER_DEFAULT_POOL_SIZE
        );
    }

    /// ⚠ NEGATIVE CONTROL. Without it the two assertions above are satisfied by
    /// any value in a wide band, and a nonsense pool (say 1, or 10_000) could
    /// still pass one of them. This pins that BOTH bounds bite.
    #[test]
    fn negative_control_the_old_value_would_fail_the_guard() {
        let bg = background_pool_consumers();
        let old = 10u32;
        assert!(
            old < bg + MIN_REQUEST_HEADROOM,
            "the pre-fix value of 10 must FAIL the headroom guard, otherwise the guard \
             would not have caught the bug it was written for"
        );
        assert!(
            10_000 > PGBOUNCER_DEFAULT_POOL_SIZE,
            "an absurdly large pool must fail the downstream-cap guard"
        );
    }

    /// The acquire timeout is what turns exhaustion into a 30s hang rather than
    /// a fast failure. Pinned so a change is deliberate and reviewed.
    #[test]
    fn the_acquire_timeout_is_pinned() {
        assert_eq!(
            default_acquire_timeout(),
            30,
            "acquire_timeout is why pool exhaustion presents as a 30s hang that looks \
             exactly like a network fault; changing it changes the failure SHAPE"
        );
    }
}
