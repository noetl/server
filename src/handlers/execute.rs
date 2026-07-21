//! Execution API handlers.
//!
//! Handles playbook execution start and status endpoints.

use std::collections::HashMap;

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// Request to start playbook execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteRequest {
    /// Playbook catalog path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Catalog ID (alternative to path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog_id: Option<i64>,
    /// Input payload/workload.
    #[serde(default, alias = "workload")]
    pub payload: HashMap<String, serde_json::Value>,
    /// Parent execution ID (for nested executions).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_execution_id: Option<i64>,
    /// Dedicated worker pool / command segment for this whole execution
    /// (noetl/ai-meta#90 Phase 2).  When set, every command of this
    /// execution publishes to `noetl.commands.<execution_pool>.<eid>`
    /// instead of the path-derived default (`system` / `shared`).  The
    /// subscription continuous runtime passes the subscription's
    /// `dispatch.execution_pool` (or a header-directive `x-noetl-pool`
    /// override) so the firehose lands on an isolated segment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_pool: Option<String>,
    /// W3C distributed-trace context to stamp onto this execution's events
    /// (`meta.trace`) and propagate to its commands + child executions
    /// (noetl/ai-meta#90 Phase 2, RFC §7.4).  Shape:
    /// `{ "traceparent": "...", "tracestate": "...", "baggage": {...} }`.
    /// `execution_id` stays the primary NoETL trace key; this is the
    /// external join.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<serde_json::Value>,
    /// Opt-in exactly-once dedup (noetl/ai-meta#90 Phase 7, RFC §10 OQ1).
    /// When present, the server consults `noetl.subscription_dedup` scoped by
    /// `parent_execution_id` (the subscription) and collapses a duplicate
    /// delivery (same `dedup.key` within `dedup.window_secs`) to a single
    /// execution — no second `playbook_started`, no second command fan-out.
    /// Absent → no dedup (the default; at IoT volume a DB write per message
    /// is too costly, so dedup is opt-in per subscription).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dedup: Option<DedupSpec>,
}

/// Opt-in dedup directive carried on an execute request (noetl/ai-meta#90
/// Phase 7).  The continuous subscription runtime stamps this only when the
/// `kind: Subscription` declares `dedup.enabled: true`; `key` is the resolved
/// `idempotency_key` header directive, falling back to the source `message_id`
/// (RFC §10 OQ8).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DedupSpec {
    /// The idempotency key to dedup on (idempotency_key directive → message_id).
    pub key: String,
    /// Window in seconds; a duplicate older than this is treated as fresh.
    #[serde(default = "default_dedup_window_secs")]
    pub window_secs: u64,
}

fn default_dedup_window_secs() -> u64 {
    crate::db::queries::subscription_dedup::DEFAULT_WINDOW_SECS
}

/// Per-execution command routing resolved once and threaded through every
/// `persist_engine_command` call so the initial command (this handler) and
/// every orchestrator follow-up (`events::trigger_orchestrator`) land on the
/// same dedicated pool segment and carry the same trace context.
///
/// It is persisted on the `playbook_started` event `meta` so the orchestrator
/// — which rebuilds state from the event log on each pass — can recover it.
#[derive(Debug, Clone, Default)]
pub(crate) struct CommandRouting {
    /// Dedicated pool / command-segment override for the whole execution.
    pub pool: Option<String>,
    /// W3C trace context (opaque JSON) propagated onto events + commands.
    pub trace: Option<serde_json::Value>,
}

impl CommandRouting {
    /// Recover the routing the `/api/execute` caller supplied from a
    /// `playbook_started` event's `meta` JSON (used by the orchestrator).
    pub(crate) fn from_started_meta(meta: &serde_json::Value) -> CommandRouting {
        CommandRouting {
            pool: meta
                .get("execution_pool")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            trace: meta.get("trace").filter(|v| !v.is_null()).cloned(),
        }
    }
}

impl ExecuteRequest {
    /// Validate the request.
    pub fn validate(&self) -> Result<(), String> {
        if self.path.is_none() && self.catalog_id.is_none() {
            return Err("Either 'path' or 'catalog_id' must be provided".to_string());
        }
        Ok(())
    }
}

/// Response for starting execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteResponse {
    /// Execution ID.
    pub execution_id: String,
    /// Execution status.
    pub status: String,
    /// Number of commands generated.
    pub commands_generated: i32,
}

/// Start playbook execution.
///
/// POST /api/execute
///
/// Creates playbook_started event and emits command.issued events.
/// All state is derived from events - no separate workflow/transition tables.
pub async fn execute(
    State(state): State<AppState>,
    Json(request): Json<ExecuteRequest>,
) -> Result<Json<ExecuteResponse>, AppError> {
    let outcome = execute_one(&state, request, "single").await?;
    Ok(Json(outcome.into_response()))
}

/// Hard cap on items in one `POST /api/execute/batch` call.  A runaway-loop
/// backstop set far above any real subscription batch (the runtime caps its
/// own batch at `RUNTIME_BATCH_DEFAULT`-ish); a larger request is rejected
/// rather than silently truncated.
const MAX_BATCH_ITEMS: usize = 1000;

/// Batch execute request (noetl/ai-meta#90 Phase 7, RFC §10 OQ12/#13).
#[derive(Debug, Deserialize)]
pub struct BatchExecuteRequest {
    /// One full execute request per message.  Each carries its own
    /// `path`/`payload`/`execution_pool`/`trace`/`parent_execution_id`/`dedup`,
    /// so the directive-resolved per-message routing + trace propagation +
    /// dedup are preserved inside the batch — a batch is N independent
    /// executions in one HTTP round-trip, not one shared execution.
    pub executions: Vec<ExecuteRequest>,
}

/// Per-item result in a batch response.  Partial failure is first-class: one
/// bad item yields an `error` entry, the rest still create executions.
#[derive(Debug, Clone, Serialize)]
pub struct BatchItemResult {
    /// Position in the request `executions` array — lets the caller correlate
    /// each result back to the message it submitted.
    pub index: usize,
    /// `"started"` | `"duplicate"` | `"error"`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_id: Option<String>,
    #[serde(default)]
    pub commands_generated: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Batch execute response — per-item results plus a summary so the caller can
/// assert N→N without walking every entry.
#[derive(Debug, Serialize)]
pub struct BatchExecuteResponse {
    pub count: usize,
    pub started: usize,
    pub duplicates: usize,
    pub failed: usize,
    pub results: Vec<BatchItemResult>,
}

/// Start N executions in one request.
///
/// POST /api/execute/batch
///
/// Each element is processed independently through [`execute_one`] — the same
/// wire-format path the single endpoint uses — so per-message event
/// traceability, the directive/pool routing, the W3C trace propagation, and
/// the opt-in dedup window all behave exactly as for a one-at-a-time dispatch.
/// **Partial failure is contained**: a single item that fails to create an
/// execution becomes an `error` result at its index; the rest still run.  The
/// data-access boundary is intact — the server still owns every DB write; the
/// runtime only collapses N HTTP round-trips into one.
pub async fn execute_batch(
    State(state): State<AppState>,
    Json(request): Json<BatchExecuteRequest>,
) -> Result<Json<BatchExecuteResponse>, AppError> {
    let n = request.executions.len();
    if n == 0 {
        return Err(AppError::Validation(
            "execute batch requires a non-empty 'executions' array".to_string(),
        ));
    }
    if n > MAX_BATCH_ITEMS {
        return Err(AppError::Validation(format!(
            "execute batch size {} exceeds the {} cap",
            n, MAX_BATCH_ITEMS
        )));
    }

    let span = tracing::info_span!("execute.batch", batch_size = n);
    let _guard = span.enter();
    crate::metrics::record_execute_batch_size(n);

    let mut results = Vec::with_capacity(n);
    let mut started = 0usize;
    let mut duplicates = 0usize;
    let mut failed = 0usize;

    for (index, req) in request.executions.into_iter().enumerate() {
        match execute_one(&state, req, "batch").await {
            Ok(outcome) => {
                if outcome.status == "duplicate" {
                    duplicates += 1;
                } else {
                    started += 1;
                }
                results.push(BatchItemResult {
                    index,
                    status: outcome.status.to_string(),
                    execution_id: Some(outcome.execution_id.to_string()),
                    commands_generated: outcome.commands_generated,
                    error: None,
                });
            }
            Err(e) => {
                failed += 1;
                crate::metrics::record_execute_outcome("batch", "error");
                tracing::warn!(index, error = %e, "batch item failed (continuing)");
                results.push(BatchItemResult {
                    index,
                    status: "error".to_string(),
                    execution_id: None,
                    commands_generated: 0,
                    error: Some(e.to_string()),
                });
            }
        }
    }

    info!(
        batch_size = n,
        started, duplicates, failed, "execute batch complete"
    );

    Ok(Json(BatchExecuteResponse {
        count: n,
        started,
        duplicates,
        failed,
        results,
    }))
}

/// The outcome of creating (or deduplicating) one execution.  Shared by the
/// single (`/api/execute`) and batch (`/api/execute/batch`) entry points.
#[derive(Debug, Clone)]
pub(crate) struct ExecuteOutcome {
    pub execution_id: i64,
    /// `"started"` for a fresh execution, `"duplicate"` when the dedup window
    /// collapsed this delivery onto an existing execution.
    pub status: &'static str,
    pub commands_generated: i32,
}

impl ExecuteOutcome {
    fn into_response(self) -> ExecuteResponse {
        ExecuteResponse {
            execution_id: self.execution_id.to_string(),
            status: self.status.to_string(),
            commands_generated: self.commands_generated,
        }
    }
}

/// Create (or dedup) one execution.  Factored out of the `/api/execute`
/// handler so the batch endpoint can reuse the exact same wire-format logic
/// per item (noetl/ai-meta#90 Phase 7).  `entry` is the metrics label
/// (`"single"` | `"batch"`).
pub(crate) async fn execute_one(
    state: &AppState,
    request: ExecuteRequest,
    entry: &'static str,
) -> Result<ExecuteOutcome, AppError> {
    // Validate request
    request.validate().map_err(AppError::Validation)?;

    debug!(
        "Execute request: path={:?}, catalog_id={:?}",
        request.path, request.catalog_id
    );

    // Generate execution_id via the application-side snowflake
    // generator (Phase F R1.5 of noetl/ai-meta#49).  ID is
    // available before any I/O so spans + metrics can use it
    // immediately, retries stay idempotent, and the opt-in dedup
    // window (below) can reserve it before any expensive work.
    let execution_id = state.snowflake.generate()?;

    // Opt-in exactly-once dedup window (noetl/ai-meta#90 Phase 7, RFC §10
    // OQ1).  Checked *before* catalog resolution + parse so a duplicate is
    // cheap — it never touches the catalog, never emits an event, never fans
    // out a command.  Scope is the subscription (parent_execution_id); the
    // claim is race-safe (INSERT … ON CONFLICT) so two replicas can't both
    // create an execution for the same key.
    if let Some(dedup) = request.dedup.as_ref() {
        let scope = request.parent_execution_id.unwrap_or(0);
        use crate::db::queries::subscription_dedup::{claim, DedupOutcome};
        match claim(
            state.pools.cluster(),
            scope,
            &dedup.key,
            dedup.window_secs,
            execution_id,
        )
        .await?
        {
            DedupOutcome::Duplicate {
                existing_execution_id,
            } => {
                emit_deduplicated_event(
                    state,
                    scope,
                    &dedup.key,
                    existing_execution_id,
                    execution_id,
                )
                .await;
                crate::metrics::record_execute_outcome(entry, "duplicate");
                info!(
                    subscription_id = scope,
                    existing_execution_id,
                    suppressed_execution_id = execution_id,
                    dedup_key = %dedup.key,
                    "dedup window collapsed a duplicate delivery to the existing execution"
                );
                return Ok(ExecuteOutcome {
                    execution_id: existing_execution_id,
                    status: "duplicate",
                    commands_generated: 0,
                });
            }
            DedupOutcome::Fresh => { /* reserved execution_id; proceed */ }
        }
    }

    // Resolve catalog entry
    let (catalog_id, path) = resolve_catalog(state, &request).await?;

    info!(
        "Starting execution for path={}, catalog_id={}",
        path, catalog_id
    );

    // Get playbook from catalog
    let playbook_yaml = get_playbook_yaml(state, catalog_id).await?;

    // Parse playbook
    let playbook = crate::playbook::parser::parse_playbook(&playbook_yaml)?;

    // Build the effective workload by merging playbook YAML
    // `workload:` defaults with the request's `payload:` overrides.
    // The orchestrator persists this as the `workload` key on the
    // `playbook_started` event; ExecutionState::handle_event reads
    // it back on every subsequent orchestrator pass so downstream
    // steps see the same workload the start step did.
    //
    // Without this merge, the playbook YAML's `workload:` defaults
    // never reached anything past the start step:
    // generate_initial_commands does its own merge (below) for the
    // start step's command context, but the playbook_started event
    // captured only the request payload — so when state.workload is
    // hydrated from that event, downstream steps' build_context
    // returned an empty `{}` and Jinja templates referencing
    // workload fields rendered to None.  See noetl/ai-meta#56.
    let mut merged_workload = serde_json::Map::new();
    if let Some(serde_json::Value::Object(map)) = &playbook.workload {
        for (k, v) in map {
            merged_workload.insert(k.clone(), v.clone());
        }
    }
    for (k, v) in &request.payload {
        merged_workload.insert(k.clone(), v.clone());
    }
    let workload = serde_json::Value::Object(merged_workload);

    // Resolve the per-execution routing once (noetl/ai-meta#90 Phase 2).
    // A `trace` not supplied explicitly is inherited from the parent
    // execution when this is a child run, so a W3C trace propagates down
    // the whole playbook nesting (RFC §7.4) bounded by that nesting.
    let trace = match (&request.trace, request.parent_execution_id) {
        (Some(t), _) if !t.is_null() => Some(t.clone()),
        (_, Some(parent)) => inherit_parent_trace(state, parent).await,
        _ => None,
    };
    let routing = CommandRouting {
        pool: request.execution_pool.clone().filter(|s| !s.is_empty()),
        trace,
    };

    // Emit playbook_started event
    let start_event_id = emit_playbook_started_event(
        state,
        execution_id,
        catalog_id,
        &path,
        &workload,
        request.parent_execution_id,
        &routing,
    )
    .await?;

    // Generate initial commands for the start step
    let commands_generated = generate_initial_commands(
        state,
        execution_id,
        catalog_id,
        start_event_id,
        &playbook,
        &request.payload,
        &routing,
    )
    .await?;

    info!(
        "Execution started: execution_id={}, commands_generated={}",
        execution_id, commands_generated
    );

    crate::metrics::record_execute_outcome(entry, "new");
    Ok(ExecuteOutcome {
        execution_id,
        status: "started",
        commands_generated,
    })
}

/// Emit a `subscription.message.deduplicated` event on the subscription's
/// lifecycle log (noetl/ai-meta#90 Phase 7) so a collapsed duplicate is
/// auditable end to end — which delivery was suppressed, which execution it
/// folded onto.  Keyed by the subscription id (`scope`); best-effort, since a
/// failed audit must never turn a correct dedup into an error.  The event type
/// is intentionally *not* one of the six lifecycle types the subscription
/// status query matches, so it can share the subscription's `execution_id`
/// without perturbing its lifecycle state (the Phase-4 fix, server#185).
async fn emit_deduplicated_event(
    state: &AppState,
    scope: i64,
    dedup_key: &str,
    existing_execution_id: i64,
    suppressed_execution_id: i64,
) {
    let event_id = match state.snowflake.generate() {
        Ok(id) => id,
        Err(e) => {
            tracing::warn!(subscription_id = scope, error = %e, "dedup audit: snowflake gen failed");
            return;
        }
    };
    // noetl.event.catalog_id is NOT NULL with an FK to noetl.catalog, so the
    // dedup audit must carry the subscription's own catalog_id.  Read it back
    // from the subscription scope's lifecycle events (its execution_id ==
    // scope).  Best-effort: if it can't be resolved, skip the audit rather
    // than turning a correct dedup into an error.
    // RFC #115 Phase 6: under `event_read_path=audit_only`, serve the scope's
    // catalog_id from the in-memory execute-time descriptor — ZERO `noetl.event`
    // read.  Cold descriptor falls through to the scan (counted `scan`).
    let audit_only = matches!(
        state.config.event_read_path,
        crate::config::EventReadPath::AuditOnly
    );
    // Read the (possibly KV-coherent) descriptor once.
    let desc_catalog: Option<i64> = if audit_only {
        state
            .exec_descriptors
            .get(scope)
            .await
            .map(|d| d.catalog_id)
            .filter(|c| *c != 0)
    } else {
        None
    };
    let catalog_id: Option<i64> = if let Some(cid) = desc_catalog {
        crate::metrics::record_event_hotpath_read("dedup_audit_catalog", "served_descriptor");
        Some(cid)
    } else if audit_only {
        // Cold descriptor under audit_only: catalog_id from `noetl.command` — the
        // synchronous queue — ZERO `noetl.event` read.
        crate::metrics::record_event_hotpath_read("dedup_audit_catalog", "served_command");
        sqlx::query_scalar("SELECT catalog_id FROM noetl.command WHERE execution_id = $1 LIMIT 1")
            .bind(scope)
            .fetch_optional(state.pools.pool_for(scope))
            .await
            .ok()
            .flatten()
    } else {
        crate::metrics::record_event_hotpath_read("dedup_audit_catalog", "scan");
        sqlx::query_scalar(
            "SELECT catalog_id FROM noetl.event WHERE execution_id = $1 ORDER BY event_id ASC LIMIT 1",
        )
        .bind(scope)
        .fetch_optional(state.pools.pool_for(scope))
        .await
        .ok()
        .flatten()
    };
    let Some(catalog_id) = catalog_id else {
        tracing::debug!(
            subscription_id = scope,
            "dedup audit: no catalog_id for scope; skipping audit event"
        );
        return;
    };
    let context = serde_json::json!({
        "subscription_id": scope.to_string(),
        "dedup_key": dedup_key,
        "original_execution_id": existing_execution_id.to_string(),
        "suppressed_execution_id": suppressed_execution_id.to_string(),
        "duplicate_suppressed": true,
    });
    let meta = serde_json::json!({
        "emitted_at": chrono::Utc::now().to_rfc3339(),
        "emitter": "control_plane",
    });
    // CQRS write-path chokepoint (#103 2d-3); best-effort, mirrors the prior
    // warn-on-failure.
    let ev = crate::handlers::event_write::EventRow::new(
        event_id,
        scope,
        catalog_id,
        "subscription.message.deduplicated",
        "DEDUPLICATED",
        chrono::Utc::now(),
    )
    .with_nodes("subscription", "ingress")
    .with_node_type("subscription")
    .with_context(context)
    .with_meta(meta);
    if let Err(e) =
        crate::handlers::event_write::emit_event(state, state.pools.pool_for(scope), ev).await
    {
        tracing::warn!(subscription_id = scope, error = %e, "dedup audit event insert failed (non-fatal)");
    }
}

/// Resolve catalog entry from path or catalog_id.
async fn resolve_catalog(state: &AppState, request: &ExecuteRequest) -> AppResult<(i64, String)> {
    if let Some(catalog_id) = request.catalog_id {
        // Lookup by catalog_id (Phase F R4-3: noetl.catalog is cluster-wide)
        let entry = sqlx::query_as::<_, (i64, String)>(
            "SELECT catalog_id, path FROM noetl.catalog WHERE catalog_id = $1",
        )
        .bind(catalog_id)
        .fetch_optional(state.pools.cluster())
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Catalog entry not found: {}", catalog_id)))?;

        Ok(entry)
    } else if let Some(path) = &request.path {
        // Lookup by path (latest version; Phase F R4-3: cluster-wide)
        let entry = sqlx::query_as::<_, (i64, String)>(
            "SELECT catalog_id, path FROM noetl.catalog WHERE path = $1 ORDER BY version DESC LIMIT 1",
        )
        .bind(path)
        .fetch_optional(state.pools.cluster())
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Playbook not found: {}", path)))?;

        Ok(entry)
    } else {
        Err(AppError::Validation(
            "Either path or catalog_id must be provided".to_string(),
        ))
    }
}

/// Get playbook YAML from catalog.
async fn get_playbook_yaml(state: &AppState, catalog_id: i64) -> AppResult<String> {
    // Try to get content first (raw YAML), fall back to payload (JSON)
    // Phase F R4-3: noetl.catalog is cluster-wide.
    let row: (Option<String>, Option<serde_json::Value>) =
        sqlx::query_as::<_, (Option<String>, Option<serde_json::Value>)>(
            "SELECT content, payload FROM noetl.catalog WHERE catalog_id = $1",
        )
        .bind(catalog_id)
        .fetch_optional(state.pools.cluster())
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Catalog entry not found: {}", catalog_id)))?;

    match row {
        (Some(content), _) if !content.is_empty() => Ok(content),
        (_, Some(payload)) => {
            // Convert JSON payload to YAML string
            serde_yaml::to_string(&payload).map_err(|e| {
                AppError::Internal(format!("Failed to convert payload to YAML: {}", e))
            })
        }
        _ => Err(AppError::NotFound(format!(
            "No playbook content found for catalog_id: {}",
            catalog_id
        ))),
    }
}

/// Emit playbook_started event.
async fn emit_playbook_started_event(
    state: &AppState,
    execution_id: i64,
    catalog_id: i64,
    path: &str,
    workload: &serde_json::Value,
    parent_execution_id: Option<i64>,
    routing: &CommandRouting,
) -> AppResult<i64> {
    let event_id = state.snowflake.generate()?;

    let context = serde_json::json!({
        "catalog_id": catalog_id.to_string(),
        "execution_id": execution_id.to_string(),
        "path": path,
        "workload": workload,
    });

    // Persist the per-execution routing (pool segment + trace) on the
    // start event's meta so the orchestrator — which rebuilds state from
    // the event log on each pass — recovers it for every follow-up command
    // (noetl/ai-meta#90 Phase 2).
    let mut meta = serde_json::json!({
        "emitted_at": chrono::Utc::now().to_rfc3339(),
        "emitter": "control_plane",
    });
    if let serde_json::Value::Object(ref mut m) = meta {
        if let Some(pool) = routing.pool.as_ref() {
            m.insert("execution_pool".to_string(), serde_json::json!(pool));
        }
        if let Some(trace) = routing.trace.as_ref() {
            m.insert("trace".to_string(), trace.clone());
        }
    }

    // Stateless off-server drive edge (RFC #115 Phase 4 remainder,
    // noetl/ai-meta#107 step 2): seed the execute-time descriptor with the two
    // execution-scoped, immutable facts the drive dispatch needs — catalog_id +
    // the routing meta — so under `NOETL_STATE_BUILDER=offserver` the drive can
    // route the orchestrate command WITHOUT rebuilding `WorkflowState` (ZERO
    // `noetl.event` reads on the drive path).  Seeded here, the first place both
    // are known; read in `events::trigger_orchestrator_inner`'s stateless branch.
    state
        .exec_descriptors
        .seed(execution_id, catalog_id, Some(meta.clone()))
        .await;

    // CQRS write-path chokepoint (#103 2d-3): INSERT (gate off) or publish (on).
    let ev = crate::handlers::event_write::EventRow::new(
        event_id,
        execution_id,
        catalog_id,
        "playbook_started",
        "STARTED",
        chrono::Utc::now(),
    )
    .with_nodes("playbook", path)
    .with_node_type("execution")
    .with_parent_execution_id(parent_execution_id)
    .with_context(context)
    .with_meta(meta);
    crate::handlers::event_write::emit_event(state, state.pools.pool_for(execution_id), ev).await?;

    Ok(event_id)
}

/// Inherit the W3C trace context (RFC §7.4) from a parent execution's
/// `playbook_started` event so a child run joins the same distributed trace.
/// Best-effort: a missing parent / missing trace simply yields `None`.
async fn inherit_parent_trace(
    state: &AppState,
    parent_execution_id: i64,
) -> Option<serde_json::Value> {
    // RFC #115 Phase 6: under `event_read_path=audit_only`, read the parent's
    // trace off its in-memory execute-time descriptor (its `routing_meta` is the
    // `playbook_started` meta this query reads) — ZERO `noetl.event` read.  A cold
    // parent descriptor falls through to the scan (counted `scan`).
    if matches!(
        state.config.event_read_path,
        crate::config::EventReadPath::AuditOnly
    ) {
        if let Some(desc) = state.exec_descriptors.get(parent_execution_id).await {
            if desc.catalog_id != 0 {
                crate::metrics::record_event_hotpath_read(
                    "inherit_parent_trace",
                    "served_descriptor",
                );
                return desc
                    .routing_meta
                    .as_ref()
                    .and_then(|m| m.get("trace"))
                    .filter(|v| !v.is_null())
                    .cloned();
            }
        }
        // Cold parent descriptor under audit_only: the W3C trace lives only in the
        // parent's `playbook_started` meta (no `noetl.command` source).  Trace
        // inheritance is best-effort observability — return None (the child opens
        // a fresh trace segment) rather than scan `noetl.event`, keeping the
        // never-scan invariant.  Only the parent-restart path is affected.
        crate::metrics::record_event_hotpath_read("inherit_parent_trace", "served_none_cold");
        return None;
    }
    crate::metrics::record_event_hotpath_read("inherit_parent_trace", "scan");

    let meta: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT meta FROM noetl.event WHERE execution_id = $1 AND event_type = 'playbook_started' \
         ORDER BY event_id ASC LIMIT 1",
    )
    .bind(parent_execution_id)
    .fetch_optional(state.pools.pool_for(parent_execution_id))
    .await
    .ok()
    .flatten();
    meta.and_then(|m| m.get("trace").filter(|v| !v.is_null()).cloned())
}

/// Marker key for an offloaded command context (noetl/ai-meta#114).  When a
/// `command.issued` event's `{tool_config, args, render_context}` exceeds
/// [`AppConfig::command_context_max_bytes`](crate::config::AppConfig), the full
/// context is stashed in `noetl.result_store` and the event + command row carry
/// only `{ "__context_ref__": "noetl://…" }` so the published event stays under
/// the NATS `max_payload`.  `get_command` / `claim_command` resolve it back
/// before the worker sees the command.
pub(crate) const COMMAND_CONTEXT_REF_KEY: &str = "__context_ref__";

/// Logical `name` segment used for offloaded command contexts in the result
/// store.  A constant (not the step name) keeps the `noetl://` URI parseable
/// regardless of step-name characters and unambiguous to spot in the store.
const COMMAND_CONTEXT_RESULT_NAME: &str = "__command_context__";

/// Offload an over-budget command context to the result store, returning a tiny
/// `{ "__context_ref__": "noetl://…", "__context_bytes__": N }` marker in its
/// place (noetl/ai-meta#114).
///
/// The off-server orchestrate drive embeds the full resolved upstream context
/// into the next step's `render_context` when `refs_in_state` is false; for a
/// large-context fixture this balloons the `command.issued` event past the NATS
/// `max_payload`, so the publish-only gate can't ack it and the execution
/// wedges.  This keeps the published event small while the full context lives
/// durably in `noetl.result_store`, resolved on the read side exactly like the
/// #113 drive-result offload.
///
/// Returns the original `cmd_context` untouched when it is within budget (the
/// common case — ordinary commands are a few KB), so the hot path is a single
/// `serde_json::to_vec` length check with no extra DB round-trip.
async fn maybe_offload_command_context(
    state: &AppState,
    execution_id: i64,
    cmd_context: serde_json::Value,
) -> AppResult<serde_json::Value> {
    let max_bytes = state.config.command_context_max_bytes;
    let bytes = match serde_json::to_vec(&cmd_context) {
        Ok(v) => v.len(),
        // If it can't even be serialised, hand it back unchanged — emit_event
        // will surface the real encode error at the publish boundary.
        Err(_) => return Ok(cmd_context),
    };
    if bytes <= max_bytes {
        return Ok(cmd_context);
    }

    let result_store = crate::services::result_store::ResultStoreService::new(
        state.pools.pool_for(execution_id).clone(),
        state.snowflake.clone(),
    );
    let put = result_store
        .put(
            execution_id,
            &crate::services::result_store::PutResultBody {
                name: COMMAND_CONTEXT_RESULT_NAME.to_string(),
                data: cmd_context,
                scope: "execution".to_string(),
                source_step: None,
                store: None,
                ttl: None,
                correlation: None,
                compress: false,
            },
        )
        .await?;

    crate::metrics::record_orchestrate_drive("context_offloaded");
    debug!(
        execution_id,
        bytes,
        max_bytes,
        noetl_ref = %put.r#ref,
        "command context exceeded budget — offloaded to result store (noetl/ai-meta#114)"
    );

    Ok(serde_json::json!({
        COMMAND_CONTEXT_REF_KEY: put.r#ref,
        "__context_bytes__": bytes,
    }))
}

/// Persist one engine-generated command + its `command.issued`
/// event + its NATS notification.
///
/// Used both by `generate_initial_commands` (the `/api/execute`
/// path that publishes the first command) and by
/// `trigger_orchestrator` in `events.rs` (the state-machine path
/// that publishes follow-up commands after `command.completed`
/// events).  The helper is `pub(crate)` so the events module can
/// call it without duplicating the wire-format logic that
/// noetl/ai-meta#49 phases B+C+D-R1 carefully established.
///
/// `step` is read for `args` (which the worker copies into the
/// tool config) and the canonical `step` name.  `command.tool`
/// supplies the rendered tool kind + config.  The function
/// generates a fresh `event_id` snowflake for the `command.issued`
/// row, derives `command_id` from `(execution_id, step, event_id)`,
/// and threads the notification through NATS via the path-based
/// routing scheme.
///
/// Returns the new `event_id` so callers can attribute downstream
/// events back to this command.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn persist_engine_command(
    state: &AppState,
    execution_id: i64,
    catalog_id: i64,
    parent_event_id: i64,
    step: &crate::playbook::types::Step,
    command: &crate::engine::commands::Command,
    render_context: &HashMap<String, serde_json::Value>,
    playbook: &crate::playbook::types::Playbook,
    routing: &CommandRouting,
) -> AppResult<i64> {
    let event_id = state.snowflake.generate()?;

    // R3b iterator fan-out: include the iteration index in
    // command_id so each per-iteration command row has a unique
    // identifier (the noetl.command primary key is per-execution
    // command_id), and the worker can correlate its emitted events
    // back to the right iteration.  Plain steps keep the
    // pre-existing 3-segment shape `<exec>:<step>:<event>`.
    let command_id = if let Some(iter) = command.iterator.as_ref() {
        format!(
            "{}:{}:{}:i{}",
            execution_id, step.step, event_id, iter.index
        )
    } else {
        format!("{}:{}:{}", execution_id, step.step, event_id)
    };

    let cmd_args = match &step.args {
        Some(map) => serde_json::to_value(map).unwrap_or_else(|_| serde_json::json!({})),
        None => serde_json::json!({}),
    };
    // Offload an over-budget context to the result store + ref marker so the
    // published `command.issued` event stays under the NATS max_payload
    // (noetl/ai-meta#114).  Within-budget contexts pass through unchanged.
    let cmd_context = maybe_offload_command_context(
        state,
        execution_id,
        serde_json::json!({
            "tool_config": command.tool.config,
            "args": cmd_args,
            "render_context": render_context,
        }),
    )
    .await?;

    // Base meta — extended below with iteration_index/_total for
    // iterator commands so `state.apply_event` (Phase D R3b state
    // aggregation) sees the per-event iteration index on the
    // matching `command.completed` row when the worker echoes
    // meta forward.
    let mut cmd_meta = serde_json::json!({
        "command_id": command_id,
        "step": step.step,
        "tool_kind": command.tool.kind,
        "max_attempts": 3,
        "attempt": 1,
        "execution_id": execution_id.to_string(),
        "catalog_id": catalog_id.to_string(),
        "actionable": true,
    });
    if let Some(iter) = command.iterator.as_ref() {
        if let serde_json::Value::Object(ref mut map) = cmd_meta {
            map.insert("iteration_index".to_string(), serde_json::json!(iter.index));
            map.insert("iteration_total".to_string(), serde_json::json!(iter.total));
            map.insert(
                "iterator_step".to_string(),
                serde_json::json!(iter.iterator_step.clone()),
            );
            map.insert(
                "item_var".to_string(),
                serde_json::json!(iter.item_var.clone()),
            );
        }
    }
    // Thread the W3C trace context onto the command.issued event meta so the
    // event log carries the external trace join and the worker can attach it
    // to its dispatch span (noetl/ai-meta#90 Phase 2, RFC §7.4).
    if let Some(trace) = routing.trace.as_ref() {
        if let serde_json::Value::Object(ref mut map) = cmd_meta {
            map.insert("trace".to_string(), trace.clone());
        }
    }
    // Carry the command's own metadata (e.g. the cursor-loop phase/frame for
    // mode: cursor, noetl/ai-meta#100) onto the command.issued event meta so a
    // later orchestrator pass can recognise the claim vs body command by
    // correlating the completion back to this issued event via command_id.
    if let Some(serde_json::Value::Object(extra)) = command.metadata.as_ref() {
        if let serde_json::Value::Object(ref mut map) = cmd_meta {
            for (k, v) in extra {
                map.insert(k.clone(), v.clone());
            }
        }
    }

    // CQRS write-path chokepoint (#103 2d-3): the command.issued EVENT goes
    // through emit_event (INSERT gate-off / publish gate-on).  The noetl.command
    // ROW below stays a synchronous INSERT — it's the command queue, not the
    // event log (#103 is event-log-scoped), and the worker fetches command
    // config from noetl.command, so it must be present read-your-writes.
    // The real predecessor event (RFC #115 §4.1) — the chain head before this
    // command.issued advances it (see persist_engine_commands_batch).
    let issuing_event = state
        .chain_heads
        .head(execution_id)
        .await
        .unwrap_or(parent_event_id);
    let ev = crate::handlers::event_write::EventRow::new(
        event_id,
        execution_id,
        catalog_id,
        "command.issued",
        "PENDING",
        chrono::Utc::now(),
    )
    .with_node(&step.step)
    .with_node_type(command.tool.kind.as_str())
    .with_context(cmd_context.clone())
    .with_meta(cmd_meta.clone())
    .with_parent_event_id(parent_event_id);
    crate::handlers::event_write::emit_event(state, state.pools.pool_for(execution_id), ev).await?;

    if let Err(e) = insert_command_row(
        state,
        execution_id,
        event_id,
        catalog_id,
        parent_event_id,
        issuing_event,
        &step.step,
        command.tool.kind.as_str(),
        &cmd_context,
        &cmd_meta,
    )
    .await
    {
        tracing::warn!(
            error = %e,
            execution_id,
            event_id,
            "Failed to insert noetl.command row (non-fatal — event log is source of truth)"
        );
    }

    publish_command_notification(
        state,
        execution_id,
        event_id,
        &command_id,
        &step.step,
        command.tool.kind.as_str(),
        playbook,
        routing,
    )
    .await?;

    Ok(event_id)
}

/// Batch variant of [`persist_engine_command`] (noetl/ai-meta#102 step 1).
///
/// A cursor fan-out issues one body command per row — on a 50-row frame that's
/// ~100 individual `INSERT`s (event + command per call) through PgBouncer to a
/// small Cloud SQL, the write-path bottleneck.  This collapses the whole batch
/// into **two multi-row `INSERT`s** (all `command.issued` events, then all
/// `noetl.command` rows) via `QueryBuilder`, then loops the NATS publishes (those
/// hit in-cluster NATS, not the DB, so they're cheap).  Per-command row content
/// is identical to the single path — same `command_id` derivation, `cmd_context`,
/// and `cmd_meta` (iterator / trace / cursor metadata).
///
/// Returns the number of commands persisted.
pub(crate) async fn persist_engine_commands_batch(
    state: &AppState,
    execution_id: i64,
    catalog_id: i64,
    parent_event_id: i64,
    commands: &[crate::engine::commands::Command],
    playbook: &crate::playbook::types::Playbook,
    routing: &CommandRouting,
) -> AppResult<i32> {
    if commands.is_empty() {
        return Ok(0);
    }

    // A command whose step is unknown is a hard orchestrator bug (same as the
    // single path's `ok_or_else`).  Surface it before touching the DB.
    struct Prepared<'a> {
        event_id: i64,
        command_id: String,
        num_command_id: i64,
        step_name: &'a str,
        tool_kind: &'a str,
        cmd_context: serde_json::Value,
        cmd_meta: serde_json::Value,
    }

    let now = chrono::Utc::now();
    let mut prepared: Vec<Prepared> = Vec::with_capacity(commands.len());
    for command in commands {
        let step = playbook.get_step(&command.step_name).ok_or_else(|| {
            AppError::Internal(format!(
                "Orchestrator returned command for unknown step '{}'",
                command.step_name
            ))
        })?;
        let render_context: HashMap<String, serde_json::Value> =
            command.context.clone().unwrap_or_default();

        let event_id = state.snowflake.generate()?;
        let command_id = if let Some(iter) = command.iterator.as_ref() {
            format!(
                "{}:{}:{}:i{}",
                execution_id, step.step, event_id, iter.index
            )
        } else {
            format!("{}:{}:{}", execution_id, step.step, event_id)
        };

        let cmd_args = match &step.args {
            Some(map) => serde_json::to_value(map).unwrap_or_else(|_| serde_json::json!({})),
            None => serde_json::json!({}),
        };
        // Offload an over-budget context to the result store + ref marker so the
        // published `command.issued` event stays under the NATS max_payload
        // (noetl/ai-meta#114).  This is the hot drive path — the next-step
        // command a large-context fixture produces is exactly the one that
        // blows the 1MB ceiling.  Within-budget contexts pass through unchanged.
        let cmd_context = maybe_offload_command_context(
            state,
            execution_id,
            serde_json::json!({
                "tool_config": command.tool.config,
                "args": cmd_args,
                "render_context": render_context,
            }),
        )
        .await?;

        let mut cmd_meta = serde_json::json!({
            "command_id": command_id,
            "step": step.step,
            "tool_kind": command.tool.kind,
            "max_attempts": 3,
            "attempt": 1,
            "execution_id": execution_id.to_string(),
            "catalog_id": catalog_id.to_string(),
            "actionable": true,
        });
        if let Some(iter) = command.iterator.as_ref() {
            if let serde_json::Value::Object(ref mut map) = cmd_meta {
                map.insert("iteration_index".to_string(), serde_json::json!(iter.index));
                map.insert("iteration_total".to_string(), serde_json::json!(iter.total));
                map.insert(
                    "iterator_step".to_string(),
                    serde_json::json!(iter.iterator_step.clone()),
                );
                map.insert(
                    "item_var".to_string(),
                    serde_json::json!(iter.item_var.clone()),
                );
            }
        }
        if let Some(trace) = routing.trace.as_ref() {
            if let serde_json::Value::Object(ref mut map) = cmd_meta {
                map.insert("trace".to_string(), trace.clone());
            }
        }
        if let Some(serde_json::Value::Object(extra)) = command.metadata.as_ref() {
            if let serde_json::Value::Object(ref mut map) = cmd_meta {
                for (k, v) in extra {
                    map.insert(k.clone(), v.clone());
                }
            }
        }

        prepared.push(Prepared {
            event_id,
            command_id,
            num_command_id: state.snowflake.generate()?,
            step_name: step.step.as_str(),
            tool_kind: command.tool.kind.as_str(),
            cmd_context,
            cmd_meta,
        });
    }

    let pool = state.pools.pool_for(execution_id);

    // Multi-row `command.issued` EVENTS through the CQRS chokepoint (#103 2d-3):
    // one multi-row INSERT (gate off) or one publish per row (gate on). The
    // noetl.command ROWS below stay synchronous (command queue, not event log).
    // prev_event_id for the command rows (RFC #115 §4.1) is the **real** event
    // whose application issued the command — the `step.enter` / unblocking
    // completion that is the chain head right now, BEFORE the command.issued
    // batch advances it.  This is exactly the predecessor the watermark stamps
    // on each command.issued event.  Using it (not `parent_event_id`, which
    // under the off-server drive is the suppressed `__orchestrate__` trigger)
    // keeps the command pointer referencing a materialized event.  For a cursor
    // fan-out the head is the claim/branch event, so every body command points
    // back at its fan-out origin (§4.4).  Fallback to `parent_event_id` only if
    // the chain head is somehow unset (no events yet — shouldn't happen after
    // playbook_started).
    let issuing_event = state
        .chain_heads
        .head(execution_id)
        .await
        .unwrap_or(parent_event_id);
    let event_rows: Vec<crate::handlers::event_write::EventRow> = prepared
        .iter()
        .map(|p| {
            crate::handlers::event_write::EventRow::new(
                p.event_id,
                execution_id,
                catalog_id,
                "command.issued",
                "PENDING",
                now,
            )
            .with_node(p.step_name)
            .with_node_type(p.tool_kind)
            .with_context(p.cmd_context.clone())
            .with_meta(p.cmd_meta.clone())
            .with_parent_event_id(parent_event_id)
        })
        .collect();
    crate::handlers::event_write::emit_events(state, pool, &event_rows).await?;

    // Multi-row INSERT of all `noetl.command` rows (non-fatal — the event log is
    // the source of truth; mirror the single path's warn-on-failure).
    // `prev_event_id` (RFC #115 Phase 2, noetl/ai-meta#115 §4.1): the event whose
    // application issued this command — i.e. the drive trigger (`parent_event_id`).
    // For a cursor fan-out that trigger is the claim/branch event, so every body
    // command points back at its fan-out origin (§4.4).  `latest_event_id` keeps
    // its existing meaning (most-recent lifecycle event); `prev_event_id` is the
    // immutable issuing link.
    let mut cqb = sqlx::QueryBuilder::new(
        "INSERT INTO noetl.command (command_id, event_id, execution_id, catalog_id, \
         step_name, tool_kind, status, attempt, context, meta, latest_event_id, prev_event_id) ",
    );
    cqb.push_values(prepared.iter(), |mut b, p| {
        b.push_bind(p.num_command_id)
            .push_bind(p.event_id)
            .push_bind(execution_id)
            .push_bind(catalog_id)
            .push_bind(p.step_name)
            .push_bind(p.tool_kind)
            .push_bind("PENDING")
            .push_bind(1_i32)
            .push_bind(&p.cmd_context)
            .push_bind(&p.cmd_meta)
            .push_bind(parent_event_id)
            .push_bind(issuing_event);
    });
    if let Err(e) = cqb.build().execute(pool).await {
        tracing::warn!(
            error = %e,
            execution_id,
            count = prepared.len(),
            "Batch noetl.command insert failed (non-fatal — event log is source of truth)"
        );
    }

    // Publish NATS notifications (in-cluster, cheap; loop is fine).
    for p in &prepared {
        publish_command_notification(
            state,
            execution_id,
            p.event_id,
            &p.command_id,
            p.step_name,
            p.tool_kind,
            playbook,
            routing,
        )
        .await?;
    }

    Ok(prepared.len() as i32)
}

/// Dispatch the worker-driven orchestrate "meta" command (noetl/ai-meta#108
/// slice 3): issue `system/orchestrate` (entry `run_state`) as a single wasm
/// command under the reserved `__orchestrate__` step, carrying the serialized
/// `OrchestrateStateInput` as its args. The worker runs the drive and reports
/// the `OrchestrationResult`; the server applies it on the command's completion
/// (`apply_worker_orchestration` in `handlers::events`).
///
/// Unlike `persist_engine_commands_batch` this does NOT require the step to
/// exist in the playbook — `__orchestrate__` is infrastructure, not a workflow
/// step, and its `command.*` events are ignored by `WorkflowState::apply_event`
/// (the `ORCHESTRATE_META_STEP` guard) so it never pollutes the drive state.
pub(crate) async fn dispatch_orchestrate_command(
    state: &AppState,
    execution_id: i64,
    catalog_id: i64,
    parent_event_id: i64,
    input: serde_json::Value,
    playbook: &crate::playbook::types::Playbook,
    routing: &CommandRouting,
) -> AppResult<()> {
    const STEP: &str = "__orchestrate__";
    let event_id = state.snowflake.generate()?;
    let command_id = format!("{execution_id}:{STEP}:{event_id}");

    // The worker reads `tool_config.plugin` + `tool_config.args` (its
    // `wasm_config_to_ref`); `args` carries the OrchestrateStateInput.
    let tool_config = serde_json::json!({
        "plugin": { "path": "system/orchestrate", "version": 1, "entry": "run_state" },
        "args": input,
    });
    let cmd_context = serde_json::json!({
        "tool_config": tool_config,
        "args": {},
        "render_context": {},
    });
    let cmd_meta = serde_json::json!({
        "command_id": command_id,
        "step": STEP,
        "tool_kind": "wasm",
        "max_attempts": 1,
        "attempt": 1,
        "execution_id": execution_id.to_string(),
        "catalog_id": catalog_id.to_string(),
        "actionable": true,
    });

    // The meta-command writes NOTHING to noetl.event (noetl/ai-meta#108): it is
    // infrastructure, not a workflow step, and at scale a `command.issued` row
    // per drive would burst noetl.event + Postgres. Its delivery record lives
    // only in noetl.command — the worker's claim/get path falls back to it when
    // noetl.event has no `command.issued` for the event_id. So this row is the
    // sole record the worker fetches: it is FATAL on error (no event-log
    // fallback), unlike the best-effort mirror in `persist_engine_commands_batch`.
    let pool = state.pools.pool_for(execution_id);
    let num_command_id = state.snowflake.generate()?;
    sqlx::query(
        "INSERT INTO noetl.command (command_id, event_id, execution_id, catalog_id, \
         step_name, tool_kind, status, attempt, context, meta, latest_event_id, prev_event_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(num_command_id)
    .bind(event_id)
    .bind(execution_id)
    .bind(catalog_id)
    .bind(STEP)
    .bind("wasm")
    .bind("PENDING")
    .bind(1_i32)
    .bind(&cmd_context)
    .bind(&cmd_meta)
    .bind(parent_event_id)
    // prev_event_id (RFC #115 §4.1): the drive trigger that issued this
    // meta-command.  The `__orchestrate__` command is suppressed from the event
    // chain (no command.issued row), but its issuing link is recorded uniformly.
    .bind(parent_event_id)
    .execute(pool)
    .await?;

    // Route the drive to the dedicated `system` worker pool (noetl/ai-meta#108)
    // so it doesn't compete with user compute for slots — the system pool
    // subscribes to `noetl.commands.system.>`. The user pool's segment is derived
    // from the playbook path; override it to `system` here, preserving the
    // execution's W3C trace so the drive stays on the same trace.
    let system_routing = CommandRouting {
        pool: Some("system".to_string()),
        trace: routing.trace.clone(),
    };
    publish_command_notification(
        state,
        execution_id,
        event_id,
        &command_id,
        STEP,
        "wasm",
        playbook,
        &system_routing,
    )
    .await?;
    crate::metrics::record_orchestrate_drive("dispatched");
    Ok(())
}

/// Generate initial commands for the start step.
///
/// If the start step declares a `loop:` block, this function fans out
/// one command per item in the resolved collection (mirroring the
/// Phase D R3b fan-out that [`orchestrator.rs`] performs for
/// mid-execution iterator steps).  When the start step has no loop,
/// the pre-existing single-command path is preserved verbatim.
#[allow(clippy::too_many_arguments)]
async fn generate_initial_commands(
    state: &AppState,
    execution_id: i64,
    catalog_id: i64,
    parent_event_id: i64,
    playbook: &crate::playbook::types::Playbook,
    payload: &HashMap<String, serde_json::Value>,
    routing: &CommandRouting,
) -> AppResult<i32> {
    // Find start step
    let start_step = playbook
        .get_step("start")
        .ok_or_else(|| AppError::Validation("Start step 'start' not found".to_string()))?;

    // Build command context by merging playbook workload (defaults) with execution payload (overrides)
    // RFC noetl/ai-meta#115 Phase 5: narrow the initial command's worker-bound
    // context to its minimal slice when the atomic-item-context flag is on.
    let command_builder = crate::engine::commands::CommandBuilder::with_atomic_item_context(
        state.config.atomic_item_context,
    );
    let mut context = HashMap::new();

    // First, add playbook workload defaults
    if let Some(serde_json::Value::Object(map)) = &playbook.workload {
        for (k, v) in map {
            context.insert(k.clone(), v.clone());
        }
    }

    // Then, override with execution payload values (execution values take precedence)
    for (k, v) in payload {
        context.insert(k.clone(), v.clone());
    }

    // Also expose as 'workload' for {{ workload.session_token }} syntax
    context.insert(
        "workload".to_string(),
        serde_json::to_value(&context).unwrap_or_default(),
    );

    // Iterator fan-out at initial dispatch: mirror the R3b shape from
    // orchestrator.rs.  If the start step has a `loop:` block, resolve
    // the `in:` expression and emit commands.
    //
    // #76: respects LoopMode — parallel dispatches all items at once,
    // sequential (default) dispatches only iteration 0.  Also emits a
    // `step.enter` event with `iterations_expected` so the state
    // machine knows how many command.completed events to wait for.
    if let Some(loop_cfg) = start_step.r#loop.as_ref() {
        // Render the loop expression to a raw JSON value and require a
        // JSON array.  Unlike orchestrator.rs (which coerces numbers to
        // ranges and strings to splits), the initial-dispatch boundary
        // enforces strict array typing so callers pass an explicit list.
        let renderer = crate::template::TemplateRenderer::new();
        let raw_value =
            renderer.render_to_value(loop_cfg.in_expr.as_deref().unwrap_or(""), &context)?;

        let items: Vec<serde_json::Value> = match raw_value {
            serde_json::Value::Array(arr) => arr,
            other => {
                let type_name = match &other {
                    serde_json::Value::Null => "null",
                    serde_json::Value::Bool(_) => "bool",
                    serde_json::Value::Number(_) => "number",
                    serde_json::Value::String(_) => "string",
                    serde_json::Value::Object(_) => "object",
                    serde_json::Value::Array(_) => unreachable!(),
                };
                return Err(AppError::Validation(format!(
                    "start step loop.in must resolve to a JSON array, got: {}",
                    type_name
                )));
            }
        };

        let total = items.len();
        let is_parallel = loop_cfg
            .spec
            .as_ref()
            .map(|s| s.mode == crate::playbook::types::LoopMode::Parallel)
            .unwrap_or(false);

        info!(
            execution_id,
            total,
            iterator = %loop_cfg.iterator,
            mode = if is_parallel { "parallel" } else { "sequential" },
            "Fanning out {} iterations for start step (iterator='{}', mode={})",
            total,
            loop_cfg.iterator,
            if is_parallel { "parallel" } else { "sequential" },
        );

        // Emit step.enter with iterations_expected so the state
        // machine knows how many iterations to wait for before
        // marking the step Completed.  Without this, the first
        // command.completed would flip the step to Completed and
        // the remaining iterations would be orphaned.
        let enter_event_id = state.snowflake.generate()?;
        let enter_result = serde_json::json!({
            "status": "ENTERED",
            "context": {
                "iterations_expected": total,
                "iterator_var": loop_cfg.iterator,
            },
        });
        // CQRS write-path chokepoint (#103 2d-3).
        let ev = crate::handlers::event_write::EventRow::new(
            enter_event_id,
            execution_id,
            catalog_id,
            "step.enter",
            "ENTERED",
            chrono::Utc::now(),
        )
        .with_node(&start_step.step)
        .with_result(enter_result.clone())
        .with_meta(serde_json::json!({"emitted_by": "execute_handler"}))
        .with_parent_event_id(parent_event_id);
        crate::handlers::event_write::emit_event(state, state.pools.pool_for(execution_id), ev)
            .await?;

        // #76: parallel = all items; sequential = only iteration 0.
        let dispatch_count = if is_parallel { total } else { 1 };
        for (idx, item) in items.into_iter().take(dispatch_count).enumerate() {
            let iter_meta = crate::engine::commands::IteratorMetadata {
                parent_execution_id: execution_id,
                iterator_step: start_step.step.clone(),
                item_var: loop_cfg.iterator.clone(),
                item,
                index: idx,
                total,
            };
            let command = command_builder.build_iteration_command(
                0,
                execution_id,
                catalog_id,
                parent_event_id,
                start_step,
                &context,
                iter_meta,
            )?;
            // Use the command's own enriched context (which includes
            // flat iter vars + ctx/workload shims) rather than the raw
            // orchestrator `context`.  The orchestrator dispatch path in
            // events.rs does the same (line 1491).  Without this, the
            // worker's render_context only carries the `ctx` wrapper and
            // pipeline `input:`/`command:` templates referencing
            // `{{ iter.<var> }}` fail with "undefined value".
            let cmd_render_ctx = command.context.clone().unwrap_or_default();
            persist_engine_command(
                state,
                execution_id,
                catalog_id,
                parent_event_id,
                start_step,
                &command,
                &cmd_render_ctx,
                playbook,
                routing,
            )
            .await?;
        }

        return Ok(total as i32);
    }

    // RFC #115 Phase 5 observability: classify how the start command was sized
    // (the narrowing itself happens inside build_command).  Zero cost when off.
    if state.config.atomic_item_context {
        let narrowed = start_step.r#loop.is_none()
            && noetl_orchestrate_core::input_binding::analyze(start_step).bounded;
        crate::metrics::record_atomic_item_context(if narrowed {
            "narrowed"
        } else {
            "full_fallback"
        });
    }

    let command = command_builder.build_command(
        0, // Will be replaced with actual event_id
        execution_id,
        catalog_id,
        parent_event_id,
        start_step,
        &context,
        None,
    )?;

    // Persist + publish via the shared helper so the same wire-format
    // logic feeds both the /api/execute path (this function) and the
    // orchestrator-triggered transitions in events.rs::trigger_orchestrator.
    // Use the command's enriched context (with ctx/workload shims) so
    // the worker sees the full render_context for template resolution.
    let cmd_render_ctx = command.context.clone().unwrap_or_default();
    persist_engine_command(
        state,
        execution_id,
        catalog_id,
        parent_event_id,
        start_step,
        &command,
        &cmd_render_ctx,
        playbook,
        routing,
    )
    .await?;

    Ok(1)
}

/// Insert a row into `noetl.command` mirroring the `command.issued`
/// event row.  See [`generate_initial_commands`] for the rationale —
/// the worker doesn't read from this table today but Python's server
/// populates it and downstream tooling depends on it.
#[allow(clippy::too_many_arguments)]
async fn insert_command_row(
    state: &AppState,
    execution_id: i64,
    event_id: i64,
    catalog_id: i64,
    parent_event_id: i64,
    prev_event_id: i64,
    step_name: &str,
    tool_kind: &str,
    context: &serde_json::Value,
    meta: &serde_json::Value,
) -> AppResult<()> {
    let command_id = state.snowflake.generate()?;
    // No ON CONFLICT clause: `noetl.command` is a partitioned
    // table, so a PRIMARY KEY index on the partition root doesn't
    // exist — only per-partition indexes do.  PG rejects
    // `ON CONFLICT (command_id)` as "no unique or exclusion
    // constraint matching the ON CONFLICT specification".  We use
    // a fresh snowflake `command_id` per call so duplicate inserts
    // can't happen on the happy path anyway; on retry the caller
    // generates a new id.
    sqlx::query(
        r#"
        INSERT INTO noetl.command (
            command_id, event_id, execution_id, catalog_id,
            step_name, tool_kind, status, attempt,
            context, meta, latest_event_id, prev_event_id
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12
        )
        "#,
    )
    .bind(command_id)
    .bind(event_id)
    .bind(execution_id)
    .bind(catalog_id)
    .bind(step_name)
    .bind(tool_kind)
    .bind("PENDING")
    .bind(1_i32)
    .bind(context)
    .bind(meta)
    .bind(parent_event_id)
    // prev_event_id (RFC #115 §4.1): the real predecessor (step.enter /
    // unblocking completion) that issued this command — the chain head captured
    // by the caller before the command.issued event advanced it.
    .bind(prev_event_id)
    .execute(state.pools.pool_for(execution_id))
    .await?;
    Ok(())
}

/// Publish a small notification to NATS so the Rust worker pool
/// (subscribed to `noetl.commands.<segment>.>`) can claim the
/// command.  The payload shape matches the Python
/// `NATSEventPublisher.publish_command()` so the worker doesn't
/// need to distinguish between servers — same wire format.
///
/// Subject derivation mirrors the path-based routing scheme in
/// `repos/noetl/noetl/core/runtime/pool_routing.py`:
///
/// - `system/*` playbook paths → `noetl.commands.system.<execution_id>`
/// - Everything else            → `noetl.commands.shared.<execution_id>`
///
/// When NATS isn't configured (e.g. local unit-test mode), the
/// notification is skipped with a WARN — caller still records the
/// `command.issued` event so dashboards work, but no worker will
/// pick the command up.
#[allow(clippy::too_many_arguments)]
async fn publish_command_notification(
    state: &AppState,
    execution_id: i64,
    event_id: i64,
    command_id: &str,
    step: &str,
    _tool_kind: &str,
    playbook: &crate::playbook::types::Playbook,
    routing: &CommandRouting,
) -> AppResult<()> {
    // noetl/ai-meta#194 L1 T4 — the command-bus transport. `nats` (default)
    // preserves the exact path below; `ehdb`/`shadow` also/only publish to the
    // EHDB writer (see the dispatch after the notification is built).
    let mode = state.command_bus_mode;

    // Pool segment selection (noetl/ai-meta#90 Phase 2).  Precedence:
    //   1. an explicit per-execution `execution_pool` override (the
    //      subscription continuous runtime / a header `x-noetl-pool`
    //      directive) — lands the run on `noetl.commands.<override>.<eid>`,
    //   2. else the path-derived default: `system/` paths → `system`,
    //      `subscription/` paths → `subscription`, everything else →
    //      `shared` (the same default Python uses).
    // The override isolates a subscription firehose on a dedicated segment
    // without touching the shared command stream.
    let pool_segment = match routing.pool.as_deref() {
        Some(p) if !p.is_empty() => p,
        _ => match playbook.metadata.path.as_deref() {
            Some(p) if p.starts_with("system/") => "system",
            Some(p) if p.starts_with("subscription/") => "subscription",
            _ => "shared",
        },
    };
    // noetl/ai-meta#166 Phase 5: route the system pool's commands to a per-shard
    // subject so the owning drive replica's per-shard consumer receives them
    // first (no Phase-4 NAK redirect). Default off → legacy pool subject. The
    // shard subject stays under `noetl.commands.<pool>.>`, so flipping the flag
    // before the fleet switches to per-shard filters degrades to the NAK path
    // rather than dropping a hop (see `sharding::command_subject`).
    let command_shard_count = state.config.command_shard_count.unwrap_or(1);
    let subject = crate::sharding::command_subject(
        pool_segment,
        execution_id,
        state.config.shard_subject_route,
        command_shard_count,
    );
    let publish_route = if subject.contains(".shard.") {
        "sharded"
    } else {
        "legacy"
    };

    let server_url = state
        .config
        .public_server_url
        .clone()
        .unwrap_or_else(|| "http://localhost:8082".to_string());

    let mut notification = serde_json::json!({
        "execution_id": execution_id,
        "event_id": event_id,
        "command_id": command_id,
        "step": step,
        "server_url": server_url,
        // Stamp the resolved pool segment on the dispatch so the worker can
        // decline commands that aren't for its pool — defence-in-depth against a
        // JetStream consumer whose filter_subject drifted broad and so receives
        // another pool's commands (noetl/ai-meta#108).
        "execution_pool": pool_segment,
    });
    // Carry the W3C trace context on the command notification so the worker
    // can attach it to its dispatch span (RFC §7.4).
    if let Some(trace) = routing.trace.as_ref() {
        if let serde_json::Value::Object(ref mut m) = notification {
            m.insert("trace".to_string(), trace.clone());
        }
    }
    let payload = serde_json::to_vec(&notification)
        .map_err(|e| AppError::Internal(format!("Serialize command notification: {e}")))?;

    // --- EHDB command bus (L1 T4) --------------------------------------------
    // In `ehdb`/`shadow` mode, mirror the command onto the per-shard EHDB writer
    // (routed by execution_id; event_id is the sort key). In `ehdb` mode a
    // publish failure fails the dispatch (the command wasn't delivered); in
    // `shadow` mode NATS is authoritative, so a failure is logged and swallowed.
    if mode.publishes_ehdb() {
        match state.ehdb_command_publisher.as_ref() {
            Some(publisher) => match publisher.publish(execution_id, event_id, &payload).await {
                Ok(seq) => {
                    tracing::info!(
                        execution_id,
                        event_id,
                        ehdb_sort_key = seq,
                        command_id = %command_id,
                        "Published command notification to EHDB bus"
                    );
                }
                Err(e) => {
                    if matches!(mode, crate::command_bus::CommandBusMode::Ehdb) {
                        return Err(AppError::Internal(format!("EHDB command publish: {e}")));
                    }
                    tracing::warn!(
                        execution_id,
                        event_id,
                        error = %e,
                        "EHDB shadow command publish failed (NATS authoritative)"
                    );
                }
            },
            None if matches!(mode, crate::command_bus::CommandBusMode::Ehdb) => {
                tracing::warn!(
                    execution_id,
                    event_id,
                    "EHDB command bus selected but no writers configured; command not delivered"
                );
            }
            None => {}
        }
    }

    // --- NATS command bus (today's path) -------------------------------------
    if mode.publishes_nats() {
        let Some(nats_client) = state.nats.as_ref() else {
            tracing::warn!(
                execution_id,
                event_id,
                "NATS not configured; command notification skipped — worker won't claim this command"
            );
            return Ok(());
        };

        let js = async_nats::jetstream::new((**nats_client).clone());
        js.publish(subject.clone(), payload.into())
            .await
            .map_err(|e| AppError::Internal(format!("NATS publish failed: {e}")))?
            .await
            .map_err(|e| AppError::Internal(format!("NATS publish ack failed: {e}")))?;

        crate::metrics::record_command_publish(publish_route, pool_segment);

        tracing::info!(
            execution_id,
            event_id,
            %subject,
            route = publish_route,
            command_id = %command_id,
            "Published command notification to NATS"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::commands::{CommandBuilder, IteratorMetadata};
    use crate::playbook::types::{Loop, Step, ToolDefinition, ToolKind, ToolSpec};
    use crate::template::TemplateRenderer;

    // -----------------------------------------------------------------------
    // Helper: build a minimal Step for tests (no loop by default).
    // -----------------------------------------------------------------------
    fn make_python_step(name: &str, loop_cfg: Option<Loop>) -> Step {
        Step {
            step: name.to_string(),
            desc: None,
            spec: None,
            when: None,
            args: None,
            vars: None,
            set_vars: None,
            r#loop: loop_cfg,
            tool: ToolDefinition::Single(Box::new(ToolSpec {
                kind: ToolKind::Python,
                auth: None,
                libs: None,
                args: None,
                code: Some("num = input_data.get('num'); return {'number': num}".to_string()),
                url: None,
                method: None,
                query: None,
                command: None,
                connection: None,
                params: None,
                headers: None,
                eval: None,
                output_select: None,
                extra: HashMap::new(),
            })),
            next: None,
        }
    }

    /// Simulate the pure (non-async, non-DB) logic of
    /// `generate_initial_commands` for the iterator fan-out path.
    /// Returns `(commands, AppError)` depending on what the loop
    /// expression resolves to.
    fn run_initial_fanout(
        step: &Step,
        context: &HashMap<String, serde_json::Value>,
    ) -> Result<Vec<crate::engine::commands::Command>, AppError> {
        let command_builder = CommandBuilder::new();
        let execution_id = 1_i64;
        let catalog_id = 2_i64;
        let parent_event_id = 0_i64;

        if let Some(loop_cfg) = step.r#loop.as_ref() {
            let renderer = TemplateRenderer::new();
            let raw_value = renderer
                .render_to_value(loop_cfg.in_expr.as_deref().unwrap_or(""), context)
                .map_err(|e| AppError::Internal(e.to_string()))?;

            let items: Vec<serde_json::Value> = match raw_value {
                serde_json::Value::Array(arr) => arr,
                other => {
                    let type_name = match &other {
                        serde_json::Value::Null => "null",
                        serde_json::Value::Bool(_) => "bool",
                        serde_json::Value::Number(_) => "number",
                        serde_json::Value::String(_) => "string",
                        serde_json::Value::Object(_) => "object",
                        serde_json::Value::Array(_) => unreachable!(),
                    };
                    return Err(AppError::Validation(format!(
                        "start step loop.in must resolve to a JSON array, got: {}",
                        type_name
                    )));
                }
            };

            let total = items.len();
            let mut commands = Vec::with_capacity(total);
            for (idx, item) in items.into_iter().enumerate() {
                let iter_meta = IteratorMetadata {
                    parent_execution_id: execution_id,
                    iterator_step: step.step.clone(),
                    item_var: loop_cfg.iterator.clone(),
                    item,
                    index: idx,
                    total,
                };
                let cmd = command_builder
                    .build_iteration_command(
                        0,
                        execution_id,
                        catalog_id,
                        parent_event_id,
                        step,
                        context,
                        iter_meta,
                    )
                    .map_err(|e| AppError::Internal(e.to_string()))?;
                commands.push(cmd);
            }
            return Ok(commands);
        }

        // No loop — single command.
        let cmd = command_builder
            .build_command(
                0,
                execution_id,
                catalog_id,
                parent_event_id,
                step,
                context,
                None,
            )
            .map_err(|e| AppError::Internal(e.to_string()))?;
        Ok(vec![cmd])
    }

    /// Fan-out test: start step with loop produces one command per item.
    #[test]
    fn test_generate_initial_commands_fans_out_when_start_has_loop() {
        let loop_cfg = Loop {
            in_expr: Some("{{ items }}".to_string()),
            cursor: None,
            iterator: "item".to_string(),
            spec: None,
        };
        let step = make_python_step("start", Some(loop_cfg));

        let mut context = HashMap::new();
        context.insert("items".to_string(), serde_json::json!([1, 2, 3]));

        let commands = run_initial_fanout(&step, &context).expect("should fan out");
        assert_eq!(commands.len(), 3, "expected 3 commands for 3-element list");

        for (expected_idx, cmd) in commands.iter().enumerate() {
            let iter = cmd.iterator.as_ref().expect("iterator metadata present");
            assert_eq!(
                iter.item_var, "item",
                "item_var must be the declared iterator name"
            );
            assert_eq!(
                iter.index, expected_idx,
                "index must match enumeration order"
            );
            assert_eq!(iter.total, 3, "total must be the collection length");
        }
    }

    /// Back-compat: start step without loop produces exactly one command, no iterator metadata.
    #[test]
    fn test_generate_initial_commands_single_command_when_no_loop() {
        let step = make_python_step("start", None);
        let context = HashMap::new();

        let commands = run_initial_fanout(&step, &context).expect("should produce one command");
        assert_eq!(
            commands.len(),
            1,
            "expected exactly 1 command for non-loop start step"
        );
        assert!(
            commands[0].iterator.is_none(),
            "non-loop command must not carry iterator metadata"
        );
    }

    /// Non-array loop.in returns AppError::Validation with the documented message.
    #[test]
    fn test_generate_initial_commands_rejects_non_array_loop_in() {
        let loop_cfg = Loop {
            in_expr: Some("{{ count }}".to_string()),
            cursor: None,
            iterator: "item".to_string(),
            spec: None,
        };
        let step = make_python_step("start", Some(loop_cfg));

        let mut context = HashMap::new();
        context.insert("count".to_string(), serde_json::json!(42));

        let err =
            run_initial_fanout(&step, &context).expect_err("scalar loop.in should return Err");
        match err {
            AppError::Validation(msg) => {
                assert!(
                    msg.contains("start step loop.in must resolve to a JSON array"),
                    "unexpected validation message: {msg}"
                );
                assert!(
                    msg.contains("number"),
                    "message should name the actual type: {msg}"
                );
            }
            other => panic!("expected Validation error, got: {other:?}"),
        }
    }

    #[test]
    fn test_execute_request_validation() {
        let request = ExecuteRequest {
            path: None,
            catalog_id: None,
            payload: HashMap::new(),
            parent_execution_id: None,
            execution_pool: None,
            trace: None,
            dedup: None,
        };
        assert!(request.validate().is_err());

        let request = ExecuteRequest {
            path: Some("test/playbook".to_string()),
            catalog_id: None,
            payload: HashMap::new(),
            parent_execution_id: None,
            execution_pool: None,
            trace: None,
            dedup: None,
        };
        assert!(request.validate().is_ok());

        let request = ExecuteRequest {
            path: None,
            catalog_id: Some(12345),
            payload: HashMap::new(),
            parent_execution_id: None,
            execution_pool: None,
            trace: None,
            dedup: None,
        };
        assert!(request.validate().is_ok());
    }

    #[test]
    fn test_execute_response_serialization() {
        let response = ExecuteResponse {
            execution_id: "12345".to_string(),
            status: "started".to_string(),
            commands_generated: 1,
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("12345"));
        assert!(json.contains("started"));
    }

    // -----------------------------------------------------------------------
    // Phase 7 — dedup spec + batch request/response shapes
    // -----------------------------------------------------------------------

    /// `dedup.window_secs` defaults to the module default when omitted, and an
    /// explicit value is honored.
    #[test]
    fn test_dedup_spec_window_default() {
        let d: DedupSpec = serde_json::from_str(r#"{"key":"abc"}"#).unwrap();
        assert_eq!(d.key, "abc");
        assert_eq!(
            d.window_secs,
            crate::db::queries::subscription_dedup::DEFAULT_WINDOW_SECS
        );

        let d: DedupSpec = serde_json::from_str(r#"{"key":"abc","window_secs":42}"#).unwrap();
        assert_eq!(d.window_secs, 42);
    }

    /// An execute request carries an optional `dedup` block; absent → None
    /// (dedup off, the default), present → parsed.
    #[test]
    fn test_execute_request_dedup_optional() {
        let r: ExecuteRequest = serde_json::from_str(r#"{"path":"p"}"#).unwrap();
        assert!(r.dedup.is_none());

        let r: ExecuteRequest = serde_json::from_str(
            r#"{"path":"p","parent_execution_id":7,"dedup":{"key":"k1","window_secs":60}}"#,
        )
        .unwrap();
        let d = r.dedup.expect("dedup present");
        assert_eq!(d.key, "k1");
        assert_eq!(d.window_secs, 60);
    }

    /// A batch request is N independent execute requests, each preserving its
    /// own per-message path / pool / trace / dedup.
    #[test]
    fn test_batch_request_preserves_per_item_routing() {
        let body = r#"{
            "executions": [
                {"path":"domain/a","execution_pool":"iot","payload":{"x":1}},
                {"path":"domain/b","parent_execution_id":99,"dedup":{"key":"k"}},
                {"catalog_id":5,"trace":{"traceparent":"00-abc-def-01"}}
            ]
        }"#;
        let req: BatchExecuteRequest = serde_json::from_str(body).unwrap();
        assert_eq!(req.executions.len(), 3);
        assert_eq!(req.executions[0].path.as_deref(), Some("domain/a"));
        assert_eq!(req.executions[0].execution_pool.as_deref(), Some("iot"));
        assert_eq!(req.executions[1].parent_execution_id, Some(99));
        assert_eq!(req.executions[1].dedup.as_ref().unwrap().key, "k");
        assert_eq!(req.executions[2].catalog_id, Some(5));
        assert!(req.executions[2].trace.is_some());
    }

    /// The batch response serializes a summary + per-item results; an error
    /// item carries its message, a started item carries its execution_id.
    #[test]
    fn test_batch_response_serialization() {
        let resp = BatchExecuteResponse {
            count: 2,
            started: 1,
            duplicates: 0,
            failed: 1,
            results: vec![
                BatchItemResult {
                    index: 0,
                    status: "started".to_string(),
                    execution_id: Some("777".to_string()),
                    commands_generated: 1,
                    error: None,
                },
                BatchItemResult {
                    index: 1,
                    status: "error".to_string(),
                    execution_id: None,
                    commands_generated: 0,
                    error: Some("Playbook not found: nope".to_string()),
                },
            ],
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"count\":2"));
        assert!(json.contains("\"started\":1"));
        assert!(json.contains("\"failed\":1"));
        assert!(json.contains("777"));
        assert!(json.contains("Playbook not found"));
        // A started item omits `error`; an error item omits `execution_id`.
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(v["results"][0].get("error").is_none());
        assert!(v["results"][1].get("execution_id").is_none());
    }

    #[test]
    fn test_execute_outcome_into_response() {
        let outcome = ExecuteOutcome {
            execution_id: 42,
            status: "duplicate",
            commands_generated: 0,
        };
        let resp = outcome.into_response();
        assert_eq!(resp.execution_id, "42");
        assert_eq!(resp.status, "duplicate");
        assert_eq!(resp.commands_generated, 0);
    }
}
