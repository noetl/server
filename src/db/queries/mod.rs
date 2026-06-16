//! Database queries for the NoETL Control Plane.
//!
//! This module contains database query functions organized by domain.

pub mod catalog;
pub mod credential;
pub mod event;
pub mod event_outbox;
pub mod keychain;
pub mod result_store;
pub mod secret_audit;
pub mod subscription_dedup;
pub mod wallet_rotate;
