//! HTTP handlers for the `/api/internal/*` route family.
//!
//! Mirror of the Python implementation in
//! `repos/noetl/noetl/server/api/internal/` (noetl/noetl v4.10.0).
//! Tracks noetl/server#11 → noetl/ai-meta#49 Phase C.
//!
//! All routes are gated by `RequireInternalApiToken` — a bearer-token
//! extractor that pulls the expected token from the
//! `NOETL_INTERNAL_API_TOKEN` env var and constant-time-compares it to
//! the request's `Authorization: Bearer <token>` header.  System
//! worker pool playbooks carry the token via their K8s ServiceAccount
//! Secret; user playbooks don't have it and get 403.

use std::env;

use axum::{
    extract::{FromRequestParts, State},
    http::{request::Parts, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tracing::{debug, info, warn};

use crate::db::DbPool;
use crate::error::AppResult;
use crate::services::internal as svc;
use crate::state::AppState;

const TOKEN_ENV: &str = "NOETL_INTERNAL_API_TOKEN";

// ===========================================================================
// Auth extractor
// ===========================================================================

/// Axum extractor that validates the bearer token before the handler
/// runs.  Returns `(StatusCode, String)` errors that axum maps to
/// HTTP responses; the handler never sees an unauthenticated request.
///
/// Failure modes (mirror the Python side):
///
/// - 503 `Service Unavailable` if `NOETL_INTERNAL_API_TOKEN` is unset
///   or empty in the server env.  Intentional — no permissive default
///   for a privileged surface.
/// - 403 `Forbidden` if the `Authorization` header is missing, malformed
///   (no `Bearer` scheme), or the token doesn't match.
#[derive(Debug)]
pub struct RequireInternalApiToken;

impl<S> FromRequestParts<S> for RequireInternalApiToken
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, String);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        // 1. Server config — token env var must be set.
        let expected = match env::var(TOKEN_ENV) {
            Ok(value) if !value.trim().is_empty() => value,
            _ => {
                warn!(
                    "Internal API called but {} is not set; rejecting with 503.",
                    TOKEN_ENV
                );
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!(
                        "Internal API not configured: {} env var unset on the server. \
                         Set it to the system worker pool's ServiceAccount token before \
                         calling /api/internal/* endpoints.",
                        TOKEN_ENV
                    ),
                ));
            }
        };

        // 2. Authorization header — must exist.
        let header = match parts.headers.get("authorization") {
            Some(v) => v,
            None => {
                return Err((
                    StatusCode::FORBIDDEN,
                    "Internal API requires Authorization header with Bearer token.".to_string(),
                ));
            }
        };
        let header_value = header.to_str().map_err(|_| {
            (
                StatusCode::FORBIDDEN,
                "Internal API Authorization header is not valid ASCII.".to_string(),
            )
        })?;

        // 3. Bearer scheme + non-empty token.
        let mut parts_iter = header_value.splitn(2, ' ');
        let scheme = parts_iter.next().unwrap_or("");
        let token = parts_iter.next().unwrap_or("").trim();
        if !scheme.eq_ignore_ascii_case("bearer") || token.is_empty() {
            return Err((
                StatusCode::FORBIDDEN,
                "Internal API requires 'Bearer <token>' Authorization scheme.".to_string(),
            ));
        }

        // 4. Constant-time comparison — never use == on secrets.
        let provided = token.as_bytes();
        let expected_bytes = expected.as_bytes();
        if provided.len() != expected_bytes.len()
            || !bool::from(provided.ct_eq(expected_bytes))
        {
            return Err((
                StatusCode::FORBIDDEN,
                "Invalid service-account token for /api/internal/*.".to_string(),
            ));
        }

        Ok(RequireInternalApiToken)
    }
}

// ===========================================================================
// Request/response shapes — byte-identical to the Python side
// ===========================================================================

#[derive(Debug, Deserialize, Default)]
pub struct OutboxClaimRequest {
    #[serde(default = "default_claim_limit")]
    pub limit: i64,
}

fn default_claim_limit() -> i64 {
    100
}

#[derive(Debug, Serialize)]
pub struct OutboxClaimResponse {
    pub rows: Vec<svc::OutboxRow>,
    pub claimed: i64,
}

#[derive(Debug, Deserialize)]
pub struct OutboxMarkPublishedRequest {
    pub outbox_ids: Vec<i64>,
}

#[derive(Debug, Serialize)]
pub struct OutboxMarkPublishedResponse {
    pub marked: i64,
}

#[derive(Debug, Deserialize)]
pub struct OutboxMarkFailedRequest {
    pub outbox_id: i64,
    pub error: String,
    #[serde(default = "default_attempts")]
    pub attempts: i32,
    #[serde(default = "default_max_delay_seconds")]
    pub max_delay_seconds: i32,
}

fn default_attempts() -> i32 {
    1
}

fn default_max_delay_seconds() -> i32 {
    300
}

#[derive(Debug, Serialize)]
pub struct OutboxMarkFailedResponse {
    pub marked: bool,
    pub available_at_in: i64,
}

#[derive(Debug, Serialize)]
pub struct OutboxPendingCountResponse {
    pub pending: i64,
}

#[derive(Debug, Deserialize)]
pub struct EventsProjectRequest {
    pub events: Vec<svc::EventEnvelope>,
}

#[derive(Debug, Serialize)]
pub struct EventsProjectResponse {
    pub projected: i64,
    pub duplicates: i64,
}

/// Request for `POST /api/internal/projection/advance` (noetl/ai-meta#103
/// phase 2b).  The `system/projector` playbook extracts the distinct
/// `execution_id`s from a `noetl_events` stream batch and posts them here; the
/// server recomputes + saves each one's `projection_snapshot`.
#[derive(Debug, Deserialize)]
pub struct ProjectionAdvanceRequest {
    pub execution_ids: Vec<i64>,
}

#[derive(Debug, Serialize)]
pub struct ProjectionAdvanceResponse {
    pub advanced: Vec<crate::handlers::events::SnapshotAdvance>,
    pub failed: Vec<ProjectionAdvanceFailure>,
}

#[derive(Debug, Serialize)]
pub struct ProjectionAdvanceFailure {
    pub execution_id: i64,
    pub error: String,
}

// ===========================================================================
// Route handlers
// ===========================================================================

/// `POST /api/internal/outbox/claim`
///
/// Claim a batch of PENDING/FAILED outbox rows and mark them IN_FLIGHT.
/// Replaces the direct-DB call from today's Python publisher.
#[tracing::instrument(skip(pool, _token), fields(limit = request.limit))]
pub async fn outbox_claim(
    State(pool): State<DbPool>,
    _token: RequireInternalApiToken,
    Json(request): Json<OutboxClaimRequest>,
) -> AppResult<Json<OutboxClaimResponse>> {
    let rows = svc::claim_batch(&pool, request.limit).await?;
    let claimed = rows.len() as i64;
    debug!(claimed, "outbox/claim done");
    Ok(Json(OutboxClaimResponse { rows, claimed }))
}

/// `POST /api/internal/outbox/mark-published`
///
/// Mark a batch of outbox rows PUBLISHED.  Idempotent.
#[tracing::instrument(skip(pool, _token), fields(count = request.outbox_ids.len()))]
pub async fn outbox_mark_published(
    State(pool): State<DbPool>,
    _token: RequireInternalApiToken,
    Json(request): Json<OutboxMarkPublishedRequest>,
) -> AppResult<Json<OutboxMarkPublishedResponse>> {
    if request.outbox_ids.is_empty() {
        return Err(crate::error::AppError::BadRequest(
            "outbox_ids must not be empty".to_string(),
        ));
    }
    let marked = svc::mark_published_batch(&pool, &request.outbox_ids).await?;
    debug!(marked, "outbox/mark-published done");
    Ok(Json(OutboxMarkPublishedResponse { marked }))
}

/// `POST /api/internal/outbox/mark-failed`
///
/// Mark a single outbox row FAILED with exponential backoff.
#[tracing::instrument(
    skip(pool, _token),
    fields(outbox_id = request.outbox_id, attempts = request.attempts)
)]
pub async fn outbox_mark_failed(
    State(pool): State<DbPool>,
    _token: RequireInternalApiToken,
    Json(request): Json<OutboxMarkFailedRequest>,
) -> AppResult<Json<OutboxMarkFailedResponse>> {
    if request.error.is_empty() {
        return Err(crate::error::AppError::BadRequest(
            "error must not be empty".to_string(),
        ));
    }
    let delay = svc::mark_failed_row(
        &pool,
        request.outbox_id,
        &request.error,
        request.attempts,
        request.max_delay_seconds,
    )
    .await?;
    info!(delay_seconds = delay, "outbox/mark-failed applied");
    Ok(Json(OutboxMarkFailedResponse {
        marked: true,
        available_at_in: delay,
    }))
}

/// `GET /api/internal/outbox/pending-count`
///
/// KEDA HTTP scaler trigger source.
#[tracing::instrument(skip(pool, _token))]
pub async fn outbox_pending_count(
    State(pool): State<DbPool>,
    _token: RequireInternalApiToken,
) -> AppResult<Json<OutboxPendingCountResponse>> {
    let pending = svc::pending_count(&pool).await?;
    Ok(Json(OutboxPendingCountResponse { pending }))
}

/// `POST /api/internal/events/project`
///
/// Batch-INSERT events into `noetl.event`.  Idempotent via
/// `ON CONFLICT (event_id) DO NOTHING`.
#[tracing::instrument(skip(pool, _token), fields(batch_size = request.events.len()))]
pub async fn events_project(
    State(pool): State<DbPool>,
    _token: RequireInternalApiToken,
    Json(request): Json<EventsProjectRequest>,
) -> AppResult<Json<EventsProjectResponse>> {
    if request.events.is_empty() {
        return Err(crate::error::AppError::BadRequest(
            "events must not be empty".to_string(),
        ));
    }
    let (projected, duplicates) = svc::project_events(&pool, &request.events).await?;
    info!(projected, duplicates, "events/project done");
    Ok(Json(EventsProjectResponse {
        projected,
        duplicates,
    }))
}

/// `POST /api/internal/projection/advance`
///
/// CQRS read-model write (noetl/ai-meta#103 phase 2b).  For each (deduped)
/// `execution_id` the `system/projector` playbook extracted from a
/// `noetl_events` stream batch, recompute + save `projection_snapshot` via the
/// orchestrator's bounded-rebuild machinery (no command dispatch).  Idempotent
/// (monotonic snapshot upsert), so a redelivered batch is a forward no-op.  A
/// per-execution failure is reported in `failed` without aborting the batch, so
/// one bad execution can't block the projector's progress on the rest.
pub async fn projection_advance(
    State(state): State<AppState>,
    _token: RequireInternalApiToken,
    Json(request): Json<ProjectionAdvanceRequest>,
) -> AppResult<Json<ProjectionAdvanceResponse>> {
    let mut seen = std::collections::HashSet::new();
    let mut advanced = Vec::new();
    let mut failed = Vec::new();
    for execution_id in request
        .execution_ids
        .into_iter()
        .filter(|e| seen.insert(*e))
    {
        match crate::handlers::events::advance_snapshot(&state, execution_id).await {
            Ok(a) => {
                crate::metrics::record_projection_advanced(a.version);
                advanced.push(a);
            }
            Err(e) => {
                warn!(execution_id, %e, "projection/advance: execution failed");
                failed.push(ProjectionAdvanceFailure {
                    execution_id,
                    error: e.to_string(),
                });
            }
        }
    }
    info!(
        advanced = advanced.len(),
        failed = failed.len(),
        "projection/advance done"
    );
    Ok(Json(ProjectionAdvanceResponse { advanced, failed }))
}

/// `POST /api/internal/cleanup/purge`
///
/// Scheduled-cleanup entry point for the system worker pool
/// (noetl/ai-meta#96).  Reclaims clearly-transient `noetl.*` storage per the
/// retention policy in the request body: DELETE terminal `noetl.command` rows +
/// dead `noetl.runtime` worker registrations, and — opt-in only — DROP whole
/// old `noetl.event` partitions (the event log is range-partitioned by
/// execution_id, so retention drops partitions instead of scanning + deleting
/// rows).  An empty body uses safe defaults (commands 7d, runtime 60m, events
/// skipped).  Per data-access-boundary.md this is the only path the
/// `system/scheduled_cleanup` playbook uses to touch these tables.
#[tracing::instrument(skip(pool, _token))]
pub async fn cleanup_purge(
    State(pool): State<DbPool>,
    _token: RequireInternalApiToken,
    body: Option<Json<svc::CleanupPolicy>>,
) -> AppResult<Json<svc::CleanupResult>> {
    let policy = body.map(|Json(p)| p).unwrap_or_default();
    let result = svc::purge_stale(&pool, &policy).await?;
    crate::metrics::record_cleanup_purged("command", result.commands_purged);
    crate::metrics::record_cleanup_purged("runtime", result.runtime_purged);
    crate::metrics::record_cleanup_purged("event_partition", result.events_purged);
    info!(
        commands_purged = result.commands_purged,
        runtime_purged = result.runtime_purged,
        event_partitions_dropped = result.events_purged,
        dropped = ?result.event_partitions_dropped,
        command_retention_days = policy.command_retention_days,
        runtime_stale_minutes = policy.runtime_stale_minutes,
        event_retention_days = policy.event_retention_days,
        "cleanup/purge done"
    );
    Ok(Json(result))
}

// ===========================================================================
// Tests — auth extractor (the only logic that doesn't need a real DB)
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::FromRequestParts;
    use axum::http::{Request, StatusCode};

    /// Helper to invoke the extractor without a router around it.
    async fn run_extractor(
        env_token: Option<&str>,
        header: Option<&str>,
    ) -> Result<RequireInternalApiToken, (StatusCode, String)> {
        // Save + override the env var (tests run sequentially-isolated via
        // the `serial_test` pattern would be cleaner; here we accept the
        // small risk because each test sets + removes its own value).
        match env_token {
            Some(v) => unsafe { env::set_var(TOKEN_ENV, v) },
            None => unsafe { env::remove_var(TOKEN_ENV) },
        }

        let mut builder = Request::builder().method("GET").uri("/test");
        if let Some(h) = header {
            builder = builder.header("authorization", h);
        }
        let req = builder.body(Body::empty()).unwrap();
        let (mut parts, _body) = req.into_parts();

        let result = <RequireInternalApiToken as FromRequestParts<()>>::from_request_parts(
            &mut parts,
            &(),
        )
        .await;

        unsafe { env::remove_var(TOKEN_ENV) };
        result
    }

    // Tests touch the process-global ``NOETL_INTERNAL_API_TOKEN``
    // env var — they must run sequentially or one test's setenv races
    // another's getenv.  ``#[serial]`` from ``serial_test`` serialises
    // these without forcing the whole suite to single-threaded mode.

    #[tokio::test]
    #[serial_test::serial]
    async fn rejects_when_env_unset() {
        let err = run_extractor(None, Some("Bearer foo")).await.unwrap_err();
        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(err.1.contains(TOKEN_ENV));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn rejects_when_env_blank() {
        let err = run_extractor(Some("   "), Some("Bearer foo"))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn rejects_when_no_authorization_header() {
        let err = run_extractor(Some("secret-123"), None).await.unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
        assert!(err.1.contains("Bearer"));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn rejects_non_bearer_scheme() {
        let err = run_extractor(Some("secret-123"), Some("Basic secret-123"))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn rejects_wrong_token() {
        let err = run_extractor(Some("secret-123"), Some("Bearer wrong"))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn accepts_valid_token() {
        let result = run_extractor(Some("secret-123"), Some("Bearer secret-123")).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn accepts_valid_token_case_insensitive_scheme() {
        let result = run_extractor(Some("secret-123"), Some("bearer secret-123")).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn rejects_empty_token_after_bearer() {
        let err = run_extractor(Some("secret-123"), Some("Bearer "))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::FORBIDDEN);
    }
}
