//! Azure AAD client-credentials dynamic-secret provider
//! (Secrets Wallet Phase 6d.3, noetl/ai-meta#61).
//!
//! Mints short-lived OAuth2 access tokens against Azure Active Directory
//! using the `client_credentials` grant.  This is the off-cluster
//! (non-IMDS) path: a deployment running outside AKS — on a different
//! cloud, on-prem, or a worker box — that needs to call Azure APIs
//! authenticates via a service principal triple
//! (`AZURE_TENANT_ID` / `AZURE_CLIENT_ID` / `AZURE_CLIENT_SECRET`).
//!
//! For in-cluster AKS deployments use the existing `azure` (Key Vault)
//! provider which already does IMDS Managed Identity.  This provider
//! does not deduplicate Key Vault access; it's a generic AAD token
//! source — pass the resulting bearer to whatever Azure API the playbook
//! step calls.
//!
//! ## Reference shape
//!
//! `[<tenant>:]<scope>`
//!
//! - `<scope>` — the OAuth2 v2.0 scope, typically
//!   `https://graph.microsoft.com/.default` or
//!   `https://vault.azure.net/.default`.
//! - `<tenant>:` prefix — override the default tenant (rare; most
//!   deployments authenticate against a single tenant).
//!
//! Empty ref → falls back to env (`NOETL_AZURE_OAUTH_SCOPE`, default
//! `https://graph.microsoft.com/.default`).
//!
//! ## Returned value
//!
//! `SecretValue.value` is the raw `access_token` (no JSON wrapper —
//! typically used as `Authorization: Bearer <value>`).
//! `SecretValue.expires_at` is `now + expires_in` from AAD's response.

use async_trait::async_trait;
use serde::Deserialize;
use std::time::Duration;

use super::{SecretProvider, SecretRef, SecretValue};
use crate::error::{AppError, AppResult};

/// Stable provider id.
const PROVIDER: &str = "azure_oauth";

const DEFAULT_AAD_HOST: &str = "https://login.microsoftonline.com";
const DEFAULT_SCOPE: &str = "https://graph.microsoft.com/.default";

/// AAD client-credentials backend.
pub struct AzureOAuthProvider {
    http: reqwest::Client,
    /// AAD authority host (override for sovereign clouds, e.g.
    /// `https://login.microsoftonline.us` for Azure Gov).
    aad_host: String,
    tenant_id: String,
    client_id: String,
    client_secret: String,
    default_scope: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    /// AAD returns the lifetime in seconds-from-now (integer).
    expires_in: u64,
    /// Optional — AAD echoes the granted token type, almost always
    /// `Bearer`.  We don't use it, but accept it so the deserializer
    /// doesn't fail on tokens that include the field.
    #[serde(default)]
    #[allow(dead_code)]
    token_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedRef {
    tenant: Option<String>,
    scope: Option<String>,
}

fn parse_ref(raw: &str) -> ParsedRef {
    let raw = raw.trim();
    if raw.is_empty() {
        return ParsedRef {
            tenant: None,
            scope: None,
        };
    }
    // Tenants are GUIDs or `<name>.onmicrosoft.com`.  A `:` in the ref
    // delimits the (optional) tenant from the scope.  Scopes always
    // contain `/` or `https://`, so the `:` in `https://` won't be
    // misparsed — we split on the FIRST `:` and only treat the prefix
    // as a tenant if it doesn't look like a URL scheme.
    if let Some((maybe_tenant, scope)) = raw.split_once(':') {
        if !maybe_tenant.is_empty()
            && !scope.is_empty()
            && !maybe_tenant.contains('/')
            && !maybe_tenant.eq_ignore_ascii_case("https")
            && !maybe_tenant.eq_ignore_ascii_case("http")
        {
            return ParsedRef {
                tenant: Some(maybe_tenant.to_string()),
                scope: Some(scope.to_string()),
            };
        }
    }
    ParsedRef {
        tenant: None,
        scope: Some(raw.to_string()),
    }
}

impl AzureOAuthProvider {
    /// Resolve config from the environment.
    ///
    /// Required env vars:
    ///
    /// - `AZURE_TENANT_ID` — directory id (GUID or `*.onmicrosoft.com`).
    /// - `AZURE_CLIENT_ID` — app registration / service principal id.
    /// - `AZURE_CLIENT_SECRET` — service principal secret.
    ///
    /// Optional env vars:
    ///
    /// - `NOETL_AZURE_OAUTH_SCOPE` — default scope when the ref doesn't
    ///   supply one (default `https://graph.microsoft.com/.default`).
    /// - `NOETL_AZURE_AAD_HOST` — AAD authority host (sovereign clouds:
    ///   `https://login.microsoftonline.us` for Gov,
    ///   `https://login.chinacloudapi.cn` for China).
    pub fn from_env() -> AppResult<Self> {
        let tenant_id = std::env::var("AZURE_TENANT_ID").map_err(|_| {
            AppError::Config(
                "azure_oauth: AZURE_TENANT_ID is not set (required for the \
                 `azure_oauth` secret provider)"
                    .to_string(),
            )
        })?;
        let client_id = std::env::var("AZURE_CLIENT_ID").map_err(|_| {
            AppError::Config(
                "azure_oauth: AZURE_CLIENT_ID is not set (required for the \
                 `azure_oauth` secret provider)"
                    .to_string(),
            )
        })?;
        let client_secret = std::env::var("AZURE_CLIENT_SECRET").map_err(|_| {
            AppError::Config(
                "azure_oauth: AZURE_CLIENT_SECRET is not set (required for the \
                 `azure_oauth` secret provider)"
                    .to_string(),
            )
        })?;
        let default_scope = std::env::var("NOETL_AZURE_OAUTH_SCOPE")
            .unwrap_or_else(|_| DEFAULT_SCOPE.to_string());
        let aad_host = std::env::var("NOETL_AZURE_AAD_HOST")
            .unwrap_or_else(|_| DEFAULT_AAD_HOST.to_string());
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| AppError::Config(format!("azure_oauth: http client build failed: {e}")))?;
        Ok(Self {
            http,
            aad_host,
            tenant_id,
            client_id,
            client_secret,
            default_scope,
        })
    }

    /// Build the AAD v2.0 token endpoint URL.
    fn token_url_for(&self, tenant: &str) -> String {
        format!("{host}/{tenant}/oauth2/v2.0/token", host = self.aad_host)
    }

    /// Build the form-urlencoded request body.
    fn build_body(client_id: &str, client_secret: &str, scope: &str) -> String {
        format!(
            "grant_type=client_credentials\
             &client_id={cid}\
             &client_secret={cs}\
             &scope={scope}",
            cid = percent_encode(client_id),
            cs = percent_encode(client_secret),
            scope = percent_encode(scope),
        )
    }

    /// Compute `expires_at` from AAD's `expires_in` seconds-from-now.
    fn compute_expires_at(
        expires_in_secs: u64,
        now: chrono::DateTime<chrono::Utc>,
    ) -> chrono::DateTime<chrono::Utc> {
        // i64::try_from is safe up to 2^63-1 (~292 billion years); a u64
        // expires_in that doesn't fit is broken input from AAD — defensive
        // clamp at i64::MAX so the chrono math stays well-defined.
        let secs = i64::try_from(expires_in_secs).unwrap_or(i64::MAX);
        now + chrono::Duration::seconds(secs)
    }
}

#[async_trait]
impl SecretProvider for AzureOAuthProvider {
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    async fn fetch(&self, secret: &SecretRef) -> AppResult<SecretValue> {
        let parsed = parse_ref(&secret.name);
        let tenant = parsed.tenant.unwrap_or_else(|| self.tenant_id.clone());
        let scope = parsed.scope.unwrap_or_else(|| self.default_scope.clone());

        let url = self.token_url_for(&tenant);
        let body = Self::build_body(&self.client_id, &self.client_secret, &scope);

        let resp = self
            .http
            .post(&url)
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| AppError::Internal(format!("azure_oauth: POST {url} failed: {e}")))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| AppError::Internal(format!("azure_oauth: read body failed: {e}")))?;
        if !status.is_success() {
            return Err(AppError::Internal(format!(
                "azure_oauth: AAD returned HTTP {status}: {text}"
            )));
        }

        let parsed: TokenResponse = serde_json::from_str(&text).map_err(|e| {
            AppError::Internal(format!("azure_oauth: parse AAD response: {e}"))
        })?;
        let expires_at = Self::compute_expires_at(parsed.expires_in, chrono::Utc::now());

        Ok(SecretValue {
            value: parsed.access_token,
            // AAD doesn't return a version id; surface None so the cache
            // layer doesn't pretend otherwise.
            version: None,
            expires_at: Some(expires_at),
        })
    }
}

/// Minimal application/x-www-form-urlencoded percent encoding (same
/// rules as `aws_sts`).  Encodes every byte outside the unreserved set
/// `[A-Za-z0-9-._~]`.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // parse_ref
    // -----------------------------------------------------------------

    #[test]
    fn parse_ref_empty() {
        let p = parse_ref("");
        assert_eq!(
            p,
            ParsedRef {
                tenant: None,
                scope: None,
            }
        );
    }

    #[test]
    fn parse_ref_bare_scope_url() {
        let p = parse_ref("https://graph.microsoft.com/.default");
        // The `https:` prefix is NOT a tenant; the whole ref is the scope.
        assert_eq!(
            p,
            ParsedRef {
                tenant: None,
                scope: Some("https://graph.microsoft.com/.default".to_string()),
            }
        );
    }

    #[test]
    fn parse_ref_tenant_prefix_splits() {
        let p = parse_ref("contoso.onmicrosoft.com:https://vault.azure.net/.default");
        assert_eq!(
            p,
            ParsedRef {
                tenant: Some("contoso.onmicrosoft.com".to_string()),
                scope: Some("https://vault.azure.net/.default".to_string()),
            }
        );
    }

    #[test]
    fn parse_ref_guid_tenant_prefix() {
        let p = parse_ref("11111111-2222-3333-4444-555555555555:https://api.fabrikam.com/.default");
        assert_eq!(
            p.tenant.as_deref(),
            Some("11111111-2222-3333-4444-555555555555")
        );
        assert_eq!(
            p.scope.as_deref(),
            Some("https://api.fabrikam.com/.default")
        );
    }

    // -----------------------------------------------------------------
    // token_url_for / build_body
    // -----------------------------------------------------------------

    fn test_provider() -> AzureOAuthProvider {
        AzureOAuthProvider {
            http: reqwest::Client::new(),
            aad_host: DEFAULT_AAD_HOST.to_string(),
            tenant_id: "default-tenant".to_string(),
            client_id: "ID".to_string(),
            client_secret: "SECRET".to_string(),
            default_scope: DEFAULT_SCOPE.to_string(),
        }
    }

    #[test]
    fn token_url_for_uses_tenant() {
        let p = test_provider();
        assert_eq!(
            p.token_url_for("contoso.onmicrosoft.com"),
            "https://login.microsoftonline.com/contoso.onmicrosoft.com/oauth2/v2.0/token"
        );
    }

    #[test]
    fn token_url_for_honours_sovereign_host() {
        let mut p = test_provider();
        p.aad_host = "https://login.microsoftonline.us".to_string();
        assert_eq!(
            p.token_url_for("11111111-2222-3333-4444-555555555555"),
            "https://login.microsoftonline.us/11111111-2222-3333-4444-555555555555/oauth2/v2.0/token"
        );
    }

    #[test]
    fn build_body_form_urlencoded_shape() {
        let body = AzureOAuthProvider::build_body(
            "client-app-id",
            "very/secret*value!",
            "https://graph.microsoft.com/.default",
        );
        assert!(body.contains("grant_type=client_credentials"));
        // client_id is unreserved-safe.
        assert!(body.contains("client_id=client-app-id"));
        // client_secret's specials get escaped.
        assert!(body.contains("client_secret=very%2Fsecret%2Avalue%21"));
        // Scope's `/` and `:` get escaped.
        assert!(body.contains("scope=https%3A%2F%2Fgraph.microsoft.com%2F.default"));
    }

    // -----------------------------------------------------------------
    // expires_at math
    // -----------------------------------------------------------------

    #[test]
    fn compute_expires_at_adds_seconds() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-06-07T03:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let at = AzureOAuthProvider::compute_expires_at(3600, now);
        assert_eq!(at - now, chrono::Duration::seconds(3600));
    }

    #[test]
    fn compute_expires_at_handles_zero() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-06-07T03:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let at = AzureOAuthProvider::compute_expires_at(0, now);
        assert_eq!(at, now);
    }

    // -----------------------------------------------------------------
    // Response parser
    // -----------------------------------------------------------------

    #[test]
    fn response_parses_aad_token() {
        let body = r#"{
            "token_type": "Bearer",
            "expires_in": 3599,
            "ext_expires_in": 3599,
            "access_token": "eyJ0eXAiOi.AAD.token"
        }"#;
        let parsed: TokenResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.access_token, "eyJ0eXAiOi.AAD.token");
        assert_eq!(parsed.expires_in, 3599);
    }

    #[test]
    fn response_parses_minimal_shape_without_token_type() {
        // AAD always returns token_type, but tolerate its absence so the
        // parser doesn't fail on a paranoid mock.
        let body = r#"{
            "expires_in": 3600,
            "access_token": "minimal-token"
        }"#;
        let parsed: TokenResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.access_token, "minimal-token");
        assert_eq!(parsed.expires_in, 3600);
    }

    #[test]
    fn response_parse_fails_on_missing_token() {
        let body = r#"{ "expires_in": 3600 }"#;
        let result: Result<TokenResponse, _> = serde_json::from_str(body);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------
    // percent_encode
    // -----------------------------------------------------------------

    #[test]
    fn percent_encode_preserves_unreserved() {
        assert_eq!(percent_encode("ABCabc123-_.~"), "ABCabc123-_.~");
    }

    #[test]
    fn percent_encode_escapes_specials() {
        assert_eq!(percent_encode("https://x/.default"), "https%3A%2F%2Fx%2F.default");
    }
}
