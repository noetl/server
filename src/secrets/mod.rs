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

mod aws;
mod azure;
pub mod dynamic;
mod gcp;
mod k8s;
mod registry;
pub mod residency;
mod resolver;
mod vault;

pub use aws::AwsSmSecretProvider;
pub use azure::AzureKeyVaultProvider;
pub use gcp::GcpSecretManager;
pub use k8s::K8sSecretProvider;
pub use registry::get_provider;
pub use resolver::{resolve_keychain_entry, resolve_keychain_entry_with_meta};
pub use vault::VaultSecretProvider;

use std::sync::{Arc, OnceLock};

use async_trait::async_trait;

use crate::error::{AppError, AppResult};

/// A resolved secret plus its provenance.
///
/// `value` is the secret material as a UTF-8 string; `version` is the
/// provider's resolved version identifier when the backend reports one
/// (e.g. the concrete version number behind a `latest` alias);
/// `expires_at` is Secrets-Wallet Phase 6d — the issuer's reported expiry
/// for short-lived dynamic secrets (AWS STS / AAD bearer / GCP access
/// tokens / OAuth2 access tokens with `expires_in`).  `None` for
/// long-lived API keys whose lifetime is operator-controlled.
#[derive(Debug, Clone, Default)]
pub struct SecretValue {
    pub value: String,
    pub version: Option<String>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A request to fetch one secret from a provider.
///
/// Fields are provider-agnostic; each backend interprets them:
/// - `name` — the secret id / name, or a fully-qualified resource path.
/// - `project` — GCP project / AWS account / Azure vault / Vault mount.
/// - `version` — version / stage; defaults to the provider's "latest".
/// - `region` — Secrets-Wallet Phase 6a: home region of the secret as
///   declared on the [`KeychainDef`] (or filled from `NOETL_SERVER_REGION`
///   as a fallback).  AWS uses it as the regional endpoint host; Azure /
///   Vault use it to route to the per-region cluster / vault; GCP includes
///   it in the resource id.  `None` means the provider falls back to its
///   own default region (back-compat with pre-6a deployments).
#[derive(Debug, Clone, Default)]
pub struct SecretRef {
    pub name: String,
    pub project: Option<String>,
    pub version: Option<String>,
    pub region: Option<String>,
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

/// The server's home region, read once from `NOETL_SERVER_REGION` at process
/// startup.  Empty when the env is unset (legacy mode).
///
/// Used as the fallback for a [`KeychainDef`] that didn't declare its own
/// region — the keychain entry's declared region always wins over this.
/// Phase 6a (residency-aware distributed resolution) — when residency
/// enforcement lands (Phase 6c), this is also the value compared against an
/// entry's `region` to decide whether resolution is allowed.
pub fn server_region() -> &'static str {
    static R: OnceLock<String> = OnceLock::new();
    R.get_or_init(|| std::env::var("NOETL_SERVER_REGION").unwrap_or_default())
        .as_str()
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
        "k8s" | "kubernetes" => Ok(Arc::new(K8sSecretProvider::from_env()?)),
        "vault" => Ok(Arc::new(VaultSecretProvider::from_env()?)),
        "aws" | "aws_sm" => Ok(Arc::new(AwsSmSecretProvider::from_env()?)),
        "azure" | "azure_kv" => Ok(Arc::new(AzureKeyVaultProvider::from_env()?)),
        other => Err(AppError::Config(format!(
            "unsupported keychain secret provider '{other}' (supported: gcp, k8s, vault, aws, azure)"
        ))),
    }
}
