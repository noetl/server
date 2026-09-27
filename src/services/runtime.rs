//! Runtime management service.
//!
//! Provides operations for managing worker pools, API servers,
//! and brokers in the NoETL runtime.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::db::DbPool;
use crate::error::{AppError, AppResult};

/// Runtime kind.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    WorkerPool,
    ServerApi,
    Broker,
}

impl RuntimeKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RuntimeKind::WorkerPool => "worker_pool",
            RuntimeKind::ServerApi => "server_api",
            RuntimeKind::Broker => "broker",
        }
    }
}

impl std::str::FromStr for RuntimeKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "worker_pool" => Ok(RuntimeKind::WorkerPool),
            "server_api" => Ok(RuntimeKind::ServerApi),
            "broker" => Ok(RuntimeKind::Broker),
            _ => Err(format!("Unknown runtime kind: {}", s)),
        }
    }
}

/// Runtime entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Runtime {
    pub runtime_id: i64,
    pub name: String,
    pub kind: String,
    pub uri: Option<String>,
    pub status: String,
    pub labels: Option<serde_json::Value>,
    pub capabilities: Option<serde_json::Value>,
    pub capacity: Option<i32>,
    pub runtime: Option<serde_json::Value>,
    pub heartbeat: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Is this registration LIVE — i.e. did it heartbeat within
    /// `NOETL_ORPHAN_WORKER_TTL_SECS`?
    ///
    /// Derived, never stored.  `status` is the worker's own last self-report and
    /// keeps saying `"ready"` for as long as the row survives, so a crashed pod
    /// reads as a ready worker until the hourly cleanup removes it — up to ~2h.
    /// Every liveness CONSUMER already asks the right question
    /// (`orphan_sweep.rs`, `nonconvergence_sweep.rs`: `heartbeat >= NOW() -
    /// INTERVAL '1 second' * orphan_worker_ttl_secs`); only the REPORT did not,
    /// which is how a pool of 5 live and 3 dead registrations gets read as an
    /// under-provisioned pool with broken cleanup.  Answering it here with the
    /// same TTL the sweeps use means the report and dispatch can never disagree.
    #[serde(default)]
    pub live: bool,
    /// Seconds since this registration last heartbeat — the number that makes a
    /// stale row obvious without the reader doing clock arithmetic.
    #[serde(default)]
    pub heartbeat_age_seconds: i64,
}

/// Is a registration live, and how old is its heartbeat?
///
/// The single definition of the question, so the reported liveness cannot drift
/// from the liveness the sweeps enforce.  Pure, so it is testable without a
/// database or a clock.
pub fn runtime_liveness(
    heartbeat: DateTime<Utc>,
    now: DateTime<Utc>,
    ttl_secs: i64,
) -> (bool, i64) {
    let age = (now - heartbeat).num_seconds();
    (age <= ttl_secs, age)
}

/// Request to register a runtime.
///
/// Wire shape compatible with both the Rust noetl-worker (which
/// sends `component_type`) and the Python server's
/// `RuntimeRegistrationRequest` (which also sends `component_type`
/// — kept the same field name for parity).  The Rust server's
/// canonical name for the routing dimension is `kind`, so the
/// alias maps `component_type` onto it.  Defaulting `kind` to
/// `worker_pool` lets handlers used downstream (e.g. heartbeat)
/// accept the worker's minimal payload as well.  See
/// noetl/ai-meta#53 Gap 2.
#[derive(Debug, Clone, Deserialize)]
pub struct RegisterRuntimeRequest {
    pub name: String,
    #[serde(default = "default_kind", alias = "component_type")]
    pub kind: String,
    pub uri: Option<String>,
    #[serde(default = "default_status")]
    pub status: String,
    pub labels: Option<serde_json::Value>,
    pub capabilities: Option<serde_json::Value>,
    pub capacity: Option<i32>,
    pub runtime: Option<serde_json::Value>,
    // Accepted but not persisted — the worker sends a hostname for
    // operator visibility; we just record it as a label below if
    // labels are empty.  Captured here so serde doesn't reject the
    // field as unknown when `deny_unknown_fields` is enabled in
    // the future.
    #[serde(default)]
    pub hostname: Option<String>,
}

fn default_kind() -> String {
    "worker_pool".to_string()
}

fn default_status() -> String {
    "active".to_string()
}

/// Filter for listing runtimes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeFilter {
    pub kind: Option<String>,
    pub status: Option<String>,
    pub name: Option<String>,
}

/// Runtime management service.
#[derive(Clone)]
pub struct RuntimeService {
    db: DbPool,
    snowflake: std::sync::Arc<crate::snowflake::SnowflakeGenerator>,
    /// The liveness TTL the sweeps use (`NOETL_ORPHAN_WORKER_TTL_SECS`), so a
    /// reported `live` can never disagree with the sweeps' verdict.
    liveness_ttl_secs: i64,
}

impl RuntimeService {
    /// Create a new runtime service.
    ///
    /// `snowflake` is the application-side ID generator shared
    /// with `AppState` and the other services.  Phase F R1.5 of
    /// noetl/ai-meta#49 moved id generation out of the DB-side
    /// `noetl.snowflake_id()` function.
    pub fn new(
        db: DbPool,
        snowflake: std::sync::Arc<crate::snowflake::SnowflakeGenerator>,
        liveness_ttl_secs: u64,
    ) -> Self {
        Self {
            db,
            snowflake,
            liveness_ttl_secs: liveness_ttl_secs as i64,
        }
    }

    /// Register a new runtime (worker pool, server, or broker).
    pub async fn register(&self, request: &RegisterRuntimeRequest) -> AppResult<Runtime> {
        // Validate kind
        let kind = match request.kind.as_str() {
            "worker_pool" | "server_api" | "broker" => request.kind.as_str(),
            _ => {
                return Err(AppError::Validation(format!(
                    "Invalid runtime kind: {}",
                    request.kind
                )))
            }
        };

        // Generate runtime_id via the application-side snowflake
        // generator (Phase F R1.5 of noetl/ai-meta#49).  Wrap in
        // a 1-tuple to keep the existing destructuring shape
        // below.
        let runtime_id: (i64,) = (self.snowflake.generate()?,);

        let now = Utc::now();

        // Check if runtime with same kind and name exists
        let existing: Option<(i64,)> =
            sqlx::query_as("SELECT runtime_id FROM noetl.runtime WHERE kind = $1 AND name = $2")
                .bind(kind)
                .bind(&request.name)
                .fetch_optional(&self.db)
                .await?;

        if let Some((existing_id,)) = existing {
            // Update existing runtime
            sqlx::query(
                r#"
                UPDATE noetl.runtime SET
                    uri = $1,
                    status = $2,
                    labels = $3,
                    capabilities = $4,
                    capacity = $5,
                    runtime = $6,
                    heartbeat = $7,
                    updated_at = $7
                WHERE runtime_id = $8
                "#,
            )
            .bind(&request.uri)
            .bind(&request.status)
            .bind(&request.labels)
            .bind(&request.capabilities)
            .bind(request.capacity)
            .bind(&request.runtime)
            .bind(now)
            .bind(existing_id)
            .execute(&self.db)
            .await?;

            return self.get(existing_id).await;
        }

        // Insert new runtime
        sqlx::query(
            r#"
            INSERT INTO noetl.runtime (
                runtime_id, name, kind, uri, status,
                labels, capabilities, capacity, runtime,
                heartbeat, created_at, updated_at
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $10, $10)
            "#,
        )
        .bind(runtime_id.0)
        .bind(&request.name)
        .bind(kind)
        .bind(&request.uri)
        .bind(&request.status)
        .bind(&request.labels)
        .bind(&request.capabilities)
        .bind(request.capacity)
        .bind(&request.runtime)
        .bind(now)
        .execute(&self.db)
        .await?;

        self.get(runtime_id.0).await
    }

    /// Deregister a runtime.
    pub async fn deregister(&self, kind: &str, name: &str) -> AppResult<()> {
        let result = sqlx::query("DELETE FROM noetl.runtime WHERE kind = $1 AND name = $2")
            .bind(kind)
            .bind(name)
            .execute(&self.db)
            .await?;

        if result.rows_affected() == 0 {
            return Err(AppError::NotFound(format!(
                "Runtime not found: {} {}",
                kind, name
            )));
        }

        Ok(())
    }

    /// Look up the X25519 sealing public key a worker registered with itself.
    ///
    /// Secrets Wallet Phase 5b (noetl/ai-meta#61).  Workers opt into sealed
    /// credential delivery by including a base64 32-byte X25519 public key in
    /// the `runtime` JSON blob they pass to register, e.g.:
    ///
    /// ```json
    /// {
    ///   "worker_public_key": "<base64(32-byte-x25519-pub)>"
    /// }
    /// ```
    ///
    /// This is the JSON shape the server already persists as the `runtime`
    /// column; no schema migration is needed and workers that don't opt in
    /// keep working unchanged (the sealing endpoint just returns
    /// `BadRequest` for those workers).
    ///
    /// Returns `Ok(Some(pk))` when the lookup succeeds, `Ok(None)` when the
    /// worker exists but didn't register a key, and an error when the
    /// `worker_pool` runtime row doesn't exist or the key is malformed.
    pub async fn get_worker_public_key(&self, worker_name: &str) -> AppResult<Option<[u8; 32]>> {
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};

        let row: Option<(Option<serde_json::Value>,)> = sqlx::query_as(
            "SELECT runtime FROM noetl.runtime WHERE kind = 'worker_pool' AND name = $1",
        )
        .bind(worker_name)
        .fetch_optional(&self.db)
        .await?;
        let runtime_json = match row {
            Some((Some(v),)) => v,
            Some((None,)) | None => {
                // Row missing entirely, OR runtime metadata column is NULL —
                // either way the worker did not opt into sealing.  The
                // caller surfaces the right error message.
                return Ok(None);
            }
        };
        let encoded = match runtime_json
            .get("worker_public_key")
            .and_then(|v| v.as_str())
        {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(None),
        };
        let bytes = B64.decode(encoded).map_err(|e| {
            AppError::BadRequest(format!(
                "worker '{worker_name}' worker_public_key base64: {e}"
            ))
        })?;
        let array: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            AppError::BadRequest(format!(
                "worker '{worker_name}' worker_public_key must be 32 bytes, got {}",
                bytes.len()
            ))
        })?;
        Ok(Some(array))
    }

    /// Update heartbeat for a runtime.
    pub async fn heartbeat(&self, kind: &str, name: &str) -> AppResult<()> {
        let result = sqlx::query(
            "UPDATE noetl.runtime SET heartbeat = NOW(), updated_at = NOW() WHERE kind = $1 AND name = $2",
        )
        .bind(kind)
        .bind(name)
        .execute(&self.db)
        .await?;

        if result.rows_affected() == 0 {
            return Err(AppError::NotFound(format!(
                "Runtime not found: {} {}",
                kind, name
            )));
        }

        Ok(())
    }

    /// Get a runtime by ID.
    #[allow(clippy::type_complexity)]
    pub async fn get(&self, runtime_id: i64) -> AppResult<Runtime> {
        let row: Option<(
            i64,
            String,
            String,
            Option<String>,
            String,
            Option<serde_json::Value>,
            Option<serde_json::Value>,
            Option<i32>,
            Option<serde_json::Value>,
            DateTime<Utc>,
            DateTime<Utc>,
            DateTime<Utc>,
        )> = sqlx::query_as(
            r#"
            SELECT runtime_id, name, kind, uri, status,
                   labels, capabilities, capacity, runtime,
                   heartbeat, created_at, updated_at
            FROM noetl.runtime
            WHERE runtime_id = $1
            "#,
        )
        .bind(runtime_id)
        .fetch_optional(&self.db)
        .await?;

        match row {
            Some((
                runtime_id,
                name,
                kind,
                uri,
                status,
                labels,
                capabilities,
                capacity,
                runtime,
                heartbeat,
                created_at,
                updated_at,
            )) => {
                let (live, heartbeat_age_seconds) =
                    runtime_liveness(heartbeat, Utc::now(), self.liveness_ttl_secs);
                Ok(Runtime {
                    runtime_id,
                    name,
                    kind,
                    uri,
                    status,
                    labels,
                    capabilities,
                    capacity,
                    runtime,
                    heartbeat,
                    created_at,
                    updated_at,
                    live,
                    heartbeat_age_seconds,
                })
            }
            None => Err(AppError::NotFound(format!(
                "Runtime not found: {}",
                runtime_id
            ))),
        }
    }

    /// List runtimes with optional filters.
    #[allow(clippy::type_complexity)]
    pub async fn list(&self, filter: &RuntimeFilter) -> AppResult<Vec<Runtime>> {
        let rows: Vec<(
            i64,
            String,
            String,
            Option<String>,
            String,
            Option<serde_json::Value>,
            Option<serde_json::Value>,
            Option<i32>,
            Option<serde_json::Value>,
            DateTime<Utc>,
            DateTime<Utc>,
            DateTime<Utc>,
        )> = sqlx::query_as(
            r#"
            SELECT runtime_id, name, kind, uri, status,
                   labels, capabilities, capacity, runtime,
                   heartbeat, created_at, updated_at
            FROM noetl.runtime
            WHERE ($1::TEXT IS NULL OR kind = $1)
              AND ($2::TEXT IS NULL OR status = $2)
              AND ($3::TEXT IS NULL OR name LIKE $3)
            ORDER BY kind, name
            "#,
        )
        .bind(&filter.kind)
        .bind(&filter.status)
        .bind(filter.name.as_ref().map(|n| format!("%{}%", n)))
        .fetch_all(&self.db)
        .await?;

        // One clock read for the whole response: two rows with identical
        // heartbeats must never be reported with different liveness.
        let now = Utc::now();
        Ok(rows
            .into_iter()
            .map(
                |(
                    runtime_id,
                    name,
                    kind,
                    uri,
                    status,
                    labels,
                    capabilities,
                    capacity,
                    runtime,
                    heartbeat,
                    created_at,
                    updated_at,
                )| {
                    let (live, heartbeat_age_seconds) =
                        runtime_liveness(heartbeat, now, self.liveness_ttl_secs);
                    Runtime {
                        runtime_id,
                        name,
                        kind,
                        uri,
                        status,
                        labels,
                        capabilities,
                        capacity,
                        runtime,
                        heartbeat,
                        created_at,
                        updated_at,
                        live,
                        heartbeat_age_seconds,
                    }
                },
            )
            .collect())
    }

    /// List worker pools specifically.
    pub async fn list_worker_pools(&self) -> AppResult<Vec<Runtime>> {
        self.list(&RuntimeFilter {
            kind: Some("worker_pool".to_string()),
            ..Default::default()
        })
        .await
    }

    /// Update runtime status.
    pub async fn update_status(&self, runtime_id: i64, status: &str) -> AppResult<()> {
        let result = sqlx::query(
            "UPDATE noetl.runtime SET status = $1, updated_at = NOW() WHERE runtime_id = $2",
        )
        .bind(status)
        .bind(runtime_id)
        .execute(&self.db)
        .await?;

        if result.rows_affected() == 0 {
            return Err(AppError::NotFound(format!(
                "Runtime not found: {}",
                runtime_id
            )));
        }

        Ok(())
    }

    /// Clean up stale runtimes (no heartbeat for given duration).
    pub async fn cleanup_stale(&self, stale_after_seconds: i64) -> AppResult<i64> {
        let result = sqlx::query(
            r#"
            DELETE FROM noetl.runtime
            WHERE heartbeat < NOW() - INTERVAL '1 second' * $1
            "#,
        )
        .bind(stale_after_seconds)
        .execute(&self.db)
        .await?;

        Ok(result.rows_affected() as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_runtime_kind_from_str() {
        assert!(matches!(
            "worker_pool".parse::<RuntimeKind>().unwrap(),
            RuntimeKind::WorkerPool
        ));
        assert!(matches!(
            "server_api".parse::<RuntimeKind>().unwrap(),
            RuntimeKind::ServerApi
        ));
        assert!("invalid".parse::<RuntimeKind>().is_err());
    }

    /// The prod reading that sent a year of investigation at the wrong thing.
    ///
    /// On 2026-09-27 `/api/worker/pools` returned 11 rows, 3 of them with
    /// heartbeats 72 minutes old and every one of them `status: "ready"` —
    /// because `status` is the worker's own last self-report and a dead pod
    /// never files a correction.  Read as a pool census that is "2 live, 7
    /// stale", it looks like both under-provisioning AND broken cleanup, and it
    /// points at the claim loop.  It is neither: claim latency was ~190ms p50
    /// throughout, and every liveness CONSUMER already filtered on heartbeat
    /// freshness.  Only the report was silent about it.
    #[test]
    fn a_stale_registration_reports_itself_as_not_live() {
        let now = Utc::now();
        let ttl = 90; // NOETL_ORPHAN_WORKER_TTL_SECS default: 6 missed 15s beats

        // The three real rows, at their real ages.
        for age in [4324, 4325, 4333] {
            let (live, reported_age) =
                runtime_liveness(now - chrono::Duration::seconds(age), now, ttl);
            assert!(!live, "a {age}s-old heartbeat must not report as live");
            assert_eq!(reported_age, age, "the age must be reported, not hidden");
        }

        // And the five that genuinely were live, at their real ages.
        for age in [0, 1, 2, 4, 11, 14] {
            let (live, _) = runtime_liveness(now - chrono::Duration::seconds(age), now, ttl);
            assert!(live, "a {age}s-old heartbeat is a working worker");
        }
    }

    /// The boundary is inclusive, and one beat past the TTL is dead.
    ///
    /// The positive control matters here: without the `live` assertion,
    /// `runtime_liveness` could return `false` unconditionally and satisfy the
    /// test above while reporting the whole fleet dead.
    #[test]
    fn the_liveness_boundary_is_exactly_the_ttl() {
        let now = Utc::now();
        assert_eq!(
            runtime_liveness(now - chrono::Duration::seconds(90), now, 90),
            (true, 90),
            "exactly at the TTL is still live — the same comparison the sweeps make"
        );
        assert_eq!(
            runtime_liveness(now - chrono::Duration::seconds(91), now, 90),
            (false, 91),
            "one second past the TTL is dead"
        );
    }

    /// A clock skew that puts a heartbeat in the future must not read as dead.
    #[test]
    fn a_future_heartbeat_is_live() {
        let now = Utc::now();
        let (live, age) = runtime_liveness(now + chrono::Duration::seconds(5), now, 90);
        assert!(live, "a slightly-ahead worker clock is not a dead worker");
        assert!(age <= 0, "a future heartbeat has a non-positive age, got {age}");
    }

    #[test]
    fn test_runtime_kind_as_str() {
        assert_eq!(RuntimeKind::WorkerPool.as_str(), "worker_pool");
        assert_eq!(RuntimeKind::ServerApi.as_str(), "server_api");
        assert_eq!(RuntimeKind::Broker.as_str(), "broker");
    }

    #[test]
    fn test_runtime_serialization() {
        let runtime = Runtime {
            runtime_id: 12345,
            name: "worker-1".to_string(),
            kind: "worker_pool".to_string(),
            uri: Some("http://localhost:8080".to_string()),
            status: "active".to_string(),
            labels: Some(serde_json::json!({"env": "dev"})),
            capabilities: Some(serde_json::json!(["python", "http"])),
            capacity: Some(10),
            runtime: None,
            heartbeat: Utc::now(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            live: true,
            heartbeat_age_seconds: 0,
        };

        let json = serde_json::to_string(&runtime).unwrap();
        assert!(json.contains("worker-1"));
        assert!(json.contains("worker_pool"));
    }

    #[test]
    fn test_register_request_defaults() {
        let json = r#"{"name": "worker-1", "kind": "worker_pool"}"#;
        let request: RegisterRuntimeRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.status, "active");
    }

    #[test]
    fn test_register_accepts_component_type_alias() {
        // noetl/ai-meta#53 Gap 2: the Rust noetl-worker sends
        // `component_type` (matching the Python broker's wire
        // shape), not `kind`.  The Rust server must accept it.
        let json = r#"{
            "name": "worker-rust-pod-1",
            "component_type": "worker_pool",
            "runtime": "rust",
            "status": "ready",
            "hostname": "noetl-worker-rust-abc",
            "labels": {"pool_name": "worker-rust-pool"}
        }"#;
        let request: RegisterRuntimeRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.name, "worker-rust-pod-1");
        assert_eq!(request.kind, "worker_pool");
        assert_eq!(request.status, "ready");
        assert_eq!(request.runtime, Some(serde_json::json!("rust")));
        assert_eq!(request.hostname.as_deref(), Some("noetl-worker-rust-abc"));
    }

    #[test]
    fn test_register_defaults_kind_when_missing() {
        // If neither `kind` nor `component_type` is present, default
        // to `worker_pool`.  This matches the Python broker's lax
        // behaviour and unblocks heartbeat-style minimal payloads.
        let json = r#"{"name": "worker-1"}"#;
        let request: RegisterRuntimeRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.kind, "worker_pool");
    }

    #[test]
    fn test_runtime_filter_default() {
        let filter = RuntimeFilter::default();
        assert!(filter.kind.is_none());
        assert!(filter.status.is_none());
    }
}
