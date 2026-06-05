//! Cryptography module for the NoETL Control Plane.
//!
//! Provides AES-GCM encryption for credential data, plus envelope encryption
//! (per-record DEK wrapped by a KEK) for the secrets wallet — see
//! [`envelope`] + [`keymanager`] (noetl/ai-meta#61).

pub mod encryption;
pub mod envelope;
pub mod keymanager;

pub use encryption::{decrypt, encrypt, Encryptor};
pub use envelope::{EnvelopeCipher, EnvelopeRecord, ENC_ALG, ENC_VERSION};
pub use keymanager::{KeyManager, LocalDevKms, WrappedDek};
