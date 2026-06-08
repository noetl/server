//! Condition evaluation for workflow transitions (Canonical Format).
//!
//! Evaluates Jinja2-style conditions and next[].when logic
//! for workflow transition decisions.
//!
//! Canonical format:
//! - step.when: transition enable guard (evaluated before step runs)
//! - next[].when: conditional routing (evaluated after step completes)
//! - No case/when/then blocks

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::playbook::types::{NextSpec, Step};
use crate::template::TemplateRenderer;

/// Result of evaluating a condition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationResult {
    /// Whether the condition evaluated to true.
    pub matched: bool,
    /// The next step to transition to (if matched).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_step: Option<String>,
    /// Parameters to pass to the next step (legacy Targets path, plain merge).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub with_params: Option<serde_json::Value>,
    /// Arc-level `set:` variable mutations (Router / NextArc path).
    /// Values are unrendered Jinja2 templates; the orchestrator renders them
    /// against the producing step's completion context and then applies
    /// scope-stripping via `apply_set_mutations` before dispatching the
    /// downstream command.  Distinct from `with_params` so the orchestrator
    /// can apply rendering + scope-stripping only to these.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arc_set_vars: Option<HashMap<String, serde_json::Value>>,
    /// Error message if evaluation failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl EvaluationResult {
    /// Create a matched result (legacy Targets path, plain with_params).
    pub fn matched(next_step: &str, with_params: Option<serde_json::Value>) -> Self {
        Self {
            matched: true,
            next_step: Some(next_step.to_string()),
            with_params,
            arc_set_vars: None,
            error: None,
        }
    }

    /// Create a matched result carrying arc-level `set:` mutations (Router path).
    pub fn matched_with_set(
        next_step: &str,
        set_vars: Option<HashMap<String, serde_json::Value>>,
    ) -> Self {
        Self {
            matched: true,
            next_step: Some(next_step.to_string()),
            with_params: None,
            arc_set_vars: set_vars,
            error: None,
        }
    }

    /// Create a non-matched result.
    pub fn not_matched() -> Self {
        Self {
            matched: false,
            next_step: None,
            with_params: None,
            arc_set_vars: None,
            error: None,
        }
    }

    /// Create a non-matched result that carries the target step name.
    /// Used by `evaluate_next_transitions` to surface arc targets
    /// that didn't fire (because exclusive mode already chose a
    /// sibling, or the arc's `when` evaluated false).  The
    /// orchestrator emits `step.skipped` for these so the R4 fan-in
    /// barrier correctly treats them as terminal.  See
    /// noetl/ai-meta#67.
    pub fn not_matched_with_target(target: &str) -> Self {
        Self {
            matched: false,
            next_step: Some(target.to_string()),
            with_params: None,
            arc_set_vars: None,
            error: None,
        }
    }

    /// Create an error result.
    pub fn error(message: &str) -> Self {
        Self {
            matched: false,
            next_step: None,
            with_params: None,
            arc_set_vars: None,
            error: Some(message.to_string()),
        }
    }
}

/// Next transition evaluation mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NextMode {
    /// First matching when condition wins (default).
    #[default]
    Exclusive,
    /// All matching when conditions fire.
    Inclusive,
}

impl NextMode {
    /// Parse from string.
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "inclusive" => NextMode::Inclusive,
            _ => NextMode::Exclusive,
        }
    }
}

/// Condition evaluator for workflow transitions (canonical format).
pub struct ConditionEvaluator {
    renderer: TemplateRenderer,
}

impl Default for ConditionEvaluator {
    fn default() -> Self {
        Self::new()
    }
}

impl ConditionEvaluator {
    /// Create a new condition evaluator.
    pub fn new() -> Self {
        Self {
            renderer: TemplateRenderer::new(),
        }
    }

    /// Evaluate a simple condition expression.
    pub fn evaluate_condition(
        &self,
        condition: &str,
        context: &HashMap<String, serde_json::Value>,
    ) -> AppResult<bool> {
        self.renderer.evaluate_condition(condition, context)
    }

    /// Evaluate step enable guard (step.when).
    ///
    /// Returns true if the step should execute, false if it should be skipped.
    /// If no when guard is present, returns true (step always executes).
    pub fn evaluate_step_when(
        &self,
        step: &Step,
        context: &HashMap<String, serde_json::Value>,
    ) -> AppResult<bool> {
        match &step.when {
            Some(when_expr) => self.evaluate_condition(when_expr, context),
            None => Ok(true), // No guard = always execute
        }
    }

    /// Evaluate next transitions with optional when conditions (canonical format).
    ///
    /// Supports two evaluation modes via step.spec.next_mode:
    /// - exclusive (default): First matching when condition wins
    /// - inclusive: All matching when conditions fire
    ///
    /// Entries without a when condition always match.
    pub fn evaluate_next_transitions(
        &self,
        step: &Step,
        context: &HashMap<String, serde_json::Value>,
    ) -> AppResult<Vec<EvaluationResult>> {
        let mut results = Vec::new();

        // Determine next_mode
        let next_mode = step
            .spec
            .as_ref()
            .and_then(|s| s.next_mode.as_ref())
            .map(|m| NextMode::from_str(m))
            .unwrap_or_default();

        match &step.next {
            Some(NextSpec::Single(next_step)) => {
                // Single next: always transition (no condition)
                results.push(EvaluationResult::matched(next_step, None));
            }
            Some(NextSpec::List(next_steps)) => {
                // List of next steps: transition to all (parallel branches)
                for next_step in next_steps {
                    results.push(EvaluationResult::matched(next_step, None));
                }
            }
            Some(NextSpec::Router(router)) => {
                // Canonical v10 format: router with spec and arcs
                // Determine mode from router spec (overrides step-level next_mode)
                let router_mode = router
                    .spec
                    .as_ref()
                    .and_then(|s| s.mode.as_ref())
                    .map(|m| NextMode::from_str(m))
                    .unwrap_or(next_mode);

                // #67 fix: emit `EvaluationResult { matched: false,
                // next_step: Some(...) }` for arcs that don't fire
                // (either because exclusive mode already picked an
                // earlier match, or because the arc's `when`
                // evaluated false).  process_in_progress uses these
                // to emit `step.skipped` for those targets so the
                // R4 fan-in barrier correctly treats them as
                // terminal.  Without this, a sibling arc target
                // (e.g. process_low under exclusive routing) stays
                // Pending forever and a fan-in target downstream
                // (e.g. summarize) deadlocks.
                let mut exclusive_matched = false;
                for arc in &router.arcs {
                    // In exclusive mode, once we've matched once,
                    // emit the rest as not-matched-with-target so
                    // the caller can skip them.  We do NOT evaluate
                    // their when (irrelevant — they won't fire).
                    if exclusive_matched {
                        results.push(EvaluationResult::not_matched_with_target(&arc.step));
                        continue;
                    }

                    // Evaluate when condition if present
                    let should_transition = match &arc.when {
                        Some(when_expr) => self.evaluate_condition(when_expr, context)?,
                        None => true, // No condition = always matches
                    };

                    if should_transition {
                        results.push(EvaluationResult::matched_with_set(
                            &arc.step,
                            arc.set_vars.clone(),
                        ));

                        // In exclusive mode, first match wins — but
                        // we don't `break`; we continue the loop so
                        // remaining arcs surface as
                        // not_matched_with_target (the trip wire for
                        // step.skipped emission).
                        if router_mode == NextMode::Exclusive {
                            exclusive_matched = true;
                        }
                    } else {
                        // Arc's when evaluated false — also emit as
                        // not_matched_with_target so callers can
                        // emit step.skipped.  Applies to BOTH
                        // exclusive (a false when before the
                        // matched one) AND inclusive modes.
                        results.push(EvaluationResult::not_matched_with_target(&arc.step));
                    }
                }
            }
            Some(NextSpec::Targets(targets)) => {
                // Legacy canonical format: targets with optional when conditions
                let mut exclusive_matched = false;
                for target in targets {
                    if exclusive_matched {
                        results.push(EvaluationResult::not_matched_with_target(&target.step));
                        continue;
                    }

                    // Evaluate when condition if present
                    let should_transition = match &target.when {
                        Some(when_expr) => self.evaluate_condition(when_expr, context)?,
                        None => true, // No condition = always matches
                    };

                    if should_transition {
                        let with_params = target.args.as_ref().map(|args| {
                            serde_json::to_value(args).unwrap_or(serde_json::Value::Null)
                        });
                        // NOTE: legacy CanonicalNextTarget.args is retained as a plain
                        // pass-through (no scope-stripping) for back-compat with any
                        // existing fixtures that use the Targets format.  New playbooks
                        // use NextArc.set_vars (YAML key: `set:`).
                        results.push(EvaluationResult::matched(&target.step, with_params));

                        // In exclusive mode, first match wins — but
                        // continue iterating to surface the unmatched
                        // siblings (see Router branch above).
                        if next_mode == NextMode::Exclusive {
                            exclusive_matched = true;
                        }
                    } else {
                        results.push(EvaluationResult::not_matched_with_target(&target.step));
                    }
                }
            }
            None => {
                // No next specified - workflow ends or implicit 'end'
            }
        }

        Ok(results)
    }

    /// Evaluate transition logic for a step (structural next, no conditions).
    ///
    /// Returns the next step(s) to execute based on the step's `next` configuration.
    /// This is for backwards compatibility - use evaluate_next_transitions for
    /// canonical format with conditional routing.
    pub fn evaluate_next(
        &self,
        step: &Step,
        context: &HashMap<String, serde_json::Value>,
    ) -> AppResult<Vec<EvaluationResult>> {
        // Delegate to the canonical evaluation method
        self.evaluate_next_transitions(step, context)
    }

    /// Evaluate a loop condition.
    ///
    /// Returns the collection to iterate over after rendering templates.
    pub fn evaluate_loop(
        &self,
        loop_expr: &str,
        context: &HashMap<String, serde_json::Value>,
    ) -> AppResult<Vec<serde_json::Value>> {
        // Render the loop expression to get the collection
        let value = self.renderer.render_to_value(loop_expr, context)?;

        // Convert to array
        match value {
            serde_json::Value::Array(arr) => Ok(arr),
            serde_json::Value::Object(map) => {
                // Convert object to array of key-value pairs
                Ok(map
                    .into_iter()
                    .map(|(k, v)| serde_json::json!({"key": k, "value": v}))
                    .collect())
            }
            serde_json::Value::String(s) => {
                // Try to parse as JSON array
                if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&s) {
                    Ok(arr)
                } else {
                    // Split string by newlines or commas
                    Ok(s.split([',', '\n'])
                        .map(|item| serde_json::Value::String(item.trim().to_string()))
                        .filter(|v| !v.as_str().unwrap_or("").is_empty())
                        .collect())
                }
            }
            serde_json::Value::Number(n) => {
                // Create a range [0, n)
                let n = n.as_u64().unwrap_or(0) as usize;
                Ok((0..n).map(|i| serde_json::json!(i)).collect())
            }
            _ => Err(AppError::Validation(format!(
                "Loop expression did not evaluate to an iterable: {}",
                loop_expr
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_evaluate_simple_condition() {
        let evaluator = ConditionEvaluator::new();
        let mut context = HashMap::new();
        context.insert("status".to_string(), serde_json::json!("success"));
        context.insert("count".to_string(), serde_json::json!(5));

        assert!(evaluator
            .evaluate_condition("status == 'success'", &context)
            .unwrap());
        assert!(!evaluator
            .evaluate_condition("status == 'failed'", &context)
            .unwrap());
        assert!(evaluator.evaluate_condition("count > 3", &context).unwrap());
        assert!(!evaluator
            .evaluate_condition("count > 10", &context)
            .unwrap());
    }

    #[test]
    fn test_evaluate_loop_array() {
        let evaluator = ConditionEvaluator::new();
        let mut context = HashMap::new();
        context.insert("items".to_string(), serde_json::json!(["a", "b", "c"]));

        let result = evaluator.evaluate_loop("{{ items }}", &context).unwrap();
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn test_evaluate_loop_number() {
        let evaluator = ConditionEvaluator::new();
        let mut context = HashMap::new();
        context.insert("count".to_string(), serde_json::json!(5));

        let result = evaluator.evaluate_loop("{{ count }}", &context).unwrap();
        assert_eq!(result.len(), 5);
    }

    #[test]
    fn test_evaluation_result_serialization() {
        let result =
            EvaluationResult::matched("next_step", Some(serde_json::json!({"key": "value"})));
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("next_step"));
        assert!(json.contains("matched"));
    }

    #[test]
    fn test_evaluation_result_not_matched() {
        let result = EvaluationResult::not_matched();
        assert!(!result.matched);
        assert!(result.next_step.is_none());
    }

    #[test]
    fn test_evaluation_result_error() {
        let result = EvaluationResult::error("something went wrong");
        assert!(!result.matched);
        assert_eq!(result.error, Some("something went wrong".to_string()));
    }

    #[test]
    fn test_next_mode_parsing() {
        assert_eq!(NextMode::from_str("exclusive"), NextMode::Exclusive);
        assert_eq!(NextMode::from_str("inclusive"), NextMode::Inclusive);
        assert_eq!(NextMode::from_str("EXCLUSIVE"), NextMode::Exclusive);
        assert_eq!(NextMode::from_str("INCLUSIVE"), NextMode::Inclusive);
        assert_eq!(NextMode::from_str("unknown"), NextMode::Exclusive); // default
    }
}
