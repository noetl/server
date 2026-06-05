//! GCP Cloud KMS key manager (Secrets Wallet Phase 2, noetl/ai-meta#61).
//!
//! Implements [`KeyManager`] over **Google Cloud KMS**: `wrap_dek` /
//! `unwrap_dek` become Cloud KMS `:encrypt` / `:decrypt` calls on a symmetric
//! crypto key. The KEK never leaves Cloud KMS — only the wrapped DEK is ever
//! held in the process.
//!
//! Auth uses **ambient GKE Workload Identity**: a short-lived access token is
//! fetched from the instance metadata server (per `execution-model.md`'s
//! already-in-place-trust rule — no bootstrap secret stored in the wallet).
//! The transport is the Cloud KMS REST API over `reqwest` (no heavy gRPC
//! dependency).
//!
//! Selection is runtime (`NOETL_KMS_PROVIDER=gcp-kms` + `NOETL_GCP_KMS_KEY=<resource>`);
//! the default stays [`super::LocalDevKms`] so kind / dev keep working.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use serde::Deserialize;
use tokio::sync::Mutex;

use crate::crypto::keymanager::{KeyManager, WrappedDek};
use crate::error::{AppError, AppResult};

const PROVIDER: &str = "gcp-kms";
const DEFAULT_KMS_ENDPOINT: &str = "https://cloudkms.googleapis.com/v1";
const DEFAULT_METADATA_TOKEN_URL: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";

/// GCP Cloud KMS-backed key manager.
pub struct GcpKms {
    http: reqwest::Client,
    /// The symmetric crypto key resource: `projects/<p>/locations/<l>/keyRings/<r>/cryptoKeys/<k>`.
    key_name: String,
    kms_endpoint: String,
    metadata_token_url: String,
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
struct EncryptResponse {
    /// The key VERSION used, e.g. `.../cryptoKeys/<k>/cryptoKeyVersions/<v>`.
    #[serde(default)]
    name: String,
    ciphertext: String,
}

#[derive(Deserialize)]
struct DecryptResponse {
    plaintext: String,
}

impl GcpKms {
    /// Build from a Cloud KMS crypto-key resource name.
    pub fn new(key_name: &str) -> AppResult<Self> {
        if !key_name.contains("/cryptoKeys/") {
            return Err(AppError::Encryption(format!(
                "NOETL_GCP_KMS_KEY must be a Cloud KMS cryptoKey resource \
                 (projects/.../cryptoKeys/<key>), got '{key_name}'"
            )));
        }
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .map_err(|e| AppError::Encryption(format!("kms http client: {e}")))?,
            key_name: key_name.to_string(),
            kms_endpoint: DEFAULT_KMS_ENDPOINT.to_string(),
            metadata_token_url: DEFAULT_METADATA_TOKEN_URL.to_string(),
            token: Arc::new(Mutex::new(None)),
        })
    }

    /// Fetch (and cache) a Workload-Identity access token from the metadata
    /// server. Refreshed when within 60s of expiry.
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
            .map_err(|e| AppError::Encryption(format!("kms metadata token request: {e}")))?;
        if !resp.status().is_success() {
            return Err(AppError::Encryption(format!(
                "kms metadata token: HTTP {}",
                resp.status().as_u16()
            )));
        }
        let body: MetadataToken = resp
            .json()
            .await
            .map_err(|e| AppError::Encryption(format!("kms metadata token decode: {e}")))?;
        let ttl = body.expires_in.saturating_sub(60).max(1);
        *guard = Some(CachedToken {
            value: body.access_token.clone(),
            expires_at: Instant::now() + Duration::from_secs(ttl),
        });
        Ok(body.access_token)
    }
}

/// Pure: the Cloud KMS `:encrypt` request body for a DEK.
fn encrypt_body(dek: &[u8]) -> serde_json::Value {
    serde_json::json!({ "plaintext": B64.encode(dek) })
}

/// Pure: the Cloud KMS `:decrypt` request body for a wrapped DEK.
fn decrypt_body(ciphertext: &[u8]) -> serde_json::Value {
    serde_json::json!({ "ciphertext": B64.encode(ciphertext) })
}

/// Pure: parse an `:encrypt` response into (wrapped-dek bytes, key version).
fn parse_encrypt(body: &EncryptResponse) -> AppResult<(Vec<u8>, String)> {
    let ct = B64
        .decode(body.ciphertext.as_bytes())
        .map_err(|e| AppError::Encryption(format!("kms encrypt ciphertext base64: {e}")))?;
    // The version is the trailing `cryptoKeyVersions/<v>` segment, if present.
    let version = body
        .name
        .rsplit("cryptoKeyVersions/")
        .next()
        .filter(|v| !v.is_empty() && *v != body.name)
        .unwrap_or("primary")
        .to_string();
    Ok((ct, version))
}

#[async_trait]
impl KeyManager for GcpKms {
    async fn wrap_dek(&self, dek: &[u8]) -> AppResult<WrappedDek> {
        let token = self.access_token().await?;
        let url = format!("{}/{}:encrypt", self.kms_endpoint, self.key_name);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&encrypt_body(dek))
            .send()
            .await
            .map_err(|e| AppError::Encryption(format!("kms encrypt request: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let detail = resp.text().await.unwrap_or_default();
            return Err(AppError::Encryption(format!(
                "kms encrypt: HTTP {status} {}",
                detail.chars().take(200).collect::<String>()
            )));
        }
        let body: EncryptResponse = resp
            .json()
            .await
            .map_err(|e| AppError::Encryption(format!("kms encrypt decode: {e}")))?;
        let (ciphertext, version) = parse_encrypt(&body)?;
        Ok(WrappedDek {
            provider: PROVIDER.to_string(),
            key_id: self.key_name.clone(),
            key_version: version,
            ciphertext,
        })
    }

    async fn unwrap_dek(&self, wrapped: &WrappedDek) -> AppResult<zeroize::Zeroizing<Vec<u8>>> {
        if wrapped.provider != PROVIDER {
            return Err(AppError::Encryption(format!(
                "GcpKms cannot unwrap a DEK from provider '{}'",
                wrapped.provider
            )));
        }
        let token = self.access_token().await?;
        // Decrypt against the cryptoKey (Cloud KMS selects the version from the
        // ciphertext); `wrapped.key_id` is that cryptoKey resource name.
        let url = format!("{}/{}:decrypt", self.kms_endpoint, wrapped.key_id);
        let resp = self
            .http
            .post(&url)
            .bearer_auth(token)
            .json(&decrypt_body(&wrapped.ciphertext))
            .send()
            .await
            .map_err(|e| AppError::Encryption(format!("kms decrypt request: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let detail = resp.text().await.unwrap_or_default();
            return Err(AppError::Encryption(format!(
                "kms decrypt: HTTP {status} {}",
                detail.chars().take(200).collect::<String>()
            )));
        }
        let body: DecryptResponse = resp
            .json()
            .await
            .map_err(|e| AppError::Encryption(format!("kms decrypt decode: {e}")))?;
        let pt = B64
            .decode(body.plaintext.as_bytes())
            .map_err(|e| AppError::Encryption(format!("kms decrypt plaintext base64: {e}")))?;
        Ok(zeroize::Zeroizing::new(pt))
    }

    fn provider(&self) -> &str {
        PROVIDER
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_cryptokey_resource() {
        assert!(GcpKms::new("projects/p/locations/l/keyRings/r").is_err());
        assert!(GcpKms::new("projects/p/locations/l/keyRings/r/cryptoKeys/k").is_ok());
    }

    #[test]
    fn encrypt_body_is_base64_plaintext() {
        let b = encrypt_body(b"\x00\x01\x02");
        assert_eq!(b["plaintext"], B64.encode([0u8, 1, 2]));
    }

    #[test]
    fn decrypt_body_is_base64_ciphertext() {
        let b = decrypt_body(b"\xff\xfe");
        assert_eq!(b["ciphertext"], B64.encode([255u8, 254]));
    }

    #[test]
    fn parse_encrypt_extracts_version_and_bytes() {
        let r = EncryptResponse {
            name: "projects/p/locations/l/keyRings/r/cryptoKeys/k/cryptoKeyVersions/7".to_string(),
            ciphertext: B64.encode(b"wrapped"),
        };
        let (ct, ver) = parse_encrypt(&r).unwrap();
        assert_eq!(ct, b"wrapped");
        assert_eq!(ver, "7");
    }

    #[test]
    fn parse_encrypt_defaults_version_when_absent() {
        let r = EncryptResponse {
            name: String::new(),
            ciphertext: B64.encode(b"x"),
        };
        let (_ct, ver) = parse_encrypt(&r).unwrap();
        assert_eq!(ver, "primary");
    }
}
