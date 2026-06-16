//! Orchestrator state snapshots (noetl/ai-meta#101, block b).
//!
//! Persists the orchestrator's reconstructed [`WorkflowState`] to
//! `noetl.projection_snapshot` so the per-execution rebuild path is
//! **bounded**: instead of replaying the whole event log (which OOM'd the
//! server at scale — a 10×1000 PFT crashed the server at ~19k events), a
//! rebuild loads the latest snapshot + only the events newer than the
//! snapshot's `version` (the highest `event_id` folded into it).
//!
//! The snapshot is a generic event-sourcing aggregate row keyed by
//! `(tenant_id, organization_id, aggregate_type, aggregate_id)` — we use
//! `aggregate_type = "orchestrator_workflow_state"` and
//! `aggregate_id = execution_id`. `version` is the snapshot watermark
//! (highest applied `event_id`); `meta.applied_count` carries the number of
//! events folded in so the caller can detect stragglers after a rebuild.

use sha2::{Digest, Sha256};
use sqlx::Row;

use crate::db::DbPool;
use crate::engine::state::WorkflowState;
use crate::error::{AppError, AppResult};

const AGGREGATE_TYPE: &str = "orchestrator_workflow_state";

/// A snapshot loaded back from the store.
pub struct LoadedSnapshot {
    pub state: WorkflowState,
    /// Highest `event_id` folded into the snapshot.
    pub version: i64,
    /// Number of events folded in (for straggler detection on rebuild).
    pub applied_count: i64,
    /// Wall-clock time the snapshot was written.  The rebuild re-scans events
    /// with `created_at` newer than this minus a margin, so a straggler that
    /// landed *below* `version` *after* the snapshot was taken is still caught
    /// (re-applying overlap is safe — cursor counters are gated by the
    /// `cursor_issued`/`cursor_completed` id-sets that the snapshot carries).
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// The `playbook_started` event's meta (pool segment + trace routing),
    /// carried on the snapshot because that event predates every snapshot and
    /// so is never re-loaded in the events-since window.
    pub routing_meta: Option<serde_json::Value>,
}

/// Upsert the orchestrator state snapshot for an execution.
///
/// One row per execution (the PK collapses to `aggregate_id` once
/// `tenant_id`/`organization_id`/`aggregate_type` are fixed), so each save
/// overwrites the previous snapshot with a newer watermark.
pub async fn save(
    pool: &DbPool,
    execution_id: i64,
    version: i64,
    applied_count: i64,
    routing_meta: Option<&serde_json::Value>,
    state: &WorkflowState,
) -> AppResult<()> {
    let snapshot = serde_json::to_value(state)
        .map_err(|e| AppError::Internal(format!("orch_snapshot.save: serialise: {e}")))?;
    let checksum = {
        let bytes = serde_json::to_vec(&snapshot).unwrap_or_default();
        hex::encode(Sha256::digest(&bytes))
    };
    let meta = serde_json::json!({
        "applied_count": applied_count,
        "routing_meta": routing_meta,
    });

    sqlx::query(
        r#"
        INSERT INTO noetl.projection_snapshot
            (aggregate_id, aggregate_type, version, snapshot, checksum, meta, updated_at)
        VALUES ($1, $2, $3, $4, $5, $6, now())
        ON CONFLICT (tenant_id, organization_id, aggregate_type, aggregate_id)
        DO UPDATE SET
            version = EXCLUDED.version,
            snapshot = EXCLUDED.snapshot,
            checksum = EXCLUDED.checksum,
            meta = EXCLUDED.meta,
            updated_at = now()
        "#,
    )
    .bind(execution_id.to_string())
    .bind(AGGREGATE_TYPE)
    .bind(version)
    .bind(&snapshot)
    .bind(&checksum)
    .bind(&meta)
    .execute(pool)
    .await
    .map_err(|e| AppError::Internal(format!("orch_snapshot.save: upsert: {e}")))?;

    Ok(())
}

/// Load the latest orchestrator state snapshot for an execution, if any.
///
/// Returns `None` when no snapshot exists yet (early in a run, before the
/// first save) — the caller then rebuilds from the full (still-small) log.
pub async fn load_latest(pool: &DbPool, execution_id: i64) -> AppResult<Option<LoadedSnapshot>> {
    let row = sqlx::query(
        r#"
        SELECT version, snapshot, meta, updated_at
        FROM noetl.projection_snapshot
        WHERE aggregate_type = $1 AND aggregate_id = $2
          AND tenant_id = 'default' AND organization_id = 'default'
        "#,
    )
    .bind(AGGREGATE_TYPE)
    .bind(execution_id.to_string())
    .fetch_optional(pool)
    .await
    .map_err(|e| AppError::Internal(format!("orch_snapshot.load_latest: query: {e}")))?;

    let Some(row) = row else {
        return Ok(None);
    };

    let version: i64 = row.try_get("version").unwrap_or(0);
    let updated_at: chrono::DateTime<chrono::Utc> =
        row.try_get("updated_at").unwrap_or_else(|_| chrono::Utc::now());
    let snapshot: serde_json::Value = row
        .try_get("snapshot")
        .map_err(|e| AppError::Internal(format!("orch_snapshot.load_latest: snapshot col: {e}")))?;
    let meta: serde_json::Value = row.try_get("meta").unwrap_or(serde_json::Value::Null);
    let applied_count = meta
        .get("applied_count")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let routing_meta = meta
        .get("routing_meta")
        .filter(|v| !v.is_null())
        .cloned();

    // A snapshot that fails to deserialise (e.g. a WorkflowState shape change
    // across a deploy) is treated as absent — the caller falls back to a full
    // rebuild, which is always correct, just slower.  Better than erroring the
    // whole trigger.
    let state: WorkflowState = match serde_json::from_value(snapshot) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                execution_id,
                version,
                %e,
                "orch_snapshot.load_latest: snapshot deserialise failed; ignoring (full rebuild)"
            );
            return Ok(None);
        }
    };

    Ok(Some(LoadedSnapshot {
        state,
        version,
        applied_count,
        updated_at,
        routing_meta,
    }))
}
