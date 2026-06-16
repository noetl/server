//! Service layer for the NoETL Control Plane.
//!
//! Services encapsulate business logic and coordinate
//! between handlers and database queries.

pub mod catalog;
pub mod credential;
pub mod event;
pub mod event_stream;
pub mod execution;
pub mod internal;
pub mod keychain;
pub mod keychain_refresh;
pub mod orch_snapshot;
pub mod replay;
pub mod result_store;
pub mod runtime;
pub mod secret_audit;
pub mod ui_schema;
pub mod wallet_rotate;

pub use catalog::CatalogService;
pub use credential::CredentialService;
pub use event::EventService;
pub use execution::ExecutionService;
pub use keychain::KeychainService;
pub use replay::ReplayService;
pub use result_store::ResultStoreService;
pub use runtime::RuntimeService;
pub use secret_audit::SecretAuditService;
