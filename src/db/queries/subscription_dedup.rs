//! `noetl.subscription_dedup` — the opt-in exactly-once dedup window
//! (noetl/ai-meta#90 Phase 7, RFC §10 OQ1/OQ2).
//!
//! All sources NoETL consumes are at-least-once, and the store-and-forward
//! spool (Phase 4) deliberately *replays*.  For most streams the playbook is
//! expected to be idempotent (key on `message_id`).  But a low-volume critical
//! stream can ask the server to collapse a duplicate delivery to a single
//! execution: when a `kind: Subscription` declares `dedup.enabled: true`, the
//! continuous runtime stamps each `POST /api/execute` with a `dedup` block
//! (`{ key, window_secs }`) and the server consults this table.
//!
//! The table is **bounded in age**: each access first prunes the scope's rows
//! older than the window, so a key that re-arrives *outside* the window is
//! treated as fresh (and the table can't grow without bound).  The dedup
//! decision is **race-safe**: the claim is an `INSERT … ON CONFLICT DO
//! NOTHING`, so a duplicate never creates an execution even if two replicas
//! process the two copies concurrently — the unique `(subscription_id,
//! dedup_key)` constraint is the arbiter.
//!
//! The opt-in design is deliberate: at IoT volume a DB write per message is too
//! costly, so dedup defaults **off** (RFC §10 OQ1 "offer dedup opt-in for
//! low-volume critical streams").  The table lives on the cluster pool so it is
//! a single dedup authority regardless of which shard the child execution
//! lands on (`agents/rules/data-access-boundary.md`: server owns the table,
//! workers reach it only through `/api/execute`).

use crate::db::DbPool;
use crate::error::AppResult;

/// Default dedup window when a subscription opts in without naming one.
pub const DEFAULT_WINDOW_SECS: u64 = 300;

/// Outcome of a dedup claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupOutcome {
    /// First time this `(subscription, key)` has been seen in the window — the
    /// caller proceeds to create the execution under `execution_id`.
    Fresh,
    /// A live row already holds this key within the window — the caller skips
    /// execution creation and returns the existing execution id.
    Duplicate { existing_execution_id: i64 },
}

/// Idempotent table creation.  Runs once at server startup (mirrors
/// [`crate::db::queries::secret_audit::ensure_table`]) so the schema lands on
/// first boot without an out-of-band migration.  The table is
/// `noetl/server`-owned end to end (only `/api/execute` writes it), so a
/// `CREATE TABLE IF NOT EXISTS` at startup is the right shape.
pub async fn ensure_table(pool: &DbPool) -> AppResult<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS noetl.subscription_dedup (
            subscription_id BIGINT      NOT NULL,
            dedup_key       TEXT        NOT NULL,
            execution_id    BIGINT      NOT NULL,
            seen_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
            PRIMARY KEY (subscription_id, dedup_key)
        )
        "#,
    )
    .execute(pool)
    .await?;
    // Prune query filters on (subscription_id, seen_at); index it so the
    // per-access prune stays cheap even on a busy critical stream.
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS subscription_dedup_scope_seen
        ON noetl.subscription_dedup (subscription_id, seen_at)
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Attempt to claim `(subscription_id, dedup_key)` for `execution_id` within a
/// `window_secs` window.
///
/// 1. Prune the scope's rows older than the window — bounds the table by age
///    and lets a key that re-arrives *outside* the window be treated as fresh.
/// 2. `INSERT … ON CONFLICT DO NOTHING` — the race-safe claim.  One row
///    affected → [`DedupOutcome::Fresh`]; zero rows → a live duplicate, whose
///    winning `execution_id` is read back as [`DedupOutcome::Duplicate`].
///
/// The caller must have generated `execution_id` *before* calling this (the
/// snowflake is reserved up front per `observability.md` Principle 3) so the
/// reserved id is what the table records on the fresh path.
pub async fn claim(
    pool: &DbPool,
    subscription_id: i64,
    dedup_key: &str,
    window_secs: u64,
    execution_id: i64,
) -> AppResult<DedupOutcome> {
    // 1. Age out expired entries for this scope (bounded-by-age).
    sqlx::query(
        "DELETE FROM noetl.subscription_dedup \
         WHERE subscription_id = $1 AND seen_at < now() - make_interval(secs => $2)",
    )
    .bind(subscription_id)
    .bind(window_secs as f64)
    .execute(pool)
    .await?;

    // 2. Race-safe claim.
    let inserted = sqlx::query(
        "INSERT INTO noetl.subscription_dedup (subscription_id, dedup_key, execution_id, seen_at) \
         VALUES ($1, $2, $3, now()) \
         ON CONFLICT (subscription_id, dedup_key) DO NOTHING",
    )
    .bind(subscription_id)
    .bind(dedup_key)
    .bind(execution_id)
    .execute(pool)
    .await?;

    if inserted.rows_affected() == 1 {
        return Ok(DedupOutcome::Fresh);
    }

    // Lost the race (or a prior delivery already holds the key) — read the
    // winner so the caller can point this duplicate at the existing execution.
    let existing: i64 = sqlx::query_scalar(
        "SELECT execution_id FROM noetl.subscription_dedup \
         WHERE subscription_id = $1 AND dedup_key = $2",
    )
    .bind(subscription_id)
    .bind(dedup_key)
    .fetch_one(pool)
    .await?;
    Ok(DedupOutcome::Duplicate {
        existing_execution_id: existing,
    })
}
