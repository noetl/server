//! GCP IAM Credentials `generateAccessToken` dynamic-secret provider
//! (Secrets Wallet Phase 6d.2, noetl/ai-meta#61).
//!
//! Mints short-lived OAuth2 access tokens for a *target* service account
//! by impersonating it via the IAM Credentials API.  The caller's
//! authentication is its own Workload-Identity token read from the GKE
//! metadata server (same source as [`crate::secrets::GcpSecretManager`]
//! and [`crate::crypto::GcpKms`]).
//!
//! ## When to reach for this provider
//!
//! - The keychain needs an access token to call a Google API on behalf of
//!   another service account (cross-project / cross-tenant impersonation).
//! - The credential lifetime is bounded (Phase 6d's `cache_decision` honours
//!   the `expireTime` returned by IAM Credentials).
//! - The caller already has Workload-Identity bindings — no static service
//!   account JSON key on disk.
//!
//! For long-lived secrets stored in Secret Manager use the `gcp` provider.
//!
//! ## Reference shape
//!
//! `<target-sa-email>[#<scope>]`
//!
//! - `<target-sa-email>` — the service account to impersonate
//!   (`projects/-/serviceAccounts/<email>` in the IAM Credentials URL).
//!   Empty ref falls back to `NOETL_GCP_IMPERSONATE_SA`.
//! - `#<scope>` — single OAuth2 scope (default
//!   `https://www.googleapis.com/auth/cloud-platform`).  Multiple scopes
//!   in one ref are not supported in this round; if multi-scope is needed,
//!   open a follow-up sub-issue.
//!
//! ## Returned value
//!
//! `SecretValue.value` is the raw `accessToken` string (no JSON wrapper —
//! the playbook step normally uses it directly as
//! `Authorization: Bearer <value>`).  `SecretValue.expires_at` carries the
//! IAM Credentials-reported `expireTime`.

use async_trait::async_trait;
use serde::Deserialize;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use super::{SecretProvider, SecretRef, SecretValue};
use crate::error::{AppError, AppResult};

/// Stable provider id.
const PROVIDER: &str = "gcp_iam";

const DEFAULT_IAM_CREDENTIALS_ENDPOINT: &str = "https://iamcredentials.googleapis.com/v1";
const DEFAULT_METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";
const DEFAULT_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const DEFAULT_LIFETIME_SECS: u32 = 3600;

/// GCP IAM Credentials backend.
pub struct GcpIamProvider {
    http: reqwest::Client,
    endpoint: String,
    metadata_token_url: String,
    default_target_sa: Option<String>,
    default_lifetime_secs: u32,
    /// Cached Workload-Identity token (caller's identity, NOT the
    /// impersonated SA — that one's the result of the fetch).
    token: Arc<Mutex<Option<CachedToken>>>,
}

struct CachedToken {
    value: String,
    expires_at: Instant,
}

#[derive(Deserialize)]
struct MetadataToken {
    access_token: String,
    expires_in: u64,
}

#[derive(Deserialize)]
struct GenerateAccessTokenResponse {
    #[serde(rename = "accessToken")]
    access_token: String,
    /// RFC3339 timestamp.
    #[serde(rename = "expireTime")]
    expire_time: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedRef {
    target_sa: Option<String>,
    scope: Option<String>,
}

fn parse_ref(raw: &str) -> ParsedRef {
    let raw = raw.trim();
    if raw.is_empty() {
        return ParsedRef {
            target_sa: None,
            scope: None,
        };
    }
    // Split on `#` even when the scope portion is empty — the SA email
    // never contains a `#`, so a bare trailing `#` is user typo we silently
    // strip rather than smuggle into the SA field.
    let (sa, scope) = match raw.split_once('#') {
        Some((s, k)) if !k.is_empty() => (s.trim(), Some(k.trim().to_string())),
        Some((s, _)) => (s.trim(), None),
        None => (raw, None),
    };
    let target_sa = if sa.is_empty() {
        None
    } else {
        Some(sa.to_string())
    };
    ParsedRef { target_sa, scope }
}

impl GcpIamProvider {
    /// Resolve config from the environment.
    ///
    /// Env vars (all optional):
    ///
    /// - `NOETL_GCP_IMPERSONATE_SA` — default target service account email.
    /// - `NOETL_GCP_IAM_CREDENTIALS_ENDPOINT` — endpoint override
    ///   (for tests / private googleapis).
    /// - `NOETL_GCP_METADATA_TOKEN_URL` — metadata-server token URL
    ///   (shared override with [`crate::secrets::GcpSecretManager`]).
    /// - `NOETL_GCP_IAM_LIFETIME_SECS` — requested credential lifetime
    ///   (default 3600).
    pub fn from_env() -> AppResult<Self> {
        let endpoint = std::env::var("NOETL_GCP_IAM_CREDENTIALS_ENDPOINT")
            .unwrap_or_else(|_| DEFAULT_IAM_CREDENTIALS_ENDPOINT.to_string());
        let metadata_token_url = std::env::var("NOETL_GCP_METADATA_TOKEN_URL")
            .unwrap_or_else(|_| DEFAULT_METADATA_TOKEN_URL.to_string());
        let default_target_sa = std::env::var("NOETL_GCP_IMPERSONATE_SA").ok();
        let default_lifetime_secs = std::env::var("NOETL_GCP_IAM_LIFETIME_SECS")
            .ok()
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(DEFAULT_LIFETIME_SECS);
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| AppError::Config(format!("gcp_iam: http client build failed: {e}")))?;
        Ok(Self {
            http,
            endpoint,
            metadata_token_url,
            default_target_sa,
            default_lifetime_secs,
            token: Arc::new(Mutex::new(None)),
        })
    }

    /// Fetch (and cache) the caller's Workload-Identity access token.  Same
    /// shape as [`crate::secrets::GcpSecretManager::access_token`] —
    /// refreshes within ~60s of expiry.
    async fn caller_access_token(&self) -> AppResult<String> {
        let mut guard = self.token.lock().await;
        if let Some(tok) = guard.as_ref() {
            if tok.expires_at > Instant::now() {
                return Ok(tok.value.clone());
            }
        }
        let resp = self
            .http
            .get(&self.metadata_token_url)
            .header("Metadata-Flavor", "Google")
            .send()
            .await
            .map_err(|e| {
                AppError::Internal(format!(
                    "gcp_iam: GET {url} (metadata-server token) failed: {e}",
                    url = self.metadata_token_url
                ))
            })?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(AppError::Internal(format!(
                "gcp_iam: metadata-server token HTTP {status}: {body}"
            )));
        }
        let parsed: MetadataToken = resp
            .json()
            .await
            .map_err(|e| AppError::Internal(format!("gcp_iam: parse metadata token: {e}")))?;
        let expires_at =
            Instant::now() + Duration::from_secs(parsed.expires_in.saturating_sub(60));
        let value = parsed.access_token.clone();
        *guard = Some(CachedToken {
            value: value.clone(),
            expires_at,
        });
        Ok(value)
    }

    /// Build the `generateAccessToken` URL for `target_sa`.
    fn endpoint_for(&self, target_sa: &str) -> String {
        format!(
            "{endpoint}/projects/-/serviceAccounts/{sa}:generateAccessToken",
            endpoint = self.endpoint,
            sa = target_sa,
        )
    }

    /// Build the JSON body that IAM Credentials expects.
    fn build_body(scope: &str, lifetime_secs: u32) -> serde_json::Value {
        serde_json::json!({
            "scope":    [scope],
            "lifetime": format!("{lifetime_secs}s"),
        })
    }
}

#[async_trait]
impl SecretProvider for GcpIamProvider {
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    async fn fetch(&self, secret: &SecretRef) -> AppResult<SecretValue> {
        let parsed = parse_ref(&secret.name);
        let target_sa = parsed
            .target_sa
            .or_else(|| self.default_target_sa.clone())
            .ok_or_else(|| {
                AppError::Config(
                    "gcp_iam: no target service account (set NOETL_GCP_IMPERSONATE_SA \
                     or include the SA email in the keychain ref)"
                        .to_string(),
                )
            })?;
        let scope = parsed.scope.unwrap_or_else(|| DEFAULT_SCOPE.to_string());

        let caller_token = self.caller_access_token().await?;
        let url = self.endpoint_for(&target_sa);
        let body = Self::build_body(&scope, self.default_lifetime_secs);

        let resp = self
            .http
            .post(&url)
            .bearer_auth(caller_token)
            .json(&body)
            .send()
            .await
            .map_err(|e| AppError::Internal(format!("gcp_iam: POST {url} failed: {e}")))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| AppError::Internal(format!("gcp_iam: read body failed: {e}")))?;
        if !status.is_success() {
            return Err(AppError::Internal(format!(
                "gcp_iam: iamcredentials returned HTTP {status}: {text}"
            )));
        }

        let parsed: GenerateAccessTokenResponse = serde_json::from_str(&text)
            .map_err(|e| AppError::Internal(format!("gcp_iam: parse response: {e}")))?;
        let expires_at = chrono::DateTime::parse_from_rfc3339(&parsed.expire_time)
            .map_err(|e| {
                AppError::Internal(format!(
                    "gcp_iam: parse expireTime '{}': {e}",
                    parsed.expire_time
                ))
            })?
            .with_timezone(&chrono::Utc);

        Ok(SecretValue {
            value: parsed.access_token,
            // IAM Credentials doesn't return a stable version id; we surface
            // None here so the cache layer doesn't pretend otherwise.
            version: None,
            expires_at: Some(expires_at),
        })
    }
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
                target_sa: None,
                scope: None,
            }
        );
    }

    #[test]
    fn parse_ref_bare_sa() {
        let p = parse_ref("etl-runner@my-project.iam.gserviceaccount.com");
        assert_eq!(
            p,
            ParsedRef {
                target_sa: Some("etl-runner@my-project.iam.gserviceaccount.com".to_string()),
                scope: None,
            }
        );
    }

    #[test]
    fn parse_ref_with_scope() {
        let p = parse_ref(
            "etl-runner@my-project.iam.gserviceaccount.com#https://www.googleapis.com/auth/bigquery",
        );
        assert_eq!(
            p,
            ParsedRef {
                target_sa: Some("etl-runner@my-project.iam.gserviceaccount.com".to_string()),
                scope: Some("https://www.googleapis.com/auth/bigquery".to_string()),
            }
        );
    }

    #[test]
    fn parse_ref_trailing_hash_is_ignored() {
        // `#` with no scope payload — treat as no scope.
        let p = parse_ref("etl-runner@my-project.iam.gserviceaccount.com#");
        assert_eq!(
            p.target_sa.as_deref(),
            Some("etl-runner@my-project.iam.gserviceaccount.com")
        );
        assert!(p.scope.is_none());
    }

    // -----------------------------------------------------------------
    // endpoint_for / build_body
    // -----------------------------------------------------------------

    fn test_provider() -> GcpIamProvider {
        GcpIamProvider {
            http: reqwest::Client::new(),
            endpoint: DEFAULT_IAM_CREDENTIALS_ENDPOINT.to_string(),
            metadata_token_url: DEFAULT_METADATA_TOKEN_URL.to_string(),
            default_target_sa: None,
            default_lifetime_secs: 3600,
            token: Arc::new(Mutex::new(None)),
        }
    }

    #[test]
    fn endpoint_for_builds_iam_credentials_url() {
        let p = test_provider();
        assert_eq!(
            p.endpoint_for("etl-runner@my-project.iam.gserviceaccount.com"),
            "https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/etl-runner@my-project.iam.gserviceaccount.com:generateAccessToken"
        );
    }

    #[test]
    fn endpoint_for_honours_override() {
        let mut p = test_provider();
        p.endpoint = "http://mock-iamcredentials.test/v1".to_string();
        assert_eq!(
            p.endpoint_for("x@y.iam.gserviceaccount.com"),
            "http://mock-iamcredentials.test/v1/projects/-/serviceAccounts/x@y.iam.gserviceaccount.com:generateAccessToken"
        );
    }

    #[test]
    fn build_body_wraps_scope_in_array_and_formats_lifetime() {
        let body = GcpIamProvider::build_body(
            "https://www.googleapis.com/auth/cloud-platform",
            3600,
        );
        let scopes = body.get("scope").and_then(|v| v.as_array()).expect("array");
        assert_eq!(scopes.len(), 1);
        assert_eq!(
            scopes[0].as_str(),
            Some("https://www.googleapis.com/auth/cloud-platform")
        );
        // IAM Credentials expects "<n>s" suffix, NOT "<n>" — that's a
        // common foot-gun and worth pinning explicitly.
        assert_eq!(body.get("lifetime").and_then(|v| v.as_str()), Some("3600s"));
    }

    #[test]
    fn build_body_handles_short_lifetime() {
        let body = GcpIamProvider::build_body("https://www.googleapis.com/auth/bigquery", 60);
        assert_eq!(body.get("lifetime").and_then(|v| v.as_str()), Some("60s"));
        assert_eq!(
            body.get("scope").and_then(|v| v.as_array()).unwrap()[0].as_str(),
            Some("https://www.googleapis.com/auth/bigquery")
        );
    }

    // -----------------------------------------------------------------
    // Response parser
    // -----------------------------------------------------------------

    #[test]
    fn response_parses_iso_expire_time() {
        let body = r#"{
            "accessToken": "ya29.example-impersonated-token",
            "expireTime": "2026-06-07T03:00:00Z"
        }"#;
        let parsed: GenerateAccessTokenResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.access_token, "ya29.example-impersonated-token");
        let expires_at = chrono::DateTime::parse_from_rfc3339(&parsed.expire_time)
            .unwrap()
            .with_timezone(&chrono::Utc);
        // 2026-06-07T03:00:00Z round-trips back to an equivalent DateTime.
        assert_eq!(
            expires_at,
            chrono::DateTime::parse_from_rfc3339("2026-06-07T03:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc)
        );
    }

    #[test]
    fn response_parse_fails_on_missing_token() {
        let body = r#"{ "expireTime": "2026-06-07T03:00:00Z" }"#;
        let result: Result<GenerateAccessTokenResponse, _> = serde_json::from_str(body);
        assert!(result.is_err());
    }
}
