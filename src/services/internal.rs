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
// Scheduled cleanup (noetl/ai-meta#96)
// ---------------------------------------------------------------------------

/// Retention policy for one cleanup run.  All windows are inclusive of "older
/// than"; a `0` / `None` window means **skip that table** so an empty body is
/// a safe no-op and the event log is never purged unless explicitly asked.
#[derive(Debug, Clone, Deserialize)]
pub struct CleanupPolicy {
    /// Purge terminal (`completed_at IS NOT NULL`) `noetl.command` rows older
    /// than this many days.  These are transient queue entries — the durable
    /// audit trail lives in `noetl.event`.  Default 7.
    #[serde(default = "default_command_days")]
    pub command_retention_days: i64,
    /// Purge `noetl.runtime` `worker_pool` rows whose heartbeat is older than
    /// this many minutes (dead/scaled-down worker registrations).  Default 60.
    #[serde(default = "default_runtime_minutes")]
    pub runtime_stale_minutes: i64,
    /// Purge `noetl.event` rows older than this many days.  **Opt-in**: `0`
    /// (the default) skips the event log entirely, because it is the
    /// append-only source of truth and purging it makes those executions
    /// un-replayable.  Set a large value (e.g. 365) deliberately.
    #[serde(default)]
    pub event_retention_days: i64,
}

fn default_command_days() -> i64 {
    7
}
fn default_runtime_minutes() -> i64 {
    60
}

impl Default for CleanupPolicy {
    fn default() -> Self {
        Self {
            command_retention_days: default_command_days(),
            runtime_stale_minutes: default_runtime_minutes(),
            event_retention_days: 0,
        }
    }
}

/// Per-table purge counts returned by [`purge_stale`].
#[derive(Debug, Clone, Serialize)]
pub struct CleanupResult {
    pub commands_purged: u64,
    pub runtime_purged: u64,
    /// Number of `noetl.event` partitions dropped (the event log is
    /// range-partitioned by execution_id, so retention drops whole old
    /// partitions instead of row-by-row DELETE).
    pub events_purged: u64,
    /// Names of the dropped event partitions (for the audit log).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub event_partitions_dropped: Vec<String>,
}

/// NoETL snowflake epoch in Unix ms (2024-01-01T00:00:00Z) and the timestamp
/// shift (sequence 12 + machine_id 10 bits).  Mirrors `crate::snowflake`.
const NOETL_EPOCH_MS: i64 = 1_704_067_200_000;
const SNOWFLAKE_TS_SHIFT: i64 = 22;

/// Drop `noetl.event` partitions whose entire `execution_id` range is older
/// than the retention cutoff.
///
/// `noetl.event` is `PARTITION BY RANGE (execution_id)` where execution_id is a
/// time-ordered snowflake (`(unix_ms - epoch) << 22 | machine | seq`). A
/// partition whose upper bound `<=` the cutoff snowflake id therefore holds
/// only rows older than `retention_days` — so it can be `DROP`ped wholesale.
/// This reclaims space instantly with no DELETE scan, dead tuples, or vacuum,
/// which is the whole point of partitioning the event log for retention.
/// `event_default` (the catch-all) is never dropped.
async fn drop_old_event_partitions(
    pool: &DbPool,
    retention_days: i64,
) -> AppResult<Vec<String>> {
    // cutoff snowflake id for "retention_days ago".
    let cutoff: i64 = sqlx::query_scalar(
        r#"
        SELECT ((((extract(epoch from now()) * 1000)::bigint)
                 - ($1::bigint * 86400000) - $2::bigint) << $3::int)::bigint
        "#,
    )
    .bind(retention_days)
    .bind(NOETL_EPOCH_MS)
    .bind(SNOWFLAKE_TS_SHIFT as i32)
    .fetch_one(pool)
    .await?;

    // Partitions of noetl.event and their bound expressions (excluding the
    // DEFAULT catch-all).
    let parts: Vec<(String, String)> = sqlx::query_as(
        r#"
        SELECT c.relname, pg_get_expr(c.relpartbound, c.oid)
        FROM pg_inherits i
        JOIN pg_class c ON c.oid = i.inhrelid
        JOIN pg_class p ON p.oid = i.inhparent
        JOIN pg_namespace n ON n.oid = p.relnamespace
        WHERE n.nspname = 'noetl' AND p.relname = 'event'
          AND c.relname <> 'event_default'
        "#,
    )
    .fetch_all(pool)
    .await?;

    let mut dropped = Vec::new();
    for (name, bound) in parts {
        // bound: `FOR VALUES FROM (<lo>) TO (<hi>)`.  Skip MAXVALUE uppers
        // (they extend into the future and can never be fully old).
        match parse_partition_upper(&bound) {
            Some(hi) if hi <= cutoff => {
                // `name` comes from pg_catalog (trusted); quote it defensively.
                sqlx::query(&format!("DROP TABLE IF EXISTS noetl.\"{}\"", name))
                    .execute(pool)
                    .await?;
                dropped.push(name);
            }
            _ => {}
        }
    }
    Ok(dropped)
}

/// Extract the upper bound of a RANGE partition bound expression
/// (`... TO (<n>)`).  Returns `None` for `MAXVALUE` or unparseable bounds.
fn parse_partition_upper(bound: &str) -> Option<i64> {
    let start = bound.rfind("TO (")? + 4;
    let rest = &bound[start..];
    let end = rest.find(')')?;
    rest[..end].trim().trim_matches('\'').parse::<i64>().ok()
}

/// Delete clearly-transient `noetl.*` rows per the retention policy.
///
/// Conservative by design: only terminal command rows, dead worker
/// registrations, and (opt-in) old event rows are touched. A window of `<= 0`
/// skips that table. Counts are returned per table so the caller can record
/// metrics + a single structured log line instead of per-delete logging.
pub async fn purge_stale(pool: &DbPool, policy: &CleanupPolicy) -> AppResult<CleanupResult> {
    let commands_purged = if policy.command_retention_days > 0 {
        sqlx::query(
            r#"
            DELETE FROM noetl.command
            WHERE completed_at IS NOT NULL
              AND completed_at < now() - make_interval(days => $1::int)
            "#,
        )
        .bind(policy.command_retention_days as i32)
        .execute(pool)
        .await?
        .rows_affected()
    } else {
        0
    };

    let runtime_purged = if policy.runtime_stale_minutes > 0 {
        sqlx::query(
            r#"
            DELETE FROM noetl.runtime
            WHERE kind = 'worker_pool'
              AND heartbeat < now() - make_interval(mins => $1::int)
            "#,
        )
        .bind(policy.runtime_stale_minutes as i32)
        .execute(pool)
        .await?
        .rows_affected()
    } else {
        0
    };

    // Event log is opt-in only — default policy never touches it.  When
    // enabled, retention drops whole old partitions (the event table is
    // range-partitioned by execution_id) rather than DELETE-ing rows.
    let event_partitions_dropped = if policy.event_retention_days > 0 {
        drop_old_event_partitions(pool, policy.event_retention_days).await?
    } else {
        Vec::new()
    };

    Ok(CleanupResult {
        commands_purged,
        runtime_purged,
        events_purged: event_partitions_dropped.len() as u64,
        event_partitions_dropped,
    })
}

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

    // noetl.event schema (kind 2026-06-02):
    //   - PRIMARY KEY (event_id)
    //   - NOT NULL: execution_id, catalog_id, event_id, created_at,
    //               tenant_id, organization_id
    //   - All others nullable.
    //
    // The projector accepts envelopes that may or may not carry
    // catalog_id/tenant_id/organization_id (depends on the emitter).
    // Defaults:
    //   - catalog_id      → 0   (sentinel)
    //   - tenant_id       → 'default'
    //   - organization_id → 'default'
    //   - created_at      → now()
    let result = sqlx::query(
        r#"
        INSERT INTO noetl.event (
            event_id,
            execution_id,
            catalog_id,
            parent_event_id,
            event_type,
            node_id,
            node_name,
            node_type,
            status,
            duration,
            context,
            result,
            meta,
            error,
            stack_trace,
            tenant_id,
            organization_id,
            created_at
        )
        SELECT
            (row->>'event_id')::bigint,
            COALESCE(NULLIF(row->>'execution_id', '')::bigint, 0),
            COALESCE(NULLIF(row->>'catalog_id', '')::bigint, 0),
            NULLIF(row->>'parent_event_id', '')::bigint,
            row->>'event_type',
            row->>'node_id',
            row->>'node_name',
            row->>'node_type',
            row->>'status',
            NULLIF(row->>'duration', '')::double precision,
            NULLIF(row->'context', 'null'::jsonb),
            NULLIF(row->'result', 'null'::jsonb),
            NULLIF(row->'meta', 'null'::jsonb),
            row->>'error',
            row->>'stack_trace',
            COALESCE(NULLIF(row->>'tenant_id', ''), 'default'),
            COALESCE(NULLIF(row->>'organization_id', ''), 'default'),
            COALESCE(NULLIF(row->>'timestamp', '')::timestamp,
                     NULLIF(row->>'created_at', '')::timestamp,
                     now())
        FROM jsonb_array_elements($1::jsonb) AS row
        -- noetl.event is partitioned (15 partitions); event_id alone
        -- is not a partition-spanning unique constraint.  Using
        -- ``ON CONFLICT DO NOTHING`` without a target catches any
        -- uniqueness violation across partitions — sufficient for
        -- projector idempotency.
        ON CONFLICT DO NOTHING
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
    use super::CleanupPolicy;

    #[test]
    fn cleanup_empty_body_uses_safe_defaults_and_skips_events() {
        // An empty `{}` body must deserialize to the safe defaults: prune
        // transient command + dead runtime rows, but NEVER touch the event
        // log (source of truth) unless explicitly asked.
        let p: CleanupPolicy = serde_json::from_str("{}").unwrap();
        assert_eq!(p.command_retention_days, 7);
        assert_eq!(p.runtime_stale_minutes, 60);
        assert_eq!(p.event_retention_days, 0, "event log must be opt-in");
        // The `Default` impl agrees with the serde defaults.
        let d = CleanupPolicy::default();
        assert_eq!(d.command_retention_days, 7);
        assert_eq!(d.runtime_stale_minutes, 60);
        assert_eq!(d.event_retention_days, 0);
    }

    #[test]
    fn parse_partition_upper_extracts_range_bound() {
        use super::parse_partition_upper;
        // Numeric upper bound -> parsed.
        assert_eq!(
            parse_partition_upper("FOR VALUES FROM (264905529753600000) TO (297520437657600000)"),
            Some(297520437657600000)
        );
        // MINVALUE lower, numeric upper (the event_pre_* partition).
        assert_eq!(
            parse_partition_upper("FOR VALUES FROM (MINVALUE) TO (264905529753600000)"),
            Some(264905529753600000)
        );
        // MAXVALUE upper -> never fully old -> None (skip).
        assert_eq!(
            parse_partition_upper("FOR VALUES FROM (569000000000000000) TO (MAXVALUE)"),
            None
        );
        // Quoted form tolerated.
        assert_eq!(
            parse_partition_upper("FOR VALUES FROM ('1') TO ('100')"),
            Some(100)
        );
    }

    #[test]
    fn cleanup_event_retention_is_explicit_opt_in() {
        let p: CleanupPolicy =
            serde_json::from_str(r#"{"event_retention_days": 365}"#).unwrap();
        assert_eq!(p.event_retention_days, 365);
        // Other fields still fall back to safe defaults.
        assert_eq!(p.command_retention_days, 7);
        assert_eq!(p.runtime_stale_minutes, 60);
    }

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
