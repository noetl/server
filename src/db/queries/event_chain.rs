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
//! The columns are owned by the platform schema
//! (`db/ddl/postgres/schema_ddl.sql` in this repo defines them for fresh
//! installs; ops still provisions from noetl/noetl's copy until it repoints), but the
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
/// role (defined in `db/ddl/postgres/schema_ddl.sql` in this repo); the
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

// ---------------------------------------------------------------------------
// The chain read — the source for chain-following advance.
// ---------------------------------------------------------------------------

/// One `noetl.event` row as the chain decision needs it.
///
/// ⚠⚠ **Every nullable column is `Option`, and that is the whole lesson of
/// server#443.** That change selected `NULL::text AS content` into a `String`
/// field: sqlx refuses that row **only against a real database**, so it passed
/// unit tests AND a throwaway-Postgres proof that ran raw SQL, and then
/// returned HTTP 500 on every `/api/catalog/list` call in production.
/// *A test that renders SQL is not a test that decodes it.*
///
/// `prev_event_id` is NULL at a chain root and `parent_execution_id` is NULL at
/// a tree root — both are genuinely nullable, so both are `Option<i64>`, and
/// `tests/chain_source_pg.rs` decodes them against a live PostgreSQL.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChainRow {
    pub event_id: i64,
    pub execution_id: i64,
    pub prev_event_id: Option<i64>,
    pub parent_execution_id: Option<i64>,
    pub event_type: String,
}

/// Event types that end an execution.
///
/// Kept here beside the read so the classification travels with the query, and
/// exposed so a test can assert the set rather than re-listing it.
pub const TERMINAL_EVENT_TYPES: &[&str] = &[
    "execution.completed",
    "execution.failed",
    "execution.cancelled",
    "playbook.completed",
    "playbook.failed",
];

pub fn is_terminal_event_type(event_type: &str) -> bool {
    TERMINAL_EVENT_TYPES.contains(&event_type)
}

/// Read one execution's chain rows, ascending by `event_id`.
///
/// Bounded by `limit` so a pathological execution cannot pull an unbounded
/// result set onto the poller path.
///
/// ⚠ This is a `WHERE execution_id` read of `noetl.event` — the very scan class
/// the redesign exists to retire. It is here as the **transitional** source: it
/// buys the correct *semantics* (finished, runnable and missing-link become
/// distinguishable, which is what removes the re-drive loop) without yet buying
/// the *performance*. The performance arrives when this is repointed at the
/// execution-partitioned chain store. Do not mistake one for the other.
pub async fn read_chain(pool: &DbPool, execution_id: i64, limit: i64) -> AppResult<Vec<ChainRow>> {
    let rows: Vec<ChainRow> = sqlx::query_as::<_, ChainRow>(
        "SELECT event_id, execution_id, prev_event_id, parent_execution_id, event_type \
         FROM noetl.event WHERE execution_id = $1 ORDER BY event_id LIMIT $2",
    )
    .bind(execution_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Build the advance-decision view from chain rows.
///
/// `exec_seq` is the row's ordinal within the execution — the chain decision
/// only needs a monotone position to pick a head, and `event_id` ordering
/// already provides one.
pub fn rows_to_chain_view(
    execution_id: i64,
    rows: Vec<ChainRow>,
) -> crate::chain_advance::ChainView {
    let events = rows
        .into_iter()
        .enumerate()
        .map(|(i, r)| crate::chain_advance::ChainEventView {
            exec_seq: (i + 1) as u64,
            event_id: r.event_id.to_string(),
            prev_event_id: r.prev_event_id.map(|v| v.to_string()),
            execution_id: r.execution_id.to_string(),
            parent_execution_id: r.parent_execution_id.map(|v| v.to_string()),
            terminal: is_terminal_event_type(&r.event_type),
        })
        .collect();
    crate::chain_advance::ChainView::new(execution_id.to_string(), events)
}
