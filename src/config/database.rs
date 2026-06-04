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

fn default_max_connections() -> u32 {
    10
}

fn default_min_connections() -> u32 {
    1
}

fn default_acquire_timeout() -> u64 {
    30
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
        let mut config = DatabaseConfig::default();
        config.password = "secret".to_string();
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

    // NOTE: these tests mutate process-wide env vars.  Run with
    // `cargo test -- --test-threads=1` if you add more, or guard
    // with a mutex.  The shape today is deliberately small so the
    // serial cost is negligible.

    fn with_env<F: FnOnce() -> R, R>(shards: Option<&str>, cluster: Option<&str>, f: F) -> R {
        let prev_shards = std::env::var("NOETL_SHARDS").ok();
        let prev_cluster = std::env::var("NOETL_CLUSTER_DSN").ok();

        match shards {
            Some(v) => std::env::set_var("NOETL_SHARDS", v),
            None => std::env::remove_var("NOETL_SHARDS"),
        }
        match cluster {
            Some(v) => std::env::set_var("NOETL_CLUSTER_DSN", v),
            None => std::env::remove_var("NOETL_CLUSTER_DSN"),
        }

        let out = f();

        match prev_shards {
            Some(v) => std::env::set_var("NOETL_SHARDS", v),
            None => std::env::remove_var("NOETL_SHARDS"),
        }
        match prev_cluster {
            Some(v) => std::env::set_var("NOETL_CLUSTER_DSN", v),
            None => std::env::remove_var("NOETL_CLUSTER_DSN"),
        }

        out
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
