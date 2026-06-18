//! The core's own error type — no dependency on the server's `AppError`, so the
//! crate stays runtime-free.  The server maps `CoreError` into its `AppError` at
//! the boundary via a `From` impl.

/// Errors produced by the orchestrator drive core.
#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    /// Template parse / render / condition-evaluation failure.
    #[error("Template error: {0}")]
    Template(String),

    /// Invalid playbook shape / drive precondition (e.g. a malformed loop or
    /// command spec).  Maps to the server's `AppError::Validation`.
    #[error("Validation error: {0}")]
    Validation(String),
}

/// Result alias for the core.
pub type CoreResult<T> = Result<T, CoreError>;
