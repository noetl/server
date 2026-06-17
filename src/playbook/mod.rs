//! NoETL Playbook DSL v2.
//!
//! This module provides playbook parsing and validation:
//! - Type definitions for playbook structure
//! - YAML parsing
//! - Validation

pub mod parser;
// The playbook type model moved into the pure `noetl-orchestrate-core` crate
// (so `evaluate` + the drive core use it on both native and wasm32 —
// noetl/ai-meta#108).  Re-exported here as `types` so every
// `crate::playbook::types::*` call site is unchanged.
pub use noetl_orchestrate_core::playbook as types;

pub use parser::{extract_kind, extract_metadata, parse_playbook, validate_playbook};
pub use types::{
    CanonicalNextTarget, Command, EvalCondition, EvalElse, EvalEntry, KeychainDef, Loop, LoopMode,
    LoopSpec, Metadata, NextSpec, Playbook, Step, StepSpec, ToolCall, ToolDefinition, ToolKind,
    ToolSpec, WorkbookTask,
};
