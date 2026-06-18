//! Workflow execution engine.
//!
//! This module provides the core execution engine for NoETL:
//!
//! - **Orchestrator**: Coordinates workflow execution flow
//! - **State**: Reconstructs execution state from events
//! - **Evaluator**: Evaluates conditions and case/when/then logic
//! - **Commands**: Generates commands for workers

// `commands` + `evaluator` moved into the pure `noetl-orchestrate-core` crate
// (noetl/ai-meta#108); re-exported here so `crate::engine::commands` /
// `super::commands` call sites (orchestrator, handlers) are unchanged.
// The whole drive core now lives in noetl-orchestrate-core (native + wasm32);
// re-exported so `crate::engine::*` call sites are unchanged (noetl/ai-meta#109).
pub use noetl_orchestrate_core::{commands, evaluator, orchestrator, state};

pub use noetl_orchestrate_core::commands::{Command, CommandBuilder};
pub use noetl_orchestrate_core::evaluator::ConditionEvaluator;
pub use noetl_orchestrate_core::orchestrator::WorkflowOrchestrator;
pub use noetl_orchestrate_core::state::{ExecutionState, StepState, WorkflowState};
