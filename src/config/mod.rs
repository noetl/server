//! Configuration module for the NoETL Control Plane server.
//!
//! This module provides configuration loading from environment variables
//! using the `envy` crate for type-safe environment variable parsing.

mod app;
pub mod database;

pub use app::{AppConfig, StateBuildMode, StateBuilder};
pub use database::{DatabaseConfig, ShardConnection, ShardConnectionError, ShardingConfig};
