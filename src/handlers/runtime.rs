//! Runtime management API handlers.
//!
//! Handles worker pool registration, heartbeat, and listing.

use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::services::runtime::{RegisterRuntimeRequest, RuntimeFilter, RuntimeService};

/// Request for deregistering a runtime.
///
/// Accepts both `kind` (canonical) and `component_type` (what the
/// Rust noetl-worker and the Python server send) — see
/// noetl/ai-meta#53 Gap 2.  Defaults `kind` to `worker_pool` when
/// neither is present.
#[derive(Debug, Clone, Deserialize)]
pub struct DeregisterRequest {
    #[serde(default = "default_kind", alias = "component_type")]
    pub kind: String,
    pub name: String,
}

/// Request for heartbeat.
///
/// The Rust noetl-worker sends only `{name}` for heartbeats (the
/// Python broker upserts by name alone), so `kind` is optional
/// here and defaults to `worker_pool`.  The `component_type`
/// alias is accepted too for clients that include it.
#[derive(Debug, Clone, Deserialize)]
pub struct HeartbeatRequest {
    #[serde(default = "default_kind", alias = "component_type")]
    pub kind: String,
    pub name: String,
}

fn default_kind() -> String {
    "worker_pool".to_string()
}

/// Response for runtime operations.
#[derive(Debug, Clone, Serialize)]
pub struct RuntimeOperationResponse {
    pub status: String,
    pub message: String,
}

/// Query parameters for listing runtimes.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListRuntimesQuery {
    pub kind: Option<String>,
    pub status: Option<String>,
    pub name: Option<String>,
}

/// Register a worker pool.
///
/// POST /api/worker/pool/register
pub async fn register_pool(
    service: State<RuntimeService>,
    request: Json<RegisterRuntimeRequest>,
) -> Result<Json<crate::services::runtime::Runtime>, AppError> {
    let started_at = std::time::Instant::now();
    let result = register_pool_inner(service, request).await;
    let status_label = if result.is_ok() { "ok" } else { "error" };
    crate::metrics::record_write_request(
        crate::metrics::endpoint::RUNTIME_REGISTER,
        status_label,
        started_at.elapsed().as_secs_f64(),
    );
    result
}

async fn register_pool_inner(
    State(service): State<RuntimeService>,
    Json(request): Json<RegisterRuntimeRequest>,
) -> Result<Json<crate::services::runtime::Runtime>, AppError> {
    let runtime = service.register(&request).await?;
    Ok(Json(runtime))
}

/// Deregister a worker pool.
///
/// DELETE /api/worker/pool/deregister
pub async fn deregister_pool(
    State(service): State<RuntimeService>,
    Json(request): Json<DeregisterRequest>,
) -> Result<Json<RuntimeOperationResponse>, AppError> {
    service.deregister(&request.kind, &request.name).await?;
    Ok(Json(RuntimeOperationResponse {
        status: "ok".to_string(),
        message: format!("Runtime {} {} deregistered", request.kind, request.name),
    }))
}

/// Send heartbeat for a worker pool.
///
/// POST /api/worker/pool/heartbeat
pub async fn heartbeat(
    service: State<RuntimeService>,
    request: Json<HeartbeatRequest>,
) -> Result<Json<RuntimeOperationResponse>, AppError> {
    let started_at = std::time::Instant::now();
    let result = heartbeat_inner(service, request).await;
    let status_label = if result.is_ok() { "ok" } else { "error" };
    crate::metrics::record_write_request(
        crate::metrics::endpoint::RUNTIME_HEARTBEAT,
        status_label,
        started_at.elapsed().as_secs_f64(),
    );
    result
}

async fn heartbeat_inner(
    State(service): State<RuntimeService>,
    Json(request): Json<HeartbeatRequest>,
) -> Result<Json<RuntimeOperationResponse>, AppError> {
    service.heartbeat(&request.kind, &request.name).await?;
    Ok(Json(RuntimeOperationResponse {
        status: "ok".to_string(),
        message: "Heartbeat recorded".to_string(),
    }))
}

/// List worker pools.
///
/// GET /api/worker/pools
pub async fn list_pools(
    State(service): State<RuntimeService>,
    Query(query): Query<ListRuntimesQuery>,
) -> Result<Json<Vec<crate::services::runtime::Runtime>>, AppError> {
    let filter = RuntimeFilter {
        kind: query.kind.or(Some("worker_pool".to_string())),
        status: query.status,
        name: query.name,
    };

    let pools = service.list(&filter).await?;
    Ok(Json(pools))
}

/// List all runtimes.
///
/// GET /api/runtimes (route unwired pending Python parity backport — see
/// noetl/ai-meta#49 Phase A round 4 + the follow-up issue tracking the
/// `/api/runtimes` Python addition).  Handler retained so re-wiring is a
/// one-line `main.rs` edit when Python lands the equivalent endpoint.
#[allow(dead_code)]
pub async fn list_all(
    State(service): State<RuntimeService>,
    Query(query): Query<ListRuntimesQuery>,
) -> Result<Json<Vec<crate::services::runtime::Runtime>>, AppError> {
    let filter = RuntimeFilter {
        kind: query.kind,
        status: query.status,
        name: query.name,
    };

    let runtimes = service.list(&filter).await?;
    Ok(Json(runtimes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deregister_request_deserialization() {
        let json = r#"{"kind": "worker_pool", "name": "worker-1"}"#;
        let request: DeregisterRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.kind, "worker_pool");
        assert_eq!(request.name, "worker-1");
    }

    #[test]
    fn test_heartbeat_request_deserialization() {
        let json = r#"{"kind": "worker_pool", "name": "worker-1"}"#;
        let request: HeartbeatRequest = serde_json::from_str(json).unwrap();
        assert_eq!(request.kind, "worker_pool");
        assert_eq!(request.name, "worker-1");
    }

    #[test]
    fn test_operation_response_serialization() {
        let response = RuntimeOperationResponse {
            status: "ok".to_string(),
            message: "Success".to_string(),
        };

        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("ok"));
        assert!(json.contains("Success"));
    }
}
