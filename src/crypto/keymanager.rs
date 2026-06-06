//! Key management for envelope encryption (Secrets Wallet Phase 1,
//! noetl/ai-meta#61).
//!
//! The wallet uses **envelope encryption**: each secret record is encrypted
//! with a per-record **data-encryption key (DEK)**, and the DEK is itself
//! encrypted ("wrapped") by a **key-encryption key (KEK)** held in a key
//! manager (a KMS in production, or [`LocalDevKms`] in dev/kind). Only the
//! wrapped DEK is stored beside the ciphertext; the plaintext DEK exists in
//! memory only for the duration of an encrypt/decrypt and is zeroized after.
//!
//! This module defines the [`KeyManager`] trait (the KEK boundary) and the
//! in-process [`LocalDevKms`] implementation that wraps DEKs with the server's
//! AES-256-GCM master key. Real KMS providers (GCP Cloud KMS, AWS KMS, Azure
//! Key Vault, Vault Transit) implement the same trait in Phase 2.

use async_trait::async_trait;
use zeroize::Zeroizing;

use crate::crypto::encryption::Encryptor;
use crate::error::{AppError, AppResult};

/// A DEK that has been wrapped (encrypted) by a [`KeyManager`]'s KEK, plus the
/// metadata needed to identify which KEK (provider / key / version) wrapped it
/// so it can be unwrapped later — even after a key rotation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WrappedDek {
    /// KEK provider id, e.g. `local`, `gcp-kms`, `aws-kms`, `azure-kv`, `vault`.
    pub provider: String,
    /// KEK identifier within the provider (a key resource name / alias).
    pub key_id: String,
    /// KEK version that wrapped this DEK (for rotation-aware unwrap).
    pub key_version: String,
    /// The wrapped (KEK-encrypted) DEK bytes.
    pub ciphertext: Vec<u8>,
}

/// The KEK boundary. Implementations wrap/unwrap DEKs; the plaintext DEK never
/// leaves the caller, and a KMS-backed implementation never exposes the KEK.
#[async_trait]
pub trait KeyManager: Send + Sync {
    /// Wrap (encrypt) a freshly-generated 32-byte DEK with the current KEK.
    async fn wrap_dek(&self, dek: &[u8]) -> AppResult<WrappedDek>;

    /// Unwrap (decrypt) a previously-wrapped DEK. The returned bytes are
    /// zeroized on drop.
    async fn unwrap_dek(&self, wrapped: &WrappedDek) -> AppResult<Zeroizing<Vec<u8>>>;

    /// Provider id stored alongside records (`local`, `gcp-kms`, …).
    fn provider(&self) -> &str;

    /// Secrets Wallet Phase 7a — current KEK version the next `wrap_dek` call
    /// will tag.  Wallet-level rotation primitives use this to decide whether
    /// a stored record's `wrapped.key_version` is the same as the active
    /// version (skip) or older (re-wrap).
    ///
    /// Default implementation reports `"unknown"` so the rotation primitive
    /// treats every record as "different version, rewrap" — safe but
    /// inefficient.  Real `KeyManager` implementations override this with
    /// the version they actually use at `wrap_dek` time.
    fn current_key_version(&self) -> &str {
        "unknown"
    }
}

/// In-process key manager that wraps DEKs with the server's AES-256-GCM master
/// key (the `NOETL_ENCRYPTION_KEY`, now fail-closed per Phase 1a). This is the
/// dev / kind / single-node KEK. It is **not** a real KMS — the KEK lives in
/// the process — but it gives envelope encryption (per-record DEKs, rotatable
/// by re-wrapping) without an external dependency, and the master key can be
/// supplied from a mounted secret. Production swaps in a KMS-backed
/// [`KeyManager`] in Phase 2 without touching the record format.
pub struct LocalDevKms {
    kek: Encryptor,
    key_id: String,
    key_version: String,
}

impl LocalDevKms {
    /// Build a `LocalDevKms` from a base64-encoded 32-byte master key (the
    /// resolved `NOETL_ENCRYPTION_KEY`).
    pub fn from_master_key_base64(master_key_base64: &str) -> AppResult<Self> {
        let kek = Encryptor::from_base64(master_key_base64)?;
        Ok(Self {
            kek,
            key_id: "env:NOETL_ENCRYPTION_KEY".to_string(),
            key_version: "v1".to_string(),
        })
    }

    /// Build a `LocalDevKms` with an explicit `key_version` label.  Used
    /// by Phase-7a rotation tests to simulate the "operator bumped the
    /// version" path; production paths never need this (the version
    /// comes from `from_master_key_base64`'s `"v1"` default until a
    /// real KMS reports a different one).
    #[cfg(test)]
    pub fn from_master_key_base64_with_version(
        master_key_base64: &str,
        key_version: &str,
    ) -> AppResult<Self> {
        let kek = Encryptor::from_base64(master_key_base64)?;
        Ok(Self {
            kek,
            key_id: "env:NOETL_ENCRYPTION_KEY".to_string(),
            key_version: key_version.to_string(),
        })
    }
}

#[async_trait]
impl KeyManager for LocalDevKms {
    async fn wrap_dek(&self, dek: &[u8]) -> AppResult<WrappedDek> {
        let ciphertext = self.kek.encrypt(dek)?;
        Ok(WrappedDek {
            provider: "local".to_string(),
            key_id: self.key_id.clone(),
            key_version: self.key_version.clone(),
            ciphertext,
        })
    }

    async fn unwrap_dek(&self, wrapped: &WrappedDek) -> AppResult<Zeroizing<Vec<u8>>> {
        if wrapped.provider != "local" {
            return Err(AppError::Encryption(format!(
                "LocalDevKms cannot unwrap a DEK from provider '{}'",
                wrapped.provider
            )));
        }
        let dek = self.kek.decrypt(&wrapped.ciphertext)?;
        Ok(Zeroizing::new(dek))
    }

    fn provider(&self) -> &str {
        "local"
    }

    fn current_key_version(&self) -> &str {
        &self.key_version
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kms() -> LocalDevKms {
        let key = Encryptor::generate_key_base64();
        LocalDevKms::from_master_key_base64(&key).unwrap()
    }

    #[tokio::test]
    async fn wrap_unwrap_round_trips() {
        let km = kms();
        let dek = Encryptor::generate_key(); // 32 bytes
        let wrapped = km.wrap_dek(&dek).await.unwrap();
        assert_eq!(wrapped.provider, "local");
        // The wrapped bytes are NOT the plaintext DEK.
        assert_ne!(wrapped.ciphertext, dek);
        let unwrapped = km.unwrap_dek(&wrapped).await.unwrap();
        assert_eq!(unwrapped.as_slice(), dek.as_slice());
    }

    #[tokio::test]
    async fn unwrap_with_a_different_kek_fails() {
        let km1 = kms();
        let km2 = kms(); // different master key
        let dek = Encryptor::generate_key();
        let wrapped = km1.wrap_dek(&dek).await.unwrap();
        assert!(km2.unwrap_dek(&wrapped).await.is_err());
    }

    #[tokio::test]
    async fn rejects_foreign_provider() {
        let km = kms();
        let dek = Encryptor::generate_key();
        let mut wrapped = km.wrap_dek(&dek).await.unwrap();
        wrapped.provider = "gcp-kms".to_string();
        assert!(km.unwrap_dek(&wrapped).await.is_err());
    }
}
