//! `noetl.result_store` queries (Result Store MVP,
//! [`noetl/ai-meta#70`](https://github.com/noetl/ai-meta/issues/70)).
//!
//! The table is created idempotently at server startup via
//! [`ensure_table`].  Insert is one row per `PUT /api/result/{eid}`
//! call; resolve is a point-lookup by `(execution_id, name,
//! result_id)` coming from the `noetl://` URI the server minted.
//!
//! Pattern mirrors `db::queries::secret_audit` — startup-time DDL,
//! no out-of-band migration file.

use chrono::{DateTime, Utc};
use sqlx::Row;

use crate::db::DbPool;
use crate::error::AppResult;

/// One row in `noetl.result_store`.
///
/// `result_id` is an application-side snowflake (per
/// `agents/rules/observability.md` Principle 3) so the URI can be
/// built before the DB round-trip.
#[derive(Debug, Clone)]
pub struct ResultStoreRow {
    /// Application-side snowflake id — primary key.
    pub result_id: i64,
    pub execution_id: i64,
    /// Logical name for the result (the `name` field in the PUT body,
    /// usually the step name).
    pub name: String,
    /// Lifecycle scope (`"execution"` is the only value the worker
    /// sends today; any short string is accepted).
    pub scope: String,
    /// Step that produced the result; optional.
    pub source_step: Option<String>,
    /// Stored payload (the full `data` JSON from the PUT body).
    pub data: serde_json::Value,
    /// Serialised byte count of the stored JSON.
    pub bytes: i64,
    /// SHA-256 hex digest of the serialised JSON.
    pub sha256: String,
    /// Media type of the payload (default `"application/json"`).
    pub media_type: String,
    pub created_at: DateTime<Utc>,
    /// Expiry — always `None` in the MVP.
    pub expires_at: Option<DateTime<Utc>>,
}

/// Idempotent table creation.  Runs once at startup so the schema
/// lands on first boot without an out-of-band migration step.  The
/// table is owned end-to-end by noetl-server.
pub async fn ensure_table(pool: &DbPool) -> AppResult<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS noetl.result_store (
            result_id    BIGINT PRIMARY KEY,
            execution_id BIGINT NOT NULL,
            name         TEXT NOT NULL,
            scope        TEXT NOT NULL,
            source_step  TEXT,
            data         JSONB NOT NULL,
            bytes        BIGINT NOT NULL,
            sha256       TEXT NOT NULL,
            media_type   TEXT NOT NULL DEFAULT 'application/json',
            created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
            expires_at   TIMESTAMPTZ
        )
        "#,
    )
    .execute(pool)
    .await?;

    // Index for resolve path: (execution_id, name) covers the most
    // common lookup — find the latest result for a given step in an
    // execution.
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS result_store_eid_name
        ON noetl.result_store (execution_id, name)
        "#,
    )
    .execute(pool)
    .await?;

    // Index for GC follow-ups: walk rows by creation time.
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS result_store_created_at
        ON noetl.result_store (created_at)
        "#,
    )
    .execute(pool)
    .await?;

    Ok(())
}

/// Insert one result-store row.  The caller has already generated the
/// snowflake `result_id` and computed `bytes` / `sha256`.
pub async fn insert(pool: &DbPool, row: &ResultStoreRow) -> AppResult<()> {
    sqlx::query(
        r#"
        INSERT INTO noetl.result_store (
            result_id, execution_id, name, scope, source_step,
            data, bytes, sha256, media_type, created_at, expires_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        ON CONFLICT (result_id) DO NOTHING
        "#,
    )
    .bind(row.result_id)
    .bind(row.execution_id)
    .bind(&row.name)
    .bind(&row.scope)
    .bind(row.source_step.as_deref())
    .bind(&row.data)
    .bind(row.bytes)
    .bind(&row.sha256)
    .bind(&row.media_type)
    .bind(row.created_at)
    .bind(row.expires_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// Point-lookup by `(execution_id, name, result_id)` — the three
/// components the resolver extracts from a `noetl://` URI.
///
/// Returns `None` when no matching row exists (caller maps to HTTP
/// 404).
pub async fn get_by_ref(
    pool: &DbPool,
    execution_id: i64,
    name: &str,
    result_id: i64,
) -> AppResult<Option<ResultStoreRow>> {
    let rows = sqlx::query(
        r#"
        SELECT
            result_id, execution_id, name, scope, source_step,
            data, bytes, sha256, media_type, created_at, expires_at
        FROM noetl.result_store
        WHERE execution_id = $1
          AND name = $2
          AND result_id = $3
        LIMIT 1
        "#,
    )
    .bind(execution_id)
    .bind(name)
    .bind(result_id)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().next().map(|r| ResultStoreRow {
        result_id: r.get::<i64, _>("result_id"),
        execution_id: r.get::<i64, _>("execution_id"),
        name: r.get::<String, _>("name"),
        scope: r.get::<String, _>("scope"),
        source_step: r.get::<Option<String>, _>("source_step"),
        data: r.get::<serde_json::Value, _>("data"),
        bytes: r.get::<i64, _>("bytes"),
        sha256: r.get::<String, _>("sha256"),
        media_type: r.get::<String, _>("media_type"),
        created_at: r.get::<DateTime<Utc>, _>("created_at"),
        expires_at: r.get::<Option<DateTime<Utc>>, _>("expires_at"),
    }))
}
