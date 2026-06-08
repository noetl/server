//! Workflow orchestration engine.
//!
//! Coordinates workflow execution by:
//! - Analyzing events to determine current state
//! - Evaluating transitions to determine next steps
//! - Publishing commands for workers

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::db::models::Event;
use crate::error::{AppError, AppResult};
use crate::playbook::types::{NextSpec, Playbook, Step};

use super::commands::{Command, CommandBuilder, IteratorMetadata};
use super::evaluator::ConditionEvaluator;
use super::state::{ExecutionState, WorkflowState};

/// Merge iterator metadata into the step-enter context so
/// `state.apply_event` can stamp `iterations_expected` (and a
/// readable iterator name) onto the resulting `StepInfo` during
/// state reconstruction.  `with_params` is the existing transition
/// context (if any); the helper returns a new JSON object that
/// includes both that AND the iteration keys.
fn merge_iteration_context(
    with_params: Option<serde_json::Value>,
    iterations_expected: i32,
    iterator_var: &str,
) -> serde_json::Value {
    let mut obj = match with_params {
        Some(serde_json::Value::Object(m)) => m,
        _ => serde_json::Map::new(),
    };
    obj.insert(
        "iterations_expected".to_string(),
        serde_json::json!(iterations_expected),
    );
    obj.insert(
        "iterator_var".to_string(),
        serde_json::Value::String(iterator_var.to_string()),
    );
    serde_json::Value::Object(obj)
}

/// Build the inverse arc graph for a workflow: `target_step` →
/// `{ upstream steps that point at it }`.
///
/// Used by the orchestrator's fan-in / reduce barrier (Phase D R4,
/// noetl/ai-meta#49 → noetl/server#142).  A step with **more than
/// one** entry in its upstream set is a reduce boundary — its
/// dispatch is deferred until every upstream is in a terminal
/// state (`Completed | Failed | Skipped`).  Single-upstream
/// targets are unaffected.
///
/// Mirrors `repos/noetl/noetl/core/dsl/engine/planner.py`'s
/// `build_fanout_reduce_plan` `incoming` map — every step's
/// `next` arc collects all its outgoing targets, and the inverse
/// gives the set of upstreams per step.  Targets that are not
/// real steps in the workflow (notably the sentinel `"end"`) are
/// skipped — the orchestrator already special-cases `end` via the
/// `reached_end` quiescent path.
///
/// The empty/missing-`next` case yields no edges (a terminal step
/// produces no targets).  Self-loops are recorded; the dispatch
/// path treats them like any other upstream.
fn build_incoming_arcs<'a>(
    steps: &'a HashMap<&'a str, &'a Step>,
) -> HashMap<&'a str, HashSet<&'a str>> {
    let mut incoming: HashMap<&'a str, HashSet<&'a str>> = HashMap::new();
    for (step_name, step) in steps {
        let targets = collect_arc_targets(step);
        for target in targets {
            // Only count targets that resolve to a real step in
            // the workflow definition; the dispatch path drops
            // unknown targets too.
            if steps.contains_key(target.as_str()) {
                // SAFETY: the key lives as long as `steps` (which
                // outlives this function's borrow).
                let target_key: &'a str = steps
                    .get_key_value(target.as_str())
                    .map(|(k, _)| *k)
                    .expect("contains_key just confirmed it");
                incoming.entry(target_key).or_default().insert(step_name);
            }
        }
    }
    incoming
}

/// Collect every target step-name referenced by a step's `next`
/// router, across all four `NextSpec` variants.  Helper for
/// [`build_incoming_arcs`].
fn collect_arc_targets(step: &Step) -> Vec<String> {
    match &step.next {
        None => Vec::new(),
        Some(NextSpec::Single(name)) => vec![name.clone()],
        Some(NextSpec::List(names)) => names.clone(),
        Some(NextSpec::Router(router)) => router.arcs.iter().map(|a| a.step.clone()).collect(),
        Some(NextSpec::Targets(targets)) => targets.iter().map(|t| t.step.clone()).collect(),
    }
}

/// Result of orchestration evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrchestrationResult {
    /// Current execution state.
    pub state: ExecutionState,
    /// Commands to issue.
    pub commands: Vec<Command>,
    /// Whether the execution should complete.
    pub should_complete: bool,
    /// Completion status if should_complete is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_status: Option<CompletionStatus>,
    /// Events to emit.
    pub events_to_emit: Vec<EventToEmit>,
}

/// Completion status for a workflow.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionStatus {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_steps: Option<Vec<String>>,
}

/// Event to emit during orchestration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventToEmit {
    pub event_type: String,
    pub node_name: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Workflow orchestrator.
pub struct WorkflowOrchestrator {
    evaluator: ConditionEvaluator,
    command_builder: CommandBuilder,
}

impl Default for WorkflowOrchestrator {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkflowOrchestrator {
    /// Create a new workflow orchestrator.
    pub fn new() -> Self {
        Self {
            evaluator: ConditionEvaluator::new(),
            command_builder: CommandBuilder::new(),
        }
    }

    /// Evaluate an execution and determine next actions.
    ///
    /// This is the main orchestration entry point, called when:
    /// - A new execution starts
    /// - A worker reports a result (via event)
    pub fn evaluate(
        &self,
        events: &[Event],
        playbook: &Playbook,
        trigger_event_type: Option<&str>,
    ) -> AppResult<OrchestrationResult> {
        // Reconstruct workflow state from events
        let state = WorkflowState::from_events(events)
            .ok_or_else(|| AppError::Validation("No events found for execution".to_string()))?;

        debug!(
            "Evaluating execution {}, state: {}, trigger: {:?}",
            state.execution_id, state.state, trigger_event_type
        );

        // Check for terminal states
        if matches!(
            state.state,
            ExecutionState::Completed | ExecutionState::Failed | ExecutionState::Cancelled
        ) {
            return Ok(OrchestrationResult {
                state: state.state,
                commands: vec![],
                should_complete: false,
                completion_status: None,
                events_to_emit: vec![],
            });
        }

        // Skip evaluation for progress marker events
        if let Some(event_type) = trigger_event_type {
            if matches!(event_type, "step_started" | "step_running") {
                debug!("Skipping orchestration for progress marker event");
                return Ok(OrchestrationResult {
                    state: state.state,
                    commands: vec![],
                    should_complete: false,
                    completion_status: None,
                    events_to_emit: vec![],
                });
            }
        }

        // Build context for evaluation (convert Value to HashMap)
        let context = value_to_hashmap(&state.build_context());

        // Build step lookup
        let steps: HashMap<&str, &Step> = playbook
            .workflow
            .iter()
            .map(|s| (s.step.as_str(), s))
            .collect();

        // Determine what to do based on state
        match state.state {
            ExecutionState::Initial => {
                // Start first step(s) - always start with "start" step
                self.dispatch_initial_steps(&state, playbook, &context)
            }
            ExecutionState::InProgress => {
                // Check if we need to dispatch the initial step
                // (playbook_started but no steps entered yet)
                if state.steps.is_empty() {
                    return self.dispatch_initial_steps(&state, playbook, &context);
                }
                // Process completed steps and determine next steps
                self.process_in_progress(&state, &steps, &context, trigger_event_type)
            }
            _ => Ok(OrchestrationResult {
                state: state.state,
                commands: vec![],
                should_complete: false,
                completion_status: None,
                events_to_emit: vec![],
            }),
        }
    }

    /// Dispatch initial workflow steps.
    fn dispatch_initial_steps(
        &self,
        state: &WorkflowState,
        playbook: &Playbook,
        context: &HashMap<String, serde_json::Value>,
    ) -> AppResult<OrchestrationResult> {
        let mut commands = Vec::new();
        let mut events_to_emit = Vec::new();

        // Find start step (always named "start")
        let start_step = playbook
            .get_step("start")
            .ok_or_else(|| AppError::Validation("Start step 'start' not found".to_string()))?;

        info!("Dispatching initial step: {}", start_step.step);

        // Create step.enter event
        events_to_emit.push(EventToEmit {
            event_type: "step.enter".to_string(),
            node_name: Some(start_step.step.clone()),
            status: "ENTERED".to_string(),
            context: None,
            result: None,
            error: None,
        });

        // Build command for the step
        // Note: In a real implementation, command_id would come from get_snowflake_id()
        let command = self.command_builder.build_command(
            0, // Placeholder - real implementation would use snowflake ID
            state.execution_id,
            state.catalog_id,
            0, // Placeholder - would be parent event ID
            start_step,
            context,
            None,
        )?;

        commands.push(command);

        Ok(OrchestrationResult {
            state: ExecutionState::InProgress,
            commands,
            should_complete: false,
            completion_status: None,
            events_to_emit,
        })
    }

    /// Process an in-progress execution.
    fn process_in_progress(
        &self,
        state: &WorkflowState,
        steps: &HashMap<&str, &Step>,
        context: &HashMap<String, serde_json::Value>,
        trigger_event_type: Option<&str>,
    ) -> AppResult<OrchestrationResult> {
        let mut commands = Vec::new();
        let mut events_to_emit = Vec::new();
        // R3c parallel-branch completion: track whether the
        // transition path saw a route to `end` so we can defer the
        // completion decision until after every parallel branch is
        // accounted for.  See `decide_completion` at the end of this
        // function — completing on the first branch that hits `end`
        // would falsely mark the playbook done while the other
        // branches are still running.
        let mut reached_end = false;
        // Steps already dispatched in THIS pass.  `is_step_done` /
        // `running_steps` read from the events DB, so two sibling
        // arcs whose `next_step` resolves to the same target would
        // otherwise both queue commands in the same orchestrator
        // round (neither has a persisted event yet).  Track here so
        // the second arc skips dispatch.  Surfaced as part of the
        // `end`-step-with-action fix (noetl/ai-meta#54): parallel
        // branches converging on `end` would double-queue without
        // this guard.
        let mut dispatched_in_pass: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        // R4 fan-in / reduce barrier (noetl/ai-meta#49 Phase D Round 4,
        // sub-issue noetl/server#142).  A step that has more than one
        // upstream arc — the canonical `PlannedReduce` shape from the
        // Python `repos/noetl/noetl/core/dsl/engine/planner.py` —
        // should fire ONCE after every upstream finishes, not once
        // per upstream completion.  Today the dispatch-skip checks
        // below cover same-pass dedup (a sibling arc to the same
        // target in the same orchestrator round) + already-running /
        // already-done.  They do NOT cover the cross-pass case where
        // branch A completes in pass 1 and branch B is still running:
        // the orchestrator would dispatch `reduce_customer` based on
        // A alone, and `reduce_customer` would never see B's result.
        //
        // The barrier check below defers dispatch of any multi-
        // upstream target until every upstream is in a terminal state
        // (`Completed | Failed | Skipped`, i.e. `is_step_done`).
        // command.failed already short-circuits via the dedicated path
        // at the top of this function, so reaching the dispatch loop
        // with all upstreams done means none failed mid-flight.
        let incoming_arcs = build_incoming_arcs(steps);

        // command.failed gets its own dedicated short-circuit path
        // BEFORE the transition-trigger filter — a failed step must
        // not have its next.arcs evaluated, and the orchestrator
        // must emit `playbook.failed` once all in-flight work is
        // drained (the existing completion path waited for every
        // branch to reach `end`, which a failed branch never does).
        // See noetl/ai-meta#58 for the e2e finding (control_flow_workbook
        // stalled at `command.failed` with no terminal event).
        if matches!(trigger_event_type, Some("command.failed")) {
            // Detect failed steps via the durable `state` field
            // (set by apply_event when `command.failed` or
            // `step_failed` lands), not via `info.error.is_some()`.
            // The error-string extraction at apply_event time only
            // catches top-level `result.error`; many tools emit
            // their failure context under `result.context.error`,
            // so step.error stays None even on real failures.
            // step.state is the reliable signal.
            let failed_steps: Vec<String> = state
                .steps
                .iter()
                .filter(|(_, info)| matches!(info.state, crate::engine::state::StepState::Failed))
                .map(|(name, _)| name.clone())
                .collect();

            // No failed step recorded yet (race between event ingest
            // and apply_event) — keep waiting; the next trigger
            // round will see it.
            if failed_steps.is_empty() {
                return Ok(OrchestrationResult {
                    state: ExecutionState::InProgress,
                    commands,
                    should_complete: false,
                    completion_status: None,
                    events_to_emit,
                });
            }

            // Sibling parallel branches still running — defer the
            // terminal decision so each in-flight branch gets to
            // emit its own outcome event into the log.  When the
            // last running branch finishes, that branch's
            // command.completed or command.failed will re-trigger
            // us and we'll re-check this condition.
            if state.has_running_steps() {
                return Ok(OrchestrationResult {
                    state: ExecutionState::InProgress,
                    commands,
                    should_complete: false,
                    completion_status: None,
                    events_to_emit,
                });
            }

            // All in-flight work drained, at least one step failed
            // — emit the terminal playbook.failed event.
            return Ok(OrchestrationResult {
                state: ExecutionState::Failed,
                commands: vec![],
                should_complete: true,
                completion_status: Some(CompletionStatus {
                    status: "FAILED".to_string(),
                    error: Some(format!("Failed steps: {}", failed_steps.join(", "))),
                    failed_steps: Some(failed_steps),
                }),
                events_to_emit,
            });
        }

        // Only process transitions on completion events
        if !matches!(
            trigger_event_type,
            Some("command.completed")
                | Some("action_completed")
                | Some("step.exit")
                | Some("step_completed")
                | Some("iterator_completed")
        ) {
            return Ok(OrchestrationResult {
                state: ExecutionState::InProgress,
                commands,
                should_complete: false,
                completion_status: None,
                events_to_emit,
            });
        }

        // #67: pre-compute the per-completed-step eval_results so
        // we can do TWO ordered passes:
        //   pass 1 — emit step.skipped for unmatched arc targets,
        //            so `in_pass_skipped` is fully populated before
        //            any barrier check;
        //   pass 2 — dispatch matched arc targets, with the barrier
        //            able to see every same-pass skip via the
        //            collected in_pass_skipped set.
        // Without this two-pass ordering, HashMap iteration order
        // determined whether `summarize` got dispatched in the same
        // pass as `start`'s step.skipped for `process_low`.
        let mut per_step_evals: Vec<(String, &Step, Vec<crate::engine::evaluator::EvaluationResult>)> =
            Vec::new();
        for step_name in state.steps.keys() {
            if !state.is_step_completed(step_name) {
                continue;
            }
            let step = match steps.get(step_name.as_str()) {
                Some(s) => *s,
                None => continue,
            };
            let eval_results = self.evaluator.evaluate_next(step, context)?;
            per_step_evals.push((step_name.clone(), step, eval_results));
        }

        // Pass 1: emit step.skipped for every unmatched arc target
        // across ALL completed steps, before any dispatch barrier
        // check.  Under `mode: exclusive`, the untaken sibling
        // arc's target never runs; without step.skipped it would
        // stay Pending forever and the R4 fan-in barrier on
        // downstream merge points would deadlock.
        for (_completed_step_name, _completed_step, eval_results) in &per_step_evals {
            for result in eval_results {
                if result.matched {
                    continue;
                }
                let Some(target_name) = &result.next_step else {
                    continue;
                };
                // Skip if already terminal / running / dispatched
                // (already emitted in this loop).
                if state.is_step_done(target_name)
                    || state.running_steps().contains(&target_name.as_str())
                    || dispatched_in_pass.contains(target_name)
                {
                    continue;
                }
                if !steps.contains_key(target_name.as_str()) {
                    continue;
                }
                info!(
                    "Step '{}' skipped (exclusive routing chose a sibling)",
                    target_name
                );
                events_to_emit.push(EventToEmit {
                    event_type: "step.skipped".to_string(),
                    node_name: Some(target_name.clone()),
                    status: "SKIPPED".to_string(),
                    context: None,
                    result: None,
                    error: None,
                });
                dispatched_in_pass.insert(target_name.clone());
            }
        }

        // Pass 2: dispatch matched arc targets.
        for (step_name, _step, eval_results) in per_step_evals {
            let _ = step_name; // kept for parity with old log shape if needed
            for result in eval_results {
                if !result.matched {
                    continue;
                }

                if let Some(next_step_name) = &result.next_step {
                    // R3c parallel-branch completion: hitting `end`
                    // no longer short-circuits the per-result loop.
                    // We mark `reached_end` and continue so that
                    // sibling matched arcs in the SAME completion
                    // round (and other parallel branches in flight)
                    // are still considered.  The final
                    // should_complete decision happens after the
                    // loops finish, gated on no remaining commands
                    // queued and no other steps still running.
                    if next_step_name == "end" {
                        debug!("Branch reached 'end'; deferring completion decision");
                        reached_end = true;

                        // If `end` is defined as a real step in the
                        // workflow (the canonical v10 shape — an
                        // aggregator that may carry its own cleanup
                        // tool), fall through to the normal dispatch
                        // path below so the end step's action runs.
                        // Without this, every `end:` step with a
                        // `tool:` block (e.g. `test_end_with_action`'s
                        // cleanup Python) was silently skipped — the
                        // orchestrator went straight to
                        // `playbook.completed` without executing the
                        // end step.
                        //
                        // Skip dispatch only when `end` is not a
                        // defined step (legacy "pure terminal" case);
                        // `reached_end_quiescent` then handles the
                        // completion transition.
                        if !steps.contains_key("end") {
                            continue;
                        }
                    }

                    // Get next step definition
                    let next_step = match steps.get(next_step_name.as_str()) {
                        Some(s) => *s,
                        None => {
                            warn!("Next step '{}' not found in workflow", next_step_name);
                            continue;
                        }
                    };

                    // Skip if already completed or running
                    if state.is_step_done(next_step_name) {
                        debug!("Step '{}' already done, skipping", next_step_name);
                        continue;
                    }

                    if state.running_steps().contains(&next_step_name.as_str()) {
                        debug!("Step '{}' already running, skipping", next_step_name);
                        continue;
                    }

                    // Same-pass dedup: a sibling arc may have just
                    // queued a command for this step in this round.
                    if dispatched_in_pass.contains(next_step_name) {
                        debug!(
                            "Step '{}' already dispatched in this pass, skipping",
                            next_step_name
                        );
                        continue;
                    }

                    // R4 fan-in / reduce barrier
                    // (noetl/ai-meta#49 Phase D R4, noetl/server#142).
                    // If the target step has more than one upstream
                    // arc, defer dispatch until every upstream is
                    // terminal.  `is_step_done` treats Skipped (the
                    // step.when guard-false path) and Failed as done
                    // for barrier purposes; the dedicated
                    // command.failed path at the top of this function
                    // owns terminal-status-on-failure, so reaching
                    // this branch with any upstream Failed is
                    // structurally impossible (we'd have returned
                    // ExecutionState::Failed first).
                    if let Some(upstreams) = incoming_arcs.get(next_step_name.as_str()) {
                        if upstreams.len() > 1 {
                            // #67: also treat upstreams that this
                            // same pass just emitted `step.skipped`
                            // for as terminal.  Without this, the
                            // skip event lands in `events_to_emit`
                            // but `state.is_step_done` won't see it
                            // until the next orchestrator pass (when
                            // trigger_orchestrator persists the
                            // events and re-triggers).  Letting
                            // `summarize` dispatch in the SAME pass
                            // is structurally fine — the worker
                            // pulls commands from NATS after the
                            // events are persisted, by which point
                            // the skip event is already durable.
                            let in_pass_skipped: std::collections::HashSet<&str> =
                                events_to_emit
                                    .iter()
                                    .filter(|e| e.event_type == "step.skipped")
                                    .filter_map(|e| e.node_name.as_deref())
                                    .collect();
                            let pending: Vec<&str> = upstreams
                                .iter()
                                .copied()
                                .filter(|up| {
                                    !state.is_step_done(up) && !in_pass_skipped.contains(up)
                                })
                                .collect();
                            if !pending.is_empty() {
                                debug!(
                                    "Reduce step '{}' deferring dispatch — {} of {} upstream(s) still pending: {:?}",
                                    next_step_name,
                                    pending.len(),
                                    upstreams.len(),
                                    pending,
                                );
                                continue;
                            }
                        }
                    }

                    // Build context for next step with additional params
                    let mut step_context = context.clone();
                    if let Some(serde_json::Value::Object(params)) = &result.with_params {
                        for (k, v) in params {
                            step_context.insert(k.clone(), v.clone());
                        }
                    }

                    // Iterative `step.when` enable-guard chain.  When a
                    // step's `when` expression evaluates false we emit
                    // `step.skipped` instead of `step.enter`, then walk
                    // forward to that step's own `next` arcs and try
                    // again — repeats until we land on either a step
                    // whose guard passes (emit step.enter + command) or
                    // a terminal/end transition (mark completion).
                    //
                    // Doing this inline in the same orchestrator pass
                    // avoids the re-trigger gymnastics that would
                    // otherwise be needed: `step.skipped` has no
                    // `command.completed` to fire the next round on.
                    let mut current_step: &Step = next_step;
                    let mut current_step_name: String = next_step_name.clone();
                    let mut current_with_params = result.with_params.clone();
                    let mut current_ctx = step_context;
                    let mut should_dispatch = true;
                    let mut hit_end = false;
                    let mut completion: Option<CompletionStatus> = None;

                    loop {
                        let guard_ok = self
                            .evaluator
                            .evaluate_step_when(current_step, &current_ctx)?;
                        if guard_ok {
                            break;
                        }

                        info!(
                            "Step '{}' skipped (when guard false)",
                            current_step_name
                        );
                        events_to_emit.push(EventToEmit {
                            event_type: "step.skipped".to_string(),
                            node_name: Some(current_step_name.clone()),
                            status: "SKIPPED".to_string(),
                            context: current_with_params.clone(),
                            result: None,
                            error: None,
                        });

                        // Follow the skipped step's transitions.  Pick
                        // the first matched arc — once we've decided
                        // to skip, we've already committed to the
                        // single-path chain.
                        let chained =
                            self.evaluator.evaluate_next(current_step, &current_ctx)?;
                        let next_after_skip = chained
                            .into_iter()
                            .find(|r| r.matched && r.next_step.is_some());

                        let Some(arc) = next_after_skip else {
                            // No further transition.  Treat the skipped
                            // step as terminal — workflow ends here
                            // unless another step is still running.
                            should_dispatch = false;
                            break;
                        };
                        let target_name = arc.next_step.expect("matched arc has next_step");

                        if target_name == "end" {
                            hit_end = true;
                            should_dispatch = false;
                            completion = Some(CompletionStatus {
                                status: "COMPLETED".to_string(),
                                error: None,
                                failed_steps: None,
                            });
                            break;
                        }

                        let Some(target_step) = steps.get(target_name.as_str()) else {
                            warn!(
                                "Chained next step '{}' not found in workflow",
                                target_name
                            );
                            should_dispatch = false;
                            break;
                        };

                        // Merge any with_params from the chained arc
                        // into the context for the next iteration.
                        if let Some(serde_json::Value::Object(params)) = &arc.with_params {
                            for (k, v) in params {
                                current_ctx.insert(k.clone(), v.clone());
                            }
                        }

                        current_step = *target_step;
                        current_step_name = target_name;
                        current_with_params = arc.with_params;
                    }

                    if hit_end {
                        // R3c: defer completion same as the direct
                        // `end` arc above — sibling branches in this
                        // same pass (or parallel branches in flight)
                        // may still need to run.  Note the completion
                        // status from the skip-chain (the caller
                        // may have set it from a chained arc); if so,
                        // remember it for the final decision.
                        if reached_end {
                            // Keep the existing reached_end flag.
                        } else {
                            reached_end = true;
                        }
                        let _ = completion;
                        continue;
                    }

                    if !should_dispatch {
                        continue;
                    }

                    // R3a skip-chain re-entry guard: after walking
                    // forward through one or more skipped steps, the
                    // chain target may itself already be Completed
                    // or running.  Without this guard, every
                    // subsequent command.completed event for any
                    // later step in the workflow re-triggers the
                    // orchestrator, which re-evaluates `start`'s
                    // transitions, walks the skip chain again, and
                    // emits a fresh step.enter + command.issued for
                    // the chain target.  Surfaced by Phase D R3a
                    // re-validation after noetl/ai-meta#53 unblocked
                    // multi-trigger paths — the chain target was
                    // `tail`, which got re-issued on every
                    // tail.command.completed.
                    if state.is_step_done(&current_step_name) {
                        debug!(
                            "Skip-chain target '{}' already done, suppressing re-dispatch",
                            current_step_name
                        );
                        continue;
                    }
                    if state.running_steps().contains(&current_step_name.as_str()) {
                        debug!(
                            "Skip-chain target '{}' already running, suppressing re-dispatch",
                            current_step_name
                        );
                        continue;
                    }

                    // R3b iterator fan-out: if the landed step
                    // declares `step.loop`, evaluate the loop
                    // expression and emit one command per item.  The
                    // single `step.enter` event carries
                    // `iterations_expected` in its context so state
                    // reconstruction can aggregate per-iteration
                    // `command.completed` events into one
                    // step-level completion (see
                    // `state.rs::apply_event`).  Sequential and
                    // parallel modes both fan out the same way at
                    // this layer; concurrency is shaped downstream
                    // by the worker pool.
                    if let Some(loop_cfg) = current_step.r#loop.as_ref() {
                        let items = self
                            .evaluator
                            .evaluate_loop(&loop_cfg.in_expr, &current_ctx)?;
                        let total: usize = items.len();

                        if total == 0 {
                            // Empty collection — emit step.enter with
                            // total=0 + a synthetic step.exit so
                            // downstream transitions still fire.  No
                            // command dispatched.
                            info!(
                                "Iterator step '{}' produced empty collection — short-circuiting",
                                current_step_name
                            );
                            let enter_ctx = merge_iteration_context(
                                current_with_params.clone(),
                                0i32,
                                &loop_cfg.iterator,
                            );
                            events_to_emit.push(EventToEmit {
                                event_type: "step.enter".to_string(),
                                node_name: Some(current_step_name.clone()),
                                status: "ENTERED".to_string(),
                                context: Some(enter_ctx),
                                result: None,
                                error: None,
                            });
                            events_to_emit.push(EventToEmit {
                                event_type: "step.exit".to_string(),
                                node_name: Some(current_step_name.clone()),
                                status: "COMPLETED".to_string(),
                                context: None,
                                result: Some(serde_json::Value::Array(vec![])),
                                error: None,
                            });
                            continue;
                        }

                        info!(
                            "Fanning out {} iterations for step '{}' (iterator='{}')",
                            total, current_step_name, loop_cfg.iterator
                        );

                        // One `step.enter` carries the total so
                        // state.apply_event can stamp
                        // iterations_expected onto the StepInfo.
                        let enter_ctx = merge_iteration_context(
                            current_with_params.clone(),
                            total as i32,
                            &loop_cfg.iterator,
                        );
                        events_to_emit.push(EventToEmit {
                            event_type: "step.enter".to_string(),
                            node_name: Some(current_step_name.clone()),
                            status: "ENTERED".to_string(),
                            context: Some(enter_ctx),
                            result: None,
                            error: None,
                        });

                        // One command per item via
                        // build_iteration_command (which injects
                        // `<iterator>`, `_index`, `_total` into the
                        // command's render context).
                        for (idx, item) in items.into_iter().enumerate() {
                            let iter_meta = IteratorMetadata {
                                parent_execution_id: state.execution_id,
                                iterator_step: current_step_name.clone(),
                                item_var: loop_cfg.iterator.clone(),
                                item,
                                index: idx,
                                total,
                            };
                            let command = self.command_builder.build_iteration_command(
                                0,
                                state.execution_id,
                                state.catalog_id,
                                0,
                                current_step,
                                &current_ctx,
                                iter_meta,
                            )?;
                            commands.push(command);
                        }

                        continue;
                    }

                    info!("Transitioning to step: {}", current_step_name);

                    // Create step.enter event for the step we landed
                    // on (after walking the skip chain, if any).
                    events_to_emit.push(EventToEmit {
                        event_type: "step.enter".to_string(),
                        node_name: Some(current_step_name.clone()),
                        status: "ENTERED".to_string(),
                        context: current_with_params,
                        result: None,
                        error: None,
                    });

                    // Build command
                    let command = self.command_builder.build_command(
                        0,
                        state.execution_id,
                        state.catalog_id,
                        0,
                        current_step,
                        &current_ctx,
                        None,
                    )?;

                    commands.push(command);
                    dispatched_in_pass.insert(current_step_name.clone());
                }
            }
        }

        // R3c parallel-branch completion: complete when EITHER
        // - check_completion returns true (existing semantic: every
        //   step's terminal arc is satisfied + no running branches);
        // - OR a branch reached `end` AND we didn't queue new commands
        //   in this pass AND no other branches are still running.
        // The second clause covers the case where multiple parallel
        // branches converge on `end` — the LAST branch to arrive sees
        // `reached_end == true` with everything else done and finalises
        // the workflow.  The early-return that used to fire on the
        // FIRST branch to hit `end` would have falsely completed the
        // workflow while sibling branches were still in flight.
        let check_says_done = self.check_completion(state, steps)?;
        let reached_end_quiescent =
            reached_end && commands.is_empty() && !state.has_running_steps();
        let should_complete = check_says_done || reached_end_quiescent;

        let completion_status = if should_complete {
            // Check for failures
            let failed_steps: Vec<String> = state
                .steps
                .iter()
                .filter(|(_, info)| info.error.is_some())
                .map(|(name, _)| name.clone())
                .collect();

            if failed_steps.is_empty() {
                Some(CompletionStatus {
                    status: "COMPLETED".to_string(),
                    error: None,
                    failed_steps: None,
                })
            } else {
                Some(CompletionStatus {
                    status: "FAILED".to_string(),
                    error: Some(format!("Failed steps: {}", failed_steps.join(", "))),
                    failed_steps: Some(failed_steps),
                })
            }
        } else {
            None
        };

        Ok(OrchestrationResult {
            state: ExecutionState::InProgress,
            commands,
            should_complete,
            completion_status,
            events_to_emit,
        })
    }

    /// Check if the execution should complete.
    fn check_completion(
        &self,
        state: &WorkflowState,
        steps: &HashMap<&str, &Step>,
    ) -> AppResult<bool> {
        // Check if there are any running steps
        if state.has_running_steps() {
            return Ok(false);
        }

        // Check if 'end' step is completed
        if state.is_step_completed("end") {
            return Ok(true);
        }

        // Check if all steps with no successors are completed
        for (name, step) in steps {
            if step.next.is_none() && state.is_step_completed(name) {
                // Found a terminal step that's completed
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Handle a failed step.
    pub fn handle_failure(
        &self,
        _state: &WorkflowState,
        step_name: &str,
        error: &str,
    ) -> AppResult<OrchestrationResult> {
        info!("Handling failure for step '{}': {}", step_name, error);

        let events_to_emit = vec![EventToEmit {
            event_type: "step_failed".to_string(),
            node_name: Some(step_name.to_string()),
            status: "FAILED".to_string(),
            context: None,
            result: None,
            error: Some(error.to_string()),
        }];

        Ok(OrchestrationResult {
            state: ExecutionState::Failed,
            commands: vec![],
            should_complete: true,
            completion_status: Some(CompletionStatus {
                status: "FAILED".to_string(),
                error: Some(error.to_string()),
                failed_steps: Some(vec![step_name.to_string()]),
            }),
            events_to_emit,
        })
    }
}

/// Convert a serde_json::Value to HashMap (extracts top-level object keys).
fn value_to_hashmap(value: &serde_json::Value) -> HashMap<String, serde_json::Value> {
    match value {
        serde_json::Value::Object(map) => map.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        _ => HashMap::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playbook::types::{Metadata, NextSpec, ToolDefinition, ToolKind, ToolSpec};
    use chrono::Utc;

    fn make_step(name: &str, next: Option<&str>) -> Step {
        Step {
            step: name.to_string(),
            desc: None,
            spec: None,
            when: None,
            args: None,
            vars: None,
            r#loop: None,
            tool: ToolDefinition::Single(ToolSpec {
                kind: ToolKind::Python,
                eval: None,
                auth: None,
                libs: None,
                args: None,
                code: Some("return {}".to_string()),
                url: None,
                method: None,
                query: None,
                command: None,
                connection: None,
                params: None,
                headers: None,
                output_select: None,
                extra: HashMap::new(),
            }),
            next: next.map(|n| NextSpec::Single(n.to_string())),
        }
    }

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
    fn test_evaluate_initial_state() {
        let orchestrator = WorkflowOrchestrator::new();

        let events = vec![{
            let mut e = make_event("playbook_started", None);
            e.context = Some(serde_json::json!({
                "workload": {},
                "path": "test",
                "version": "1"
            }));
            e
        }];

        let playbook = Playbook {
            api_version: "noetl.io/v2".to_string(),
            kind: "Playbook".to_string(),
            metadata: Metadata {
                name: "test_playbook".to_string(),
                path: Some("test/path".to_string()),
                description: None,
                labels: None,
                extra: HashMap::new(),
            },
            workload: None,
            vars: None,
            keychain: None,
            workbook: None,
            workflow: vec![
                make_step("start", Some("step2")),
                make_step("step2", Some("end")),
                make_step("end", None),
            ],
        };

        let result = orchestrator.evaluate(&events, &playbook, None).unwrap();

        assert_eq!(result.state, ExecutionState::InProgress);
        assert!(!result.commands.is_empty());
        assert!(!result.events_to_emit.is_empty());
    }

    #[test]
    fn test_evaluate_errors_on_invalid_template_in_step_body() {
        // noetl/ai-meta#54 (e2e regression sweep): a step whose tool
        // `code` body carries an invalid Jinja expression (`{{ ctx.* }}`)
        // must make `evaluate` return `Err` — deterministically, not
        // `Ok`-with-no-commands and not a panic.  `handlers::events::
        // trigger_orchestrator` relies on this contract to emit a
        // terminal `playbook.failed` event instead of stranding the
        // execution in RUNNING forever (the original symptom:
        // `test_vars_template_access` hung after `set_variables`).
        let orchestrator = WorkflowOrchestrator::new();

        // `start` has completed; evaluate must now build the command for
        // `bad_step`, which renders its invalid-template code body.
        let events = vec![
            {
                let mut e = make_event("playbook_started", None);
                e.context = Some(serde_json::json!({
                    "workload": {}, "path": "test", "version": "1"
                }));
                e
            },
            make_event("command.completed", Some("start")),
        ];

        let bad_step = {
            let mut s = make_step("bad_step", Some("end"));
            s.tool = ToolDefinition::Single(ToolSpec {
                kind: ToolKind::Python,
                eval: None,
                auth: None,
                libs: None,
                args: None,
                code: Some("# uses {{ ctx.* }} templates\nresult = {}".to_string()),
                url: None,
                method: None,
                query: None,
                command: None,
                connection: None,
                params: None,
                headers: None,
                output_select: None,
                extra: HashMap::new(),
            });
            s
        };

        let playbook = Playbook {
            api_version: "noetl.io/v2".to_string(),
            kind: "Playbook".to_string(),
            metadata: Metadata {
                name: "bad_template".to_string(),
                path: Some("test/bad_template".to_string()),
                description: None,
                labels: None,
                extra: HashMap::new(),
            },
            workload: None,
            vars: None,
            keychain: None,
            workbook: None,
            workflow: vec![
                make_step("start", Some("bad_step")),
                bad_step,
                make_step("end", None),
            ],
        };

        let result = orchestrator.evaluate(&events, &playbook, Some("command.completed"));
        assert!(
            result.is_err(),
            "evaluate must return Err for an invalid template in a step body, got Ok"
        );
    }

    #[test]
    fn test_handle_failure() {
        let orchestrator = WorkflowOrchestrator::new();
        let state = WorkflowState::new(12345, 67890);

        let result = orchestrator
            .handle_failure(&state, "failed_step", "Something went wrong")
            .unwrap();

        assert_eq!(result.state, ExecutionState::Failed);
        assert!(result.should_complete);
        assert!(result.completion_status.is_some());
        let status = result.completion_status.unwrap();
        assert_eq!(status.status, "FAILED");
        assert!(status.error.is_some());
    }

    #[test]
    fn test_command_failed_emits_terminal_playbook_failed() {
        // noetl/ai-meta#58 — process_in_progress used to early-return
        // on command.failed and never emit the terminal playbook.failed
        // event.  Execution would stall mid-flight forever.  With the
        // fix, a command.failed trigger drains in-flight work and
        // (when nothing is still running) marks the playbook as
        // FAILED with the failed_steps list populated.
        let orchestrator = WorkflowOrchestrator::new();

        let start = make_step("start", Some("eval_flag"));
        let eval_flag = make_step("eval_flag", Some("end"));
        let end = make_step("end", None);

        let events = vec![
            {
                let mut e = make_event("playbook_started", None);
                e.context = Some(serde_json::json!({
                    "workload": {}, "path": "test", "version": "1"
                }));
                e
            },
            make_event("command.completed", Some("start")),
            make_event("step.enter", Some("eval_flag")),
            {
                let mut e = make_event("call.error", Some("eval_flag"));
                e.result = Some(serde_json::json!({"error": "Tool not found: workbook"}));
                e
            },
            {
                let mut e = make_event("command.failed", Some("eval_flag"));
                e.result = Some(serde_json::json!({"error": "Tool not found: workbook"}));
                e
            },
        ];

        let playbook = Playbook {
            api_version: "noetl.io/v2".to_string(),
            kind: "Playbook".to_string(),
            metadata: Metadata {
                name: "fail_terminal".to_string(),
                path: Some("test/fail_terminal".to_string()),
                description: None,
                labels: None,
                extra: HashMap::new(),
            },
            workload: None,
            vars: None,
            keychain: None,
            workbook: None,
            workflow: vec![start, eval_flag, end],
        };

        let result = orchestrator
            .evaluate(&events, &playbook, Some("command.failed"))
            .unwrap();

        assert!(
            result.should_complete,
            "command.failed must terminate the playbook when no other steps are running"
        );
        assert_eq!(result.state, ExecutionState::Failed);
        let status = result
            .completion_status
            .expect("completion_status must be populated on terminal failure");
        assert_eq!(status.status, "FAILED");
        assert_eq!(status.failed_steps.as_ref().unwrap(), &vec!["eval_flag".to_string()]);
        assert!(status
            .error
            .as_ref()
            .unwrap()
            .contains("eval_flag"));
    }

    #[test]
    fn test_command_failed_defers_terminal_while_sibling_running() {
        // Parallel-branch case: branch_a fails while branch_b is
        // still in flight.  The orchestrator must NOT mark the
        // playbook FAILED yet — wait for branch_b to drain so the
        // event log carries every branch's outcome.  When branch_b
        // eventually finishes (success or failure), the next trigger
        // round re-checks and emits the terminal event then.
        let orchestrator = WorkflowOrchestrator::new();

        let start = make_step_with_parallel_next("start", &["branch_a", "branch_b"]);
        let branch_a = make_step("branch_a", Some("end"));
        let branch_b = make_step("branch_b", Some("end"));
        let end = make_step("end", None);

        let events = vec![
            {
                let mut e = make_event("playbook_started", None);
                e.context = Some(serde_json::json!({
                    "workload": {}, "path": "test", "version": "1"
                }));
                e
            },
            make_event("command.completed", Some("start")),
            // Both branches entered + claimed.
            make_event("step.enter", Some("branch_a")),
            make_event("command.issued", Some("branch_a")),
            make_event("step.enter", Some("branch_b")),
            make_event("command.issued", Some("branch_b")),
            // branch_a fails; branch_b still running.
            {
                let mut e = make_event("call.error", Some("branch_a"));
                e.result = Some(serde_json::json!({"error": "branch_a blew up"}));
                e
            },
            {
                let mut e = make_event("command.failed", Some("branch_a"));
                e.result = Some(serde_json::json!({"error": "branch_a blew up"}));
                e
            },
        ];

        let playbook = Playbook {
            api_version: "noetl.io/v2".to_string(),
            kind: "Playbook".to_string(),
            metadata: Metadata {
                name: "fail_with_sibling".to_string(),
                path: Some("test/fail_with_sibling".to_string()),
                description: None,
                labels: None,
                extra: HashMap::new(),
            },
            workload: None,
            vars: None,
            keychain: None,
            workbook: None,
            workflow: vec![start, branch_a, branch_b, end],
        };

        let result = orchestrator
            .evaluate(&events, &playbook, Some("command.failed"))
            .unwrap();

        assert!(
            !result.should_complete,
            "playbook must NOT terminate while branch_b is still running"
        );
        // State stays InProgress for the deferred outcome.
        assert_eq!(result.state, ExecutionState::InProgress);
    }

    #[test]
    fn test_step_when_guard_skips_step() {
        // Playbook: start → middle (when=false) → end
        // Expectation: orchestrator emits step.skipped(middle) and
        // walks the chain forward to `end`, completing the workflow
        // without ever dispatching a command for `middle`.
        let orchestrator = WorkflowOrchestrator::new();

        let mut start = make_step("start", Some("middle"));
        // start has no guard
        start.when = None;
        let mut middle = make_step("middle", Some("end"));
        middle.when = Some("{{ false }}".to_string());
        let end = make_step("end", None);

        let events = vec![
            {
                let mut e = make_event("playbook_started", None);
                e.context = Some(serde_json::json!({
                    "workload": {},
                    "path": "test",
                    "version": "1"
                }));
                e
            },
            make_event("command.completed", Some("start")),
        ];

        let playbook = Playbook {
            api_version: "noetl.io/v2".to_string(),
            kind: "Playbook".to_string(),
            metadata: Metadata {
                name: "skip_test".to_string(),
                path: Some("test/skip".to_string()),
                description: None,
                labels: None,
                extra: HashMap::new(),
            },
            workload: None,
            vars: None,
            keychain: None,
            workbook: None,
            workflow: vec![start, middle, end],
        };

        let result = orchestrator
            .evaluate(&events, &playbook, Some("command.completed"))
            .unwrap();

        // No command dispatched (skip chain reached `end` directly).
        assert!(
            result.commands.is_empty(),
            "skip chain should not dispatch any command, got {:?}",
            result.commands
        );
        // Should complete with status=COMPLETED.
        assert!(result.should_complete);
        assert_eq!(
            result.completion_status.as_ref().map(|c| c.status.as_str()),
            Some("COMPLETED")
        );
        // A step.skipped event was emitted for `middle`.
        let skipped: Vec<_> = result
            .events_to_emit
            .iter()
            .filter(|e| e.event_type == "step.skipped")
            .collect();
        assert_eq!(skipped.len(), 1, "expected one step.skipped event");
        assert_eq!(skipped[0].node_name.as_deref(), Some("middle"));
    }

    #[test]
    fn test_step_when_guard_passes_dispatches_step() {
        // Same shape but middle's when is true — orchestrator
        // should dispatch a command for `middle` and emit
        // step.enter(middle) (no step.skipped).
        let orchestrator = WorkflowOrchestrator::new();

        let start = make_step("start", Some("middle"));
        let mut middle = make_step("middle", Some("end"));
        middle.when = Some("{{ true }}".to_string());
        let end = make_step("end", None);

        let events = vec![
            {
                let mut e = make_event("playbook_started", None);
                e.context = Some(serde_json::json!({
                    "workload": {},
                    "path": "test",
                    "version": "1"
                }));
                e
            },
            make_event("command.completed", Some("start")),
        ];

        let playbook = Playbook {
            api_version: "noetl.io/v2".to_string(),
            kind: "Playbook".to_string(),
            metadata: Metadata {
                name: "guard_test".to_string(),
                path: Some("test/guard".to_string()),
                description: None,
                labels: None,
                extra: HashMap::new(),
            },
            workload: None,
            vars: None,
            keychain: None,
            workbook: None,
            workflow: vec![start, middle, end],
        };

        let result = orchestrator
            .evaluate(&events, &playbook, Some("command.completed"))
            .unwrap();

        assert_eq!(result.commands.len(), 1, "should dispatch middle");
        let enters: Vec<_> = result
            .events_to_emit
            .iter()
            .filter(|e| e.event_type == "step.enter")
            .collect();
        assert_eq!(enters.len(), 1);
        assert_eq!(enters[0].node_name.as_deref(), Some("middle"));
        let skipped = result
            .events_to_emit
            .iter()
            .any(|e| e.event_type == "step.skipped");
        assert!(!skipped, "should NOT emit step.skipped when guard passes");
    }

    #[test]
    fn test_step_loop_fans_out_iterations() {
        // Playbook: start → looped (loop.in=[1,2,3]) → end.
        // Expectation: orchestrator emits one step.enter(looped)
        // carrying iterations_expected=3 in context, and dispatches
        // three commands (one per item) each with iterator metadata.
        let orchestrator = WorkflowOrchestrator::new();

        let start = make_step("start", Some("looped"));
        let mut looped = make_step("looped", Some("end"));
        looped.r#loop = Some(crate::playbook::types::Loop {
            in_expr: "{{ [1, 2, 3] }}".to_string(),
            iterator: "n".to_string(),
            spec: None,
        });
        let end = make_step("end", None);

        let events = vec![
            {
                let mut e = make_event("playbook_started", None);
                e.context = Some(serde_json::json!({
                    "workload": {},
                    "path": "test",
                    "version": "1"
                }));
                e
            },
            make_event("command.completed", Some("start")),
        ];

        let playbook = Playbook {
            api_version: "noetl.io/v2".to_string(),
            kind: "Playbook".to_string(),
            metadata: Metadata {
                name: "loop_test".to_string(),
                path: Some("test/loop".to_string()),
                description: None,
                labels: None,
                extra: HashMap::new(),
            },
            workload: None,
            vars: None,
            keychain: None,
            workbook: None,
            workflow: vec![start, looped, end],
        };

        let result = orchestrator
            .evaluate(&events, &playbook, Some("command.completed"))
            .unwrap();

        // Three iteration commands.
        assert_eq!(
            result.commands.len(),
            3,
            "expected 3 iteration commands, got {}",
            result.commands.len()
        );
        // All carry iterator metadata.
        for (idx, cmd) in result.commands.iter().enumerate() {
            let iter = cmd.iterator.as_ref().expect("iterator metadata present");
            assert_eq!(iter.index, idx);
            assert_eq!(iter.total, 3);
            assert_eq!(iter.iterator_step, "looped");
            assert_eq!(iter.item_var, "n");
        }
        // Exactly one step.enter, with iterations_expected=3.
        let enters: Vec<_> = result
            .events_to_emit
            .iter()
            .filter(|e| e.event_type == "step.enter")
            .collect();
        assert_eq!(enters.len(), 1);
        assert_eq!(enters[0].node_name.as_deref(), Some("looped"));
        let enter_ctx = enters[0].context.as_ref().unwrap();
        assert_eq!(
            enter_ctx.get("iterations_expected").and_then(|v| v.as_i64()),
            Some(3)
        );
        assert_eq!(
            enter_ctx.get("iterator_var").and_then(|v| v.as_str()),
            Some("n")
        );
    }

    #[test]
    fn test_step_loop_empty_collection_short_circuits() {
        // Loop expression evaluates to []; orchestrator should
        // emit step.enter (iterations_expected=0) AND a synthetic
        // step.exit so transitions downstream still fire.  No
        // commands dispatched.
        let orchestrator = WorkflowOrchestrator::new();

        let start = make_step("start", Some("looped"));
        let mut looped = make_step("looped", Some("end"));
        looped.r#loop = Some(crate::playbook::types::Loop {
            in_expr: "{{ [] }}".to_string(),
            iterator: "x".to_string(),
            spec: None,
        });
        let end = make_step("end", None);

        let events = vec![
            {
                let mut e = make_event("playbook_started", None);
                e.context = Some(serde_json::json!({
                    "workload": {},
                    "path": "test",
                    "version": "1"
                }));
                e
            },
            make_event("command.completed", Some("start")),
        ];

        let playbook = Playbook {
            api_version: "noetl.io/v2".to_string(),
            kind: "Playbook".to_string(),
            metadata: Metadata {
                name: "loop_empty".to_string(),
                path: Some("test/loop_empty".to_string()),
                description: None,
                labels: None,
                extra: HashMap::new(),
            },
            workload: None,
            vars: None,
            keychain: None,
            workbook: None,
            workflow: vec![start, looped, end],
        };

        let result = orchestrator
            .evaluate(&events, &playbook, Some("command.completed"))
            .unwrap();
        assert!(
            result.commands.is_empty(),
            "empty collection should dispatch no commands"
        );
        let types: Vec<&str> = result
            .events_to_emit
            .iter()
            .map(|e| e.event_type.as_str())
            .collect();
        assert!(types.contains(&"step.enter"));
        assert!(types.contains(&"step.exit"));
    }

    /// Helper: build a step with a Router-style `next` that has
    /// multiple unconditional arcs (parallel fan-out) in inclusive
    /// mode.
    fn make_step_with_parallel_next(name: &str, targets: &[&str]) -> Step {
        use crate::playbook::types::{NextArc, NextRouter, NextRouterSpec};
        let mut step = make_step(name, None);
        step.next = Some(NextSpec::Router(NextRouter {
            spec: Some(NextRouterSpec {
                mode: Some("inclusive".to_string()),
            }),
            arcs: targets
                .iter()
                .map(|t| NextArc {
                    step: t.to_string(),
                    when: None,
                    args: None,
                })
                .collect(),
        }));
        step
    }

    #[test]
    fn test_67_exclusive_routing_emits_step_skipped_for_unmatched_siblings() {
        // noetl/ai-meta#67: under `mode: exclusive` routing, only
        // one arc fires; the untaken sibling arcs' targets never
        // run.  Pre-fix the orchestrator silently dropped those
        // siblings (the R4 fan-in barrier then waited for them
        // forever on any downstream merge point that joined on
        // both branches — deadlock).
        //
        // This test pins the fix: after start.command.completed,
        // the orchestrator emits `step.skipped` for the untaken
        // sibling (`process_low`) in the SAME orchestrator pass.
        // The downstream merge target (`summarize`) — declared with
        // two upstreams in the static planner — now dispatches in
        // the same pass because the barrier check treats
        // in-pass step.skipped as terminal.
        //
        // Reproduces the comprehensive_test.yaml shape: summarize's
        // `input:` block has a Jinja conditional `{{ A if A else B.x }}`
        // referencing the untaken sibling — that's the surface
        // symptom, but the underlying bug was the missing
        // step.skipped, not the template render.
        let orchestrator = WorkflowOrchestrator::new();

        // start → process_high (mode: exclusive; only the start
        // event sets up the routing). process_high → summarize.
        let start = {
            let mut s = make_step("start", None);
            s.next = Some(NextSpec::Router(crate::playbook::types::NextRouter {
                spec: Some(crate::playbook::types::NextRouterSpec {
                    mode: Some("exclusive".to_string()),
                }),
                arcs: vec![
                    crate::playbook::types::NextArc {
                        step: "process_high".to_string(),
                        when: Some("{{ start.random_value > 10 }}".to_string()),
                        args: None,
                    },
                    crate::playbook::types::NextArc {
                        step: "process_low".to_string(),
                        when: Some("{{ start.random_value <= 10 }}".to_string()),
                        args: None,
                    },
                ],
            }));
            s
        };
        let process_high = make_step("process_high", Some("summarize"));
        let process_low = make_step("process_low", Some("summarize"));
        // summarize's tool has `args` with a Jinja conditional that
        // references the untaken sibling step.  Mirrors the
        // comprehensive_test fixture.
        let summarize = {
            let mut s = make_step("summarize", Some("end"));
            s.tool = ToolDefinition::Single(ToolSpec {
                kind: ToolKind::Python,
                eval: None,
                auth: None,
                libs: None,
                args: Some(serde_json::json!({
                    "category": "{{ process_high.category if process_high else process_low.category }}",
                    "final_value": "{{ process_high.processed if process_high else process_low.processed }}"
                })),
                code: Some("result = {\"category\": category}".to_string()),
                url: None,
                method: None,
                query: None,
                command: None,
                connection: None,
                params: None,
                headers: None,
                output_select: None,
                extra: HashMap::new(),
            });
            s
        };
        let end = make_step("end", None);

        // Events: playbook started, start completed (random_value=15
        // surfaces process_high), process_high completed.
        let events = vec![
            {
                let mut e = make_event("playbook_started", None);
                e.context = Some(serde_json::json!({
                    "workload": {"threshold": 10},
                    "path": "test",
                    "version": "1"
                }));
                e
            },
            {
                let mut e = make_event("call.done", Some("start"));
                e.result = Some(serde_json::json!({
                    "status": "COMPLETED",
                    "context": {
                        "result": {
                            "status": "success",
                            "context": {
                                "data": {
                                    "random_value": 15,
                                    "status": "initialized"
                                }
                            }
                        }
                    }
                }));
                e
            },
            make_event("command.completed", Some("start")),
            {
                let mut e = make_event("call.done", Some("process_high"));
                e.result = Some(serde_json::json!({
                    "status": "COMPLETED",
                    "context": {
                        "result": {
                            "status": "success",
                            "context": {
                                "data": {
                                    "category": "high",
                                    "original": 15,
                                    "processed": 30,
                                    "status": "high_processed"
                                }
                            }
                        }
                    }
                }));
                e
            },
            make_event("command.completed", Some("process_high")),
        ];

        let playbook = Playbook {
            api_version: "noetl.io/v2".to_string(),
            kind: "Playbook".to_string(),
            metadata: Metadata {
                name: "comprehensive_repro".to_string(),
                path: Some("test/comprehensive_repro".to_string()),
                description: None,
                labels: None,
                extra: HashMap::new(),
            },
            workload: None,
            vars: None,
            keychain: None,
            workbook: None,
            workflow: vec![start, process_high, process_low, summarize, end],
        };

        // Triggered by process_high.command.completed.  Expected
        // after the #67 fix:
        // - exactly 1 command for `summarize`
        // - 1 step.skipped event for `process_low` (the untaken
        //   exclusive sibling)
        // - 1 step.enter event for `summarize`
        // - !should_complete (summarize hasn't run yet)
        let result = orchestrator
            .evaluate(&events, &playbook, Some("command.completed"))
            .expect("evaluate should succeed after #67 fix");

        let commands: Vec<&str> =
            result.commands.iter().map(|c| c.step_name.as_str()).collect();
        let skipped: Vec<&str> = result
            .events_to_emit
            .iter()
            .filter(|e| e.event_type == "step.skipped")
            .filter_map(|e| e.node_name.as_deref())
            .collect();
        let entered: Vec<&str> = result
            .events_to_emit
            .iter()
            .filter(|e| e.event_type == "step.enter")
            .filter_map(|e| e.node_name.as_deref())
            .collect();

        assert_eq!(
            commands,
            vec!["summarize"],
            "expected 1 command for summarize, got {:?}",
            commands
        );
        assert_eq!(
            skipped,
            vec!["process_low"],
            "expected step.skipped for process_low (the untaken exclusive sibling), got {:?}",
            skipped
        );
        assert!(
            entered.contains(&"summarize"),
            "expected step.enter for summarize, got entries: {:?}",
            entered
        );
        assert!(
            !result.should_complete,
            "summarize is queued but not yet completed — must not should_complete"
        );
    }

    #[test]
    fn test_parallel_branches_dispatch_both_in_one_pass() {
        // start → [branch_a, branch_b] (mode: inclusive)
        // After start completes, orchestrator should emit 2 commands
        // (one per branch) and 2 step.enter events; no step.skipped.
        let orchestrator = WorkflowOrchestrator::new();

        let start = make_step_with_parallel_next("start", &["branch_a", "branch_b"]);
        let branch_a = make_step("branch_a", Some("end"));
        let branch_b = make_step("branch_b", Some("end"));
        let end = make_step("end", None);

        let events = vec![
            {
                let mut e = make_event("playbook_started", None);
                e.context = Some(serde_json::json!({
                    "workload": {},
                    "path": "test",
                    "version": "1"
                }));
                e
            },
            make_event("command.completed", Some("start")),
        ];

        let playbook = Playbook {
            api_version: "noetl.io/v2".to_string(),
            kind: "Playbook".to_string(),
            metadata: Metadata {
                name: "parallel_test".to_string(),
                path: Some("test/parallel".to_string()),
                description: None,
                labels: None,
                extra: HashMap::new(),
            },
            workload: None,
            vars: None,
            keychain: None,
            workbook: None,
            workflow: vec![start, branch_a, branch_b, end],
        };

        let result = orchestrator
            .evaluate(&events, &playbook, Some("command.completed"))
            .unwrap();

        // Both parallel branches must dispatch in the same pass.
        assert_eq!(
            result.commands.len(),
            2,
            "expected 2 parallel commands, got {}",
            result.commands.len()
        );
        let dispatched: Vec<String> =
            result.commands.iter().map(|c| c.step_name.clone()).collect();
        assert!(dispatched.contains(&"branch_a".to_string()));
        assert!(dispatched.contains(&"branch_b".to_string()));

        // One step.enter event per branch.
        let enters: Vec<&str> = result
            .events_to_emit
            .iter()
            .filter(|e| e.event_type == "step.enter")
            .filter_map(|e| e.node_name.as_deref())
            .collect();
        assert_eq!(enters.len(), 2);
        assert!(enters.contains(&"branch_a"));
        assert!(enters.contains(&"branch_b"));

        // Workflow is NOT yet complete — both branches still need to
        // run before `end` can finalise.
        assert!(!result.should_complete);
    }

    #[test]
    fn test_parallel_one_branch_done_defers_completion() {
        // start → [branch_a, branch_b]; branch_a is completed but
        // branch_b is still entered (running).  Orchestrator's
        // evaluate should NOT mark the workflow done just because
        // branch_a transitioned to `end`.
        let orchestrator = WorkflowOrchestrator::new();

        let start = make_step_with_parallel_next("start", &["branch_a", "branch_b"]);
        let branch_a = make_step("branch_a", Some("end"));
        let branch_b = make_step("branch_b", Some("end"));
        let end = make_step("end", None);

        let events = vec![
            {
                let mut e = make_event("playbook_started", None);
                e.context = Some(serde_json::json!({
                    "workload": {}, "path": "test", "version": "1"
                }));
                e
            },
            make_event("command.completed", Some("start")),
            // branch_b is "entered" but not yet completed (state
            // transitions: Entered → CommandIssued via subsequent
            // events that we don't include here).
            make_event("step.enter", Some("branch_b")),
            make_event("command.issued", Some("branch_b")),
            // branch_a completed.
            make_event("step.enter", Some("branch_a")),
            make_event("command.completed", Some("branch_a")),
        ];

        let playbook = Playbook {
            api_version: "noetl.io/v2".to_string(),
            kind: "Playbook".to_string(),
            metadata: Metadata {
                name: "parallel_defer".to_string(),
                path: Some("test/parallel_defer".to_string()),
                description: None,
                labels: None,
                extra: HashMap::new(),
            },
            workload: None,
            vars: None,
            keychain: None,
            workbook: None,
            workflow: vec![start, branch_a, branch_b, end],
        };

        let result = orchestrator
            .evaluate(&events, &playbook, Some("command.completed"))
            .unwrap();

        // branch_a hit `end` but branch_b still pending — workflow
        // should NOT be marked complete.
        assert!(
            !result.should_complete,
            "workflow must not complete while branch_b is still running"
        );
    }

    #[test]
    fn test_parallel_all_branches_done_dispatches_end_once() {
        // Both branches completed and both routed to `end`.  With
        // the noetl/ai-meta#54 fix, `end` is now a real dispatchable
        // step (not a pure terminal sentinel) — the orchestrator
        // queues a single command for it, and same-pass dedup
        // prevents the second branch's arc from double-dispatching.
        // Completion happens later, on `end`'s own command.completed.
        let orchestrator = WorkflowOrchestrator::new();

        let start = make_step_with_parallel_next("start", &["branch_a", "branch_b"]);
        let branch_a = make_step("branch_a", Some("end"));
        let branch_b = make_step("branch_b", Some("end"));
        let end = make_step("end", None);

        let events = vec![
            {
                let mut e = make_event("playbook_started", None);
                e.context = Some(serde_json::json!({
                    "workload": {}, "path": "test", "version": "1"
                }));
                e
            },
            make_event("command.completed", Some("start")),
            make_event("step.enter", Some("branch_a")),
            make_event("command.completed", Some("branch_a")),
            make_event("step.enter", Some("branch_b")),
            make_event("command.completed", Some("branch_b")),
        ];

        let playbook = Playbook {
            api_version: "noetl.io/v2".to_string(),
            kind: "Playbook".to_string(),
            metadata: Metadata {
                name: "parallel_done".to_string(),
                path: Some("test/parallel_done".to_string()),
                description: None,
                labels: None,
                extra: HashMap::new(),
            },
            workload: None,
            vars: None,
            keychain: None,
            workbook: None,
            workflow: vec![start, branch_a, branch_b, end],
        };

        let result = orchestrator
            .evaluate(&events, &playbook, Some("command.completed"))
            .unwrap();

        assert_eq!(
            result.commands.len(),
            1,
            "end step must be dispatched exactly once, not duplicated by sibling arcs"
        );
        assert!(
            !result.should_complete,
            "should not complete until end's own command.completed lands"
        );
    }

    #[test]
    fn test_parallel_all_branches_plus_end_completed_finalises() {
        // Follow-on round: once `end`'s own command.completed is in
        // the event log, check_completion fires and the workflow
        // terminates with status COMPLETED.
        let orchestrator = WorkflowOrchestrator::new();

        let start = make_step_with_parallel_next("start", &["branch_a", "branch_b"]);
        let branch_a = make_step("branch_a", Some("end"));
        let branch_b = make_step("branch_b", Some("end"));
        let end = make_step("end", None);

        let events = vec![
            {
                let mut e = make_event("playbook_started", None);
                e.context = Some(serde_json::json!({
                    "workload": {}, "path": "test", "version": "1"
                }));
                e
            },
            make_event("command.completed", Some("start")),
            make_event("step.enter", Some("branch_a")),
            make_event("command.completed", Some("branch_a")),
            make_event("step.enter", Some("branch_b")),
            make_event("command.completed", Some("branch_b")),
            make_event("step.enter", Some("end")),
            make_event("command.completed", Some("end")),
        ];

        let playbook = Playbook {
            api_version: "noetl.io/v2".to_string(),
            kind: "Playbook".to_string(),
            metadata: Metadata {
                name: "parallel_done_end".to_string(),
                path: Some("test/parallel_done_end".to_string()),
                description: None,
                labels: None,
                extra: HashMap::new(),
            },
            workload: None,
            vars: None,
            keychain: None,
            workbook: None,
            workflow: vec![start, branch_a, branch_b, end],
        };

        let result = orchestrator
            .evaluate(&events, &playbook, Some("command.completed"))
            .unwrap();

        assert!(
            result.should_complete,
            "all branches done + end's own command.completed ⇒ COMPLETED"
        );
        assert_eq!(
            result.completion_status.as_ref().map(|c| c.status.as_str()),
            Some("COMPLETED")
        );
    }

    #[test]
    fn test_orchestration_result_serialization() {
        let result = OrchestrationResult {
            state: ExecutionState::InProgress,
            commands: vec![],
            should_complete: false,
            completion_status: None,
            events_to_emit: vec![],
        };

        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("in_progress"));
    }

    // ============================================================
    // R4 fan-in / reduce barrier tests (noetl/server#142).
    //
    // Topology under test:
    //
    //     start
    //       ├── branch_a ─┐
    //       └── branch_b ─┴── reduce → end
    //
    // `reduce` has TWO incoming arcs (branch_a, branch_b).  The
    // orchestrator must defer its dispatch until BOTH branches
    // finish.
    // ============================================================

    /// Build the fanout_reduce topology used across the R4 tests.
    /// Mirrors `tests/fixtures/playbooks/fanout_reduce/fanout_reduce_phase6.yaml`
    /// in `repos/noetl`.
    fn make_fanout_reduce_workflow() -> Vec<Step> {
        let start = make_step_with_parallel_next("start", &["branch_a", "branch_b"]);
        let branch_a = make_step("branch_a", Some("reduce"));
        let branch_b = make_step("branch_b", Some("reduce"));
        let reduce = make_step("reduce", Some("end"));
        let end = make_step("end", None);
        vec![start, branch_a, branch_b, reduce, end]
    }

    fn fanout_reduce_playbook(workflow: Vec<Step>) -> Playbook {
        Playbook {
            api_version: "noetl.io/v2".to_string(),
            kind: "Playbook".to_string(),
            metadata: Metadata {
                name: "fanout_reduce_test".to_string(),
                path: Some("test/fanout_reduce".to_string()),
                description: None,
                labels: None,
                extra: HashMap::new(),
            },
            workload: None,
            vars: None,
            keychain: None,
            workbook: None,
            workflow,
        }
    }

    #[test]
    fn test_reduce_step_defers_when_one_upstream_still_running() {
        // start fans out to branch_a + branch_b; both target `reduce`.
        // Events show branch_a COMPLETED, branch_b only ENTERED
        // (still running).  Expected: orchestrator does NOT dispatch
        // `reduce` — the second upstream hasn't finished.
        let orchestrator = WorkflowOrchestrator::new();
        let workflow = make_fanout_reduce_workflow();

        let events = vec![
            {
                let mut e = make_event("playbook_started", None);
                e.context = Some(serde_json::json!({
                    "workload": {},
                    "path": "test/fanout_reduce",
                    "version": "1"
                }));
                e
            },
            // start was the previous round; both branches have been
            // dispatched.
            make_event("step.enter", Some("start")),
            make_event("command.completed", Some("start")),
            make_event("step.enter", Some("branch_a")),
            make_event("step.enter", Some("branch_b")),
            // branch_a finishes; branch_b is still running.
            make_event("command.completed", Some("branch_a")),
        ];

        let playbook = fanout_reduce_playbook(workflow);
        let result = orchestrator
            .evaluate(&events, &playbook, Some("command.completed"))
            .unwrap();

        // The orchestrator must NOT have dispatched `reduce` yet —
        // branch_b is still in-flight.
        let dispatched: Vec<String> =
            result.commands.iter().map(|c| c.step_name.clone()).collect();
        assert!(
            !dispatched.contains(&"reduce".to_string()),
            "reduce should not dispatch while branch_b is still running; got commands: {:?}",
            dispatched,
        );
        assert!(!result.should_complete);
    }

    #[test]
    fn test_reduce_step_dispatches_after_all_upstreams_complete() {
        // Same topology; events now show BOTH branches completed.
        // Expected: orchestrator dispatches `reduce` exactly once.
        let orchestrator = WorkflowOrchestrator::new();
        let workflow = make_fanout_reduce_workflow();

        let events = vec![
            {
                let mut e = make_event("playbook_started", None);
                e.context = Some(serde_json::json!({
                    "workload": {},
                    "path": "test/fanout_reduce",
                    "version": "1"
                }));
                e
            },
            make_event("step.enter", Some("start")),
            make_event("command.completed", Some("start")),
            make_event("step.enter", Some("branch_a")),
            make_event("step.enter", Some("branch_b")),
            make_event("command.completed", Some("branch_a")),
            // branch_b finishes last; this is the trigger event.
            make_event("command.completed", Some("branch_b")),
        ];

        let playbook = fanout_reduce_playbook(workflow);
        let result = orchestrator
            .evaluate(&events, &playbook, Some("command.completed"))
            .unwrap();

        // Both branches done → `reduce` dispatches exactly once.
        let reduce_dispatches: usize = result
            .commands
            .iter()
            .filter(|c| c.step_name == "reduce")
            .count();
        assert_eq!(
            reduce_dispatches, 1,
            "expected reduce to dispatch exactly once after both upstreams done; got commands: {:?}",
            result.commands.iter().map(|c| c.step_name.clone()).collect::<Vec<_>>(),
        );
    }

    /// Phase D R4 slice 2 (noetl/server#144) flipped this from
    /// `#[ignore]` to active by adding the `step.skipped` arm to
    /// `state::apply_event`.  The barrier check already treated
    /// Skipped as terminal via `is_step_done` (state.rs:540); the
    /// missing piece was the apply_event mapping that records the
    /// skipped step into `state.steps` with `StepState::Skipped`.
    #[test]
    fn test_reduce_step_treats_skipped_upstream_as_done() {
        // Same topology but branch_b is SKIPPED (the step.when
        // guard-false path emits `step.skipped`, which apply_event
        // marks as terminal `StepState::Skipped`).  branch_a
        // COMPLETED.  Expected: `reduce` dispatches — `is_step_done`
        // already treats Skipped as terminal, so the barrier check
        // should clear and dispatch should proceed.
        let orchestrator = WorkflowOrchestrator::new();
        let workflow = make_fanout_reduce_workflow();

        let events = vec![
            {
                let mut e = make_event("playbook_started", None);
                e.context = Some(serde_json::json!({
                    "workload": {},
                    "path": "test/fanout_reduce",
                    "version": "1"
                }));
                e
            },
            make_event("step.enter", Some("start")),
            make_event("command.completed", Some("start")),
            make_event("step.enter", Some("branch_a")),
            // branch_b never enters — it's skipped via a step.skipped
            // event instead (the canonical when-guard-false path).
            {
                let mut e = make_event("step.skipped", Some("branch_b"));
                e.status = "SKIPPED".to_string();
                e
            },
            // branch_a finishes; trigger event for the orchestrator.
            make_event("command.completed", Some("branch_a")),
        ];

        let playbook = fanout_reduce_playbook(workflow);
        let result = orchestrator
            .evaluate(&events, &playbook, Some("command.completed"))
            .unwrap();

        let reduce_dispatches: usize = result
            .commands
            .iter()
            .filter(|c| c.step_name == "reduce")
            .count();
        assert_eq!(
            reduce_dispatches, 1,
            "expected reduce to dispatch once after branch_a Completed + branch_b Skipped; got commands: {:?}",
            result.commands.iter().map(|c| c.step_name.clone()).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn test_build_incoming_arcs_identifies_reduce_boundary() {
        // Unit-level coverage of the helper used by the barrier
        // check.  fanout_reduce topology: `reduce` has 2 upstreams,
        // every other step has at most 1.
        let workflow = make_fanout_reduce_workflow();
        let steps: HashMap<&str, &Step> =
            workflow.iter().map(|s| (s.step.as_str(), s)).collect();

        let incoming = build_incoming_arcs(&steps);

        // `reduce` has two upstreams (branch_a + branch_b).
        let reduce_upstreams = incoming
            .get("reduce")
            .expect("reduce should have an upstream set");
        assert_eq!(
            reduce_upstreams.len(),
            2,
            "expected reduce to have 2 upstreams; got {:?}",
            reduce_upstreams,
        );
        assert!(reduce_upstreams.contains("branch_a"));
        assert!(reduce_upstreams.contains("branch_b"));

        // Single-upstream + no-upstream steps.
        assert_eq!(incoming.get("branch_a").map(|u| u.len()).unwrap_or(0), 1);
        assert_eq!(incoming.get("branch_b").map(|u| u.len()).unwrap_or(0), 1);
        // `end` is referenced only from `reduce` (single upstream).
        assert_eq!(incoming.get("end").map(|u| u.len()).unwrap_or(0), 1);
        // `start` has no upstreams.
        assert!(incoming.get("start").is_none());
    }
}
