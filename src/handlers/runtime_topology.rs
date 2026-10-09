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

/// The body a fleet member sends to register or renew.
#[derive(Debug, Deserialize)]
pub struct RegisterBody {
    /// One of server / gateway / ehdb / worker / playbook / execution.
    pub kind: String,
    /// The member's id. Must be `[A-Za-z0-9-_.:]` — it becomes a substrate key.
    pub id: String,
    /// What the member advertises about itself (a version string, typically).
    #[serde(default)]
    pub contract: String,
}

/// `POST /api/runtime/register` — register or renew a fleet member.
///
/// ⚠ **This is how a worker, gateway or EHDB instance joins the registry.**
/// `agents/rules/data-access-boundary.md` lists `noetl.runtime` — worker registration and
/// heartbeat — as **server-owned**, so a member reaches it through this API rather than
/// opening its own store. That is not only the rule: a member writing its own local D8
/// would be **invisible** to `/api/runtime/topology`, because the stores are separate. The
/// shared store is what makes the fleet discoverable at all.
///
/// Idempotent by design: one call on a timer both registers and renews, so a client never
/// has to remember across its own restarts which of the two it owes.
pub async fn register(Json(body): Json<RegisterBody>) -> impl IntoResponse {
    let Some(kind) = crate::runtime_registry::parse_kind(&body.kind) else {
        // ⚠ 400 rather than defaulting to Worker. `RuntimeKind::default()` IS `Worker`, so
        // a silent coercion would file a gateway under the wrong role and make
        // `discover(Gateway)` wrong in a way nothing reports.
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "unknown kind",
                "got": body.kind,
                "expected": ["server", "gateway", "ehdb", "worker", "playbook", "execution"],
            })),
        )
            .into_response();
    };
    match crate::runtime_registry::upsert(kind, &body.id, &body.contract) {
        Ok(fresh) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "id": body.id,
                "kind": format!("{kind:?}"),
                "registered": fresh,
                "renewed": !fresh,
                "ttl_secs": crate::runtime_registry::ttl_micros() / 1_000_000,
            })),
        )
            .into_response(),
        // ehdb's `validate_op` refuses an empty id, an over-long id or contract, a dot-run,
        // and anything outside `[A-Za-z0-9-_.:]`. Those are caller errors, so they surface
        // as 400 rather than 500 — the id becomes a substrate key and a traversal attempt
        // must never reach the log.
        Err(e) if e.contains("worker_id") || e.contains("contract") => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid registration", "detail": e })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "runtime registry unavailable", "detail": e })),
        )
            .into_response(),
    }
}

/// The body for a clean departure.
#[derive(Debug, Deserialize)]
pub struct DeregisterBody {
    pub id: String,
}

/// `POST /api/runtime/deregister` — leave immediately instead of waiting out the lease.
///
/// ⚠ Optional, never required. A member that vanishes without calling this still expires by
/// TTL, which is the whole point of a lease: correctness must not depend on a shutdown hook
/// running. This only shortens the window.
pub async fn deregister(Json(body): Json<DeregisterBody>) -> impl IntoResponse {
    match crate::runtime_registry::deregister(&body.id) {
        Ok(gone) => (
            StatusCode::OK,
            Json(serde_json::json!({ "id": body.id, "deregistered": gone })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "runtime registry unavailable", "detail": e })),
        )
            .into_response(),
    }
}
