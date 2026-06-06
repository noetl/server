//! Opt-in TLS / mTLS for the control-plane HTTP listener
//! (Secrets Wallet Phase 4a, noetl/ai-meta#61).
//!
//! Plain HTTP by default (unchanged).  When `NOETL_TLS_CERT` + `NOETL_TLS_KEY`
//! are set the server serves HTTPS; adding `NOETL_TLS_CLIENT_CA` requires +
//! verifies client certificates (**mTLS**).  This is the transport half of the
//! sealed-delivery work: with mTLS the worker↔server credential channel is
//! authenticated + encrypted, so the resolved secret no longer travels
//! plaintext over the wire (today `GET /api/credentials/<alias>` is plain
//! HTTP).  Payload sealing (encrypting the value to the worker's key) is the
//! companion Phase 5.

use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig};

use crate::error::{AppError, AppResult};

/// TLS file paths resolved from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsParams {
    pub cert_path: String,
    pub key_path: String,
    /// When set, client certs are required + verified against this CA (mTLS).
    pub client_ca_path: Option<String>,
}

impl TlsParams {
    /// Whether mutual TLS (client-cert verification) is enabled.
    pub fn mtls(&self) -> bool {
        self.client_ca_path.is_some()
    }
}

/// Resolve the TLS mode from the environment.
///
/// `None` ⇒ plain HTTP (the default).  Both `NOETL_TLS_CERT` and
/// `NOETL_TLS_KEY` enable HTTPS; `NOETL_TLS_CLIENT_CA` additionally enables
/// mTLS.  Setting exactly one of cert/key is a misconfiguration (fail fast).
pub fn tls_params_from_env() -> AppResult<Option<TlsParams>> {
    let env = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
    resolve_tls_params(
        env("NOETL_TLS_CERT"),
        env("NOETL_TLS_KEY"),
        env("NOETL_TLS_CLIENT_CA"),
    )
}

/// Pure resolver (testable without touching the process environment).
pub fn resolve_tls_params(
    cert: Option<String>,
    key: Option<String>,
    client_ca: Option<String>,
) -> AppResult<Option<TlsParams>> {
    match (cert, key) {
        (Some(cert_path), Some(key_path)) => Ok(Some(TlsParams {
            cert_path,
            key_path,
            client_ca_path: client_ca,
        })),
        (None, None) => Ok(None),
        _ => Err(AppError::Config(
            "TLS misconfigured: set both NOETL_TLS_CERT and NOETL_TLS_KEY (or neither)".to_string(),
        )),
    }
}

/// Build a rustls [`ServerConfig`] for the listener.
///
/// Installs the `ring` crypto provider (matching the rest of the stack;
/// idempotent).  mTLS — a [`WebPkiClientVerifier`] — is wired when
/// `client_ca_path` is set.
pub fn build_server_config(params: &TlsParams) -> AppResult<ServerConfig> {
    // Idempotent: ignore "a provider is already installed".
    let _ = rustls::crypto::ring::default_provider().install_default();

    let certs = load_certs(&params.cert_path)?;
    let key = load_key(&params.key_path)?;

    let builder = ServerConfig::builder();
    let config = match &params.client_ca_path {
        Some(ca) => {
            let mut roots = RootCertStore::empty();
            for cert in load_certs(ca)? {
                roots.add(cert).map_err(|e| {
                    AppError::Config(format!("TLS: client CA '{ca}' not a valid cert: {e}"))
                })?;
            }
            let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
                .build()
                .map_err(|e| AppError::Config(format!("TLS: client-cert verifier: {e}")))?;
            builder.with_client_cert_verifier(verifier)
        }
        None => builder.with_no_client_auth(),
    };
    config
        .with_single_cert(certs, key)
        .map_err(|e| AppError::Config(format!("TLS: server cert/key: {e}")))
}

fn load_certs(path: &str) -> AppResult<Vec<CertificateDer<'static>>> {
    let file =
        File::open(path).map_err(|e| AppError::Config(format!("TLS: open '{path}': {e}")))?;
    let mut reader = BufReader::new(file);
    let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AppError::Config(format!("TLS: reading certs '{path}': {e}")))?;
    if certs.is_empty() {
        return Err(AppError::Config(format!(
            "TLS: no certificates in '{path}'"
        )));
    }
    Ok(certs)
}

fn load_key(path: &str) -> AppResult<PrivateKeyDer<'static>> {
    let file =
        File::open(path).map_err(|e| AppError::Config(format!("TLS: open '{path}': {e}")))?;
    let mut reader = BufReader::new(file);
    rustls_pemfile::private_key(&mut reader)
        .map_err(|e| AppError::Config(format!("TLS: reading key '{path}': {e}")))?
        .ok_or_else(|| AppError::Config(format!("TLS: no private key in '{path}'")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_none_when_unset() {
        assert!(resolve_tls_params(None, None, None).unwrap().is_none());
    }

    #[test]
    fn resolve_https_without_mtls() {
        let p = resolve_tls_params(Some("c.pem".into()), Some("k.pem".into()), None)
            .unwrap()
            .unwrap();
        assert_eq!(p.cert_path, "c.pem");
        assert_eq!(p.key_path, "k.pem");
        assert!(!p.mtls());
    }

    #[test]
    fn resolve_mtls_when_client_ca_set() {
        let p = resolve_tls_params(
            Some("c.pem".into()),
            Some("k.pem".into()),
            Some("ca.pem".into()),
        )
        .unwrap()
        .unwrap();
        assert!(p.mtls());
        assert_eq!(p.client_ca_path.as_deref(), Some("ca.pem"));
    }

    #[test]
    fn resolve_rejects_partial_cert_key() {
        assert!(resolve_tls_params(Some("c.pem".into()), None, None).is_err());
        assert!(resolve_tls_params(None, Some("k.pem".into()), None).is_err());
    }

    #[test]
    fn build_config_errors_on_missing_cert_file() {
        let params = TlsParams {
            cert_path: "/nonexistent/cert.pem".into(),
            key_path: "/nonexistent/key.pem".into(),
            client_ca_path: None,
        };
        let err = build_server_config(&params).unwrap_err();
        assert!(format!("{err:?}").contains("open"), "got: {err:?}");
    }
}
