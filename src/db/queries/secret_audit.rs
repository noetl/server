//! `noetl.secret_audit` queries (Secrets Wallet Phase 7b.2,
//! [`noetl/ai-meta#61`](https://github.com/noetl/ai-meta/issues/61)).
//!
//! The table is created idempotently at server startup via
//! [`ensure_table`].  Insert is one row per credential resolution
//! outcome; query is bounded by `credential` / `from` / `to` /
//! `execution_id` filters with a hard `LIMIT` cap so an operator can't
//! accidentally pull millions of rows.

use chrono::{DateTime, Utc};
use sqlx::Row;

use crate::db::DbPool;
use crate::error::AppResult;
use crate::services::secret_audit::AuditEvent;

/// Hard cap on rows returned by the audit query endpoint — even if the
/// caller asks for more, only this many come back.  Operators pulling
/// for compliance reports paginate by adjusting `to` / `from`.
pub const QUERY_HARD_CAP: i64 = 10_000;

/// Idempotent table creation.  Runs once at startup so the schema
/// lands on first boot without operators needing to run a migration
/// out of band.  The table is `noetl/server`-owned end-to-end (no
/// other component writes to it) so a CREATE-on-startup is the right
/// shape.
pub async fn ensure_table(pool: &DbPool) -> AppResult<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS noetl.secret_audit (
            audit_id            BIGINT PRIMARY KEY,
            occurred_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
            credential          TEXT NOT NULL,
            operation           TEXT NOT NULL,
            outcome             TEXT NOT NULL,
            worker_id           TEXT,
            execution_id        BIGINT,
            parent_execution_id BIGINT,
            server_region       TEXT,
            broker_region       TEXT,
            kek_version         TEXT,
            notes               TEXT
        )
        "#,
    )
    .execute(pool)
    .await?;
    // Two indexes that the compliance-query shape needs.  Idempotent
    // (CREATE INDEX IF NOT EXISTS); cheap on an empty / lightly-used
    // table.
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS secret_audit_credential_time
        ON noetl.secret_audit (credential, occurred_at DESC)
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS secret_audit_execution
        ON noetl.secret_audit (execution_id)
        WHERE execution_id IS NOT NULL
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert one audit row.  Caller is the DB-backed `AuditSink`; this
/// function never logs the secret value (the `AuditEvent` itself never
/// carries one).
pub async fn insert(pool: &DbPool, event: &AuditEvent) -> AppResult<()> {
    sqlx::query(
        r#"
        INSERT INTO noetl.secret_audit (
            audit_id, occurred_at, credential, operation, outcome,
            worker_id, execution_id, parent_execution_id,
            server_region, broker_region, kek_version, notes
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
        ON CONFLICT (audit_id) DO NOTHING
        "#,
    )
    .bind(event.audit_id)
    .bind(event.occurred_at)
    .bind(&event.credential)
    .bind(&event.operation)
    .bind(&event.outcome)
    .bind(event.worker_id.as_deref())
    .bind(event.execution_id)
    .bind(event.parent_execution_id)
    .bind(event.server_region.as_deref())
    .bind(event.broker_region.as_deref())
    .bind(event.kek_version.as_deref())
    .bind(event.notes.as_deref())
    .execute(pool)
    .await?;
    Ok(())
}

/// Filter shape for the query endpoint.
#[derive(Debug, Clone, Default)]
pub struct AuditQuery {
    pub credential: Option<String>,
    pub execution_id: Option<i64>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub limit: Option<i64>,
}

/// Run the bounded query.  `limit` is capped at [`QUERY_HARD_CAP`].
/// Results are ordered by `occurred_at DESC` (newest first).
pub async fn query(pool: &DbPool, q: AuditQuery) -> AppResult<Vec<AuditEvent>> {
    let limit = q.limit.unwrap_or(100).clamp(1, QUERY_HARD_CAP);
    let rows = sqlx::query(
        r#"
        SELECT
            audit_id, occurred_at, credential, operation, outcome,
            worker_id, execution_id, parent_execution_id,
            server_region, broker_region, kek_version, notes
        FROM noetl.secret_audit
        WHERE ($1::text       IS NULL OR credential = $1)
          AND ($2::bigint     IS NULL OR execution_id = $2)
          AND ($3::timestamptz IS NULL OR occurred_at >= $3)
          AND ($4::timestamptz IS NULL OR occurred_at < $4)
        ORDER BY occurred_at DESC
        LIMIT $5
        "#,
    )
    .bind(q.credential)
    .bind(q.execution_id)
    .bind(q.from)
    .bind(q.to)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| AuditEvent {
            audit_id: r.get::<i64, _>("audit_id"),
            occurred_at: r.get::<DateTime<Utc>, _>("occurred_at"),
            credential: r.get::<String, _>("credential"),
            operation: r.get::<String, _>("operation"),
            outcome: r.get::<String, _>("outcome"),
            worker_id: r.get::<Option<String>, _>("worker_id"),
            execution_id: r.get::<Option<i64>, _>("execution_id"),
            parent_execution_id: r.get::<Option<i64>, _>("parent_execution_id"),
            server_region: r.get::<Option<String>, _>("server_region"),
            broker_region: r.get::<Option<String>, _>("broker_region"),
            kek_version: r.get::<Option<String>, _>("kek_version"),
            notes: r.get::<Option<String>, _>("notes"),
        })
        .collect())
}
