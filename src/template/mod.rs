//! Template rendering module.
//!
//! The renderer now lives in the pure `noetl-orchestrate-core` crate (so it
//! compiles to both this binary and the future wasm32 `system/orchestrate`
//! plug-in — noetl/ai-meta#108).  Re-exported here so existing call sites keep
//! using `crate::template::TemplateRenderer` unchanged; the core's `CoreError`
//! maps into `AppError` via the `From` impl in `crate::error`.

pub use noetl_orchestrate_core::template::TemplateRenderer;
