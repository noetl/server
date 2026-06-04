//! Health check endpoints for the NoETL Control Plane API.

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::db::pool::health_check as db_health_check;
use crate::state::AppState;

/// Health check response.
#[derive(Debug, Serialize, Deserialize)]
pub struct HealthCheckResponse {
    /// Health status ("ok" or "unhealthy")
    pub status: String,
}

/// Detailed health check response for the API.
#[derive(Debug, Serialize, Deserialize)]
pub struct ApiHealthResponse {
    /// Overall health status
    pub status: String,

    /// Database connectivity status
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,

    /// NATS connectivity status
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nats: Option<String>,

    /// Server uptime in seconds
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_seconds: Option<u64>,

    /// Server version
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// DB pool telemetry response.
#[derive(Debug, Serialize, Deserialize)]
pub struct PoolStatusResponse {
    pub pool_min: u32,
    pub pool_max: u32,
    pub pool_size: u32,
    pub pool_available: u32,
    pub requests_waiting: u32,
    pub utilization: f64,
    pub slots_available: u32,
    pub status: String,
}

/// Basic health check endpoint.
///
/// `GET /health`
///
/// Returns a simple health status. This endpoint is suitable for
/// load balancer health checks as it returns quickly.
///
/// # Returns
///
/// - `200 OK` with `{"status": "ok"}` if the server is running
pub async fn health_check() -> Json<HealthCheckResponse> {
    Json(HealthCheckResponse {
        status: "ok".to_string(),
    })
}

/// Detailed API health check endpoint.
///
/// `GET /api/health`
///
/// Returns detailed health status including database and NATS connectivity.
///
/// # Arguments
///
/// * `state` - Application state containing database pool and NATS client
///
/// # Returns
///
/// - `200 OK` with detailed health status if all services are healthy
/// - `503 Service Unavailable` if any critical service is unhealthy
pub async fn api_health(State(state): State<AppState>) -> (StatusCode, Json<ApiHealthResponse>) {
    // Phase F R4-3c: probe the cluster pool — that's the
    // "can this replica answer at all" check.  Per-shard health
    // is a separate concern (a future /api/health/shards endpoint
    // would iterate state.pools.all_shards() and probe each); not
    // needed for the basic readiness signal.
    let db_healthy = db_health_check(state.pools.cluster()).await;

    let nats_status = if state.has_nats() {
        Some("connected".to_string())
    } else {
        Some("not_configured".to_string())
    };

    let overall_status = if db_healthy {
        "ok".to_string()
    } else {
        "unhealthy".to_string()
    };

    let status_code = if db_healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    let response = ApiHealthResponse {
        status: overall_status,
        database: Some(if db_healthy {
            "connected".to_string()
        } else {
            "disconnected".to_string()
        }),
        nats: nats_status,
        uptime_seconds: Some(state.uptime_seconds()),
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
    };

    (status_code, Json(response))
}

/// Real-time DB pool telemetry.
///
/// `GET /api/pool/status`
pub async fn pool_status(State(state): State<AppState>) -> Json<PoolStatusResponse> {
    // Phase F R4-3c: report cluster pool stats here.  In
    // single-pool fallback mode (NOETL_SHARDS empty) the cluster
    // handle IS the only pool, so the numbers are unchanged from
    // pre-R4.  In sharded mode this endpoint surfaces just the
    // cluster pool — per-shard pool utilization belongs on
    // /metrics (per-shard gauge labels) or a follow-up
    // /api/pool/status/shards endpoint.
    let cluster = state.pools.cluster();
    let pool_size = u32::try_from(cluster.size()).unwrap_or(u32::MAX);
    let pool_available = u32::try_from(cluster.num_idle()).unwrap_or(u32::MAX);
    let pool_max = pool_size.max(pool_available);
    let pool_min = 0;
    let active = pool_size.saturating_sub(pool_available);
    let utilization = if pool_max > 0 {
        (active as f64 / pool_max as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };

    Json(PoolStatusResponse {
        pool_min,
        pool_max,
        pool_size,
        pool_available,
        requests_waiting: 0,
        utilization,
        slots_available: pool_available,
        status: "ok".to_string(),
    })
}

/// Prometheus metrics endpoint.
///
/// `GET /metrics`
///
/// Returns the text-exposition format documented at
/// <https://prometheus.io/docs/instrumenting/exposition_formats/>.
/// Routed only when `disable_metrics` is `false` in
/// [`AppConfig`].  Per
/// [`agents/rules/observability.md`](https://github.com/noetl/ai-meta/blob/main/agents/rules/observability.md)
/// Principles 1+2, every substantive code change ships a
/// counter/histogram alongside the implementation; this endpoint
/// is the surface those metrics are scraped from.
pub async fn metrics() -> Response {
    match crate::metrics::gather_text() {
        Ok(text) => (
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            text,
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "Failed to gather Prometheus metrics");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to render metrics: {e}"),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_health_check() {
        let response = health_check().await;
        assert_eq!(response.status, "ok");
    }

    #[tokio::test]
    async fn test_metrics_endpoint_returns_ok() {
        // Ensure at least one observation exists in the registry so
        // the response is non-trivial.  This also covers the happy
        // path of `gather_text` end-to-end through the handler.
        crate::metrics::record_event_ingest("test.metrics_endpoint", "ok", 0.001);
        let response = metrics().await;
        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            content_type.contains("text/plain"),
            "expected text/plain, got: {content_type}"
        );
    }
}
