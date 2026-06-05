//! Envelope encryption for the secrets wallet (Phase 1, noetl/ai-meta#61).
//!
//! [`EnvelopeCipher::seal`] generates a fresh per-record DEK, AES-256-GCM
//! encrypts the plaintext under it, then wraps the DEK with the configured
//! [`KeyManager`]'s KEK. [`EnvelopeCipher::open`] reverses it. The plaintext
//! DEK is zeroized after each operation; only the ciphertext + the wrapped DEK
//! (+ metadata) are persisted.
//!
//! The persisted shape is [`EnvelopeRecord`]; the DB layer (Phase 1c/1d) maps
//! its fields onto the `*_encrypted` / `wrapped_dek` / `kek_*` / `enc_*`
//! columns on `noetl.credential` and `noetl.keychain`.

use std::sync::Arc;

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::crypto::encryption::Encryptor;
use crate::crypto::keymanager::{KeyManager, WrappedDek};
use crate::error::{AppError, AppResult};

/// The current envelope-encryption format version stored in `enc_version`.
/// `0` (or NULL) means a legacy single-master-key record (pre-Phase-1).
pub const ENC_VERSION: i16 = 1;
/// The content-encryption algorithm stored in `enc_alg`.
pub const ENC_ALG: &str = "AES-256-GCM";

/// A sealed secret: ciphertext + the wrapped DEK + algorithm metadata. This is
/// the unit the storage layer persists and the wallet reads back.
#[derive(Clone, Debug)]
pub struct EnvelopeRecord {
    /// `nonce || AES-256-GCM(dek, plaintext)` (the `Encryptor` wire format).
    pub ciphertext: Vec<u8>,
    /// The DEK, wrapped by the KEK.
    pub wrapped: WrappedDek,
    /// Content-encryption algorithm (`AES-256-GCM`).
    pub enc_alg: String,
    /// Envelope format version (`ENC_VERSION`).
    pub enc_version: i16,
}

/// On-storage JSON form of an [`EnvelopeRecord`]: the bytes that go into the
/// `data_encrypted` (TEXT) / `data` (BYTEA) column. Self-describing (carries
/// the KEK identity + format version) so the re-encrypt / rotation job can act
/// on it. Forward-only: there is no legacy fallback — a stored value that does
/// not parse as this shape is an error (re-register the secret).
#[derive(Serialize, Deserialize)]
struct StoredEnvelope {
    /// Envelope format version.
    v: i16,
    /// Content-encryption algorithm.
    alg: String,
    /// base64 `nonce || ciphertext`.
    ct: String,
    /// Wrapped DEK + the KEK that wrapped it.
    dek: StoredDek,
}

#[derive(Serialize, Deserialize)]
struct StoredDek {
    /// KEK provider (`local`, `gcp-kms`, …).
    p: String,
    /// KEK key id.
    kid: String,
    /// KEK key version.
    kv: String,
    /// base64 wrapped-DEK bytes.
    ct: String,
}

impl EnvelopeRecord {
    /// Serialise to the on-storage JSON string (UTF-8; safe for TEXT + BYTEA).
    pub fn to_storage_string(&self) -> String {
        let s = StoredEnvelope {
            v: self.enc_version,
            alg: self.enc_alg.clone(),
            ct: B64.encode(&self.ciphertext),
            dek: StoredDek {
                p: self.wrapped.provider.clone(),
                kid: self.wrapped.key_id.clone(),
                kv: self.wrapped.key_version.clone(),
                ct: B64.encode(&self.wrapped.ciphertext),
            },
        };
        serde_json::to_string(&s).expect("StoredEnvelope serialises")
    }

    /// Parse an on-storage value. Errors (rather than falling back) if the
    /// value is not a valid envelope — forward-only, no legacy path.
    pub fn from_storage_str(raw: &str) -> AppResult<EnvelopeRecord> {
        let s: StoredEnvelope = serde_json::from_str(raw.trim())
            .map_err(|e| AppError::Encryption(format!("not a wallet envelope record: {e}")))?;
        let ciphertext = B64
            .decode(s.ct.as_bytes())
            .map_err(|e| AppError::Encryption(format!("envelope ct base64: {e}")))?;
        let dek_ct = B64
            .decode(s.dek.ct.as_bytes())
            .map_err(|e| AppError::Encryption(format!("envelope dek base64: {e}")))?;
        Ok(EnvelopeRecord {
            ciphertext,
            wrapped: WrappedDek {
                provider: s.dek.p,
                key_id: s.dek.kid,
                key_version: s.dek.kv,
                ciphertext: dek_ct,
            },
            enc_alg: s.alg,
            enc_version: s.v,
        })
    }
}

/// Seals/opens secrets with envelope encryption over a [`KeyManager`].
#[derive(Clone)]
pub struct EnvelopeCipher {
    km: Arc<dyn KeyManager>,
}

impl EnvelopeCipher {
    pub fn new(km: Arc<dyn KeyManager>) -> Self {
        Self { km }
    }

    /// The KEK provider id (`local`, `gcp-kms`, …) for logging / audit.
    pub fn provider(&self) -> &str {
        self.km.provider()
    }

    /// Encrypt `plaintext`: fresh DEK → AES-256-GCM → wrap the DEK.
    pub async fn seal(&self, plaintext: &[u8]) -> AppResult<EnvelopeRecord> {
        // Per-record 256-bit DEK; zeroized on drop.
        let mut dek = Zeroizing::new(vec![0u8; 32]);
        rand::thread_rng().fill_bytes(dek.as_mut_slice());

        let cipher = Encryptor::from_bytes(&dek)?;
        let ciphertext = cipher.encrypt(plaintext)?;
        let wrapped = self.km.wrap_dek(&dek).await?;

        Ok(EnvelopeRecord {
            ciphertext,
            wrapped,
            enc_alg: ENC_ALG.to_string(),
            enc_version: ENC_VERSION,
        })
    }

    /// Decrypt an [`EnvelopeRecord`]: unwrap the DEK → AES-256-GCM decrypt.
    pub async fn open(&self, record: &EnvelopeRecord) -> AppResult<Vec<u8>> {
        let dek = self.km.unwrap_dek(&record.wrapped).await?;
        let cipher = Encryptor::from_bytes(&dek)?;
        cipher.decrypt(&record.ciphertext)
    }

    /// Convenience: seal a JSON value (serialised compactly).
    pub async fn seal_json(&self, value: &serde_json::Value) -> AppResult<EnvelopeRecord> {
        let bytes = serde_json::to_vec(value)
            .map_err(|e| crate::error::AppError::Encryption(format!("serialize: {e}")))?;
        self.seal(&bytes).await
    }

    /// Convenience: open into a JSON value.
    pub async fn open_json(&self, record: &EnvelopeRecord) -> AppResult<serde_json::Value> {
        let bytes = self.open(record).await?;
        serde_json::from_slice(&bytes)
            .map_err(|e| crate::error::AppError::Encryption(format!("deserialize: {e}")))
    }

    /// Seal a JSON value directly into the on-storage string form (the value to
    /// persist in `data_encrypted` / `data`).
    pub async fn seal_json_to_storage(&self, value: &serde_json::Value) -> AppResult<String> {
        Ok(self.seal_json(value).await?.to_storage_string())
    }

    /// Open an on-storage value into JSON.
    pub async fn open_storage_json(&self, raw: &str) -> AppResult<serde_json::Value> {
        let record = EnvelopeRecord::from_storage_str(raw)?;
        self.open_json(&record).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::keymanager::LocalDevKms;

    fn cipher() -> EnvelopeCipher {
        let key = Encryptor::generate_key_base64();
        let km = LocalDevKms::from_master_key_base64(&key).unwrap();
        EnvelopeCipher::new(Arc::new(km))
    }

    #[tokio::test]
    async fn seal_open_round_trips() {
        let c = cipher();
        let pt = b"super-secret-password";
        let rec = c.seal(pt).await.unwrap();
        assert_eq!(rec.enc_version, ENC_VERSION);
        assert_eq!(rec.enc_alg, ENC_ALG);
        // Ciphertext is not the plaintext, and the wrapped DEK is present.
        assert_ne!(rec.ciphertext, pt);
        assert!(!rec.wrapped.ciphertext.is_empty());
        let out = c.open(&rec).await.unwrap();
        assert_eq!(out, pt);
    }

    #[tokio::test]
    async fn two_seals_use_distinct_deks() {
        let c = cipher();
        let a = c.seal(b"x").await.unwrap();
        let b = c.seal(b"x").await.unwrap();
        // Different per-record DEKs → different wrapped DEKs and ciphertexts.
        assert_ne!(a.wrapped.ciphertext, b.wrapped.ciphertext);
        assert_ne!(a.ciphertext, b.ciphertext);
    }

    #[tokio::test]
    async fn seal_open_json() {
        let c = cipher();
        let v = serde_json::json!({"db_host": "pg", "db_password": "p@ss"});
        let rec = c.seal_json(&v).await.unwrap();
        let out = c.open_json(&rec).await.unwrap();
        assert_eq!(out, v);
    }

    #[tokio::test]
    async fn record_from_another_kek_does_not_open() {
        let c1 = cipher();
        let c2 = cipher();
        let rec = c1.seal(b"secret").await.unwrap();
        assert!(c2.open(&rec).await.is_err());
    }

    #[tokio::test]
    async fn storage_string_round_trips() {
        let c = cipher();
        let v = serde_json::json!({"db_host": "pg", "db_password": "p@ss;'"});
        let stored = c.seal_json_to_storage(&v).await.unwrap();
        // Self-describing JSON carrying the format version + KEK identity.
        assert!(stored.contains("\"v\":1"));
        assert!(stored.contains("\"local\""));
        let out = c.open_storage_json(&stored).await.unwrap();
        assert_eq!(out, v);
    }

    #[tokio::test]
    async fn non_envelope_storage_value_errors() {
        let c = cipher();
        // A legacy/garbage value is not a wallet envelope → error (forward-only).
        assert!(c.open_storage_json("AAAAlegacy-base64==").await.is_err());
    }
}
