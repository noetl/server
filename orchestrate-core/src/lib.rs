//! `noetl-orchestrate-core` — the pure orchestrator drive core.
//!
//! This crate holds the referentially-transparent pieces of the NoETL
//! orchestrator: given an execution's event history and its playbook, compute
//! the next commands.  It has **no** dependency on the server's runtime (no DB,
//! no axum, no NATS) so it compiles to two targets from one source:
//!
//! - linked into `noetl-control-plane` for today's in-process drive path, and
//! - compiled to a `wasm32-unknown-unknown` system plug-in — the
//!   `system/orchestrate` kernel service — with no WASI surface, behind the
//!   worker's capability ring.
//!
//! See the program blueprint
//! (`docs/architecture/noetl_server_dissolution_and_global_grid.md`) and
//! noetl/ai-meta#108.  The migration is incremental: the template renderer is
//! the first slice; `evaluator`, `state`, `commands`, and `orchestrator` follow.

pub mod commands;
pub mod error;
pub mod event;
pub mod evaluator;
pub mod playbook;
pub mod template;
