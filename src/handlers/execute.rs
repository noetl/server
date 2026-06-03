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
    // Validate request
    request.validate().map_err(AppError::Validation)?;

    debug!(
        "Execute request: path={:?}, catalog_id={:?}",
        request.path, request.catalog_id
    );

    // Resolve catalog entry
    let (catalog_id, path) = resolve_catalog(&state, &request).await?;

    info!(
        "Starting execution for path={}, catalog_id={}",
        path, catalog_id
    );

    // Get playbook from catalog
    let playbook_yaml = get_playbook_yaml(&state, catalog_id).await?;

    // Parse playbook
    let playbook = crate::playbook::parser::parse_playbook(&playbook_yaml)?;

    // Generate execution_id
    let execution_id = generate_snowflake_id(&state).await?;

    // Build workload from payload
    let workload = serde_json::to_value(&request.payload)
        .map_err(|e| AppError::Internal(format!("Failed to serialize payload: {}", e)))?;

    // Emit playbook_started event
    let start_event_id = emit_playbook_started_event(
        &state,
        execution_id,
        catalog_id,
        &path,
        &workload,
        request.parent_execution_id,
    )
    .await?;

    // Generate initial commands for the start step
    let commands_generated = generate_initial_commands(
        &state,
        execution_id,
        catalog_id,
        start_event_id,
        &playbook,
        &request.payload,
    )
    .await?;

    info!(
        "Execution started: execution_id={}, commands_generated={}",
        execution_id, commands_generated
    );

    Ok(Json(ExecuteResponse {
        execution_id: execution_id.to_string(),
        status: "started".to_string(),
        commands_generated,
    }))
}

/// Resolve catalog entry from path or catalog_id.
async fn resolve_catalog(state: &AppState, request: &ExecuteRequest) -> AppResult<(i64, String)> {
    if let Some(catalog_id) = request.catalog_id {
        // Lookup by catalog_id
        let entry = sqlx::query_as::<_, (i64, String)>(
            "SELECT catalog_id, path FROM noetl.catalog WHERE catalog_id = $1",
        )
        .bind(catalog_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("Catalog entry not found: {}", catalog_id)))?;

        Ok(entry)
    } else if let Some(path) = &request.path {
        // Lookup by path (latest version)
        let entry = sqlx::query_as::<_, (i64, String)>(
            "SELECT catalog_id, path FROM noetl.catalog WHERE path = $1 ORDER BY version DESC LIMIT 1",
        )
        .bind(path)
        .fetch_optional(&state.db)
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
    let row: (Option<String>, Option<serde_json::Value>) =
        sqlx::query_as::<_, (Option<String>, Option<serde_json::Value>)>(
            "SELECT content, payload FROM noetl.catalog WHERE catalog_id = $1",
        )
        .bind(catalog_id)
        .fetch_optional(&state.db)
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

/// Generate a snowflake ID.
async fn generate_snowflake_id(state: &AppState) -> AppResult<i64> {
    let row: (i64,) = sqlx::query_as::<_, (i64,)>("SELECT noetl.snowflake_id()")
        .fetch_one(&state.db)
        .await?;

    Ok(row.0)
}

/// Emit playbook_started event.
async fn emit_playbook_started_event(
    state: &AppState,
    execution_id: i64,
    catalog_id: i64,
    path: &str,
    workload: &serde_json::Value,
    parent_execution_id: Option<i64>,
) -> AppResult<i64> {
    let event_id = generate_snowflake_id(state).await?;

    let context = serde_json::json!({
        "catalog_id": catalog_id.to_string(),
        "execution_id": execution_id.to_string(),
        "path": path,
        "workload": workload,
    });

    let meta = serde_json::json!({
        "emitted_at": chrono::Utc::now().to_rfc3339(),
        "emitter": "control_plane",
    });

    sqlx::query(
        r#"
        INSERT INTO noetl.event (
            execution_id, catalog_id, event_id, parent_execution_id,
            event_type, node_id, node_name, node_type, status,
            context, meta, created_at
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12
        )
        "#,
    )
    .bind(execution_id)
    .bind(catalog_id)
    .bind(event_id)
    .bind(parent_execution_id)
    .bind("playbook_started")
    .bind("playbook")
    .bind(path)
    .bind("execution")
    .bind("STARTED")
    .bind(&context)
    .bind(&meta)
    .bind(chrono::Utc::now())
    .execute(&state.db)
    .await?;

    Ok(event_id)
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
) -> AppResult<i64> {
    let event_id = generate_snowflake_id(state).await?;

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
    let cmd_context = serde_json::json!({
        "tool_config": command.tool.config,
        "args": cmd_args,
        "render_context": render_context,
    });

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
            map.insert(
                "iteration_index".to_string(),
                serde_json::json!(iter.index),
            );
            map.insert(
                "iteration_total".to_string(),
                serde_json::json!(iter.total),
            );
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

    sqlx::query(
        r#"
        INSERT INTO noetl.event (
            event_id, execution_id, catalog_id, event_type,
            node_id, node_name, node_type, status,
            context, meta, parent_event_id, created_at
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12
        )
        "#,
    )
    .bind(event_id)
    .bind(execution_id)
    .bind(catalog_id)
    .bind("command.issued")
    .bind(&step.step)
    .bind(&step.step)
    .bind(command.tool.kind.as_str())
    .bind("PENDING")
    .bind(&cmd_context)
    .bind(&cmd_meta)
    .bind(parent_event_id)
    .bind(chrono::Utc::now())
    .execute(&state.db)
    .await?;

    if let Err(e) = insert_command_row(
        state,
        execution_id,
        event_id,
        catalog_id,
        parent_event_id,
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
    )
    .await?;

    Ok(event_id)
}

/// Generate initial commands for the start step.
#[allow(clippy::too_many_arguments)]
async fn generate_initial_commands(
    state: &AppState,
    execution_id: i64,
    catalog_id: i64,
    parent_event_id: i64,
    playbook: &crate::playbook::types::Playbook,
    payload: &HashMap<String, serde_json::Value>,
) -> AppResult<i32> {
    // Find start step
    let start_step = playbook
        .get_step("start")
        .ok_or_else(|| AppError::Validation("Start step 'start' not found".to_string()))?;

    // Build command context by merging playbook workload (defaults) with execution payload (overrides)
    let command_builder = crate::engine::commands::CommandBuilder::new();
    let mut context = HashMap::new();

    // First, add playbook workload defaults
    if let Some(workload) = &playbook.workload {
        if let serde_json::Value::Object(map) = workload {
            for (k, v) in map {
                context.insert(k.clone(), v.clone());
            }
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
    persist_engine_command(
        state,
        execution_id,
        catalog_id,
        parent_event_id,
        start_step,
        &command,
        &context,
        playbook,
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
    step_name: &str,
    tool_kind: &str,
    context: &serde_json::Value,
    meta: &serde_json::Value,
) -> AppResult<()> {
    let command_id = generate_snowflake_id(state).await?;
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
            context, meta, latest_event_id
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11
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
    .execute(&state.db)
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
async fn publish_command_notification(
    state: &AppState,
    execution_id: i64,
    event_id: i64,
    command_id: &str,
    step: &str,
    _tool_kind: &str,
    playbook: &crate::playbook::types::Playbook,
) -> AppResult<()> {
    let Some(nats_client) = state.nats.as_ref() else {
        tracing::warn!(
            execution_id,
            event_id,
            "NATS not configured; command notification skipped — worker won't claim this command"
        );
        return Ok(());
    };

    // Pool segment from the playbook's catalog path.  The
    // `Playbook` type's `metadata.path` is `Option<String>`; if
    // unset we default to `shared` (the same default Python uses).
    let pool_segment = match playbook.metadata.path.as_deref() {
        Some(p) if p.starts_with("system/") => "system",
        _ => "shared",
    };
    let subject = format!("noetl.commands.{}.{}", pool_segment, execution_id);

    let server_url = state
        .config
        .public_server_url
        .clone()
        .unwrap_or_else(|| "http://localhost:8082".to_string());

    let notification = serde_json::json!({
        "execution_id": execution_id,
        "event_id": event_id,
        "command_id": command_id,
        "step": step,
        "server_url": server_url,
    });
    let payload = serde_json::to_vec(&notification)
        .map_err(|e| AppError::Internal(format!("Serialize command notification: {e}")))?;

    let js = async_nats::jetstream::new((**nats_client).clone());
    js.publish(subject.clone(), payload.into())
        .await
        .map_err(|e| AppError::Internal(format!("NATS publish failed: {e}")))?
        .await
        .map_err(|e| AppError::Internal(format!("NATS publish ack failed: {e}")))?;

    tracing::info!(
        execution_id,
        event_id,
        %subject,
        command_id = %command_id,
        "Published command notification to NATS"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_execute_request_validation() {
        let request = ExecuteRequest {
            path: None,
            catalog_id: None,
            payload: HashMap::new(),
            parent_execution_id: None,
        };
        assert!(request.validate().is_err());

        let request = ExecuteRequest {
            path: Some("test/playbook".to_string()),
            catalog_id: None,
            payload: HashMap::new(),
            parent_execution_id: None,
        };
        assert!(request.validate().is_ok());

        let request = ExecuteRequest {
            path: None,
            catalog_id: Some(12345),
            payload: HashMap::new(),
            parent_execution_id: None,
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
}
