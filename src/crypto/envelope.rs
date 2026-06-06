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

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
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

    /// Secrets Wallet Phase 7a — re-wrap one stored envelope under the
    /// current KEK version.
    ///
    /// Parses `raw` as a stored envelope.  If `wrapped.key_version` already
    /// equals the [`KeyManager::current_key_version`], returns
    /// [`RewrapOutcome::Skipped`] (the row is already on the current key).
    /// Otherwise unwraps the DEK under whichever historical version
    /// produced it, then re-wraps under the current version, and returns
    /// the new storage string in [`RewrapOutcome::Rewrapped`].
    ///
    /// The plaintext payload is **never reconstructed** — this is a
    /// pure DEK re-wrap.  AES-GCM ciphertext bytes stay byte-identical;
    /// only the `dek` field of the stored envelope changes.  Phase 7a's
    /// rotation job iterates this primitive across the
    /// `noetl.credential` + `noetl.keychain` tables under a per-batch
    /// transaction; 7a.2 will land the actual scan + endpoint plumbing.
    pub async fn rewrap_storage_string(&self, raw: &str) -> AppResult<RewrapOutcome> {
        let record = EnvelopeRecord::from_storage_str(raw)?;
        let current = self.km.current_key_version();
        // Skip the no-op case — the row is already on the active version.
        // This is the common path during a routine rotation sweep, so
        // bailing out without touching the KEK matters.
        if record.wrapped.key_version == current {
            return Ok(RewrapOutcome::Skipped {
                key_version: current.to_string(),
            });
        }
        let dek = self.km.unwrap_dek(&record.wrapped).await?;
        let new_wrapped = self.km.wrap_dek(&dek).await?;
        let new_record = EnvelopeRecord {
            ciphertext: record.ciphertext,
            wrapped: new_wrapped,
            enc_alg: record.enc_alg,
            enc_version: record.enc_version,
        };
        Ok(RewrapOutcome::Rewrapped {
            old_key_version: record.wrapped.key_version,
            new_key_version: current.to_string(),
            new_storage_string: new_record.to_storage_string(),
        })
    }
}

/// Outcome of a Phase-7a rotation pass over one stored envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewrapOutcome {
    /// The record was already wrapped under the current KEK version; no
    /// KMS call happened.
    Skipped { key_version: String },
    /// The record was unwrapped under the old KEK version and re-wrapped
    /// under the current one.  `new_storage_string` is the value to
    /// persist back into the `data_encrypted` column.
    Rewrapped {
        old_key_version: String,
        new_key_version: String,
        new_storage_string: String,
    },
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

    // -------- Phase 7a: KEK rotation primitives --------

    #[tokio::test]
    async fn rewrap_skips_records_already_on_current_version() {
        let c = cipher();
        let v = serde_json::json!({"db_password": "p@ss"});
        let stored = c.seal_json_to_storage(&v).await.unwrap();
        // Same cipher → same KEK version → skip.
        match c.rewrap_storage_string(&stored).await.unwrap() {
            RewrapOutcome::Skipped { key_version } => {
                assert_eq!(key_version, "v1");
            }
            other => panic!("expected Skipped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rewrap_emits_new_envelope_under_current_version_when_older() {
        // Build a record under "v1" then move the manager to "v2" — same
        // master key (LocalDevKms uses one Encryptor for all versions, so
        // unwrap works across versions in this test); the rewrap primitive
        // should produce a fresh storage string tagged "v2".
        let key = Encryptor::generate_key_base64();
        let km_v1 = LocalDevKms::from_master_key_base64(&key).unwrap();
        let c_v1 = EnvelopeCipher::new(Arc::new(km_v1));
        let stored_v1 = c_v1
            .seal_json_to_storage(&serde_json::json!({"a": 1}))
            .await
            .unwrap();

        // Build a "v2" manager via the test-only constructor.  The KEK
        // bytes are identical (same master_key_base64), so unwrap still
        // succeeds even though the version label differs — this models
        // a KMS key-version bump where the previous version is still
        // available for unwrap.
        let km_v2 = LocalDevKms::from_master_key_base64_with_version(&key, "v2").unwrap();
        let c_v2 = EnvelopeCipher::new(Arc::new(km_v2));

        match c_v2.rewrap_storage_string(&stored_v1).await.unwrap() {
            RewrapOutcome::Rewrapped {
                old_key_version,
                new_key_version,
                new_storage_string,
            } => {
                assert_eq!(old_key_version, "v1");
                assert_eq!(new_key_version, "v2");
                // The new envelope decrypts to the same plaintext.
                let opened = c_v2.open_storage_json(&new_storage_string).await.unwrap();
                assert_eq!(opened, serde_json::json!({"a": 1}));
                // The new storage string carries the new version tag.
                assert!(new_storage_string.contains("\"kv\":\"v2\""));
            }
            other => panic!("expected Rewrapped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rewrap_rejects_non_envelope_storage_value() {
        let c = cipher();
        // Same forward-only contract as `open_storage_json` — bad input
        // bubbles up as an Encryption error rather than silently being
        // treated as up-to-date.
        let err = c
            .rewrap_storage_string("not-a-wallet-envelope")
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("not a wallet envelope"));
    }

    #[test]
    fn local_kms_reports_its_key_version() {
        let key = Encryptor::generate_key_base64();
        let km = LocalDevKms::from_master_key_base64(&key).unwrap();
        // Locked against accidental drift — Phase 7a's rotation primitive
        // depends on this being the same string the manager tags
        // `wrap_dek` outputs with.
        assert_eq!(km.current_key_version(), "v1");
    }
}
