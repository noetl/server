//! EHDB Data Query Interface — read-only query API over NoETL platform data.
//!
//! This module exposes the **read surface** for the EHDB platform tiers
//! (event-log, projection, KV, object, vector) under the `/api/ehdb/...`
//! namespace. It is the server-side half of the EHDB Data Query Interface
//! (noetl/ai-meta#178); the `noetl ehdb query ...` CLI is the client half.
//!
//! ## Control-plane boundary (load-bearing)
//!
//! The server stays **control-plane**. It serves *projection / read-model*
//! queries — list executions, execution state, event read-model — **directly**
//! from the read-model store it already reads (Postgres `noetl.event`), via the
//! existing [`ExecutionService`]. It does **not** open EHDB data-plane tier
//! storage: no EHDB engine is linked into this binary, honouring the same guard
//! `worker/src/ehdb/guard.rs` enforces (roles `server` / `api` / `gateway` are
//! refused data-plane access).
//!
//! For **raw data-plane tier queries** (raw event-log scan, KV, object, vector)
//! the server ROUTES the query to the worker / system data-plane over the drive
//! rather than reading tier storage itself. That routing seam is documented and
//! stubbed here ([`raw_tier_query`]) as an extension point for the follow-up
//! slices; it returns `501 Not Implemented` with the routing contract until the
//! worker-side query handler lands.
//!
//! ## Secret-free by construction
//!
//! Every response DTO here carries only **projected read-model columns** —
//! ids, status, node names, counts, timestamps, paths. The `result` / `error` /
//! `context` / `workload` payload bodies (which can carry credential material)
//! are never selected or surfaced. This mirrors the payload-free discipline of
//! `ehdb_reference::projection::{ExecutionStateView, EventReadModelView}`, so
//! there is no credential surface to scrub — secret-free is structural, not a
//! post-hoc filter.
//!
//! ## Read-only + bounded
//!
//! All endpoints are `GET` (no mutation). Every list/scan is bounded by a
//! capped `limit` with a forward `after` cursor for pagination.
//!
//! ## Auth posture
//!
//! These read endpoints follow the same posture as the `/api/executions`
//! endpoints they mirror: the gateway is the auth-enforcement point in front of
//! the server; the server exposes the read-model read scope. No mutation scope
//! is reachable through this module.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::AppError;
use crate::services::execution::{ExecutionFilter, ExecutionService};

/// Cap on rows any single EHDB list/scan will return.
const MAX_EHDB_LIMIT: i32 = 1000;
/// Default page size for the execution list.
const DEFAULT_EXECUTIONS_LIMIT: i32 = 50;
/// Default page size for event reads.
const DEFAULT_EVENTS_LIMIT: i32 = 100;

/// A read-model row for one execution's derived state.
///
/// Mirrors the shape of `ehdb_reference::projection::ExecutionStateView`
/// (secret-free). `execution_id` / `catalog_id` / `parent_execution_id` are
/// stringified so JSON consumers (JS `Number` tops out at 2^53) don't lose
/// precision on 64-bit snowflake ids.
///
/// In the **list** projection `current_node` and `parent_execution_id` are
/// `null` (not loaded by the bounded list aggregation); fetch the single
/// execution for those.
#[derive(Debug, Clone, Serialize)]
pub struct EhdbExecutionState {
    pub execution_id: String,
    pub catalog_id: String,
    pub path: Option<String>,
    pub status: String,
    /// `true` once a terminal marker is folded (COMPLETED / FAILED / CANCELLED).
    pub terminal: bool,
    pub current_node: Option<String>,
    pub parent_execution_id: Option<String>,
    pub event_count: u64,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

/// A read-model row for one event — the projected columns only.
///
/// Mirrors `ehdb_reference::projection::EventReadModelView` (secret-free): the
/// `result` / `error` / `context` payload bodies are deliberately omitted.
#[derive(Debug, Clone, Serialize)]
pub struct EhdbEventReadModel {
    pub event_id: String,
    pub execution_id: String,
    pub event_type: String,
    pub node_name: Option<String>,
    pub status: Option<String>,
    pub created_at: DateTime<Utc>,
}

fn is_terminal(status: &str) -> bool {
    matches!(
        status.to_ascii_uppercase().as_str(),
        "COMPLETED" | "FAILED" | "CANCELLED"
    )
}

/// `GET /api/ehdb` — interface manifest.
///
/// Advertises the read-only endpoints and the data-plane tier routing status so
/// the CLI (and humans) can discover the surface without external docs.
pub async fn index() -> Json<serde_json::Value> {
    Json(json!({
        "action": "ehdb.index",
        "service": "ehdb-query-interface",
        "read_only": true,
        "control_plane": true,
        "description": "Read-only query API over NoETL platform read-model data. \
                        Server serves projection/read-model tiers directly and \
                        routes raw data-plane tier queries to the worker.",
        "endpoints": [
            {"method": "GET", "path": "/api/ehdb/executions", "tier": "projection",
             "serves": "direct", "desc": "List executions (read-model)."},
            {"method": "GET", "path": "/api/ehdb/executions/{execution_id}", "tier": "projection",
             "serves": "direct", "desc": "Execution derived-state read-model."},
            {"method": "GET", "path": "/api/ehdb/executions/{execution_id}/events", "tier": "projection",
             "serves": "direct", "desc": "Event read-model scoped to one execution."},
            {"method": "GET", "path": "/api/ehdb/events", "tier": "eventlog",
             "serves": "direct", "desc": "Event read-model scan by global sequence."},
            {"method": "GET", "path": "/api/ehdb/tiers/{tier}", "tier": "eventlog|kv|object|vector",
             "serves": "routed", "desc": "Raw data-plane tier query — routed to worker (stub)."}
        ],
        "tiers": {
            "projection": "served-direct",
            "eventlog": "read-model served-direct; raw scan routed",
            "kv": "routed (not-yet-wired)",
            "object": "routed (not-yet-wired)",
            "vector": "routed (not-yet-wired)"
        }
    }))
}

/// Query parameters for the execution list.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListExecutionsQuery {
    pub status: Option<String>,
    pub path: Option<String>,
    pub catalog_id: Option<i64>,
    pub limit: Option<i32>,
    pub offset: Option<i32>,
}

/// `GET /api/ehdb/executions` — list executions (projection read-model).
///
/// Served directly from the read-model via [`ExecutionService::list`].
pub async fn list_executions(
    State(service): State<ExecutionService>,
    Query(query): Query<ListExecutionsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_EXECUTIONS_LIMIT)
        .clamp(1, MAX_EHDB_LIMIT);
    let offset = query.offset.unwrap_or(0).max(0);

    let filter = ExecutionFilter {
        catalog_id: query.catalog_id,
        path: query.path,
        status: query.status,
        limit: Some(limit),
        offset: Some(offset),
    };

    let summaries = service.list(&filter).await?;
    let executions: Vec<EhdbExecutionState> = summaries
        .into_iter()
        .map(|s| EhdbExecutionState {
            execution_id: s.execution_id.to_string(),
            catalog_id: s.catalog_id.to_string(),
            path: s.path,
            terminal: is_terminal(&s.status),
            status: s.status,
            current_node: None,
            parent_execution_id: None,
            event_count: s.event_count.max(0) as u64,
            started_at: Some(s.started_at),
            completed_at: s.completed_at,
        })
        .collect();

    Ok(Json(json!({
        "action": "ehdb.executions.list",
        "tier": "projection",
        "limit": limit,
        "offset": offset,
        "returned": executions.len(),
        "executions": executions,
    })))
}

/// `GET /api/ehdb/executions/{execution_id}` — execution state read-model.
///
/// Served directly via [`ExecutionService::get`]; `status` reuses the server's
/// existing terminal-priority derivation (no reinvented query logic).
pub async fn get_execution_state(
    State(service): State<ExecutionService>,
    Path(execution_id): Path<i64>,
) -> Result<Json<serde_json::Value>, AppError> {
    match service.get(execution_id).await {
        Ok(detail) => {
            // current_node = the node of the most recent event that named one.
            // `detail.events` is chronological ASC; walk from the newest back.
            let current_node = detail.events.iter().rev().find_map(|e| e.node_name.clone());
            let state = EhdbExecutionState {
                execution_id: detail.execution_id.to_string(),
                catalog_id: detail.catalog_id.to_string(),
                path: detail.path,
                terminal: is_terminal(&detail.status),
                status: detail.status,
                current_node,
                parent_execution_id: detail.parent_execution_id.map(|p| p.to_string()),
                event_count: detail.events.len() as u64,
                started_at: Some(detail.started_at),
                completed_at: detail.completed_at,
            };
            Ok(Json(json!({
                "action": "ehdb.execution.state",
                "tier": "projection",
                "execution_id": execution_id.to_string(),
                "exists": true,
                "state": state,
            })))
        }
        // Absent execution → `exists: false` (read-model semantics), not a 404,
        // matching `ehdb_reference::projection::ProjectionReadExecutionOutcome`.
        Err(AppError::NotFound(_)) => Ok(Json(json!({
            "action": "ehdb.execution.state",
            "tier": "projection",
            "execution_id": execution_id.to_string(),
            "exists": false,
            "state": serde_json::Value::Null,
        }))),
        Err(e) => Err(e),
    }
}

/// Query parameters for event reads.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct EventsQuery {
    /// Forward cursor: return events with `event_id > after`.
    pub after: Option<i64>,
    pub limit: Option<i32>,
}

/// `GET /api/ehdb/executions/{execution_id}/events` — event read-model for one
/// execution.
pub async fn list_execution_events(
    State(service): State<ExecutionService>,
    Path(execution_id): Path<i64>,
    Query(query): Query<EventsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_EVENTS_LIMIT)
        .clamp(1, MAX_EHDB_LIMIT);

    let rows = service
        .ehdb_events_by_execution(execution_id, query.after, limit as i64)
        .await?;

    let events: Vec<EhdbEventReadModel> = rows
        .into_iter()
        .map(
            |(event_id, event_type, node_name, status, created_at)| EhdbEventReadModel {
                event_id: event_id.to_string(),
                execution_id: execution_id.to_string(),
                event_type,
                node_name,
                status,
                created_at,
            },
        )
        .collect();

    let next_cursor = if events.len() as i32 == limit {
        events.last().map(|e| e.event_id.clone())
    } else {
        None
    };

    Ok(Json(json!({
        "action": "ehdb.execution.events",
        "tier": "projection",
        "execution_id": execution_id.to_string(),
        "exists": !events.is_empty(),
        "limit": limit,
        "returned": events.len(),
        "next_cursor": next_cursor,
        "events": events,
    })))
}

/// `GET /api/ehdb/events` — event read-model scan by global sequence.
pub async fn scan_events(
    State(service): State<ExecutionService>,
    Query(query): Query<EventsQuery>,
) -> Result<Json<serde_json::Value>, AppError> {
    let limit = query
        .limit
        .unwrap_or(DEFAULT_EVENTS_LIMIT)
        .clamp(1, MAX_EHDB_LIMIT);

    let rows = service.ehdb_events_scan(query.after, limit as i64).await?;

    let events: Vec<EhdbEventReadModel> = rows
        .into_iter()
        .map(
            |(event_id, execution_id, event_type, node_name, status, created_at)| {
                EhdbEventReadModel {
                    event_id: event_id.to_string(),
                    execution_id: execution_id.to_string(),
                    event_type,
                    node_name,
                    status,
                    created_at,
                }
            },
        )
        .collect();

    let next_cursor = if events.len() as i32 == limit {
        events.last().map(|e| e.event_id.clone())
    } else {
        None
    };

    Ok(Json(json!({
        "action": "ehdb.events.scan",
        "tier": "eventlog",
        "limit": limit,
        "returned": events.len(),
        "next_cursor": next_cursor,
        "events": events,
    })))
}

/// `GET /api/ehdb/tiers/{tier}` — raw data-plane tier query routing seam.
///
/// This is the documented extension point for the raw data-plane tiers
/// (`eventlog` raw scan, `kv`, `object`, `vector`). The server is barred from
/// opening tier storage directly (control-plane guard), so a raw tier query is
/// **routed to the worker / system data-plane over the drive**. Until that
/// worker-side query handler lands, this returns `501 Not Implemented` with the
/// routing contract so callers get a precise, machine-readable stub rather than
/// a 404.
///
/// ### Routing contract (follow-up slices)
///
/// 1. Server validates `tier` ∈ {eventlog, kv, object, vector} and the
///    read-only tier request (bounded limit, secret-free projection).
/// 2. Server enqueues an internal query command on the drive addressed to the
///    system data-plane pool (NATS subject; the same mechanism the server uses
///    to dispatch work to the worker), carrying `tier` + request JSON +
///    `execution_id` correlation.
/// 3. Worker guards the request (`worker/src/ehdb/guard.rs` — data-plane roles
///    only), resolves the backend for the tier
///    (`worker/src/ehdb/backends.rs::resolve`), and invokes the matching
///    `ehdb_reference` driver read (`EventLogDriver::scan_global`,
///    `KvStateDriver::get/scan`, `ObjectBlobDriver::get/list`,
///    `VectorDriver::query`).
/// 4. Worker returns the tier `*Outcome` (already `Serialize` + secret-free) on
///    a query-response subject; server relays it as the HTTP body.
pub async fn raw_tier_query(Path(tier): Path<String>) -> impl IntoResponse {
    let tier_l = tier.to_ascii_lowercase();
    let known = matches!(tier_l.as_str(), "eventlog" | "kv" | "object" | "vector");
    if !known {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "action": "ehdb.tier.query",
                "tier": tier,
                "error": "unknown tier",
                "known_tiers": ["eventlog", "kv", "object", "vector"],
            })),
        );
    }
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "action": "ehdb.tier.query",
            "tier": tier_l,
            "status": "routing-not-wired",
            "read_only": true,
            "control_plane_guard": "server does not open data-plane tier storage; \
                                    raw tier queries route to the worker/system data-plane",
            "contract": {
                "1_validate": "server validates tier + bounded read-only request",
                "2_dispatch": "server enqueues an internal query command on the drive \
                               to the system data-plane pool (NATS), carrying tier + \
                               request + execution_id",
                "3_execute": "worker guards (data-plane role only), resolves the tier \
                              backend, invokes the ehdb_reference driver read",
                "4_relay": "worker returns the tier *Outcome (Serialize, secret-free); \
                            server relays it as the HTTP body"
            },
            "tracks": "noetl/ai-meta#178"
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_detection_is_case_insensitive() {
        assert!(is_terminal("COMPLETED"));
        assert!(is_terminal("failed"));
        assert!(is_terminal("Cancelled"));
        assert!(!is_terminal("RUNNING"));
        assert!(!is_terminal("running"));
        assert!(!is_terminal("UNKNOWN"));
    }

    #[test]
    fn event_read_model_is_payload_free() {
        // The DTO must not carry any payload-bearing field: serialize a row and
        // assert the JSON keys are exactly the projected columns.
        let ev = EhdbEventReadModel {
            event_id: "123".to_string(),
            execution_id: "456".to_string(),
            event_type: "playbook.started".to_string(),
            node_name: Some("step_1".to_string()),
            status: Some("RUNNING".to_string()),
            created_at: Utc::now(),
        };
        let v = serde_json::to_value(&ev).unwrap();
        let obj = v.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "created_at",
                "event_id",
                "event_type",
                "execution_id",
                "node_name",
                "status",
            ]
        );
        // No payload bodies leaked.
        for banned in ["result", "error", "context", "payload", "workload", "meta"] {
            assert!(!obj.contains_key(banned), "leaked payload field: {banned}");
        }
    }

    #[test]
    fn execution_state_is_payload_free() {
        let st = EhdbExecutionState {
            execution_id: "1".to_string(),
            catalog_id: "2".to_string(),
            path: Some("weather/forecast".to_string()),
            status: "COMPLETED".to_string(),
            terminal: true,
            current_node: Some("end".to_string()),
            parent_execution_id: None,
            event_count: 7,
            started_at: Some(Utc::now()),
            completed_at: Some(Utc::now()),
        };
        let v = serde_json::to_value(&st).unwrap();
        let obj = v.as_object().unwrap();
        for banned in ["result", "error", "context", "payload", "workload", "meta"] {
            assert!(!obj.contains_key(banned), "leaked payload field: {banned}");
        }
    }
}
