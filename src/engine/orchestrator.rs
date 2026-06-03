//! Workflow orchestration engine.
//!
//! Coordinates workflow execution by:
//! - Analyzing events to determine current state
//! - Evaluating transitions to determine next steps
//! - Publishing commands for workers

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::db::models::Event;
use crate::error::{AppError, AppResult};
use crate::playbook::types::{Playbook, Step};

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

        // Find completed steps that need transition evaluation
        for step_name in state.steps.keys() {
            if !state.is_step_completed(step_name) {
                continue;
            }

            // Get step definition
            let step = match steps.get(step_name.as_str()) {
                Some(s) => *s,
                None => continue,
            };

            // Evaluate next transitions
            let eval_results = self.evaluator.evaluate_next(step, context)?;

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
                        continue;
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
    fn test_parallel_all_branches_done_completes() {
        // Both branches completed and both routed to `end` — the
        // orchestrator's deferred completion should fire.
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

        assert!(
            result.should_complete,
            "both branches done + both at end ⇒ complete"
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
}
