//! **The discovery surface** — `GET /api/runtime/topology` and `/api/runtime/watch`
//! (noetl/ai-meta#455 P1–P4).
//!
//! Without an endpoint, "every service is discoverable" is a claim about a library rather
//! than about the platform: the registry would be reachable only from inside the process
//! that writes it. This is what makes it demonstrable end to end.
//!
//! ⚠ **Gated.** These routes **enumerate** service ids and contracts, and enumeration is a
//! capability distinct from reporting on an id the caller already holds — the same
//! reasoning that gated `ehdb_equivalence_routes` and `ehdb_object_parity_routes`
//! (noetl/ai-meta#307, #348). An unauthenticated route nobody argued about is how
//! noetl/ai-meta#312 happened.
//!
//! ⚠ `contract` is a version string the instance advertises about itself. It is **never**
//! secret material: secrets are referenced by `SecretRef` (`provider://path#version`), a
//! pointer type that cannot hold a value.

use axum::{extract::Query, http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;

/// `GET /api/runtime/topology` — every live registration, grouped by kind.
///
/// `503` when the registry is off or unopenable, rather than an empty `200`: an empty
/// topology and an absent registry are opposite conditions, and returning `200 {}` for both
/// is the absent-vs-zero defect in HTTP form.
pub async fn topology() -> impl IntoResponse {
    match crate::runtime_registry::topology() {
        Some(t) => (StatusCode::OK, Json(serde_json::json!(t))).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "runtime registry unavailable",
                "detail": "the registry is disabled or its durable root is not mounted; \
                           an empty topology would be indistinguishable from this",
            })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct WatchQuery {
    /// Resume cursor. Omit or `0` for the whole history.
    #[serde(default)]
    pub after: u64,
}

/// `GET /api/runtime/watch?after=N` — topology changes after `N`, plus the next cursor.
///
/// ⚠ Poll-based, and named so. A consumer's latency is its poll interval. The property that
/// makes it a watch rather than a scan is that **resuming from the returned cursor yields
/// nothing when nothing changed** — a "watch" that re-delivers the whole log every poll
/// looks identical in any test that only ever polls from 0.
pub async fn watch(Query(q): Query<WatchQuery>) -> impl IntoResponse {
    match crate::runtime_registry::watch(q.after) {
        Some((ops, cursor)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "after": q.after,
                "cursor": cursor,
                "count": ops.len(),
                "ops": ops,
            })),
        )
            .into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "runtime registry unavailable" })),
        )
            .into_response(),
    }
}
