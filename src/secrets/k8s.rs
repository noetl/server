//! Kubernetes Secrets provider (Secrets Wallet Phase 3.x, noetl/ai-meta#61).
//!
//! Resolves secret references against the Kubernetes API server's
//! `Secret` objects, authenticating with the pod's mounted ServiceAccount
//! token and trusting the cluster CA — both projected at the standard
//! `/var/run/secrets/kubernetes.io/serviceaccount/` paths. No cloud
//! credentials and no external network: the only dependency is the
//! in-cluster API server, which makes this the one secret backend that is
//! fully kind-validatable end-to-end.
//!
//! A keychain entry references a value as `[<namespace>/]<secret>/<key>`:
//! - `duffel-creds/token` — key `token` of Secret `duffel-creds` in the
//!   pod's own namespace.
//! - `noetl/duffel-creds/token` — same, in an explicit namespace.
//! - `duffel-creds` — the Secret must hold exactly one `data` key, which is
//!   returned (a convenience for single-value secrets).

use std::time::Duration;

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::Deserialize;

use super::{SecretProvider, SecretRef, SecretValue};
use crate::error::{AppError, AppResult};

/// Stable provider id.
const PROVIDER: &str = "k8s";

const DEFAULT_API_URL: &str = "https://kubernetes.default.svc";
const SA_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

/// Kubernetes Secrets backend.
pub struct K8sSecretProvider {
    http: reqwest::Client,
    api_url: String,
    /// Path to the ServiceAccount bearer token, re-read per fetch so
    /// projected-token rotation is honored. Overridable for tests.
    token_file: Option<String>,
    /// Raw token override (tests / mock API servers). Takes precedence over
    /// `token_file` when set.
    token_inline: Option<String>,
    default_namespace: String,
}

/// A parsed Kubernetes secret reference: which namespace, which Secret, and
/// which `data` key (None ⇒ the Secret must hold exactly one key).
#[derive(Debug, PartialEq, Eq)]
pub struct K8sRef {
    pub namespace: String,
    pub secret: String,
    pub key: Option<String>,
}

#[derive(Deserialize)]
struct SecretObject {
    #[serde(default)]
    metadata: SecretMetadata,
    #[serde(default)]
    data: std::collections::BTreeMap<String, String>,
}

#[derive(Deserialize, Default)]
struct SecretMetadata {
    #[serde(rename = "resourceVersion", default)]
    resource_version: Option<String>,
}

impl K8sSecretProvider {
    /// Build a provider from ambient in-cluster configuration.
    ///
    /// - API URL: `NOETL_K8S_API_URL`, else `https://$KUBERNETES_SERVICE_HOST:$KUBERNETES_SERVICE_PORT`,
    ///   else `https://kubernetes.default.svc`.
    /// - CA: `NOETL_K8S_CA_FILE`, else `<sa>/ca.crt` — added as a trust root
    ///   when present (an `http://` mock API server needs none).
    /// - Token: `NOETL_K8S_TOKEN` (inline) or `NOETL_K8S_TOKEN_FILE`, else
    ///   `<sa>/token`; re-read per fetch.
    /// - Default namespace: `NOETL_K8S_NAMESPACE`, else `<sa>/namespace`,
    ///   else `default`.
    pub fn from_env() -> AppResult<Self> {
        let api_url = std::env::var("NOETL_K8S_API_URL").ok().unwrap_or_else(|| {
            match (
                std::env::var("KUBERNETES_SERVICE_HOST"),
                std::env::var("KUBERNETES_SERVICE_PORT"),
            ) {
                (Ok(host), Ok(port)) => format!("https://{host}:{port}"),
                (Ok(host), Err(_)) => format!("https://{host}:443"),
                _ => DEFAULT_API_URL.to_string(),
            }
        });

        let ca_file =
            std::env::var("NOETL_K8S_CA_FILE").unwrap_or_else(|_| format!("{SA_DIR}/ca.crt"));
        let token_inline = std::env::var("NOETL_K8S_TOKEN")
            .ok()
            .filter(|s| !s.is_empty());
        let token_file = std::env::var("NOETL_K8S_TOKEN_FILE")
            .ok()
            .or_else(|| Some(format!("{SA_DIR}/token")));

        let default_namespace = std::env::var("NOETL_K8S_NAMESPACE")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                std::fs::read_to_string(format!("{SA_DIR}/namespace"))
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_else(|| "default".to_string());

        let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(10));
        // Trust the cluster CA when the bundle is present (in-cluster). An
        // `http://` mock endpoint (tests) carries no CA and needs none.
        if let Ok(pem) = std::fs::read(&ca_file) {
            match reqwest::Certificate::from_pem_bundle(&pem) {
                Ok(certs) => {
                    for cert in certs {
                        builder = builder.add_root_certificate(cert);
                    }
                }
                Err(e) => {
                    return Err(AppError::Config(format!(
                        "kubernetes secrets: CA bundle '{ca_file}' not valid PEM: {e}"
                    )));
                }
            }
        }
        let http = builder.build().map_err(|e| {
            AppError::ExternalService(format!("kubernetes secrets http client: {e}"))
        })?;

        Ok(Self {
            http,
            api_url,
            token_file,
            token_inline,
            default_namespace,
        })
    }

    /// Resolve the ServiceAccount bearer token (inline override, else file).
    fn bearer_token(&self) -> AppResult<String> {
        if let Some(tok) = &self.token_inline {
            return Ok(tok.clone());
        }
        let path = self.token_file.as_deref().ok_or_else(|| {
            AppError::Config("kubernetes secrets: no ServiceAccount token configured".to_string())
        })?;
        std::fs::read_to_string(path)
            .map(|s| s.trim().to_string())
            .map_err(|e| {
                AppError::Config(format!(
                    "kubernetes secrets: reading ServiceAccount token '{path}': {e}"
                ))
            })
    }
}

#[async_trait]
impl SecretProvider for K8sSecretProvider {
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    async fn fetch(&self, secret: &SecretRef) -> AppResult<SecretValue> {
        let parsed = parse_k8s_ref(&secret.name, &self.default_namespace)?;
        let url = build_secret_url(&self.api_url, &parsed.namespace, &parsed.secret);
        let token = self.bearer_token()?;

        tracing::debug!(
            provider = PROVIDER,
            namespace = %parsed.namespace,
            secret = %parsed.secret,
            key = parsed.key.as_deref().unwrap_or("<single>"),
            "secret.fetch"
        );

        let resp = self
            .http
            .get(&url)
            .bearer_auth(token)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| {
                AppError::ExternalService(format!("kubernetes secrets API request: {e}"))
            })?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| AppError::ExternalService(format!("kubernetes secrets read body: {e}")))?;
        if !status.is_success() {
            // The API error body (Status object) describes the failure
            // (NotFound, Forbidden) and contains no secret material.
            let snippet: String = body.chars().take(200).collect();
            return Err(AppError::ExternalService(format!(
                "kubernetes secrets: HTTP {} reading '{}/{}': {snippet}",
                status.as_u16(),
                parsed.namespace,
                parsed.secret
            )));
        }
        extract_secret_value(&body, &parsed.secret, parsed.key.as_deref())
    }
}

/// Parse `[<namespace>/]<secret>/<key>` (or bare `<secret>`) into a [`K8sRef`].
///
/// - 3 segments → `<namespace>/<secret>/<key>`.
/// - 2 segments → `<secret>/<key>` in `default_ns`.
/// - 1 segment  → `<secret>` in `default_ns`, key unspecified (single-key
///   Secret).
///
/// Empty segments or more than 3 segments are rejected — Kubernetes object
/// names and namespaces never contain `/`.
pub fn parse_k8s_ref(name: &str, default_ns: &str) -> AppResult<K8sRef> {
    let parts: Vec<&str> = name.split('/').collect();
    let bad = || {
        AppError::Config(format!(
            "kubernetes secrets: invalid reference '{name}' \
             (expected '[<namespace>/]<secret>/<key>' or '<secret>')"
        ))
    };
    match parts.as_slice() {
        [secret] if !secret.is_empty() => Ok(K8sRef {
            namespace: default_ns.to_string(),
            secret: secret.to_string(),
            key: None,
        }),
        [secret, key] if !secret.is_empty() && !key.is_empty() => Ok(K8sRef {
            namespace: default_ns.to_string(),
            secret: secret.to_string(),
            key: Some(key.to_string()),
        }),
        [ns, secret, key] if !ns.is_empty() && !secret.is_empty() && !key.is_empty() => {
            Ok(K8sRef {
                namespace: ns.to_string(),
                secret: secret.to_string(),
                key: Some(key.to_string()),
            })
        }
        _ => Err(bad()),
    }
}

/// Build the core-v1 Secret read URL.
pub fn build_secret_url(api_url: &str, namespace: &str, secret: &str) -> String {
    let base = api_url.trim_end_matches('/');
    format!("{base}/api/v1/namespaces/{namespace}/secrets/{secret}")
}

/// Decode a Secret object body into the requested key's UTF-8 value.
///
/// `key = None` requires the Secret to hold exactly one `data` entry. The
/// `resourceVersion` rides along as the [`SecretValue::version`] provenance
/// stamp.
pub fn extract_secret_value(
    body: &str,
    secret_name: &str,
    key: Option<&str>,
) -> AppResult<SecretValue> {
    let obj: SecretObject = serde_json::from_str(body).map_err(|e| {
        AppError::ExternalService(format!("kubernetes secrets: invalid Secret object: {e}"))
    })?;

    let chosen_key: String = match key {
        Some(k) => k.to_string(),
        None => match obj.data.len() {
            1 => obj.data.keys().next().unwrap().clone(),
            0 => {
                return Err(AppError::ExternalService(format!(
                    "kubernetes secrets: Secret '{secret_name}' has no data"
                )))
            }
            n => {
                let keys: Vec<&str> = obj.data.keys().map(|s| s.as_str()).collect();
                return Err(AppError::Config(format!(
                    "kubernetes secrets: Secret '{secret_name}' has {n} keys ({}); \
                     reference a specific one as '{secret_name}/<key>'",
                    keys.join(", ")
                )));
            }
        },
    };

    let b64 = obj.data.get(&chosen_key).ok_or_else(|| {
        AppError::ExternalService(format!(
            "kubernetes secrets: Secret '{secret_name}' has no key '{chosen_key}'"
        ))
    })?;
    let raw = B64.decode(b64.trim()).map_err(|e| {
        AppError::ExternalService(format!("kubernetes secrets: value not base64: {e}"))
    })?;
    let value = String::from_utf8(raw).map_err(|e| {
        AppError::ExternalService(format!("kubernetes secrets: value not UTF-8: {e}"))
    })?;
    Ok(SecretValue {
        value,
        version: obj.metadata.resource_version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bare_secret_uses_default_ns_no_key() {
        let r = parse_k8s_ref("duffel-creds", "noetl").unwrap();
        assert_eq!(
            r,
            K8sRef {
                namespace: "noetl".to_string(),
                secret: "duffel-creds".to_string(),
                key: None,
            }
        );
    }

    #[test]
    fn parse_secret_slash_key_uses_default_ns() {
        let r = parse_k8s_ref("duffel-creds/token", "noetl").unwrap();
        assert_eq!(
            r,
            K8sRef {
                namespace: "noetl".to_string(),
                secret: "duffel-creds".to_string(),
                key: Some("token".to_string()),
            }
        );
    }

    #[test]
    fn parse_ns_secret_key_is_explicit() {
        let r = parse_k8s_ref("prod/duffel-creds/token", "noetl").unwrap();
        assert_eq!(
            r,
            K8sRef {
                namespace: "prod".to_string(),
                secret: "duffel-creds".to_string(),
                key: Some("token".to_string()),
            }
        );
    }

    #[test]
    fn parse_rejects_empty_and_too_many_segments() {
        assert!(parse_k8s_ref("", "ns").is_err());
        assert!(parse_k8s_ref("a/b/c/d", "ns").is_err());
        assert!(parse_k8s_ref("a//c", "ns").is_err());
    }

    #[test]
    fn build_url_trims_trailing_slash() {
        let url = build_secret_url("https://kubernetes.default.svc/", "noetl", "duffel-creds");
        assert_eq!(
            url,
            "https://kubernetes.default.svc/api/v1/namespaces/noetl/secrets/duffel-creds"
        );
    }

    #[test]
    fn extract_named_key_decodes_value_and_version() {
        // base64("hunter2") = "aHVudGVyMg=="
        let body = r#"{"metadata":{"resourceVersion":"4242"},"data":{"token":"aHVudGVyMg==","other":"eA=="}}"#;
        let v = extract_secret_value(body, "creds", Some("token")).unwrap();
        assert_eq!(v.value, "hunter2");
        assert_eq!(v.version.as_deref(), Some("4242"));
    }

    #[test]
    fn extract_single_key_when_unspecified() {
        let body = r#"{"data":{"only":"aHVudGVyMg=="}}"#;
        let v = extract_secret_value(body, "creds", None).unwrap();
        assert_eq!(v.value, "hunter2");
        assert_eq!(v.version, None);
    }

    #[test]
    fn extract_unspecified_key_with_many_keys_errors() {
        let body = r#"{"data":{"a":"eA==","b":"eQ=="}}"#;
        let err = extract_secret_value(body, "creds", None).unwrap_err();
        assert!(format!("{err:?}").contains("has 2 keys"), "got: {err:?}");
    }

    #[test]
    fn extract_missing_key_errors() {
        let body = r#"{"data":{"token":"aHVudGVyMg=="}}"#;
        let err = extract_secret_value(body, "creds", Some("nope")).unwrap_err();
        assert!(format!("{err:?}").contains("no key 'nope'"), "got: {err:?}");
    }

    #[test]
    fn extract_rejects_non_base64_value() {
        let body = r#"{"data":{"token":"!!!not base64!!!"}}"#;
        let err = extract_secret_value(body, "creds", Some("token")).unwrap_err();
        assert!(format!("{err:?}").contains("not base64"), "got: {err:?}");
    }

    #[test]
    fn extract_rejects_malformed_json() {
        let err = extract_secret_value("not json", "creds", None).unwrap_err();
        assert!(
            format!("{err:?}").contains("invalid Secret object"),
            "got: {err:?}"
        );
    }
}
