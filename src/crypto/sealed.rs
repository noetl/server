//! Sealed payload primitives (Secrets Wallet Phase 5a, noetl/ai-meta#61).
//!
//! Defense in depth on top of the Phase-4 mTLS transport (server v2.30.0 / worker
//! v5.12.0 / ops 4c+4d).  mTLS encrypts the *wire*; sealing encrypts the *payload*
//! to a key only the recipient worker holds.  The cleartext never enters the
//! response body; an operator with kubectl exec on the server pod sees only the
//! ciphertext (and the briefly-held cleartext during the encrypt call itself).
//!
//! ## Construction
//!
//! Standard ephemeral-static ECDH "sealed box" shape:
//!
//! 1. Recipient (worker) generates a long-lived [`x25519_dalek::StaticSecret`]
//!    at startup and registers its [`PublicKey`] with the server (Phase 5b).
//! 2. Sender (server) at seal time generates a fresh
//!    [`x25519_dalek::EphemeralSecret`], computes `shared = ECDH(eph_sk, recipient_pk)`,
//!    derives a 32-byte key + 12-byte nonce via **HKDF-SHA256** with a
//!    domain-separating info string (`alg` + `v`), and AEAD-encrypts with
//!    **ChaCha20-Poly1305**.  The associated data ("AAD") binds the algorithm
//!    string + version so a future algorithm change rejects forged-as-old
//!    envelopes with a clean auth failure.
//! 3. Wire envelope carries the ephemeral public key + AEAD ciphertext.  The
//!    recipient runs the same ECDH + HKDF + AEAD-decrypt in reverse.
//!
//! The seal is **one-shot**: the ephemeral secret is consumed by the ECDH agree
//! and dropped; the same `recipient_pk` may be used to seal arbitrarily many
//! independent payloads without compromise.  No state is persisted on the sender.
//!
//! ## Wire format
//!
//! [`SealedEnvelope`] serializes as JSON with all binary fields base64-encoded
//! (standard padded `base64::STANDARD` alphabet — easy to embed in HTTP response
//! bodies and YAML test fixtures):
//!
//! ```json
//! {
//!   "alg": "x25519-hkdf-sha256-chacha20-poly1305",
//!   "v": 1,
//!   "eph_pub":    "<32 bytes b64>",
//!   "ciphertext": "<n+16 bytes b64>"
//! }
//! ```
//!
//! The nonce is **derived** from the ECDH shared secret via HKDF (not a separate
//! field): one ephemeral key per call gives a unique shared secret + unique
//! derived nonce, so nonce reuse is structurally impossible.  Including the
//! nonce on the wire would only invite confusion about whether the recipient
//! checks it (the AAD already pins `alg` + `v`).
//!
//! Phase 5b wires this into the credential-fetch endpoint; Phase 5c is the
//! worker-side ephemeral keypair + unseal + `zeroize`-after-use.

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use x25519_dalek::{EphemeralSecret, PublicKey, StaticSecret};

use crate::error::{AppError, AppResult};

/// Algorithm identifier carried on the wire + bound into the AEAD AAD.
///
/// A future revision (different curve / KDF / cipher) MUST mint a new string
/// here; mismatch on `open` is a clean auth failure rather than a silent
/// downgrade.
pub const SEAL_ALG: &str = "x25519-hkdf-sha256-chacha20-poly1305";

/// Format version.  Bumped only when the wire shape changes (the algorithm
/// constant is the orthogonal control for crypto changes).
pub const SEAL_V: u8 = 1;

/// HKDF "info" prefix.  Domain-separates this scheme from any other use of
/// the same shared secret elsewhere in the stack.
const KDF_INFO: &[u8] = b"noetl-sealed-v1";

/// Sealed envelope as it travels on the wire.
///
/// All binary fields are base64-encoded (`base64::STANDARD`, padded).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SealedEnvelope {
    /// Algorithm id ([`SEAL_ALG`]).  Bound into AAD; tamper rejects.
    pub alg: String,
    /// Format version ([`SEAL_V`]).  Bound into AAD; tamper rejects.
    pub v: u8,
    /// Ephemeral X25519 public key (32 bytes, base64).
    pub eph_pub: String,
    /// AEAD ciphertext (plaintext-len + 16-byte tag, base64).
    pub ciphertext: String,
}

/// Seal `plaintext` to `recipient_pk`.
///
/// Generates a fresh ephemeral X25519 keypair, computes the ECDH shared secret,
/// derives a 32-byte encryption key + 12-byte AEAD nonce via HKDF-SHA256, and
/// encrypts with ChaCha20-Poly1305.  The associated data binds [`SEAL_ALG`] +
/// [`SEAL_V`].
pub fn seal(recipient_pk: &PublicKey, plaintext: &[u8]) -> AppResult<SealedEnvelope> {
    let eph_sk = EphemeralSecret::random_from_rng(rand_core::OsRng);
    let eph_pk = PublicKey::from(&eph_sk);
    let shared = eph_sk.diffie_hellman(recipient_pk);

    let (key, nonce) = derive_key_nonce(shared.as_bytes())?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: associated_data().as_bytes(),
            },
        )
        .map_err(|e| AppError::Internal(format!("sealed encrypt: {e}")))?;

    Ok(SealedEnvelope {
        alg: SEAL_ALG.to_string(),
        v: SEAL_V,
        eph_pub: B64.encode(eph_pk.as_bytes()),
        ciphertext: B64.encode(&ciphertext),
    })
}

/// Open a [`SealedEnvelope`] addressed to `recipient_sk`.
///
/// Reverses `seal`: decodes the wire fields, reconstructs the ECDH shared
/// secret, re-derives the key + nonce, and verifies + decrypts the AEAD.
/// Returns the plaintext bytes; the caller is responsible for `zeroize`ing
/// them after use (the worker-side integration in Phase 5c).
pub fn open(recipient_sk: &StaticSecret, env: &SealedEnvelope) -> AppResult<Vec<u8>> {
    if env.alg != SEAL_ALG {
        return Err(AppError::BadRequest(format!(
            "sealed open: unsupported alg '{}' (expected '{SEAL_ALG}')",
            env.alg
        )));
    }
    if env.v != SEAL_V {
        return Err(AppError::BadRequest(format!(
            "sealed open: unsupported version {} (expected {SEAL_V})",
            env.v
        )));
    }
    let eph_pub_bytes = B64
        .decode(&env.eph_pub)
        .map_err(|e| AppError::BadRequest(format!("sealed open: eph_pub base64: {e}")))?;
    let eph_pub_array: [u8; 32] = eph_pub_bytes.as_slice().try_into().map_err(|_| {
        AppError::BadRequest(format!(
            "sealed open: eph_pub must be 32 bytes, got {}",
            eph_pub_bytes.len()
        ))
    })?;
    let eph_pk = PublicKey::from(eph_pub_array);
    let ciphertext = B64
        .decode(&env.ciphertext)
        .map_err(|e| AppError::BadRequest(format!("sealed open: ciphertext base64: {e}")))?;

    let shared = recipient_sk.diffie_hellman(&eph_pk);
    let (key, nonce) = derive_key_nonce(shared.as_bytes())?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(&key));
    cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &ciphertext,
                aad: associated_data().as_bytes(),
            },
        )
        .map_err(|e| AppError::BadRequest(format!("sealed open: AEAD verify/decrypt: {e}")))
}

/// Domain-separated HKDF that produces a 32-byte AEAD key + 12-byte nonce from
/// the ECDH shared secret.
fn derive_key_nonce(shared: &[u8; 32]) -> AppResult<([u8; 32], [u8; 12])> {
    let hkdf = Hkdf::<Sha256>::new(None, shared);
    let mut okm = [0u8; 32 + 12];
    hkdf.expand(KDF_INFO, &mut okm)
        .map_err(|e| AppError::Internal(format!("sealed kdf: {e}")))?;
    let mut key = [0u8; 32];
    let mut nonce = [0u8; 12];
    key.copy_from_slice(&okm[..32]);
    nonce.copy_from_slice(&okm[32..]);
    Ok((key, nonce))
}

/// Associated data bound into the AEAD; rejects forged-as-old envelopes when
/// the algorithm or version constants change.
fn associated_data() -> String {
    format!("{SEAL_ALG}|v={SEAL_V}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: generate a recipient long-lived keypair.
    fn recipient_keypair() -> (StaticSecret, PublicKey) {
        let sk = StaticSecret::random_from_rng(rand_core::OsRng);
        let pk = PublicKey::from(&sk);
        (sk, pk)
    }

    #[test]
    fn round_trip_short_payload() {
        let (sk, pk) = recipient_keypair();
        let env = seal(&pk, b"hello-secret").unwrap();
        let opened = open(&sk, &env).unwrap();
        assert_eq!(opened, b"hello-secret");
    }

    #[test]
    fn round_trip_realistic_credential_payload() {
        // A multi-field credential JSON — the shape a `get_credential`
        // response will seal in Phase 5b.
        let (sk, pk) = recipient_keypair();
        let plaintext =
            br#"{"type":"bearer","data":{"token":"sk-test-AbCdEf123","expires_in":3600}}"#;
        let env = seal(&pk, plaintext).unwrap();
        let opened = open(&sk, &env).unwrap();
        assert_eq!(&opened[..], plaintext);
    }

    #[test]
    fn envelope_uses_documented_wire_constants() {
        let (_, pk) = recipient_keypair();
        let env = seal(&pk, b"x").unwrap();
        assert_eq!(env.alg, SEAL_ALG);
        assert_eq!(env.v, SEAL_V);
        let eph = B64.decode(env.eph_pub).unwrap();
        assert_eq!(eph.len(), 32, "eph_pub is 32 bytes (X25519)");
        let ct = B64.decode(env.ciphertext).unwrap();
        // ChaCha20-Poly1305 tag is 16 bytes; plaintext is 1 byte ⇒ ct = 17.
        assert_eq!(ct.len(), 1 + 16);
    }

    #[test]
    fn two_seals_to_same_recipient_use_distinct_eph_keys() {
        // Each call generates a fresh ephemeral key, so the wire envelopes
        // differ even when sealing the same plaintext to the same recipient.
        // Critically this means the derived nonce + key differ too, so AEAD
        // nonce reuse is structurally impossible across calls.
        let (_, pk) = recipient_keypair();
        let a = seal(&pk, b"same-input").unwrap();
        let b = seal(&pk, b"same-input").unwrap();
        assert_ne!(a.eph_pub, b.eph_pub);
        assert_ne!(a.ciphertext, b.ciphertext);
    }

    #[test]
    fn open_rejects_tampered_ciphertext() {
        let (sk, pk) = recipient_keypair();
        let mut env = seal(&pk, b"important").unwrap();
        // Flip a byte in the ciphertext.
        let mut ct = B64.decode(&env.ciphertext).unwrap();
        ct[0] ^= 0x01;
        env.ciphertext = B64.encode(&ct);
        let err = open(&sk, &env).unwrap_err();
        assert!(format!("{err:?}").contains("AEAD verify/decrypt"));
    }

    #[test]
    fn open_rejects_tampered_eph_pub() {
        let (sk, pk) = recipient_keypair();
        let mut env = seal(&pk, b"important").unwrap();
        // Substitute a different (valid) ephemeral pub — derived key + nonce
        // shift, decrypt fails.
        let (_, other_pk) = recipient_keypair();
        env.eph_pub = B64.encode(other_pk.as_bytes());
        let err = open(&sk, &env).unwrap_err();
        assert!(format!("{err:?}").contains("AEAD verify/decrypt"));
    }

    #[test]
    fn open_rejects_wrong_recipient() {
        // Wrong recipient → wrong shared secret → wrong key/nonce → AEAD
        // verify fails.
        let (_alice_sk, alice_pk) = recipient_keypair();
        let (bob_sk, _bob_pk) = recipient_keypair();
        let env = seal(&alice_pk, b"for-alice").unwrap();
        let err = open(&bob_sk, &env).unwrap_err();
        assert!(format!("{err:?}").contains("AEAD verify/decrypt"));
    }

    #[test]
    fn open_rejects_unknown_alg() {
        let (sk, pk) = recipient_keypair();
        let mut env = seal(&pk, b"x").unwrap();
        env.alg = "x25519-hkdf-sha256-aes-gcm-v2".to_string();
        let err = open(&sk, &env).unwrap_err();
        assert!(format!("{err:?}").contains("unsupported alg"));
    }

    #[test]
    fn open_rejects_wrong_version() {
        let (sk, pk) = recipient_keypair();
        let mut env = seal(&pk, b"x").unwrap();
        env.v = SEAL_V + 7;
        let err = open(&sk, &env).unwrap_err();
        assert!(format!("{err:?}").contains("unsupported version"));
    }

    #[test]
    fn open_rejects_short_eph_pub() {
        let (sk, pk) = recipient_keypair();
        let mut env = seal(&pk, b"x").unwrap();
        env.eph_pub = B64.encode([0u8; 16]); // wrong length
        let err = open(&sk, &env).unwrap_err();
        assert!(format!("{err:?}").contains("eph_pub must be 32 bytes"));
    }

    #[test]
    fn open_rejects_invalid_base64() {
        let (sk, pk) = recipient_keypair();
        let mut env = seal(&pk, b"x").unwrap();
        env.ciphertext = "%not base64%".to_string();
        let err = open(&sk, &env).unwrap_err();
        assert!(format!("{err:?}").contains("ciphertext base64"));
    }

    #[test]
    fn envelope_round_trips_through_json() {
        // Wire stability: serializing + parsing a SealedEnvelope must not
        // change the bytes the recipient sees.
        let (sk, pk) = recipient_keypair();
        let env = seal(&pk, b"json-stable").unwrap();
        let json = serde_json::to_string(&env).unwrap();
        let parsed: SealedEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(env, parsed);
        let opened = open(&sk, &parsed).unwrap();
        assert_eq!(opened, b"json-stable");
    }
}
