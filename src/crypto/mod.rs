//! Cryptography module for the NoETL Control Plane.
//!
//! Provides AES-GCM encryption for credential data, plus envelope encryption
//! (per-record DEK wrapped by a KEK) for the secrets wallet — see
//! [`envelope`] + [`keymanager`] (noetl/ai-meta#61).

pub mod encryption;
pub mod envelope;
pub mod keymanager;
pub mod kms_gcp;
pub mod sealed;

pub use encryption::{decrypt, encrypt, Encryptor};
pub use envelope::{EnvelopeCipher, EnvelopeRecord, ENC_ALG, ENC_VERSION};
pub use keymanager::{KeyManager, LocalDevKms, WrappedDek};
pub use kms_gcp::GcpKms;
pub use sealed::{open as sealed_open, seal as sealed_seal, SealedEnvelope, SEAL_ALG, SEAL_V};

use std::sync::Arc;

use crate::error::{AppError, AppResult};

/// Build the configured KEK key manager (Secrets Wallet, noetl/ai-meta#61).
///
/// `provider` selects the backend:
/// - `local` (default) — [`LocalDevKms`] wraps DEKs with the in-process
///   AES-256-GCM master key. The dev / kind / single-node KEK.
/// - `gcp-kms` — [`GcpKms`] wraps DEKs via Google Cloud KMS (the KEK never
///   leaves Cloud KMS); requires `gcp_kms_key` (a cryptoKey resource name).
///
/// New KMS providers (AWS, Azure, Vault) add an arm here behind the same
/// [`KeyManager`] trait — the stored record format is unchanged.
pub fn build_key_manager(
    provider: &str,
    master_key_base64: &str,
    gcp_kms_key: Option<&str>,
) -> AppResult<Arc<dyn KeyManager>> {
    match provider.trim().to_ascii_lowercase().as_str() {
        "" | "local" => Ok(Arc::new(LocalDevKms::from_master_key_base64(
            master_key_base64,
        )?)),
        "gcp-kms" | "gcp" => {
            let key = gcp_kms_key.ok_or_else(|| {
                AppError::Encryption(
                    "NOETL_KMS_PROVIDER=gcp-kms requires NOETL_GCP_KMS_KEY (a Cloud KMS \
                     cryptoKey resource: projects/.../cryptoKeys/<key>)"
                        .to_string(),
                )
            })?;
            Ok(Arc::new(GcpKms::new(key)?))
        }
        other => Err(AppError::Encryption(format!(
            "unknown NOETL_KMS_PROVIDER '{other}' (expected 'local' or 'gcp-kms')"
        ))),
    }
}

/// Build the wallet's [`EnvelopeCipher`] from environment-configured KMS
/// settings (`NOETL_KMS_PROVIDER`, `NOETL_GCP_KMS_KEY`) + the master key.
pub fn build_envelope_cipher(master_key_base64: &str) -> AppResult<EnvelopeCipher> {
    let provider = std::env::var("NOETL_KMS_PROVIDER").unwrap_or_else(|_| "local".to_string());
    let gcp_key = std::env::var("NOETL_GCP_KMS_KEY").ok();
    let km = build_key_manager(&provider, master_key_base64, gcp_key.as_deref())?;
    tracing::info!(kms_provider = %km.provider(), "Wallet KEK provider initialized");
    Ok(EnvelopeCipher::new(km))
}

#[cfg(test)]
mod factory_tests {
    use super::*;
    use crate::crypto::Encryptor;

    #[test]
    fn defaults_to_local() {
        let key = Encryptor::generate_key_base64();
        let km = build_key_manager("local", &key, None).unwrap();
        assert_eq!(km.provider(), "local");
        // Empty provider also defaults to local.
        let km2 = build_key_manager("", &key, None).unwrap();
        assert_eq!(km2.provider(), "local");
    }

    #[test]
    fn gcp_requires_key() {
        let key = Encryptor::generate_key_base64();
        assert!(build_key_manager("gcp-kms", &key, None).is_err());
        let km = build_key_manager(
            "gcp-kms",
            &key,
            Some("projects/p/locations/l/keyRings/r/cryptoKeys/k"),
        )
        .unwrap();
        assert_eq!(km.provider(), "gcp-kms");
    }

    #[test]
    fn unknown_provider_errors() {
        let key = Encryptor::generate_key_base64();
        assert!(build_key_manager("vault-xyz", &key, None).is_err());
    }
}
