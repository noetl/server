//! Worker-local drive: `POST /api/executions/{execution_id}/advance`
//! (noetl/ai-meta#558, Stage 1 of the latency-reduction plan, RFC #556).
//!
//! # Why
//!
//! Under the stateless off-server drive every step pays two hand-offs: the
//! completion is published, the materializer writes it, the server triggers,
//! `__orchestrate__` is dispatched to the system pool, claimed there, the state
//! is built from the WAL spine, the WASM runs, its `call.done` comes back, the
//! server applies it — and only then is the next command published for a user
//! worker to claim. The kind harness (ops `ci/kind/latency`, #557) measured the
//! first hand-off at ~19 ms idle and ~125 ms p50 / ~250 ms p95 at 8 concurrent
//! executions: every drive queues on the 2-slot system pool.
//!
//! # What
//!
//! The worker that just completed a step runs that SAME drive itself and hands
//! the result back:
//!
//! 1. its `command.completed` carries [`LOCAL_DRIVE_HEADER`]; the server records a
//!    claim and suppresses the normal trigger for that event
//!    (`events::trigger_orchestrator`);
//! 2. `begin` — the server takes the claim, takes the orchestrate in-flight guard
//!    exactly as `dispatch_offserver_stateless_drive` does, and returns the SAME
//!    drive input that dispatch would have put on the `__orchestrate__` command
//!    (playbook, `expected_head`, trigger), with the execution's tail ring as the
//!    spine;
//! 3. the worker builds the state with the system pool's own builder
//!    (`build_offserver_input`) and runs `system/orchestrate`;
//! 4. `apply` — the server runs `apply_worker_orchestration`, the function the
//!    system pool's `call.done` reaches, and returns the commands it issued so the
//!    worker can claim a same-pool one directly instead of waiting for the feed;
//! 5. `abort` — the worker could not drive (spine incomplete, WASM error): the
//!    server releases the guard and dispatches the normal drive at once.
//!
//! The drive input and the apply are the existing code, so the events and
//! commands are the ones the system pool would have produced.
//!
//! # Safety
//!
//! Every refusal falls back to today's dispatch. A worker that dies after its
//! completion leaves a `Claimed` entry that lapses after
//! `NOETL_LOCAL_DRIVE_CLAIM_TTL_MS`; one that dies after `begin` leaves the
//! in-flight guard, which expires (40 s) exactly as it does when a system-pool
//! worker dies mid-drive, and the reconcile poller re-drives.
//!
//! Mounted only when `NOETL_SERVER_ADVANCE_API=true`.
//!
//! [`LOCAL_DRIVE_HEADER`]: crate::handlers::events::LOCAL_DRIVE_HEADER

use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::error::{AppError, AppResult};
use crate::handlers::execute::IssuedCommand;
use crate::state::AppState;

/// Request body.
#[derive(Debug, Deserialize)]
pub struct AdvanceRequest {
    /// `begin` | `apply` | `abort`.
    pub phase: String,
    /// The `command.completed` event id the worker marked for local drive.
    #[serde(deserialize_with = "de_i64")]
    pub trigger_event_id: i64,
    /// `apply`: the drive output, in the shape the `__orchestrate__` command's
    /// `call.done` carries (anything holding `output_b64`, or an error envelope).
    #[serde(default)]
    pub payload: serde_json::Value,
    /// `abort`: why the worker could not drive (a metric label).
    #[serde(default)]
    pub reason: Option<String>,
}

/// Response body.
#[derive(Debug, Serialize, Default)]
pub struct AdvanceResponse {
    /// `drive` (begin accepted) | `fallback` (begin refused; today's path runs) |
    /// `applied` | `aborted`.
    pub status: String,
    /// `fallback`: why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `drive`: the `system/orchestrate` command args — identical to what
    /// `dispatch_offserver_stateless_drive` would have dispatched, with the tail.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
    /// `drive`: the plug-in to run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugin: Option<serde_json::Value>,
    /// `applied`: commands generated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commands: Option<i32>,
    /// `applied`: the commands this application issued.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub issued: Vec<IssuedCommand>,
}

fn de_i64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum N {
        I(i64),
        S(String),
    }
    match N::deserialize(d)? {
        N::I(i) => Ok(i),
        N::S(s) => s.parse().map_err(serde::de::Error::custom),
    }
}

fn fallback(reason: &str) -> Json<AdvanceResponse> {
    crate::metrics::record_orchestrate_drive(&format!("local_drive_fallback_{reason}"));
    Json(AdvanceResponse {
        status: "fallback".into(),
        reason: Some(reason.into()),
        ..Default::default()
    })
}

/// `POST /api/executions/{execution_id}/advance`.
pub async fn advance(
    State(state): State<AppState>,
    Path(execution_id): Path<i64>,
    Json(req): Json<AdvanceRequest>,
) -> AppResult<Json<AdvanceResponse>> {
    match req.phase.as_str() {
        "begin" => begin(&state, execution_id, req.trigger_event_id).await,
        "apply" => apply(&state, execution_id, req.trigger_event_id, &req.payload).await,
        "abort" => {
            abort(
                &state,
                execution_id,
                req.trigger_event_id,
                req.reason.as_deref().unwrap_or("worker"),
            )
            .await
        }
        other => Err(AppError::Validation(format!(
            "advance: unknown phase '{other}' (want begin|apply|abort)"
        ))),
    }
}

/// Release a claim we will not drive and run today's trigger for it.
async fn release_and_trigger(state: &AppState, execution_id: i64, trigger_event_id: i64) {
    state.local_drive.done(trigger_event_id, execution_id);
    if let Err(e) = crate::handlers::events::trigger_orchestrator_inner(
        state,
        execution_id,
        trigger_event_id,
        false,
    )
    .await
    {
        warn!(execution_id, %e, "local drive fallback: normal trigger failed");
    }
}

async fn begin(
    state: &AppState,
    execution_id: i64,
    trigger_event_id: i64,
) -> AppResult<Json<AdvanceResponse>> {
    // The claim is the only licence to drive. No claim → whatever suppressed
    // nothing, so today's trigger has run or will.
    match state.local_drive.begin(trigger_event_id) {
        Some(claimed) if claimed == execution_id => {}
        Some(other) => {
            // Mismatched path id: never drive another execution's claim.
            state.local_drive.done(trigger_event_id, other);
            release_and_trigger(state, other, trigger_event_id).await;
            return Ok(fallback("execution_mismatch"));
        }
        None => return Ok(fallback("no_claim")),
    }

    // The stateless off-server topology is the only one this mirrors.
    let topology_ok = state.config.orchestrate_plugin_drive
        && matches!(
            state.config.state_builder,
            crate::config::StateBuilder::Offserver
        );
    let desc = state.exec_descriptors.get(execution_id).await;
    let desc = match desc {
        Some(d)
            if topology_ok
                && d.catalog_id != 0
                && crate::handlers::event_write::should_publish(state, d.catalog_id).await =>
        {
            d
        }
        _ => {
            release_and_trigger(state, execution_id, trigger_event_id).await;
            return Ok(fallback("topology"));
        }
    };
    if desc.terminal {
        state.local_drive.done(trigger_event_id, execution_id);
        return Ok(fallback("terminal"));
    }
    if state.drive_tombstones.clear(execution_id) {
        debug!(
            execution_id,
            "local drive: a real event cleared the tombstone; resuming"
        );
    }

    let cache_slot = state.orch_cache.entry(execution_id);
    let mut cache = cache_slot.lock().await;
    if cache.orchestrate_in_flight && !crate::handlers::events::in_flight_guard_holds(&cache) {
        crate::handlers::events::expire_in_flight_guard(&mut cache, execution_id);
    }
    if cache.orchestrate_in_flight {
        // Another drive is computing against an older head. Owe it a re-drive
        // exactly as a dropped trigger is owed one, and step aside.
        if state.config.orch_retrigger_on_clear {
            cache.orchestrate_retrigger_pending = true;
            crate::metrics::record_orchestrate_drive("retrigger_recorded");
        }
        drop(cache);
        state.local_drive.done(trigger_event_id, execution_id);
        return Ok(fallback("in_flight"));
    }

    let catalog_id = desc.catalog_id;
    let expected_head = state.chain_heads.head(execution_id).await.unwrap_or(0);
    let playbook = match load_playbook(state, catalog_id).await {
        Ok(p) => p,
        Err(e) => {
            drop(cache);
            warn!(execution_id, %e, "local drive: playbook load failed; falling back");
            release_and_trigger(state, execution_id, trigger_event_id).await;
            return Ok(fallback("playbook"));
        }
    };
    let tail_events = state.chain_tails.snapshot(execution_id);

    // The SAME args `dispatch_offserver_stateless_drive` builds, plus the tail.
    let args = serde_json::json!({
        "__offserver_build__": true,
        "__stateless__": true,
        "execution_id": execution_id,
        "trigger_event_id": trigger_event_id,
        "playbook": &*playbook,
        "expected_head": expected_head,
        "tail_events": tail_events,
    });
    cache.orchestrate_in_flight = true;
    cache.orchestrate_in_flight_since = Some(std::time::Instant::now());
    drop(cache);
    crate::metrics::record_orchestrate_drive("local_drive_begin");
    debug!(
        execution_id,
        trigger_event_id,
        expected_head,
        tail = args["tail_events"].as_array().map_or(0, |a| a.len()),
        "local drive: begin"
    );
    Ok(Json(AdvanceResponse {
        status: "drive".into(),
        args: Some(args),
        plugin: Some(serde_json::json!({
            "path": "system/orchestrate", "version": 1, "entry": "run_state"
        })),
        ..Default::default()
    }))
}

async fn apply(
    state: &AppState,
    execution_id: i64,
    trigger_event_id: i64,
    payload: &serde_json::Value,
) -> AppResult<Json<AdvanceResponse>> {
    if state.local_drive.phase(trigger_event_id) != Some(crate::state::LocalDrivePhase::Driving) {
        // Not ours to apply (never began, or already finished). The guard is not
        // ours either — leave it alone.
        crate::metrics::record_orchestrate_drive("local_drive_apply_rejected");
        return Ok(fallback("not_driving"));
    }
    state.local_drive.done(trigger_event_id, execution_id);
    let (res, issued) =
        crate::handlers::execute::collect_issued_commands(crate::handlers::events::apply_worker_orchestration(
            state,
            execution_id,
            trigger_event_id,
            payload,
        ))
        .await;
    match res {
        Ok(commands) => {
            crate::metrics::record_orchestrate_drive("local_drive_applied");
            info!(
                execution_id,
                commands,
                issued = issued.len(),
                "local drive: applied worker orchestrate result"
            );
            Ok(Json(AdvanceResponse {
                status: "applied".into(),
                commands: Some(commands),
                issued,
                ..Default::default()
            }))
        }
        Err(e) => {
            // apply_worker_orchestration cleared the guard on entry; drive normally.
            warn!(execution_id, %e, "local drive: apply failed; dispatching the normal drive");
            crate::metrics::record_orchestrate_drive("local_drive_apply_failed");
            release_and_trigger(state, execution_id, trigger_event_id).await;
            Ok(fallback("apply_failed"))
        }
    }
}

async fn abort(
    state: &AppState,
    execution_id: i64,
    trigger_event_id: i64,
    reason: &str,
) -> AppResult<Json<AdvanceResponse>> {
    if state.local_drive.phase(trigger_event_id) != Some(crate::state::LocalDrivePhase::Driving) {
        return Ok(fallback("not_driving"));
    }
    {
        let cache_slot = state.orch_cache.entry(execution_id);
        let mut cache = cache_slot.lock().await;
        cache.orchestrate_in_flight = false;
        cache.orchestrate_in_flight_since = None;
    }
    let label = match reason {
        "incomplete" | "wasm" | "decode" | "plugin" | "unsupported" => reason,
        _ => "other",
    };
    crate::metrics::record_orchestrate_drive(&format!("local_drive_abort_{label}"));
    debug!(execution_id, reason, "local drive: aborted; dispatching the normal drive");
    release_and_trigger(state, execution_id, trigger_event_id).await;
    Ok(Json(AdvanceResponse {
        status: "aborted".into(),
        ..Default::default()
    }))
}

/// Playbook by catalog id, cached: a `catalog_id` names immutable content (a new
/// registration is a new id), so the parse is reusable for the life of the
/// process. Only the local-drive path uses this cache; today's dispatch and
/// apply are untouched.
async fn load_playbook(
    state: &AppState,
    catalog_id: i64,
) -> AppResult<std::sync::Arc<crate::playbook::types::Playbook>> {
    static CACHE: std::sync::OnceLock<
        std::sync::Mutex<
            std::collections::HashMap<i64, std::sync::Arc<crate::playbook::types::Playbook>>,
        >,
    > = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(p) = cache.lock().unwrap().get(&catalog_id).cloned() {
        return Ok(p);
    }
    let yaml: String = sqlx::query_scalar("SELECT content FROM noetl.catalog WHERE catalog_id = $1")
        .bind(catalog_id)
        .fetch_one(state.pools.cluster())
        .await
        .map_err(|e| AppError::Internal(format!("local drive: load playbook {catalog_id}: {e}")))?;
    let parsed = std::sync::Arc::new(crate::playbook::parser::parse_playbook(&yaml)?);
    let mut map = cache.lock().unwrap();
    if map.len() >= 512 {
        map.clear();
    }
    map.insert(catalog_id, parsed.clone());
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_accepts_string_or_number_ids() {
        let a: AdvanceRequest =
            serde_json::from_str(r#"{"phase":"begin","trigger_event_id":"123"}"#).unwrap();
        let b: AdvanceRequest =
            serde_json::from_str(r#"{"phase":"begin","trigger_event_id":123}"#).unwrap();
        assert_eq!(a.trigger_event_id, 123);
        assert_eq!(b.trigger_event_id, 123);
    }

    #[test]
    fn fallback_names_its_reason() {
        let r = fallback("no_claim");
        assert_eq!(r.0.status, "fallback");
        assert_eq!(r.0.reason.as_deref(), Some("no_claim"));
        let v = serde_json::to_value(&r.0).unwrap();
        assert!(v.get("args").is_none() && v.get("issued").is_none());
    }
}
