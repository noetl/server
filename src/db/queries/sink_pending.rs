//! `noetl.sink_pending` — the server-visible sink-state feed
//! ([noetl/ai-meta#199](https://github.com/noetl/ai-meta/issues/199) Slice B).
//!
//! The write-behind-cache invariant ("never GC un-sunk business context") needs
//! the Feather result-tier GC — which runs **in the server** — to know which
//! executions still hold business context that has not been sunk to the
//! customer's system of record. That state lives in the **worker's** in-process
//! sink gate (worker#190), a different process. This table is the shared signal
//! between them: workers report an execution pending-sink via the internal API
//! (`POST /api/internal/sink-state/mark`) and clear it on confirmation
//! (`POST /api/internal/sink-state/confirm`); the GC reads the set here.
//!
//! Honors [`data-access-boundary.md`](../../../../agents/rules/data-access-boundary.md):
//! only the server touches `noetl.*`; workers write through the internal API,
//! never directly. The table is created idempotently at startup via
//! [`ensure_table`] (same startup-DDL pattern as `result_store`), owned
//! end-to-end by noetl-server.

use crate::db::DbPool;
use crate::error::AppResult;

/// Idempotent table creation. Runs once at startup so the schema lands on first
/// boot without an out-of-band migration step.
pub async fn ensure_table(pool: &DbPool) -> AppResult<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS noetl.sink_pending (
            execution_id BIGINT PRIMARY KEY,
            worker_id    TEXT,
            marked_at    TIMESTAMPTZ NOT NULL DEFAULT now()
        )
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Record an execution as holding un-sunk business context (idempotent). Called
/// when a worker's sink step begins / marks the execution pending-sink. Refreshes
/// `worker_id` + `marked_at` on a repeat so the row reflects the latest reporter.
pub async fn mark(pool: &DbPool, execution_id: i64, worker_id: Option<&str>) -> AppResult<()> {
    sqlx::query(
        r#"
        INSERT INTO noetl.sink_pending (execution_id, worker_id, marked_at)
        VALUES ($1, $2, now())
        ON CONFLICT (execution_id)
        DO UPDATE SET worker_id = EXCLUDED.worker_id, marked_at = now()
        "#,
    )
    .bind(execution_id)
    .bind(worker_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Clear an execution's pending-sink state — its business context was sunk to the
/// customer store (or the execution was abandoned). Idempotent: deleting an
/// absent row is a no-op. Returns true if a row was removed.
pub async fn confirm(pool: &DbPool, execution_id: i64) -> AppResult<bool> {
    let res = sqlx::query("DELETE FROM noetl.sink_pending WHERE execution_id = $1")
        .bind(execution_id)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() > 0)
}

/// The set of executions currently pending-sink — the source the result-tier GC
/// consults so it never reclaims un-sunk business context. Bounded by `limit`
/// (the pending set is small in practice — executions mid-sink); a safety cap so
/// a runaway feed can't return an unbounded list into the sweep.
pub async fn list_pending(pool: &DbPool, limit: i64) -> AppResult<Vec<i64>> {
    let rows: Vec<(i64,)> =
        sqlx::query_as("SELECT execution_id FROM noetl.sink_pending ORDER BY marked_at LIMIT $1")
            .bind(limit)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// Default safety cap for [`list_pending`].
///
/// noetl/ai-meta#199 — a cap on a SAFETY gate has to be observable, because
/// hitting it makes the gate fail OPEN.  `list_pending` is `ORDER BY marked_at
/// LIMIT n`, so truncation silently drops the **newest** marks — precisely the
/// executions whose business context is most likely still live — and the
/// Feather GC then reclaims them.
///
/// That contradicts the sweep's own documented invariant, which says the gate
/// "can only ever retain more, never delete more".  With a silent cap it can
/// delete more.  See [`list_pending_checked`].
pub const DEFAULT_LIST_LIMIT: i64 = 100_000;

/// [`list_pending`] plus whether the result was truncated by the cap.
///
/// Returns `(ids, complete)`. `complete == false` means the feed hit
/// `DEFAULT_LIST_LIMIT` and the caller is holding an INCOMPLETE view of what is
/// un-sunk.
///
/// A caller gating deletion on this set must fail CLOSED when `complete` is
/// false: retaining objects it might have reclaimed costs storage, whereas
/// reclaiming business context that was never sunk to the customer's system of
/// record is unrecoverable. Those are not symmetric, so the tie does not go to
/// the sweep.
pub async fn list_pending_checked(pool: &DbPool, limit: i64) -> AppResult<(Vec<i64>, bool)> {
    let ids = list_pending(pool, limit).await?;
    let complete = (ids.len() as i64) < limit;
    Ok((ids, complete))
}
