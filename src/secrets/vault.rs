//! HashiCorp Vault provider (Secrets Wallet Phase 3.x, noetl/ai-meta#61).
//!
//! Resolves secret references against a Vault **KV v2** secrets engine via its
//! REST API, authenticating with a Vault token (`X-Vault-Token`). Like the
//! Kubernetes Secrets provider, Vault can run in-cluster, so this backend is
//! fully kind-validatable end-to-end (a dev-mode Vault + a KV secret, no
//! external cloud).
//!
//! A keychain entry references a value as `[<mount>/]<path>#<key>`:
//! - `secret/duffel#token` — key `token` of the secret at logical path
//!   `duffel` under the `secret` mount.
//! - `secret/app/duffel#token` — nested path `app/duffel`.
//! - `duffel#token` — `<path>#<key>` with the default mount
//!   (`NOETL_VAULT_KV_MOUNT`, default `secret`).
//! - `secret/duffel` — the secret must hold exactly one key, which is returned.
//!
//! KV v2 inserts a `/data/` segment in the API path: the logical path
//! `<mount>/<path>` reads from `GET <addr>/v1/<mount>/data/<path>`, and the
//! value lives at `.data.data.<key>` with the version at
//! `.data.metadata.version`.

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use super::{SecretProvider, SecretRef, SecretValue};
use crate::error::{AppError, AppResult};

/// Stable provider id.
const PROVIDER: &str = "vault";

const DEFAULT_ADDR: &str = "http://127.0.0.1:8200";
const DEFAULT_MOUNT: &str = "secret";

/// HashiCorp Vault (KV v2) backend.
pub struct VaultSecretProvider {
    http: reqwest::Client,
    addr: String,
    default_mount: String,
    /// Vault namespace (Enterprise); sent as `X-Vault-Namespace` when set.
    namespace: Option<String>,
    /// Token file path, re-read per fetch (rotating tokens). Overridable.
    token_file: Option<String>,
    /// Inline token override (`VAULT_TOKEN`); precedence over the file.
    token_inline: Option<String>,
}

/// A parsed Vault KV reference: which mount, which logical path, which key
/// (None ⇒ the secret must hold exactly one key).
#[derive(Debug, PartialEq, Eq)]
pub struct VaultRef {
    pub mount: String,
    pub path: String,
    pub key: Option<String>,
}

#[derive(Deserialize)]
struct KvResponse {
    data: KvData,
}

#[derive(Deserialize)]
struct KvData {
    #[serde(default)]
    data: std::collections::BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    metadata: KvMetadata,
}

#[derive(Deserialize, Default)]
struct KvMetadata {
    #[serde(default)]
    version: Option<i64>,
}

impl VaultSecretProvider {
    /// Build a provider from ambient configuration.
    ///
    /// - Address: `VAULT_ADDR`, else `http://127.0.0.1:8200`.
    /// - Default mount: `NOETL_VAULT_KV_MOUNT`, else `secret`.
    /// - Namespace: `VAULT_NAMESPACE` (Enterprise), optional.
    /// - Token: `VAULT_TOKEN` (inline) or `NOETL_VAULT_TOKEN_FILE`; re-read per
    ///   fetch.
    /// - CA: `NOETL_VAULT_CA_FILE` — added as a trust root for `https://` Vault.
    pub fn from_env() -> AppResult<Self> {
        let addr = std::env::var("VAULT_ADDR")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_ADDR.to_string());
        let default_mount = std::env::var("NOETL_VAULT_KV_MOUNT")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_MOUNT.to_string());
        let namespace = std::env::var("VAULT_NAMESPACE")
            .ok()
            .filter(|s| !s.is_empty());
        let token_inline = std::env::var("VAULT_TOKEN").ok().filter(|s| !s.is_empty());
        let token_file = std::env::var("NOETL_VAULT_TOKEN_FILE")
            .ok()
            .filter(|s| !s.is_empty());

        let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(10));
        if let Ok(ca_file) = std::env::var("NOETL_VAULT_CA_FILE") {
            let pem = std::fs::read(&ca_file).map_err(|e| {
                AppError::Config(format!("vault: reading CA file '{ca_file}': {e}"))
            })?;
            match reqwest::Certificate::from_pem_bundle(&pem) {
                Ok(certs) => {
                    for cert in certs {
                        builder = builder.add_root_certificate(cert);
                    }
                }
                Err(e) => {
                    return Err(AppError::Config(format!(
                        "vault: CA bundle '{ca_file}' not valid PEM: {e}"
                    )));
                }
            }
        }
        let http = builder
            .build()
            .map_err(|e| AppError::ExternalService(format!("vault http client: {e}")))?;

        Ok(Self {
            http,
            addr,
            default_mount,
            namespace,
            token_file,
            token_inline,
        })
    }

    /// Resolve the Vault token (inline override, else file).
    fn token(&self) -> AppResult<String> {
        if let Some(tok) = &self.token_inline {
            return Ok(tok.clone());
        }
        let path = self.token_file.as_deref().ok_or_else(|| {
            AppError::Config(
                "vault: no token configured (set VAULT_TOKEN or NOETL_VAULT_TOKEN_FILE)"
                    .to_string(),
            )
        })?;
        std::fs::read_to_string(path)
            .map(|s| s.trim().to_string())
            .map_err(|e| AppError::Config(format!("vault: reading token file '{path}': {e}")))
    }
}

#[async_trait]
impl SecretProvider for VaultSecretProvider {
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    async fn fetch(&self, secret: &SecretRef) -> AppResult<SecretValue> {
        let parsed = parse_vault_ref(&secret.name, &self.default_mount)?;
        let url = build_kv_url(&self.addr, &parsed.mount, &parsed.path);
        let token = self.token()?;

        tracing::debug!(
            provider = PROVIDER,
            mount = %parsed.mount,
            path = %parsed.path,
            key = parsed.key.as_deref().unwrap_or("<single>"),
            "secret.fetch"
        );

        let mut req = self.http.get(&url).header("X-Vault-Token", token);
        if let Some(ns) = &self.namespace {
            req = req.header("X-Vault-Namespace", ns);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| AppError::ExternalService(format!("vault KV request: {e}")))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| AppError::ExternalService(format!("vault read body: {e}")))?;
        if !status.is_success() {
            // Vault error bodies are `{"errors": [...]}` — no secret material.
            let snippet: String = body.chars().take(200).collect();
            return Err(AppError::ExternalService(format!(
                "vault: HTTP {} reading '{}/{}': {snippet}",
                status.as_u16(),
                parsed.mount,
                parsed.path
            )));
        }
        extract_kv_value(&body, &parsed.path, parsed.key.as_deref())
    }
}

/// Parse `[<mount>/]<path>#<key>` (or bare `[<mount>/]<path>`) into a
/// [`VaultRef`].
///
/// The `#` separates the (optional) key.  Before it, the first `/`-segment is
/// the mount **only when at least one `/` is present**; otherwise the default
/// mount applies and the whole string is the path (KV paths may themselves
/// contain `/`).
pub fn parse_vault_ref(name: &str, default_mount: &str) -> AppResult<VaultRef> {
    let bad = || {
        AppError::Config(format!(
            "vault: invalid reference '{name}' (expected '[<mount>/]<path>#<key>' or '[<mount>/]<path>')"
        ))
    };
    let (location, key) = match name.split_once('#') {
        Some((loc, k)) if !k.is_empty() => (loc, Some(k.to_string())),
        Some(_) => return Err(bad()), // trailing '#'
        None => (name, None),
    };
    if location.is_empty() {
        return Err(bad());
    }
    let (mount, path) = match location.split_once('/') {
        Some((m, p)) if !m.is_empty() && !p.is_empty() => (m.to_string(), p.to_string()),
        Some(_) => return Err(bad()), // leading/trailing slash
        None => (default_mount.to_string(), location.to_string()),
    };
    Ok(VaultRef { mount, path, key })
}

/// Build the KV v2 read URL: `<addr>/v1/<mount>/data/<path>`.
pub fn build_kv_url(addr: &str, mount: &str, path: &str) -> String {
    let base = addr.trim_end_matches('/');
    format!("{base}/v1/{mount}/data/{path}")
}

/// Decode a KV v2 response body into the requested key's value.
///
/// `key = None` requires the secret to hold exactly one key. The KV
/// `metadata.version` rides along as the [`SecretValue::version`] stamp.
pub fn extract_kv_value(
    body: &str,
    secret_path: &str,
    key: Option<&str>,
) -> AppResult<SecretValue> {
    let resp: KvResponse = serde_json::from_str(body)
        .map_err(|e| AppError::ExternalService(format!("vault: invalid KV response: {e}")))?;
    let map = &resp.data.data;

    let chosen_key: String = match key {
        Some(k) => k.to_string(),
        None => match map.len() {
            1 => map.keys().next().unwrap().clone(),
            0 => {
                return Err(AppError::ExternalService(format!(
                    "vault: secret '{secret_path}' has no data"
                )))
            }
            n => {
                let keys: Vec<&str> = map.keys().map(|s| s.as_str()).collect();
                return Err(AppError::Config(format!(
                    "vault: secret '{secret_path}' has {n} keys ({}); reference a specific one as \
                     '{secret_path}#<key>'",
                    keys.join(", ")
                )));
            }
        },
    };

    let raw = map.get(&chosen_key).ok_or_else(|| {
        AppError::ExternalService(format!(
            "vault: secret '{secret_path}' has no key '{chosen_key}'"
        ))
    })?;
    let value = match raw {
        serde_json::Value::String(s) => s.clone(),
        other => {
            return Err(AppError::ExternalService(format!(
                "vault: secret '{secret_path}' key '{chosen_key}' is not a string (got {})",
                match other {
                    serde_json::Value::Number(_) => "number",
                    serde_json::Value::Bool(_) => "bool",
                    serde_json::Value::Null => "null",
                    serde_json::Value::Array(_) => "array",
                    serde_json::Value::Object(_) => "object",
                    serde_json::Value::String(_) => "string",
                }
            )))
        }
    };
    Ok(SecretValue {
        value,
        version: resp.data.metadata.version.map(|v| v.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_mount_path_key() {
        let r = parse_vault_ref("secret/duffel#token", "kv").unwrap();
        assert_eq!(
            r,
            VaultRef {
                mount: "secret".to_string(),
                path: "duffel".to_string(),
                key: Some("token".to_string()),
            }
        );
    }

    #[test]
    fn parse_nested_path() {
        let r = parse_vault_ref("secret/app/duffel#token", "kv").unwrap();
        assert_eq!(r.mount, "secret");
        assert_eq!(r.path, "app/duffel");
        assert_eq!(r.key.as_deref(), Some("token"));
    }

    #[test]
    fn parse_no_slash_uses_default_mount() {
        let r = parse_vault_ref("duffel#token", "secret").unwrap();
        assert_eq!(
            r,
            VaultRef {
                mount: "secret".to_string(),
                path: "duffel".to_string(),
                key: Some("token".to_string()),
            }
        );
    }

    #[test]
    fn parse_bare_secret_no_key() {
        let r = parse_vault_ref("secret/duffel", "kv").unwrap();
        assert_eq!(r.mount, "secret");
        assert_eq!(r.path, "duffel");
        assert_eq!(r.key, None);
    }

    #[test]
    fn parse_rejects_empty_and_trailing_hash() {
        assert!(parse_vault_ref("", "secret").is_err());
        assert!(parse_vault_ref("duffel#", "secret").is_err());
        assert!(parse_vault_ref("#token", "secret").is_err());
    }

    #[test]
    fn build_url_inserts_data_segment() {
        let url = build_kv_url("https://vault.svc:8200/", "secret", "app/duffel");
        assert_eq!(url, "https://vault.svc:8200/v1/secret/data/app/duffel");
    }

    #[test]
    fn extract_named_key_and_version() {
        let body = r#"{"data":{"data":{"token":"hunter2","other":"x"},"metadata":{"version":7}}}"#;
        let v = extract_kv_value(body, "duffel", Some("token")).unwrap();
        assert_eq!(v.value, "hunter2");
        assert_eq!(v.version.as_deref(), Some("7"));
    }

    #[test]
    fn extract_single_key_when_unspecified() {
        let body = r#"{"data":{"data":{"only":"hunter2"},"metadata":{}}}"#;
        let v = extract_kv_value(body, "duffel", None).unwrap();
        assert_eq!(v.value, "hunter2");
        assert_eq!(v.version, None);
    }

    #[test]
    fn extract_unspecified_key_many_errors() {
        let body = r#"{"data":{"data":{"a":"x","b":"y"},"metadata":{}}}"#;
        let err = extract_kv_value(body, "duffel", None).unwrap_err();
        assert!(format!("{err:?}").contains("has 2 keys"), "got: {err:?}");
    }

    #[test]
    fn extract_missing_key_errors() {
        let body = r#"{"data":{"data":{"token":"x"},"metadata":{}}}"#;
        let err = extract_kv_value(body, "duffel", Some("nope")).unwrap_err();
        assert!(format!("{err:?}").contains("no key 'nope'"), "got: {err:?}");
    }

    #[test]
    fn extract_non_string_value_errors() {
        let body = r#"{"data":{"data":{"token":42},"metadata":{}}}"#;
        let err = extract_kv_value(body, "duffel", Some("token")).unwrap_err();
        assert!(format!("{err:?}").contains("not a string"), "got: {err:?}");
    }

    #[test]
    fn extract_rejects_malformed_json() {
        let err = extract_kv_value("not json", "duffel", None).unwrap_err();
        assert!(
            format!("{err:?}").contains("invalid KV response"),
            "got: {err:?}"
        );
    }
}
