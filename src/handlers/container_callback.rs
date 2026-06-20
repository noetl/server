//! `POST /api/internal/container-callback/{execution_id}/{step}` —
//! external K8s watcher callback for the container tool kind.
//!
//! Round 2 of the [Container Tool Callback umbrella][umbrella]
//! ([noetl/ai-meta#43](https://github.com/noetl/ai-meta/issues/43)).
//! See [umbrella][umbrella] for the full design.
//!
//! ## Wire shape
//!
//! - Path params: `execution_id` (i64 snowflake), `step` (non-empty
//!   step name).
//! - Body: [`ContainerCallbackRequest`] — structured Job terminal
//!   state with one of six [`TerminalState`] values, the K8s
//!   `job_name` + optional `job_uid`, `completed_at` timestamp, and
//!   optional `exit_code` / `reason` / `stdout_uri` / `stderr_uri`
//!   captured by the watcher.
//! - Response: **202 Accepted** even when no in-flight container
//!   block matches the `(execution_id, step)` pair.  The watcher is
//!   idempotent and may race with retries; the server should not
//!   4xx on a stale callback (those bump the
//!   `noetl_container_callback_stale_total` counter at the WARN
//!   level so an operator dashboards them).
//!
//! ## What "stale" means
//!
//! A callback is "stale" when no event row exists for the path's
//! `execution_id` in `noetl.event`.  Either the execution was
//! garbage-collected, the watcher has the wrong namespace, or the
//! Job was created out-of-band (not by NoETL's container tool).
//! Stale callbacks log + bump the counter + return 202 — no
//! `call.done` event is emitted.
//!
//! ## What "matched" means
//!
//! The execution_id resolves to at least one row in `noetl.event`.
//! The handler emits a `call.done` event keyed by the executor's
//! application-side snowflake; the orchestrator's normal event-id
//! idempotency (`ON CONFLICT DO NOTHING` on `event_id`) ensures
//! that a retried POST from the watcher does not double-emit.
//!
//! Tightening this check to "the block must be RUNNING and the
//! tool kind must be `container`" is a follow-up sub-issue once
//! Round 3 (Tool::Container in noetl-tools) lands and the
//! orchestrator records the tool kind on the step.enter event.
//!
//! [umbrella]: https://github.com/noetl/ai-meta/wiki/Umbrella-Container-Tool-Callback

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::handlers::internal::RequireInternalApiToken;
use crate::state::AppState;

/// Six structured Job terminal states the watcher POSTs.  Each maps
/// to a [`CallDoneStatus`] that the orchestrator branches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalState {
    /// Job's container exited 0; `Job.Complete` condition.
    Succeeded,
    /// Container exited non-zero N times; `Job.Failed` with
    /// `BackoffLimitExceeded`.
    Failed,
    /// Image pull never succeeded (init container never ran);
    /// `ImagePullBackOff`.  Alert-worthy when sustained.
    FailedImagePull,
    /// Pod OOMKilled — operator-actionable (raise memory limit).
    FailedOom,
    /// Pod evicted (node lost / drained).  Transient; orchestrator
    /// may retry.
    FailedNodeLost,
    /// `Job.spec.activeDeadlineSeconds` exceeded.  Distinguished
    /// from the orchestrator's per-step retry timeout.
    FailedTimeout,
}

impl TerminalState {
    /// `call.done` `status` label this state maps to.  Same string
    /// the watcher's documentation pins (and what gets logged on
    /// the resume event), so callers downstream can branch on it.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::FailedImagePull => "failed_image_pull",
            Self::FailedOom => "failed_oom",
            Self::FailedNodeLost => "failed_node_lost",
            Self::FailedTimeout => "failed_timeout",
        }
    }

    /// Whether the terminal state should be surfaced as a `call.done`
    /// with `status="completed"` (true) or `status="failed"` (false).
    /// The structured state survives in `meta.terminal_state` so the
    /// playbook can branch on the specific failure reason.
    fn is_success(&self) -> bool {
        matches!(self, Self::Succeeded)
    }
}

/// Request body — watcher POSTs this when it observes a Job
/// terminal-state transition on a Job carrying the
/// `noetl.execution-id` label.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerCallbackRequest {
    /// Job's terminal state.  Maps to a `call.done` outcome.
    pub state: TerminalState,
    /// K8s Job's `metadata.name` (`noetl-container-<step>-<eid>-<sfx>`).
    pub job_name: String,
    /// K8s Job's `metadata.uid`.  Optional — older watcher impls may
    /// not include it.  Carried into `meta` for forensic trail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_uid: Option<String>,
    /// When the Job transitioned to the terminal state.  ISO-8601
    /// in UTC.  Falls back to `Utc::now()` at the handler if absent
    /// (the watcher should always supply it, but a missing value is
    /// not a 400).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Container's exit code.  `None` for failure modes where no
    /// container ever started (`FailedImagePull`, `FailedNodeLost`
    /// pre-start).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Free-text reason from K8s (`message` on the failing
    /// condition / pod status).  Carried into `meta` for the
    /// playbook step's failure path to inspect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Optional `noetl://` ref the watcher captured stdout to.
    /// `None` when the watcher doesn't persist container output
    /// (default for round 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout_uri: Option<String>,
    /// Optional `noetl://` ref for stderr.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_uri: Option<String>,
}

/// Response body — small + readable; no Open-API surface yet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerCallbackResponse {
    /// `accepted_in_flight` (matched in-flight execution; emitted
    /// resume event), `accepted_stale` (no matching execution; no
    /// event emitted; stale counter bumped).
    pub status: String,
    /// `event_id` of the emitted `call.done`.  `None` on the stale
    /// path (no event emitted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
}

/// `POST /api/internal/container-callback/{execution_id}/{step}`
///
/// See module-level docs.
#[tracing::instrument(
    skip(state, _token, request),
    fields(
        execution_id = %execution_id_raw,
        step = %step,
        state = ?request.state,
        job_name = %request.job_name,
    ),
)]
pub async fn container_callback(
    State(state): State<AppState>,
    _token: RequireInternalApiToken,
    Path((execution_id_raw, step)): Path<(String, String)>,
    Json(request): Json<ContainerCallbackRequest>,
) -> Result<(StatusCode, Json<ContainerCallbackResponse>), AppError> {
    // -- Path-param validation (this is one of the few legitimate 4xx
    //    cases — malformed input, not stale state).
    let execution_id: i64 = execution_id_raw.parse().map_err(|_| {
        AppError::Validation(format!(
            "container-callback: execution_id '{execution_id_raw}' is not parseable as i64"
        ))
    })?;
    if step.trim().is_empty() {
        return Err(AppError::Validation(
            "container-callback: step path param is empty".to_string(),
        ));
    }

    let completed_at = request.completed_at.unwrap_or_else(chrono::Utc::now);

    // -- Stale check: is there ANY event for this execution_id?
    let pool = state.pools.pool_for(execution_id);
    // RFC #115 Phase 6: under `event_read_path=audit_only`, a warm execute-time
    // descriptor proves the execution exists — ZERO `noetl.event` read.  A cold
    // descriptor (server restart, or a genuinely stale callback for an execution
    // this server never saw) falls through to the scan (counted `scan`), which
    // preserves the stale-detection correctness.
    let audit_only = matches!(
        state.config.event_read_path,
        crate::config::EventReadPath::AuditOnly
    );
    let descriptor_warm = audit_only
        && state
            .exec_descriptors
            .get(execution_id)
            .await
            .map(|d| d.catalog_id != 0)
            .unwrap_or(false);
    let row: Option<(i64,)> = if descriptor_warm {
        crate::metrics::record_event_hotpath_read("container_callback_exists", "served_descriptor");
        Some((1,))
    } else if audit_only {
        // Cold descriptor under audit_only: prove existence from `noetl.command`
        // (the synchronous queue) — ZERO `noetl.event` read.
        crate::metrics::record_event_hotpath_read("container_callback_exists", "served_command");
        sqlx::query_as("SELECT 1::bigint FROM noetl.command WHERE execution_id = $1 LIMIT 1")
            .bind(execution_id)
            .fetch_optional(pool)
            .await?
    } else {
        crate::metrics::record_event_hotpath_read("container_callback_exists", "scan");
        sqlx::query_as("SELECT 1::bigint FROM noetl.event WHERE execution_id = $1 LIMIT 1")
            .bind(execution_id)
            .fetch_optional(pool)
            .await?
    };

    if row.is_none() {
        tracing::info!(
            execution_id,
            step = %step,
            state = request.state.as_str(),
            job_name = %request.job_name,
            "container-callback: stale — no events for execution_id; not emitting call.done"
        );
        crate::metrics::record_container_callback_stale(request.state.as_str());
        return Ok((
            StatusCode::ACCEPTED,
            Json(ContainerCallbackResponse {
                status: "accepted_stale".to_string(),
                event_id: None,
            }),
        ));
    }

    // -- Emit the resume `call.done` event.
    //
    // catalog_id and node_id we don't have at this boundary; the
    // orchestrator resolves them from the matching step.enter on
    // the executor side.  Round 3 (Tool::Container) carries them
    // into the watcher's payload via Job labels so this handler
    // can plumb them through; until then we pass `None` and the
    // orchestrator looks them up from the prior step.enter event
    // for `(execution_id, step)`.
    let event_id = state.snowflake.generate().map_err(|e| {
        AppError::Internal(format!("container-callback: snowflake generate failed: {e}"))
    })?;
    let terminal_context = serde_json::json!({
        "terminal_state": request.state.as_str(),
        "job_name": request.job_name,
        "job_uid": request.job_uid,
        "completed_at": completed_at,
        "exit_code": request.exit_code,
        "reason": request.reason,
        "stdout_uri": request.stdout_uri,
        "stderr_uri": request.stderr_uri,
    });
    let status_label = if request.state.is_success() {
        "COMPLETED"
    } else {
        "FAILED"
    };

    // The `result` column carries a constraint-shaped envelope
    // (`chk_event_result_shape`): a top-level string `status` plus an
    // optional object `context`.  Mirror handlers::events so the
    // call.done row validates and the orchestrator reads the terminal
    // outcome the same way it reads every other call.done.
    let result_obj = serde_json::json!({
        "status": status_label,
        "context": terminal_context,
    });

    // catalog_id is required by the schema; resolve from the
    // execution's existing events (the start event carries it).
    // RFC #115 Phase 6: under `event_read_path=audit_only` a warm descriptor
    // carries catalog_id — ZERO `noetl.event` read; cold falls through to scan.
    let catalog_id: i64 = if descriptor_warm {
        crate::metrics::record_event_hotpath_read("container_callback_catalog", "served_descriptor");
        state
            .exec_descriptors
            .get(execution_id)
            .await
            .map(|d| d.catalog_id)
            .unwrap_or(0)
    } else if audit_only {
        // Cold descriptor under audit_only: catalog_id from `noetl.command` — the
        // synchronous queue — ZERO `noetl.event` read.
        crate::metrics::record_event_hotpath_read("container_callback_catalog", "served_command");
        sqlx::query_as::<_, (i64,)>(
            "SELECT catalog_id FROM noetl.command WHERE execution_id = $1 LIMIT 1",
        )
        .bind(execution_id)
        .fetch_optional(pool)
        .await?
        .map(|(c,)| c)
        .unwrap_or(0)
    } else {
        crate::metrics::record_event_hotpath_read("container_callback_catalog", "scan");
        sqlx::query_as::<_, (i64,)>(
            "SELECT catalog_id FROM noetl.event WHERE execution_id = $1 \
             AND event_type IN ('playbook.initialized', 'playbook_started') \
             LIMIT 1",
        )
        .bind(execution_id)
        .fetch_optional(pool)
        .await?
        .map(|(c,)| c)
        .unwrap_or(0)
    };

    // Persist the resume `call.done` using the deployed `noetl.event`
    // column set (matches handlers::events).  The previous path went
    // through `db::queries::event::insert_event`, whose SQL targets
    // `attempt` + `id` columns that do not exist on the deployed
    // schema — so every callback POST 500'd with
    // `column "attempt" of relation "event" does not exist`, which
    // blocked the noetl/ai-meta#43 container-callback chain end to end.
    // CQRS write-path chokepoint (#103 2d-3).
    let ev = crate::handlers::event_write::EventRow::new(
        event_id,
        execution_id,
        catalog_id,
        "call.done",
        status_label,
        chrono::Utc::now(),
    )
    .with_node(&step)
    .with_result(result_obj.clone())
    .with_meta(serde_json::json!({ "node_type": "container" }));
    crate::handlers::event_write::emit_event(&state, pool, ev).await?;

    crate::metrics::record_container_callback(request.state.as_str());
    tracing::info!(
        execution_id,
        step = %step,
        state = request.state.as_str(),
        event_id,
        "container-callback: emitted call.done"
    );

    Ok((
        StatusCode::ACCEPTED,
        Json(ContainerCallbackResponse {
            status: "accepted_in_flight".to_string(),
            event_id: Some(event_id.to_string()),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_state_as_str_round_trip() {
        let cases = [
            (TerminalState::Succeeded, "succeeded"),
            (TerminalState::Failed, "failed"),
            (TerminalState::FailedImagePull, "failed_image_pull"),
            (TerminalState::FailedOom, "failed_oom"),
            (TerminalState::FailedNodeLost, "failed_node_lost"),
            (TerminalState::FailedTimeout, "failed_timeout"),
        ];
        for (state, label) in cases {
            assert_eq!(state.as_str(), label);
        }
    }

    #[test]
    fn terminal_state_is_success_only_succeeded() {
        assert!(TerminalState::Succeeded.is_success());
        for st in [
            TerminalState::Failed,
            TerminalState::FailedImagePull,
            TerminalState::FailedOom,
            TerminalState::FailedNodeLost,
            TerminalState::FailedTimeout,
        ] {
            assert!(!st.is_success(), "{:?} should not count as success", st);
        }
    }

    #[test]
    fn request_deserialises_minimal_body() {
        let raw = r#"{
            "state": "succeeded",
            "job_name": "noetl-container-step1-abcd-xyz"
        }"#;
        let parsed: ContainerCallbackRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.state, TerminalState::Succeeded);
        assert_eq!(parsed.job_name, "noetl-container-step1-abcd-xyz");
        assert!(parsed.job_uid.is_none());
        assert!(parsed.completed_at.is_none());
        assert!(parsed.exit_code.is_none());
    }

    #[test]
    fn request_deserialises_full_body() {
        let raw = r#"{
            "state": "failed_oom",
            "job_name": "noetl-container-train-42-q1",
            "job_uid": "01234567-89ab-cdef-0123-456789abcdef",
            "completed_at": "2026-06-07T04:00:00Z",
            "exit_code": 137,
            "reason": "Memory limit exceeded (256Mi)",
            "stdout_uri": "noetl://execution/42/result/train/1/stdout",
            "stderr_uri": "noetl://execution/42/result/train/1/stderr"
        }"#;
        let parsed: ContainerCallbackRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(parsed.state, TerminalState::FailedOom);
        assert_eq!(parsed.exit_code, Some(137));
        assert_eq!(parsed.reason.as_deref(), Some("Memory limit exceeded (256Mi)"));
        assert_eq!(
            parsed.completed_at,
            Some(
                chrono::DateTime::parse_from_rfc3339("2026-06-07T04:00:00Z")
                    .unwrap()
                    .with_timezone(&chrono::Utc)
            )
        );
    }

    #[test]
    fn request_rejects_unknown_state() {
        let raw = r#"{
            "state": "in_progress",
            "job_name": "j"
        }"#;
        let err = serde_json::from_str::<ContainerCallbackRequest>(raw).err();
        assert!(err.is_some(), "unknown state should fail deserialisation");
    }

    #[test]
    fn response_serialises_in_flight_with_event_id() {
        let r = ContainerCallbackResponse {
            status: "accepted_in_flight".to_string(),
            event_id: Some("1234567890".to_string()),
        };
        let body = serde_json::to_value(&r).unwrap();
        assert_eq!(body.get("status").and_then(|v| v.as_str()), Some("accepted_in_flight"));
        assert_eq!(body.get("event_id").and_then(|v| v.as_str()), Some("1234567890"));
    }

    #[test]
    fn response_serialises_stale_without_event_id() {
        let r = ContainerCallbackResponse {
            status: "accepted_stale".to_string(),
            event_id: None,
        };
        let body = serde_json::to_value(&r).unwrap();
        assert_eq!(body.get("status").and_then(|v| v.as_str()), Some("accepted_stale"));
        // skip_serializing_if elides the field on None.
        assert!(body.get("event_id").is_none());
    }
}
