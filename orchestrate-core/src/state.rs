//! Execution state reconstruction from events.
//!
//! Provides state reconstruction for event-sourced workflow execution.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::event::Event;

/// Serde skip predicate for `i32` fields that default to 0.
pub fn is_zero(v: &i32) -> bool {
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

impl ExecutionState {
    /// True once the execution has reached a terminal state — completed,
    /// failed, or cancelled.  Past this point no further orchestration must
    /// be driven (noetl/ai-meta#113 facet 2): a cancel (or any terminal
    /// event) must stop the worker-driven drive from re-issuing
    /// `__orchestrate__`, which otherwise re-loops forever (only a server
    /// restart cleared it before the terminal guard was added).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
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
    /// `event_id` of the lifecycle event that most recently transitioned
    /// this step into `Completed`.
    ///
    /// noetl/ai-meta#85: used as the **stable** per-completion key for
    /// durable `set:` persistence.  `completed_at` derives from
    /// `event.timestamp`, which the event loader fills with
    /// `Utc::now()` when the row's `created_at` is unreadable — so it is
    /// NOT stable across reconstructions and can't gate once-per-
    /// completion emission.  `event_id` is the row's snowflake primary
    /// key: stable across reconstructions and monotonic, so it both
    /// dedups re-emission and gives a deterministic completion order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_event_id: Option<i64>,
    pub attempt: i32,

    // -------- Async callback parking (noetl/ai-meta#186) --------
    //
    // A tool that dispatches long-running external work (today only
    // `kind: container`, which creates a K8s Job) returns
    // `ToolResult.pending_callback = Some(true)` and the worker deliberately
    // skips its own `call.done`, freeing the slot per the execution model's
    // callback rule.  The terminal arrives later via
    // `POST /api/internal/container-callback/…`, which emits the `call.done`.
    //
    // That INVERTS the usual order.  Normally a step goes
    // `command.issued → call.done → command.completed`, and
    // `command.completed` is what flips it to `Completed`.  On this path
    // `command.completed` arrives FIRST — carrying only the Job handle — and
    // the real result lands on the LATER `call.done`.
    //
    // Completing the step on that first `command.completed` is what let the DAG
    // run ahead of the container: dependent steps dispatched while the Job was
    // still starting, and read a database the previous step had not finished
    // creating.
    /// The step is parked on an async callback: a `command.completed` carrying
    /// `pending_callback` has arrived, but the terminal `call.done` has not.
    /// Never flips the step to `Completed`, so no transition can advance past it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending_callback: bool,
    /// Sticky: this step has parked on a callback at least once.
    ///
    /// Kept AFTER the resume clears `pending_callback`, because the
    /// orchestrator needs to know that `call.done` is a meaningful
    /// advancement trigger for this execution.  Without it the resume would
    /// complete the step in state and nothing would ever re-evaluate the DAG —
    /// the step would sit `Completed` forever with no successor dispatched,
    /// trading a premature advance for a permanent stall.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub uses_callback: bool,

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
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub iteration_command_ids: std::collections::BTreeSet<String>,

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
    #[serde(default, skip_serializing_if = "crate::state::is_zero")]
    pub iterations_dispatched: i32,

    /// True when this step is a `mode: cursor` loop (noetl/ai-meta#100).  Set
    /// from the `step.enter` context marker `__cursor_loop`.  Cursor steps are
    /// NOT completed by individual claim/body `command.completed` events — the
    /// orchestrator's cursor-drive block completes the step via a drain
    /// `step.exit` carrying `__cursor_drained` once the claim returns no rows.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_cursor: bool,

    // -------- Cursor-loop frame tracking (noetl/ai-meta#100) --------
    //
    // Maintained INCREMENTALLY by `apply_event` so the orchestrator never
    // re-scans the whole event log to rebuild cursor progress (the per-trigger
    // O(n) scan was the scaling bottleneck).  Reset on each `step.enter`
    // (loop-back re-entry starts fresh).
    /// Issued cursor sub-commands: command_id -> (phase "claim"|"body", frame).
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub cursor_issued: std::collections::HashMap<String, (String, i64)>,
    /// Completed cursor sub-command command_ids (dedup repeated completions).
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub cursor_completed: std::collections::BTreeSet<String>,
    /// Per-frame progress, keyed by frame index.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub cursor_frames: std::collections::BTreeMap<i64, CursorFrame>,
}

/// Per-frame progress of a `mode: cursor` loop (noetl/ai-meta#100).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CursorFrame {
    pub claim_completed: bool,
    pub claim_rows: Vec<serde_json::Value>,
    pub body_issued: usize,
    pub body_completed: usize,
    /// References-in-state (noetl/ai-meta#101 phase 2): when the claim result is
    /// over budget the orchestrator keeps the reference instead of the inline
    /// rows, so `claim_rows` can't be filled in the sync `apply_event` pass.
    /// The `noetl://` URI is stashed here; `trigger_orchestrator` resolves it
    /// (async) into `claim_rows` before the cursor drive runs.  `None` on the
    /// common inline path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_ref: Option<String>,
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
            completed_event_id: None,
            attempt: 0,
            pending_callback: false,
            uses_callback: false,
            iterations_expected: None,
            iteration_command_ids: std::collections::BTreeSet::new(),
            iteration_results: Vec::new(),
            iterations_dispatched: 0,
            is_cursor: false,
            cursor_issued: std::collections::HashMap::new(),
            cursor_completed: std::collections::BTreeSet::new(),
            cursor_frames: std::collections::BTreeMap::new(),
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
    /// Durable workflow context — the fold over `ctx.updated` events.
    ///
    /// noetl/ai-meta#85: workflow-arc loops carry their loop variable
    /// (offset / cursor / counter) through step-level `set: ctx.*`
    /// mutations.  Those mutations are ephemeral per orchestrator pass
    /// — recomputed from the workload default + step results each time
    /// — so on the next iteration's pass the variable reverts to its
    /// workload default and the loop thrashes (`0,0,1,0,1,2,…` instead
    /// of advancing).  The fix persists each completing step's rendered
    /// `set:` values into the event log as a `ctx.updated` event; this
    /// map is the latest-wins fold over those events, keyed by the bare
    /// (scope-stripped) variable name.  `build_context` overlays it on
    /// top of the workload default so a loop variable survives across
    /// re-dispatch.  Bare keys match the post-`apply_set_mutations`
    /// shape (`ctx.offset` → `offset`).
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub ctx: HashMap<String, serde_json::Value>,
    /// Per-producing-step completion `event_id` whose `set:` mutations
    /// have already been persisted as a `ctx.updated` event.
    ///
    /// noetl/ai-meta#85: a step's `set:` fires exactly once per
    /// completion.  Re-emitting it on every subsequent pass (the old
    /// "apply all completed steps' set each pass" loop) is the thrash
    /// source — e.g. `start`'s `set: ctx.offset: {{ workload.offset }}`
    /// (= 0) competed non-deterministically with `check_pagination`'s
    /// advancing `set: ctx.offset` in random HashMap order.  The
    /// orchestrator persists a step's `set:` only when this map's entry
    /// for the step differs from the step's current `completed_event_id`,
    /// making emission idempotent per completion.  Keyed by the stable
    /// completion `event_id` (see [`StepInfo::completed_event_id`]) —
    /// `completed_at` can't be used because it derives from the
    /// `Utc::now()` loader fallback and varies across reconstructions.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub ctx_set_marks: HashMap<String, i64>,
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
/// Does this `command.completed` say the step is parked on an async callback?
///
/// The worker stamps `pending_callback: true` into the event context when a
/// tool returned `ToolResult.pending_callback` (noetl/ai-meta#186 / #227 part
/// B).  Absent on every other path, and absence means "not parked" — never
/// "parked", so an old event or a tool that does not set it behaves exactly as
/// before.
pub fn is_parked_on_callback(event: &Event) -> bool {
    fn marker(v: Option<&serde_json::Value>) -> Option<bool> {
        v.and_then(|c| c.get("pending_callback"))
            .and_then(|v| v.as_bool())
    }

    // The worker stamps the marker into the TOOL RESULT's context, so on the
    // wire it arrives at `result.context.pending_callback` and the event's own
    // `context` is null.  Reading only `event.context` — which is what this did
    // — returned false for every real event, so no step ever parked and the DAG
    // advanced on the dispatch (noetl/ai-meta#186 Bug 1, caught by kind
    // validation after the unit tests passed against a hand-built event).
    //
    // `event.context` is still checked so a caller that inlines the marker
    // there keeps working.
    // Three locations, matching the sweep's SQL exactly
    // (`handlers/nonconvergence_sweep.rs`, which checks
    // `result->'context'`, `context` and `meta`).  Both predicates answer the
    // same question — "is this execution parked by design?" — and they must not
    // disagree: the sweep skipping an execution the orchestrator advanced past,
    // or vice versa, is worse than either being wrong alone.
    marker(event.result.as_ref().and_then(|r| r.get("context")))
        .or_else(|| marker(event.context.as_ref()))
        .or_else(|| marker(event.meta.as_ref()))
        .unwrap_or(false)
}

pub fn extract_command_id(event: &Event) -> Option<String> {
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
            ctx: HashMap::new(),
            ctx_set_marks: HashMap::new(),
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
        #[cfg(not(target_arch = "wasm32"))]
        let start = std::time::Instant::now();

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

        // Perf logging — native only.  `Instant` isn't portable to
        // wasm32-unknown-unknown (no clock); in the plug-in the host times the
        // invoke instead, so this self-timing is compiled out (noetl/ai-meta#109).
        #[cfg(not(target_arch = "wasm32"))]
        {
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
        }

        Some(state)
    }

    /// Apply a single event to update the workflow state.
    /// The reserved step name of the worker-driven orchestrate "meta" command
    /// (noetl/ai-meta#108). The server issues `system/orchestrate` as a command
    /// under this step so the worker pool runs the drive; its lifecycle events
    /// are infrastructure, NOT workflow steps, so they are ignored here (see
    /// `apply_event`) — otherwise `steps.entry(name).or_insert_with(..)` below
    /// would create a phantom step and corrupt the drive state.
    pub const ORCHESTRATE_META_STEP: &'static str = "__orchestrate__";

    pub fn apply_event(&mut self, event: &Event) {
        // Ignore the worker-driven orchestrate meta-command's own events: they
        // drive the execution but are not workflow steps. Without this, a
        // `command.issued`/`command.completed` for `__orchestrate__` would
        // phantom-create a step in `self.steps` (noetl/ai-meta#108).
        if event.node_name.as_deref() == Some(Self::ORCHESTRATE_META_STEP) {
            return;
        }
        match event.event_type.as_str() {
            "playbook_started" => {
                self.state = ExecutionState::InProgress;
                self.started_at = Some(event.timestamp);
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
                self.completed_at = Some(event.timestamp);
            }
            "playbook_failed" | "playbook.failed" => {
                self.state = ExecutionState::Failed;
                self.completed_at = Some(event.timestamp);
            }
            "playbook.cancelled" | "playbook_cancelled" => {
                // The ExecutionService cancel chokepoint emits the underscore
                // form `playbook_cancelled` (services/execution.rs); without
                // matching it here the cached WorkflowState never transitioned
                // to Cancelled and the drive kept re-issuing `__orchestrate__`
                // (noetl/ai-meta#113 facet 2).
                self.state = ExecutionState::Cancelled;
                self.completed_at = Some(event.timestamp);
            }
            "ctx.updated" => {
                // noetl/ai-meta#85: durable loop-variable propagation.
                // The orchestrator persists a completing step's rendered
                // `set: ctx.*` mutations here so they survive across the
                // re-dispatch that workflow-arc loops require.  Payload
                // shape (post `{status, context}` envelope wrapping by
                // `trigger_orchestrator`):
                //   result.context = {
                //     "step": "<producing step>",
                //     "gen": <completion event_id>,
                //     "values": { "<bare key>": <json>, ... }
                //   }
                // Older / direct callers may populate the event row's
                // `context` column instead; accept both shapes, matching
                // the dual-shape read the `step.enter` arm uses for
                // `iterations_expected`.
                let payload = event
                    .result
                    .as_ref()
                    .and_then(|r| r.get("context"))
                    .or(event.context.as_ref());
                if let Some(payload) = payload {
                    if let Some(serde_json::Value::Object(values)) = payload.get("values") {
                        // Latest-wins fold: a later iteration's value
                        // overwrites the earlier one.  `start`'s
                        // initializer (offset = 0) is shadowed by the
                        // loop's advancing values because its event is
                        // earlier in the log.
                        for (key, val) in values {
                            self.ctx.insert(key.clone(), val.clone());
                        }
                    }
                    // Record which completion this persisted, so the
                    // orchestrator emits a step's `set:` only once per
                    // completion (idempotent emission).
                    if let Some(step) = payload.get("step").and_then(|v| v.as_str()) {
                        if let Some(gen) = payload.get("gen").and_then(|v| v.as_i64()) {
                            self.ctx_set_marks.insert(step.to_string(), gen);
                        }
                    }
                }
            }
            "step.enter" | "step_enter" | "step_started" => {
                if let Some(name) = &event.node_name {
                    let step = self
                        .steps
                        .entry(name.clone())
                        .or_insert_with(|| StepInfo::new(name));
                    step.state = StepState::Entered;
                    step.entered_at = Some(event.timestamp);
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
                    // noetl/ai-meta#100: mark cursor-loop steps from the
                    // `__cursor_loop` context marker (same dual-shape read).
                    let cursor_marked = event
                        .result
                        .as_ref()
                        .and_then(|r| r.get("context"))
                        .and_then(|c| c.get("__cursor_loop"))
                        .and_then(|v| v.as_bool())
                        .or_else(|| {
                            event
                                .context
                                .as_ref()
                                .and_then(|c| c.get("__cursor_loop"))
                                .and_then(|v| v.as_bool())
                        })
                        .unwrap_or(false);
                    if cursor_marked {
                        step.is_cursor = true;
                        // Loop-back re-entry: a fresh step.enter resets frame
                        // tracking so the re-run's frame 0 doesn't merge with a
                        // prior drained run's frames.
                        step.cursor_issued.clear();
                        step.cursor_completed.clear();
                        step.cursor_frames.clear();
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
                    step.entered_at = Some(event.timestamp);
                    step.completed_at = Some(event.timestamp);
                }
            }
            "command.issued" => {
                if let Some(name) = &event.node_name {
                    // noetl/ai-meta#100: incrementally track cursor sub-command
                    // dispatch (phase/frame) from the command.issued meta so the
                    // orchestrator never rescans the log to rebuild frame state.
                    let cursor_meta = event.meta.as_ref().and_then(|m| m.get("cursor")).map(|c| {
                        (
                            c.get("phase")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            c.get("frame").and_then(|v| v.as_i64()).unwrap_or(0),
                        )
                    });
                    let cid = extract_command_id(event);
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
                    if let (Some((phase, frame)), Some(cid)) = (cursor_meta, cid) {
                        if step
                            .cursor_issued
                            .insert(cid, (phase.clone(), frame))
                            .is_none()
                        {
                            let f = step.cursor_frames.entry(frame).or_default();
                            if phase == "body" {
                                f.body_issued += 1;
                            }
                        }
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
                    // noetl/ai-meta#100: a `mode: cursor` loop's claim/body
                    // sub-commands must NOT complete the step.  The step is
                    // completed only by the orchestrator's drain `step.exit`
                    // (carrying `__cursor_drained` in its context).  So for a
                    // cursor step, skip completion unless the event is the drain.
                    let is_drain = event
                        .result
                        .as_ref()
                        .and_then(|r| r.get("context"))
                        .and_then(|c| c.get("__cursor_drained"))
                        .and_then(|v| v.as_bool())
                        .or_else(|| {
                            event
                                .context
                                .as_ref()
                                .and_then(|c| c.get("__cursor_drained"))
                                .and_then(|v| v.as_bool())
                        })
                        .unwrap_or(false);
                    // Correlate a cursor sub-command completion to its frame
                    // (claim done / one more body row done) — incremental.
                    let cid = extract_command_id(event);
                    let step = self
                        .steps
                        .entry(name.clone())
                        .or_insert_with(|| StepInfo::new(name));

                    if step.is_cursor && !is_drain {
                        if let Some(cid) = cid {
                            if let Some((phase, frame)) = step.cursor_issued.get(&cid).cloned() {
                                if step.cursor_completed.insert(cid) {
                                    let f = step.cursor_frames.entry(frame).or_default();
                                    if phase == "claim" {
                                        f.claim_completed = true;
                                    } else if phase == "body" {
                                        f.body_completed += 1;
                                    }
                                }
                            }
                        }
                        // Cursor sub-command completion — record nothing toward
                        // step completion; the drive block re-claims / drains.
                        return;
                    }

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
                            step.completed_at = Some(event.timestamp);
                            step.completed_event_id = Some(event.event_id);
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
                    } else if is_parked_on_callback(event) {
                        // noetl/ai-meta#186 — the step is NOT finished.
                        //
                        // The tool dispatched external work and returned
                        // immediately; this `command.completed` carries only the
                        // handle.  Completing here is what let the DAG run ahead
                        // of a container Job: dependents dispatched seconds
                        // later and read a schema the previous step had not
                        // finished creating.
                        //
                        // Leave `state` where command.started put it, so
                        // `is_step_completed` stays false and no transition can
                        // advance past this step.  The terminal `call.done` from
                        // the callback (below) is what completes it.
                        step.pending_callback = true;
                        step.uses_callback = true;
                    } else {
                        // Plain (non-iterator) step.
                        step.state = StepState::Completed;
                        step.completed_at = Some(event.timestamp);
                        step.completed_event_id = Some(event.event_id);
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
                    // The claim's RETURNING rows arrive on call.done (not
                    // command.completed).  Capture them into the frame.
                    let cid = extract_command_id(event);
                    let rows = event
                        .result
                        .as_ref()
                        .and_then(extract_user_data)
                        .as_ref()
                        .and_then(|d| d.get("rows"))
                        .and_then(|r| r.as_array())
                        .cloned();
                    // References-in-state (noetl/ai-meta#101 phase 2): when the
                    // claim is over budget the orchestrator kept the reference,
                    // so there are no inline `rows` here.  Capture the URI so the
                    // async pre-drive pass can resolve it into `claim_rows` —
                    // otherwise the cursor would see 0 rows and wrongly DRAIN.
                    let claim_ref = if rows.is_none() {
                        event.result.as_ref().and_then(result_reference_uri)
                    } else {
                        None
                    };
                    let step = self
                        .steps
                        .entry(name.clone())
                        .or_insert_with(|| StepInfo::new(name));
                    // noetl/ai-meta#100: cursor sub-command call.done results
                    // must not overwrite the cursor step's result (the drive
                    // block sets it on drain).
                    if step.is_cursor {
                        if let Some(cid) = cid {
                            if let Some((phase, frame)) = step.cursor_issued.get(&cid).cloned() {
                                if phase == "claim" {
                                    let f = step.cursor_frames.entry(frame).or_default();
                                    if let Some(rows) = rows {
                                        f.claim_rows = rows;
                                    } else if let Some(cref) = claim_ref {
                                        f.claim_ref = Some(cref);
                                    }
                                }
                            }
                        }
                        return;
                    }
                    // For iterator steps the iteration-aware branch
                    // in command.completed builds the per-iteration
                    // result array; leave it alone here.  Plain steps
                    // get their data attached.
                    if step.iterations_expected.is_none() {
                        if let Some(result) = event.result.clone() {
                            step.result = Some(result);
                        }
                    }

                    // noetl/ai-meta#186 — the resume.
                    //
                    // For a parked step this `call.done` is not the mid-flight
                    // data-attachment event it is on every other path: it is the
                    // TERMINAL, arriving from
                    // `POST /api/internal/container-callback/…` after the K8s
                    // Job reached a terminal state.  Its `command.completed`
                    // already came and went without completing the step, so
                    // nothing else will ever complete it.
                    //
                    // Completing here is what makes the park safe.  Without it
                    // the step would sit parked for ever and the fix would trade
                    // a premature advance for a permanent stall — a worse bug,
                    // and a silent one.
                    if step.pending_callback {
                        step.pending_callback = false;
                        step.state = StepState::Completed;
                        step.completed_at = Some(event.timestamp);
                        step.completed_event_id = Some(event.event_id);
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
                    step.completed_at = Some(event.timestamp);
                    // noetl/ai-meta#186 — a parked step whose external work
                    // FAILED must unpark too.  Leaving `pending_callback` set on
                    // a failure would keep the step out of every terminal path
                    // and the execution would never finish.
                    step.pending_callback = false;
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

        // noetl/ai-meta#85: overlay the durable workflow context last,
        // so persisted `set: ctx.*` loop variables win over both the
        // workload default and step-result top-level keys.  This is the
        // same precedence the legacy per-pass `set:` application had
        // (it ran after `build_context` and overwrote the context), now
        // sourced from the event log instead of recomputed each pass —
        // which is what lets a loop variable advance monotonically
        // across re-dispatch instead of reverting to its workload
        // default.  Keys are bare (scope already stripped at emit time).
        for (key, value) in &self.ctx {
            context.insert(key.clone(), value.clone());
        }

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
/// Locate a `noetl://` result-reference URI on an event result envelope
/// (references-in-state, noetl/ai-meta#101 phase 2).  Nested envelope:
/// `context.result.reference.ref`; top-level: `reference.ref`.
pub fn result_reference_uri(result: &serde_json::Value) -> Option<String> {
    result
        .pointer("/context/result/reference/ref")
        .and_then(|v| v.as_str())
        .or_else(|| result.pointer("/reference/ref").and_then(|v| v.as_str()))
        .map(str::to_string)
}

pub fn extract_user_data(result: &serde_json::Value) -> Option<serde_json::Value> {
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
            event_id: 1,
            execution_id: 12345,
            catalog_id: 67890,
            event_type: event_type.to_string(),
            node_name: node_name.map(|s| s.to_string()),
            status: "".to_string(),
            context: None,
            result: None,
            meta: None,
            // Fixed epoch — the core has no clock (deterministic fixtures).
            timestamp: DateTime::from_timestamp(0, 0).unwrap(),
            parent_execution_id: None,
            attempt: None,
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
    fn test_execution_state_is_terminal() {
        // noetl/ai-meta#113 facet 2 — the drive guard keys off this.
        assert!(!ExecutionState::Initial.is_terminal());
        assert!(!ExecutionState::InProgress.is_terminal());
        assert!(ExecutionState::Completed.is_terminal());
        assert!(ExecutionState::Failed.is_terminal());
        assert!(ExecutionState::Cancelled.is_terminal());
    }

    #[test]
    fn test_apply_event_cancels_on_both_event_spellings() {
        // The cancel chokepoint emits the underscore form `playbook_cancelled`
        // (services/execution.rs); the dotted `playbook.cancelled` also exists
        // on legacy paths.  Both must drive the state to Cancelled / terminal so
        // the off-server drive stops re-issuing `__orchestrate__`
        // (noetl/ai-meta#113 facet 2).
        for spelling in ["playbook_cancelled", "playbook.cancelled"] {
            let mut state = WorkflowState::new(1, 2);
            state.apply_event(&make_event("playbook_started", None));
            assert_eq!(state.state, ExecutionState::InProgress);
            state.apply_event(&make_event(spelling, None));
            assert_eq!(
                state.state,
                ExecutionState::Cancelled,
                "{spelling} should transition to Cancelled"
            );
            assert!(
                state.state.is_terminal(),
                "{spelling} state must be terminal"
            );
        }
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
    fn test_auth_login_start_error_survives_apply_event_into_arc_context() {
        // Repro of the prod auth0_login stall (noetl/ai-meta#49): the
        // `start` step errors (empty token → {"error": "Invalid JWT format"}),
        // emitted on `call.done`; `command.completed` carries only
        // {status, command_id} (no data).  next.arcs gate on
        // `{{ start.error is defined }}` — which must resolve true so the
        // error-callback arc fires instead of all arcs skipping.
        let mut state = WorkflowState::new(1, 1);

        // call.done — rich envelope carrying the user error data.
        let mut call_done = make_event("call.done", Some("start"));
        call_done.event_id = 1;
        call_done.result = Some(serde_json::json!({
            "status": "COMPLETED",
            "context": {
                "result": {
                    "status": "success",
                    "context": {
                        "data": { "error": "Invalid JWT format" },
                        "status": "success",
                        "stderr": "",
                        "stdout": ""
                    }
                }
            }
        }));
        // command.completed — no user data (just status + command_id).
        let mut cmd_completed = make_event("command.completed", Some("start"));
        cmd_completed.event_id = 2;
        cmd_completed.result = Some(serde_json::json!({
            "status": "success",
            "context": { "status": "success", "command_id": "x:start:y" }
        }));

        state.apply_event(&call_done);
        state.apply_event(&cmd_completed);

        let ctx = state.build_context();
        let start = ctx.get("start").expect("`start` step entry exposed");
        assert!(
            start.get("error").and_then(|v| v.as_str()) == Some("Invalid JWT format"),
            "start.error must resolve for arc routing; got start = {start:?}"
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
    fn orchestrate_meta_command_events_do_not_pollute_state() {
        // The worker-driven orchestrate meta-command (noetl/ai-meta#108) issues
        // `command.issued`/`command.completed` under the reserved `__orchestrate__`
        // step. Those must NOT create a phantom step — otherwise the drive would
        // see a step that isn't in the playbook.
        let mut ws = WorkflowState::new(12345, 67890);
        ws.apply_event(&make_event(
            "command.issued",
            Some(WorkflowState::ORCHESTRATE_META_STEP),
        ));
        ws.apply_event(&make_event(
            "command.completed",
            Some(WorkflowState::ORCHESTRATE_META_STEP),
        ));
        assert!(
            !ws.steps.contains_key(WorkflowState::ORCHESTRATE_META_STEP),
            "the meta-command must not create a workflow step"
        );
        assert!(
            ws.steps.is_empty(),
            "no steps should exist, got {:?}",
            ws.steps.keys().collect::<Vec<_>>()
        );

        // A real step's command.issued still creates its step (guard is scoped).
        ws.apply_event(&make_event("command.issued", Some("real_step")));
        assert!(ws.steps.contains_key("real_step"));
    }

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

    #[test]
    fn test_command_issued_after_completion_reactivates_step() {
        // noetl/ai-meta#85: workflow-arc loop re-entry re-dispatches an
        // already-`Completed` loop step via a fresh `command.issued`.
        // State reconstruction must let that later lifecycle event win,
        // moving the step out of the terminal `Completed` state so the
        // dispatch guards see it as running again.  The prior result is
        // retained (carries the loop variable forward) until the new
        // iteration's `call.done` overwrites it.
        let mut state = WorkflowState::new(1, 1);

        let mut completed = make_event("command.completed", Some("fetch_page"));
        completed.result = Some(serde_json::json!({"next_offset": 10}));
        state.apply_event(&completed);
        assert!(state.is_step_done("fetch_page"));
        assert_eq!(
            state.get_step_result("fetch_page"),
            Some(&serde_json::json!({"next_offset": 10})),
        );

        // Re-dispatch: step.enter then command.issued for the next
        // iteration.
        state.apply_event(&make_event("step.enter", Some("fetch_page")));
        state.apply_event(&make_event("command.issued", Some("fetch_page")));

        let after = state.steps.get("fetch_page").unwrap();
        assert_eq!(after.state, StepState::CommandIssued);
        assert!(!state.is_step_done("fetch_page"));
        assert!(!state.is_step_completed("fetch_page"));
        // Prior result is still visible to context assembly — the loop
        // variable survives the re-entry.
        assert_eq!(
            state.get_step_result("fetch_page"),
            Some(&serde_json::json!({"next_offset": 10})),
        );
    }

    /// Build a `ctx.updated` event in the persisted `{status, context}`
    /// envelope shape (noetl/ai-meta#85).  `gen` is the producing step's
    /// completion event_id.
    fn make_ctx_updated(step: &str, gen: i64, values: serde_json::Value) -> Event {
        let mut e = make_event("ctx.updated", None);
        e.result = Some(serde_json::json!({
            "status": "CONTEXT",
            "context": {
                "step": step,
                "gen": gen,
                "values": values,
            },
        }));
        e
    }

    #[test]
    fn test_ctx_updated_event_folds_latest_wins_and_records_mark() {
        // noetl/ai-meta#85: the durable ctx is the latest-wins fold over
        // ctx.updated events; the per-step mark records the completion
        // event_id that was persisted so the orchestrator emits once per
        // completion.
        let mut state = WorkflowState::new(1, 1);

        // start initializes offset = 0; later iterations advance it.
        state.apply_event(&make_ctx_updated(
            "start",
            100,
            serde_json::json!({ "offset": 0, "limit": 10 }),
        ));
        state.apply_event(&make_ctx_updated(
            "check_pagination",
            200,
            serde_json::json!({ "offset": 10 }),
        ));
        state.apply_event(&make_ctx_updated(
            "check_pagination",
            300,
            serde_json::json!({ "offset": 20 }),
        ));

        // offset = 20 (latest wins over start's 0 and the first check),
        // limit = 10 survives (only start set it, never overwritten).
        assert_eq!(state.ctx.get("offset"), Some(&serde_json::json!(20)));
        assert_eq!(state.ctx.get("limit"), Some(&serde_json::json!(10)));

        // Marks track the latest persisted completion event_id per step.
        assert_eq!(state.ctx_set_marks.get("start"), Some(&100));
        assert_eq!(state.ctx_set_marks.get("check_pagination"), Some(&300));
    }

    #[test]
    fn test_build_context_overlays_durable_ctx_over_workload_default() {
        // The durable loop variable must win over the workload default
        // in build_context — that's what stops the loop reverting to its
        // workload seed on each pass.
        let mut state = WorkflowState::new(1, 1);
        state.workload = Some(serde_json::json!({ "offset": 0, "limit": 10 }));
        state
            .ctx
            .insert("offset".to_string(), serde_json::json!(30));

        let ctx = state.build_context();
        assert_eq!(ctx.get("offset"), Some(&serde_json::json!(30)));
        // Untouched workload keys remain.
        assert_eq!(ctx.get("limit"), Some(&serde_json::json!(10)));
        // The workload namespace still reflects the original seed.
        assert_eq!(
            ctx.get("workload").and_then(|w| w.get("offset")),
            Some(&serde_json::json!(0)),
        );
    }
}

#[cfg(test)]
mod pending_callback_tests {
    use super::*;
    use chrono::DateTime;

    fn ev(id: i64, ty: &str, step: &str, ctx: Option<serde_json::Value>) -> Event {
        Event {
            event_id: id,
            execution_id: 1,
            catalog_id: 1,
            event_type: ty.to_string(),
            node_name: Some(step.to_string()),
            status: "success".to_string(),
            context: ctx,
            result: None,
            meta: None,
            timestamp: DateTime::from_timestamp(id, 0).expect("fixed epoch"),
            parent_execution_id: None,
            attempt: None,
        }
    }

    fn parked() -> serde_json::Value {
        serde_json::json!({"command_id": "e:s:1", "status": "success", "pending_callback": true})
    }

    /// noetl/ai-meta#186 Bug 1. The observed prod/kind timeline was:
    /// `run_schema_creation` dispatched the Job, `command.completed` arrived
    /// 1.2 s later, the DAG advanced, and `verify_data` failed with
    /// `42P01 relation ... does not exist` ~15 s BEFORE the Jobs succeeded.
    ///
    /// A `command.completed` carrying `pending_callback` must NOT complete the
    /// step, or nothing can stop a dependent step from dispatching.
    /// The shape the WORKER actually emits: the marker is nested in the tool
    /// result's context and the event's own `context` is null.
    ///
    /// Every other test in this module hand-builds `context: Some(...)` with
    /// `result: None`, which is the inverse of the wire format — so they passed
    /// while the DAG advanced in production.  Kind validation on released images
    /// caught it: three container Jobs created inside the same second while the
    /// first took ~4s to finish.
    fn ev_wire(id: i64, ty: &str, step: &str) -> Event {
        Event {
            event_id: id,
            execution_id: 1,
            catalog_id: 1,
            event_type: ty.to_string(),
            node_name: Some(step.to_string()),
            status: "success".to_string(),
            context: None,
            result: Some(serde_json::json!({
                "context": {
                    "command_id": "e:s:1",
                    "pending_callback": true,
                    "status": "success"
                },
                "status": "success"
            })),
            meta: None,
            timestamp: DateTime::from_timestamp(id, 0).expect("fixed epoch"),
            parent_execution_id: None,
            attempt: None,
        }
    }

    #[test]
    fn the_marker_is_read_from_meta_like_the_sweep_sql() {
        // The sweep's SQL accepts the marker in meta; this predicate must agree,
        // or the sweep skips an execution the orchestrator already advanced past.
        let mut e = ev(2, "command.completed", "run_schema", None);
        e.meta = Some(serde_json::json!({"pending_callback": true}));
        assert!(
            is_parked_on_callback(&e),
            "meta must be accepted, as the SQL does"
        );
    }

    #[test]
    fn the_marker_is_read_from_the_wire_shape_result_context() {
        let e = ev_wire(2, "command.completed", "run_schema");
        assert!(
            is_parked_on_callback(&e),
            "the worker stamps pending_callback into result.context; reading only \
             event.context misses every real event"
        );
    }

    #[test]
    fn a_wire_shaped_parked_completion_does_not_complete_the_step() {
        let mut st = WorkflowState::new(1, 1);
        st.apply_event(&ev(1, "command.started", "run_schema", None));
        st.apply_event(&ev_wire(2, "command.completed", "run_schema"));
        let step = st.steps.get("run_schema").expect("step present");
        assert!(
            step.pending_callback,
            "the step must park on the wire shape"
        );
        assert!(step.uses_callback, "and be marked a callback user");
        assert!(
            step.completed_at.is_none(),
            "a parked step must NOT be complete — this is what let the DAG advance"
        );
    }

    #[test]
    fn a_parked_command_completed_does_not_complete_the_step() {
        let mut st = WorkflowState::new(1, 1);
        st.apply_event(&ev(1, "command.started", "run_schema", None));
        st.apply_event(&ev(2, "command.completed", "run_schema", Some(parked())));
        let step = st.steps.get("run_schema").expect("step recorded");
        assert!(step.pending_callback, "the step must be parked");
        assert!(step.uses_callback, "and marked as a callback user");
        assert_ne!(
            step.state,
            StepState::Completed,
            "a parked step must not be Completed — this is the premature DAG advance in #186"
        );
    }

    /// The other half. Parking without a resume would trade a premature
    /// advance for a permanent stall, which is worse because it is silent.
    #[test]
    fn the_callback_call_done_completes_the_parked_step() {
        let mut st = WorkflowState::new(1, 1);
        st.apply_event(&ev(1, "command.started", "run_schema", None));
        st.apply_event(&ev(2, "command.completed", "run_schema", Some(parked())));
        st.apply_event(&ev(3, "call.done", "run_schema", None));
        let step = st.steps.get("run_schema").unwrap();
        assert!(!step.pending_callback, "the resume must unpark");
        assert_eq!(step.state, StepState::Completed, "and complete the step");
        assert_eq!(step.completed_event_id, Some(3), "completed BY the resume");
        assert!(
            step.uses_callback,
            "sticky, so call.done stays a valid trigger"
        );
    }

    /// A failed Job must unpark too, or the execution can never terminate.
    #[test]
    fn a_failed_parked_step_unparks() {
        let mut st = WorkflowState::new(1, 1);
        st.apply_event(&ev(1, "command.started", "run_schema", None));
        st.apply_event(&ev(2, "command.completed", "run_schema", Some(parked())));
        st.apply_event(&ev(3, "command.failed", "run_schema", None));
        let step = st.steps.get("run_schema").unwrap();
        assert!(
            !step.pending_callback,
            "a failure must not leave the step parked for ever"
        );
        assert_eq!(step.state, StepState::Failed);
    }

    /// The negative control, and the reason this is safe to ship on by default:
    /// a step that never sets the marker must behave EXACTLY as before —
    /// completed by `command.completed`, never parked, never sticky.
    #[test]
    fn an_ordinary_step_is_untouched() {
        let mut st = WorkflowState::new(1, 1);
        st.apply_event(&ev(1, "command.started", "plain", None));
        st.apply_event(&ev(2, "call.done", "plain", None));
        st.apply_event(&ev(
            3,
            "command.completed",
            "plain",
            Some(serde_json::json!({"command_id": "e:s:1", "status": "success"})),
        ));
        let step = st.steps.get("plain").unwrap();
        assert!(!step.pending_callback);
        assert!(
            !step.uses_callback,
            "no callback machinery for an ordinary step"
        );
        assert_eq!(step.state, StepState::Completed);
        assert_eq!(
            step.completed_event_id,
            Some(3),
            "still completed by command.completed, not by the earlier call.done"
        );
    }

    /// Absence means "not parked", never "parked" — so an event log written by
    /// an older worker replays with the pre-#186 behaviour.
    #[test]
    fn a_missing_marker_is_not_parked() {
        assert!(!is_parked_on_callback(&ev(
            1,
            "command.completed",
            "s",
            None
        )));
        assert!(!is_parked_on_callback(&ev(
            1,
            "command.completed",
            "s",
            Some(serde_json::json!({"status": "success"}))
        )));
        assert!(is_parked_on_callback(&ev(
            1,
            "command.completed",
            "s",
            Some(parked())
        )));
    }
}

/// Canonical digest of a [`WorkflowState`] — the identity an event-sourced
/// read model can be checked against (ai-meta#265 Phase 0).
///
/// # The property, and how easily it is lost
///
/// `WorkflowState` holds `ctx`, `ctx_set_marks` and `steps` in
/// [`std::collections::HashMap`], and `serde_json` serialises a `HashMap` in
/// **iteration order**, which `RandomState` seeds **per process**. So
/// `sha256(serde_json::to_vec(state))` — digesting the struct directly — is a
/// different value in every process for the same logical state.
///
/// Measured, four processes, one identical state
/// (`examples/fold_digest_probe.rs`):
///
/// ```text
/// raw_digest=79722d00…   ctx order: key_03 key_05 key_07 …
/// raw_digest=a0359074…   ctx order: key_08 key_12 key_02 …
/// raw_digest=eeffabd1…   ctx order: key_05 key_10 key_14 …
/// raw_digest=33bad35c…
/// canonical_digest=1f0766f9…  (identical in all four)
/// ```
///
/// # What the audit found
///
/// **Today's digests are already canonical — by accident, not by contract.**
/// `services::orch_snapshot::save` happens to call `serde_json::to_value(state)`
/// first and digest *that*, and `serde_json::Value`'s object map is a
/// `BTreeMap` (the `preserve_order` feature is not enabled here), so its bytes
/// are key-sorted at every level. The same is true of ai-meta#265's read-side
/// `sha256_of`, which takes a `&Value`.
///
/// Nothing states that this is required and nothing tests it. A refactor that
/// digested the struct directly — the obvious, shorter thing to write — would
/// silently produce a per-process digest, and it would pass every existing
/// check, because no existing check ever re-derives a digest in a second
/// process: the value is computed once and copied. #265's cross-store
/// comparator compares the incumbent's stored checksum against the one the
/// mirror carried, which is the same number moved, not recomputed.
///
/// The event-sourced read model is precisely the thing that breaks that
/// assumption: a second process folds the same events and compares digests. So
/// this function makes the property explicit, names it, and the tests below
/// pin it.
///
/// ⚠ If `serde_json`'s `preserve_order` feature is ever enabled in this
/// workspace, `Value` becomes insertion-ordered and this stops being canonical.
/// `the_canonical_digest_is_stable_across_hash_orders` is the guard.
pub fn canonical_state_digest(state: &WorkflowState) -> String {
    use sha2::{Digest, Sha256};
    let value = serde_json::to_value(state).unwrap_or(serde_json::Value::Null);
    let bytes = serde_json::to_vec(&value).unwrap_or_default();
    hex::encode(Sha256::digest(&bytes))
}

#[cfg(test)]
mod canonical_digest_tests {
    use super::*;

    fn state_with(order: &[usize]) -> WorkflowState {
        let mut ws = WorkflowState::new(42, 7);
        for &i in order {
            ws.ctx
                .insert(format!("key_{i:02}"), serde_json::json!({ "n": i }));
            ws.ctx_set_marks.insert(format!("mark_{i:02}"), i as i64);
        }
        ws
    }

    /// The set-backed half of the property (noetl/ai-meta#314).
    ///
    /// `canonical_state_digest` is canonical for MAPS because `serde_json::Value`
    /// uses a `BTreeMap`, so object keys come out sorted at every level. A **set**
    /// gets none of that: it serialises to a JSON *array*, and an array keeps
    /// whatever iteration order the collection has. With a `HashSet` that order is
    /// per-process (SipHash, randomly seeded), so two processes folding the same
    /// events produced arrays with the same elements in different orders and
    /// therefore different digests.
    ///
    /// Measured on prod 2026-09-01: a 30-execution equivalence sweep reported
    /// **100% input agreement** and only **8/30 digest agreement**. Every one of
    /// the 22 "divergences" was a pure permutation of
    /// `/steps/*/iteration_command_ids` — same command ids, reordered, zero
    /// genuine divergence. While that holds, the digest cannot gate a serve-flip:
    /// it reports ~73% divergence on identical data, and a REAL divergence would
    /// be invisible in that noise.
    ///
    /// The existing test above uses `ctx` and `ctx_set_marks`, which are both
    /// maps — so it passed throughout and could never have caught this.
    #[test]
    fn a_set_backed_field_serialises_in_a_deterministic_order() {
        // Enough elements that a hash-ordered collection coming out sorted by
        // chance is not a thing that happens: 24! orderings.
        let ids: Vec<String> = (0..24).map(|i| format!("exec:step:{i:04}:i{i}")).collect();

        let mut forward = WorkflowState::new(42, 7);
        let mut backward = WorkflowState::new(42, 7);
        let step_f = forward
            .steps
            .entry("loop_step".to_string())
            .or_insert_with(|| StepInfo::new("loop_step"));
        for id in ids.iter() {
            step_f.iteration_command_ids.insert(id.clone());
        }
        let step_b = backward
            .steps
            .entry("loop_step".to_string())
            .or_insert_with(|| StepInfo::new("loop_step"));
        for id in ids.iter().rev() {
            step_b.iteration_command_ids.insert(id.clone());
        }

        let v = serde_json::to_value(&forward).unwrap();
        let arr = v["steps"]["loop_step"]["iteration_command_ids"]
            .as_array()
            .expect("the set must still serialise as a JSON array — the wire shape is unchanged");
        let got: Vec<&str> = arr.iter().map(|x| x.as_str().unwrap()).collect();
        let mut want: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
        want.sort_unstable();
        assert_eq!(
            got, want,
            "a set-backed field must serialise in sorted order, or its digest depends on              the process that folded it"
        );

        assert_eq!(
            canonical_state_digest(&forward),
            canonical_state_digest(&backward),
            "the same set built in opposite insertion orders must digest identically —              this is what made 22 of 30 prod executions read as divergent when their              data was identical"
        );
    }

    /// The elements still round-trip, so this is a canonicalisation and not a
    /// quiet data change.
    #[test]
    fn converting_the_set_did_not_change_what_it_holds() {
        let mut ws = WorkflowState::new(1, 1);
        let step = ws
            .steps
            .entry("s".to_string())
            .or_insert_with(|| StepInfo::new("s"));
        for i in 0..8 {
            step.iteration_command_ids.insert(format!("cmd-{i}"));
        }
        step.cursor_completed.insert("cur-a".to_string());
        let json = serde_json::to_string(&ws).unwrap();
        let back: WorkflowState = serde_json::from_str(&json).unwrap();
        let s = &back.steps["s"];
        assert_eq!(
            s.iteration_command_ids.len(),
            8,
            "no element lost on round-trip"
        );
        assert!(s.iteration_command_ids.contains("cmd-3"));
        assert!(s.cursor_completed.contains("cur-a"));
        // Deduplication is still the point of it being a set.
        let step = ws.steps.get_mut("s").unwrap();
        assert!(!step.iteration_command_ids.insert("cmd-3".to_string()));
    }

    /// The same logical state, built in two different insertion orders, must
    /// digest identically.
    ///
    /// This is the in-process half of the property. The cross-process half
    /// cannot be asserted from inside one test binary — it was measured with
    /// `examples/fold_digest_probe.rs`, run four times, and the raw digest
    /// differed every time while this one did not.
    #[test]
    fn the_canonical_digest_is_stable_across_hash_orders() {
        let forward: Vec<usize> = (0..16).collect();
        let backward: Vec<usize> = (0..16).rev().collect();
        assert_eq!(
            canonical_state_digest(&state_with(&forward)),
            canonical_state_digest(&state_with(&backward)),
            "the canonical digest must not depend on insertion order"
        );
    }

    /// NEGATIVE CONTROL. A "canonicaliser" that returned a constant would pass
    /// the test above and prove nothing.
    #[test]
    fn the_canonical_digest_moves_when_the_state_moves() {
        let base = state_with(&(0..16).collect::<Vec<_>>());
        let mut changed = state_with(&(0..16).collect::<Vec<_>>());
        changed
            .ctx
            .insert("key_07".to_string(), serde_json::json!({ "n": 999 }));
        assert_ne!(
            canonical_state_digest(&base),
            canonical_state_digest(&changed),
            "a one-value change must change the digest, or this is not a digest"
        );

        // …and a change buried in a NESTED object, since the canonicalisation
        // has to reach the whole tree and not just the top level.
        let mut nested = state_with(&(0..16).collect::<Vec<_>>());
        nested.ctx.insert(
            "key_03".to_string(),
            serde_json::json!({ "n": 3, "deep": { "b": 2, "a": 1 } }),
        );
        assert_ne!(
            canonical_state_digest(&base),
            canonical_state_digest(&nested)
        );
    }

    /// The raw form is NOT canonical — asserted so the difference is a tested
    /// property rather than a claim in a doc comment.
    ///
    /// Same logical state, two insertion orders. Within one process the
    /// `HashMap` seed is fixed, so the two maps iterate the same way and the
    /// raw bytes agree — which is exactly why this defect survived: every
    /// same-process check agrees. The test therefore asserts the weaker, true
    /// thing: the raw serialisation carries key order at all, so it is a
    /// function of iteration and not of value.
    #[test]
    fn the_raw_serialisation_carries_hash_order() {
        let ws = state_with(&(0..16).collect::<Vec<_>>());
        let raw = serde_json::to_vec(&ws).expect("serialise");
        let text = String::from_utf8_lossy(&raw);
        let first = text
            .split("\"key_")
            .nth(1)
            .map(|s| s[..2].to_string())
            .expect("ctx keys present");
        let canon = serde_json::to_vec(&serde_json::to_value(&ws).unwrap()).unwrap();
        let canon_text = String::from_utf8_lossy(&canon);
        let canon_first = canon_text
            .split("\"key_")
            .nth(1)
            .map(|s| s[..2].to_string())
            .expect("ctx keys present");
        assert_eq!(
            canon_first, "00",
            "the canonical form must start at the lowest key; got {canon_first}"
        );
        // `first` is whatever this process's seed produced. No assertion on its
        // value — asserting it would bake one process's seed into the suite.
        let _ = first;
    }
}
