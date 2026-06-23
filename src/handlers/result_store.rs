//! Result-store HTTP handlers (Result Store MVP,
//! [`noetl/ai-meta#70`](https://github.com/noetl/ai-meta/issues/70)).
//!
//! - `PUT /api/result/{execution_id}` — store a result; return the
//!   `noetl://` URI + metadata.
//! - `GET /api/result/resolve?ref={uri}` — resolve a URI to the
//!   stored payload.
//!
//! These are the two endpoints the worker calls
//! (`repos/worker/src/client/control_plane.rs:557–594`) and the
//! tools layer fetches from
//! (`repos/tools/src/tools/result_fetch.rs:230`).

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

use crate::error::AppResult;
use crate::services::result_store::{parse_noetl_ref, PutResultBody, ResultStoreService};

// ---------------------------------------------------------------------------
// Handler state
// ---------------------------------------------------------------------------

/// Dependencies injected into both handlers via Axum's
/// `axum::extract::State`.
#[derive(Clone)]
pub struct ResultStoreDeps {
    pub service: ResultStoreService,
    /// Phase D minting flip (noetl/ai-meta#104 Phase D): when true the worker
    /// treats the URN tier as authoritative, so each `result_store` write here is
    /// the reversible **dual-write fallback leg** — counted on
    /// `noetl_result_store_dual_write_total`. Off → ordinary authoritative write.
    pub mint_authoritative: bool,
}

// ---------------------------------------------------------------------------
// PUT /api/result/{execution_id}
// ---------------------------------------------------------------------------

/// Store a result and return the `noetl://` URI + metadata.
///
/// Caller: `repos/worker/src/client/control_plane.rs` `put_result`.
///
/// Wire contract:
/// - Body: `{ name, data, scope, source_step? }`
/// - Response 200: `{ ref, store, scope, bytes, sha256, expires_at }`
/// - Response 400: malformed body.
/// - Response 500: server-side error (DB, snowflake).
pub async fn put_result(
    State(deps): State<ResultStoreDeps>,
    Path(execution_id): Path<i64>,
    Json(body): Json<PutResultBody>,
) -> AppResult<impl IntoResponse> {
    let span = tracing::info_span!(
        "result_store.put",
        execution_id,
        name = %body.name,
        scope = %body.scope,
    );
    let _g = span.enter();

    let t0 = std::time::Instant::now();
    let result = deps.service.put(execution_id, &body).await;
    let elapsed = t0.elapsed().as_secs_f64();

    match result {
        Ok(resp) => {
            tracing::info!(
                execution_id,
                name = %body.name,
                bytes = resp.bytes,
                result_ref = %resp.r#ref,
                duration_seconds = elapsed,
                "result_store.put: stored",
            );
            crate::metrics::record_result_store_put(elapsed, resp.bytes as usize, "ok");
            // Phase D (#104): under the minting flip the URN tier is
            // authoritative and this write is the reversible dual-write fallback
            // leg — count it so the dual-write window is observable.
            if deps.mint_authoritative {
                crate::metrics::record_result_store_dual_write();
                tracing::debug!(
                    execution_id,
                    name = %body.name,
                    "result_store dual-write (Phase D fallback leg; tier authoritative)",
                );
            }
            Ok((StatusCode::OK, Json(resp)))
        }
        Err(e) => {
            tracing::warn!(
                execution_id,
                name = %body.name,
                error = %e,
                duration_seconds = elapsed,
                "result_store.put: failed",
            );
            crate::metrics::record_result_store_put(elapsed, 0, "error");
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// GET /api/result/resolve?ref=<uri>
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ResolveQuery {
    pub r#ref: String,
}

/// Resolve a `noetl://` URI to the stored payload JSON.
///
/// Caller: `repos/tools/src/tools/result_fetch.rs` `fetch_via_http`.
///
/// Wire contract:
/// - Query: `?ref=noetl://execution/<eid>/result/<name>/<id>`
/// - Response 200: the raw `data` JSON (the body IS the data).
/// - Response 400: malformed URI.
/// - Response 404: no matching row.
pub async fn resolve_ref(
    State(deps): State<ResultStoreDeps>,
    Query(params): Query<ResolveQuery>,
) -> AppResult<impl IntoResponse> {
    let span = tracing::info_span!(
        "result_store.resolve",
        noetl_ref = %params.r#ref,
    );
    let _g = span.enter();

    let t0 = std::time::Instant::now();

    let parsed = match parse_noetl_ref(&params.r#ref) {
        Ok(r) => r,
        Err(msg) => {
            tracing::warn!(
                noetl_ref = %params.r#ref,
                error = %msg,
                "result_store.resolve: malformed URI",
            );
            crate::metrics::record_result_store_resolve(t0.elapsed().as_secs_f64(), "bad_request");
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": msg})),
            )
                .into_response());
        }
    };

    let execution_id = parsed.execution_id;
    let name = parsed.name.clone();

    let result = deps.service.resolve(&parsed).await;
    let elapsed = t0.elapsed().as_secs_f64();

    match result {
        Ok(Some(data)) => {
            tracing::info!(
                execution_id,
                name = %name,
                duration_seconds = elapsed,
                "result_store.resolve: found",
            );
            crate::metrics::record_result_store_resolve(elapsed, "ok");
            Ok((StatusCode::OK, Json(data)).into_response())
        }
        Ok(None) => {
            tracing::warn!(
                execution_id,
                name = %name,
                noetl_ref = %params.r#ref,
                "result_store.resolve: not found",
            );
            crate::metrics::record_result_store_resolve(elapsed, "not_found");
            Ok((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({"error": "result not found"})),
            )
                .into_response())
        }
        Err(e) => {
            tracing::warn!(
                execution_id,
                name = %name,
                error = %e,
                duration_seconds = elapsed,
                "result_store.resolve: error",
            );
            crate::metrics::record_result_store_resolve(elapsed, "error");
            Err(e)
        }
    }
}
