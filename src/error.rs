//! Error types for the NoETL Control Plane server.
//!
//! This module provides custom error types that implement `IntoResponse`
//! for seamless integration with Axum handlers.

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::json;
use thiserror::Error;

/// Application-level errors for the control plane.
#[derive(Error, Debug)]
pub enum AppError {
    /// Database error
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),

    /// Not found error
    #[error("Resource not found: {0}")]
    NotFound(String),

    /// Validation error
    #[error("Validation error: {0}")]
    Validation(String),

    /// Authentication error
    #[error("Authentication error: {0}")]
    Auth(String),

    /// Authorization error
    #[error("Authorization error: {0}")]
    Forbidden(String),

    /// Conflict error (e.g., duplicate resource)
    #[error("Conflict: {0}")]
    Conflict(String),

    /// Bad request error
    #[error("Bad request: {0}")]
    BadRequest(String),

    /// Internal server error
    #[error("Internal error: {0}")]
    Internal(String),

    /// Configuration error
    #[error("Configuration error: {0}")]
    Config(String),

    /// NATS messaging error
    #[error("NATS error: {0}")]
    Nats(String),

    /// Serialization error
    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Template rendering error
    #[error("Template error: {0}")]
    Template(String),

    /// Encryption error
    #[error("Encryption error: {0}")]
    Encryption(String),

    // NOTE: the `From<noetl_orchestrate_core::error::CoreError>` impl below maps
    // the drive core's errors into these variants at the boundary
    // (noetl/ai-meta#108).

    /// External service error
    #[error("External service error: {0}")]
    ExternalService(String),

    /// Parse error (YAML, JSON, etc.)
    #[error("Parse error: {0}")]
    Parse(String),

    /// Secrets Wallet Phase 6c — residency policy violation: a server
    /// in one region attempted to resolve a keychain entry whose
    /// `residency: strict` policy region-locks it elsewhere.  Surfaces
    /// to operators as HTTP 403 with a clear "credential X is
    /// region-locked to Y; this server is in Z" message that NEVER
    /// includes the value itself.
    #[error(
        "Residency violation: credential '{credential}' is region-locked to '{entry_region}'; this server is in '{server_region}'"
    )]
    ResidencyViolation {
        credential: String,
        entry_region: String,
        server_region: String,
    },

    /// Secrets Wallet Phase 6e — cross-region broker is configured for
    /// the credential's home region but unreachable / returned a non-2xx /
    /// produced a malformed envelope.  HTTP 502 to the caller so they can
    /// distinguish "policy says no" (403 from `ResidencyViolation`) from
    /// "policy says yes via broker, but the broker is down" (transient).
    ///
    /// `cause` is a free-text reason — never a structured error chain
    /// (we don't want `#[source]`-style trait-object plumbing inside an
    /// HTTP-bounded error).
    #[error("Cross-region broker {broker_url} unreachable: {cause}")]
    CrossRegionUnreachable { broker_url: String, cause: String },
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error_message) = match &self {
            AppError::Database(e) => {
                tracing::error!(error = %e, "Database error");
                (StatusCode::INTERNAL_SERVER_ERROR, self.to_string())
            }
            AppError::NotFound(msg) => (StatusCode::NOT_FOUND, msg.clone()),
            AppError::Validation(msg) => (StatusCode::UNPROCESSABLE_ENTITY, msg.clone()),
            AppError::Auth(msg) => (StatusCode::UNAUTHORIZED, msg.clone()),
            AppError::Forbidden(msg) => (StatusCode::FORBIDDEN, msg.clone()),
            AppError::Conflict(msg) => (StatusCode::CONFLICT, msg.clone()),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            AppError::Internal(msg) => {
                tracing::error!(error = %msg, "Internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, msg.clone())
            }
            AppError::Config(msg) => {
                tracing::error!(error = %msg, "Configuration error");
                (StatusCode::INTERNAL_SERVER_ERROR, msg.clone())
            }
            AppError::Nats(msg) => {
                tracing::error!(error = %msg, "NATS error");
                (StatusCode::SERVICE_UNAVAILABLE, msg.clone())
            }
            AppError::Serialization(e) => {
                tracing::error!(error = %e, "Serialization error");
                (StatusCode::INTERNAL_SERVER_ERROR, self.to_string())
            }
            AppError::Template(msg) => {
                tracing::error!(error = %msg, "Template error");
                (StatusCode::INTERNAL_SERVER_ERROR, msg.clone())
            }
            AppError::Encryption(msg) => {
                tracing::error!(error = %msg, "Encryption error");
                (StatusCode::INTERNAL_SERVER_ERROR, msg.clone())
            }
            AppError::ExternalService(msg) => {
                tracing::warn!(error = %msg, "External service error");
                (StatusCode::BAD_GATEWAY, msg.clone())
            }
            AppError::Parse(msg) => {
                tracing::error!(error = %msg, "Parse error");
                (StatusCode::BAD_REQUEST, msg.clone())
            }
            AppError::ResidencyViolation { .. } => {
                tracing::warn!(error = %self, "Residency violation");
                (StatusCode::FORBIDDEN, self.to_string())
            }
            AppError::CrossRegionUnreachable { .. } => {
                tracing::warn!(error = %self, "Cross-region broker unreachable");
                (StatusCode::BAD_GATEWAY, self.to_string())
            }
        };

        let body = Json(json!({
            "error": error_message,
            "status": status.as_u16()
        }));

        (status, body).into_response()
    }
}

/// Result type alias using AppError.
pub type AppResult<T> = Result<T, AppError>;

impl From<anyhow::Error> for AppError {
    fn from(err: anyhow::Error) -> Self {
        AppError::Internal(err.to_string())
    }
}

impl From<envy::Error> for AppError {
    fn from(err: envy::Error) -> Self {
        AppError::Config(err.to_string())
    }
}

impl From<crate::snowflake::SnowflakeError> for AppError {
    fn from(err: crate::snowflake::SnowflakeError) -> Self {
        // Snowflake errors are always 500-class: either the
        // system clock is broken (ClockBeforeEpoch), the state
        // mutex was poisoned (a panic happened inside generate(),
        // which shouldn't), or the config wasn't validated at
        // startup (MachineIdOutOfRange — should be impossible
        // here since AppState::new validates).
        AppError::Internal(err.to_string())
    }
}

/// Map the pure drive core's error into the server's `AppError` at the
/// boundary, so call sites that `?` a `CoreResult` keep returning `AppResult`
/// unchanged (noetl/ai-meta#108).
impl From<noetl_orchestrate_core::error::CoreError> for AppError {
    fn from(e: noetl_orchestrate_core::error::CoreError) -> Self {
        match e {
            noetl_orchestrate_core::error::CoreError::Template(msg) => AppError::Template(msg),
            noetl_orchestrate_core::error::CoreError::Validation(msg) => AppError::Validation(msg),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_not_found_error() {
        let err = AppError::NotFound("User not found".to_string());
        assert_eq!(err.to_string(), "Resource not found: User not found");
    }

    #[test]
    fn test_validation_error() {
        let err = AppError::Validation("Invalid email".to_string());
        assert_eq!(err.to_string(), "Validation error: Invalid email");
    }
}
