//! HTTP handlers for the NoETL Control Plane API.
//!
//! This module contains all route handlers organized by domain.

pub mod auth;
pub mod auth_verify;
pub mod catalog;
pub mod cells;
pub mod container_callback;
pub mod credentials;
pub mod cross_region;
pub mod dashboard;
pub mod database;
pub mod ehdb;
pub mod ehdb_eventlog_mirror;
pub mod ehdb_eventlog_mirror_queue;
pub mod ehdb_parity;
pub mod event_write;
pub mod events;
pub mod execute;
pub mod executions;
pub mod health;
pub mod ingress;
pub mod internal;
pub mod keychain;
pub mod nonconvergence_sweep;
pub mod objects;
pub mod orphan_sweep;
pub mod plugins;
pub mod registry;
pub mod replay;
pub mod result_store;
pub mod result_tier;
pub mod runtime;
pub mod secret_audit;
pub mod sharding;
pub mod sink_state;
pub mod subscription;
pub mod system;
pub mod variables;
pub mod wallet_rotate;

pub use events::{get_command, handle_event};
pub use execute::{execute, execute_batch};
pub use health::{api_health, health_check};
