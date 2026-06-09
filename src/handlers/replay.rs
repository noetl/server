//! Replay API handlers.
//!
//! Phase D Round 5 of the Rust server FastAPI parity port
//! (noetl/ai-meta#49 → noetl/server#148).  Round 1 ships the
//! endpoint scaffold + minimal `execution` projection.
//!
//! Python reference:
//! [`noetl/server/api/replay/endpoint.py`](https://github.com/noetl/noetl/blob/main/noetl/server/api/replay/endpoint.py).

use axum::{
    extract::{Query, State},
    Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::error::AppError;
use crate::services::replay::{
    ReplayCutoff, ReplayProjection, ReplayService, ReplayState,
};

/// Query parameters for `GET /api/replay/state`.
///
/// Mirrors the Python endpoint at
/// `repos/noetl/noetl/server/api/replay/endpoint.py`:
/// `execution_id` is required; the cutoff fields are mutually
/// exclusive (Phase D R5 Round 1 keeps the same 400 behaviour
/// when more than one is set); `tenant_id` + `organization_id`
/// default to `"default"` per the umbrella's multi-tenancy
/// scaffold.
#[derive(Debug, Clone, Deserialize)]
pub struct ReplayStateQuery {
    /// Execution id to replay.  Required.
    pub execution_id: i64,

    /// Tenant boundary.  Default `"default"` matches Python.
    #[serde(default = "default_tenant_id")]
    pub tenant_id: String,

    /// Organization boundary.  Default `"default"` matches
    /// Python.
    #[serde(default = "default_org_id")]
    pub organization_id: String,

    /// Replay through this event_id (inclusive).  Mutually
    /// exclusive with `as_of_position` / `as_of_time`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of_event_id: Option<i64>,

    /// Alias for event-position cutoff.  Mutually exclusive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of_position: Option<i64>,

    /// Replay through this `event_time` (inclusive).  Mutually
    /// exclusive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of_time: Option<DateTime<Utc>>,

    /// Which projection to fold the events into.  Default `all`
    /// (Round 1: extra map fields stay empty regardless of
    /// projection).
    #[serde(default = "default_projection")]
    pub projection: String,

    /// Maximum events to fold this round.  Bounded `1..=100_000`
    /// by the service layer.
    #[serde(default = "default_limit")]
    pub limit: i64,

    /// Resolve payload refs and return bounded verification
    /// summaries.  Round 1 accepts the param but doesn't yet
    /// resolve — payload_resolver is Round 6.
    #[serde(default)]
    pub resolve_payloads: bool,
}

fn default_tenant_id() -> String {
    "default".to_string()
}
fn default_org_id() -> String {
    "default".to_string()
}
fn default_projection() -> String {
    "all".to_string()
}
fn default_limit() -> i64 {
    10_000
}

/// Replay an execution's events into a deterministic state.
///
/// `GET /api/replay/state`
pub async fn replay_state(
    State(service): State<ReplayService>,
    Query(query): Query<ReplayStateQuery>,
) -> Result<Json<ReplayState>, AppError> {
    // Reject ambiguous cutoff combinations — matches Python's
    // `endpoint.py` 400 behaviour for the same shape.
    let cutoff = ReplayCutoff {
        as_of_event_id: query.as_of_event_id,
        as_of_position: query.as_of_position,
        as_of_time: query.as_of_time,
    };
    if cutoff.set_count() > 1 {
        return Err(AppError::BadRequest(
            "Use only one replay cutoff: as_of_event_id, as_of_position, or as_of_time".to_string(),
        ));
    }

    let projection = ReplayProjection::parse_wire(&query.projection).ok_or_else(|| {
        AppError::BadRequest(format!(
            "unknown projection {:?}; expected one of: execution, stage, frame, command, business_object, loop, all",
            query.projection
        ))
    })?;

    // Round 6 will wire `resolve_payloads` into the
    // payload_resolver port; today we accept the param but the
    // returned state is unaffected.
    let _ = query.resolve_payloads;

    let state = service
        .replay_state(
            &query.tenant_id,
            &query.organization_id,
            query.execution_id,
            cutoff,
            projection,
            query.limit,
        )
        .await?;
    Ok(Json(state))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_defaults_match_python_endpoint() {
        // Round-trip serde from a minimal query; everything else
        // should fall back to the Python defaults.
        let raw = r#"{"execution_id":42}"#;
        let q: ReplayStateQuery = serde_json::from_str(raw).unwrap();
        assert_eq!(q.execution_id, 42);
        assert_eq!(q.tenant_id, "default");
        assert_eq!(q.organization_id, "default");
        assert_eq!(q.projection, "all");
        assert_eq!(q.limit, 10_000);
        assert!(q.as_of_event_id.is_none());
        assert!(!q.resolve_payloads);
    }
}
