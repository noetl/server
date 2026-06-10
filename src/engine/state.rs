//! Execution state reconstruction from events.
//!
//! Provides state reconstruction for event-sourced workflow execution.

use std::collections::HashMap;
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::db::models::Event;

/// Serde skip predicate for `i32` fields that default to 0.
pub(crate) fn is_zero(v: &i32) -> bool {
    *v == 0
}

/// High-level execution state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    /// Execution has not started yet.
    Initial,
    /// Execution is in progress.
    InProgress,
    /// Execution completed successfully.
    Completed,
    /// Execution failed.
    Failed,
    /// Execution was cancelled.
    Cancelled,
}

impl std::fmt::Display for ExecutionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Initial => write!(f, "initial"),
            Self::InProgress => write!(f, "in_progress"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
            Self::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl From<&str> for ExecutionState {
    fn from(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "initial" | "pending" => Self::Initial,
            "in_progress" | "running" => Self::InProgress,
            "completed" | "success" => Self::Completed,
            "failed" | "error" => Self::Failed,
            "cancelled" | "canceled" => Self::Cancelled,
            _ => Self::Initial,
        }
    }
}

/// State of a single workflow step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    /// Step has not been entered yet.
    Pending,
    /// Step has been entered (step.enter).
    Entered,
    /// Command has been issued.
    CommandIssued,
    /// Command has been claimed by a worker.
    CommandClaimed,
    /// Command execution has started.
    CommandStarted,
    /// Step completed successfully.
    Completed,
    /// Step failed.
    Failed,
    /// Step was skipped.
    Skipped,
}

impl std::fmt::Display for StepState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Entered => write!(f, "entered"),
            Self::CommandIssued => write!(f, "command_issued"),
            Self::CommandClaimed => write!(f, "command_claimed"),
            Self::CommandStarted => write!(f, "command_started"),
            Self::Completed => write!(f, "completed"),
            Self::Failed => write!(f, "failed"),
            Self::Skipped => write!(f, "skipped"),
        }
    }
}

/// Step information including state and result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepInfo {
    pub name: String,
    pub state: StepState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entered_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    pub attempt: i32,

    // -------- Iterator fan-out (Phase D R3b) --------
    //
    // A step with `step.loop` fans out into N iteration commands at
    // dispatch time.  The orchestrator emits ONE `step.enter` (which
    // records `iterations_expected` here) and N `command.issued`
    // events, each with a per-iteration `command_id` of the shape
    // `<exec>:<step>:<event>:i<index>` and `iteration_index` in
    // meta.  Workers that act on those commands echo `command_id`
    // forward in their emitted events but do NOT necessarily echo
    // `iteration_index` (worker contract is per-command, not
    // per-iteration), so `apply_event` deduplicates `command.completed`
    // events by `command_id` instead of by iteration_index.  The
    // step's `state` flips to `Completed` once we've seen
    // `iterations_expected` distinct command_ids complete.
    //
    // Non-looped steps leave these at their defaults and behave
    // exactly as before.
    /// Total iterations expected when the step is a `step.loop` step.
    /// `None` for non-looped steps; set from the `step.enter` event
    /// context at fan-out time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iterations_expected: Option<i32>,

    /// Distinct iteration command_ids observed as `command.completed`
    /// (dedup so a dual-worker race emitting two `command.completed`
    /// for the same command_id only counts once).  Always empty for
    /// non-looped steps.
    #[serde(default, skip_serializing_if = "std::collections::HashSet::is_empty")]
    pub iteration_command_ids: std::collections::HashSet<String>,

    /// Per-iteration result payloads collected in dispatch order.
    /// Used to assemble the aggregate result the next step sees in
    /// its render context.  Empty for non-looped steps.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub iteration_results: Vec<serde_json::Value>,

    // -------- Sequential-mode dispatch (#76) --------
    //
    // Tracks how many iteration commands have been issued (via
    // `command.issued` events) for this step.  Used by the
    // sequential-dispatch logic in orchestrator.rs: dispatch the
    // next iteration only when `iterations_dispatched ==
    // iterations_completed()` (no in-flight iteration).  For
    // parallel mode this field still increments but is never
    // consulted.  Non-iterator steps leave it at 0.
    /// Number of `command.issued` events observed for this iterator
    /// step.  Always 0 for non-iterator steps.
    #[serde(default, skip_serializing_if = "crate::engine::state::is_zero")]
    pub iterations_dispatched: i32,
}

impl StepInfo {
    /// Create a new step info in pending state.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            state: StepState::Pending,
            result: None,
            error: None,
            entered_at: None,
            completed_at: None,
            attempt: 0,
            iterations_expected: None,
            iteration_command_ids: std::collections::HashSet::new(),
            iteration_results: Vec::new(),
            iterations_dispatched: 0,
        }
    }

    /// True if this step was dispatched as a `step.loop` fan-out.
    pub fn is_iterator(&self) -> bool {
        self.iterations_expected.is_some()
    }

    /// Number of distinct iterations that have completed.  Always
    /// `0` for non-iterator steps.
    pub fn iterations_completed(&self) -> i32 {
        self.iteration_command_ids.len() as i32
    }
}

/// Complete workflow state reconstructed from events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowState {
    pub execution_id: i64,
    pub catalog_id: i64,
    pub state: ExecutionState,
    pub steps: HashMap<String, StepInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workload: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_execution_id: Option<i64>,
}

/// Pull a `command_id` out of an event row.  Workers echo
/// `command_id` forward through their emitted events; depending on
/// the lifecycle slot it may live on `meta`, on `result.context`
/// (constraint-compliant envelope), or — in older shapes — on
/// `result.data`.  Returns the first match.  Used by R3b iterator
/// state aggregation to deduplicate `command.completed` events.
fn extract_command_id(event: &Event) -> Option<String> {
    if let Some(meta) = &event.meta {
        if let Some(s) = meta.get("command_id").and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
    }
    if let Some(result) = &event.result {
        if let Some(s) = result
            .get("context")
            .and_then(|c| c.get("command_id"))
            .and_then(|v| v.as_str())
        {
            return Some(s.to_string());
        }
        if let Some(s) = result
            .get("data")
            .and_then(|d| d.get("command_id"))
            .and_then(|v| v.as_str())
        {
            return Some(s.to_string());
        }
    }
    None
}

impl WorkflowState {
    /// Create a new workflow state.
    pub fn new(execution_id: i64, catalog_id: i64) -> Self {
        Self {
            execution_id,
            catalog_id,
            state: ExecutionState::Initial,
            steps: HashMap::new(),
            workload: None,
            path: None,
            version: None,
            started_at: None,
            completed_at: None,
            parent_execution_id: None,
        }
    }

    /// Reconstruct workflow state from a list of events.
    pub fn from_events(events: &[Event]) -> Option<Self> {
        let start = Instant::now();

        if events.is_empty() {
            return None;
        }

        // Get execution_id and catalog_id from first event
        let first = &events[0];
        let mut state = Self::new(first.execution_id, first.catalog_id);

        // Process events in order
        for event in events {
            state.apply_event(event);
        }

        let duration = start.elapsed();
        let event_count = events.len();

        // Log performance metrics for state reconstruction
        tracing::info!(
            target: "noetl.performance",
            execution_id = %first.execution_id,
            phase = "state_reconstruction",
            event_count = %event_count,
            step_count = %state.steps.len(),
            duration_ms = %duration.as_millis(),
            "State reconstructed from events"
        );

        // Warn if reconstruction is slow (potential bottleneck)
        if duration.as_millis() > 100 || event_count > 50 {
            tracing::warn!(
                target: "noetl.performance",
                execution_id = %first.execution_id,
                event_count = %event_count,
                duration_ms = %duration.as_millis(),
                "Slow state reconstruction detected - consider optimizing event loading"
            );
        }

        Some(state)
    }

    /// Apply a single event to update the workflow state.
    pub fn apply_event(&mut self, event: &Event) {
        match event.event_type.as_str() {
            "playbook_started" => {
                self.state = ExecutionState::InProgress;
                self.started_at = Some(event.created_at);
                self.parent_execution_id = event.parent_execution_id;

                // Extract workload from context
                if let Some(context) = &event.context {
                    if let Some(workload) = context.get("workload") {
                        self.workload = Some(workload.clone());
                    }
                    if let Some(path) = context.get("path").and_then(|v| v.as_str()) {
                        self.path = Some(path.to_string());
                    }
                    if let Some(version) = context.get("version").and_then(|v| v.as_str()) {
                        self.version = Some(version.to_string());
                    }
                }
            }
            "playbook_completed" | "playbook.completed" => {
                self.state = ExecutionState::Completed;
                self.completed_at = Some(event.created_at);
            }
            "playbook_failed" | "playbook.failed" => {
                self.state = ExecutionState::Failed;
                self.completed_at = Some(event.created_at);
            }
            "playbook.cancelled" => {
                self.state = ExecutionState::Cancelled;
                self.completed_at = Some(event.created_at);
            }
            "step.enter" | "step_enter" | "step_started" => {
                if let Some(name) = &event.node_name {
                    let step = self
                        .steps
                        .entry(name.clone())
                        .or_insert_with(|| StepInfo::new(name));
                    step.state = StepState::Entered;
                    step.entered_at = Some(event.created_at);
                    // R3b iterator fan-out: orchestrator stamps the
                    // iteration total onto the step.enter event so
                    // state reconstruction knows how many
                    // command.completed events to wait for before
                    // marking the step truly Completed.  The
                    // orchestrator emits `EventToEmit { context:
                    // Some(...) }`, which `trigger_orchestrator`
                    // persists by wrapping it inside the
                    // constraint-compliant `{status, context}`
                    // result envelope (per noetl/server#29) — so
                    // the canonical storage location is
                    // `event.result.context.iterations_expected`.
                    // Older callers may have populated the event
                    // row's `context` column directly; we accept
                    // both shapes.  Workers' own per-iteration
                    // step.enter events don't carry this key and
                    // leave the previously-set value alone.
                    let total = event
                        .result
                        .as_ref()
                        .and_then(|r| r.get("context"))
                        .and_then(|c| c.get("iterations_expected"))
                        .and_then(|v| v.as_i64())
                        .or_else(|| {
                            event
                                .context
                                .as_ref()
                                .and_then(|c| c.get("iterations_expected"))
                                .and_then(|v| v.as_i64())
                        });
                    if let Some(total) = total {
                        step.iterations_expected = Some(total as i32);
                    }
                }
            }
            "step.skipped" | "step_skipped" => {
                // Phase D R4 slice 2 (noetl/ai-meta#49 →
                // noetl/server#144).  The orchestrator emits
                // `step.skipped` when a step's `when` guard evaluates
                // false (see `process_in_progress` in orchestrator.rs).
                // Without this arm `reconstruct` left the step in
                // `StepState::Pending` and every downstream consumer
                // (fan-in barrier check, completion-decision quiescent
                // clause, next-pass dispatch loop) was blind to the
                // skip.  The barrier check needs `is_step_done` to
                // see `Skipped` so a fan-in target with a guard-false
                // upstream + a real upstream eventually dispatches.
                //
                // We set `entered_at` to the event's `created_at` —
                // semantically the step's lifecycle began at the
                // moment the guard was evaluated; the workflow has
                // no other anchor for skipped steps.
                if let Some(name) = &event.node_name {
                    let step = self
                        .steps
                        .entry(name.clone())
                        .or_insert_with(|| StepInfo::new(name));
                    step.state = StepState::Skipped;
                    step.entered_at = Some(event.created_at);
                    step.completed_at = Some(event.created_at);
                }
            }
            "command.issued" => {
                if let Some(name) = &event.node_name {
                    let step = self
                        .steps
                        .entry(name.clone())
                        .or_insert_with(|| StepInfo::new(name));
                    step.state = StepState::CommandIssued;
                    // #76: track dispatched iteration count for
                    // sequential-mode guard in orchestrator.rs.
                    if step.is_iterator() {
                        step.iterations_dispatched += 1;
                    }
                }
            }
            "command.claimed" => {
                if let Some(name) = &event.node_name {
                    let step = self
                        .steps
                        .entry(name.clone())
                        .or_insert_with(|| StepInfo::new(name));
                    step.state = StepState::CommandClaimed;
                }
            }
            "command.started" | "action_started" => {
                if let Some(name) = &event.node_name {
                    let step = self
                        .steps
                        .entry(name.clone())
                        .or_insert_with(|| StepInfo::new(name));
                    step.state = StepState::CommandStarted;
                    if let Some(attempt) = event.attempt {
                        step.attempt = attempt;
                    }
                }
            }
            "command.completed" | "action_completed" | "step.exit" | "step_completed" => {
                if let Some(name) = &event.node_name {
                    let step = self
                        .steps
                        .entry(name.clone())
                        .or_insert_with(|| StepInfo::new(name));

                    // R3b iterator-aware completion: if this step is
                    // a loop step (iterations_expected set), count
                    // each distinct `command_id` (sourced from meta
                    // or result.context) toward completion.  Workers
                    // emit multiple events per command (claimed →
                    // started → call.done → completed), and a
                    // dual-worker race may even emit two
                    // `command.completed` events for the same
                    // command_id — both are deduped by the HashSet.
                    // Only flip state to Completed once we've seen
                    // `iterations_expected` distinct command_ids
                    // complete.  Non-iterator steps continue to
                    // complete on the first command.completed.
                    if let Some(expected) = step.iterations_expected {
                        let command_id = extract_command_id(event);
                        if let Some(cid) = command_id {
                            // First time we've seen this iteration?
                            // Append its result in arrival order.
                            if step.iteration_command_ids.insert(cid) {
                                if let Some(result) = event.result.clone() {
                                    step.iteration_results.push(result);
                                }
                            }
                        }
                        if step.iterations_completed() >= expected {
                            step.state = StepState::Completed;
                            step.completed_at = Some(event.created_at);
                            // Aggregate result = list of per-iteration
                            // results in arrival order (may not match
                            // dispatch index in parallel mode — see
                            // R3b follow-up).
                            step.result =
                                Some(serde_json::Value::Array(step.iteration_results.clone()));
                        }
                        // Mid-iteration: leave step.state at whatever
                        // command.started / command.claimed last set
                        // it to so `is_step_completed` returns false.
                    } else {
                        // Plain (non-iterator) step.
                        step.state = StepState::Completed;
                        step.completed_at = Some(event.created_at);
                        // Only overwrite step.result with command.completed's
                        // envelope if the user-data hasn't been written yet
                        // (e.g. by an earlier `call.done`).  command.completed
                        // carries only `{status, command_id}`, no data — so
                        // overwriting would lose the rich payload that
                        // next.arcs / step.when need.  See noetl/ai-meta#60
                        // for the orchestrator-template gap that surfaced
                        // this.
                        if step.result.is_none() {
                            step.result = event.result.clone();
                        }
                    }
                }
            }
            "call.done" | "action_done" => {
                // The worker emits `call.done` between
                // `command.started` and `command.completed` to carry
                // the user-code result.  Capture step.result here so
                // the orchestrator's template context (built via
                // `build_context`) can expose `{{ step_name.field }}`
                // for next.arcs / step.when evaluation.
                //
                // The state stays at CommandStarted — `command.completed`
                // (above) flips to Completed.  This event's purpose
                // here is data attachment only.
                if let Some(name) = &event.node_name {
                    let step = self
                        .steps
                        .entry(name.clone())
                        .or_insert_with(|| StepInfo::new(name));
                    // For iterator steps the iteration-aware branch
                    // in command.completed builds the per-iteration
                    // result array; leave it alone here.  Plain steps
                    // get their data attached.
                    if step.iterations_expected.is_none() {
                        if let Some(result) = event.result.clone() {
                            step.result = Some(result);
                        }
                    }
                }
            }
            "command.failed" | "action_failed" | "step_failed" => {
                if let Some(name) = &event.node_name {
                    let step = self
                        .steps
                        .entry(name.clone())
                        .or_insert_with(|| StepInfo::new(name));
                    step.state = StepState::Failed;
                    step.completed_at = Some(event.created_at);
                    // Extract error from result.  Two shapes seen in
                    // the wild — top-level `result.error` and the
                    // nested `result.context.error` (the worker's
                    // standard envelope wraps the tool's
                    // `{status, error, ...}` output under
                    // `result.context`).  Try the top-level form
                    // first, then fall back to the nested form so
                    // step.error gets populated regardless of which
                    // tool emitted the failure.  See
                    // noetl/ai-meta#58 for the orchestrator-side
                    // failure-termination fix that depends on this.
                    if let Some(result) = &event.result {
                        let err_value =
                            result.get("error").and_then(|v| v.as_str()).or_else(|| {
                                result
                                    .get("context")
                                    .and_then(|c| c.get("error"))
                                    .and_then(|v| v.as_str())
                            });
                        if let Some(error) = err_value {
                            step.error = Some(error.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Get the result for a specific step.
    pub fn get_step_result(&self, step_name: &str) -> Option<&serde_json::Value> {
        self.steps.get(step_name).and_then(|s| s.result.as_ref())
    }

    /// Get all step results as a map.
    pub fn get_all_results(&self) -> HashMap<String, serde_json::Value> {
        self.steps
            .iter()
            .filter_map(|(name, info)| info.result.clone().map(|r| (name.clone(), r)))
            .collect()
    }

    /// Check if a step has completed (successfully or with failure).
    pub fn is_step_done(&self, step_name: &str) -> bool {
        self.steps
            .get(step_name)
            .map(|s| {
                matches!(
                    s.state,
                    StepState::Completed | StepState::Failed | StepState::Skipped
                )
            })
            .unwrap_or(false)
    }

    /// Check if a step completed successfully.
    pub fn is_step_completed(&self, step_name: &str) -> bool {
        self.steps
            .get(step_name)
            .map(|s| matches!(s.state, StepState::Completed))
            .unwrap_or(false)
    }

    /// Check if a step failed.
    pub fn is_step_failed(&self, step_name: &str) -> bool {
        self.steps
            .get(step_name)
            .map(|s| matches!(s.state, StepState::Failed))
            .unwrap_or(false)
    }

    /// Get the names of all completed steps.
    pub fn completed_steps(&self) -> Vec<&str> {
        self.steps
            .iter()
            .filter(|(_, info)| matches!(info.state, StepState::Completed))
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// Get the names of all running steps.
    pub fn running_steps(&self) -> Vec<&str> {
        self.steps
            .iter()
            .filter(|(_, info)| {
                matches!(
                    info.state,
                    StepState::Entered
                        | StepState::CommandIssued
                        | StepState::CommandClaimed
                        | StepState::CommandStarted
                )
            })
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// Check if there are any running steps.
    pub fn has_running_steps(&self) -> bool {
        !self.running_steps().is_empty()
    }

    /// Build a context map for template rendering.
    pub fn build_context(&self) -> serde_json::Value {
        let mut context = serde_json::Map::new();

        // Add workload variables.  Each key is exposed both at the
        // top level (so `{{ skip_middle }}` works) AND under the
        // `workload` namespace (so `{{ workload.skip_middle }}`
        // works) — matches the Python reference shape and the
        // generate_initial_commands path in handlers/execute.rs.
        // Without the `workload` namespace, step.when expressions
        // that reference `workload.X` raise an undefined-value
        // template error during transition evaluation.
        if let Some(serde_json::Value::Object(wl)) = &self.workload {
            for (k, v) in wl {
                context.insert(k.clone(), v.clone());
            }
            context.insert(
                "workload".to_string(),
                serde_json::Value::Object(wl.clone()),
            );
        }

        // Add step results under TWO shapes, matching the Python
        // reference and the canonical v10 playbook YAML:
        //
        // - `steps.<name>` carries the FULL envelope as-stored
        //   (back-compat for `{{ steps.eval_flag.status }}`-style
        //   references and admin tooling that wants the wrapper
        //   metadata).
        // - `<name>` at the TOP level carries the UNWRAPPED user
        //   data — the dict the tool's user code assigned to
        //   `result = {...}`.  This is the shape next.arcs /
        //   step.when guards read via `{{ eval_flag.is_hot }}`
        //   (no `steps.` / no `.data.` prefix needed).
        //
        // The envelope shape stored on `info.result` after wrapping
        // by `apply_event` is:
        //   { status, context: { result: { status, context: {
        //       data: <USER_DATA>, status, stdout, stderr, ... } } } }
        // — `extract_user_data` walks the envelope and returns the
        // inner `data` value.  See noetl/ai-meta#60 for the e2e
        // finding that surfaced this orchestrator template gap.
        let mut steps = serde_json::Map::new();
        for (name, info) in &self.steps {
            if let Some(result) = &info.result {
                steps.insert(name.clone(), result.clone());
                if let Some(user_data) = extract_user_data(result) {
                    // Expose BOTH the flat user_data fields (so
                    // `{{ step.field }}` works) AND a synthetic
                    // `.data` accessor that re-references the same
                    // user_data (so `{{ step.data }}` /
                    // `{{ step.data.field }}` also work).  Canonical
                    // v10 fixtures use both shapes interchangeably —
                    // single-tool python steps producing a flat
                    // `result = {...}` dict need the `.data` accessor
                    // because the worker envelope doesn't add it
                    // (only the task_sequence flatten path does, and
                    // single-tool steps skip task_sequence wrapping).
                    //
                    // Don't clobber an existing `.data` on the
                    // user_data: the task_sequence flatten may have
                    // already populated it from a labeled sub-task's
                    // `data` field.  Tracks noetl/ai-meta#66.
                    let with_data = match &user_data {
                        serde_json::Value::Object(map) if !map.contains_key("data") => {
                            let mut m = map.clone();
                            m.insert("data".to_string(), user_data.clone());
                            serde_json::Value::Object(m)
                        }
                        _ => user_data,
                    };
                    context.insert(name.clone(), with_data);
                }
            }
        }
        context.insert("steps".to_string(), serde_json::Value::Object(steps));

        // Add execution metadata
        context.insert(
            "execution_id".to_string(),
            serde_json::json!(self.execution_id.to_string()),
        );
        context.insert(
            "catalog_id".to_string(),
            serde_json::json!(self.catalog_id.to_string()),
        );

        if let Some(path) = &self.path {
            context.insert("path".to_string(), serde_json::json!(path));
        }
        if let Some(version) = &self.version {
            context.insert("version".to_string(), serde_json::json!(version));
        }

        serde_json::Value::Object(context)
    }
}

/// Apply DSL Core `set:` mutations to a variable map (template rendering context).
///
/// Mirrors Python's `_apply_set_mutations` in
/// `noetl/core/dsl/engine/executor/common.py:472-484` verbatim:
///
/// - Scoped keys (`ctx.x`, `iter.x`, `step.x`) have the scope prefix stripped
///   and the bare key is written.
/// - Bare keys (no dot) are written as-is.
/// - Dotted keys whose scope is not `ctx`/`iter`/`step` are written as-is
///   (the dot does NOT split them; the full key is the map key).
///
/// `mutations` contains the **already-rendered** template values (caller must
/// render before calling).  The function is purely a scope-stripping write.
pub fn apply_set_mutations(
    variables: &mut HashMap<String, serde_json::Value>,
    mutations: &HashMap<String, serde_json::Value>,
) {
    for (key, value) in mutations {
        if let Some((scope, bare)) = key.split_once('.') {
            if matches!(scope, "ctx" | "iter" | "step") {
                variables.insert(bare.to_string(), value.clone());
                continue;
            }
        }
        variables.insert(key.clone(), value.clone());
    }
}

/// Unwrap a step result envelope to the inner user data dict.
///
/// The wrap layers come from `apply_event`'s standard envelope:
///
/// ```text
/// outer = {
///   status: "COMPLETED",
///   context: {
///     result: {
///       status: "success",
///       context: {
///         data: <USER_DATA>,
///         status, stdout, stderr, ...
///       }
///     },
///     ...
///   }
/// }
/// ```
///
/// Returns the inner `data` value when the wrapper shape matches.
/// Falls back to the outer value (or any partially-unwrapped layer)
/// when the wrapper is absent — handles tooling that emitted a
/// flat result without going through the worker's envelope path.
/// Returns None only when the input is JSON null.
///
/// Tracks noetl/ai-meta#60 — without this unwrap, v10 playbooks
/// that reference `{{ step_name.field }}` in next.arcs / step.when
/// see an undefined value because the envelope's `status` /
/// `context` keys swallowed the user fields.
pub(crate) fn extract_user_data(result: &serde_json::Value) -> Option<serde_json::Value> {
    if result.is_null() {
        return None;
    }
    // Try outer.context.result.context.data — the standard
    // wrapper shape.  Each step is optional so a partial
    // unwrap still yields a useful value for back-compat.
    let inner = result
        .get("context")
        .and_then(|v| v.get("result"))
        .and_then(|v| v.get("context"))
        .and_then(|v| v.get("data"));
    if let Some(data) = inner {
        return Some(flatten_task_sequence_data(data));
    }
    // Single-layer wrappers (e.g. {status, context}).
    if let Some(ctx) = result.get("context") {
        if let Some(data) = ctx.get("data") {
            return Some(flatten_task_sequence_data(data));
        }
        return Some(ctx.clone());
    }
    Some(result.clone())
}

/// Flatten task_sequence's label-keyed result map so that the
/// user-facing `{{ step.field }}` references resolve.
///
/// Every v10 step uses the `tool: [...]` list shape, which the
/// server wraps as a `task_sequence` pipeline even when the step
/// has a single tool.  `task_sequence` then aggregates the
/// sub-task results as `{label1: <data1>, label2: <data2>, ...}`,
/// so the unwrapped envelope data ends up as
/// `{init_action: {data: {executed: true}, status, message}}`
/// rather than the user-assigned dict the YAML template
/// expects (`{data: {executed: true}, status, message}`).
///
/// Strategy: when `data` is a non-empty object whose values are
/// ALL objects (the task_sequence labeled-results signature),
/// merge each task's fields at the top level — last-task-wins on
/// key collisions, matching the `_prev` convention inside the
/// pipeline.  The original labeled shape is preserved so
/// `{{ step.label.field }}` references still work alongside the
/// flat `{{ step.field }}` form.
///
/// For non-task_sequence data (a single tool that wasn't wrapped,
/// or a tool that returned a scalar / array / mixed map) this is
/// a no-op — the returned value equals the input.
fn flatten_task_sequence_data(data: &serde_json::Value) -> serde_json::Value {
    let map = match data.as_object() {
        Some(m) if !m.is_empty() => m,
        _ => return data.clone(),
    };
    // Heuristic: labeled-results shape has every value as an
    // object.  A user-assigned dict that happens to be
    // `{data: ..., status: ...}` has scalar / string values for
    // some keys, so this won't accidentally merge it.
    let all_objects = map.values().all(|v| v.is_object());
    if !all_objects {
        return data.clone();
    }
    // Build merged shape: labeled-keys at top + flat keys from
    // each task's value.  Iterate in insertion order so the last
    // task's keys win on collision (matches `_prev`).
    let mut merged = map.clone();
    for value in map.values() {
        if let serde_json::Value::Object(task_map) = value {
            for (k, v) in task_map {
                merged.insert(k.clone(), v.clone());
            }
        }
    }
    serde_json::Value::Object(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(event_type: &str, node_name: Option<&str>) -> Event {
        Event {
            id: 1,
            execution_id: 12345,
            catalog_id: 67890,
            event_id: 1,
            parent_event_id: None,
            parent_execution_id: None,
            event_type: event_type.to_string(),
            node_id: None,
            node_name: node_name.map(|s| s.to_string()),
            node_type: None,
            status: "".to_string(),
            context: None,
            meta: None,
            result: None,
            worker_id: None,
            attempt: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn test_execution_state_display() {
        assert_eq!(ExecutionState::Initial.to_string(), "initial");
        assert_eq!(ExecutionState::InProgress.to_string(), "in_progress");
        assert_eq!(ExecutionState::Completed.to_string(), "completed");
    }

    #[test]
    fn test_execution_state_from_str() {
        assert_eq!(ExecutionState::from("initial"), ExecutionState::Initial);
        assert_eq!(ExecutionState::from("RUNNING"), ExecutionState::InProgress);
        assert_eq!(ExecutionState::from("completed"), ExecutionState::Completed);
        assert_eq!(ExecutionState::from("FAILED"), ExecutionState::Failed);
    }

    #[test]
    fn test_workflow_state_from_events() {
        let events = vec![
            {
                let mut e = make_event("playbook_started", None);
                e.context = Some(serde_json::json!({
                    "workload": {"key": "value"},
                    "path": "test/playbook",
                    "version": "1"
                }));
                e
            },
            make_event("step.enter", Some("step1")),
            make_event("command.issued", Some("step1")),
            {
                let mut e = make_event("command.completed", Some("step1"));
                e.result = Some(serde_json::json!({"output": "success"}));
                e
            },
        ];

        let state = WorkflowState::from_events(&events).unwrap();
        assert_eq!(state.execution_id, 12345);
        assert_eq!(state.state, ExecutionState::InProgress);
        assert!(state.is_step_completed("step1"));
        assert_eq!(
            state.get_step_result("step1"),
            Some(&serde_json::json!({"output": "success"}))
        );
    }

    #[test]
    fn test_workflow_state_build_context() {
        let mut state = WorkflowState::new(12345, 67890);
        state.workload = Some(serde_json::json!({"var1": "value1"}));
        state.path = Some("test/path".to_string());

        let mut step_info = StepInfo::new("step1");
        step_info.result = Some(serde_json::json!({"output": "result1"}));
        state.steps.insert("step1".to_string(), step_info);

        let context = state.build_context();
        assert_eq!(context.get("var1").and_then(|v| v.as_str()), Some("value1"));
        assert_eq!(
            context.get("path").and_then(|v| v.as_str()),
            Some("test/path")
        );
        assert!(context.get("steps").is_some());
    }

    #[test]
    fn test_step_state_transitions() {
        let mut state = WorkflowState::new(1, 1);

        state.apply_event(&make_event("step.enter", Some("step1")));
        assert_eq!(state.steps.get("step1").unwrap().state, StepState::Entered);

        state.apply_event(&make_event("command.issued", Some("step1")));
        assert_eq!(
            state.steps.get("step1").unwrap().state,
            StepState::CommandIssued
        );

        state.apply_event(&make_event("command.completed", Some("step1")));
        assert_eq!(
            state.steps.get("step1").unwrap().state,
            StepState::Completed
        );
    }

    /// Phase D R4 slice 2 (noetl/server#144).  `step.skipped`
    /// events emitted by the orchestrator (`process_in_progress`
    /// when a step's `when` guard evaluates false) used to be
    /// silently dropped by `apply_event` — leaving the step in
    /// `StepState::Pending` and breaking the fan-in barrier's
    /// terminal-state check for guard-skipped upstreams.  The new
    /// arm records the step into `state.steps` with
    /// `StepState::Skipped` and stamps `entered_at` +
    /// `completed_at` to the event timestamp so the lifecycle is
    /// recorded even though no actual work ran.
    #[test]
    fn step_skipped_event_marks_state_skipped() {
        let mut state = WorkflowState::new(1, 1);

        // Step doesn't exist yet — apply_event creates it.
        state.apply_event(&make_event("step.skipped", Some("guarded_step")));
        let step = state
            .steps
            .get("guarded_step")
            .expect("apply_event should record the skipped step");
        assert_eq!(step.state, StepState::Skipped);
        assert!(step.entered_at.is_some());
        assert!(step.completed_at.is_some());

        // Skipped step is terminal — `is_step_done` returns true
        // (this is the load-bearing check for the fan-in barrier).
        assert!(state.is_step_done("guarded_step"));
        // But it's NOT completed (Completed and Skipped are
        // distinct terminal states); the dashboard should be able
        // to tell them apart.
        assert!(!state.is_step_completed("guarded_step"));
    }

    /// Underscore alias `step_skipped` works the same as the
    /// dotted form — both are emitted depending on the producer
    /// (Python-era code historically used the underscore form;
    /// the orchestrator and apply_event now accept both).
    #[test]
    fn step_skipped_underscore_alias_also_marks_skipped() {
        let mut state = WorkflowState::new(1, 1);
        state.apply_event(&make_event("step_skipped", Some("guarded_step")));
        assert_eq!(
            state.steps.get("guarded_step").unwrap().state,
            StepState::Skipped
        );
    }

    #[test]
    fn test_iterator_step_aggregates_completion() {
        // Simulate the events an iterator step produces:
        //   step.enter (iterations_expected=3)
        //   command.completed (iteration_index=0)
        //   command.completed (iteration_index=1)
        //   command.completed (iteration_index=2)
        //
        // The step's state stays "not completed" until all 3
        // iterations land, then flips to Completed with an
        // aggregated array result.
        let mut state = WorkflowState::new(1, 1);

        let mut enter = make_event("step.enter", Some("looped"));
        enter.context = Some(serde_json::json!({
            "iterations_expected": 3,
            "iterator_var": "item",
        }));
        state.apply_event(&enter);
        let after_enter = state.steps.get("looped").unwrap();
        assert_eq!(after_enter.state, StepState::Entered);
        assert_eq!(after_enter.iterations_expected, Some(3));
        assert_eq!(after_enter.iterations_completed(), 0);

        for (idx, payload) in [(0, "a"), (1, "b"), (2, "c")] {
            let mut ev = make_event("command.completed", Some("looped"));
            ev.meta = Some(serde_json::json!({
                "command_id": format!("e:looped:0:i{}", idx),
                "iteration_index": idx,
                "iteration_total": 3,
            }));
            ev.result = Some(serde_json::json!({ "value": payload }));
            state.apply_event(&ev);
        }

        let info = state.steps.get("looped").unwrap();
        assert_eq!(info.state, StepState::Completed);
        assert_eq!(info.iterations_completed(), 3);
        // Aggregate result is the per-iteration array in arrival order.
        let agg = info.result.as_ref().unwrap();
        assert_eq!(agg.as_array().map(|a| a.len()), Some(3));
        let values: Vec<String> = agg
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.get("value").unwrap().as_str().unwrap().to_string())
            .collect();
        assert_eq!(values, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_iterator_step_dedupes_duplicate_command_completed() {
        // Two `command.completed` events for the same command_id
        // (dual-worker race) should NOT double-count the iteration.
        let mut state = WorkflowState::new(1, 1);
        let mut enter = make_event("step.enter", Some("looped"));
        enter.context = Some(serde_json::json!({
            "iterations_expected": 2,
        }));
        state.apply_event(&enter);

        for _ in 0..2 {
            let mut ev = make_event("command.completed", Some("looped"));
            ev.meta = Some(serde_json::json!({
                "command_id": "e:looped:0:i0",
            }));
            ev.result = Some(serde_json::json!({"i": 0}));
            state.apply_event(&ev);
        }
        // Only 1 distinct command_id seen.
        let info = state.steps.get("looped").unwrap();
        assert_eq!(info.iterations_completed(), 1);
        assert_ne!(info.state, StepState::Completed);

        // Now the second iteration's command_id completes.
        let mut ev = make_event("command.completed", Some("looped"));
        ev.meta = Some(serde_json::json!({
            "command_id": "e:looped:0:i1",
        }));
        ev.result = Some(serde_json::json!({"i": 1}));
        state.apply_event(&ev);
        let info = state.steps.get("looped").unwrap();
        assert_eq!(info.iterations_completed(), 2);
        assert_eq!(info.state, StepState::Completed);
    }

    #[test]
    fn test_iterator_step_partial_completion_stays_running() {
        // Two of three iterations done — step should NOT be
        // Completed yet (state is whatever the last event left it
        // at, but `is_step_completed` returns false).
        let mut state = WorkflowState::new(1, 1);

        let mut enter = make_event("step.enter", Some("looped"));
        enter.context = Some(serde_json::json!({
            "iterations_expected": 3,
        }));
        state.apply_event(&enter);

        for idx in 0..2 {
            let mut ev = make_event("command.completed", Some("looped"));
            ev.meta = Some(serde_json::json!({
                "command_id": format!("e:looped:0:i{}", idx),
            }));
            ev.result = Some(serde_json::json!({"i": idx}));
            state.apply_event(&ev);
        }

        let info = state.steps.get("looped").unwrap();
        assert_ne!(info.state, StepState::Completed);
        assert_eq!(info.iterations_completed(), 2);
        assert!(!state.is_step_completed("looped"));
    }

    #[test]
    fn test_iterator_partial_with_worker_step_exit_does_not_complete() {
        // Reproduces the R3b kind-val symptom: orchestrator emits
        // step.enter(iterations_expected=3), 3 command.issued events
        // fire, then ONE iteration's worker lifecycle arrives
        // (command.claimed/started + worker's step.enter + call.done
        // + step.exit + command.completed).  The looped step must
        // NOT be marked Completed after only 1 iteration, even
        // though step.exit AND command.completed both go through
        // the iteration-aware match arm and both carry the same
        // command_id in result.context.
        let mut state = WorkflowState::new(1, 1);

        // 1. Orchestrator's initial step.enter — populates
        //    iterations_expected.  In production the orchestrator
        //    persists this via `trigger_orchestrator`, which wraps
        //    `EventToEmit.context` in a `{status, context}` result
        //    envelope (per noetl/server#29's chk_event_result_shape
        //    constraint).  So the canonical storage location is
        //    `event.result.context.iterations_expected`, NOT the
        //    event row's `context` column.  Earlier tests used the
        //    `event.context` shape; we accept both via the
        //    apply_event fallback, so this test uses the
        //    production shape.
        let mut enter = make_event("step.enter", Some("looped"));
        enter.result = Some(serde_json::json!({
            "status": "ENTERED",
            "context": {
                "iterations_expected": 3,
                "iterator_var": "item",
            },
        }));
        state.apply_event(&enter);

        // 2. Three command.issued events — each with a distinct
        //    per-iteration command_id in meta.
        for idx in 0..3 {
            let mut ev = make_event("command.issued", Some("looped"));
            ev.meta = Some(serde_json::json!({
                "command_id": format!("exec:looped:e0:i{}", idx),
                "iteration_index": idx,
                "iteration_total": 3,
            }));
            state.apply_event(&ev);
        }

        // 3. One iteration's worker lifecycle (only iter i2).
        let cid = "exec:looped:e0:i2".to_string();
        let mut claimed = make_event("command.claimed", Some("looped"));
        claimed.meta = Some(serde_json::json!({"command_id": cid}));
        state.apply_event(&claimed);

        let mut started = make_event("command.started", Some("looped"));
        started.meta = Some(serde_json::json!({"command_id": cid}));
        state.apply_event(&started);

        // Worker's per-iteration step.enter — no iterations_expected
        // in context, so iterations_expected must stay Some(3).
        let mut worker_enter = make_event("step.enter", Some("looped"));
        worker_enter.context = Some(serde_json::json!({"status": "started"}));
        state.apply_event(&worker_enter);

        // call.done — not in any match arm, no state change.
        let call_done = make_event("call.done", Some("looped"));
        state.apply_event(&call_done);

        // step.exit — IS in the command.completed arm.  Carries
        // command_id in result.context.
        let mut step_exit = make_event("step.exit", Some("looped"));
        step_exit.result = Some(serde_json::json!({
            "status": "COMPLETED",
            "context": { "command_id": cid.clone(), "status": "COMPLETED" }
        }));
        state.apply_event(&step_exit);

        // command.completed — same command_id (dedupes via HashSet).
        let mut completed = make_event("command.completed", Some("looped"));
        completed.result = Some(serde_json::json!({
            "status": "COMPLETED",
            "context": { "command_id": cid.clone(), "worker_id": "w" }
        }));
        state.apply_event(&completed);

        let info = state.steps.get("looped").unwrap();
        assert_eq!(info.iterations_expected, Some(3));
        assert_eq!(
            info.iterations_completed(),
            1,
            "only ONE distinct command_id observed across step.exit + command.completed; \
             iteration_command_ids = {:?}",
            info.iteration_command_ids
        );
        assert_ne!(
            info.state,
            StepState::Completed,
            "looped must NOT be Completed after only 1 of 3 iterations; state = {:?}",
            info.state
        );
        assert!(!state.is_step_completed("looped"));
    }

    #[test]
    fn test_plain_step_unaffected_by_iterator_logic() {
        // A plain step (no iterations_expected) continues to
        // complete on the first command.completed, same as before.
        let mut state = WorkflowState::new(1, 1);
        state.apply_event(&make_event("step.enter", Some("plain")));
        let mut ev = make_event("command.completed", Some("plain"));
        ev.result = Some(serde_json::json!({"ok": true}));
        state.apply_event(&ev);
        let info = state.steps.get("plain").unwrap();
        assert_eq!(info.state, StepState::Completed);
        assert_eq!(info.iterations_expected, None);
        assert_eq!(info.iterations_completed(), 0);
        assert_eq!(info.result, Some(serde_json::json!({"ok": true})));
    }

    #[test]
    fn test_extract_user_data_unwraps_standard_envelope() {
        // Standard wrapper shape emitted by apply_event after
        // the worker's PythonTool result-capture (noetl/tools#17).
        // The orchestrator's template context needs the inner
        // `data` exposed so `{{ step_name.field }}` resolves.
        let envelope = serde_json::json!({
            "status": "COMPLETED",
            "context": {
                "result": {
                    "status": "success",
                    "context": {
                        "data": {"is_hot": true, "message": "hot"},
                        "status": "success",
                        "stdout": "",
                        "stderr": "",
                    },
                },
                "call_index": 0,
            },
        });
        let data = extract_user_data(&envelope).expect("unwrap should succeed");
        assert_eq!(data.get("is_hot").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(data.get("message").and_then(|v| v.as_str()), Some("hot"));
    }

    #[test]
    fn test_extract_user_data_handles_flat_result() {
        // Back-compat: a tool that emitted a flat result without
        // going through the wrapper.  No `context.result.context.data`
        // path → fall back through the partial-unwrap branches and
        // ultimately return the input.
        let flat = serde_json::json!({"is_hot": false});
        let data = extract_user_data(&flat).expect("flat result preserved");
        assert_eq!(data, flat);
    }

    #[test]
    fn test_extract_user_data_null_returns_none() {
        let null = serde_json::Value::Null;
        assert!(extract_user_data(&null).is_none());
    }

    #[test]
    fn test_build_context_exposes_step_data_at_top_level() {
        // noetl/ai-meta#60 — workflow YAML uses `{{ eval_flag.is_hot }}`
        // (no `steps.` prefix), so the build_context must expose
        // each step's unwrapped data at the top level alongside the
        // back-compat `steps.<name>` shape.
        let mut state = WorkflowState::new(1, 1);
        state.workload = Some(serde_json::json!({"temp": 30}));
        let mut info = StepInfo::new("eval_flag");
        info.result = Some(serde_json::json!({
            "status": "COMPLETED",
            "context": {
                "result": {
                    "status": "success",
                    "context": {
                        "data": {"is_hot": true, "message": "hot"},
                    },
                },
            },
        }));
        state.steps.insert("eval_flag".to_string(), info);

        let ctx = state.build_context();
        // Top-level: `eval_flag.is_hot` resolves to the user data.
        let eval_flag = ctx.get("eval_flag").expect("top-level step data exposed");
        assert_eq!(
            eval_flag.get("is_hot").and_then(|v| v.as_bool()),
            Some(true)
        );
        // Back-compat: `steps.eval_flag` still holds the full envelope.
        let steps = ctx.get("steps").expect("steps namespace present");
        assert!(
            steps.get("eval_flag").is_some(),
            "back-compat steps namespace populated"
        );
        // Workload still at top level (from earlier build_context behavior).
        assert_eq!(ctx.get("temp").and_then(|v| v.as_i64()), Some(30));
    }

    #[test]
    fn test_build_context_exposes_step_data_accessor_for_flat_user_dict() {
        // noetl/ai-meta#66 — canonical fixtures reference `{{ step.data }}`
        // (or `{{ step.data.field }}`) on the next step's `input`
        // block to feed an upstream step's user dict into a
        // downstream step.  Pre-fix: only flat-field accessors
        // (`{{ step.field }}`) worked; `{{ step.data }}` resolved
        // to None because single-tool python steps don't go through
        // the task_sequence flatten path that synthesizes `.data`.
        let mut state = WorkflowState::new(1, 1);
        let mut info = StepInfo::new("run_from_file");
        // Mirror the live kind execution 322087210360770560 envelope:
        //   result.context.result.context.data = the user's main() return.
        info.result = Some(serde_json::json!({
            "status": "COMPLETED",
            "context": {
                "result": {
                    "status": "success",
                    "context": {
                        "data": {
                            "status": "success",
                            "messages": ["Hello, NoETL! (#1)", "Hello, NoETL! (#2)", "Hello, NoETL! (#3)"],
                            "total_greetings": 3,
                            "script_source": "file"
                        }
                    }
                }
            }
        }));
        state.steps.insert("run_from_file".to_string(), info);

        let ctx = state.build_context();
        let step = ctx
            .get("run_from_file")
            .expect("top-level step entry exposed");

        // Existing flat-field path (back-compat):
        assert_eq!(
            step.get("status").and_then(|v| v.as_str()),
            Some("success"),
            "flat `run_from_file.status` must still resolve"
        );
        assert_eq!(
            step.get("total_greetings").and_then(|v| v.as_i64()),
            Some(3),
            "flat `run_from_file.total_greetings` must still resolve"
        );

        // New `.data` accessor — the #66 fix:
        let data = step
            .get("data")
            .expect("`.data` accessor populated for flat user dict");
        assert_eq!(
            data.get("status").and_then(|v| v.as_str()),
            Some("success"),
            "`run_from_file.data.status` must resolve"
        );
        assert_eq!(
            data.get("total_greetings").and_then(|v| v.as_i64()),
            Some(3),
            "`run_from_file.data.total_greetings` must resolve"
        );
        assert_eq!(
            data.get("messages")
                .and_then(|v| v.as_array())
                .map(|a| a.len()),
            Some(3),
            "`run_from_file.data.messages` must resolve"
        );
    }

    #[test]
    fn test_build_context_data_accessor_does_not_clobber_existing_data_field() {
        // Edge case: the task_sequence flatten path already merges a
        // `.data` key in for labeled sub-task results.  The #66 fix
        // must not overwrite that path's `data` field with the
        // outer user_data.
        let mut state = WorkflowState::new(1, 1);
        let mut info = StepInfo::new("multi_step");
        // task_sequence-shaped envelope: data = {label1: {data: ...}, label2: ...}
        info.result = Some(serde_json::json!({
            "status": "COMPLETED",
            "context": {
                "result": {
                    "status": "success",
                    "context": {
                        "data": {
                            "init_action": {
                                "data": {"executed": true, "value": 42},
                                "status": "success"
                            }
                        }
                    }
                }
            }
        }));
        state.steps.insert("multi_step".to_string(), info);

        let ctx = state.build_context();
        let step = ctx.get("multi_step").expect("step entry exposed");

        // After task_sequence flatten:
        //   - `multi_step.init_action.data.executed` works (labeled path)
        //   - `multi_step.data.executed` works (flattened path; the
        //     flatten merged init_action's `data` field up).
        // The #66 fix must NOT overwrite that flattened `data` field
        // with the outer user_data (`{init_action: ..., data: ..., status: ...}`),
        // which would wrap `.data.data.executed` and break the
        // existing template path.
        let labeled = step
            .get("init_action")
            .and_then(|v| v.get("data"))
            .and_then(|v| v.get("executed"))
            .and_then(|v| v.as_bool());
        assert_eq!(
            labeled,
            Some(true),
            "labeled task_sequence path stays intact"
        );

        let flat = step
            .get("data")
            .and_then(|v| v.get("executed"))
            .and_then(|v| v.as_bool());
        assert_eq!(
            flat,
            Some(true),
            "flattened `multi_step.data.executed` must still resolve (#66 fix preserves task_sequence flatten)"
        );
    }

    #[test]
    fn test_extract_user_data_flattens_task_sequence_wrap() {
        // Real e2e payload from `test_start_with_action`'s call.done
        // event on the Rust kind cluster (Phase F R5).  task_sequence
        // wraps the single Python tool's result under the tool's
        // label (`init_action`), so the unwrapped envelope `data` is
        // `{init_action: {data: {executed: true}, ...}}` rather than
        // the user's assigned dict.  After the flatten:
        //   - `start.init_action.data.executed` still works (back-compat)
        //   - `start.data.executed` ALSO works (the YAML template's expectation)
        let envelope = serde_json::json!({
            "status": "COMPLETED",
            "context": {
                "call_index": 0,
                "command_id": "321180039523602432:start:321180039552962560",
                "result": {
                    "status": "success",
                    "context": {
                        "data": {
                            "init_action": {
                                "data": {
                                    "executed": true,
                                    "input": {"test_value": "hello"}
                                },
                                "message": "Start step executed with action type",
                                "status": "success"
                            }
                        },
                        "duration_ms": 79,
                        "exit_code": 0,
                        "status": "success",
                        "stderr": "",
                        "stdout": ""
                    }
                }
            }
        });
        let unwrapped = extract_user_data(&envelope).expect("envelope unwraps");
        // Flat reference — the failing YAML template path:
        assert_eq!(
            unwrapped
                .get("data")
                .and_then(|v| v.get("executed"))
                .and_then(|v| v.as_bool()),
            Some(true),
            "start.data.executed must resolve after flatten"
        );
        assert_eq!(
            unwrapped.get("status").and_then(|v| v.as_str()),
            Some("success"),
            "start.status must resolve after flatten"
        );
        // Labeled reference — back-compat:
        assert_eq!(
            unwrapped
                .get("init_action")
                .and_then(|v| v.get("data"))
                .and_then(|v| v.get("executed"))
                .and_then(|v| v.as_bool()),
            Some(true),
            "start.init_action.data.executed must still resolve"
        );
    }

    // -----------------------------------------------------------------------
    // apply_set_mutations tests (arc-level `set:` DSL contract)
    // -----------------------------------------------------------------------

    #[test]
    fn test_apply_set_mutations_strips_ctx_prefix() {
        let mut vars: HashMap<String, serde_json::Value> = HashMap::new();
        let mutations = [("ctx.foo".to_string(), serde_json::json!(1))]
            .into_iter()
            .collect();
        apply_set_mutations(&mut vars, &mutations);
        assert_eq!(vars.get("foo"), Some(&serde_json::json!(1)));
        assert!(
            !vars.contains_key("ctx.foo"),
            "scoped key must not be present"
        );
    }

    #[test]
    fn test_apply_set_mutations_strips_iter_prefix() {
        let mut vars: HashMap<String, serde_json::Value> = HashMap::new();
        let mutations = [("iter.bar".to_string(), serde_json::json!(2))]
            .into_iter()
            .collect();
        apply_set_mutations(&mut vars, &mutations);
        assert_eq!(vars.get("bar"), Some(&serde_json::json!(2)));
        assert!(!vars.contains_key("iter.bar"));
    }

    #[test]
    fn test_apply_set_mutations_strips_step_prefix() {
        let mut vars: HashMap<String, serde_json::Value> = HashMap::new();
        let mutations = [("step.baz".to_string(), serde_json::json!(3))]
            .into_iter()
            .collect();
        apply_set_mutations(&mut vars, &mutations);
        assert_eq!(vars.get("baz"), Some(&serde_json::json!(3)));
        assert!(!vars.contains_key("step.baz"));
    }

    #[test]
    fn test_apply_set_mutations_keeps_bare_keys() {
        let mut vars: HashMap<String, serde_json::Value> = HashMap::new();
        let mutations = [("qux".to_string(), serde_json::json!(4))]
            .into_iter()
            .collect();
        apply_set_mutations(&mut vars, &mutations);
        assert_eq!(vars.get("qux"), Some(&serde_json::json!(4)));
    }

    #[test]
    fn test_apply_set_mutations_keeps_unknown_scope_dot_keys() {
        // A dotted key whose scope is not ctx/iter/step is written
        // as the full key (dot is part of the map key, not stripped).
        let mut vars: HashMap<String, serde_json::Value> = HashMap::new();
        let mutations = [("app.config".to_string(), serde_json::json!({"level": 5}))]
            .into_iter()
            .collect();
        apply_set_mutations(&mut vars, &mutations);
        assert_eq!(
            vars.get("app.config"),
            Some(&serde_json::json!({"level": 5}))
        );
        assert!(
            !vars.contains_key("config"),
            "bare key must NOT be present for unknown scope"
        );
    }

    #[test]
    fn test_apply_set_mutations_all_cases_together() {
        // Pin all four cases in one call (mirrors the prompt spec).
        let mut vars: HashMap<String, serde_json::Value> = HashMap::new();
        let mutations: HashMap<String, serde_json::Value> = [
            ("ctx.foo".to_string(), serde_json::json!(1)),
            ("iter.bar".to_string(), serde_json::json!(2)),
            ("step.baz".to_string(), serde_json::json!(3)),
            ("qux".to_string(), serde_json::json!(4)),
            ("app.config".to_string(), serde_json::json!({"level": 5})),
        ]
        .into_iter()
        .collect();
        apply_set_mutations(&mut vars, &mutations);
        assert_eq!(vars.get("foo"), Some(&serde_json::json!(1)));
        assert_eq!(vars.get("bar"), Some(&serde_json::json!(2)));
        assert_eq!(vars.get("baz"), Some(&serde_json::json!(3)));
        assert_eq!(vars.get("qux"), Some(&serde_json::json!(4)));
        assert_eq!(
            vars.get("app.config"),
            Some(&serde_json::json!({"level": 5}))
        );
        // Scoped prefixed forms must not appear as top-level keys.
        assert!(!vars.contains_key("ctx.foo"));
        assert!(!vars.contains_key("iter.bar"));
        assert!(!vars.contains_key("step.baz"));
    }
}
