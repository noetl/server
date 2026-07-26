//! Sink-state feed HTTP endpoints
//! ([noetl/ai-meta#199](https://github.com/noetl/ai-meta/issues/199) Slice B).
//!
//! The write-behind-cache invariant ("never GC un-sunk business context") needs
//! the server-side Feather result-tier GC to know which executions still hold
//! business context that has not been sunk to the customer's system of record.
//! That state lives in the worker's in-process sink gate (worker#190) — a
//! different process — so the worker reports it here and the GC reads it from
//! `noetl.sink_pending` ([`crate::db::queries::sink_pending`]).
//!
//! Internal-only surface, gated by `RequireInternalApiToken` like the sibling
//! `/api/internal/result-tier/gc` route — workers write through the API, never
//! directly to `noetl.*` ([`data-access-boundary.md`]). Both endpoints are
//! idempotent, so a worker retry / redelivery never corrupts the set.
//!
//! - `POST /api/internal/sink-state/mark` — record an execution pending-sink.
//! - `POST /api/internal/sink-state/confirm` — clear it (context sunk).

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::db::queries::sink_pending;
use crate::db::DbPool;
use crate::error::AppResult;
use crate::handlers::internal::RequireInternalApiToken;

/// State bundle — just the pool (the table is owned end-to-end by the server).
#[derive(Clone)]
pub struct SinkStateDeps {
    pub pool: DbPool,
}

/// `POST /api/internal/sink-state/mark` body.
#[derive(Debug, Clone, Deserialize)]
pub struct SinkMarkRequest {
    pub execution_id: i64,
    /// The reporting worker (diagnostic — which worker holds the un-sunk context).
    #[serde(default)]
    pub worker_id: Option<String>,
}

/// `POST /api/internal/sink-state/confirm` body.
#[derive(Debug, Clone, Deserialize)]
pub struct SinkConfirmRequest {
    pub execution_id: i64,
}

/// Response for both endpoints.
#[derive(Debug, Clone, Serialize)]
pub struct SinkStateResponse {
    pub execution_id: i64,
    /// For `confirm`: whether a pending row was actually cleared. For `mark`:
    /// always true (upsert).
    pub changed: bool,
}

/// `POST /api/internal/sink-state/mark` — record an execution as holding un-sunk
/// business context. Idempotent upsert.
#[tracing::instrument(skip(deps, _token, req), fields(execution_id = req.execution_id))]
pub async fn mark(
    State(deps): State<SinkStateDeps>,
    _token: RequireInternalApiToken,
    Json(req): Json<SinkMarkRequest>,
) -> AppResult<Json<SinkStateResponse>> {
    sink_pending::mark(&deps.pool, req.execution_id, req.worker_id.as_deref()).await?;
    crate::metrics::record_sink_state("mark");
    Ok(Json(SinkStateResponse {
        execution_id: req.execution_id,
        changed: true,
    }))
}

/// `POST /api/internal/sink-state/confirm` — clear an execution's pending-sink
/// state (its context was sunk). Idempotent: clearing an absent row is a no-op.
#[tracing::instrument(skip(deps, _token, req), fields(execution_id = req.execution_id))]
pub async fn confirm(
    State(deps): State<SinkStateDeps>,
    _token: RequireInternalApiToken,
    Json(req): Json<SinkConfirmRequest>,
) -> AppResult<Json<SinkStateResponse>> {
    let changed = sink_pending::confirm(&deps.pool, req.execution_id).await?;
    crate::metrics::record_sink_state("confirm");
    Ok(Json(SinkStateResponse {
        execution_id: req.execution_id,
        changed,
    }))
}
