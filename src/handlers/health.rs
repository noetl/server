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

    // NATS is gone (noetl/ai-meta#212). The field is retained as a constant so
    // an existing health scraper does not break on a missing key.
    let nats_status = Some("removed".to_string());

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
    let pool_size = cluster.size();
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
/// Compose the scrape body: the process registry's text plus the embedded L0
/// engine's own exposition, when there is one.
///
/// Split out so the composition is testable without an embedded engine: the
/// engine lives behind a process-wide `OnceLock` keyed off an env flag, which a
/// unit test cannot set up reliably.
pub(crate) fn compose_metrics(base: String, l0: Option<String>) -> String {
    match l0 {
        Some(extra) if !extra.is_empty() => {
            let mut out = base;
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&extra);
            out
        }
        _ => base,
    }
}

pub async fn metrics() -> Response {
    match crate::metrics::gather_text() {
        Ok(text) => {
            let text = compose_metrics(
                text,
                crate::handlers::ehdb_embedded::render_l0_metrics(),
            );
            (
                StatusCode::OK,
                [(
                    header::CONTENT_TYPE,
                    "text/plain; version=0.0.4; charset=utf-8",
                )],
                text,
            )
                .into_response()
        }
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

/// `GET /api/health/ready` — readiness that exercises a real read path.
///
/// ⚠⚠ This exists because `/api/health` returned **200 for the entire six
/// minutes** that `/api/catalog/list` was returning HTTP 500 in production
/// (noetl/server#443: a `NULL`-selected column decoded against a non-`Option`
/// field). The rollout could not halt itself, because nothing it probed was
/// broken.
///
/// A readiness probe that only asks "is the process up" cannot stop a bad image
/// from taking traffic. This one performs the **smallest real query on the path
/// that broke**: one row, bodies off — which is precisely the shape that fails
/// when a `NULL`-selected column meets a non-`Option` field, because the failure
/// happens at *decode*, not at connect.
///
/// ⚠ Deliberately bounded: `limit = 1` and `include_content = false`, so it is
/// cheap enough to run every 10s. It must never become a reason the server is
/// slow.
///
/// ⚠ Returns 503 (not 500) on failure so the kubelet reads it as "not ready"
/// rather than a crash — an unready pod stops receiving traffic and halts the
/// rollout; a crashing one restarts and retries the same bad image.
pub async fn readiness(
    State(state): State<crate::state::AppState>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, Json<serde_json::Value>)> {
    use crate::db::queries::catalog::{list_catalog_entries, CatalogListOptions};

    let opts = CatalogListOptions {
        limit: Some(1),
        include_content: false,
        ..Default::default()
    };
    match list_catalog_entries(state.pools.cluster(), None, true, &opts).await {
        Ok((rows, _total)) => Ok(Json(serde_json::json!({
            "status": "ready",
            // Reported so a human reading the probe's output can tell "the query
            // ran and the catalog is empty" from "the query ran and decoded a
            // row" — only the second exercises the decode this guards.
            "rows_decoded": rows.len(),
        }))),
        Err(e) => Err((
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "status": "not_ready",
                "reason": "catalog read path failed",
                "error": e.to_string(),
            })),
        )),
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

#[cfg(test)]
mod l0_exposition_tests {
    use super::compose_metrics;

    #[test]
    fn the_l0_block_is_appended_when_present() {
        let base = "# HELP a x\na 1\n".to_string();
        let l0 = Some("# HELP ehdb_l0_manifest_parts y\nehdb_l0_manifest_parts{dataset=\"d1_event_log\"} 0\n".to_string());
        let out = compose_metrics(base.clone(), l0);
        assert!(out.starts_with(&base), "the registry's own text must survive");
        assert!(
            out.contains("ehdb_l0_manifest_parts{dataset=\"d1_event_log\"} 0"),
            "the L0 block must be appended: {out}"
        );
    }

    #[test]
    fn a_missing_or_empty_l0_block_changes_nothing() {
        // `None` is the contended-scrape and engine-absent case. It must not
        // corrupt the body or append a stray newline-only block.
        let base = "a 1\n".to_string();
        assert_eq!(compose_metrics(base.clone(), None), base);
        assert_eq!(compose_metrics(base.clone(), Some(String::new())), base);
    }

    #[test]
    fn a_base_without_a_trailing_newline_does_not_glue_two_series_together() {
        // Prometheus parses line-wise; "a 1ehdb_l0_x 0" is one corrupt line and
        // would take the whole scrape down, not just the appended block.
        let out = compose_metrics("a 1".to_string(), Some("ehdb_l0_x 0\n".to_string()));
        assert!(out.contains("a 1\nehdb_l0_x 0"), "lines must stay separate: {out:?}");
    }

    /// The series this process promises to expose must actually exist in the
    /// library, by name. If ehdb renames one, this fails instead of the series
    /// silently vanishing from prod — which is exactly how it was absent before.
    #[test]
    fn the_promised_state_gauges_exist_in_the_library() {
        let names = ehdb_l0::L0MetricsSnapshot::series_names();
        assert!(
            names.len() >= 25,
            "parsed only {} series names — a near-empty list would make every \
             assertion below pass vacuously",
            names.len()
        );
        for promised in [
            "manifest_parts",
            "parts_local_only",
            "parts_under_replicated",
            "dedupe_window_records",
            "records_superseded",
        ] {
            assert!(
                names.contains(&promised),
                "ehdb-l0 no longer exports `{promised}`; prod would lose the series \
                 silently. Known names: {names:?}"
            );
        }
    }
}
