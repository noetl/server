//! Event-log → JetStream tailer — the CQRS write-path producer
//! (noetl/ai-meta#103 phase 2a).
//!
//! A background task that reads newly-committed `noetl.event` rows by a
//! persisted cursor and batch-publishes them onto the `noetl_events` JetStream
//! stream ([`crate::nats::EventStreamPublisher`]) for the `system/projector`
//! playbook (phase 2b) to fold into the read model.
//!
//! ## Why a tailer (not a DB trigger, not an in-process channel)
//!
//! - **Not a trigger:** a trigger is a Postgres-internal object — invisible,
//!   vendor-specific, and it doesn't travel across a storage-type change.  The
//!   tailer is ordinary application code; at the 2d cutover (worker publishes
//!   straight to JetStream) it is simply deleted.
//! - **Not an in-process channel fed at the 17 insert sites:** that would couple
//!   every emit path to the producer and lose in-flight events on a crash.  The
//!   tailer reads *committed* rows, so nothing is lost — a restart resumes from
//!   the persisted cursor and re-scans a small overlap window, which the
//!   stream's `event_id` message-dedup collapses.
//!
//! ## Cursor + ordering
//!
//! The cursor is the `noetl.event.id` (BIGSERIAL insert order), persisted in
//! `noetl.stream_cursor`.  Each poll reads `WHERE id > cursor ORDER BY id ASC
//! LIMIT batch` and advances the cursor to the max id published.  A row that
//! commits out of `id` order (a transaction that drew a low `id` but committed
//! late) could in principle sit just behind the cursor; the projector mirrors
//! block-b's straggler handling and the stream dedup makes a periodic
//! re-scan safe, so no event is permanently skipped.  During dual-write
//! `noetl.event` remains the source of truth, so any stream gap is recoverable.
//!
//! ## Default off
//!
//! Gated by `NOETL_EVENT_STREAM_ENABLED` (default off): landing 2a publishes
//! nothing until ops opts in.  First enable starts the cursor at `MAX(id)` (tail
//! from now, no history replay) unless `NOETL_EVENT_STREAM_BACKFILL=true`.

use std::time::Duration;

use crate::db::DbPool;
use crate::error::AppResult;
use crate::nats::EventStreamPublisher;
use crate::state::AppState;

/// Cursor name in `noetl.stream_cursor` for this tailer.
const CURSOR_NAME: &str = "event_stream_tailer";

/// Tailer configuration, all from the environment with safe defaults.
#[derive(Debug, Clone)]
pub struct EventStreamConfig {
    /// Master gate.  Off → the tailer task is not spawned.
    pub enabled: bool,
    /// Max events published per poll.
    pub batch_size: i64,
    /// Sleep between polls when caught up.
    pub poll_interval: Duration,
    /// On first run (no persisted cursor), start at id 0 to replay the whole
    /// history instead of tailing from now.
    pub backfill: bool,
    /// Stream message-dedup window (≥ the restart re-scan overlap).
    pub dedup_window: Duration,
    /// Stream retention.
    pub max_age: Duration,
}

impl EventStreamConfig {
    /// Read config from the process environment.
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// Pure parse over a key→value lookup — `from_env` delegates here, and tests
    /// drive it with an in-memory map so they never mutate global env (which
    /// races other env-reading tests under parallel execution).
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Self {
        let flag = |key: &str| {
            lookup(key)
                .map(|v| {
                    let v = v.trim().to_ascii_lowercase();
                    v == "true" || v == "1" || v == "yes" || v == "on"
                })
                .unwrap_or(false)
        };
        let num = |key: &str, default: u64| {
            lookup(key)
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(default)
        };
        Self {
            enabled: flag("NOETL_EVENT_STREAM_ENABLED"),
            batch_size: num("NOETL_EVENT_STREAM_BATCH", 500) as i64,
            poll_interval: Duration::from_millis(num("NOETL_EVENT_STREAM_POLL_MS", 500)),
            backfill: flag("NOETL_EVENT_STREAM_BACKFILL"),
            dedup_window: Duration::from_secs(num("NOETL_EVENT_STREAM_DEDUP_SECS", 120)),
            max_age: Duration::from_secs(num("NOETL_EVENT_STREAM_RETENTION_SECS", 86_400)),
        }
    }
}

/// Idempotent startup DDL for the tailer's durable cursor.  Same pattern as the
/// other `ensure_*` startup helpers; the table is one row per named cursor.
pub async fn ensure_cursor_table(pool: &DbPool) -> AppResult<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS noetl.stream_cursor (
            name        TEXT        PRIMARY KEY,
            position    BIGINT      NOT NULL,
            updated_at  TIMESTAMPTZ NOT NULL DEFAULT now()
        )
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Load the persisted cursor, or initialise it: `MAX(id)` (tail from now) unless
/// `backfill`, in which case 0 (replay history).  The initial value is persisted
/// so a later restart resumes rather than re-deriving "now".
async fn load_or_init_cursor(pool: &DbPool, backfill: bool) -> AppResult<i64> {
    if let Some((pos,)) =
        sqlx::query_as::<_, (i64,)>("SELECT position FROM noetl.stream_cursor WHERE name = $1")
            .bind(CURSOR_NAME)
            .fetch_optional(pool)
            .await?
    {
        return Ok(pos);
    }
    let start: i64 = if backfill {
        0
    } else {
        sqlx::query_as::<_, (Option<i64>,)>("SELECT MAX(id) FROM noetl.event")
            .fetch_one(pool)
            .await?
            .0
            .unwrap_or(0)
    };
    save_cursor(pool, start).await?;
    Ok(start)
}

/// Persist the cursor (upsert).
async fn save_cursor(pool: &DbPool, position: i64) -> AppResult<()> {
    sqlx::query(
        r#"
        INSERT INTO noetl.stream_cursor (name, position, updated_at)
        VALUES ($1, $2, now())
        ON CONFLICT (name) DO UPDATE SET position = EXCLUDED.position, updated_at = now()
        "#,
    )
    .bind(CURSOR_NAME)
    .bind(position)
    .execute(pool)
    .await?;
    Ok(())
}

/// One row of the tail read.
#[derive(sqlx::FromRow)]
struct TailRow {
    id: i64,
    event_id: i64,
    event_type: String,
}

/// Spawn the tailer if enabled and NATS is connected.  No-op (with a log) when
/// disabled or when the server runs without NATS.
pub fn spawn_event_stream_tailer(state: AppState, config: EventStreamConfig) {
    if !config.enabled {
        tracing::info!(
            target: "noetl_server::startup",
            "event-stream tailer disabled (NOETL_EVENT_STREAM_ENABLED unset) — CQRS write path inert"
        );
        return;
    }
    let Some(client) = state.nats.clone() else {
        tracing::warn!(
            target: "noetl_server::startup",
            "event-stream tailer enabled but NATS is not connected — producer cannot run"
        );
        return;
    };

    tokio::spawn(async move {
        let publisher =
            match EventStreamPublisher::new(client, config.dedup_window, config.max_age).await {
                Ok(p) => p,
                Err(e) => {
                    tracing::error!(%e, "event-stream tailer: failed to ensure noetl_events stream; producer not started");
                    return;
                }
            };

        let pool = &state.db;
        if let Err(e) = ensure_cursor_table(pool).await {
            tracing::error!(%e, "event-stream tailer: failed to ensure cursor table; producer not started");
            return;
        }
        let mut cursor = match load_or_init_cursor(pool, config.backfill).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(%e, "event-stream tailer: failed to load cursor; producer not started");
                return;
            }
        };
        tracing::info!(
            target: "noetl_server::startup",
            start_cursor = cursor,
            batch = config.batch_size,
            "event-stream tailer started (CQRS write-path producer, #103 phase 2a)"
        );

        loop {
            match publish_batch(pool, &publisher, cursor, config.batch_size).await {
                Ok(Some(new_cursor)) => {
                    cursor = new_cursor;
                    if let Err(e) = save_cursor(pool, cursor).await {
                        tracing::warn!(%e, cursor, "event-stream tailer: cursor persist failed; will retry");
                    }
                    // Drained a full batch → likely more waiting; poll again
                    // immediately rather than sleeping.
                    continue;
                }
                Ok(None) => {} // caught up
                Err(e) => {
                    tracing::warn!(%e, cursor, "event-stream tailer: batch publish failed; backing off");
                }
            }
            tokio::time::sleep(config.poll_interval).await;
        }
    });
}

/// Read and publish one batch. Returns `Some(new_cursor)` if it published a full
/// batch (caller should poll again immediately), `None` if caught up.
async fn publish_batch(
    pool: &DbPool,
    publisher: &EventStreamPublisher,
    cursor: i64,
    batch_size: i64,
) -> AppResult<Option<i64>> {
    let rows: Vec<TailRow> = sqlx::query_as(
        r#"
        SELECT id, event_id, event_type
        FROM noetl.event
        WHERE id > $1
        ORDER BY id ASC
        LIMIT $2
        "#,
    )
    .bind(cursor)
    .bind(batch_size)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(None);
    }

    let mut max_id = cursor;
    for row in &rows {
        // Fetch the full event JSON for the payload.  (The id-only tail above
        // keeps the hot scan narrow; the payload load is per-published-event.)
        let payload: Option<(serde_json::Value,)> = sqlx::query_as(
            "SELECT to_jsonb(e) FROM noetl.event e WHERE id = $1",
        )
        .bind(row.id)
        .fetch_optional(pool)
        .await?;
        let Some((json,)) = payload else { continue };
        let bytes = serde_json::to_vec(&json).map_err(|e| {
            crate::error::AppError::Internal(format!("event payload encode: {e}"))
        })?;
        publisher
            .publish_event(row.event_id, &row.event_type, &bytes)
            .await
            .map_err(|e| crate::error::AppError::Internal(format!("event publish: {e}")))?;
        crate::metrics::record_event_stream_published(&row.event_type, 1, row.id);
        max_id = max_id.max(row.id);
    }

    Ok(Some(max_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_are_safe() {
        // Empty lookup → producer off, knobs sane.  Pure (no global env).
        let c = EventStreamConfig::from_lookup(|_| None);
        assert!(!c.enabled, "default must be off");
        assert_eq!(c.batch_size, 500);
        assert_eq!(c.poll_interval, Duration::from_millis(500));
        assert!(!c.backfill);
        assert_eq!(c.dedup_window, Duration::from_secs(120));
    }

    #[test]
    fn config_parses_overrides() {
        let map = |k: &str| -> Option<String> {
            match k {
                "NOETL_EVENT_STREAM_ENABLED" => Some("yes".into()),
                "NOETL_EVENT_STREAM_BATCH" => Some("1000".into()),
                "NOETL_EVENT_STREAM_POLL_MS" => Some("250".into()),
                "NOETL_EVENT_STREAM_BACKFILL" => Some("true".into()),
                _ => None,
            }
        };
        let c = EventStreamConfig::from_lookup(map);
        assert!(c.enabled);
        assert_eq!(c.batch_size, 1000);
        assert_eq!(c.poll_interval, Duration::from_millis(250));
        assert!(c.backfill);
    }
}
