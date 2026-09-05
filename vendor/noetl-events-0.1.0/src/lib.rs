//! NoETL shared event envelope.
//!
//! This crate is the single canonical home for the wire-format
//! `ExecutorEvent` envelope and the `EventSink` trait every NoETL
//! Rust binary emits events through.  Producers (CLI's local-mode
//! runner, noetl-worker's NATS pull consumer) and consumers
//! (noetl-server's `POST /api/events` endpoint, the projector that
//! writes the `noetl.event` row, the gateway's SSE stream) all
//! agree on the shape because they all link this one type.
//!
//! Until EE-4 the envelope lived in `noetl-executor::events`.
//! That worked while only producers needed it — the noetl-server
//! kept its own hand-aligned `EventRequest` and the alignment was
//! enforced by code review.  When the Rust server started
//! producing events itself (Phase D R2 orchestrator wiring, see
//! noetl/server#31) the duplicated type became actively
//! maintenance-heavy, so EE-4 (noetl/ai-meta#49 follow-up) carves
//! it out into this dependency-free crate that every component
//! can link without dragging in the rest of the executor's
//! surface (playbook parser, runtime trait, tools bridge, …).
//!
//! Re-export contract: `noetl-executor::events` re-exports every
//! public symbol from this crate verbatim so the existing call
//! sites keep working unchanged.  EE-4 PR 2 publishes the crate
//! to crates.io and EE-4 PR 3 adds it as a direct dependency to
//! `noetl-server`.
//!
//! # Wire-format alignment
//!
//! `ExecutorEvent` is intentionally close to the Python
//! `EventEmitRequest` shape so events emitted by either stack
//! project against the same `noetl.event` columns.  See the
//! noetl/noetl wiki page `handle_event_timing` for the field
//! catalogue and the per-field semantics.

use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};

/// One event the executor emits.  Keep field naming aligned with the
/// Python side (`noetl.event` table columns) so envelopes can be
/// projected by either stack.
///
/// R-1.2 PR-2a: `execution_id` is now `i64` (matching the Python
/// `noetl.event.execution_id` bigint column, the CLI's
/// `BridgeContext.execution_id`, the worker's
/// `CommandNotification.execution_id`, and
/// `noetl_tools::context::ExecutionContext.execution_id`).
///
/// R-1.2 PR-EE-1 (0.3.1): adds the optional fields the Python
/// `EventEmitRequest` (and the Rust `noetl-server` `EventRequest`)
/// already expect — `event_id`, `worker_id`, `meta`.  All are
/// `Option` + `#[serde(default, skip_serializing_if =
/// "Option::is_none")]` so older wire-format consumers deserialize
/// cleanly and producers that don't populate them omit them from
/// the JSON entirely.  This step is the additive prerequisite for
/// the cross-repo event envelope reconciliation tracked on
/// [noetl/ai-meta#30](https://github.com/noetl/ai-meta/issues/30).
///
/// EE-4 (noetl/ai-meta#49): the struct moved out of
/// `noetl-executor::events` into this dedicated `noetl-events`
/// crate so noetl-server can link it directly.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExecutorEvent {
    /// Execution this event belongs to.  Matches the
    /// `noetl.event.execution_id` bigint column.
    pub execution_id: i64,

    /// Event type (e.g. `"step.enter"`, `"call.done"`,
    /// `"step.exit"`, `"command.completed"`, `"command.failed"`).
    /// Python side uses an `EventType` enum but Rust accepts the
    /// raw string; the server validates against the enum when
    /// projecting.
    pub event_type: String,

    /// Step name (`node_id` / `node_name` in the Python projector).
    pub step: String,

    /// Lifecycle status (e.g. `"STARTED"`, `"COMPLETED"`,
    /// `"FAILED"`).
    pub status: String,

    /// Wall-clock when the event was produced.  Stamped at emit
    /// time so the event log preserves per-component ordering
    /// even across server-clock skew.
    pub created_at: DateTime<Utc>,

    /// Free-form payload; the projector reads typed fields out
    /// of this.  Renamed from the worker's `payload` field to
    /// match the Python `EventEmitRequest.context` field; serde
    /// alias accepts either name on the wire.
    #[serde(alias = "payload")]
    pub context: serde_json::Value,

    /// Application-side snowflake id for this event.  Per
    /// [`agents/rules/observability.md`][rule] Principle 3,
    /// the emitting process generates this BEFORE the row hits
    /// the database so spans / metrics / cross-component
    /// correlation can use it immediately.  `None` falls back to
    /// the DB-side default (existing `gen_snowflake()` function).
    ///
    /// [rule]: https://github.com/noetl/ai-meta/blob/main/agents/rules/observability.md
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<i64>,

    /// Worker that emitted the event (worker pod's id, or
    /// `"cli-local"` for CLI mode).  Used for shard-aware queries
    /// + diagnostic correlation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,

    /// Free-form metadata that doesn't belong in `context` —
    /// retry counts, parent-event refs, catalog ids.  Matches the
    /// Python `EventEmitRequest.meta` field 1:1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
}

/// Pluggable sink the executor emits `ExecutorEvent`s through.
///
/// Producers swap implementations without touching dispatch code:
///
/// - CLI's local-mode runner: a stdout / record-replay sink.
/// - noetl-worker: an HTTP sink that POSTs to `/api/events`.
/// - tests: `NoopSink` below.
///
/// Implementations should be idempotent per `event_id` once R-1.2
/// adds id assignment.
#[async_trait]
pub trait EventSink: Send + Sync {
    /// Emit `event` to whatever durable surface the sink is bound to.
    async fn emit(&self, event: ExecutorEvent) -> Result<()>;
}

/// Trait alias used by the dispatch loop — wraps a sink and a
/// pre-computed `execution_id` so callers don't have to thread it
/// through every call site.
pub struct EventEmitter {
    pub sink: std::sync::Arc<dyn EventSink>,
    pub execution_id: i64,
}

impl EventEmitter {
    pub fn new(execution_id: i64, sink: std::sync::Arc<dyn EventSink>) -> Self {
        Self { sink, execution_id }
    }

    pub async fn emit(
        &self,
        event_type: &str,
        step: &str,
        status: &str,
        context: serde_json::Value,
    ) -> Result<()> {
        let event = ExecutorEvent {
            execution_id: self.execution_id,
            event_type: event_type.to_string(),
            step: step.to_string(),
            status: status.to_string(),
            created_at: Utc::now(),
            context,
            event_id: None,
            worker_id: None,
            meta: None,
        };
        self.sink.emit(event).await
    }
}

/// Drops every event on the floor.  Useful in tests and as the default
/// during the R-1.1 skeleton phase before the real CLI/worker sinks
/// exist.
#[derive(Default)]
pub struct NoopSink;

#[async_trait]
impl EventSink for NoopSink {
    async fn emit(&self, _event: ExecutorEvent) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn noop_sink_accepts_any_event() {
        let sink: Arc<dyn EventSink> = Arc::new(NoopSink);
        let emitter = EventEmitter::new(12345, sink);
        emitter
            .emit(
                "batch.completed",
                "start",
                "COMPLETED",
                serde_json::json!({"processing_ms": 12.3}),
            )
            .await
            .expect("noop emit");
    }

    // ---- R-1.2 PR-EE-1 — envelope enrichment ------------------------

    fn dummy_event() -> ExecutorEvent {
        ExecutorEvent {
            execution_id: 1,
            event_type: "step.enter".to_string(),
            step: "fetch".to_string(),
            status: "STARTED".to_string(),
            created_at: Utc::now(),
            context: serde_json::json!({}),
            event_id: None,
            worker_id: None,
            meta: None,
        }
    }

    #[test]
    fn new_optional_fields_omit_from_serialized_json_when_none() {
        let event = dummy_event();
        let json = serde_json::to_value(&event).unwrap();
        // The new optional fields should not appear in the wire
        // format when None — older consumers shouldn't see
        // unfamiliar keys.
        assert!(json.get("event_id").is_none(), "event_id omitted");
        assert!(json.get("worker_id").is_none(), "worker_id omitted");
        assert!(json.get("meta").is_none(), "meta omitted");
    }

    #[test]
    fn new_optional_fields_serialize_when_present() {
        let event = ExecutorEvent {
            event_id: Some(478775660589088777),
            worker_id: Some("worker-1".to_string()),
            meta: Some(serde_json::json!({"attempts": 2})),
            ..dummy_event()
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["event_id"], serde_json::json!(478775660589088777_i64));
        assert_eq!(json["worker_id"], "worker-1");
        assert_eq!(json["meta"]["attempts"], 2);
    }

    #[test]
    fn deserializes_payload_alias_into_context() {
        // Older wire format uses `payload`; new envelope uses
        // `context`.  Serde alias lets both deserialize.
        let json = serde_json::json!({
            "execution_id": 5,
            "event_type": "step.enter",
            "step": "s",
            "status": "STARTED",
            "created_at": "2026-05-31T00:00:00Z",
            "payload": {"foo": "bar"},
        });
        let event: ExecutorEvent = serde_json::from_value(json).unwrap();
        assert_eq!(event.context, serde_json::json!({"foo": "bar"}));
    }

    #[test]
    fn deserializes_missing_optional_fields_with_none() {
        // Wire format without event_id / worker_id / meta should
        // deserialize cleanly with the new fields set to None.
        let json = serde_json::json!({
            "execution_id": 5,
            "event_type": "step.enter",
            "step": "s",
            "status": "STARTED",
            "created_at": "2026-05-31T00:00:00Z",
            "context": {},
        });
        let event: ExecutorEvent = serde_json::from_value(json).unwrap();
        assert!(event.event_id.is_none());
        assert!(event.worker_id.is_none());
        assert!(event.meta.is_none());
    }

    #[test]
    fn round_trips_with_all_optional_fields_set() {
        let original = ExecutorEvent {
            execution_id: 478775660589088776,
            event_type: "command.completed".to_string(),
            step: "fetch_calendar".to_string(),
            status: "COMPLETED".to_string(),
            created_at: chrono::DateTime::parse_from_rfc3339("2026-05-31T03:14:15Z")
                .unwrap()
                .with_timezone(&Utc),
            context: serde_json::json!({"result": {"items": 42}}),
            event_id: Some(478775660589088777),
            worker_id: Some("worker-prod-7".to_string()),
            meta: Some(serde_json::json!({"attempts": 3, "parent_event_id": "478775660589088770"})),
        };
        let json = serde_json::to_value(&original).unwrap();
        let parsed: ExecutorEvent = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.execution_id, original.execution_id);
        assert_eq!(parsed.event_id, original.event_id);
        assert_eq!(parsed.worker_id, original.worker_id);
        assert_eq!(parsed.meta, original.meta);
    }
}
