//! GCP Secret Manager provider (Secrets Wallet Phase 3b, noetl/ai-meta#61).
//!
//! Resolves secret references against Google Secret Manager via its REST
//! `:access` endpoint, authenticating with an ambient GKE Workload-Identity
//! token from the instance metadata server — the same token source
//! [`crate::crypto::GcpKms`] uses for the KEK. No service-account key material
//! is read from the environment; the platform mints the token.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::Deserialize;
use tokio::sync::Mutex;

use super::{SecretProvider, SecretRef, SecretValue};
use crate::error::{AppError, AppResult};

/// Stable provider id.
const PROVIDER: &str = "gcp";

const DEFAULT_SM_ENDPOINT: &str = "https://secretmanager.googleapis.com/v1";
const DEFAULT_METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";

/// GCP Secret Manager backend.
pub struct GcpSecretManager {
    http: reqwest::Client,
    endpoint: String,
    metadata_token_url: String,
    default_project: Option<String>,
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
struct AccessResponse {
    /// Fully-qualified version resource, e.g.
    /// `projects/p/secrets/s/versions/3`.
    #[serde(default)]
    name: Option<String>,
    payload: AccessPayload,
}

#[derive(Deserialize)]
struct AccessPayload {
    /// Base64-encoded secret bytes.
    data: String,
}

impl GcpSecretManager {
    /// Build a provider from ambient configuration.
    ///
    /// `default_project` falls back to `GOOGLE_CLOUD_PROJECT` / `GCP_PROJECT`
    /// when a [`SecretRef`] doesn't carry its own `project`. Endpoint + token
    /// URL are overridable for tests via `NOETL_GCP_SM_ENDPOINT` /
    /// `NOETL_GCP_METADATA_TOKEN_URL`.
    pub fn from_env() -> AppResult<Self> {
        let endpoint = std::env::var("NOETL_GCP_SM_ENDPOINT")
            .unwrap_or_else(|_| DEFAULT_SM_ENDPOINT.to_string());
        let metadata_token_url = std::env::var("NOETL_GCP_METADATA_TOKEN_URL")
            .unwrap_or_else(|_| DEFAULT_METADATA_TOKEN_URL.to_string());
        let default_project = std::env::var("GOOGLE_CLOUD_PROJECT")
            .ok()
            .or_else(|| std::env::var("GCP_PROJECT").ok());
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| {
                AppError::ExternalService(format!("gcp secret manager http client: {e}"))
            })?;
        Ok(Self {
            http,
            endpoint,
            metadata_token_url,
            default_project,
            token: Arc::new(Mutex::new(None)),
        })
    }

    /// Fetch (and cache) a Workload-Identity access token from the metadata
    /// server. Refreshed when within ~60s of expiry.
    async fn access_token(&self) -> AppResult<String> {
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
            .map_err(|e| AppError::ExternalService(format!("gcp metadata token request: {e}")))?;
        if !resp.status().is_success() {
            return Err(AppError::ExternalService(format!(
                "gcp metadata token: HTTP {}",
                resp.status().as_u16()
            )));
        }
        let body: MetadataToken = resp
            .json()
            .await
            .map_err(|e| AppError::ExternalService(format!("gcp metadata token decode: {e}")))?;
        let ttl = body.expires_in.saturating_sub(60).max(1);
        *guard = Some(CachedToken {
            value: body.access_token.clone(),
            expires_at: Instant::now() + Duration::from_secs(ttl),
        });
        Ok(body.access_token)
    }
}

#[async_trait]
impl SecretProvider for GcpSecretManager {
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    async fn fetch(&self, secret: &SecretRef) -> AppResult<SecretValue> {
        let version = secret
            .version
            .clone()
            .unwrap_or_else(|| "latest".to_string());

        // A fully-qualified `projects/.../secrets/...` name carries its own
        // project; otherwise resolve from the ref or the ambient default.
        let project = if secret.name.starts_with("projects/") {
            String::new()
        } else {
            secret
                .project
                .clone()
                .or_else(|| self.default_project.clone())
                .ok_or_else(|| {
                    AppError::Config(
                        "gcp secret manager: no project (set keychain project or \
                         GOOGLE_CLOUD_PROJECT)"
                            .to_string(),
                    )
                })?
        };

        let url = build_access_url(&self.endpoint, &project, &secret.name, &version);
        let token = self.access_token().await?;

        tracing::debug!(provider = PROVIDER, secret = %secret.name, version = %version, "secret.fetch");

        let resp = self
            .http
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| {
                AppError::ExternalService(format!("gcp secret manager access request: {e}"))
            })?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| AppError::ExternalService(format!("gcp secret manager read body: {e}")))?;
        if !status.is_success() {
            // GCP error bodies describe the failure (NOT_FOUND,
            // PERMISSION_DENIED) and contain no secret material.
            let snippet: String = body.chars().take(200).collect();
            return Err(AppError::ExternalService(format!(
                "gcp secret manager: HTTP {} accessing '{}': {snippet}",
                status.as_u16(),
                secret.name
            )));
        }
        parse_access_response(&body)
    }
}

/// Build the Secret Manager `:access` URL for a secret version.
///
/// A fully-qualified `projects/.../secrets/...` name is honored verbatim —
/// `version` is appended only if the path doesn't already pin one. Otherwise
/// the canonical `projects/{project}/secrets/{name}/versions/{version}` shape
/// is built.
pub fn build_access_url(endpoint: &str, project: &str, name: &str, version: &str) -> String {
    let endpoint = endpoint.trim_end_matches('/');
    if name.starts_with("projects/") {
        if name.contains("/versions/") {
            return format!("{endpoint}/{name}:access");
        }
        return format!("{endpoint}/{name}/versions/{version}:access");
    }
    format!("{endpoint}/projects/{project}/secrets/{name}/versions/{version}:access")
}

/// Parse a Secret Manager `:access` response body into a [`SecretValue`].
pub fn parse_access_response(body: &str) -> AppResult<SecretValue> {
    let resp: AccessResponse = serde_json::from_str(body).map_err(|e| {
        AppError::ExternalService(format!("gcp secret manager: invalid access response: {e}"))
    })?;
    let raw = B64.decode(resp.payload.data.trim()).map_err(|e| {
        AppError::ExternalService(format!("gcp secret manager: payload not base64: {e}"))
    })?;
    let value = String::from_utf8(raw).map_err(|e| {
        AppError::ExternalService(format!("gcp secret manager: payload not UTF-8: {e}"))
    })?;
    // Extract the resolved version (segment after `/versions/`).
    let version = resp
        .name
        .as_deref()
        .and_then(|n| n.split("/versions/").nth(1))
        .map(|s| s.to_string());
    Ok(SecretValue { value, version })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_url_from_simple_name() {
        let url = build_access_url(DEFAULT_SM_ENDPOINT, "my-proj", "duffel-token", "latest");
        assert_eq!(
            url,
            "https://secretmanager.googleapis.com/v1/projects/my-proj/secrets/duffel-token/versions/latest:access"
        );
    }

    #[test]
    fn access_url_trims_trailing_slash_on_endpoint() {
        let url = build_access_url("https://sm.example/v1/", "p", "s", "2");
        assert_eq!(
            url,
            "https://sm.example/v1/projects/p/secrets/s/versions/2:access"
        );
    }

    #[test]
    fn access_url_honors_fully_qualified_name_without_version() {
        let url = build_access_url(DEFAULT_SM_ENDPOINT, "ignored", "projects/p/secrets/s", "5");
        assert_eq!(
            url,
            "https://secretmanager.googleapis.com/v1/projects/p/secrets/s/versions/5:access"
        );
    }

    #[test]
    fn access_url_honors_fully_qualified_name_with_version() {
        let url = build_access_url(
            DEFAULT_SM_ENDPOINT,
            "ignored",
            "projects/p/secrets/s/versions/7",
            "latest",
        );
        assert_eq!(
            url,
            "https://secretmanager.googleapis.com/v1/projects/p/secrets/s/versions/7:access"
        );
    }

    #[test]
    fn parse_response_decodes_value_and_version() {
        // base64("hunter2") = "aHVudGVyMg=="
        let body =
            r#"{"name":"projects/p/secrets/s/versions/3","payload":{"data":"aHVudGVyMg=="}}"#;
        let resolved = parse_access_response(body).expect("parse");
        assert_eq!(resolved.value, "hunter2");
        assert_eq!(resolved.version.as_deref(), Some("3"));
    }

    #[test]
    fn parse_response_without_name_has_no_version() {
        let body = r#"{"payload":{"data":"aHVudGVyMg=="}}"#;
        let resolved = parse_access_response(body).expect("parse");
        assert_eq!(resolved.value, "hunter2");
        assert_eq!(resolved.version, None);
    }

    #[test]
    fn parse_response_rejects_non_base64_payload() {
        let body = r#"{"payload":{"data":"!!!not base64!!!"}}"#;
        let err = parse_access_response(body).unwrap_err();
        assert!(format!("{err:?}").contains("not base64"), "got: {err:?}");
    }

    #[test]
    fn parse_response_rejects_malformed_json() {
        let err = parse_access_response("not json").unwrap_err();
        assert!(
            format!("{err:?}").contains("invalid access response"),
            "got: {err:?}"
        );
    }
}
