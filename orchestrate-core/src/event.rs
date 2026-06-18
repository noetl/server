//! The pure event type the drive core reads.
//!
//! `evaluate`/`state` consume the event log, but the server's `db::Event` is
//! `sqlx::FromRow` — native-only — so it can't compile into the wasm core.  This
//! is the db-free read-shape `evaluate` actually needs (noetl/ai-meta#109,
//! design: `orchestrate_core_event_abi.md`).
//!
//! **Named to converge.**  The field set deliberately mirrors the CQRS
//! materializer's `EventEnvelope`, under the canonical name `event::Event`, so
//! when the JetStream WAL record becomes the system's one true event
//! (noetl/ai-meta#104) this is the seed to *promote*, not a fourth shape to
//! reconcile.  The server converts its `db::Event` into this at the
//! `trigger_orchestrator` boundary via a `From` impl.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One event in an execution's log, as the drive core reads it.  A pure subset
/// of the server's `db::Event` — no DB serial `id`, `catalog_id`, `parent_event_id`,
/// `node_id`, `node_type`, or `worker_id` (the drive never reads those).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Application-side snowflake id — the drive's ordering key.
    pub event_id: i64,
    pub execution_id: i64,
    pub event_type: String,
    /// The step name (`node_name` in the DB / envelope).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_name: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
    /// Event-sourced timestamp — the drive reads this, never `Utc::now()`.  The
    /// DB column is `created_at`; the WAL/envelope name is `timestamp`, so accept
    /// both on the wire to converge cleanly with #104.
    #[serde(alias = "created_at")]
    pub timestamp: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_execution_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<i32>,
}
