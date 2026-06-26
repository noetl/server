//! NoETL Control Plane Library
//!
//! This crate provides the control plane server for NoETL, handling:
//!
//! - **Workflow Orchestration**: Execute playbooks and manage workflow state
//! - **Catalog Management**: Register and retrieve playbooks, tools, and resources
//! - **Credential Management**: Securely store and retrieve encrypted credentials
//! - **Event Processing**: Handle worker events and drive workflow execution
//! - **Execution Management**: Track and manage playbook executions
//!
//! ## Architecture
//!
//! The control plane follows an event-sourcing architecture where all state
//! is derived from events stored in PostgreSQL. NATS JetStream is used for
//! command notifications to workers.
//!
//! ## Modules
//!
//! - [`config`]: Configuration loading from environment variables
//! - [`db`]: Database connectivity and models
//! - [`error`]: Custom error types with Axum integration
//! - [`handlers`]: HTTP route handlers
//! - [`state`]: Shared application state
//!
//! ## Example
//!
//! ```ignore
//! use noetl_control_plane::{
//!     config::{AppConfig, DatabaseConfig},
//!     db::create_pool,
//!     state::AppState,
//! };
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     let app_config = AppConfig::from_env()?;
//!     let db_config = DatabaseConfig::from_env()?;
//!     let db_pool = create_pool(&db_config).await?;
//!     // Single-pool fallback shim — production main.rs builds
//!     // the DbPoolMap from ShardingConfig::from_env() for the
//!     // sharded path; test / example code uses new_legacy.
//!     let state = AppState::new_legacy(db_pool, app_config, None);
//!     // ... build and run server
//!     Ok(())
//! }
//! ```

pub mod affinity;
pub mod coherence;
pub mod config;
pub mod crypto;
pub mod db;
pub mod engine;
pub mod error;
pub mod handlers;
pub mod metrics;
pub mod nats;
pub mod playbook;
pub mod result_ext;
pub mod sanitize;
pub mod secrets;
pub mod services;
pub mod sharding;
pub mod system_plugins;
pub mod tls;
pub mod snowflake;
pub mod state;
pub mod template;

pub use error::{AppError, AppResult};
pub use result_ext::ResultExt;

/// Whether the model/dataset/eval/release registry (noetl/ai-meta#146 G3) is
/// enabled. Default off: a truthy `NOETL_REGISTRY_ENABLED`
/// (`1`/`true`/`yes`/`on`, case-insensitive) creates the `noetl.registry` table
/// at startup and mounts the `/api/internal/registry/*` routes. Off → no schema
/// change, no routes (the additive default-off contract).
pub fn registry_enabled() -> bool {
    matches!(
        std::env::var("NOETL_REGISTRY_ENABLED")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}
