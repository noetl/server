//! **NoETL instance identity**: an ML-DSA-87 key pair, and an id derived from its public key.
//!
//! # The scheme
//!
//! - **Key.** ML-DSA-87 (FIPS 204, NIST category 5). The secret is the 32-byte FIPS 204 seed,
//!   which the instance keeps in a local file it never sends.
//! - **Id.** `ni-` + the first 16 bytes of `SHA-256(public_key)`, in hex: `ni-` plus 32 hex
//!   characters. The id is *self-certifying*: whoever proves possession of the key behind it
//!   owns it, so there is no enrollment step that could hand the same id to two keys. It fits
//!   the D8 and ehgraph id charsets as is.
//! - **Signed envelope.** Every request an instance makes carries
//!   `{alg, public_key, payload, signature}`. `payload` is a JSON **string**, and the
//!   signature covers its exact bytes. Nothing is canonicalised, so there is no
//!   canonicalisation for two implementations to disagree on.
//! - **Context.** FIPS 204 domain separation with [`CONTEXT`], so a NoETL instance signature
//!   can never be replayed as a signature for any other protocol that uses the same key type.
//! - **Deterministic signing** (FIPS 204 §3.4, "hedged" off). The same seed and payload give
//!   byte-identical signatures, which is what lets the CLI and the server share a test
//!   vector. Determinism costs nothing here: every payload carries a fresh nonce.
//!
//! # What this file must not grow
//!
//! It is mirrored in noetl/cli (`src/instance/identity.rs`). Both copies carry the same test
//! vector ([`tests::the_shared_test_vector_holds`]); if one side changes the wire format, that
//! test fails on the side that did not.

use base64::Engine;
use ml_dsa::{EncodedSignature, EncodedVerifyingKey, Keypair, MlDsa87, Signature, SigningKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The only algorithm accepted. A field rather than an assumption, so a future algorithm is a
/// new value rather than a silent reinterpretation of old bytes.
pub const ALG: &str = "ML-DSA-87";
/// FIPS 204 context string. Bump the version suffix on any wire-format change.
pub const CONTEXT: &[u8] = b"noetl-instance/v1";
/// ML-DSA-87 public key length (FIPS 204 Table 2).
pub const PUBLIC_KEY_LEN: usize = 2592;
/// ML-DSA-87 signature length (FIPS 204 Table 2).
pub const SIGNATURE_LEN: usize = 4627;
/// Upper bound on a signed payload. Registration bodies are small; this stops an unauthenticated
/// caller from making the server hash a megabyte before the signature check fails.
pub const MAX_PAYLOAD_BYTES: usize = 256 * 1024;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// `ni-` + hex(SHA-256(public_key)[..16]).
pub fn instance_id_for(public_key: &[u8]) -> String {
    let digest = Sha256::digest(public_key);
    format!("ni-{}", hex::encode(&digest[..16]))
}

/// Does `s` look like an instance id? Charset and length only; possession is proved elsewhere.
pub fn is_instance_id(s: &str) -> bool {
    s.len() == 35
        && s.starts_with("ni-")
        && s[3..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// What travels on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedEnvelope {
    pub alg: String,
    /// Base64 (standard, padded) of the 2592-byte public key.
    pub public_key: String,
    /// The signed JSON document, as a string.
    pub payload: String,
    /// Base64 (standard, padded) of the 4627-byte signature.
    pub signature: String,
}

/// An instance's signing key. Holds the seed; zeroised on drop by `ml-dsa`.
pub struct InstanceKey {
    sk: SigningKey<MlDsa87>,
    public_key: Vec<u8>,
    id: String,
}

impl std::fmt::Debug for InstanceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never the seed.
        f.debug_struct("InstanceKey").field("id", &self.id).finish()
    }
}

impl InstanceKey {
    /// The key for a 32-byte FIPS 204 seed.
    pub fn from_seed(seed: [u8; 32]) -> Self {
        let sk = SigningKey::<MlDsa87>::from_seed(&seed.into());
        let public_key = sk.verifying_key().encode().as_slice().to_vec();
        let id = instance_id_for(&public_key);
        InstanceKey { sk, public_key, id }
    }

    /// A fresh key from the OS RNG.
    pub fn generate() -> Self {
        use rand::RngCore;
        let mut seed = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut seed);
        let key = Self::from_seed(seed);
        zeroize::Zeroize::zeroize(&mut seed);
        key
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn public_key(&self) -> &[u8] {
        &self.public_key
    }

    /// Sign `payload` (a JSON document's exact text).
    pub fn sign(&self, payload: String) -> SignedEnvelope {
        let sig = self
            .sk
            .expanded_key()
            .sign_deterministic(payload.as_bytes(), CONTEXT)
            // Only fails for a context over 255 bytes; CONTEXT is a constant well under that.
            .expect("ML-DSA context is a short constant");
        SignedEnvelope {
            alg: ALG.to_string(),
            public_key: B64.encode(&self.public_key),
            payload,
            signature: B64.encode(sig.encode().as_slice()),
        }
    }
}

/// Why an envelope was refused. Every variant is a caller error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityError {
    Algorithm(String),
    Encoding(&'static str),
    PayloadTooLarge(usize),
    BadSignature,
}

impl std::fmt::Display for IdentityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IdentityError::Algorithm(a) => write!(f, "unsupported alg `{a}`; expected {ALG}"),
            IdentityError::Encoding(what) => write!(f, "malformed {what}"),
            IdentityError::PayloadTooLarge(n) => {
                write!(f, "payload is {n} bytes; the limit is {MAX_PAYLOAD_BYTES}")
            }
            IdentityError::BadSignature => write!(f, "signature does not verify"),
        }
    }
}

/// Verify an envelope and return the signer's instance id.
///
/// Only cryptography: freshness, audience, purpose and trust are the caller's checks
/// (see `gate.rs`), because they need server state.
pub fn verify(env: &SignedEnvelope) -> Result<String, IdentityError> {
    if env.alg != ALG {
        return Err(IdentityError::Algorithm(env.alg.clone()));
    }
    if env.payload.len() > MAX_PAYLOAD_BYTES {
        return Err(IdentityError::PayloadTooLarge(env.payload.len()));
    }
    let pk = B64
        .decode(&env.public_key)
        .map_err(|_| IdentityError::Encoding("public_key"))?;
    let pk_arr = EncodedVerifyingKey::<MlDsa87>::try_from(pk.as_slice())
        .map_err(|_| IdentityError::Encoding("public_key length"))?;
    let sig = B64
        .decode(&env.signature)
        .map_err(|_| IdentityError::Encoding("signature"))?;
    let sig_arr = EncodedSignature::<MlDsa87>::try_from(sig.as_slice())
        .map_err(|_| IdentityError::Encoding("signature length"))?;
    let sig = Signature::<MlDsa87>::decode(&sig_arr).ok_or(IdentityError::BadSignature)?;
    let vk = ml_dsa::VerifyingKey::<MlDsa87>::decode(&pk_arr);
    if !vk.verify_with_context(env.payload.as_bytes(), CONTEXT, &sig) {
        return Err(IdentityError::BadSignature);
    }
    Ok(instance_id_for(&pk))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> InstanceKey {
        InstanceKey::from_seed([7u8; 32])
    }

    #[test]
    fn a_signed_envelope_verifies_to_the_key_id() {
        let k = key();
        let env = k.sign(r#"{"hello":"world"}"#.to_string());
        assert_eq!(verify(&env).unwrap(), k.id());
        assert!(is_instance_id(k.id()), "{}", k.id());
        assert_eq!(k.public_key().len(), PUBLIC_KEY_LEN);
    }

    #[test]
    fn any_change_to_the_payload_breaks_the_signature() {
        let mut env = key().sign(r#"{"n":1}"#.to_string());
        env.payload = r#"{"n":2}"#.to_string();
        assert_eq!(verify(&env), Err(IdentityError::BadSignature));
        // Whitespace counts too: the bytes are signed, not a parsed value.
        let mut env = key().sign(r#"{"n":1}"#.to_string());
        env.payload = r#"{ "n":1}"#.to_string();
        assert_eq!(verify(&env), Err(IdentityError::BadSignature));
    }

    #[test]
    fn a_swapped_public_key_cannot_claim_the_signature() {
        let a = key();
        let b = InstanceKey::from_seed([8u8; 32]);
        let mut env = a.sign("{}".to_string());
        env.public_key = B64.encode(b.public_key());
        assert_eq!(verify(&env), Err(IdentityError::BadSignature));
    }

    #[test]
    fn other_algorithms_and_bad_encodings_are_refused() {
        let mut env = key().sign("{}".to_string());
        env.alg = "Ed25519".into();
        assert!(matches!(verify(&env), Err(IdentityError::Algorithm(_))));
        let mut env = key().sign("{}".to_string());
        env.public_key = B64.encode([0u8; 32]);
        assert_eq!(
            verify(&env),
            Err(IdentityError::Encoding("public_key length"))
        );
        let mut env = key().sign("{}".to_string());
        env.signature = "!!".into();
        assert_eq!(verify(&env), Err(IdentityError::Encoding("signature")));
        let env = key().sign("x".repeat(MAX_PAYLOAD_BYTES + 1));
        assert!(matches!(
            verify(&env),
            Err(IdentityError::PayloadTooLarge(_))
        ));
    }

    #[test]
    fn signing_is_deterministic_per_seed_and_payload() {
        let a = key().sign("{}".to_string());
        let b = key().sign("{}".to_string());
        assert_eq!(a, b);
    }

    /// ⚠ The same vector lives in noetl/cli `src/instance/identity.rs`. A failure here means
    /// the wire format moved: change both sides, and bump [`CONTEXT`].
    #[test]
    fn the_shared_test_vector_holds() {
        let k = InstanceKey::from_seed([42u8; 32]);
        let env = k.sign(r#"{"v":1}"#.to_string());
        let sig = B64.decode(&env.signature).unwrap();
        assert_eq!(sig.len(), SIGNATURE_LEN);
        assert_eq!(k.id(), VECTOR_ID);
        assert_eq!(hex::encode(Sha256::digest(&sig)), VECTOR_SIG_SHA256);
    }

    const VECTOR_ID: &str = "ni-d43128fa8a8c785c1d44c9e7db538dbf";
    const VECTOR_SIG_SHA256: &str =
        "eb0f1a02cf7a5ac0bbe1122faa1b7ed200f7c2cc0058ac418f1c1fd60f7e615d";
}
