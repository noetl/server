//! Azure Key Vault provider (Secrets Wallet Phase 3.x, noetl/ai-meta#61).
//!
//! Resolves secret references against Azure Key Vault via the REST endpoint
//! `https://<vault>.vault.azure.net/secrets/<name>[/<version>]?api-version=7.4`,
//! authenticating with an OAuth2 access token obtained from one of:
//!
//! 1. The **Azure Instance Metadata Service** (IMDS, used by Managed Identity
//!    on AKS / VMs) at `http://169.254.169.254/metadata/identity/oauth2/token`
//!    — the platform mints the token, no client secret on the worker.
//! 2. **`AZURE_KEYVAULT_TOKEN`** — a pre-fetched bearer token (test / mock).
//!
//! The AAD client-credentials flow (tenant + client id + client secret) is a
//! clearly-scoped follow-up; for in-cluster deployments Managed Identity is
//! the preferred path.
//!
//! ## Reference shape
//!
//! `[<vault>/]<secret-name>[#<version>]`
//!
//! - bare `<secret-name>` ⇒ use the default vault from `AZURE_KEYVAULT_VAULT`.
//! - `<vault>/<secret-name>` ⇒ override the vault for this lookup (just the
//!   vault short name, e.g. `prod-eu`; the `.vault.azure.net` suffix is
//!   appended).
//! - `#<version>` ⇒ specific version (otherwise the latest is returned).

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::Mutex;

use super::{SecretProvider, SecretRef, SecretValue};
use crate::error::{AppError, AppResult};

/// Stable provider id.
const PROVIDER: &str = "azure";

const DEFAULT_API_VERSION: &str = "7.4";
const DEFAULT_IMDS_URL: &str = "http://169.254.169.254/metadata/identity/oauth2/token";
const KV_RESOURCE: &str = "https://vault.azure.net";

/// Azure Key Vault backend.
pub struct AzureKeyVaultProvider {
    http: reqwest::Client,
    /// Default vault short name (e.g. `prod-eu`); the suffix is the vault DNS.
    default_vault: Option<String>,
    /// DNS suffix — `.vault.azure.net` (commercial) or `.vault.usgovcloudapi.net`
    /// (US gov) or `.vault.azure.cn` (China).
    vault_dns_suffix: String,
    api_version: String,
    imds_token_url: String,
    /// When set: a pre-fetched bearer token used in lieu of IMDS (tests).
    static_token: Option<String>,
    token: Arc<Mutex<Option<CachedToken>>>,
}

struct CachedToken {
    value: String,
    expires_at: Instant,
}

/// IMDS token endpoint shape.  `expires_in` is a JSON string in IMDS responses.
#[derive(Deserialize)]
struct ImdsToken {
    access_token: String,
    #[serde(default)]
    expires_in: Option<String>,
}

/// Key Vault `GetSecret` response shape (only the fields we care about).
#[derive(Deserialize)]
struct GetSecretResponse {
    value: String,
    /// `https://<vault>.vault.azure.net/secrets/<name>/<version>`
    #[serde(default)]
    id: Option<String>,
}

/// Parsed Key Vault reference: vault override, secret name, optional version.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedRef {
    vault: Option<String>,
    secret_name: String,
    version: Option<String>,
}

fn parse_ref(raw: &str) -> AppResult<ParsedRef> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(AppError::Config(
            "azure secret ref: empty reference".to_string(),
        ));
    }
    // Optional `#<version>` suffix.
    let (rest, version) = match raw.split_once('#') {
        Some((r, v)) if !v.is_empty() => (r, Some(v.to_string())),
        Some(_) => {
            return Err(AppError::Config(
                "azure secret ref: empty version after '#'".to_string(),
            ));
        }
        None => (raw, None),
    };
    // Optional `<vault>/<secret-name>` prefix.
    let (vault, secret_name) = match rest.split_once('/') {
        Some((v, s)) if !v.is_empty() && !s.is_empty() => (Some(v.to_string()), s.to_string()),
        Some(_) => {
            return Err(AppError::Config(
                "azure secret ref: '<vault>/<secret-name>' shape has an empty side".to_string(),
            ));
        }
        None => (None, rest.to_string()),
    };
    Ok(ParsedRef {
        vault,
        secret_name,
        version,
    })
}

impl AzureKeyVaultProvider {
    /// Resolve config from the environment.
    pub fn from_env() -> AppResult<Self> {
        let default_vault = std::env::var("AZURE_KEYVAULT_VAULT")
            .ok()
            .filter(|s| !s.is_empty());
        let vault_dns_suffix = std::env::var("NOETL_AZURE_KEYVAULT_DNS_SUFFIX")
            .unwrap_or_else(|_| "vault.azure.net".to_string());
        let api_version = std::env::var("NOETL_AZURE_KEYVAULT_API_VERSION")
            .unwrap_or_else(|_| DEFAULT_API_VERSION.to_string());
        let imds_token_url = std::env::var("NOETL_AZURE_IMDS_TOKEN_URL")
            .unwrap_or_else(|_| DEFAULT_IMDS_URL.to_string());
        let static_token = std::env::var("AZURE_KEYVAULT_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        Ok(Self {
            http: reqwest::Client::builder()
                .build()
                .map_err(|e| AppError::Config(format!("azure secret provider: build client: {e}")))?,
            default_vault,
            vault_dns_suffix: vault_dns_suffix.trim_matches('.').to_string(),
            api_version,
            imds_token_url,
            static_token,
            token: Arc::new(Mutex::new(None)),
        })
    }

    fn vault_base(&self, vault: &str) -> String {
        format!("https://{}.{}", vault, self.vault_dns_suffix)
    }

    /// Fetch + cache the bearer token (static if set, IMDS otherwise).
    async fn get_token(&self) -> AppResult<String> {
        if let Some(t) = &self.static_token {
            return Ok(t.clone());
        }
        let mut guard = self.token.lock().await;
        if let Some(cached) = guard.as_ref() {
            if cached.expires_at > Instant::now() + Duration::from_secs(30) {
                return Ok(cached.value.clone());
            }
        }
        let resp = self
            .http
            .get(&self.imds_token_url)
            .query(&[("api-version", "2018-02-01"), ("resource", KV_RESOURCE)])
            .header("Metadata", "true")
            .send()
            .await
            .map_err(|e| {
                AppError::Config(format!("azure secret provider: IMDS token request: {e}"))
            })?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AppError::Config(format!(
                "azure secret provider: IMDS returned {status}: {}",
                text.chars().take(300).collect::<String>()
            )));
        }
        let parsed: ImdsToken = serde_json::from_str(&text).map_err(|e| {
            AppError::Config(format!("azure secret provider: decode IMDS response: {e}"))
        })?;
        let expires_in = parsed
            .expires_in
            .as_deref()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(60);
        let cached = CachedToken {
            value: parsed.access_token.clone(),
            expires_at: Instant::now() + Duration::from_secs(expires_in),
        };
        let value = cached.value.clone();
        *guard = Some(cached);
        Ok(value)
    }
}

#[async_trait]
impl SecretProvider for AzureKeyVaultProvider {
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    async fn fetch(&self, secret: &SecretRef) -> AppResult<SecretValue> {
        let parsed = parse_ref(&secret.name)?;
        let vault = parsed
            .vault
            .clone()
            .or_else(|| secret.project.clone())
            .or_else(|| self.default_vault.clone())
            .ok_or_else(|| {
                AppError::Config(
                    "azure secret provider: no vault (set AZURE_KEYVAULT_VAULT or prefix the \
                     ref with `<vault>/`)"
                        .to_string(),
                )
            })?;
        let version = parsed
            .version
            .clone()
            .or_else(|| secret.version.clone())
            .unwrap_or_default();
        // `/secrets/<name>` returns the latest version; `/<version>` pins it.
        let path = if version.is_empty() {
            format!("/secrets/{}", parsed.secret_name)
        } else {
            format!("/secrets/{}/{}", parsed.secret_name, version)
        };
        let url = format!(
            "{}{}?api-version={}",
            self.vault_base(&vault),
            path,
            self.api_version
        );

        let token = self.get_token().await?;
        let resp = self
            .http
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| {
                AppError::Config(format!(
                    "azure secret provider: GET {url} for '{}': {e}",
                    parsed.secret_name
                ))
            })?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AppError::Config(format!(
                "azure secret provider: GET '{}' returned {status}: {}",
                parsed.secret_name,
                text.chars().take(400).collect::<String>()
            )));
        }
        let body: GetSecretResponse = serde_json::from_str(&text).map_err(|e| {
            AppError::Config(format!(
                "azure secret provider: decode GetSecret response for '{}': {e}",
                parsed.secret_name
            ))
        })?;
        // Extract the concrete version from the response id when one is
        // present — KV returns `.../secrets/<name>/<version>`.
        let resolved_version = body.id.as_deref().and_then(|id| {
            id.rsplit('/').next().map(|s| s.to_string()).filter(|s| !s.is_empty())
        });
        Ok(SecretValue {
            value: body.value,
            version: resolved_version,
            expires_at: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ref_bare_name() {
        let p = parse_ref("duffel-token").unwrap();
        assert_eq!(
            p,
            ParsedRef {
                vault: None,
                secret_name: "duffel-token".into(),
                version: None
            }
        );
    }

    #[test]
    fn parse_ref_vault_and_name() {
        let p = parse_ref("prod-eu/duffel-token").unwrap();
        assert_eq!(p.vault.as_deref(), Some("prod-eu"));
        assert_eq!(p.secret_name, "duffel-token");
        assert!(p.version.is_none());
    }

    #[test]
    fn parse_ref_with_version() {
        let p = parse_ref("duffel-token#abc123").unwrap();
        assert_eq!(p.version.as_deref(), Some("abc123"));
        assert_eq!(p.secret_name, "duffel-token");
    }

    #[test]
    fn parse_ref_vault_name_and_version() {
        let p = parse_ref("prod-eu/duffel-token#abc123").unwrap();
        assert_eq!(p.vault.as_deref(), Some("prod-eu"));
        assert_eq!(p.secret_name, "duffel-token");
        assert_eq!(p.version.as_deref(), Some("abc123"));
    }

    #[test]
    fn parse_ref_rejects_empty_sides() {
        assert!(parse_ref("").is_err());
        assert!(parse_ref("/foo").is_err());
        assert!(parse_ref("foo/").is_err());
        assert!(parse_ref("foo#").is_err());
    }

    #[test]
    fn vault_base_appends_dns_suffix() {
        let mut p = AzureKeyVaultProvider::from_env().unwrap_or_else(|_| {
            // from_env should never fail (all vars are optional); guard for
            // unexpected CI env shape by constructing directly.
            AzureKeyVaultProvider {
                http: reqwest::Client::new(),
                default_vault: None,
                vault_dns_suffix: "vault.azure.net".into(),
                api_version: DEFAULT_API_VERSION.into(),
                imds_token_url: DEFAULT_IMDS_URL.into(),
                static_token: None,
                token: Arc::new(Mutex::new(None)),
            }
        });
        p.vault_dns_suffix = "vault.azure.net".into();
        assert_eq!(p.vault_base("prod-eu"), "https://prod-eu.vault.azure.net");
        p.vault_dns_suffix = "vault.azure.cn".into();
        assert_eq!(p.vault_base("prod-cn"), "https://prod-cn.vault.azure.cn");
    }

    #[tokio::test]
    async fn get_token_returns_static_when_set() {
        let p = AzureKeyVaultProvider {
            http: reqwest::Client::new(),
            default_vault: None,
            vault_dns_suffix: "vault.azure.net".into(),
            api_version: DEFAULT_API_VERSION.into(),
            imds_token_url: DEFAULT_IMDS_URL.into(),
            static_token: Some("test-token".into()),
            token: Arc::new(Mutex::new(None)),
        };
        assert_eq!(p.get_token().await.unwrap(), "test-token");
    }
}
