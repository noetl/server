//! **Durable inbox**: the task board written through to Postgres (`noetl.instance_tasks`).
//!
//! On with `NOETL_INSTANCE_STORE=postgres` (strict); unset, the board stays in memory and
//! nothing here runs. The board stays the working set (claims, long-poll wake-ups, limits all
//! work on it). This module only makes it survive a restart:
//!
//! - every change is upserted as one row per task: the whole task as JSONB plus a `rev`
//!   counter. An upsert never replaces a row with a higher `rev`, so two writes that race
//!   out of order cannot roll a task back;
//! - a pruned task's row is deleted;
//! - at start the rows are read back and the board rebuilt ([`super::tasks::Board::restore`]):
//!   queued tasks stay queued, a claimed task keeps its claim (the target's result still
//!   lands, and a lapsed lease redelivers as before), finished tasks stay readable until
//!   pruned.
//!
//! A submit is acknowledged only after its row is written; if the write fails the task is
//! dropped and the submitter gets 503 (retryable). Any other change whose write fails is
//! retried with the next write, and counted (`noetl_instance_total{outcome="store_failed"}`).
//!
//! Callback tokens are never written: a callback still due across a restart is not sent, and
//! the submitter falls back to polling the task, which is the documented contract anyway.
//! The table is created idempotently at start-up, like the server's other `noetl.*` tables.

use async_trait::async_trait;

use super::tasks::{Changes, Stored};
use crate::db::DbPool;

/// `memory` (default) or `postgres`. Strict.
pub const STORE_ENV: &str = "NOETL_INSTANCE_STORE";

pub fn postgres_store_enabled() -> bool {
    std::env::var(STORE_ENV).ok().as_deref() == Some("postgres")
}

#[async_trait]
pub trait TaskStore: Send + Sync {
    async fn write(&self, changes: &Changes) -> Result<(), String>;
    async fn load(&self) -> Result<Vec<Stored>, String>;
}

pub struct PgTaskStore {
    pool: DbPool,
}

impl PgTaskStore {
    pub async fn new(pool: DbPool) -> Result<Self, String> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS noetl.instance_tasks (
                task_id    TEXT PRIMARY KEY,
                target     TEXT NOT NULL,
                state      TEXT NOT NULL,
                rev        BIGINT NOT NULL,
                record     JSONB NOT NULL,
                updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
            )
            "#,
        )
        .execute(&pool)
        .await
        .map_err(|e| format!("create noetl.instance_tasks: {e}"))?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS instance_tasks_target_state ON noetl.instance_tasks (target, state)",
        )
        .execute(&pool)
        .await
        .map_err(|e| format!("index noetl.instance_tasks: {e}"))?;
        Ok(PgTaskStore { pool })
    }
}

fn state_name(s: &Stored) -> String {
    serde_json::to_value(s.state)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

#[async_trait]
impl TaskStore for PgTaskStore {
    async fn write(&self, changes: &Changes) -> Result<(), String> {
        if changes.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;
        for s in &changes.upserts {
            let record = serde_json::to_value(s).map_err(|e| e.to_string())?;
            sqlx::query(
                r#"
                INSERT INTO noetl.instance_tasks (task_id, target, state, rev, record, updated_at)
                VALUES ($1, $2, $3, $4, $5, now())
                ON CONFLICT (task_id) DO UPDATE
                   SET target = EXCLUDED.target, state = EXCLUDED.state, rev = EXCLUDED.rev,
                       record = EXCLUDED.record, updated_at = now()
                 WHERE noetl.instance_tasks.rev < EXCLUDED.rev
                "#,
            )
            .bind(&s.task_id)
            .bind(&s.to)
            .bind(state_name(s))
            .bind(s.rev as i64)
            .bind(record)
            .execute(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
        }
        if !changes.removed.is_empty() {
            sqlx::query("DELETE FROM noetl.instance_tasks WHERE task_id = ANY($1)")
                .bind(&changes.removed)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        }
        tx.commit().await.map_err(|e| e.to_string())
    }

    async fn load(&self) -> Result<Vec<Stored>, String> {
        let rows: Vec<(serde_json::Value,)> =
            sqlx::query_as("SELECT record FROM noetl.instance_tasks")
                .fetch_all(&self.pool)
                .await
                .map_err(|e| e.to_string())?;
        let mut out = Vec::with_capacity(rows.len());
        for (v,) in rows {
            match serde_json::from_value::<Stored>(v) {
                Ok(s) => out.push(s),
                Err(e) => {
                    tracing::warn!(error = %e, "skipping an unreadable noetl.instance_tasks row")
                }
            }
        }
        Ok(out)
    }
}

/// An in-process store for tests: the same contract (rev guard, deletes) as Postgres.
#[derive(Default)]
pub struct MemStore {
    pub rows: std::sync::Mutex<std::collections::HashMap<String, Stored>>,
    pub fail: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl TaskStore for MemStore {
    async fn write(&self, changes: &Changes) -> Result<(), String> {
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("injected store failure".into());
        }
        let mut rows = self.rows.lock().unwrap_or_else(|p| p.into_inner());
        for s in &changes.upserts {
            match rows.get(&s.task_id) {
                Some(old) if old.rev >= s.rev => {}
                _ => {
                    rows.insert(s.task_id.clone(), s.clone());
                }
            }
        }
        for id in &changes.removed {
            rows.remove(id);
        }
        Ok(())
    }

    async fn load(&self) -> Result<Vec<Stored>, String> {
        Ok(self
            .rows
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect())
    }
}
