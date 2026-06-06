//! Server-side secret-provider clients (Secrets Wallet Phase 3b,
//! noetl/ai-meta#61).
//!
//! These resolve a secret **reference** to its value from an external secret
//! manager. They live on the server (not the worker) so the keychain/credential
//! resolver can fetch a secret on a cache miss, cache it envelope-encrypted
//! (Phase 1 [`crate::crypto::EnvelopeCipher`]), and hand it back masked — the
//! raw value never enters the workflow data flow. This is the resolution
//! engine; the resolver hook that calls it lands in a later round.
//!
//! [`GcpSecretManager`] is the first backend (next to the existing
//! [`crate::crypto::GcpKms`], which it shares the Workload-Identity token
//! pattern with). AWS Secrets Manager, Azure Key Vault, HashiCorp Vault, and
//! Kubernetes Secrets follow behind the same [`SecretProvider`] trait.

mod gcp;
mod resolver;

pub use gcp::GcpSecretManager;
pub use resolver::resolve_keychain_entry;

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::{AppError, AppResult};

/// A resolved secret plus its provenance.
///
/// `value` is the secret material as a UTF-8 string; `version` is the
/// provider's resolved version identifier when the backend reports one
/// (e.g. the concrete version number behind a `latest` alias).
#[derive(Debug, Clone)]
pub struct SecretValue {
    pub value: String,
    pub version: Option<String>,
}

/// A request to fetch one secret from a provider.
///
/// Fields are provider-agnostic; each backend interprets them:
/// - `name` — the secret id / name, or a fully-qualified resource path.
/// - `project` — GCP project / AWS account / Azure vault / Vault mount.
/// - `version` — version / stage; defaults to the provider's "latest".
#[derive(Debug, Clone)]
pub struct SecretRef {
    pub name: String,
    pub project: Option<String>,
    pub version: Option<String>,
}

/// A backend that resolves [`SecretRef`]s to [`SecretValue`]s.
#[async_trait]
pub trait SecretProvider: Send + Sync {
    /// Stable provider id (`gcp`, `aws`, `azure`, `vault`, `k8s`).
    fn provider(&self) -> &'static str;

    /// Fetch one secret. Implementations never log the resolved value;
    /// callers keep it out of any state-surfacing response (masked at the
    /// boundary per the secrets-and-redaction contract).
    async fn fetch(&self, secret: &SecretRef) -> AppResult<SecretValue>;
}

/// Build a [`SecretProvider`] for a keychain entry's `provider` id.
///
/// Mirrors [`crate::crypto::build_key_manager`]. `gcp` → [`GcpSecretManager`]
/// from ambient config. An unsupported / unset provider returns an error — the
/// R3b resolver treats that as "this entry isn't provider-sourced" and falls
/// through to the credential store. AWS / Azure / Vault / K8s slot in here.
pub fn build_secret_provider(provider: &str) -> AppResult<Arc<dyn SecretProvider>> {
    match provider {
        "gcp" => Ok(Arc::new(GcpSecretManager::from_env()?)),
        other => Err(AppError::Config(format!(
            "unsupported keychain secret provider '{other}' (supported: gcp)"
        ))),
    }
}
