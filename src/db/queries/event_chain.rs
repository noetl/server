//! `prev_event_id` chain columns — the one-level event chain (RFC #115 Phase 2,
//! noetl/ai-meta#115 §4).
//!
//! Each `noetl.event` and `noetl.command` gains an additive `prev_event_id`
//! pointer naming the immediately-previous node in the execution's causal
//! order, so per-execution state can be followed **pointer-by-pointer, one
//! level at a time** instead of scanning the whole event table (the chain-walk
//! state builder lands in Phase 3 — this phase only *populates* the links;
//! nothing reads them yet).
//!
//! The columns are owned by the platform schema (`schema_ddl.sql` in
//! `noetl/noetl` carries the canonical definition for fresh installs), but the
//! server **also** ensures them idempotently at startup — mirroring
//! [`crate::db::queries::result_store::ensure_table`] — so a server image
//! carrying the populate-on-emit code never writes a column the running
//! database is missing (the gate-off INSERT binds an explicit column list; a
//! missing column would fail every insert).  `ADD COLUMN IF NOT EXISTS` on the
//! partitioned parents propagates to every partition.

use crate::db::DbPool;
use crate::error::AppResult;

/// Idempotently add `prev_event_id` to `noetl.event` + `noetl.command` and the
/// chain-walk lookup index.
///
/// **Best-effort, never fatal.**  The platform schema is owned by the DB init
/// role (the canonical definition is `schema_ddl.sql` in `noetl/noetl`); the
/// server's connection role may not own `noetl.event` / `noetl.command` and so
/// can't `ALTER` them (`must be owner of table …`).  In that deployment the
/// columns are provisioned by the owner and this function's `ADD COLUMN IF NOT
/// EXISTS` would be a no-op anyway — so a permission error (or any DDL error)
/// is logged and swallowed rather than crashing startup.  The function still
/// returns `Ok(())`; a genuinely-missing column surfaces later as a clear
/// gate-off INSERT error, not a boot loop.
pub async fn ensure_columns(pool: &DbPool) -> AppResult<()> {
    // noetl.event — additive chain link.  Parent table; ADD COLUMN cascades to
    // the range partitions.
    try_ddl(
        pool,
        "ALTER TABLE noetl.event ADD COLUMN IF NOT EXISTS prev_event_id BIGINT",
        "noetl.event.prev_event_id",
    )
    .await;
    // noetl.command — the issuing-event pointer (the event whose application
    // issued the command; RFC #115 §4.1).
    try_ddl(
        pool,
        "ALTER TABLE noetl.command ADD COLUMN IF NOT EXISTS prev_event_id BIGINT",
        "noetl.command.prev_event_id",
    )
    .await;
    // Chain-walk integrity lookup: "who points AT this node" (forward replay /
    // chain-integrity check).  The "give me node <head>" walk is served by the
    // existing (execution_id, event_id) PK.  Partial — only linked rows.
    try_ddl(
        pool,
        "CREATE INDEX IF NOT EXISTS idx_event_prev_event_id \
         ON noetl.event (execution_id, prev_event_id) \
         WHERE prev_event_id IS NOT NULL",
        "idx_event_prev_event_id",
    )
    .await;
    Ok(())
}

/// Run one idempotent DDL statement, logging-and-swallowing any error (the
/// column is then expected to be owner-provisioned).  See [`ensure_columns`].
async fn try_ddl(pool: &DbPool, sql: &str, what: &str) {
    if let Err(e) = sqlx::query(sql).execute(pool).await {
        tracing::warn!(
            error = %e,
            target = what,
            "event-chain DDL skipped (server role likely not table owner; \
             column expected to be provisioned by schema_ddl.sql)"
        );
    }
}
