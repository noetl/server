//! Service logic for the `/api/internal/*` endpoint family.
//!
//! These endpoints are called by the system worker pool's playbooks
//! (`system/outbox_publisher`, `system/projector`) instead of the
//! playbooks touching `noetl.*` tables directly.  Per the
//! data-access-boundary rule
//! (`agents/rules/data-access-boundary.md` in ai-meta): NoETL platform
//! data is server-API only.
//!
//! Contract mirrors the Python implementation in
//! `repos/noetl/noetl/server/api/internal/service.py` byte-for-byte —
//! the system pool's playbooks must work against either server during
//! migration.  Tracks noetl/server#11 → noetl/ai-meta#49 Phase C.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::types::JsonValue;

use crate::db::DbPool;
use crate::error::AppResult;

// ---------------------------------------------------------------------------
// Outbox claim
// ---------------------------------------------------------------------------

/// One outbox row returned by `claim_batch`.
///
/// Mirrors the columns the Python `claim_outbox_batch` returns; the
/// system playbook iterates over these rows and publishes each
/// `payload` to NATS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxRow {
    pub outbox_id: i64,
    pub event_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub payload: JsonValue,
    #[serde(default = "default_payload_codec")]
    pub payload_codec: String,
    #[serde(default)]
    pub attempts: i32,
}

fn default_payload_codec() -> String {
    "arrow-feather".to_string()
}

/// Claim a batch of PENDING/FAILED outbox rows and mark them
/// IN_FLIGHT.
///
/// Mirrors the SQL in `noetl.core.outbox.claim_outbox_batch`:
/// `WITH ready AS (SELECT ... FOR UPDATE SKIP LOCKED) UPDATE ...
/// RETURNING ...`.  The CTE + skip-locked combo lets multiple system
/// pool workers race the same outbox without blocking each other.
pub async fn claim_batch(pool: &DbPool, limit: i64) -> AppResult<Vec<OutboxRow>> {
    let limit = limit.clamp(1, 1000);

    let rows = sqlx::query_as::<_, (i64, i64, Option<i64>, Option<String>, JsonValue, String, i32)>(
        r#"
        WITH ready AS (
            SELECT outbox_id
            FROM noetl.outbox
            WHERE status IN ('PENDING', 'FAILED')
              AND available_at <= now()
            ORDER BY outbox_id
            LIMIT $1
            FOR UPDATE SKIP LOCKED
        )
        UPDATE noetl.outbox o
        SET status = 'IN_FLIGHT',
            attempts = attempts + 1,
            locked_at = now(),
            updated_at = now()
        FROM ready
        WHERE o.outbox_id = ready.outbox_id
        RETURNING o.outbox_id, o.event_id, o.execution_id, o.subject,
                  o.payload, o.payload_codec, o.attempts
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(outbox_id, event_id, execution_id, subject, payload, payload_codec, attempts)| {
                OutboxRow {
                    outbox_id,
                    event_id,
                    execution_id,
                    subject,
                    payload,
                    payload_codec,
                    attempts,
                }
            },
        )
        .collect())
}

// ---------------------------------------------------------------------------
// Outbox mark published
// ---------------------------------------------------------------------------

/// Mark a batch of outbox rows PUBLISHED.
///
/// Idempotent: re-marking an already-PUBLISHED row is a no-op
/// (the UPDATE just won't match).  Returns the count of rows actually
/// updated.
pub async fn mark_published_batch(pool: &DbPool, outbox_ids: &[i64]) -> AppResult<i64> {
    if outbox_ids.is_empty() {
        return Ok(0);
    }

    let result = sqlx::query(
        r#"
        UPDATE noetl.outbox
        SET status = 'PUBLISHED',
            published_at = now(),
            updated_at = now(),
            last_error = NULL
        WHERE outbox_id = ANY($1)
        "#,
    )
    .bind(outbox_ids)
    .execute(pool)
    .await?;

    Ok(result.rows_affected() as i64)
}

// ---------------------------------------------------------------------------
// Outbox mark failed
// ---------------------------------------------------------------------------

/// Mark a single outbox row FAILED with exponential backoff.
///
/// Backoff formula mirrors `noetl.core.outbox.mark_outbox_failed`:
/// `delay = min(max_delay_seconds, 2 ** min(8, max(0, attempts - 1)))`.
/// Returns the computed delay so the system playbook can surface it.
pub async fn mark_failed_row(
    pool: &DbPool,
    outbox_id: i64,
    error: &str,
    attempts: i32,
    max_delay_seconds: i32,
) -> AppResult<i64> {
    let clamped_exponent = (attempts - 1).clamp(0, 8);
    let raw_delay: i64 = 1i64 << clamped_exponent;
    let delay_seconds = raw_delay.min(max_delay_seconds as i64);

    // Truncate the error to the same 2000-char limit the Python side uses.
    let truncated_error: String = error.chars().take(2000).collect();

    sqlx::query(
        r#"
        UPDATE noetl.outbox
        SET status = 'FAILED',
            available_at = now() + ($1 || ' seconds')::interval,
            last_error = $2,
            updated_at = now()
        WHERE outbox_id = $3
        "#,
    )
    .bind(delay_seconds.to_string())
    .bind(truncated_error)
    .bind(outbox_id)
    .execute(pool)
    .await?;

    Ok(delay_seconds)
}

// ---------------------------------------------------------------------------
// Outbox pending count
// ---------------------------------------------------------------------------

/// Count outbox rows currently eligible for claim.
///
/// KEDA HTTP scaler reads this; keep it fast.  Returns rows in PENDING
/// or FAILED with `available_at <= now()`.  IN_FLIGHT / PUBLISHED rows
/// excluded.
pub async fn pending_count(pool: &DbPool) -> AppResult<i64> {
    let row: (i64,) = sqlx::query_as(
        r#"
        SELECT count(*)
        FROM noetl.outbox
        WHERE status IN ('PENDING', 'FAILED')
          AND available_at <= now()
        "#,
    )
    .fetch_one(pool)
    .await?;
    Ok(row.0)
}

// ---------------------------------------------------------------------------
// Events projector
// ---------------------------------------------------------------------------

/// One event envelope as the projector receives it.
///
/// Tolerates extra fields via `#[serde(flatten)]` on `extra` — the
/// projector must be loose-coupled with the event emitter (worker,
/// executor, server).  Required fields mirror what the projector's
/// batch INSERT writes.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EventEnvelope {
    pub event_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_event_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack_trace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_component: Option<String>,
    /// Catch-all for extra fields the emitter sends that the projector
    /// doesn't currently care about.  Preserves the
    /// `model_config={"extra": "allow"}` semantics from the Python side.
    #[serde(flatten, default)]
    pub extra: std::collections::BTreeMap<String, JsonValue>,
}

/// Project a batch of events into `noetl.event`.
///
/// Idempotent via `ON CONFLICT (event_id) DO NOTHING`.  Re-projecting
/// the same `event_id` is a no-op.  Returns `(projected, duplicates)`
/// where `projected` is the number of rows actually inserted and
/// `duplicates` is the number skipped via ON CONFLICT.
///
/// Mirrors the SQL pattern in
/// `repos/noetl/noetl/server/api/internal/service.py::project_events`
/// — single-statement batch INSERT via `jsonb_array_elements` over the
/// payload.
pub async fn project_events(pool: &DbPool, events: &[EventEnvelope]) -> AppResult<(i64, i64)> {
    if events.is_empty() {
        return Ok((0, 0));
    }

    let payload = serde_json::to_value(events)?;

    let result = sqlx::query(
        r#"
        INSERT INTO noetl.event (
            event_id,
            execution_id,
            parent_event_id,
            event_type,
            node_id,
            node_name,
            node_type,
            status,
            duration,
            timestamp,
            context,
            result,
            meta,
            error,
            stack_trace,
            trace_component
        )
        SELECT
            (row->>'event_id')::bigint,
            NULLIF(row->>'execution_id', '')::bigint,
            NULLIF(row->>'parent_event_id', '')::bigint,
            row->>'event_type',
            row->>'node_id',
            row->>'node_name',
            row->>'node_type',
            row->>'status',
            NULLIF(row->>'duration', '')::double precision,
            NULLIF(row->>'timestamp', '')::timestamptz,
            NULLIF(row->'context', 'null'::jsonb),
            NULLIF(row->'result', 'null'::jsonb),
            NULLIF(row->'meta', 'null'::jsonb),
            row->>'error',
            row->>'stack_trace',
            row->>'trace_component'
        FROM jsonb_array_elements($1::jsonb) AS row
        ON CONFLICT (event_id) DO NOTHING
        "#,
    )
    .bind(payload)
    .execute(pool)
    .await?;

    let projected = result.rows_affected() as i64;
    let duplicates = (events.len() as i64 - projected).max(0);
    Ok((projected, duplicates))
}

// ---------------------------------------------------------------------------
// Tests — backoff math (the only logic that doesn't need a real DB)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    /// Verify the exponential-backoff math matches the Python side's
    /// `min(max_delay, 2 ** min(8, max(0, attempts-1)))`.
    fn compute_delay(attempts: i32, max_delay: i32) -> i64 {
        let clamped_exponent = (attempts - 1).clamp(0, 8);
        let raw_delay: i64 = 1i64 << clamped_exponent;
        raw_delay.min(max_delay as i64)
    }

    #[test]
    fn backoff_attempts_1_is_1s() {
        assert_eq!(compute_delay(1, 300), 1);
    }

    #[test]
    fn backoff_attempts_2_is_2s() {
        assert_eq!(compute_delay(2, 300), 2);
    }

    #[test]
    fn backoff_attempts_5_is_16s() {
        assert_eq!(compute_delay(5, 300), 16);
    }

    #[test]
    fn backoff_attempts_9_clamps_to_max() {
        // 2^8 = 256, capped at max=300 → 256.
        assert_eq!(compute_delay(9, 300), 256);
    }

    #[test]
    fn backoff_attempts_20_clamps_to_max() {
        // 2^min(8, 19) = 2^8 = 256, capped at max=300 → 256.
        assert_eq!(compute_delay(20, 300), 256);
    }

    #[test]
    fn backoff_respects_max_delay_lower_than_2_pow_8() {
        // 2^min(8, 4) = 2^4 = 16, but max=10 → 10.
        assert_eq!(compute_delay(5, 10), 10);
    }

    #[test]
    fn backoff_attempts_0_is_1s() {
        // attempts-1 clamps at 0 → 2^0 = 1.
        assert_eq!(compute_delay(0, 300), 1);
    }
}
