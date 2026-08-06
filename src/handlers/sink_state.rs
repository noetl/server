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
//! directly to `noetl.*` ([`data-access-boundary.md`](https://github.com/noetl/ai-meta/blob/main/agents/rules/data-access-boundary.md)). Both endpoints are
//! idempotent, so a worker retry / redelivery never corrupts the set.
//!
//! - `POST /api/internal/sink-state/mark` — record an execution pending-sink.
//! - `POST /api/internal/sink-state/confirm` — clear it (context sunk).
//! - `POST /api/internal/sink-state/release` — clear it WITHOUT claiming the
//!   context was sunk (the sink attempt failed or moved to an async callback).

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

/// `POST /api/internal/sink-state/release` body (noetl/ai-meta#248).
#[derive(Debug, Clone, Deserialize)]
pub struct SinkReleaseRequest {
    pub execution_id: i64,
    /// Why the mark is being cleared without a sink having happened —
    /// `released_failed` / `released_pending_callback` / `released_error`.
    /// Diagnostic only; the row is cleared either way.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Response for these endpoints.
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

/// `POST /api/internal/sink-state/release` — clear an execution's pending-sink
/// state WITHOUT asserting its context was sunk (noetl/ai-meta#248).
///
/// The worker marks an execution for the duration of a sink attempt. When that
/// attempt ends any way other than clean success — a non-success result, an
/// error, or a hand-off to an async callback the reporting process never
/// observes — the mark must still be cleared on this feed, or the row is
/// retained forever and the result-tier GC never reclaims that execution's
/// objects.
///
/// Deliberately a separate endpoint rather than reusing `confirm`. The DB effect
/// is identical (the row goes away), but `confirm` is a claim that the
/// customer's system of record has the data, and a failed sink has not earned
/// that claim. Keeping them distinct means `noetl_sink_state_total{op}`
/// distinguishes "sunk" from "gave up", which is the difference an operator
/// actually needs.
///
/// Idempotent: releasing an absent row is a no-op.
#[tracing::instrument(skip(deps, _token, req), fields(execution_id = req.execution_id))]
pub async fn release(
    State(deps): State<SinkStateDeps>,
    _token: RequireInternalApiToken,
    Json(req): Json<SinkReleaseRequest>,
) -> AppResult<Json<SinkStateResponse>> {
    let changed = sink_pending::confirm(&deps.pool, req.execution_id).await?;
    crate::metrics::record_sink_state("release");
    if changed {
        tracing::debug!(
            execution_id = req.execution_id,
            reason = req.reason.as_deref().unwrap_or("unspecified"),
            "sink-state released without a sink confirmation"
        );
    }
    Ok(Json(SinkStateResponse {
        execution_id: req.execution_id,
        changed,
    }))
}
