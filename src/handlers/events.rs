//! Event handling API handlers.
//!
//! Handles worker events and command retrieval endpoints.
//!
//! SECURITY: All event payloads are sanitized before storage to prevent
//! sensitive data (bearer tokens, passwords, API keys) from being persisted.

use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tracing::{debug, info, warn};

use crate::error::{AppError, AppResult};
use crate::sanitize::sanitize_sensitive_data;
use crate::state::AppState;

/// Deserialize a snowflake-id field that may arrive on the wire as
/// either a JSON string (the historical browser-facing shape) or a
/// JSON integer (the shape `noetl-events::ExecutorEvent` emits over
/// `.json(&event)`).  Both decode to `String` so the rest of the
/// handler is unchanged.
///
/// Why the lax decoder: the worker's canonical envelope types
/// `execution_id` / `event_id` as `i64`, the Rust server's request
/// shape kept them as `String` for browser JSON-number precision,
/// and the Python server (Pydantic v2 lax mode) coerced int→str
/// silently for over a year — so the drift only manifested once
/// Rust-on-both-ends went through the same path.  See
/// `noetl/ai-meta#55` for the surfacing in Phase F R5.
fn deserialize_string_or_i64<'de, D>(deserializer: D) -> std::result::Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    use std::fmt;

    struct StringOrI64;

    impl<'de> Visitor<'de> for StringOrI64 {
        type Value = String;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a string or signed/unsigned integer representing a snowflake id")
        }

        fn visit_str<E>(self, v: &str) -> std::result::Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(v.to_string())
        }

        fn visit_string<E>(self, v: String) -> std::result::Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(v)
        }

        fn visit_i64<E>(self, v: i64) -> std::result::Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(v.to_string())
        }

        fn visit_u64<E>(self, v: u64) -> std::result::Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(v.to_string())
        }
    }

    deserializer.deserialize_any(StringOrI64)
}

/// `Option<String>` variant of [`deserialize_string_or_i64`] for
/// optional id fields like `EventRequest.event_id`.  Accepts
/// missing / null / string / integer.
fn deserialize_optional_string_or_i64<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};
    use std::fmt;

    struct OptStringOrI64;

    impl<'de> Visitor<'de> for OptStringOrI64 {
        type Value = Option<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str(
                "null, a string, or a signed/unsigned integer representing an optional snowflake id",
            )
        }

        fn visit_none<E>(self) -> std::result::Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> std::result::Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(None)
        }

        fn visit_some<D2>(self, deserializer: D2) -> std::result::Result<Self::Value, D2::Error>
        where
            D2: serde::Deserializer<'de>,
        {
            deserialize_string_or_i64(deserializer).map(Some)
        }

        fn visit_str<E>(self, v: &str) -> std::result::Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Some(v.to_string()))
        }

        fn visit_string<E>(self, v: String) -> std::result::Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Some(v))
        }

        fn visit_i64<E>(self, v: i64) -> std::result::Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Some(v.to_string()))
        }

        fn visit_u64<E>(self, v: u64) -> std::result::Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Some(v.to_string()))
        }
    }

    deserializer.deserialize_any(OptStringOrI64)
}

/// Worker event request.
///
/// The shared subset of fields with the canonical
/// [`noetl_events::ExecutorEvent`] envelope (from the
/// `noetl-events` crate published off
/// [noetl/cli](https://github.com/noetl/cli)) — `execution_id`,
/// `step`, `event_type` (with `name` alias), `payload`/`context`,
/// `meta`, `worker_id`, `event_id`, `status`, `created_at` — is
/// wire-format compatible.  The wire-compat test
/// `wire_compat_round_trips_shared_subset_with_executor_event`
/// guards this property.  EE-4 (noetl/ai-meta#49) extracted the
/// shared envelope into the dedicated `noetl-events` crate and
/// added a direct dep on it here so the wire shape has a single
/// source of truth instead of being held in sync by hand-aligned
/// doc comments.
///
/// `EventRequest` keeps several server-only fields beyond the
/// canonical envelope: `result_kind`, `result_uri`, `event_ids`
/// (drive the constraint-compliant `{status, reference}` /
/// `{status, context}` result shape per noetl/server#29);
/// `actionable`, `informative` (control orchestrator dispatch +
/// log-only persistence).  Wire-encodes `execution_id` /
/// `event_id` as `String` for JSON-number precision in browser
/// clients, vs the envelope's `i64` — the `From` / `TryFrom`
/// impls below handle the conversion at the boundary.
///
/// Pre-EE-2 (`name`-field) worker / CLI clients keep working via
/// `#[serde(alias = "name")]`; producers that omit
/// `event_id` / `status` / `created_at` get sensible server-side
/// fallbacks (DB-side `snowflake_id()` for `event_id`, the
/// name-derived `status` returned by `event_status_from_name`,
/// `Utc::now()` for `created_at`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRequest {
    /// Execution ID.  Wire format is `String` (matches the Python
    /// `EventEmitRequest` and avoids JSON-number precision loss
    /// for large snowflakes in browser clients); parsed to `i64`
    /// before the DB write.
    ///
    /// The `deserialize_with` adapter accepts the worker's canonical
    /// `noetl-events::ExecutorEvent.execution_id: i64` wire shape
    /// as well, so Rust-on-both-ends doesn't fail at the boundary.
    /// See `noetl/ai-meta#55` for the drift this fixes.
    #[serde(deserialize_with = "deserialize_string_or_i64")]
    pub execution_id: String,
    /// Step name.
    pub step: String,
    /// Event type (e.g. `step.enter`, `call.done`, `step.exit`,
    /// `command.completed`).  R-1.2 PR-EE-2: renamed from `name`;
    /// the alias keeps pre-PR-EE clients working.
    #[serde(alias = "name")]
    pub event_type: String,
    /// Event payload/result data.
    ///
    /// R-1.2 PR-EE-2: `context` alias accepted so producers that
    /// send the executor's `ExecutorEvent.context` field
    /// deserialize cleanly into this `payload`.
    #[serde(default, alias = "context")]
    pub payload: serde_json::Value,
    /// Additional metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
    /// Worker ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    /// Result kind: "data", "ref", or "refs".
    #[serde(default = "default_result_kind")]
    pub result_kind: String,
    /// Result URI for ref kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_uri: Option<String>,
    /// Event IDs for refs kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_ids: Option<Vec<i64>>,
    /// If true, server should take action.
    #[serde(default = "default_true")]
    pub actionable: bool,
    /// If true, event is for logging/observability.
    #[serde(default = "default_true")]
    pub informative: bool,
    /// Application-side snowflake ID for this event.  Per
    /// `agents/rules/observability.md` Principle 3, the emitting
    /// process generates this BEFORE the row hits the database so
    /// spans / metrics / cross-component correlation can use it
    /// immediately.  Wire format is `String` to avoid JSON-number
    /// precision loss; parsed to `i64` for the DB write.
    /// `None` falls back to the server-side `noetl.snowflake_id()`
    /// function (the existing default).
    ///
    /// Accepts both the `String` wire shape (browser clients) and
    /// the `i64` wire shape (worker's canonical envelope).  See
    /// `noetl/ai-meta#55`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_optional_string_or_i64"
    )]
    pub event_id: Option<String>,
    /// Lifecycle status (`STARTED` / `RUNNING` / `COMPLETED` /
    /// `FAILED`).  `None` falls back to name-based derivation in
    /// `event_status_from_name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Wall-clock when the event was produced.  `None` falls back
    /// to `chrono::Utc::now()`.  Stamping at emit time preserves
    /// per-component ordering across server-clock skew (matters
    /// when multiple workers emit in tight bursts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<chrono::DateTime<chrono::Utc>>,
}

fn default_result_kind() -> String {
    "data".to_string()
}

fn default_true() -> bool {
    true
}

/// Project the canonical `noetl_events::ExecutorEvent` envelope (the
/// shape every NoETL Rust producer emits through `EventSink`) into
/// the server's wire request shape.
///
/// Server-only fields (`result_kind`, `result_uri`, `event_ids`,
/// `actionable`, `informative`) get the same defaults the handler
/// applies when a producer omits them.  `execution_id` + `event_id`
/// flip to `String` because that's the wire format the server has
/// always exposed to browser clients (JSON-number precision).
impl From<noetl_events::ExecutorEvent> for EventRequest {
    fn from(ev: noetl_events::ExecutorEvent) -> Self {
        Self {
            execution_id: ev.execution_id.to_string(),
            step: ev.step,
            event_type: ev.event_type,
            payload: ev.context,
            meta: ev.meta,
            worker_id: ev.worker_id,
            result_kind: default_result_kind(),
            result_uri: None,
            event_ids: None,
            actionable: true,
            informative: true,
            event_id: ev.event_id.map(|id| id.to_string()),
            status: Some(ev.status),
            created_at: Some(ev.created_at),
        }
    }
}

/// Inverse of [`From<noetl_events::ExecutorEvent>`].  `TryFrom`
/// rather than `From` because the wire-shape `String` execution_id
/// and `String` event_id can fail to parse — the server returns 400
/// in that case in the actual handler.  Server-only fields
/// (`result_kind`, `result_uri`, `event_ids`, `actionable`,
/// `informative`) drop on the floor here — the canonical envelope
/// doesn't model them.  When `status` / `created_at` are absent on
/// the request, the conversion fills them with the same fallbacks
/// the handler uses (`event_status_from_name`, `Utc::now()`).
impl TryFrom<&EventRequest> for noetl_events::ExecutorEvent {
    type Error = anyhow::Error;

    fn try_from(req: &EventRequest) -> std::result::Result<Self, Self::Error> {
        let execution_id: i64 = req.execution_id.parse().map_err(|e| {
            anyhow::anyhow!(
                "execution_id {:?} not parseable as i64: {e}",
                req.execution_id
            )
        })?;
        let event_id = req
            .event_id
            .as_deref()
            .map(|s| s.parse::<i64>())
            .transpose()
            .map_err(|e| {
                anyhow::anyhow!("event_id {:?} not parseable as i64: {e}", req.event_id)
            })?;
        let status = req
            .status
            .clone()
            .unwrap_or_else(|| event_status_from_name(&req.event_type).to_string());
        let created_at = req.created_at.unwrap_or_else(chrono::Utc::now);
        Ok(Self {
            execution_id,
            event_type: req.event_type.clone(),
            step: req.step.clone(),
            status,
            created_at,
            context: req.payload.clone(),
            event_id,
            worker_id: req.worker_id.clone(),
            meta: req.meta.clone(),
        })
    }
}

/// Response for event handling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventResponse {
    /// Status of the operation.
    pub status: String,
    /// Event ID that was created.
    pub event_id: i64,
    /// Number of commands generated.
    pub commands_generated: i32,
}

/// Request to claim a command atomically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimRequest {
    /// Worker ID requesting the claim.
    pub worker_id: String,
}

/// Response for successful claim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimResponse {
    /// Status of the claim operation.
    pub status: String,
    /// Command event ID.
    pub event_id: i64,
    /// Execution ID.
    pub execution_id: i64,
    /// Node/step ID.
    pub node_id: String,
    /// Node/step name.
    pub node_name: String,
    /// Action/tool kind.
    pub action: String,
    /// Command context.
    pub context: serde_json::Value,
    /// Command metadata.
    pub meta: serde_json::Value,
}

/// A single batched worker event.
///
/// R-1.2 PR-EE-2: same `name` → `event_type` rename + serde alias
/// as `EventRequest`; same `context` alias for `payload`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchEventItem {
    /// Step name.
    pub step: String,
    /// Event type.
    #[serde(alias = "name")]
    pub event_type: String,
    /// Event payload/result data.
    #[serde(default, alias = "context")]
    pub payload: serde_json::Value,
    /// If true, server should take action.
    #[serde(default)]
    pub actionable: bool,
    /// If true, event is for logging/observability.
    #[serde(default = "default_true")]
    pub informative: bool,
}

/// Request for batched event ingestion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchEventRequest {
    /// Execution ID.  Accepts both string (browser clients) and
    /// integer (worker's `noetl-events::ExecutorEvent`) wire
    /// shapes.  See `noetl/ai-meta#55`.
    #[serde(deserialize_with = "deserialize_string_or_i64")]
    pub execution_id: String,
    /// Worker ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    /// Events to persist.
    pub events: Vec<BatchEventItem>,
}

/// Response for batched event ingestion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchEventResponse {
    /// Status of the operation.
    pub status: String,
    /// Inserted event IDs.
    pub event_ids: Vec<i64>,
    /// Number of generated commands.
    pub commands_generated: i32,
}

/// Command details response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandResponse {
    /// Execution ID.
    pub execution_id: i64,
    /// Node/step ID.
    pub node_id: String,
    /// Node/step name.
    pub node_name: String,
    /// Action/tool kind.
    pub action: String,
    /// Command context (tool config, args, etc.).
    pub context: serde_json::Value,
    /// Command metadata.
    pub meta: serde_json::Value,
}

/// Handle worker event.
///
/// POST /api/events
///
/// Worker reports completion with result (inline or ref).
/// Engine evaluates case/when/then and generates next commands.
///
/// Instrumented per
/// [`agents/rules/observability.md`](https://github.com/noetl/ai-meta/blob/main/agents/rules/observability.md)
/// Principle 1: a counter (`noetl_events_ingested_total{event_type,status}`)
/// and histogram (`noetl_event_ingest_duration_seconds{event_type}`) are
/// recorded on every dispatch.  See [`handle_event_inner`] for the body
/// of the handler.
pub async fn handle_event(
    state: State<AppState>,
    headers: axum::http::HeaderMap,
    request: Json<EventRequest>,
) -> Result<Json<EventResponse>, AppError> {
    // Execution-affinity (RFC noetl/ai-meta#116): single-owner write ordering.
    // When affinity is active and this replica does not own the execution, the
    // trigger (and the drive it would fire) is forwarded to the owner so the
    // off-server chain head's read→advance stays atomic per execution and never
    // forks across replicas.  Inert / owned executions fall through to local
    // processing; a failed forward degrades to local (no event dropped).
    if let crate::affinity::AffinityRoute::Forwarded(resp) =
        state.0.affinity.route_event(&headers, &request.0).await
    {
        return Ok(Json(resp));
    }

    let event_type_for_metrics = request.0.event_type.clone();
    let started_at = std::time::Instant::now();

    let result = handle_event_inner(state, request).await;

    let status_label = if result.is_ok() { "ok" } else { "error" };
    let duration_seconds = started_at.elapsed().as_secs_f64();
    crate::metrics::record_event_ingest(&event_type_for_metrics, status_label, duration_seconds);

    result
}

/// Inner body of [`handle_event`] — same logic, no instrumentation.
///
/// Split out so the wrapper can record metrics on both Ok and Err
/// paths without coupling the body to the recording call.
/// The normalized `noetl.event` row fields derived from an
/// [`EventRequest`] (which a native `ExecutorEvent` deserializes into).
///
/// This is the shared normalization that the synchronous ingest path
/// (`handle_event_inner`) and the CQRS write-path materializer
/// (`/api/internal/events/materialize`, noetl/ai-meta#103 phase 2d) both
/// apply — so the row materialized from a producer's *native* event is
/// byte-identical to the one the synchronous path writes.  Without a
/// single normalization point the two paths would drift (different
/// `status` derivation, `result` envelope, `meta` shape, `catalog_id`).
pub(crate) struct NormalizedEventRow {
    pub event_id: i64,
    pub execution_id: i64,
    pub catalog_id: Option<i64>,
    pub event_type: String,
    /// `noetl.event.node_id` + `node_name` both take the step name.
    pub node_name: String,
    pub status: String,
    pub result: serde_json::Value,
    pub meta: serde_json::Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Normalize an [`EventRequest`] into the `noetl.event` row shape: derive
/// the lifecycle `status`, build + sanitize the `result` envelope, resolve
/// the `event_id` (producer-stamped or server snowflake), look up the
/// `catalog_id`, and assemble + sanitize `meta`.  Mirrors the inline logic
/// `handle_event_inner` ran before this was extracted.
pub(crate) async fn normalize_event_to_row(
    state: &AppState,
    request: &EventRequest,
) -> Result<NormalizedEventRow, AppError> {
    let execution_id: i64 = request
        .execution_id
        .parse()
        .map_err(|_| AppError::Validation("Invalid execution_id".to_string()))?;

    // Prefer the application-supplied status; fall back to name-based
    // derivation for pre-PR-EE producers.
    let status: String = request
        .status
        .clone()
        .unwrap_or_else(|| event_status_from_name(&request.event_type).to_string());

    // Result envelope (must carry a top-level string `status` for the
    // `chk_event_result_shape` constraint) + credential scrub.
    let result = sanitize_sensitive_data(&build_result_object(request, &status));

    // noetl/ai-meta#104 Phase A: shadow-accept the canonical result URI the
    // worker stamps on over-budget references (`reference.uri`).  Default-off;
    // flag-on parses + validates + records acceptance WITHOUT changing
    // resolution (Phase C) or writing the Feather tier (Phase B).  Sits on the
    // shared normalization chokepoint so the synchronous ingest and the CQRS
    // materializer accept identically — under `NOETL_EVENT_INGEST_PUBLISH_ONLY`
    // both passes run, so the accept counter advances on the publish AND the
    // materialize of the same event (a shadow counter; the duplication is
    // harmless and expected under the gate).
    if state.config.result_uri_accept {
        accept_canonical_result_uri(&result, execution_id);
    }

    // Producer-stamped event_id, else a server-side snowflake.
    let event_id: i64 = match request.event_id.as_deref() {
        Some(raw) => raw
            .parse()
            .map_err(|_| AppError::Validation(format!("Invalid event_id: {raw}")))?,
        None => state.snowflake.generate()?,
    };

    let catalog_id = get_catalog_id(state, execution_id, "get_catalog_id_single").await?;

    let mut meta = request.meta.clone().unwrap_or_else(|| serde_json::json!({}));
    if let serde_json::Value::Object(ref mut map) = meta {
        map.insert("actionable".to_string(), serde_json::json!(request.actionable));
        map.insert(
            "informative".to_string(),
            serde_json::json!(request.informative),
        );
        if let Some(ref worker_id) = request.worker_id {
            map.insert("worker_id".to_string(), serde_json::json!(worker_id));
        }
    }
    let meta = sanitize_sensitive_data(&meta);

    let created_at = request.created_at.unwrap_or_else(chrono::Utc::now);

    Ok(NormalizedEventRow {
        event_id,
        execution_id,
        catalog_id,
        event_type: request.event_type.clone(),
        node_name: request.step.clone(),
        status,
        result,
        meta,
        created_at,
    })
}

async fn handle_event_inner(
    State(state): State<AppState>,
    Json(request): Json<EventRequest>,
) -> Result<Json<EventResponse>, AppError> {
    debug!(
        "Event received: execution_id={}, step={}, event_type={}",
        request.execution_id, request.step, request.event_type
    );

    let execution_id: i64 = request
        .execution_id
        .parse()
        .map_err(|_| AppError::Validation("Invalid execution_id".to_string()))?;

    // Events that should NOT trigger engine processing
    let skip_engine_events = [
        "command.claimed",
        "command.started",
        "command.completed",
        "command.failed",
        "step.enter",
    ];

    // For command.claimed, check if already claimed
    if request.event_type == "command.claimed" {
        if let Some(command_id) = get_command_id(&request) {
            if check_already_claimed(&state, execution_id, &command_id, &request.worker_id).await? {
                // Already claimed by same worker - idempotent success
                return Ok(Json(EventResponse {
                    status: "ok".to_string(),
                    event_id: 0,
                    commands_generated: 0,
                }));
            }
        }
    }

    // Normalize into the `noetl.event` row shape via the shared fn (the
    // same one the CQRS materializer applies to native producer events,
    // #103 phase 2d) so the synchronous + materialized writes are
    // byte-identical.
    let row = normalize_event_to_row(&state, &request).await?;
    let event_id = row.event_id;

    // System meta-command events are NOT workflow events — they're the
    // infrastructure of the worker-driven drive (noetl/ai-meta#108). A single
    // drive emits command.claimed/started/call.done/command.completed for
    // `__orchestrate__`; at scale (thousands of drives) persisting them would
    // burst noetl.event + Postgres for no benefit — the drive state is a pure
    // function of the *real* step events, and the result is applied from the
    // in-memory `call.done` payload, not from a persisted row. So skip the
    // write for the meta-step. (The `command.issued` that delivers the command
    // is written separately by `dispatch_orchestrate_command`; eliminating that
    // last row needs a noetl.event-free claim path — tracked as a follow-up.)
    let is_meta_command =
        request.step == noetl_orchestrate_core::state::WorkflowState::ORCHESTRATE_META_STEP;
    if !is_meta_command {
        // CQRS write-path chokepoint (#103 2d-3): INSERT (gate off, default) or
        // publish to noetl_events (gate on → materializer is the sole writer).
        // `catalog_id` is always Some here — the playbook_started event wrote
        // one before any worker event, and the column is NOT NULL.
        let ev = crate::handlers::event_write::EventRow::new(
            row.event_id,
            row.execution_id,
            row.catalog_id.unwrap_or(0),
            row.event_type.clone(),
            row.status.clone(),
            row.created_at,
        )
        .with_node(row.node_name.clone())
        .with_result(row.result.clone())
        .with_meta(row.meta.clone());
        crate::handlers::event_write::emit_event(
            &state,
            state.pools.pool_for(execution_id),
            ev,
        )
        .await?;
    } else {
        crate::metrics::record_orchestrate_drive("event_suppressed");
    }

    info!(
        "Event persisted: event_id={}, execution_id={}, event_type={}",
        event_id, execution_id, request.event_type
    );

    // Process through engine if applicable
    let commands_generated = if !skip_engine_events.contains(&request.event_type.as_str()) {
        // TODO: Implement engine event handling
        // This would call the orchestrator to evaluate next steps
        debug!(
            "Would process through engine: event_type={}",
            request.event_type
        );
        0
    } else {
        debug!(
            "Skipped engine for administrative event: {}",
            request.event_type
        );
        0
    };

    // Trigger orchestrator for workflow progression.
    //
    // `command.completed` advances the workflow to the next step;
    // `command.failed` checks whether the failure should terminate
    // the playbook (noetl/ai-meta#58 — without this trigger, failed
    // steps stalled the execution forever because the orchestrator
    // never got a chance to emit `playbook.failed`).
    //
    // The earlier `step != "end"` guard treated `end` as a sentinel
    // whose completion fired playbook.completed implicitly.  After
    // the noetl/ai-meta#54 orchestrator change (end is a real step
    // with its own `tool:` block), end's command.completed MUST
    // trigger the orchestrator — that's the pass where
    // `check_completion` sees end as done and emits
    // `playbook.completed`.  Without this trigger the playbook
    // stalled at `command.completed [end]` with no terminal event.
    if request.step == noetl_orchestrate_core::state::WorkflowState::ORCHESTRATE_META_STEP {
        // Worker-driven drive (noetl/ai-meta#108 slice 3): the `system/orchestrate`
        // plug-in's output (the OrchestrationResult) rides the `call.done` event
        // — the lifecycle `command.completed`/`claimed`/`started` carry no output.
        // Apply it (emit events + issue the real commands) instead of triggering
        // the orchestrator. Those real commands' completions re-trigger the drive
        // — the loop continues; the meta-command's own events never do.
        if request.event_type == "call.done" {
            match apply_worker_orchestration(&state, execution_id, event_id, &request.payload).await
            {
                Ok(cmds) => info!(
                    execution_id,
                    commands = cmds,
                    "worker-driven: applied orchestrate result"
                ),
                Err(e) => warn!(execution_id, error = %e, "worker-driven: apply failed"),
            }
        }
    } else if (request.event_type == "command.completed" || request.event_type == "command.failed")
        && !crate::handlers::event_write::should_publish(&state, row.catalog_id.unwrap_or(0)).await
    {
        // CQRS write-path cutover (#103 2d-3): when the gate is on, this event was
        // PUBLISHED, not written — `noetl.event` doesn't have it yet, so triggering
        // here would rebuild stale state. The trigger relocates to the materializer's
        // write endpoint (`handlers::internal::events_project`), which fires it AFTER
        // the row is durably inserted (read-your-writes). Gate off (default): trigger
        // inline exactly as today.
        match trigger_orchestrator(&state, execution_id, event_id).await {
            Ok(cmds) => {
                info!(
                    "Orchestrator generated {} commands for execution {}",
                    cmds, execution_id
                );
            }
            Err(e) => {
                warn!("Orchestrator error: {}", e);
            }
        }
    }

    Ok(Json(EventResponse {
        status: "ok".to_string(),
        event_id,
        commands_generated,
    }))
}

/// Resolve an offloaded command context back to its full `{tool_config, args,
/// render_context}` (noetl/ai-meta#114).
///
/// When a `command.issued` context exceeded the budget, `persist_engine_command`
/// stashed it in `noetl.result_store` and left a tiny
/// `{ "__context_ref__": "noetl://…" }` marker on the event + command row so the
/// published event stayed under the NATS `max_payload`.  Both `get_command` and
/// `claim_command` call this before handing the command to the worker, so the
/// worker is oblivious to the offload — it always receives the full context.
///
/// A within-budget (un-offloaded) context has no marker and is returned
/// unchanged.  On any resolution failure the marker is returned as-is and a WARN
/// is logged with `execution_id`; the worker then surfaces a missing-config
/// error rather than the server silently substituting empty data.
async fn resolve_command_context_ref(
    state: &AppState,
    context: serde_json::Value,
) -> serde_json::Value {
    use crate::handlers::execute::COMMAND_CONTEXT_REF_KEY;
    use crate::services::result_store::{parse_noetl_ref, ResultStoreService};

    let Some(ref_uri) = context
        .get(COMMAND_CONTEXT_REF_KEY)
        .and_then(|v| v.as_str())
    else {
        return context;
    };

    let parsed = match parse_noetl_ref(ref_uri) {
        Ok(p) => p,
        Err(e) => {
            warn!(ref_uri, %e, "command context reference unparseable; left as-is (noetl/ai-meta#114)");
            return context;
        }
    };
    let result_store =
        ResultStoreService::new(state.pools.pool_for(parsed.execution_id).clone(), state.snowflake.clone());
    match result_store.resolve(&parsed).await {
        Ok(Some(data)) => {
            crate::metrics::record_orchestrate_drive("context_ref_resolved");
            data
        }
        Ok(None) => {
            warn!(
                execution_id = parsed.execution_id,
                ref_uri, "command context reference not found in store; left as-is (noetl/ai-meta#114)"
            );
            context
        }
        Err(e) => {
            warn!(
                execution_id = parsed.execution_id,
                ref_uri, %e, "command context reference resolution failed; left as-is (noetl/ai-meta#114)"
            );
            context
        }
    }
}

/// Get command details from command.issued event.
///
/// GET /api/commands/{event_id}
///
/// Workers call this to fetch command config after NATS notification.
pub async fn get_command(
    State(state): State<AppState>,
    Path(event_id): Path<i64>,
) -> Result<Json<CommandResponse>, AppError> {
    debug!("Getting command for event_id={}", event_id);

    // Phase F R4-4: `GET /api/commands/{event_id}` is keyed by
    // event_id alone — execution_id isn't known until after the
    // lookup.  Use the cross-shard resolver: probe every shard,
    // first hit wins.  In single-pool fallback mode this is a
    // single probe against the one pool.
    // noetl.event is authoritative for normal commands; the event-free
    // `system/orchestrate` meta-command (noetl/ai-meta#108) is served from
    // noetl.command as a fallback (`pri` ordering keeps noetl.event first).
    let found = state
        .pools
        .find_first(|_shard_idx, pool| async move {
            sqlx::query_as::<_, (i64, String, String, serde_json::Value, serde_json::Value)>(
                r#"
                SELECT execution_id, node_name, node_type, context, meta FROM (
                    SELECT execution_id, node_name, node_type, context, meta, 0 AS pri
                    FROM noetl.event WHERE event_id = $1 AND event_type = 'command.issued'
                    UNION ALL
                    SELECT execution_id, step_name AS node_name, tool_kind AS node_type,
                           context, meta, 1 AS pri
                    FROM noetl.command WHERE event_id = $1
                ) s ORDER BY pri LIMIT 1
                "#,
            )
            .bind(event_id)
            .fetch_optional(&pool)
            .await
        })
        .await?;
    let row = found.map(|(_shard_idx, r)| r);

    match row {
        Some((execution_id, node_name, node_type, context, meta)) => {
            // Resolve an offloaded context back to the full payload before the
            // worker sees it (noetl/ai-meta#114).  No-op for within-budget
            // commands.
            let context = resolve_command_context_ref(&state, context).await;
            Ok(Json(CommandResponse {
                execution_id,
                node_id: node_name.clone(),
                node_name,
                action: node_type,
                context,
                meta,
            }))
        }
        None => Err(AppError::NotFound(format!("command not found: {}", event_id))),
    }
}

/// Atomically claim command and return command details.
///
/// POST /api/commands/{event_id}/claim
pub async fn claim_command(
    State(state): State<AppState>,
    Path(event_id): Path<i64>,
    Json(request): Json<ClaimRequest>,
) -> Result<Json<ClaimResponse>, AppError> {
    debug!(
        "Claim request received: event_id={}, worker_id={}",
        event_id, request.worker_id
    );

    // Phase F R4-4: resolve event_id -> execution_id via the
    // cross-shard probe, then open the tx on the per-execution
    // pool.  Two round trips in sharded mode (one probe + one tx
    // open) is the right trade-off vs. holding the tx open
    // across a fan-out scan — keeping shard-locality on the
    // claim transaction means the second SELECT (terminal-row
    // check) and any subsequent INSERTs all hit the same shard
    // and stay within the same tx scope.
    //
    // In single-pool fallback mode the resolver short-circuits
    // (one pool, one probe).
    // Resolve event_id -> execution_id. Normal commands carry a `command.issued`
    // event in noetl.event; the worker-driven `system/orchestrate` meta-command
    // (noetl/ai-meta#108) deliberately writes NO event (it would burst Postgres
    // at scale) — its command lives only in noetl.command. The `pri` ordering
    // keeps noetl.event authoritative when present (normal commands: exact prior
    // behavior); noetl.command is the fallback that only fires for the
    // event-free meta-command.
    let resolved_execution_id: Option<i64> = state
        .pools
        .find_first(|_shard_idx, pool| async move {
            sqlx::query_scalar::<_, i64>(
                r#"
                SELECT execution_id FROM (
                    SELECT execution_id, 0 AS pri FROM noetl.event
                    WHERE event_id = $1 AND event_type = 'command.issued'
                    UNION ALL
                    SELECT execution_id, 1 AS pri FROM noetl.command WHERE event_id = $1
                ) s ORDER BY pri LIMIT 1
                "#,
            )
            .bind(event_id)
            .fetch_optional(&pool)
            .await
        })
        .await?
        .map(|(_shard_idx, eid)| eid);

    let resolved_execution_id = resolved_execution_id
        .ok_or_else(|| AppError::NotFound(format!("command not found: {}", event_id)))?;

    let mut tx = state.pools.pool_for(resolved_execution_id).begin().await?;

    let cmd_row = sqlx::query(
        r#"
        SELECT execution_id, catalog_id, node_name, node_type, context, meta FROM (
            SELECT execution_id, catalog_id, node_name, node_type, context, meta, 0 AS pri
            FROM noetl.event WHERE event_id = $1 AND event_type = 'command.issued'
            UNION ALL
            SELECT execution_id, catalog_id, step_name AS node_name, tool_kind AS node_type,
                   context, meta, 1 AS pri
            FROM noetl.command WHERE event_id = $1
        ) s ORDER BY pri LIMIT 1
        "#,
    )
    .bind(event_id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(row) = cmd_row else {
        return Err(AppError::NotFound(format!("command not found: {}", event_id)));
    };

    let execution_id: i64 = row.try_get("execution_id")?;
    let catalog_id: Option<i64> = row.try_get("catalog_id")?;
    let step: String = row.try_get("node_name")?;
    let tool_kind: String = row.try_get("node_type")?;
    let context: serde_json::Value = row
        .try_get("context")
        .unwrap_or_else(|_| serde_json::json!({}));
    // Resolve an offloaded context back to the full payload before the worker
    // sees it (noetl/ai-meta#114).  No-op for within-budget commands.  Both the
    // idempotent-reclaim and the fresh-claim return paths below use this value.
    let context = resolve_command_context_ref(&state, context).await;
    let meta: serde_json::Value = row
        .try_get("meta")
        .unwrap_or_else(|_| serde_json::json!({}));
    let command_id = meta
        .get("command_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("{}:{}:{}", execution_id, step, event_id));

    // If command already reached terminal state, skip re-claim.
    let terminal_row = sqlx::query(
        r#"
        SELECT event_type
        FROM noetl.event
        WHERE execution_id = $1
          AND event_type IN ('command.completed', 'command.failed')
          AND (meta->>'command_id' = $2 OR result->'data'->>'command_id' = $2)
        ORDER BY event_id DESC
        LIMIT 1
        "#,
    )
    .bind(execution_id)
    .bind(&command_id)
    .fetch_optional(&mut *tx)
    .await?;

    if terminal_row.is_some() {
        return Err(AppError::Conflict(
            "Command already reached terminal state".to_string(),
        ));
    }

    // If execution already cancelled, reject claim.
    let cancelled_row = sqlx::query(
        r#"
        SELECT 1
        FROM noetl.event
        WHERE execution_id = $1
          AND event_type = 'execution.cancelled'
        LIMIT 1
        "#,
    )
    .bind(execution_id)
    .fetch_optional(&mut *tx)
    .await?;

    if cancelled_row.is_some() {
        return Err(AppError::Conflict(
            "Execution has been cancelled".to_string(),
        ));
    }

    // Acquire advisory transaction lock by command id.
    let lock_row =
        sqlx::query("SELECT pg_try_advisory_xact_lock(hashtext($1)::bigint) AS lock_acquired")
            .bind(&command_id)
            .fetch_one(&mut *tx)
            .await?;
    let lock_acquired: bool = lock_row.try_get("lock_acquired")?;
    if !lock_acquired {
        return Err(AppError::Conflict(
            "Command is being claimed by another worker".to_string(),
        ));
    }

    // Check if already claimed by another worker.
    let existing_claim = sqlx::query(
        r#"
        SELECT worker_id, meta
        FROM noetl.event
        WHERE execution_id = $1
          AND event_type = 'command.claimed'
          AND (meta->>'command_id' = $2 OR result->'data'->>'command_id' = $2)
        ORDER BY event_id DESC
        LIMIT 1
        "#,
    )
    .bind(execution_id)
    .bind(&command_id)
    .fetch_optional(&mut *tx)
    .await?;

    if let Some(existing) = existing_claim {
        let worker_id_db: Option<String> = existing.try_get("worker_id").ok();
        let worker_id_meta = existing
            .try_get::<serde_json::Value, _>("meta")
            .ok()
            .and_then(|value| {
                value
                    .get("worker_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            });
        let existing_worker = worker_id_db.or(worker_id_meta);

        if let Some(existing_worker_id) = existing_worker {
            if existing_worker_id != request.worker_id {
                return Err(AppError::Conflict(format!(
                    "Command already claimed by {}",
                    existing_worker_id
                )));
            }
            // Idempotent claim by same worker.
            tx.commit().await?;
            return Ok(Json(ClaimResponse {
                status: "ok".to_string(),
                event_id,
                execution_id,
                node_id: step.clone(),
                node_name: step,
                action: tool_kind,
                context,
                meta,
            }));
        }
    }

    let claim_event_id = state.snowflake.generate()?;
    // Constraint-compliant `{status, context}` envelope — see
    // noetl/server#29 for why `{kind, data}` was rejected.  The
    // explicit claim path was missed in v2.4.3 because the load
    // smoke only exercised handle_event; Phase D Round 2's
    // multi-step kind validation surfaced it.
    let claim_result = serde_json::json!({
        "status": "RUNNING",
        "context": {
            "command_id": command_id,
            "worker_id": request.worker_id,
        }
    });
    let claim_meta = serde_json::json!({
        "command_id": command_id,
        "worker_id": request.worker_id,
        "actionable": false,
        "informative": true,
    });

    // Suppress the `command.claimed` event for the worker-driven orchestrate
    // meta-command (noetl/ai-meta#108): it's infrastructure, not a workflow step,
    // so it must not write to noetl.event (the claim's own state lives in the tx
    // below). One drive at a time per execution is already guaranteed by the
    // `orchestrate_in_flight` guard + the single NATS dispatch, so skipping the
    // claimed-event idempotency anchor is safe here.
    // CQRS write-path chokepoint (#103 2d-3).  Gate-off: the command.claimed
    // event is written INSIDE the claim transaction exactly as before (atomic
    // with the noetl.command claim update).  Gate-on: the event is PUBLISHED
    // after the tx commits (the noetl.command claim stays in-tx — it's the queue
    // the worker reads), so it materializes like every other event and the
    // claim audit isn't lost.
    let publish_claim =
        crate::handlers::event_write::should_publish(&state, catalog_id.unwrap_or(0)).await;
    let mut claim_event_to_publish: Option<crate::handlers::event_write::EventRow> = None;
    if step != noetl_orchestrate_core::state::WorkflowState::ORCHESTRATE_META_STEP {
        if publish_claim {
            claim_event_to_publish = Some(
                crate::handlers::event_write::EventRow::new(
                    claim_event_id,
                    execution_id,
                    catalog_id.unwrap_or(0),
                    "command.claimed",
                    "RUNNING",
                    chrono::Utc::now(),
                )
                .with_node(step.clone())
                .with_result(claim_result.clone())
                .with_meta(claim_meta.clone())
                .with_worker_id(Some(request.worker_id.clone())),
            );
        } else {
            // One-level event chain (RFC #115 Phase 2): stamp `prev_event_id`
            // from the per-execution chain head + advance the head, EXACTLY as
            // the `event_write::emit_events` chokepoint does for every other
            // server-originated event.  This gate-off branch writes the
            // `command.claimed` row in-tx (atomic with the claim), so it never
            // passed through that chokepoint — leaving `prev_event_id = NULL`
            // AND the chain head un-advanced.  The orphan that produced
            // (noetl/ai-meta#121): the next worker event (`command.started`)
            // linked back to `command.issued`, skipping the unlinked
            // `command.claimed`, so the off-server `chain_walk_from` hit a
            // NULL-prev non-genesis head, reported the spine `Incomplete`, and
            // the server reconciler re-drove the execution in a loop.  Linking
            // here keeps the chain `command.issued → command.claimed →
            // command.started …` walkable so the off-server builder completes.
            // `link_batch` advances the head before the INSERT — the same
            // advance-then-write ordering `emit_events` already uses; a (rare)
            // commit failure leaves an advanced head re-derived on restart, no
            // worse than the chokepoint path.  Gate-on (publish) links via the
            // post-commit `emit_event` above, so this is only reached gate-off.
            let prev_event_id = state
                .chain_heads
                .link_batch(execution_id, &[claim_event_id])
                .await
                .into_iter()
                .next()
                .flatten();
            sqlx::query(
                r#"
                INSERT INTO noetl.event (
                    event_id, execution_id, catalog_id, event_type,
                    node_id, node_name, status, result, meta, worker_id,
                    prev_event_id, created_at
                ) VALUES (
                    $1, $2, $3, $4,
                    $5, $6, $7, $8, $9, $10,
                    $11, $12
                )
                "#,
            )
            .bind(claim_event_id)
            .bind(execution_id)
            .bind(catalog_id)
            .bind("command.claimed")
            .bind(&step)
            .bind(&step)
            .bind("RUNNING")
            .bind(claim_result)
            .bind(claim_meta)
            .bind(&request.worker_id)
            .bind(prev_event_id)
            .bind(chrono::Utc::now())
            .execute(&mut *tx)
            .await?;
        }
    }

    tx.commit().await?;

    if let Some(ev) = claim_event_to_publish {
        crate::handlers::event_write::emit_event(&state, state.pools.pool_for(execution_id), ev)
            .await?;
    }

    Ok(Json(ClaimResponse {
        status: "ok".to_string(),
        event_id,
        execution_id,
        node_id: step.clone(),
        node_name: step,
        action: tool_kind,
        context,
        meta,
    }))
}

/// Handle batched worker events.
///
/// POST /api/events/batch
pub async fn handle_batch_events(
    State(state): State<AppState>,
    Json(request): Json<BatchEventRequest>,
) -> Result<Json<BatchEventResponse>, AppError> {
    if request.events.is_empty() {
        return Ok(Json(BatchEventResponse {
            status: "ok".to_string(),
            event_ids: Vec::new(),
            commands_generated: 0,
        }));
    }

    let execution_id: i64 = request
        .execution_id
        .parse()
        .map_err(|_| AppError::Validation("Invalid execution_id".to_string()))?;

    let catalog_id = get_catalog_id(&state, execution_id, "get_catalog_id_batch").await?;
    // Phase F R4-3: batch writes land on this execution's shard.
    let mut tx = state.pools.pool_for(execution_id).begin().await?;
    let mut event_ids = Vec::with_capacity(request.events.len());

    // noetl/ai-meta#102 step 1 (worker side): a worker that batches a command's
    // lifecycle events (started / call.done / completed) into one POST lands them
    // as a single multi-row INSERT here — every event preserved (full per-item
    // granularity), but the N writes collapse to one round-trip.  Build all the
    // rows first, then one `QueryBuilder` multi-row INSERT (was: N individual
    // INSERTs in the txn loop).
    struct PreparedEvent {
        event_id: i64,
        event_type: String,
        step: String,
        status: String,
        result_obj: serde_json::Value,
        meta_obj: serde_json::Value,
    }
    let mut rows: Vec<PreparedEvent> = Vec::with_capacity(request.events.len());
    for item in &request.events {
        // Batch path uses the application-side snowflake generator (Phase F R1.5
        // of noetl/ai-meta#49).  Per-item app-side event_id isn't carried in
        // BatchEventItem yet; left as a follow-up.
        let event_id = state.snowflake.generate()?;
        let status = event_status_from_name(&item.event_type);

        // Constraint-compliant `{status, context}` envelope per noetl/server#29 —
        // `context` only when payload is an object; otherwise `{status}` alone.
        let mut result_map = serde_json::Map::new();
        result_map.insert(
            "status".to_string(),
            serde_json::Value::String(status.to_string()),
        );
        if let serde_json::Value::Object(_) = item.payload {
            result_map.insert("context".to_string(), item.payload.clone());
        }
        let result_obj = sanitize_sensitive_data(&serde_json::Value::Object(result_map));

        let mut meta_obj = serde_json::json!({
            "actionable": item.actionable,
            "informative": item.informative,
        });
        if let Some(worker_id) = &request.worker_id {
            if let serde_json::Value::Object(ref mut map) = meta_obj {
                map.insert("worker_id".to_string(), serde_json::json!(worker_id));
            }
        }
        let meta_obj = sanitize_sensitive_data(&meta_obj);

        event_ids.push(event_id);
        rows.push(PreparedEvent {
            event_id,
            event_type: item.event_type.clone(),
            step: item.step.clone(),
            status: status.to_string(),
            result_obj,
            meta_obj,
        });
    }

    let now = chrono::Utc::now();
    // CQRS write-path chokepoint (#103 2d-3).  Gate-off: in-tx multi-row INSERT
    // (byte-identical).  Gate-on: publish post-commit + the orchestrator trigger
    // relocates to the materializer endpoint.
    let publish_batch =
        crate::handlers::event_write::should_publish(&state, catalog_id.unwrap_or(0)).await;
    if publish_batch {
        let event_rows: Vec<crate::handlers::event_write::EventRow> = rows
            .iter()
            .map(|r| {
                crate::handlers::event_write::EventRow::new(
                    r.event_id,
                    execution_id,
                    catalog_id.unwrap_or(0),
                    r.event_type.clone(),
                    r.status.clone(),
                    now,
                )
                .with_node(r.step.clone())
                .with_result(r.result_obj.clone())
                .with_meta(r.meta_obj.clone())
                .with_worker_id(request.worker_id.clone())
            })
            .collect();
        // Commit the (possibly empty) claim tx first, then publish.
        tx.commit().await?;
        crate::handlers::event_write::emit_events(
            &state,
            state.pools.pool_for(execution_id),
            &event_rows,
        )
        .await?;
    } else {
        // Same one-level chain stamping the `event_write::emit_events`
        // chokepoint performs (noetl/ai-meta#121): this gate-off batch INSERT
        // bypasses it, so link each row's `prev_event_id` from the
        // per-execution chain head + advance the head here, in batch order, so
        // the gate-off chain stays walkable for the off-server builder.  Rows in
        // a batch share one execution.
        let ids: Vec<i64> = rows.iter().map(|r| r.event_id).collect();
        let prevs = state.chain_heads.link_batch(execution_id, &ids).await;
        let mut qb = sqlx::QueryBuilder::new(
            "INSERT INTO noetl.event (event_id, execution_id, catalog_id, event_type, \
             node_id, node_name, status, result, meta, worker_id, prev_event_id, created_at) ",
        );
        qb.push_values(rows.iter().zip(prevs.iter()), |mut b, (r, prev)| {
            b.push_bind(r.event_id)
                .push_bind(execution_id)
                .push_bind(catalog_id)
                .push_bind(&r.event_type)
                .push_bind(&r.step)
                .push_bind(&r.step)
                .push_bind(&r.status)
                .push_bind(&r.result_obj)
                .push_bind(&r.meta_obj)
                .push_bind(&request.worker_id)
                .push_bind(*prev)
                .push_bind(now);
        });
        qb.build().execute(&mut *tx).await?;
        tx.commit().await?;
    }

    // Trigger orchestrator for any command.completed in the batch,
    // including end (end is now a real dispatched step per
    // noetl/ai-meta#54 — its command.completed is the trigger that
    // makes check_completion emit playbook.completed).  Mirrors the
    // call site in `handle_event` above; runs once per qualifying
    // event so a batch with multiple completions can still advance
    // multi-step playbooks.  Errors are logged and swallowed so a
    // bad-state evaluation doesn't fail the whole batch ingest.
    // Gate-on (publish_batch): skip — the trigger relocates to the materializer's
    // write endpoint (events_project), which fires it after the row materializes.
    for (idx, item) in request.events.iter().enumerate() {
        if item.event_type == "command.completed" && !publish_batch {
            let trigger_event_id = event_ids[idx];
            match trigger_orchestrator(&state, execution_id, trigger_event_id).await {
                Ok(cmds) => {
                    info!(
                        "Orchestrator (batch) generated {} commands for execution {} step {}",
                        cmds, execution_id, item.step
                    );
                }
                Err(e) => {
                    warn!(
                        "Orchestrator error in batch for execution {} step {}: {}",
                        execution_id, item.step, e
                    );
                }
            }
        }
    }

    Ok(Json(BatchEventResponse {
        status: "ok".to_string(),
        event_ids,
        commands_generated: 0,
    }))
}

/// Extract command_id from request.
fn get_command_id(request: &EventRequest) -> Option<String> {
    // Try payload first
    if let Some(id) = request.payload.get("command_id").and_then(|v| v.as_str()) {
        return Some(id.to_string());
    }
    // Try meta
    if let Some(meta) = &request.meta {
        if let Some(id) = meta.get("command_id").and_then(|v| v.as_str()) {
            return Some(id.to_string());
        }
    }
    None
}

/// Check if command is already claimed.
async fn check_already_claimed(
    state: &AppState,
    execution_id: i64,
    command_id: &str,
    worker_id: &Option<String>,
) -> AppResult<bool> {
    let row: Option<(Option<String>, Option<serde_json::Value>)> =
        sqlx::query_as::<_, (Option<String>, Option<serde_json::Value>)>(
            r#"
            SELECT worker_id, meta FROM noetl.event
            WHERE execution_id = $1
              AND event_type = 'command.claimed'
              AND (meta->>'command_id' = $2 OR result->'data'->>'command_id' = $2)
            LIMIT 1
            "#,
        )
        .bind(execution_id)
        .bind(command_id)
        .fetch_optional(state.pools.pool_for(execution_id))
        .await?;

    if let Some((existing_worker, meta)) = row {
        let existing_worker_id = existing_worker.or_else(|| {
            meta.and_then(|m| {
                m.get("worker_id")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
        });

        if let (Some(existing), Some(current)) = (&existing_worker_id, worker_id) {
            if existing != current {
                // Different worker - reject
                return Err(AppError::Conflict(format!(
                    "Command already claimed by {}",
                    existing
                )));
            }
            // Same worker - idempotent
            return Ok(true);
        }
    }

    Ok(false)
}

/// Build the `result` JSONB envelope for a `noetl.event` row.
///
/// Shape is constrained at the DB level by
/// `chk_event_result_shape`: top-level keys are limited to
/// `status` (required string), `reference` (optional object),
/// `context` (optional object).  Anything else fails the
/// constraint.  See noetl/server#29 for the history — the
/// previous `{kind, data}` / `{kind, store_tier, logical_uri}` /
/// `{kind, event_ids, total_parts}` envelopes all violated the
/// constraint and caused every POST /api/events that reached
/// the INSERT to 500.
///
/// Mapping:
/// - `result_kind = "ref"`  + `result_uri` set
///   → `{status, reference: {store_tier, logical_uri}}`
/// - `result_kind = "refs"` + `event_ids` set
///   → `{status, reference: {event_ids, total_parts}}`
/// - default (`"data"` or unknown):
///     - `payload` is a non-null object → `{status, context: <payload>}`
///     - `payload` is null/non-object   → `{status}`
fn build_result_object(request: &EventRequest, status: &str) -> serde_json::Value {
    let mut result = serde_json::Map::new();
    result.insert(
        "status".to_string(),
        serde_json::Value::String(status.to_string()),
    );

    match request.result_kind.as_str() {
        "ref" if request.result_uri.is_some() => {
            let uri = request.result_uri.as_ref().unwrap();
            let store_tier = if uri.starts_with("gs://") {
                "gcs"
            } else if uri.starts_with("s3://") {
                "s3"
            } else {
                "artifact"
            };
            result.insert(
                "reference".to_string(),
                serde_json::json!({
                    "store_tier": store_tier,
                    "logical_uri": uri,
                }),
            );
        }
        "refs" if request.event_ids.is_some() => {
            let event_ids = request.event_ids.as_ref().unwrap();
            result.insert(
                "reference".to_string(),
                serde_json::json!({
                    "event_ids": event_ids,
                    "total_parts": event_ids.len(),
                }),
            );
        }
        _ => {
            // Constraint requires `context` (when present) to be
            // an object.  Wire-format payload may be a primitive
            // for some legacy clients — skip the key entirely in
            // that case rather than corrupting the row.
            if let serde_json::Value::Object(_) = request.payload {
                result.insert("context".to_string(), request.payload.clone());
            }
        }
    }

    serde_json::Value::Object(result)
}

/// Get catalog_id for an execution.
///
/// Reads `noetl.event` first (authoritative when present), then falls back to
/// `noetl.command`.  The fallback is load-bearing under the CQRS write-path
/// cutover (noetl/ai-meta#103 2d-3): with `NOETL_EVENT_INGEST_PUBLISH_ONLY` on,
/// `noetl.event` is EMPTY (events are published to `noetl_events` and not
/// INSERTed until the materializer drains them), so a worker-emitted event's
/// `normalize_event_to_row` would resolve `catalog_id = None → 0` and the
/// published row would carry `catalog_id = 0`, FK-violating
/// `event_catalog_id_fkey` when the materializer INSERTs it (→ the whole batch
/// fails + the events are lost).  `noetl.command` is written synchronously even
/// under the gate (it's the command queue the worker reads), so it always
/// carries the execution's `catalog_id`.
async fn get_catalog_id(state: &AppState, execution_id: i64, site: &str) -> AppResult<Option<i64>> {
    // RFC #115 Phase 6: under `event_read_path=audit_only`, serve catalog_id from
    // the in-memory execute-time descriptor (seeded at `playbook_started`, before
    // the first event emit) — ZERO `noetl.event` read on this per-ingest hot path.
    // A cold descriptor (server restart mid-execution) falls through to the scan
    // below (counted `scan`), so correctness never regresses.
    let pool = state.pools.pool_for(execution_id);
    if matches!(
        state.config.event_read_path,
        crate::config::EventReadPath::AuditOnly
    ) {
        // Warm descriptor → served, ZERO read.
        if let Some(desc) = state.exec_descriptors.get(execution_id).await {
            if desc.catalog_id != 0 {
                crate::metrics::record_event_hotpath_read(site, "served_descriptor");
                return Ok(Some(desc.catalog_id));
            }
        }
        // Cold descriptor (a post-terminal straggler after the descriptor was
        // evicted on terminal, or a server restart mid-execution): resolve
        // catalog_id from `noetl.command` — the **synchronous** command queue
        // (written for every command regardless of the publish-only gate, the
        // queue the worker reads) — WITHOUT scanning `noetl.event`.  So
        // get_catalog_id never reads `noetl.event` under audit_only.  We do NOT
        // re-seed the descriptor here: re-seeding an already-evicted terminal
        // execution would re-accumulate exactly the per-execution memory the
        // terminal eviction frees (the unbounded growth this RFC removes).
        crate::metrics::record_event_hotpath_read(site, "served_command");
        let row: Option<(i64,)> = sqlx::query_as::<_, (i64,)>(
            "SELECT catalog_id FROM noetl.command WHERE execution_id = $1 LIMIT 1",
        )
        .bind(execution_id)
        .fetch_optional(pool)
        .await?;
        return Ok(row.map(|(id,)| id));
    }
    crate::metrics::record_event_hotpath_read(site, "scan");

    // event_scan (default/prod): unchanged — noetl.event authoritative when
    // present, noetl.command the fallback under the publish-only gate.
    if let Some((id,)) = sqlx::query_as::<_, (i64,)>(
        "SELECT catalog_id FROM noetl.event WHERE execution_id = $1 LIMIT 1",
    )
    .bind(execution_id)
    .fetch_optional(pool)
    .await?
    {
        return Ok(Some(id));
    }
    let row: Option<(i64,)> = sqlx::query_as::<_, (i64,)>(
        "SELECT catalog_id FROM noetl.command WHERE execution_id = $1 LIMIT 1",
    )
    .bind(execution_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(id,)| id))
}

/// Map event name to status.
fn event_status_from_name(event_name: &str) -> &'static str {
    if event_name.contains("done")
        || event_name.contains("exit")
        || event_name.contains("completed")
    {
        "COMPLETED"
    } else if event_name.contains("error") || event_name.contains("failed") {
        "FAILED"
    } else {
        "RUNNING"
    }
}

/// Trigger orchestrator for workflow progression.
///
/// Phase D Round 2 of noetl/ai-meta#49 — wires the previously-
/// stubbed orchestrator to the real
/// [`crate::engine::WorkflowOrchestrator::evaluate`] pipeline.
///
/// Pipeline:
///
/// 1. Load all `noetl.event` rows for this execution (sorted by
///    `event_id` so [`WorkflowState::from_events`] reconstructs
///    state in the canonical order).
/// 2. Resolve `catalog_id` from one of the events, load the
///    playbook YAML, parse to [`Playbook`].
/// 3. Call `orchestrator.evaluate(&events, &playbook,
///    Some("command.completed"))`.
/// 4. For each [`EventToEmit`] returned (e.g. `step.enter` rows
///    for the next step), insert into `noetl.event`.
/// 5. For each [`engine::Command`] returned, look up the matching
///    [`Step`] in the playbook and call
///    [`crate::handlers::execute::persist_engine_command`] —
///    which inserts the `command.issued` event, the
///    `noetl.command` row, and publishes the NATS notification
///    (same code path the `/api/execute` first-command uses, so
///    the wire format stays consistent).
/// 6. If `result.should_complete` is set, emit a final
///    `playbook.completed` or `playbook.failed` event so
///    downstream consumers can observe terminal state.
///
/// `trigger_event_id` is the `event_id` of the event that
/// triggered this evaluation pass (usually the `command.completed`
/// row).  It's used as the `parent_event_id` for newly-inserted
/// events so the event log forms a proper causal chain.
/// Merge the locator accessors (`_ref`, `_store`, `_uri`) from a kept
/// `reference` block into the bounded `extracted` summary, so the flat step
/// binding (`{{ step._ref }}`, `{{ step._store }}`) carries them without the
/// bulk payload.  Only injects when `extracted` is a JSON object and the keys
/// aren't already present (the worker's own inline `{data:{_ref}}` shape wins).
/// A non-object `extracted` (scalar / array summary) is returned untouched.
fn with_ref_accessors(
    mut extracted: serde_json::Value,
    ref_block: Option<&serde_json::Value>,
) -> serde_json::Value {
    let (Some(obj), Some(reference)) = (extracted.as_object_mut(), ref_block) else {
        return extracted;
    };
    for (accessor, field) in [("_ref", "ref"), ("_store", "store"), ("_uri", "uri")] {
        if obj.contains_key(accessor) {
            continue;
        }
        if let Some(v) = reference.get(field) {
            obj.insert(accessor.to_string(), v.clone());
        }
    }
    extracted
}

/// Resolve any `{status, reference: {ref: "noetl://..."}}` results in `events`
/// back to the inline `{status, context: <data>}` shape the orchestrator reads.
///
/// Results-by-reference: the worker stages an over-budget tool result in the
/// result store and emits only a small reference on the event (keeping the
/// event log lean).  The orchestrator needs the data to evaluate `output.*`
/// guards / `set:` and to dispatch cursor bodies, so it rehydrates the
/// reference here — only for the events it's about to apply, not the whole log.
/// Inline results (no `reference`) and unresolvable refs are left untouched.
/// Resolve over-budget result references inline before the orchestrator
/// applies the events.
///
/// The worker stages results larger than its inline budget in the durable
/// result store and emits a `{data: {_ref}}` placeholder plus a sibling
/// `reference` block instead of the data (see
/// `repos/worker/src/executor/command.rs` `build_reference_or_inline`).  The
/// orchestrator's state machine (`extract_user_data`, the cursor drive,
/// `build_context`) reads the actual data out of the event — a placeholder
/// makes a cursor claim look like it returned zero rows, which stalls the
/// loop.  So before `from_events` / `evaluate_state` see the events, we swap
/// each placeholder for the resolved data.
///
/// The reference sits at one of two paths depending on how the `call.done`
/// envelope wrapped the tool result. Nested (the standard envelope) puts the
/// tool result under `context.result`, so the reference is at
/// `result.context.result.reference` and the placeholder at
/// `result.context.result.context.data._ref`. Top-level (an un-wrapped tool
/// result) puts the reference at `result.reference` and the placeholder at
/// `result.context.data._ref`.
///
/// In both cases `reference.ref` is the `noetl://` URI and the sibling
/// `context` is replaced with the resolved store payload (`{data: {...}}`),
/// then the `reference` block is dropped so the result reads like an inline
/// one.
async fn hydrate_result_references(
    events: &mut [crate::db::models::Event],
    result_store: &crate::services::result_store::ResultStoreService,
    keep_refs: bool,
) {
    use crate::services::result_store::parse_noetl_ref;

    enum Shape {
        /// reference at `result.context.result.reference`
        Nested,
        /// reference at `result.reference`
        TopLevel,
    }

    let mut hydrated = 0usize;
    let mut kept = 0usize;
    for ev in events.iter_mut() {
        let Some(result) = ev.result.as_mut() else {
            continue;
        };
        // Locate the reference URI + remember which envelope shape carried it.
        let (shape, ref_uri) = if let Some(u) = result
            .pointer("/context/result/reference/ref")
            .and_then(|v| v.as_str())
        {
            (Shape::Nested, u.to_string())
        } else if let Some(u) = result.pointer("/reference/ref").and_then(|v| v.as_str()) {
            (Shape::TopLevel, u.to_string())
        } else {
            continue;
        };

        // References-in-state (noetl/ai-meta#101 phase 2): when gated on AND the
        // reference carries an `extracted` predicate block (phase 1), keep the
        // reference and surface `extracted` as the readable `context.data` —
        // instead of resolving the full payload inline.  `extract_user_data`
        // then reads `extracted` for `when:`/`set:` evaluation; the reference
        // block stays on the event so `build_context` carries it and the worker
        // resolves the bulk payload at render time.  No `extracted` (pre-phase-1
        // refs) falls through to the resolve path below for back-compat.
        if keep_refs {
            let (extracted, ref_block) = match shape {
                Shape::Nested => (
                    result
                        .pointer("/context/result/reference/extracted")
                        .cloned(),
                    result.pointer("/context/result/reference").cloned(),
                ),
                Shape::TopLevel => (
                    result.pointer("/reference/extracted").cloned(),
                    result.pointer("/reference").cloned(),
                ),
            };
            if let Some(extracted) = extracted {
                // Surface the locator accessors (`_ref`, `_store`, `_uri`) on the
                // kept summary so reference-only consumers — `{{ step._ref }}`
                // (artifact.get lazy-load), `{{ step._ref is defined }}` /
                // `{{ step._store }}` (storage-tier predicates) — resolve off the
                // bounded block without pulling the bulk payload (the consume
                // side of noetl/ai-meta#115 Phase 1). The bulk stays in the store
                // behind `reference.ref`; the worker resolves it only for inputs
                // that bind the bulk (see worker `resolve_context_references`).
                let data = serde_json::json!({ "data": with_ref_accessors(extracted, ref_block.as_ref()) });
                match shape {
                    Shape::Nested => {
                        if let Some(inner) = result
                            .get_mut("context")
                            .and_then(|c| c.get_mut("result"))
                            .and_then(|r| r.as_object_mut())
                        {
                            inner.insert("context".to_string(), data);
                            kept += 1;
                        }
                    }
                    Shape::TopLevel => {
                        if let Some(obj) = result.as_object_mut() {
                            obj.insert("context".to_string(), data);
                            kept += 1;
                        }
                    }
                }
                continue; // keep the reference; do not resolve
            }
        }

        let parsed = match parse_noetl_ref(&ref_uri) {
            Ok(p) => p,
            Err(e) => {
                warn!(execution_id = ev.execution_id, ref_uri, %e, "unparseable result reference; left as-is");
                continue;
            }
        };
        let resolved = match result_store.resolve(&parsed).await {
            Ok(Some(data)) => data,
            Ok(None) => {
                warn!(execution_id = ev.execution_id, ref_uri, "result reference not found in store; left as-is");
                continue;
            }
            Err(e) => {
                warn!(execution_id = ev.execution_id, ref_uri, %e, "result reference resolution failed; left as-is");
                continue;
            }
        };

        // Splice the resolved payload over the `{data: {_ref}}` placeholder
        // and drop the `reference` block so the event reads as inline.
        match shape {
            Shape::Nested => {
                if let Some(inner) = result
                    .get_mut("context")
                    .and_then(|c| c.get_mut("result"))
                    .and_then(|r| r.as_object_mut())
                {
                    inner.insert("context".to_string(), resolved);
                    inner.remove("reference");
                    hydrated += 1;
                }
            }
            Shape::TopLevel => {
                if let Some(obj) = result.as_object_mut() {
                    obj.insert("context".to_string(), resolved);
                    obj.remove("reference");
                    hydrated += 1;
                }
            }
        }
    }
    if hydrated > 0 || kept > 0 {
        debug!(
            hydrated,
            kept, "hydrate_result_references: resolved / kept-by-reference over-budget results"
        );
    }
}

/// Columns the orchestrator reads from `noetl.event`, in the order
/// [`parse_event_rows`] expects.
const ORCH_EVENT_COLS: &str = r#"
        SELECT event_id, execution_id, catalog_id,
               parent_event_id, parent_execution_id,
               event_type, node_id, node_name, node_type, status,
               context, meta, result, worker_id,
               NULLIF(meta->>'attempt', '')::int AS attempt,
               created_at
        FROM noetl.event "#;

/// Same projection as [`ORCH_EVENT_COLS`] plus the `prev_event_id` chain link —
/// the column the chain-walk state builder (RFC noetl/ai-meta#115 Phase 3) follows
/// one level at a time.  Only used by [`fetch_chain_node`], which always pins
/// `WHERE execution_id = $1 AND event_id = $2` (the `(execution_id, event_id)` PK),
/// so this never drives a `WHERE execution_id`-only scan.
const ORCH_EVENT_COLS_WITH_PREV: &str = r#"
        SELECT event_id, execution_id, catalog_id,
               parent_event_id, parent_execution_id,
               event_type, node_id, node_name, node_type, status,
               context, meta, result, worker_id,
               NULLIF(meta->>'attempt', '')::int AS attempt,
               created_at, prev_event_id
        FROM noetl.event "#;

/// Map rows selected via [`ORCH_EVENT_COLS`] into `Event`s.  `attempt` lives in
/// `meta` JSONB (no dedicated column), sourced via the `NULLIF(...)::int` alias.
fn parse_event_rows(rows: Vec<sqlx::postgres::PgRow>) -> Vec<crate::db::models::Event> {
    use sqlx::Row;
    rows.into_iter()
        .map(|r| crate::db::models::Event {
            id: r.try_get("event_id").unwrap_or(0),
            execution_id: r.try_get("execution_id").unwrap_or(0),
            catalog_id: r.try_get("catalog_id").unwrap_or(0),
            event_id: r.try_get("event_id").unwrap_or(0),
            parent_event_id: r.try_get("parent_event_id").ok(),
            parent_execution_id: r.try_get("parent_execution_id").ok(),
            event_type: r.try_get("event_type").unwrap_or_default(),
            node_id: r.try_get("node_id").ok(),
            node_name: r.try_get("node_name").ok(),
            node_type: r.try_get("node_type").ok(),
            status: r.try_get("status").unwrap_or_default(),
            context: r.try_get("context").ok(),
            meta: r.try_get("meta").ok(),
            result: r.try_get("result").ok(),
            worker_id: r.try_get("worker_id").ok(),
            attempt: r.try_get("attempt").ok(),
            created_at: r
                .try_get("created_at")
                .unwrap_or_else(|_| chrono::Utc::now()),
        })
        .collect()
}

/// Outcome of a bounded state rebuild (noetl/ai-meta#101 block b).  The caller
/// sets `applied_count = total` (the live event count) after a rebuild: a
/// rebuild folds in every event after the snapshot, so the state reflects all
/// events; any straggler the window still missed is caught by the next
/// trigger's count mismatch.
struct RebuildResult {
    state: crate::engine::state::WorkflowState,
    last_event_id: i64,
    snapshot_version: i64,
    routing_meta: Option<serde_json::Value>,
}

/// Margin (seconds) the snapshot rebuild re-scans behind the snapshot's write
/// time.  A straggler that lands below the snapshot `version` *after* the
/// snapshot is written carries a recent-ish `created_at` (the column is the
/// event's emit time, not its snowflake id), so loading events with
/// `created_at > snapshot.updated_at - MARGIN` catches it.  Covers worker
/// clock skew + emit-to-insert latency; re-applying the overlap is safe because
/// cursor counters are gated by the snapshot's `cursor_issued`/`cursor_completed`
/// id-sets.  Bounded small: machine-id interleaving makes a straggler's
/// `created_at` only milliseconds off; the margin only needs to cover
/// emit-to-insert latency + worker clock skew (NTP keeps this sub-second).
const REBUILD_STRAGGLER_MARGIN_SECS: i64 = 30;

/// Rebuild orchestrator state with a **bounded** event load: from the latest
/// `projection_snapshot` + the events after it (by id, plus a `created_at`
/// margin window to catch below-watermark stragglers), or — when no snapshot
/// exists yet — the full (still-small) early log.  This replaced the unbounded
/// full-log replay that OOM'd the server at scale: a straggler below the
/// incremental watermark used to trigger a reload of the entire (growing) event
/// log on nearly every completion under high concurrency.
/// Resolve over-budget cursor claim references into inline `claim_rows` before
/// the drive runs (references-in-state, noetl/ai-meta#101 phase 2).  A referenced
/// claim has `claim_ref` set + empty `claim_rows` (the sync `apply_event` can't
/// resolve); this async pass fills the rows so the cursor fans out instead of
/// wrongly draining.  No-op unless the flag kept a claim reference.
async fn resolve_cursor_claim_refs(
    ws: &mut crate::engine::state::WorkflowState,
    result_store: &crate::services::result_store::ResultStoreService,
) {
    use crate::services::result_store::parse_noetl_ref;
    for step in ws.steps.values_mut() {
        if !step.is_cursor {
            continue;
        }
        for frame in step.cursor_frames.values_mut() {
            if !frame.claim_rows.is_empty() {
                continue;
            }
            let Some(uri) = frame.claim_ref.clone() else {
                continue;
            };
            let parsed = match parse_noetl_ref(&uri) {
                Ok(p) => p,
                Err(e) => {
                    warn!(uri, %e, "cursor claim reference unparseable; left as drain");
                    continue;
                }
            };
            match result_store.resolve(&parsed).await {
                Ok(Some(data)) => match extract_claim_rows(&data) {
                    Some(rows) => {
                        frame.claim_rows = rows;
                        frame.claim_ref = None;
                    }
                    None => warn!(uri, "cursor claim reference resolved but no rows array found"),
                },
                Ok(None) => warn!(uri, "cursor claim reference not found in store"),
                Err(e) => warn!(uri, %e, "cursor claim reference resolve failed"),
            }
        }
    }
}

/// Pull the claimed rows array out of a resolved claim payload, trying the
/// common shapes the store may hold for a tool result.
fn extract_claim_rows(data: &serde_json::Value) -> Option<Vec<serde_json::Value>> {
    for ptr in ["/rows", "/data/rows", "/result/rows", "/context/data/rows"] {
        if let Some(arr) = data.pointer(ptr).and_then(|v| v.as_array()) {
            return Some(arr.clone());
        }
    }
    crate::engine::state::extract_user_data(data)
        .and_then(|d| d.get("rows").and_then(|r| r.as_array()).cloned())
}

async fn rebuild_state(
    pool: &crate::db::DbPool,
    result_store: &crate::services::result_store::ResultStoreService,
    execution_id: i64,
    keep_refs: bool,
) -> AppResult<RebuildResult> {
    use crate::services::orch_snapshot;
    match orch_snapshot::load_latest(pool, execution_id).await? {
        Some(snap) => {
            // Events after the snapshot: newer by id, OR (straggler) below the
            // version watermark but emitted within the margin of the snapshot.
            let margin_floor =
                snap.updated_at - chrono::Duration::seconds(REBUILD_STRAGGLER_MARGIN_SECS);
            let mut events_since = parse_event_rows(
                sqlx::query(&format!(
                    "{ORCH_EVENT_COLS} WHERE execution_id = $1 \
                       AND (event_id > $2 OR created_at > $3) ORDER BY event_id ASC"
                ))
                .bind(execution_id)
                .bind(snap.version)
                .bind(margin_floor)
                .fetch_all(pool)
                .await?,
            );
            hydrate_result_references(&mut events_since, result_store, keep_refs).await;
            let mut ws = snap.state;
            for e in &events_since {
                ws.apply_event(&e.into());
            }
            // The window includes everything after the snapshot, so the highest
            // loaded id is the current head.
            let last_event_id = events_since
                .iter()
                .map(|e| e.event_id)
                .max()
                .unwrap_or(snap.version);
            Ok(RebuildResult {
                state: ws,
                last_event_id,
                snapshot_version: snap.version,
                routing_meta: snap.routing_meta,
            })
        }
        None => {
            let mut all_events = parse_event_rows(
                sqlx::query(&format!(
                    "{ORCH_EVENT_COLS} WHERE execution_id = $1 ORDER BY event_id ASC"
                ))
                .bind(execution_id)
                .fetch_all(pool)
                .await?,
            );
            hydrate_result_references(&mut all_events, result_store, keep_refs).await;
            let state = crate::engine::state::WorkflowState::from_events(&all_events.iter().map(Into::into).collect::<Vec<_>>())
                .ok_or_else(|| AppError::Validation("No events found for execution".to_string()))?;
            let last_event_id = all_events.last().map(|e| e.event_id).unwrap_or(0);
            let routing_meta = all_events
                .iter()
                .find(|e| e.event_type == "playbook_started")
                .and_then(|e| e.meta.clone());
            Ok(RebuildResult {
                state,
                last_event_id,
                snapshot_version: 0,
                routing_meta,
            })
        }
    }
}

// ── Chain-walk state builder (RFC noetl/ai-meta#115 Phase 3) ─────────────────
//
// Reconstructs `WorkflowState` by following the one-level `prev_event_id` chain
// (Phase 2) from the in-memory chain head back to the genesis event, instead of
// the `event_scan` `rebuild_state` path's `WHERE execution_id = $1 …` scans.
// Each hop is a `(execution_id, event_id)` PK point-lookup (`fetch_chain_node`);
// the collected events are sorted by `event_id` and fed to the SAME
// `WorkflowState::from_events`, so the built state is equivalent to the
// event-scan build (parity by construction).  The whole thing is gated behind
// `NOETL_STATE_BUILD_MODE=chain_walk` and falls back to event-scan on any
// completeness doubt (cold head, missing node under materializer lag, a chain
// that doesn't reach the genesis), so correctness is never sacrificed.

/// One node fetched during a chain walk: the parsed `Event` plus its
/// `prev_event_id` link (the next hop backward; `None` at the chain root).
struct ChainNode {
    event: crate::db::models::Event,
    prev_event_id: Option<i64>,
}

/// Fetch a single chain node by its `(execution_id, event_id)` primary key — the
/// only query shape the walk issues.  This is a **point lookup on the PK**, never
/// a `WHERE execution_id`-only scan.  Returns `None` when the row isn't present
/// (e.g. the event was published but not yet materialized under the publish-only
/// gate, or a dangling link) so the caller can fall back rather than build a
/// partial state.
async fn fetch_chain_node(
    pool: &crate::db::DbPool,
    execution_id: i64,
    event_id: i64,
) -> AppResult<Option<ChainNode>> {
    use sqlx::Row;
    let row = sqlx::query(&format!(
        "{ORCH_EVENT_COLS_WITH_PREV} WHERE execution_id = $1 AND event_id = $2"
    ))
    .bind(execution_id)
    .bind(event_id)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    // Read the link before the row is consumed by `parse_event_rows`.
    let prev_event_id: Option<i64> = row.try_get("prev_event_id").ok().flatten();
    let event = parse_event_rows(vec![row])
        .into_iter()
        .next()
        .expect("one row in, one event out");
    Ok(Some(ChainNode {
        event,
        prev_event_id,
    }))
}

/// The genesis (first) event type of an execution — the chain root for a
/// complete chain.  `playbook_started` is emitted first by `execute` (before any
/// `command.issued`), and the event-scan `rebuild_state` already keys
/// `routing_meta` off it, so its presence in the collected set means the walk
/// reached the true start (not just a post-restart tail).
fn chain_has_genesis(events: &[crate::db::models::Event]) -> bool {
    events.iter().any(|e| e.event_type == "playbook_started")
}

/// A successful chain-walk build: the same [`RebuildResult`] the event-scan path
/// produces, plus the per-trigger drive inputs the caller would otherwise read
/// off the event-scan window (`trigger_event_type`, `latest_ts`) and the count of
/// events the walk collected.
struct ChainWalkBuild {
    result: RebuildResult,
    collected: usize,
    trigger_event_type: String,
    latest_ts: Option<chrono::DateTime<chrono::Utc>>,
}

/// Build `WorkflowState` for `execution_id` by walking the `prev_event_id` chain
/// head→root (RFC #115 Phase 3).  Returns `None` (caller falls back to
/// event-scan) when the chain can't be trusted to be complete:
/// - the chain head is unknown (cold cache / restart / handled on another
///   replica) — `fallback_cold_head`;
/// - a walked node isn't present yet (materializer lag under the gate, dangling
///   link) — `fallback_node_missing`;
/// - the collected chain doesn't contain the genesis `playbook_started`
///   (restart-spanning tail) — `fallback_non_genesis`;
/// - the walk collected nothing — `fallback_empty`.
///
/// On success the collected events are sorted by `event_id` (the same order the
/// event-scan path applies them) and handed to the SAME `from_events`, so the
/// built state matches the event-scan build for the same execution.
async fn rebuild_state_chain_walk(
    state: &AppState,
    pool: &crate::db::DbPool,
    result_store: &crate::services::result_store::ResultStoreService,
    execution_id: i64,
    trigger_event_id: i64,
    keep_refs: bool,
) -> AppResult<Option<ChainWalkBuild>> {
    // Head from the in-memory watermark — no DB read.  Cold slot → fall back.
    let Some(head) = state.chain_heads.head(execution_id).await else {
        crate::metrics::record_state_build("chain_walk", "fallback_cold_head");
        debug!(execution_id, "chain walk: cold head, falling back to event scan");
        return Ok(None);
    };

    // Walk backward head→root by PK lookup, collecting nodes.  Bounded so a
    // corrupt cycle can't spin (real chains are at most a few thousand events).
    const MAX_WALK: usize = 5_000_000;
    let mut events: Vec<crate::db::models::Event> = Vec::new();
    let mut cursor = Some(head);
    let mut guard = 0usize;
    while let Some(eid) = cursor {
        guard += 1;
        if guard > MAX_WALK {
            warn!(
                execution_id,
                head, "chain walk exceeded bound; falling back to event scan"
            );
            crate::metrics::record_state_build("chain_walk", "fallback_node_missing");
            return Ok(None);
        }
        let Some(node) = fetch_chain_node(pool, execution_id, eid).await? else {
            // Not yet present (materializer lag) or a dangling link: can't build
            // a complete state — fall back this trigger; the next trigger retries
            // once the node materializes.
            crate::metrics::record_state_build("chain_walk", "fallback_node_missing");
            debug!(
                execution_id,
                missing_event_id = eid,
                "chain walk: node not present (lag/dangling), falling back to event scan"
            );
            return Ok(None);
        };
        cursor = node.prev_event_id;
        events.push(node.event);
    }

    if events.is_empty() {
        crate::metrics::record_state_build("chain_walk", "fallback_empty");
        return Ok(None);
    }
    // Completeness guard: the walk must have reached the genesis event.  Without
    // it the in-memory head only covered a post-restart tail and the built state
    // would miss earlier events.
    if !chain_has_genesis(&events) {
        crate::metrics::record_state_build("chain_walk", "fallback_non_genesis");
        debug!(
            execution_id,
            collected = events.len(),
            "chain walk: root is not the genesis (restart-spanning tail), falling back"
        );
        return Ok(None);
    }

    // Apply in the SAME order the event-scan path uses (`event_id ASC`): the walk
    // collected head→root (descending), so sort ascending → from_events sees the
    // identical sequence → identical state.
    events.sort_by_key(|e| e.event_id);
    crate::metrics::record_state_build_chain_hops(events.len());
    hydrate_result_references(&mut events, result_store, keep_refs).await;

    let ws = crate::engine::state::WorkflowState::from_events(
        &events.iter().map(Into::into).collect::<Vec<_>>(),
    )
    .ok_or_else(|| AppError::Validation("chain walk: from_events produced no state".to_string()))?;
    let last_event_id = events.iter().map(|e| e.event_id).max().unwrap_or(head);
    let routing_meta = events
        .iter()
        .find(|e| e.event_type == "playbook_started")
        .and_then(|e| e.meta.clone());
    let trigger_event_type = events
        .iter()
        .find(|e| e.event_id == trigger_event_id)
        .map(|e| e.event_type.clone())
        .unwrap_or_else(|| "command.completed".to_string());
    let latest_ts = events.iter().map(|e| e.created_at).max();

    crate::metrics::record_state_build("chain_walk", "ok");
    Ok(Some(ChainWalkBuild {
        result: RebuildResult {
            state: ws,
            last_event_id,
            snapshot_version: 0,
            routing_meta,
        },
        collected: events.len(),
        trigger_event_type,
        latest_ts,
    }))
}

/// Run the shadow parity check inside ONE `REPEATABLE READ` transaction so both
/// the chain walk and the bounded event scan observe a single consistent MVCC
/// snapshot of `noetl.event` (RFC #115 Phase 3 parity proof).  This removes the
/// race against concurrent materialization — under the publish-only gate the
/// chain head runs ahead of the materialized table and worker/server `event_id`s
/// interleave, so two un-isolated reads can legitimately see different sets even
/// for an identical execution.  Inside one snapshot the two builds collect the
/// SAME events; any remaining difference is a real builder bug (logged by
/// [`parity_check_states`] with the differing state keys).
///
/// Falls through (records nothing) when the chain head is cold or a chain node
/// isn't present in the snapshot — those are the same conditions the live builder
/// falls back on, not parity failures.
async fn run_parity_check(
    state: &AppState,
    pool: &crate::db::DbPool,
    result_store: &crate::services::result_store::ResultStoreService,
    execution_id: i64,
    keep_refs: bool,
) -> AppResult<()> {
    use sqlx::Row;
    let Some(head) = state.chain_heads.head(execution_id).await else {
        return Ok(());
    };
    let mut conn = pool.acquire().await?;
    sqlx::query("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *conn)
        .await?;

    // (1) Walk the chain head→root on the snapshot, collecting nodes by PK.
    let mut walk_events: Vec<crate::db::models::Event> = Vec::new();
    let mut cursor = Some(head);
    let mut complete = true;
    while let Some(eid) = cursor {
        let row = sqlx::query(&format!(
            "{ORCH_EVENT_COLS_WITH_PREV} WHERE execution_id = $1 AND event_id = $2"
        ))
        .bind(execution_id)
        .bind(eid)
        .fetch_optional(&mut *conn)
        .await?;
        let Some(row) = row else {
            complete = false;
            break;
        };
        let prev: Option<i64> = row.try_get("prev_event_id").ok().flatten();
        let ev = parse_event_rows(vec![row]).into_iter().next().expect("one row");
        cursor = prev;
        walk_events.push(ev);
    }

    // (2) Bounded scan of the same snapshot up to the walk's head.
    let max_id = walk_events.iter().map(|e| e.event_id).max();
    let scan_rows = if let Some(max_id) = max_id {
        sqlx::query(&format!(
            "{ORCH_EVENT_COLS} WHERE execution_id = $1 AND event_id <= $2 ORDER BY event_id ASC"
        ))
        .bind(execution_id)
        .bind(max_id)
        .fetch_all(&mut *conn)
        .await?
    } else {
        Vec::new()
    };
    // Read-only — end the snapshot.
    let _ = sqlx::query("COMMIT").execute(&mut *conn).await;
    drop(conn);

    // Skip non-parity cases: cold/missing chain or no genesis (the live builder
    // would fall back here too).
    if !complete || walk_events.is_empty() || !chain_has_genesis(&walk_events) {
        return Ok(());
    }
    let mut scan_events = parse_event_rows(scan_rows);
    if scan_events.is_empty() {
        return Ok(());
    }

    // Build both states off the consistent snapshot, applying in event_id order.
    walk_events.sort_by_key(|e| e.event_id);
    hydrate_result_references(&mut walk_events, result_store, keep_refs).await;
    hydrate_result_references(&mut scan_events, result_store, keep_refs).await;
    let cw = crate::engine::state::WorkflowState::from_events(
        &walk_events.iter().map(Into::into).collect::<Vec<_>>(),
    );
    let es = crate::engine::state::WorkflowState::from_events(
        &scan_events.iter().map(Into::into).collect::<Vec<_>>(),
    );
    if let (Some(es), Some(cw)) = (es, cw) {
        parity_check_states(execution_id, &es, &cw);
    }
    Ok(())
}

/// Wall-clock fields on `WorkflowState` / `StepInfo` that are populated from the
/// event's `created_at` — which `parse_event_rows` resolves to `Utc::now()`
/// whenever the column can't be decoded (the deployed `noetl.event.created_at` is
/// `timestamp without time zone`, which doesn't decode into `DateTime<Utc>`).  So
/// these timestamps **vary across every reconstruction** in BOTH build paths
/// (`orchestrate-core/src/state.rs` documents this: "the `Utc::now()` loader
/// fallback … varies across reconstructions").  They are not part of the logical
/// state that drives decisions, so the parity comparison excludes them — what's
/// being proven is that the chain-walk and event-scan builds produce the same
/// *decision-relevant* state, not the same build-time clock reads.
const NONDETERMINISTIC_STATE_KEYS: &[&str] = &["started_at", "completed_at", "entered_at"];

/// Canonicalize a JSON value for order-insensitive structural comparison: drop
/// the non-deterministic wall-clock keys ([`NONDETERMINISTIC_STATE_KEYS`]),
/// recurse into objects, and **sort every array** by its element's canonical
/// string form.  `WorkflowState` carries `HashSet` fields (`cursor_completed`,
/// `iteration_command_ids`) that serialize to JSON arrays in arbitrary order, so
/// a raw `serde_json::Value` array comparison would flag two logically-identical
/// states as different.  Sorting is sound for this parity check: both builds
/// apply events in the same `event_id` order, so any genuinely-ordered array is
/// already element-equal between them (sorting is a no-op); only the set-backed
/// arrays need reordering.  A real difference (different elements / lengths /
/// keys) still survives the sort.
fn canonicalize_state_json(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Array(a) => {
            let mut items: Vec<serde_json::Value> = a.iter().map(canonicalize_state_json).collect();
            items.sort_by_key(|v| v.to_string());
            serde_json::Value::Array(items)
        }
        serde_json::Value::Object(o) => serde_json::Value::Object(
            o.iter()
                .filter(|(k, _)| !NONDETERMINISTIC_STATE_KEYS.contains(&k.as_str()))
                .map(|(k, val)| (k.clone(), canonicalize_state_json(val)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Shadow parity check (RFC #115 Phase 3): compare an event-scan-built state with
/// a chain-walk-built state for the same execution.  Equality is **structural**
/// via canonicalized `serde_json::Value` (order-insensitive for the state's maps
/// AND its `HashSet`-backed arrays).  Records
/// `noetl_state_build_parity_total{match|mismatch}` and, on mismatch, logs a WARN
/// naming the differing top-level state keys (with `execution_id`).
/// Observation-only — the drive uses the configured `state_build_mode` result, so
/// a parity bug can never change drive behavior.
fn parity_check_states(
    execution_id: i64,
    event_scan: &crate::engine::state::WorkflowState,
    chain_walk: &crate::engine::state::WorkflowState,
) {
    let a = serde_json::to_value(event_scan).map(|v| canonicalize_state_json(&v));
    let b = serde_json::to_value(chain_walk).map(|v| canonicalize_state_json(&v));
    match (a, b) {
        (Ok(a), Ok(b)) if a == b => {
            crate::metrics::record_state_build_parity("match");
            debug!(execution_id, "state-build parity OK (chain-walk == event-scan)");
        }
        (Ok(a), Ok(b)) => {
            crate::metrics::record_state_build_parity("mismatch");
            // Name the differing top-level keys so a mismatch is actionable.
            let diff_keys: Vec<String> = match (&a, &b) {
                (serde_json::Value::Object(ma), serde_json::Value::Object(mb)) => {
                    let mut keys: Vec<String> = ma
                        .keys()
                        .chain(mb.keys())
                        .filter(|k| ma.get(*k) != mb.get(*k))
                        .cloned()
                        .collect();
                    keys.sort();
                    keys.dedup();
                    keys
                }
                _ => vec!["<root>".to_string()],
            };
            warn!(
                execution_id,
                diff_keys = %diff_keys.join(","),
                "state-build parity MISMATCH: chain-walk state != event-scan state"
            );
        }
        _ => {
            crate::metrics::record_state_build_parity("mismatch");
            warn!(execution_id, "state-build parity: a state failed to serialize");
        }
    }
}

/// Outcome of advancing one execution's projection snapshot.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SnapshotAdvance {
    pub execution_id: i64,
    /// Snapshot version after the advance (the highest `event_id` folded).
    pub version: i64,
    /// Events folded into the saved snapshot (the execution's event count).
    pub events: i64,
}

/// Advance one execution's `projection_snapshot` from the event log — the
/// CQRS read-model write the `system/projector` playbook drives
/// (noetl/ai-meta#103 phase 2b) via `POST /api/internal/projection/advance`.
///
/// This is the orchestrator's snapshot-write half **without** command dispatch:
/// load the latest snapshot + apply events-since (the bounded [`rebuild_state`]
/// path) and re-save it.  Reuses the block-b machinery verbatim, so the snapshot
/// the projector writes is byte-for-byte what the orchestrator would have written
/// itself — the orchestrator just stops doing so when
/// `projector_owns_snapshot` is set, and reads this instead.
///
/// Idempotent: [`orch_snapshot::save`] is a monotonic upsert, so re-advancing
/// an execution (a redelivered stream batch) is a no-op or a forward move.
pub(crate) async fn advance_snapshot(
    state: &AppState,
    execution_id: i64,
) -> AppResult<SnapshotAdvance> {
    let pool = state.pools.pool_for(execution_id);
    let result_store = crate::services::result_store::ResultStoreService::new(
        pool.clone(),
        state.snowflake.clone(),
    );
    let r = rebuild_state(pool, &result_store, execution_id, state.config.refs_in_state).await?;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM noetl.event WHERE execution_id = $1")
        .bind(execution_id)
        .fetch_one(pool)
        .await?;
    crate::services::orch_snapshot::save(
        pool,
        execution_id,
        r.last_event_id,
        count,
        r.routing_meta.as_ref(),
        &r.state,
    )
    .await?;
    Ok(SnapshotAdvance {
        execution_id,
        version: r.last_event_id,
        events: count,
    })
}

pub(crate) async fn trigger_orchestrator(
    state: &AppState,
    execution_id: i64,
    trigger_event_id: i64,
) -> AppResult<i32> {
    trigger_orchestrator_inner(state, execution_id, trigger_event_id, false).await
}

/// Background reconcile poller (noetl/ai-meta#101 block b — small-tier
/// resilience).  Guarantees the orchestrator never permanently stalls.
///
/// The hot path advances state incrementally and only does the (O(events))
/// consistency `COUNT` on a throttle, so a non-triggering straggler that lands
/// in a throttle gap can leave a cursor unable to fan out — and once the cursor
/// stops, no further `command.completed` events arrive, so there is no trigger
/// to retry on.  On a constrained backend (e.g. a small Cloud SQL tier behind
/// PgBouncer, where DB latency widens the gap) this is exactly the deadlock that
/// stopped a 10×1000 run.
///
/// This task periodically force-reconciles every cached (non-terminal)
/// execution: a fresh `COUNT` + bounded rebuild folds in any missed straggler
/// and re-evaluates, so processing always resumes — slow under backpressure is
/// fine; stopping is not.  Cost is bounded: one cheap rebuild per active
/// execution per tick.
pub fn spawn_orchestrator_reconciler(state: AppState) {
    tokio::spawn(async move {
        const RECONCILE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(8);
        loop {
            tokio::time::sleep(RECONCILE_INTERVAL).await;
            for execution_id in state.orch_cache.active_executions() {
                // Execution-affinity (RFC noetl/ai-meta#116): only the owner
                // replica drives an execution.  Under affinity the orch_cache on a
                // replica holds only owned executions anyway (events are forwarded
                // to the owner), but guard explicitly so a transiently-cached
                // non-owned execution (e.g. one seeded here before forwarding) is
                // never driven from two replicas' pollers.
                if state.affinity.active() && !state.affinity.owns(execution_id) {
                    continue;
                }
                // `i64::MAX` as the trigger id keeps the immediate-straggler
                // shortcut from firing on an already-applied event.
                match trigger_orchestrator_inner(&state, execution_id, i64::MAX, true).await {
                    Ok(n) if n > 0 => {
                        info!(execution_id, commands = n, "reconcile poller advanced a stuck execution")
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!(execution_id, %e, "reconcile poller: orchestrator trigger failed")
                    }
                }
            }
        }
    });
}

/// Stateless off-server drive dispatch (RFC #115 Phase 4 remainder,
/// noetl/ai-meta#107 step 2).  Routes a `system/orchestrate` command to the
/// worker pool WITHOUT the server building `WorkflowState` — the worker
/// constructs state from the `noetl_events` WAL spine (the wasm `run` /
/// from_events entry).  The server performs ZERO `noetl.event` reads here:
///
/// - `catalog_id` + `routing` come from the execute-time descriptor.
/// - `expected_head` (the staleness-guard watermark) comes from the in-memory
///   [`crate::state::ChainHeads`] — the highest event the server has emitted for
///   this execution — not from a counted/scanned state.
/// - terminal detection comes from the descriptor's `terminal` flag, stamped at
///   the emit chokepoint (cancel / finalize / playbook completed|failed).
/// - the worker resolves `trigger_event_type` off its WAL index from
///   `trigger_event_id` (no server-side event read to classify the trigger).
///
/// No server-built `state` rides the command (the warm path ships none); an
/// incomplete WAL chain on the worker after its bounded retry is a benign no-op
/// that the reconcile poller re-drives once the drain catches up — so progress is
/// guaranteed and no partial state is ever built.  Loading the playbook YAML by
/// `catalog_id` from `noetl.catalog` is a PK lookup on the cluster catalog table,
/// NOT a `noetl.event` read.
async fn dispatch_offserver_stateless_drive(
    state: &AppState,
    execution_id: i64,
    trigger_event_id: i64,
    desc: &crate::state::ExecDescriptor,
) -> AppResult<i32> {
    // Terminal guard — no event read.  A cancel/finalize/completion already
    // stamped the descriptor; stop re-dispatching and free the in-memory state.
    if desc.terminal {
        state.exec_descriptors.evict(execution_id).await;
        state.orch_cache.evict(execution_id);
        state.chain_heads.evict(execution_id).await;
        state.chain_tails.evict(execution_id); // noetl/ai-meta#156: drop the tail ring too
        crate::metrics::record_orchestrate_drive("stateless_terminal_skip");
        debug!(
            execution_id,
            "stateless drive: execution terminal (descriptor flag); evicted, no dispatch"
        );
        return Ok(0);
    }

    // Per-execution serialise: reuse the orch_cache slot purely as the in-flight
    // lock (we never build `cache.state` on this path).  Two near-simultaneous
    // triggers / the reconcile poller must not double-issue the drive.
    let cache_slot = state.orch_cache.entry(execution_id);
    let mut cache = cache_slot.lock().await;
    if cache.orchestrate_in_flight {
        crate::metrics::record_orchestrate_drive("skipped_in_flight");
        debug!(execution_id, "stateless drive: orchestrate already in flight; skip");
        return Ok(0);
    }

    let catalog_id = desc.catalog_id;
    let routing = desc
        .routing_meta
        .as_ref()
        .map(crate::handlers::execute::CommandRouting::from_started_meta)
        .unwrap_or_default();

    // Dispatch watermark from the in-memory chain head — NO DB read.  The worker
    // serves the WAL build only once its pool-side index has caught up to this,
    // so the off-server state is never staler than the server's view.
    let expected_head = state.chain_heads.head(execution_id).await.unwrap_or(0);

    // Playbook content — PK lookup on the cluster `noetl.catalog` table (not a
    // `noetl.event` read).
    let playbook_yaml: String =
        sqlx::query_scalar("SELECT content FROM noetl.catalog WHERE catalog_id = $1")
            .bind(catalog_id)
            .fetch_one(state.pools.cluster())
            .await
            .map_err(|e| {
                AppError::Internal(format!(
                    "stateless drive: load playbook for catalog_id {catalog_id}: {e}"
                ))
            })?;
    let playbook = crate::playbook::parser::parse_playbook(&playbook_yaml)?;

    // noetl/ai-meta#156: attach the per-execution event tail the server just
    // published so the worker advances its WAL index to `expected_head`
    // immediately — drain-independent — instead of parking on the global-stream
    // drain to catch up (the source of the per-hop variance).  Empty when the
    // accelerator is off or the ring holds nothing (cold dispatch) → the worker
    // falls back to today's drain-served path, so worst case equals today.
    // noetl/ai-meta#156: scope the accelerator to the allowlisted playbooks
    // (planner + MCP children).  Auth/login executions fall here with the master
    // flag on but no prefix match → empty tail → today's drain-served path, so the
    // 15s-gateway-timeout login regression cannot recur.
    let tail_attach = state
        .config
        .tail_attach_applies(playbook.metadata.path.as_deref().unwrap_or(""), &playbook.metadata.name);
    let tail_events = if tail_attach {
        state.chain_tails.snapshot(execution_id)
    } else {
        Vec::new()
    };
    if !tail_attach && state.config.offserver_attach_tail {
        crate::metrics::record_offserver_tail_scoped_out();
    } else {
        crate::metrics::record_offserver_tail_attached(tail_events.len());
    }

    let input = serde_json::json!({
        "__offserver_build__": true,
        // No server-built `state` fallback rides the stateless command; the
        // worker self-sources the spine from the WAL.  The marker tells the
        // worker that an incomplete chain is a no-op (reconcile re-drives),
        // not a fall-through to a (now absent) server-built state.
        "__stateless__": true,
        "execution_id": execution_id,
        // The worker resolves `trigger_event_type` off its WAL index from this
        // id (no server event read to classify the trigger).
        "trigger_event_id": trigger_event_id,
        "playbook": &playbook,
        "expected_head": expected_head,
        // noetl/ai-meta#156: the recently-published events for this execution
        // (oldest→newest).  The worker applies them to its WAL index before
        // building, so a warm-index hop serves on the first build attempt.
        "tail_events": tail_events,
    });

    crate::handlers::execute::dispatch_orchestrate_command(
        state,
        execution_id,
        catalog_id,
        trigger_event_id,
        input,
        &playbook,
        &routing,
    )
    .await?;
    cache.orchestrate_in_flight = true;
    crate::metrics::record_orchestrate_drive("dispatched_offserver_stateless");
    debug!(
        execution_id,
        expected_head, "stateless drive: dispatched system/orchestrate (no server state build)"
    );
    Ok(0)
}

/// Orchestrator trigger.  `force_count` is set by the background reconcile
/// poller ([`spawn_orchestrator_reconciler`]): it forces a fresh consistency
/// `COUNT` (bypassing the throttle) and proceeds to evaluate even when no new
/// events arrived, so a stuck execution — one whose cursor missed a
/// non-triggering straggler and therefore stopped emitting events, leaving no
/// trigger to retry on — is reconciled and advanced.  The poller passes
/// `trigger_event_id = i64::MAX` so the immediate-straggler shortcut is skipped.
async fn trigger_orchestrator_inner(
    state: &AppState,
    execution_id: i64,
    trigger_event_id: i64,
    force_count: bool,
) -> AppResult<i32> {
    use crate::engine::WorkflowOrchestrator;

    debug!(
        execution_id,
        trigger_event_id, force_count, "trigger_orchestrator: loading events"
    );

    // Stateless off-server drive edge (RFC #115 Phase 4 remainder,
    // noetl/ai-meta#107 step 2).  Under `NOETL_STATE_BUILDER=offserver` +
    // worker-driven drive, when we hold a warm execute-time descriptor (catalog_id
    // + routing seeded at `playbook_started`), route the drive WITHOUT building
    // `WorkflowState` on the server — state CONSTRUCTION runs on the worker pool
    // from the WAL spine.  The server reads ZERO `noetl.event` rows on this path:
    // catalog_id + routing come from the descriptor, the dispatch watermark
    // (`expected_head`) from the in-memory `ChainHeads`, terminal detection from
    // the descriptor's emit-stamped flag.  A cold descriptor (server restart) or
    // the in-process drive falls through to the server-built path below — which
    // re-seeds the descriptor and ships the built state as the worker's
    // incomplete-WAL fallback — so correctness never regresses below today.
    if state.config.orchestrate_plugin_drive
        && matches!(
            state.config.state_builder,
            crate::config::StateBuilder::Offserver
        )
    {
        if let Some(desc) = state.exec_descriptors.get(execution_id).await {
            // The stateless off-server drive builds `WorkflowState` from the
            // `noetl_events` WAL spine on the worker.  ONLY events that PUBLISH
            // reach that stream — and `should_publish` is false for system-pool
            // executions (`system/...`), whose events are INSERTed straight to
            // `noetl.event` and never enter the WAL (they drain the stream, so
            // they can't depend on it).  Routing a system execution to the
            // stateless drive means the worker's WAL build can NEVER complete →
            // it returns the `__offserver_retry__` no-op forever and the
            // reconciler re-drives in a loop (noetl/ai-meta#121: the wedged
            // `system/scheduled_cleanup` execs).  Gate the stateless drive on
            // `should_publish(catalog_id)` so system executions fall through to
            // the server-built path below — which reads `noetl.event` (where the
            // system events DO live) and ships the built state to the worker.
            if desc.catalog_id != 0
                && crate::handlers::event_write::should_publish(state, desc.catalog_id).await
            {
                return dispatch_offserver_stateless_drive(
                    state,
                    execution_id,
                    trigger_event_id,
                    &desc,
                )
                .await;
            }
        }
        // Cold/incomplete descriptor, or a system execution whose events never
        // reach the WAL → fall through to the server-built path, which re-seeds
        // the descriptor and (for system execs) ships the built state so the
        // worker drives off `run_state` instead of an unservable WAL spine.
    }

    // 1. Incremental state with bounded rebuild (noetl/ai-meta#100, #101 block b).
    //
    // The cached `WorkflowState` advances by applying only events newer than
    // `last_event_id` behind a per-execution lock (serialising one execution's
    // triggers).  When the cache is cold, or a straggler is detected (an event
    // below the incremental watermark — workers' snowflake ids from different
    // machines in the same millisecond interleave), it rebuilds from the latest
    // `projection_snapshot` + events-since (bounded), NOT the whole event log:
    // the unbounded replay spiked memory and OOM'd the server at scale.  See
    // `rebuild_state` + `services::orch_snapshot`.

    let pool = state.pools.pool_for(execution_id);
    // Results-by-reference: an over-budget tool result is stored in the result
    // store and the event carries `{status, reference}` (no inline data).  The
    // orchestrator resolves those references back to inline data before reading
    // them so referenced results are transparent to state/templates.
    let result_store = crate::services::result_store::ResultStoreService::new(
        pool.clone(),
        state.snowflake.clone(),
    );

    // Per-execution lock: serialises this execution's concurrent completion
    // triggers so only one advances the cached state at a time.
    let cache_slot = state.orch_cache.entry(execution_id);
    let mut cache = cache_slot.lock().await;

    // State-construction outputs shared by both build modes (event-scan and
    // chain-walk).  Hoisted so the chain-walk branch and the event-scan block
    // below assign the same locals the downstream drive reads.
    let mut total: Option<i64> = None;
    let mut did_rebuild = false;
    let mut straggler_applied = false;
    let mut new_events: Vec<crate::db::models::Event> = Vec::new();
    let mut trigger_event_type = String::new();
    let mut latest_ts: Option<chrono::DateTime<chrono::Utc>> = None;

    // RFC noetl/ai-meta#115 Phase 3 — chain-walk state build (flagged).  When
    // `NOETL_STATE_BUILD_MODE=chain_walk`, rebuild `WorkflowState` for this
    // trigger by walking the `prev_event_id` chain head→root (PK lookups, no
    // `noetl.event` scan) and skip the event-scan block entirely.  Any
    // completeness doubt (cold head, missing node under materializer lag, a chain
    // that doesn't reach the genesis) falls back to event-scan for this trigger —
    // so correctness is never sacrificed.  The drive logic downstream is
    // identical; only HOW `cache.state` is built changes.
    let mut chain_built = false;
    if matches!(
        state.config.state_build_mode,
        crate::config::StateBuildMode::ChainWalk
    ) {
        if let Some(cw) = rebuild_state_chain_walk(
            state,
            pool,
            &result_store,
            execution_id,
            trigger_event_id,
            state.config.refs_in_state,
        )
        .await?
        {
            cache.state = Some(cw.result.state);
            cache.last_event_id = cw.result.last_event_id;
            cache.applied_count = cw.collected as i64;
            cache.snapshot_version = cw.result.snapshot_version;
            cache.routing_meta = cw.result.routing_meta;
            cache.last_count_check = Some(std::time::Instant::now());
            // The full chain rebuild always reflects the latest state, so treat
            // every chain-walk build like a rebuild: the drive evaluates this
            // trigger (the drive is idempotent against the current state, gated by
            // the `orchestrate_in_flight` flag + cursor id-sets).
            did_rebuild = true;
            trigger_event_type = cw.trigger_event_type;
            latest_ts = cw.latest_ts;
            // total stays None → the projection-snapshot persistence gate below
            // (`total == Some(applied_count)`) is skipped; chain-walk doesn't
            // depend on or maintain `projection_snapshot`.
            chain_built = true;
        }
        // else: fell back (metric recorded inside the builder) → run event-scan.
    }

    // Parity proof (RFC #115 Phase 3): when `NOETL_STATE_BUILD_PARITY_CHECK` is
    // on, shadow-build the state BOTH ways from ONE consistent DB snapshot and
    // assert they match — observation only (the drive keeps using whichever mode
    // built `cache.state`).  Reading both representations inside a single
    // `REPEATABLE READ` transaction removes the race against concurrent
    // materialization (and the cross-machine `event_id` interleave), so a
    // mismatch is a real builder bug, not a timing artifact.  Validation switch;
    // off in prod.
    if state.config.state_build_parity_check {
        if let Err(e) = run_parity_check(
            state,
            pool,
            &result_store,
            execution_id,
            state.config.refs_in_state,
        )
        .await
        {
            debug!(execution_id, %e, "parity check skipped (read error)");
        }
    }

    if !chain_built {
        // Event-scan path: this trigger uses the `noetl.event`-scanning state
        // construction block.  RFC #115 tenet 3 no-scan proof — with chain_walk
        // on and no fallback, this counter's delta over a run stays 0.
        crate::metrics::record_state_build_event_scan();
    }

    // ===== event-scan state construction (skipped when the chain walk built) ===
    if !chain_built {

    // The consistency `COUNT(*)` over this execution's event partition is
    // O(events) — ≈27ms at 60k events — and only detects a *non-triggering*
    // straggler (an event type that doesn't fire the orchestrator, inserted
    // below the high-water mark).  Running it on every trigger throttles the
    // whole orchestrator on a large log.  So throttle it: a fresh `total` gates
    // the mismatch→rebuild + snapshot paths; between checks the incremental
    // apply + the immediate straggler handling below carry correctness.  Cold
    // cache always counts (it needs `total` to seed `applied_count`).
    const COUNT_THROTTLE: std::time::Duration = std::time::Duration::from_millis(1000);
    let now = std::time::Instant::now();
    let do_count = force_count
        || cache.state.is_none()
        || cache
            .last_count_check
            .is_none_or(|t| now.duration_since(t) >= COUNT_THROTTLE);
    total = if do_count {
        cache.last_count_check = Some(now);
        Some(
            sqlx::query_scalar("SELECT COUNT(*) FROM noetl.event WHERE execution_id = $1")
                .bind(execution_id)
                .fetch_one(pool)
                .await?,
        )
    } else {
        None
    };

    // Warm a cold cache from the latest snapshot + events-since (bounded), or
    // the full (still-small) early log when no snapshot exists yet.
    did_rebuild = false;
    if cache.state.is_none() {
        let r = rebuild_state(pool, &result_store, execution_id, state.config.refs_in_state).await?;
        cache.state = Some(r.state);
        cache.last_event_id = r.last_event_id;
        cache.applied_count = total.unwrap_or(0);
        cache.snapshot_version = r.snapshot_version;
        cache.routing_meta = r.routing_meta;
        did_rebuild = true;
    }

    // Immediate straggler: the event that fired this trigger is at/below the
    // watermark (a late insert below the high-water mark — interleaved snowflake
    // ids).  It is not in the `> watermark` load below, so apply it directly.
    // `command.completed`/`failed` are the only types that trigger, so this
    // catches the cursor-relevant stragglers (body completions) with no COUNT
    // and no rebuild.  Idempotent: cursor counters are gated by the
    // `cursor_issued`/`cursor_completed` id-sets.
    straggler_applied = false;
    if !did_rebuild && trigger_event_id <= cache.last_event_id {
        let mut strag = parse_event_rows(
            sqlx::query(&format!(
                "{ORCH_EVENT_COLS} WHERE execution_id = $1 AND event_id = $2"
            ))
            .bind(execution_id)
            .bind(trigger_event_id)
            .fetch_all(pool)
            .await?,
        );
        hydrate_result_references(&mut strag, &result_store, state.config.refs_in_state).await;
        if let Some(ws) = cache.state.as_mut() {
            for e in &strag {
                ws.apply_event(&e.into());
            }
        }
        straggler_applied = !strag.is_empty();
        // Count the straggler so the next consistency check matches and does NOT
        // fire an (expensive) bounded rebuild for an event we already applied.
        // The straggler is below the watermark, so it is never in the
        // `> watermark` load — no double count.  A rare double-trigger overcount
        // self-heals: the next check mismatches once and the rebuild resets
        // `applied_count = total`.
        cache.applied_count += strag.len() as i64;
    }

    // Events newer than what's applied.
    new_events = parse_event_rows(
        sqlx::query(&format!(
            "{ORCH_EVENT_COLS} WHERE execution_id = $1 AND event_id > $2 ORDER BY event_id ASC"
        ))
        .bind(execution_id)
        .bind(cache.last_event_id)
        .fetch_all(pool)
        .await?,
    );
    hydrate_result_references(&mut new_events, &result_store, state.config.refs_in_state).await;

    // Trigger type + drain timestamp.  The trigger event is normally among the
    // new events; after a cold rebuild / straggler apply that already folded it
    // in, fall back to a default type (and a None drain timestamp).
    trigger_event_type = new_events
        .iter()
        .find(|e| e.event_id == trigger_event_id)
        .map(|e| e.event_type.clone())
        .unwrap_or_else(|| "command.completed".to_string());
    latest_ts = new_events.last().map(|e| e.created_at);

    // Consistency.  With a fresh `total`, applying the new events should account
    // for ALL events; a shortfall means a straggler is below the watermark — and
    // crucially it may be a *non-triggering* one (the cursor claim's `call.done`
    // carrying the row batch), which would otherwise leave the cursor unable to
    // fan out and stop emitting events entirely.  A bounded rebuild re-scans the
    // recent window and folds it in.  This check runs whether or not there are
    // new events, so the reconcile poller (no new events, fresh count) still
    // catches a stuck execution.  Without a fresh `total` (throttled), trust the
    // incremental apply.
    let mismatch = matches!(total, Some(t) if cache.applied_count + new_events.len() as i64 != t);
    if mismatch {
        let r = rebuild_state(pool, &result_store, execution_id, state.config.refs_in_state).await?;
        cache.state = Some(r.state);
        cache.last_event_id = r.last_event_id;
        cache.applied_count = total.unwrap_or(cache.applied_count);
        cache.snapshot_version = r.snapshot_version;
        cache.routing_meta = r.routing_meta;
        did_rebuild = true;
    } else if !new_events.is_empty() {
        let ws = cache.state.as_mut().unwrap();
        for e in &new_events {
            ws.apply_event(&e.into());
        }
        cache.applied_count += new_events.len() as i64;
        cache.last_event_id = new_events
            .last()
            .map(|e| e.event_id)
            .unwrap_or(cache.last_event_id);
    }

    } // end event-scan state construction (`if !chain_built`)

    // Terminal-state guard (noetl/ai-meta#113 facet 2).  A cancel
    // (`playbook_cancelled`) — or any terminal event — transitions the cached
    // `WorkflowState` to a terminal `ExecutionState` via `apply_event`.  Once
    // terminal, no further `__orchestrate__` must be dispatched: a straggler
    // `command.completed` from an in-flight command, or the background reconcile
    // poller (which forces a re-trigger on every still-cached execution), would
    // otherwise keep re-issuing the drive for a cancelled execution forever —
    // before this guard only a `rollout restart` of the server cleared the
    // in-memory loop.  Evicting the slot also drops it from
    // `active_executions()`, so the reconcile poller stops re-visiting it.  This
    // runs ahead of the early-exit below so the forced-reconcile path is caught
    // even when no new events arrived this pass.
    if cache.state.as_ref().is_some_and(|ws| ws.state.is_terminal()) {
        let st = cache.state.as_ref().map(|ws| ws.state);
        drop(cache);
        state.orch_cache.evict(execution_id);
        state.chain_heads.evict(execution_id).await; // RFC #115 §4: drop the chain head too
        state.chain_tails.evict(execution_id); // noetl/ai-meta#156: drop the tail ring too
        state.exec_descriptors.evict(execution_id).await; // RFC #115 Phase 4 remainder
        debug!(
            execution_id,
            state = ?st,
            "drive: execution is terminal; evicted orch cache, no further orchestrate dispatch"
        );
        return Ok(0);
    }

    // Nothing changed and this is not a forced reconcile → exit early.  A forced
    // reconcile still evaluates (to drive a cursor that may now advance).
    if new_events.is_empty() && !did_rebuild && !straggler_applied && !force_count {
        debug!(execution_id, "No new events to evaluate — orchestrator exit early");
        return Ok(0);
    }

    // Persist a snapshot once enough events have accrued since the last one,
    // gated on a *fresh* `total` confirming the state is fully consistent (no
    // outstanding straggler) — so the snapshot is a clean base for rebuilds.
    // Best-effort: a snapshot failure must not fail the trigger.
    //
    // CQRS read-model ownership (noetl/ai-meta#103 phase 2b): when
    // `projector_owns_snapshot` is set, the system/projector playbook folds the
    // `noetl_events` stream and advances `projection_snapshot` (via
    // `/api/internal/projection/advance`), so the orchestrator stops self-writing
    // it here and only reads it.  Default off → the orchestrator self-writes
    // exactly as block-b does.
    const SNAPSHOT_INTERVAL_EVENTS: i64 = 500;
    if !state.config.projector_owns_snapshot
        && total == Some(cache.applied_count)
        && cache.last_event_id - cache.snapshot_version >= SNAPSHOT_INTERVAL_EVENTS
    {
        let version = cache.last_event_id;
        let applied = cache.applied_count;
        let routing = cache.routing_meta.clone();
        let saved = if let Some(ws) = cache.state.as_ref() {
            crate::services::orch_snapshot::save(
                pool,
                execution_id,
                version,
                applied,
                routing.as_ref(),
                ws,
            )
            .await
        } else {
            Ok(())
        };
        match saved {
            Ok(()) => cache.snapshot_version = version,
            Err(e) => {
                warn!(execution_id, %e, "orch_snapshot.save failed; continuing without snapshot")
            }
        }
    }

    // 2. Look up catalog_id (from the cached state) + playbook content.
    let catalog_id = cache.state.as_ref().map(|s| s.catalog_id).unwrap_or(0);
    if catalog_id == 0 {
        return Err(AppError::Internal(format!(
            "No catalog_id found for execution {execution_id}"
        )));
    }

    // Re-seed the execute-time descriptor from this server-built pass (RFC #115
    // Phase 4 remainder).  This path runs when the descriptor was cold (a server
    // restart mid-execution), so seeding catalog_id + the cached routing meta
    // here lets the NEXT trigger take the stateless branch — the server reads
    // `noetl.event` only on this recovery pass, then goes scan-free.
    state
        .exec_descriptors
        .seed(execution_id, catalog_id, cache.routing_meta.clone())
        .await;

    // Phase F R4-3: noetl.catalog is a cluster-wide table.
    let playbook_yaml: String =
        sqlx::query_scalar("SELECT content FROM noetl.catalog WHERE catalog_id = $1")
            .bind(catalog_id)
            .fetch_one(state.pools.cluster())
            .await
            .map_err(|e| {
                AppError::Internal(format!(
                    "Failed to load playbook for catalog_id {catalog_id}: {e}"
                ))
            })?;
    let playbook = crate::playbook::parser::parse_playbook(&playbook_yaml)?;

    // 3. Resolve over-budget cursor claim references into inline rows before the
    //    drive reads them (noetl/ai-meta#101 phase 2) — a referenced claim has
    //    `claim_ref` set + empty `claim_rows`; without this the cursor sees 0
    //    rows and wrongly DRAINs.  No-op unless the flag kept a claim reference.
    //    Scoped so the `&mut` borrow ends before the drive branches read `cache`.
    {
        let ws = cache.state.as_mut().unwrap();
        resolve_cursor_claim_refs(ws, &result_store).await;
    }

    // Worker-driven drive (noetl/ai-meta#108 slice 3): issue `system/orchestrate`
    // to the worker pool instead of evaluating in-process.  The worker runs the
    // drive on the bounded `WorkflowState`; `apply_worker_orchestration` applies
    // its result on the command's completion.  Default off → the in-process path
    // below runs unchanged.
    if state.config.orchestrate_plugin_drive {
        // Serialise drives per execution: if one is already dispatched, don't
        // double-issue (two near-simultaneous triggers / the reconcile poller
        // would otherwise produce two orchestrate commands → duplicate work).
        if cache.orchestrate_in_flight {
            crate::metrics::record_orchestrate_drive("skipped_in_flight");
            debug!(execution_id, "worker-driven: orchestrate already in flight; skip");
            return Ok(0);
        }
        let routing = cache
            .routing_meta
            .as_ref()
            .map(crate::handlers::execute::CommandRouting::from_started_meta)
            .unwrap_or_default();
        // Off-server drive cutover (RFC #115 Phase 4): under
        // `NOETL_STATE_BUILDER=offserver` the drive obtains its `WorkflowState`
        // from the pool-side WAL builder (the worker walks the `prev_event_id`
        // chain off the `noetl_events` WAL and feeds the spine to the wasm `run`
        // / from_events entry — state CONSTRUCTION runs off the server).  We mark
        // the command `__offserver_build__` + carry `execution_id` so the worker
        // self-sources the spine; the server-built `state` rides along as the
        // **fallback** the worker uses only when its WAL chain is incomplete
        // (lag / cold) — so progress + correctness never regress below the
        // server-built `run_state` path.  Default `server` → the state is the
        // sole drive input exactly as today.
        // Off-server WAL build applies only to executions whose events PUBLISH to
        // `noetl_events` — system executions (`should_publish` false) INSERT their
        // events and never enter the WAL (noetl/ai-meta#121), so the worker would
        // burn its retry window on an unservable WAL spine before falling back to
        // `run_state`.  Drive those purely server-built (`offserver=false` → the
        // worker uses the shipped `state` via `run_state` directly, no WAL probe).
        let offserver = matches!(
            state.config.state_builder,
            crate::config::StateBuilder::Offserver
        ) && crate::handlers::event_write::should_publish(state, catalog_id).await;
        // noetl/ai-meta#156: same tail-attach accelerator as the stateless edge —
        // only meaningful on the off-server path (`offserver` true), where the
        // worker self-sources the spine.  Empty when the flag is off or nothing is
        // buffered; the server-built `state` here is the fallback regardless.
        // noetl/ai-meta#156: scope the accelerator to the allowlisted playbooks
        // (planner + MCP children); auth executions get an empty tail even with the
        // master flag on, keeping today's drain-served path (no login regression).
        let tail_attach = offserver
            && state.config.tail_attach_applies(
                playbook.metadata.path.as_deref().unwrap_or(""),
                &playbook.metadata.name,
            );
        let tail_events = if tail_attach {
            state.chain_tails.snapshot(execution_id)
        } else {
            Vec::new()
        };
        if offserver {
            if !tail_attach && state.config.offserver_attach_tail {
                crate::metrics::record_offserver_tail_scoped_out();
            } else {
                crate::metrics::record_offserver_tail_attached(tail_events.len());
            }
        }
        let input = serde_json::json!({
            "state": cache.state.as_ref().expect("state present after rebuild"),
            "latest_ts": latest_ts,
            "playbook": &playbook,
            "trigger_event_type": trigger_event_type,
            "__offserver_build__": offserver,
            "execution_id": execution_id,
            // noetl/ai-meta#156: recently-published tail for this execution; the
            // worker applies it to its WAL index before building.
            "tail_events": tail_events,
            // The server's dispatch watermark (the highest event applied to the
            // server-built state).  The worker's WAL build serves only once its
            // pool-side index has caught up to this head — so the off-server
            // state is never staler than the server's view, which prevents a
            // lag-induced re-issue of a fan-in barrier step (RFC #115 Phase 4).
            "expected_head": cache.last_event_id,
            // RFC #115 Phase 5: carry the atomic-item-context flag so the
            // off-server drive narrows worker-bound command contexts too.  The
            // server-built `run_state` fallback reads it directly; the worker
            // forwards it onto its from_events `OrchestrateInput` (Phase 5b).
            "atomic_item_context": state.config.atomic_item_context,
        });
        crate::handlers::execute::dispatch_orchestrate_command(
            state,
            execution_id,
            catalog_id,
            trigger_event_id,
            input,
            &playbook,
            &routing,
        )
        .await?;
        cache.orchestrate_in_flight = true;
        crate::metrics::record_orchestrate_drive(if offserver {
            "dispatched_offserver"
        } else {
            "dispatched"
        });
        debug!(
            execution_id,
            offserver, "worker-driven: dispatched system/orchestrate to the pool"
        );
        return Ok(0);
    }

    // In-process drive (the `NOETL_ORCHESTRATE_PLUGIN_DRIVE=false` fallback).
    // RFC noetl/ai-meta#115 Phase 5: narrow each command's worker-bound context
    // to the minimal working-item slice when the atomic-item-context flag is on.
    let orchestrator =
        WorkflowOrchestrator::with_atomic_item_context(state.config.atomic_item_context);
    let ws = cache.state.as_mut().unwrap();
    let result = match orchestrator.evaluate_state(
        ws,
        latest_ts,
        &playbook,
        Some(trigger_event_type.as_str()),
    ) {
        Ok(r) => r,
        Err(e) => {
            // A deterministic orchestrator evaluate failure (invalid
            // template in a step body, unknown step in a `next` arc,
            // malformed routing) fails identically on every retry.
            // Emitting only a WARN here strands the execution in RUNNING
            // forever — the next step is never issued and no terminal
            // event is written, so `/api/executions/{id}` reports RUNNING
            // indefinitely.  Instead write a terminal `playbook.failed`
            // event so the run resolves to FAILED with the error surfaced
            // to the client.
            //
            // noetl/ai-meta#54 (e2e regression sweep): `test_vars_template_access`
            // hung after `set_variables` because an invalid `{{ ctx.* }}`
            // template in a downstream step's `code` body tripped minijinja
            // inside `evaluate`, and the WARN-only path left no terminal
            // event.  This mirrors the noetl/ai-meta#58 `command.failed`
            // stall class — a deterministic failure must still produce a
            // terminal event.
            let msg = format!("Orchestrator evaluate failed: {e}");
            warn!(
                execution_id,
                error = %msg,
                "Orchestrator evaluate error is deterministic — terminating execution as FAILED"
            );
            emit_playbook_failed(state, execution_id, catalog_id, trigger_event_id, &msg).await?;
            return Ok(0);
        }
    };

    info!(
        execution_id,
        trigger_event_id,
        new_commands = result.commands.len(),
        new_events = result.events_to_emit.len(),
        should_complete = result.should_complete,
        "Orchestrator evaluate complete"
    );

    // Recover the per-execution routing (dedicated pool segment + W3C trace)
    // the `/api/execute` caller supplied, persisted on the `playbook_started`
    // event meta, so every follow-up command lands on the same segment and
    // carries the same trace context (noetl/ai-meta#90 Phase 2).
    let routing = cache
        .routing_meta
        .as_ref()
        .map(crate::handlers::execute::CommandRouting::from_started_meta)
        .unwrap_or_default();

    // 4-6. Apply the orchestration result — emit pure events, issue commands,
    // write the terminal event. Extracted (noetl/ai-meta#108) so the
    // worker-driven drive can apply a result computed on a worker identically.
    let commands_generated = apply_orchestration_result(
        state,
        execution_id,
        catalog_id,
        trigger_event_id,
        &result,
        &playbook,
        &routing,
    )
    .await?;

    // Free the cached state for a terminal execution (noetl/ai-meta#100). The
    // caller owns the cache guard, so the evict lives here, not in the apply fn.
    if result.should_complete {
        drop(cache);
        state.orch_cache.evict(execution_id);
        state.chain_heads.evict(execution_id).await; // RFC #115 §4: drop the chain head too
        state.chain_tails.evict(execution_id); // noetl/ai-meta#156: drop the tail ring too
        state.exec_descriptors.evict(execution_id).await; // RFC #115 Phase 4 remainder
    }

    Ok(commands_generated)
}

/// Apply an [`OrchestrationResult`](noetl_orchestrate_core::orchestrator::OrchestrationResult)
/// to the event log: emit the pure events (`step.enter` etc.) in causal order,
/// issue the new commands (batched), then write the terminal
/// `playbook.completed`/`failed` event when the drive says the run is done.
///
/// Extracted verbatim from `trigger_orchestrator_inner` (noetl/ai-meta#108) so
/// the worker-driven drive can apply a result computed on a worker the same way
/// the in-process drive applies its own. Returns the number of commands issued.
/// The caller owns the per-execution cache guard and performs the terminal
/// evict (this fn has no cache access).
async fn apply_orchestration_result(
    state: &AppState,
    execution_id: i64,
    catalog_id: i64,
    trigger_event_id: i64,
    result: &noetl_orchestrate_core::orchestrator::OrchestrationResult,
    playbook: &crate::playbook::types::Playbook,
    routing: &crate::handlers::execute::CommandRouting,
) -> AppResult<i32> {
    // 4. Emit pure events (step.enter etc.) before issuing new
    //    commands so the causal chain is correct.
    for emit in &result.events_to_emit {
        let event_id = state.snowflake.generate()?;
        let event_status = if emit.status.is_empty() {
            "STARTED".to_string()
        } else {
            emit.status.clone()
        };

        // Compose the constraint-compliant {status, context} result
        // envelope when context is present, else {status} alone.
        let result_obj = match &emit.context {
            Some(serde_json::Value::Object(_)) => serde_json::json!({
                "status": event_status,
                "context": emit.context.clone().unwrap(),
            }),
            _ => serde_json::json!({"status": event_status}),
        };

        // CQRS write-path chokepoint (#103 2d-3).
        let mut ev = crate::handlers::event_write::EventRow::new(
            event_id,
            execution_id,
            catalog_id,
            emit.event_type.clone(),
            event_status.clone(),
            chrono::Utc::now(),
        )
        .with_result(result_obj.clone())
        .with_meta(serde_json::json!({"emitted_by": "orchestrator"}))
        .with_parent_event_id(trigger_event_id);
        if let Some(n) = emit.node_name.as_deref() {
            ev = ev.with_node(n);
        }
        crate::handlers::event_write::emit_event(state, state.pools.pool_for(execution_id), ev)
            .await?;
    }

    // 5. Issue new commands — batched (noetl/ai-meta#102 step 1).  A cursor
    //    fan-out's N body commands now persist as two multi-row INSERTs (all
    //    `command.issued` events, then all `noetl.command` rows) instead of ~2N
    //    individual round-trips through PgBouncer to Cloud SQL — the write-path
    //    bottleneck on a small tier.  NATS publishes still loop (in-cluster).
    let commands_generated = crate::handlers::execute::persist_engine_commands_batch(
        state,
        execution_id,
        catalog_id,
        trigger_event_id,
        &result.commands,
        playbook,
        routing,
    )
    .await?;

    // 6. Emit terminal playbook event when the orchestrator says so.
    if result.should_complete {
        let (event_type, status) = match &result.completion_status {
            Some(cs) if cs.status == "FAILED" => ("playbook.failed", "FAILED"),
            _ => ("playbook.completed", "COMPLETED"),
        };
        let event_id = state.snowflake.generate()?;
        let terminal_meta = serde_json::to_value(&result.completion_status).unwrap_or_default();
        // CQRS write-path chokepoint (#103 2d-3).
        let ev = crate::handlers::event_write::EventRow::new(
            event_id,
            execution_id,
            catalog_id,
            event_type,
            status,
            chrono::Utc::now(),
        )
        .with_node("playbook")
        .with_result(serde_json::json!({"status": status}))
        .with_meta(terminal_meta)
        .with_parent_event_id(trigger_event_id);
        crate::handlers::event_write::emit_event(state, state.pools.pool_for(execution_id), ev)
            .await?;
        info!(
            execution_id,
            terminal_event = %event_type,
            "Orchestrator marked execution as terminal"
        );
    }

    Ok(commands_generated)
}

/// Recursively find the `output_b64` string a wasm plug-in's tool result carries
/// (the worker base64-wraps the plug-in's output bytes; the exact nesting
/// depends on the result-envelope shape, so search rather than assume a path).
/// Find a `true` boolean flag by key anywhere in the value (the worker's
/// `call.done` result nests its data under tool-result envelopes).  Used to
/// detect the stateless-drive `__offserver_retry__` no-op marker (RFC #115
/// Phase 4 remainder).
fn find_bool_flag(v: &serde_json::Value, key: &str) -> bool {
    match v {
        serde_json::Value::Object(m) => {
            if m.get(key).and_then(|x| x.as_bool()) == Some(true) {
                return true;
            }
            m.values().any(|x| find_bool_flag(x, key))
        }
        serde_json::Value::Array(a) => a.iter().any(|x| find_bool_flag(x, key)),
        _ => false,
    }
}

fn find_output_b64(v: &serde_json::Value) -> Option<&str> {
    match v {
        serde_json::Value::Object(m) => {
            if let Some(serde_json::Value::String(s)) = m.get("output_b64") {
                return Some(s);
            }
            m.values().find_map(find_output_b64)
        }
        serde_json::Value::Array(a) => a.iter().find_map(find_output_b64),
        _ => None,
    }
}

/// Find the first durable result-store URI (`noetl://…`) carried by a `ref`
/// field anywhere in the value.  The worker stamps it on the `reference` block
/// of an over-budget `call.done` result (`build_call_done_result`'s durable
/// path).  Used to recover an offloaded `OrchestrationResult` whose
/// `output_b64` was staged to the result store instead of inlined — see
/// [`apply_worker_orchestration`] and noetl/ai-meta#113.  Matches the exact key
/// `"ref"` (the durable reference URI), not `"_ref"` (the inline navigation
/// hint) — both point at the same row, but `"ref"` is the durable contract.
fn find_noetl_ref(v: &serde_json::Value) -> Option<&str> {
    match v {
        serde_json::Value::Object(m) => {
            if let Some(serde_json::Value::String(s)) = m.get("ref") {
                if s.starts_with("noetl://") {
                    return Some(s);
                }
            }
            m.values().find_map(find_noetl_ref)
        }
        serde_json::Value::Array(a) => a.iter().find_map(find_noetl_ref),
        _ => None,
    }
}

/// Find the first canonical result URI (`noetl://…`) carried by a `uri` field
/// anywhere in the value.  The worker stamps it on the `reference` block of an
/// over-budget `call.done` result, additively alongside the legacy `ref`
/// (noetl/ai-meta#104 R02b — the stable logical Resource Locator
/// `noetl://<tenant>/<project>/results/<eid>/<step>/<frame>/<row>/<attempt>`).
/// Symmetric to [`find_noetl_ref`] but matches the `"uri"` key (the canonical
/// logical locator) rather than `"ref"` (the legacy durable store key); the
/// `reference` block can sit at `result.reference` or
/// `result.context.result.reference`, so the search recurses.
fn find_reference_uri(v: &serde_json::Value) -> Option<&str> {
    match v {
        serde_json::Value::Object(m) => {
            if let Some(serde_json::Value::String(s)) = m.get("uri") {
                if s.starts_with("noetl://") {
                    return Some(s);
                }
            }
            m.values().find_map(find_reference_uri)
        }
        serde_json::Value::Array(a) => a.iter().find_map(find_reference_uri),
        _ => None,
    }
}

/// Shadow-accept the canonical result URI (RFC noetl/ai-meta#104 Phase A).
///
/// When the built `result` carries a `reference.uri`, parse + validate it via
/// the shared [`noetl_locator`] (accepting **both** the canonical
/// logical Resource Locator and the legacy execution ref) and record the
/// outcome on `noetl_result_uri_accept_total{outcome}`.  A malformed URI is
/// logged (WARN, with `execution_id`) + counted `malformed` but **never fails
/// the event** — Phase A adds acceptance, not a failure path, and does not yet
/// resolve by the URI (Phase C) or write the Feather tier (Phase B).
///
/// The caller gates this on `NOETL_RESULT_URI_ACCEPT`; it is a no-op when no
/// `reference.uri` is present.
fn accept_canonical_result_uri(result: &serde_json::Value, execution_id: i64) {
    let Some(uri) = find_reference_uri(result) else {
        return;
    };
    match crate::services::result_store::parse_result_ref(uri) {
        Ok(parsed) => crate::metrics::record_result_uri_accept(parsed.shape()),
        Err(e) => {
            crate::metrics::record_result_uri_accept("malformed");
            warn!(
                execution_id,
                result_uri = uri,
                error = %e,
                "canonical result URI malformed; accepted as-is, not resolved (noetl/ai-meta#104 Phase A)"
            );
        }
    }
}

/// Decode a base64-wrapped `OrchestrationResult` (the worker base64-wraps the
/// `system/orchestrate` plug-in's output bytes).  Returns `None` on a missing
/// input, a base64 error, or a JSON error (an error envelope rather than a
/// result).  Shared by the inline and offloaded decode paths in
/// [`apply_worker_orchestration`].
fn decode_orchestration_result(
    b64: Option<&str>,
) -> Option<noetl_orchestrate_core::orchestrator::OrchestrationResult> {
    use base64::Engine;
    b64.and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

/// Decode a base64-wrapped orchestrate-drive ERROR envelope (`{"error": "..."}`)
/// — the `system/orchestrate` plug-in emits this (instead of an
/// `OrchestrationResult`) when the off-server drive's `evaluate_state` returns
/// `Err`: a deterministic, every-retry failure such as an invalid step-body
/// template, an unknown `next` step, or a loop `in:` expression that does not
/// evaluate to an iterable (noetl/ai-meta#123).  Returns the error message so
/// the caller can terminate the execution as FAILED rather than treating the
/// envelope as an undecodable result and silently no-op'ing — which strands the
/// run in RUNNING forever (the observability regression #123 fixes).
///
/// A *transient* decode miss (missing `output_b64` from a race, truncated
/// base64) parses as neither an `OrchestrationResult` nor this envelope, so it
/// stays on the benign `decode_error` re-drive path — only a structured error
/// envelope is treated as terminal.
fn decode_orchestrate_error(b64: Option<&str>) -> Option<String> {
    use base64::Engine;
    let bytes = b64.and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok())?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    match v.get("error") {
        Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// Apply the `OrchestrationResult` a worker computed for the worker-driven drive
/// (noetl/ai-meta#108 slice 3): decode it from the `system/orchestrate`
/// completion, then emit events + issue commands via `apply_orchestration_result`
/// — the same emission the in-process drive uses. Always clears the per-execution
/// in-flight guard so the next trigger can dispatch again.
async fn apply_worker_orchestration(
    state: &AppState,
    execution_id: i64,
    trigger_event_id: i64,
    payload: &serde_json::Value,
) -> AppResult<i32> {
    // Clear the in-flight guard first (the drive completed, success or not) so a
    // decode failure doesn't wedge the execution — the reconcile can re-drive.
    let cache_slot = state.orch_cache.entry(execution_id);
    let mut cache = cache_slot.lock().await;
    cache.orchestrate_in_flight = false;

    // Stateless off-server retry (RFC #115 Phase 4 remainder): a stateless drive
    // whose WAL chain was still incomplete after the worker's bounded retry
    // returns a benign no-op marker instead of a built result (no server-built
    // state fallback rides the stateless command).  This is NOT a decode error —
    // clear in-flight (already done) and let the reconcile poller re-drive once
    // the pool-side drain catches up.  No state read, no command issued.
    if find_bool_flag(payload, "__offserver_retry__") {
        crate::metrics::record_orchestrate_drive("offserver_retry");
        debug!(
            execution_id,
            "stateless drive: worker WAL incomplete; benign no-op, reconcile will re-drive"
        );
        return Ok(0);
    }

    // Inline path: the drive result fit the worker's inline budget
    // (`NOETL_EVENT_RESULT_CONTEXT_MAX_BYTES`, default 100KB) and rides the
    // `call.done` payload as an `output_b64` string.
    let mut decoded = decode_orchestration_result(find_output_b64(payload));

    // Offloaded path (noetl/ai-meta#113): a large drive result (≈ the full
    // execution context — e.g. http_to_postgres, save_edge_cases) exceeds the
    // inline budget, so the worker stages the whole tool result (which carries
    // the `output_b64`) to the durable result store and emits only a
    // `reference.ref` (`noetl://…`) — no inline `output_b64`.  Before this
    // fallback the server dropped the drive decision, re-issued `__orchestrate__`,
    // re-evaluated the same large state, re-offloaded, and never converged
    // (a runaway PENDING-orchestrate loop, no terminal event).  Resolve the ref
    // from the store (the same `result_store.resolve` the cursor/reference paths
    // already use) and decode `output_b64` from the stored result instead.
    if decoded.is_none() {
        if let Some(ref_uri) = find_noetl_ref(payload) {
            let pool = state.pools.pool_for(execution_id);
            let result_store = crate::services::result_store::ResultStoreService::new(
                pool.clone(),
                state.snowflake.clone(),
            );
            match crate::services::result_store::parse_noetl_ref(ref_uri) {
                Ok(parsed) => match result_store.resolve(&parsed).await {
                    Ok(Some(stored)) => {
                        decoded = decode_orchestration_result(find_output_b64(&stored));
                        if decoded.is_some() {
                            crate::metrics::record_orchestrate_drive("ref_resolved");
                            info!(
                                execution_id,
                                ref_uri,
                                "worker-driven: decoded offloaded OrchestrationResult from the \
                                 durable result store (over-budget drive result, noetl/ai-meta#113)"
                            );
                        } else {
                            warn!(
                                execution_id,
                                ref_uri,
                                "worker-driven: resolved orchestrate result ref but it carried no \
                                 decodable output_b64"
                            );
                        }
                    }
                    Ok(None) => warn!(
                        execution_id,
                        ref_uri, "worker-driven: orchestrate result ref not found in store"
                    ),
                    Err(e) => warn!(
                        execution_id,
                        ref_uri, %e,
                        "worker-driven: orchestrate result ref resolution failed"
                    ),
                },
                Err(e) => warn!(
                    execution_id,
                    ref_uri, %e,
                    "worker-driven: unparseable orchestrate result ref"
                ),
            }
        }
    }

    let Some(result) = decoded else {
        // noetl/ai-meta#123: distinguish a deterministic DRIVE ERROR from a
        // transient decode miss.  When the off-server drive's `evaluate_state`
        // returns `Err` (invalid template, unknown `next` step, or a loop `in:`
        // that does not evaluate to an iterable), the `system/orchestrate`
        // plug-in returns a structured `{"error": "..."}` envelope rather than an
        // `OrchestrationResult`.  Treating that as an undecodable result and
        // returning Ok(0) wedges the execution in RUNNING forever (commands=0, no
        // terminal event) — the exact silent-wedge regression #123 fixes.  The
        // in-process drive already terminates such failures as `playbook.failed`
        // (the `evaluate_state` Err arm in `trigger_orchestrator_inner`); the
        // off-server drive must do the same so a malformed playbook fails loudly.
        if let Some(err_msg) = decode_orchestrate_error(find_output_b64(payload)) {
            // catalog_id for the terminal event: prefer the warm execute-time
            // descriptor (off-server apply path), else 0 — the event still
            // resolves the run to FAILED and surfaces the message to the client.
            let catalog_id = state
                .exec_descriptors
                .get(execution_id)
                .await
                .map(|d| d.catalog_id)
                .filter(|c| *c != 0)
                .unwrap_or(0);
            let msg = format!("Orchestrator drive failed (off-server): {err_msg}");
            warn!(
                execution_id,
                error = %msg,
                "worker-driven: drive returned a deterministic error envelope — \
                 terminating execution as FAILED"
            );
            crate::metrics::record_orchestrate_drive("drive_error");
            emit_playbook_failed(state, execution_id, catalog_id, trigger_event_id, &msg).await?;
            return Ok(0);
        }
        warn!(
            execution_id,
            "worker-driven: could not decode OrchestrationResult from orchestrate completion \
             (missing output_b64, bad base64, or an error envelope)"
        );
        crate::metrics::record_orchestrate_drive("decode_error");
        return Ok(0);
    };

    // Stateless off-server apply (RFC #115 Phase 4 remainder): under
    // `NOETL_STATE_BUILDER=offserver` with a warm execute-time descriptor, the
    // worker built + drove the state from the WAL; applying its result needs only
    // `catalog_id` + `routing` (both from the descriptor) + the playbook —
    // `apply_orchestration_result` issues `result.commands`, it does not read
    // `cache.state`.  So skip the cold-rebuild ENTIRELY (no `noetl.event` read on
    // the apply side of the drive path either).  A cold descriptor falls through
    // to the crash-recovery rebuild below.
    let offserver_apply = if matches!(
        state.config.state_builder,
        crate::config::StateBuilder::Offserver
    ) {
        state
            .exec_descriptors
            .get(execution_id)
            .await
            .filter(|d| d.catalog_id != 0)
    } else {
        None
    };

    let (catalog_id, routing) = if let Some(desc) = offserver_apply {
        crate::metrics::record_orchestrate_drive("applied_stateless");
        (
            desc.catalog_id,
            desc.routing_meta
                .as_ref()
                .map(crate::handlers::execute::CommandRouting::from_started_meta)
                .unwrap_or_default(),
        )
    } else {
        // Cold cache on apply (noetl/ai-meta#104 — off-server-drive × gate
        // crash-recovery).  The orch_cache only evicts on terminal, so a cold slot
        // here means the server restarted between dispatching this `__orchestrate__`
        // command and receiving its `call.done` (the off-server round-trip — NATS →
        // worker → drive → call.done — widens that window vs the in-process drive).
        // Dropping the result would strand the execution: no next command is issued,
        // no further real event fires a trigger, and the reconcile poller only
        // re-drives executions still in `active_executions()` (empty after restart).
        // Instead rebuild `WorkflowState` from the durable log — under PUBLISH_ONLY
        // that log is the materializer's projection of the NATS WAL, the same source
        // the warm trigger reads — so the drive result applies onto consistent
        // committed state.  Idempotent: `apply_orchestration_result` issues commands
        // keyed by deterministic `command_id`, and the cursor counters are gated by
        // the `cursor_issued`/`cursor_completed` id-sets, so a re-applied result is a
        // forward no-op rather than a double-issue.
        if cache.state.is_none() {
            let pool = state.pools.pool_for(execution_id);
            let result_store = crate::services::result_store::ResultStoreService::new(
                pool.clone(),
                state.snowflake.clone(),
            );
            match rebuild_state(pool, &result_store, execution_id, state.config.refs_in_state).await
            {
                Ok(r) => {
                    let total: i64 = sqlx::query_scalar(
                        "SELECT COUNT(*) FROM noetl.event WHERE execution_id = $1",
                    )
                    .bind(execution_id)
                    .fetch_one(pool)
                    .await
                    .unwrap_or(0);
                    cache.last_event_id = r.last_event_id;
                    cache.applied_count = total;
                    cache.snapshot_version = r.snapshot_version;
                    cache.routing_meta = r.routing_meta;
                    cache.state = Some(r.state);
                    crate::metrics::record_orchestrate_drive("cold_rebuild");
                    info!(
                        execution_id,
                        "worker-driven: cold cache on apply — rebuilt WorkflowState from the durable \
                         log before applying (crash-recovery, noetl/ai-meta#104)"
                    );
                }
                Err(e) => {
                    warn!(
                        execution_id,
                        error = %e,
                        "worker-driven: cold cache on apply and rebuild failed; cannot apply \
                         orchestrate result this pass (reconcile will retry)"
                    );
                    crate::metrics::record_orchestrate_drive("cold_rebuild_failed");
                    return Ok(0);
                }
            }
            // A cold descriptor under offserver → re-seed from this recovery
            // rebuild so the next trigger goes stateless again.
            if matches!(
                state.config.state_builder,
                crate::config::StateBuilder::Offserver
            ) {
                let cid = cache.state.as_ref().map(|s| s.catalog_id).unwrap_or(0);
                state
                    .exec_descriptors
                    .seed(execution_id, cid, cache.routing_meta.clone())
                    .await;
            }
        }

        let catalog_id = cache.state.as_ref().map(|s| s.catalog_id).unwrap_or(0);
        if catalog_id == 0 {
            warn!(
                execution_id,
                "worker-driven: cold cache (no catalog_id); cannot apply orchestrate result this pass"
            );
            return Ok(0);
        }
        let routing = cache
            .routing_meta
            .as_ref()
            .map(crate::handlers::execute::CommandRouting::from_started_meta)
            .unwrap_or_default();
        (catalog_id, routing)
    };

    // Phase F R4-3: noetl.catalog is a cluster-wide table.
    let playbook_yaml: String =
        sqlx::query_scalar("SELECT content FROM noetl.catalog WHERE catalog_id = $1")
            .bind(catalog_id)
            .fetch_one(state.pools.cluster())
            .await
            .map_err(|e| {
                AppError::Internal(format!("worker-driven: load playbook {catalog_id}: {e}"))
            })?;
    let playbook = crate::playbook::parser::parse_playbook(&playbook_yaml)?;

    let commands_generated = apply_orchestration_result(
        state,
        execution_id,
        catalog_id,
        trigger_event_id,
        &result,
        &playbook,
        &routing,
    )
    .await?;
    crate::metrics::record_orchestrate_drive("applied");

    // Evict on a terminal outcome.  `should_complete` covers the normal
    // completion path (the drive emits the terminal event).  `result.state` being
    // terminal also covers a drive that ran on already-terminal worker-built
    // state (RFC #115 Phase 4 remainder: a straggler that triggered a drive after
    // a cancel — the worker's WAL-built state is `Cancelled`, evaluate returns a
    // no-op with `state: Cancelled`) so the slot is freed and not re-driven.
    if result.should_complete || result.state.is_terminal() {
        drop(cache);
        state.orch_cache.evict(execution_id);
        state.chain_heads.evict(execution_id).await; // RFC #115 §4: drop the chain head too
        state.chain_tails.evict(execution_id); // noetl/ai-meta#156: drop the tail ring too
        state.exec_descriptors.evict(execution_id).await; // RFC #115 Phase 4 remainder
    }
    Ok(commands_generated)
}

/// Emit a terminal `playbook.failed` event for an execution that hit a
/// deterministic, non-retryable orchestrator error during evaluate.
///
/// Without this an evaluate failure (invalid template in a step body,
/// unknown step in a `next` arc) leaves only a WARN in the server log
/// and strands the execution in RUNNING forever — no terminal event is
/// ever written.  The list-status aggregation in `list_executions`
/// maps `playbook.failed` -> FAILED, so writing this event resolves the
/// run and surfaces `error` to API readers.  Parented on the trigger
/// event so the causal chain stays intact.
async fn emit_playbook_failed(
    state: &AppState,
    execution_id: i64,
    catalog_id: i64,
    trigger_event_id: i64,
    error: &str,
) -> AppResult<()> {
    let event_id = state.snowflake.generate()?;
    // CQRS write-path chokepoint (#103 2d-3).
    let ev = crate::handlers::event_write::EventRow::new(
        event_id,
        execution_id,
        catalog_id,
        "playbook.failed",
        "FAILED",
        chrono::Utc::now(),
    )
    .with_node("playbook")
    .with_result(serde_json::json!({"status": "FAILED", "context": {"error": error}}))
    .with_meta(serde_json::json!({
        "emitted_by": "orchestrator",
        "reason": "evaluate_error",
        "error": error,
    }))
    .with_parent_event_id(trigger_event_id);
    crate::handlers::event_write::emit_event(state, state.pools.pool_for(execution_id), ev).await?;
    info!(
        execution_id,
        terminal_event = "playbook.failed",
        "Orchestrator evaluate error → execution terminated as FAILED"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── RFC #104 Phase A — shadow-accept the canonical result URI ────────────

    /// The over-budget `call.done` envelope the worker emits: the canonical
    /// `reference.uri` sits nested under `context.result.reference`, alongside
    /// the legacy `ref`.
    #[test]
    fn find_reference_uri_locates_nested_worker_shape() {
        let result = serde_json::json!({
            "status": "completed",
            "context": {
                "result": {
                    "reference": {
                        "ref": "noetl://execution/325/result/load_next_facility/9001",
                        "uri": "noetl://default/default/results/325/load_next_facility/2/4/1",
                        "extracted": { "rows": 4 }
                    },
                    "context": { "data": { "_ref": "noetl://execution/325/result/load_next_facility/9001" } }
                }
            }
        });
        assert_eq!(
            find_reference_uri(&result),
            Some("noetl://default/default/results/325/load_next_facility/2/4/1")
        );
    }

    /// Top-level (un-wrapped tool result) carries the reference at
    /// `result.reference`.
    #[test]
    fn find_reference_uri_locates_top_level_shape() {
        let result = serde_json::json!({
            "status": "completed",
            "reference": { "uri": "noetl://t_acme/p_gen/results/1/start/0/0/1" }
        });
        assert_eq!(
            find_reference_uri(&result),
            Some("noetl://t_acme/p_gen/results/1/start/0/0/1")
        );
    }

    /// An inline result (no reference) yields nothing — the hook is a no-op.
    #[test]
    fn find_reference_uri_absent_on_inline_result() {
        let result = serde_json::json!({
            "status": "completed",
            "context": { "data": { "rows": [1, 2, 3] } }
        });
        assert_eq!(find_reference_uri(&result), None);
        // A non-`noetl://` `uri` (e.g. an http response field) is ignored.
        let other = serde_json::json!({ "context": { "data": { "uri": "https://example.com/x" } } });
        assert_eq!(find_reference_uri(&other), None);
    }

    /// The accept hook classifies a canonical URI and advances the metric.
    #[test]
    fn accept_canonical_result_uri_records_canonical() {
        let before = crate::metrics::result_uri_accept_total()
            .with_label_values(&["canonical"])
            .get();
        let result = serde_json::json!({
            "status": "completed",
            "reference": { "uri": "noetl://default/default/results/7/start/0/0/1" }
        });
        accept_canonical_result_uri(&result, 7);
        let after = crate::metrics::result_uri_accept_total()
            .with_label_values(&["canonical"])
            .get();
        assert_eq!(after, before + 1, "canonical accept must increment the counter");
    }

    /// A malformed `reference.uri` is counted `malformed` and does NOT panic /
    /// fail — Phase A introduces no new failure path.
    #[test]
    fn accept_canonical_result_uri_records_malformed_without_failing() {
        let before = crate::metrics::result_uri_accept_total()
            .with_label_values(&["malformed"])
            .get();
        let result = serde_json::json!({
            "status": "completed",
            "reference": { "uri": "noetl://t/p/results" } // too few segments
        });
        accept_canonical_result_uri(&result, 9);
        let after = crate::metrics::result_uri_accept_total()
            .with_label_values(&["malformed"])
            .get();
        assert_eq!(after, before + 1, "malformed accept must increment the counter");
    }

    // ── RFC #115 Phase 3 — chain-walk state builder ──────────────────────────

    /// Minimal `Event` for the chain-walk ordering tests.
    fn ev(event_id: i64, prev: Option<i64>, event_type: &str) -> crate::db::models::Event {
        // `prev` mirrors the chain link a real walk follows; the builder doesn't
        // store it on `Event` (it's read off the row in `fetch_chain_node`), so
        // it's only used here to reconstruct the walk order.
        let _ = prev;
        crate::db::models::Event {
            id: event_id,
            execution_id: 42,
            catalog_id: 7,
            event_id,
            parent_event_id: None,
            parent_execution_id: None,
            event_type: event_type.to_string(),
            node_id: None,
            node_name: Some("start".to_string()),
            node_type: None,
            status: "ok".to_string(),
            context: None,
            meta: None,
            result: None,
            worker_id: None,
            attempt: None,
            created_at: chrono::DateTime::from_timestamp(event_id, 0).unwrap(),
        }
    }

    #[test]
    fn chain_has_genesis_detects_playbook_started() {
        let with = vec![ev(1, None, "playbook_started"), ev(2, Some(1), "command.issued")];
        assert!(chain_has_genesis(&with));
        // A restart-spanning tail (no genesis) must be rejected so the builder
        // falls back to event-scan rather than building a partial state.
        let tail = vec![ev(5, None, "command.completed"), ev(6, Some(5), "command.issued")];
        assert!(!chain_has_genesis(&tail));
    }

    #[test]
    fn canonicalize_strips_timestamps_and_sorts_arrays() {
        // Two logically-identical states that differ only in (a) the order of a
        // HashSet-backed array and (b) the non-deterministic wall-clock fields
        // must canonicalize equal — that's what makes the parity check robust to
        // serialization order + the `Utc::now()` created_at fallback.
        let a = serde_json::json!({
            "state": "InProgress",
            "started_at": "2026-06-19T22:56:40Z",
            "steps": { "s1": { "status": "done", "completed_at": "2026-06-19T22:56:41Z",
                               "cursor_completed": ["b", "a", "c"] } }
        });
        let b = serde_json::json!({
            "state": "InProgress",
            "started_at": "2026-06-19T23:03:43Z",   // different clock read
            "steps": { "s1": { "status": "done", "completed_at": "2026-06-19T23:03:44Z",
                               "cursor_completed": ["c", "b", "a"] } } // different set order
        });
        assert_eq!(
            canonicalize_state_json(&a),
            canonicalize_state_json(&b),
            "states differing only in set order + wall-clock fields must canonicalize equal"
        );
        // A real logical difference still survives canonicalization.
        let c = serde_json::json!({ "state": "Completed", "steps": {} });
        assert_ne!(canonicalize_state_json(&a), canonicalize_state_json(&c));
    }

    #[test]
    fn chain_walk_collection_matches_event_scan_order() {
        // Parity by construction: the event-scan path loads events `ORDER BY
        // event_id ASC`; the chain walk collects them head→root (descending) then
        // sorts by `event_id` ASC.  Both must hand `from_events` the identical
        // sequence → identical state.  Here we simulate both orderings and assert
        // the sorted sequences are equal (the builder calls the SAME from_events).
        let scan_order = [
            ev(10, None, "playbook_started"),
            ev(20, Some(10), "command.issued"),
            ev(30, Some(20), "command.completed"),
            ev(40, Some(30), "command.issued"),
        ];
        // The walk collects head(40)→root(10): reverse of scan order.
        let mut walk_collected: Vec<_> = scan_order.iter().rev().cloned().collect();
        walk_collected.sort_by_key(|e| e.event_id);
        let scan_ids: Vec<i64> = scan_order.iter().map(|e| e.event_id).collect();
        let walk_ids: Vec<i64> = walk_collected.iter().map(|e| e.event_id).collect();
        assert_eq!(
            scan_ids, walk_ids,
            "chain-walk sorted order must equal event-scan ORDER BY event_id ASC"
        );

        // And the states `from_events` produces from each ordering are identical.
        let scan_state =
            crate::engine::state::WorkflowState::from_events(&scan_order.iter().map(Into::into).collect::<Vec<_>>());
        let walk_state =
            crate::engine::state::WorkflowState::from_events(&walk_collected.iter().map(Into::into).collect::<Vec<_>>());
        assert_eq!(
            serde_json::to_value(&scan_state).unwrap(),
            serde_json::to_value(&walk_state).unwrap(),
            "chain-walk state must equal event-scan state (parity by construction)"
        );
    }

    #[test]
    fn with_ref_accessors_injects_locator_keys() {
        // #115 Phase 1: keep_refs surfaces `_ref`/`_store`/`_uri` on the bounded
        // summary so `{{ step._ref }}` / `{{ step._store }}` resolve without bulk.
        let extracted = serde_json::json!({ "status": "ok", "count": 500 });
        let reference = serde_json::json!({
            "ref": "noetl://execution/1/result/start/9",
            "store": "kv",
            "uri": "noetl://t/p/results/1/start/0/0/1",
            "extracted": {},
        });
        let out = with_ref_accessors(extracted, Some(&reference));
        assert_eq!(out["_ref"], serde_json::json!("noetl://execution/1/result/start/9"));
        assert_eq!(out["_store"], serde_json::json!("kv"));
        assert_eq!(out["_uri"], serde_json::json!("noetl://t/p/results/1/start/0/0/1"));
        // The summary scalars are preserved.
        assert_eq!(out["status"], serde_json::json!("ok"));
        assert_eq!(out["count"], serde_json::json!(500));
    }

    #[test]
    fn with_ref_accessors_preserves_existing_and_skips_non_object() {
        // A worker-provided inline `_ref` wins; a non-object summary is untouched.
        let extracted = serde_json::json!({ "_ref": "worker-set", "x": 1 });
        let reference = serde_json::json!({ "ref": "server-set" });
        let out = with_ref_accessors(extracted, Some(&reference));
        assert_eq!(out["_ref"], serde_json::json!("worker-set"));
        let scalar = serde_json::json!("just-a-string");
        assert_eq!(with_ref_accessors(scalar.clone(), Some(&reference)), scalar);
    }

    fn test_request_skeleton() -> EventRequest {
        EventRequest {
            execution_id: "123".to_string(),
            step: "step1".to_string(),
            event_type: "step.exit".to_string(),
            payload: serde_json::json!({}),
            meta: None,
            worker_id: None,
            result_kind: "data".to_string(),
            result_uri: None,
            event_ids: None,
            actionable: true,
            informative: true,
            event_id: None,
            status: None,
            created_at: None,
        }
    }

    #[test]
    fn test_event_request_defaults() {
        // New canonical field name `event_type`.
        let json = r#"{"execution_id": "123", "step": "step1", "event_type": "step.enter"}"#;
        let request: EventRequest = serde_json::from_str(json).unwrap();

        assert_eq!(request.event_type, "step.enter");
        assert_eq!(request.result_kind, "data");
        assert!(request.actionable);
        assert!(request.informative);
        assert!(request.event_id.is_none());
        assert!(request.status.is_none());
        assert!(request.created_at.is_none());
    }

    #[test]
    fn test_legacy_name_alias_deserializes_into_event_type() {
        // R-1.2 PR-EE-2 back-compat: pre-PR-EE worker / CLI
        // clients send `name` instead of `event_type`.  The
        // alias means they deserialize cleanly without a server
        // restart.
        let json = r#"{"execution_id": "123", "step": "step1", "name": "step.exit"}"#;
        let request: EventRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.event_type, "step.exit");
    }

    #[test]
    fn test_context_alias_deserializes_into_payload() {
        // Executor producers send the field as `context`; pre-PR
        // clients send `payload`.  Both deserialize into the same
        // field on the server side.
        let json = r#"{
            "execution_id": "123",
            "step": "step1",
            "event_type": "step.exit",
            "context": {"result": 42}
        }"#;
        let request: EventRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.payload["result"], 42);
    }

    #[test]
    fn test_new_optional_fields_accept_executor_event_shape() {
        // Wire format matching noetl-executor 0.3.1 ExecutorEvent:
        // event_id (snowflake as String), status, created_at, plus
        // the `context` alias.
        let json = r#"{
            "execution_id": "478775660589088776",
            "event_type": "command.completed",
            "step": "fetch_calendar",
            "status": "COMPLETED",
            "created_at": "2026-05-31T03:14:15Z",
            "context": {"items": 42},
            "event_id": "478775660589088777",
            "worker_id": "worker-prod-7",
            "meta": {"attempts": 2}
        }"#;
        let request: EventRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.event_type, "command.completed");
        assert_eq!(request.event_id.as_deref(), Some("478775660589088777"));
        assert_eq!(request.status.as_deref(), Some("COMPLETED"));
        assert_eq!(request.worker_id.as_deref(), Some("worker-prod-7"));
        assert!(request.created_at.is_some());
    }

    #[test]
    fn test_event_request_accepts_integer_execution_id() {
        // noetl/ai-meta#55 — the Rust worker emits
        // `noetl-events::ExecutorEvent` whose `execution_id` is
        // `i64`, so the wire shape is a JSON integer.  Without the
        // `deserialize_string_or_i64` adapter, strict serde rejects
        // it with "invalid type: integer, expected a string", and
        // every Rust-on-both-ends event emission fails.
        let json = r#"{
            "execution_id": 321079436235509760,
            "step": "start",
            "event_type": "step.start"
        }"#;
        let request: EventRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.execution_id, "321079436235509760");
        assert_eq!(request.step, "start");
    }

    #[test]
    fn test_event_request_accepts_string_execution_id() {
        // Legacy browser-client wire shape (snowflake as JSON
        // string).  Confirms the lax decoder didn't regress the
        // documented `String` wire format that EE-2 / EE-4 kept
        // for browser JSON-number precision.
        let json = r#"{
            "execution_id": "321079436235509760",
            "step": "start",
            "event_type": "step.start"
        }"#;
        let request: EventRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.execution_id, "321079436235509760");
    }

    #[test]
    fn test_event_request_accepts_integer_event_id() {
        // event_id is optional + `Option<String>`; the worker emits
        // `Option<i64>` from `noetl-events`.  Same drift as
        // execution_id, same fix.
        let json = r#"{
            "execution_id": "1",
            "step": "s",
            "event_type": "e",
            "event_id": 478775660589088777
        }"#;
        let request: EventRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.event_id.as_deref(), Some("478775660589088777"));
    }

    #[test]
    fn test_event_request_event_id_null_is_none() {
        // Explicit null should land as None.  Optional decoder
        // sanity check.
        let json = r#"{
            "execution_id": "1",
            "step": "s",
            "event_type": "e",
            "event_id": null
        }"#;
        let request: EventRequest = serde_json::from_str(json).unwrap();
        assert!(request.event_id.is_none());
    }

    #[test]
    fn test_batch_event_request_accepts_integer_execution_id() {
        // BatchEventRequest is the second worker→server inbound
        // type with execution_id on it (the worker uses it for
        // batched event emission).  Same drift as the per-event
        // shape; same lax decoder.
        let json = r#"{
            "execution_id": 321079436235509760,
            "worker_id": "worker-rust-pool-0",
            "events": [
                {
                    "step": "start",
                    "event_type": "step.start"
                }
            ]
        }"#;
        let request: BatchEventRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.execution_id, "321079436235509760");
        assert_eq!(request.events.len(), 1);
    }

    #[test]
    fn test_event_request_rejects_garbage_execution_id() {
        // The lax decoder should still reject obviously-bogus
        // shapes (arrays, objects, floats) — only `string` and
        // `integer` are valid for a snowflake id field.  Floats
        // are rejected because the precision loss makes them
        // ambiguous (a Pythonish `12345.0` decodes via visit_f64
        // which we don't implement).
        let bogus_shapes = [
            r#"{"execution_id": [1,2,3], "step": "s", "event_type": "e"}"#,
            r#"{"execution_id": {"id": 1}, "step": "s", "event_type": "e"}"#,
        ];
        for json in bogus_shapes {
            let result: std::result::Result<EventRequest, _> = serde_json::from_str(json);
            assert!(result.is_err(), "Expected reject for {}", json);
        }
    }

    // build_result_object — constraint-compliant shape per
    // noetl/server#29.  The DB constraint allows only
    // `status` (required string), `reference` (optional object),
    // `context` (optional object); nothing else.

    #[test]
    fn test_build_result_object_data() {
        let request = EventRequest {
            payload: serde_json::json!({"output": "success"}),
            ..test_request_skeleton()
        };

        let result = build_result_object(&request, "COMPLETED");
        assert_eq!(result["status"], "COMPLETED");
        assert_eq!(result["context"]["output"], "success");
        // No disallowed top-level keys.
        assert!(result.get("kind").is_none());
        assert!(result.get("data").is_none());
        assert!(result.get("reference").is_none());
    }

    #[test]
    fn test_build_result_object_data_with_null_payload_omits_context() {
        let request = EventRequest {
            payload: serde_json::Value::Null,
            ..test_request_skeleton()
        };
        let result = build_result_object(&request, "STARTED");
        assert_eq!(result["status"], "STARTED");
        assert!(
            result.get("context").is_none(),
            "context must not be set when payload is non-object: {result}"
        );
    }

    #[test]
    fn test_build_result_object_data_with_primitive_payload_omits_context() {
        // Constraint: when present, context must be an object.
        // Wrap-it-or-omit-it — we chose omit.
        let request = EventRequest {
            payload: serde_json::json!("just a string"),
            ..test_request_skeleton()
        };
        let result = build_result_object(&request, "RUNNING");
        assert!(result.get("context").is_none());
    }

    #[test]
    fn test_build_result_object_ref() {
        let request = EventRequest {
            result_kind: "ref".to_string(),
            result_uri: Some("gs://bucket/path/to/result".to_string()),
            ..test_request_skeleton()
        };

        let result = build_result_object(&request, "COMPLETED");
        assert_eq!(result["status"], "COMPLETED");
        let reference = &result["reference"];
        assert_eq!(reference["store_tier"], "gcs");
        assert_eq!(reference["logical_uri"], "gs://bucket/path/to/result");
        // No disallowed top-level keys.
        assert!(result.get("kind").is_none());
        assert!(result.get("store_tier").is_none());
        assert!(result.get("logical_uri").is_none());
    }

    #[test]
    fn test_build_result_object_refs() {
        let request = EventRequest {
            result_kind: "refs".to_string(),
            event_ids: Some(vec![100, 200, 300]),
            ..test_request_skeleton()
        };

        let result = build_result_object(&request, "COMPLETED");
        assert_eq!(result["status"], "COMPLETED");
        let reference = &result["reference"];
        assert_eq!(reference["event_ids"][0], 100);
        assert_eq!(reference["total_parts"], 3);
        assert!(
            result.get("event_ids").is_none(),
            "event_ids should be nested under reference, not top-level"
        );
    }

    #[test]
    fn test_build_result_object_constraint_top_level_keys_only() {
        // The DB constraint allows ONLY {status, reference, context}
        // at the top level.  Walk all output shapes and assert none
        // emit anything else.
        let allowed: std::collections::HashSet<&str> =
            ["status", "reference", "context"].iter().copied().collect();

        let cases: Vec<(&str, EventRequest)> = vec![
            (
                "data with object payload",
                EventRequest {
                    payload: serde_json::json!({"k": "v"}),
                    ..test_request_skeleton()
                },
            ),
            (
                "data with null payload",
                EventRequest {
                    payload: serde_json::Value::Null,
                    ..test_request_skeleton()
                },
            ),
            (
                "ref",
                EventRequest {
                    result_kind: "ref".to_string(),
                    result_uri: Some("gs://foo".to_string()),
                    ..test_request_skeleton()
                },
            ),
            (
                "refs",
                EventRequest {
                    result_kind: "refs".to_string(),
                    event_ids: Some(vec![1, 2]),
                    ..test_request_skeleton()
                },
            ),
        ];
        for (label, req) in cases {
            let r = build_result_object(&req, "OK");
            let obj = r.as_object().expect("result must be object");
            for k in obj.keys() {
                assert!(
                    allowed.contains(k.as_str()),
                    "[{label}] disallowed top-level key: {k} (full result: {r})"
                );
            }
            assert_eq!(r["status"], "OK", "[{label}] status must be present");
        }
    }

    #[test]
    fn test_batch_event_item_legacy_name_alias() {
        let json = r#"{"step": "s", "name": "call.done", "payload": {}}"#;
        let item: BatchEventItem = serde_json::from_str(json).unwrap();
        assert_eq!(item.event_type, "call.done");
    }

    #[test]
    fn test_event_response_serialization() {
        let response = EventResponse {
            status: "ok".to_string(),
            event_id: 12345,
            commands_generated: 2,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("ok"));
        assert!(json.contains("12345"));
    }

    #[test]
    fn test_command_response_serialization() {
        let response = CommandResponse {
            execution_id: 12345,
            node_id: "step1".to_string(),
            node_name: "step1".to_string(),
            action: "python".to_string(),
            context: serde_json::json!({"tool_config": {}}),
            meta: serde_json::json!({"attempt": 1}),
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("step1"));
        assert!(json.contains("python"));
    }

    // ---- EE-4 (noetl/ai-meta#49) wire-compat with noetl-events --------
    //
    // The server's `EventRequest` and the canonical
    // `noetl_events::ExecutorEvent` share a subset of fields that
    // make up the wire envelope every NoETL Rust producer emits.
    // The four tests below pin the round-trip semantics so a future
    // change to either type that breaks compat fails the build
    // here instead of in a kind-validation cycle.

    #[test]
    fn ee4_executor_event_converts_into_event_request() {
        let executor_event = noetl_events::ExecutorEvent {
            execution_id: 478775660589088776,
            event_type: "command.completed".to_string(),
            step: "fetch_calendar".to_string(),
            status: "COMPLETED".to_string(),
            created_at: chrono::DateTime::parse_from_rfc3339("2026-05-31T03:14:15Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
            context: serde_json::json!({"items": 42}),
            event_id: Some(478775660589088777),
            worker_id: Some("worker-prod-7".to_string()),
            meta: Some(serde_json::json!({"attempts": 2})),
        };
        let req: EventRequest = executor_event.clone().into();
        // String wire format for browser precision.
        assert_eq!(req.execution_id, "478775660589088776");
        assert_eq!(req.event_id.as_deref(), Some("478775660589088777"));
        // Shared subset round-trips field-for-field.
        assert_eq!(req.event_type, executor_event.event_type);
        assert_eq!(req.step, executor_event.step);
        assert_eq!(req.status.as_deref(), Some(executor_event.status.as_str()));
        assert_eq!(req.created_at, Some(executor_event.created_at));
        assert_eq!(req.payload, executor_event.context);
        assert_eq!(req.worker_id, executor_event.worker_id);
        assert_eq!(req.meta, executor_event.meta);
        // Server-only fields take handler defaults.
        assert_eq!(req.result_kind, "data");
        assert!(req.result_uri.is_none());
        assert!(req.event_ids.is_none());
        assert!(req.actionable);
        assert!(req.informative);
    }

    #[test]
    fn ee4_event_request_converts_into_executor_event() {
        let req = EventRequest {
            execution_id: "478775660589088776".to_string(),
            step: "fetch_calendar".to_string(),
            event_type: "command.completed".to_string(),
            payload: serde_json::json!({"items": 42}),
            meta: Some(serde_json::json!({"attempts": 2})),
            worker_id: Some("worker-prod-7".to_string()),
            result_kind: "data".to_string(),
            result_uri: None,
            event_ids: None,
            actionable: true,
            informative: true,
            event_id: Some("478775660589088777".to_string()),
            status: Some("COMPLETED".to_string()),
            created_at: Some(
                chrono::DateTime::parse_from_rfc3339("2026-05-31T03:14:15Z")
                    .unwrap()
                    .with_timezone(&chrono::Utc),
            ),
        };
        let ev: noetl_events::ExecutorEvent =
            (&req).try_into().expect("convert with explicit fields");
        assert_eq!(ev.execution_id, 478775660589088776_i64);
        assert_eq!(ev.event_id, Some(478775660589088777_i64));
        assert_eq!(ev.status, "COMPLETED");
        assert_eq!(ev.created_at, req.created_at.unwrap());
        assert_eq!(ev.context, req.payload);
        assert_eq!(ev.worker_id, req.worker_id);
        assert_eq!(ev.meta, req.meta);
    }

    #[test]
    fn ee4_try_from_event_request_fills_defaults_for_missing_status_and_created_at() {
        // Producers that don't stamp `status` / `created_at` are
        // valid on the wire; the conversion must apply the same
        // fallbacks the handler uses, so callers building an
        // `ExecutorEvent` for downstream emit don't see surprises.
        let mut req = test_request_skeleton();
        req.event_type = "command.completed".to_string();
        req.status = None;
        req.created_at = None;
        let ev: noetl_events::ExecutorEvent = (&req).try_into().expect("convert with defaults");
        assert_eq!(ev.status, "COMPLETED"); // name-derived fallback
                                            // created_at falls back to now(); just assert it's non-zero
                                            // and recent enough to be sane.
        let age = chrono::Utc::now() - ev.created_at;
        assert!(age.num_seconds() >= 0 && age.num_seconds() < 60);
    }

    #[test]
    fn ee4_try_from_event_request_rejects_non_numeric_execution_id() {
        // The wire shape is "stringified i64".  Anything else is a
        // bug at the producer; the conversion surfaces it instead of
        // silently dropping the event into the log with execution_id=0.
        let req = EventRequest {
            execution_id: "not-a-number".to_string(),
            ..test_request_skeleton()
        };
        let err = noetl_events::ExecutorEvent::try_from(&req).unwrap_err();
        assert!(
            err.to_string().contains("execution_id"),
            "error should mention the field name: {err}"
        );
    }

    // --- noetl/ai-meta#113: offloaded OrchestrationResult recovery ---

    #[test]
    fn find_output_b64_walks_nested_inline_payload() {
        // Inline (under-budget) drive: `output_b64` nests inside the
        // call.done result envelope.
        let payload = serde_json::json!({
            "command_id": "c1",
            "result": { "status": "COMPLETED", "context": { "data": {
                "output_b64": "aGVsbG8=", "flush": { "errors": [] }
            }}}
        });
        assert_eq!(find_output_b64(&payload), Some("aGVsbG8="));
    }

    #[test]
    fn find_noetl_ref_finds_durable_ref_when_inline_absent() {
        // Offloaded (over-budget) drive: no `output_b64`, only the durable
        // `reference.ref` URI — exactly the noetl/ai-meta#113 shape the worker
        // emits from `build_call_done_result`'s durable path.
        let payload = serde_json::json!({
            "command_id": "c1",
            "result": {
                "status": "COMPLETED",
                "context": { "data": { "_ref": "noetl://execution/9/result/__orchestrate__/1" } },
                "reference": {
                    "kind": "result_ref",
                    "ref": "noetl://execution/9/result/__orchestrate__/2",
                    "store": "db"
                }
            }
        });
        assert_eq!(find_output_b64(&payload), None);
        // Matches the durable `ref` (not the `_ref` navigation hint).
        assert_eq!(
            find_noetl_ref(&payload),
            Some("noetl://execution/9/result/__orchestrate__/2")
        );
    }

    #[test]
    fn find_noetl_ref_ignores_non_noetl_and_shm_only_reference() {
        // Degraded shm-only path carries an `arrow_ipc` hint with no resolvable
        // `noetl://` ref — the server can't read another node's shm, so it must
        // not mistake a non-URI string for a durable ref.
        let payload = serde_json::json!({
            "result": { "status": "COMPLETED", "reference": {
                "kind": "arrow_ipc", "shm_name": "noetl-shm-abc", "byte_length": 42
            }}
        });
        assert_eq!(find_noetl_ref(&payload), None);
    }

    #[test]
    fn decode_orchestration_result_round_trips_b64_json() {
        use base64::Engine;
        // A minimal OrchestrationResult JSON, base64-wrapped exactly as the
        // worker wraps the plug-in's output bytes.
        let or_json = serde_json::json!({
            "state": "completed",
            "commands": [],
            "should_complete": true,
            "events_to_emit": []
        });
        let b64 = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_vec(&or_json).unwrap());
        let decoded = decode_orchestration_result(Some(&b64))
            .expect("valid base64 + JSON decodes to an OrchestrationResult");
        assert!(decoded.should_complete);
        assert!(decoded.commands.is_empty());

        // None / garbage / non-result JSON → None (the error-envelope path).
        assert!(decode_orchestration_result(None).is_none());
        assert!(decode_orchestration_result(Some("not base64!!!")).is_none());
        let not_a_result =
            base64::engine::general_purpose::STANDARD.encode(b"{\"unexpected\":true}");
        assert!(decode_orchestration_result(Some(&not_a_result)).is_none());
    }

    #[test]
    fn decode_orchestrate_error_extracts_drive_error_envelope() {
        // noetl/ai-meta#123: the off-server drive returns `{"error": "..."}` when
        // `evaluate_state` fails (e.g. a non-iterable loop `in:`).  The server
        // must recognise it so it can terminate the run as FAILED instead of
        // silently no-op'ing on an undecodable result.
        use base64::Engine;
        let envelope = serde_json::json!({
            "error": "evaluate_state: Validation error: loop step 'process': \
                      Loop expression '{{ workload.batch_slots }}' did not evaluate to an \
                      iterable (got null)"
        });
        let b64 = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_vec(&envelope).unwrap());
        let msg = decode_orchestrate_error(Some(&b64)).expect("envelope decodes to its message");
        assert!(msg.contains("did not evaluate to an iterable"), "{msg}");
        assert!(msg.contains("process"), "{msg}");

        // A real OrchestrationResult is NOT an error envelope → None (stays on the
        // success path, never mis-classified as a failure).
        let or_json = serde_json::json!({
            "state": "completed", "commands": [], "should_complete": true, "events_to_emit": []
        });
        let or_b64 = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_vec(&or_json).unwrap());
        assert!(decode_orchestrate_error(Some(&or_b64)).is_none());

        // Transient decode misses (None / bad base64 / empty error) stay on the
        // benign re-drive path, NOT the terminal-fail path.
        assert!(decode_orchestrate_error(None).is_none());
        assert!(decode_orchestrate_error(Some("not base64!!!")).is_none());
        let empty_err = base64::engine::general_purpose::STANDARD.encode(b"{\"error\":\"\"}");
        assert!(decode_orchestrate_error(Some(&empty_err)).is_none());
    }
}
