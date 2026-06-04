//! Execution state reconstruction from events.
//!
//! Provides state reconstruction for event-sourced workflow execution.

use std::collections::HashMap;
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::db::models::Event;

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
            "command.issued" => {
                if let Some(name) = &event.node_name {
                    let step = self
                        .steps
                        .entry(name.clone())
                        .or_insert_with(|| StepInfo::new(name));
                    step.state = StepState::CommandIssued;
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
                            step.result = Some(serde_json::Value::Array(
                                step.iteration_results.clone(),
                            ));
                        }
                        // Mid-iteration: leave step.state at whatever
                        // command.started / command.claimed last set
                        // it to so `is_step_completed` returns false.
                    } else {
                        // Plain (non-iterator) step.
                        step.state = StepState::Completed;
                        step.completed_at = Some(event.created_at);
                        step.result = event.result.clone();
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
                    // Extract error from result or use status
                    if let Some(result) = &event.result {
                        if let Some(error) = result.get("error").and_then(|v| v.as_str()) {
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

        // Add step results under 'steps' namespace
        let mut steps = serde_json::Map::new();
        for (name, info) in &self.steps {
            if let Some(result) = &info.result {
                steps.insert(name.clone(), result.clone());
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
}
