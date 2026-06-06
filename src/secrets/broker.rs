//! Cross-region broker (Secrets Wallet Phase 6e,
//! [`noetl/ai-meta#61`](https://github.com/noetl/ai-meta/issues/61)).
//!
//! Phase 6c's residency gate is **fail-closed**: a server in `us-east-1`
//! denied a credential whose home is `eu-central-1` returns HTTP 403 —
//! the workflow step fails.  That's correct for hard-isolation use cases,
//! but the more common operational shape is "the credential should be
//! resolved IN the EU and the cleartext should never leave EU memory,
//! but the worker that needs it happens to run in US."
//!
//! Phase 6e wires this pattern up by chaining residency-denied
//! resolutions through a **broker server in the credential's home
//! region** that re-seals the result to the requesting worker via the
//! Phase-5 sealing primitives.  Only the sealed envelope crosses the
//! wire; only the addressed worker can open it.
//!
//! Two halves:
//!
//! - **Configuration ([`BrokerRegistry`])** — a `region → broker_url`
//!   map declaring which peer server serves each region.  Loaded from
//!   `NOETL_SECRET_BROKER_REGISTRY` env (JSON object).  Empty by
//!   default; deployments without a broker keep the pre-6e fail-closed
//!   behaviour.
//! - **Client ([`BrokerClient`])** — forwards a sealed-credential
//!   request to a peer.  Carries the asking worker's pubkey across so
//!   the peer can seal directly to the worker (no double-hop unseal).
//!
//! The matching broker-side endpoint lives in
//! `src/handlers/cross_region.rs`.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use crate::crypto::SealedEnvelope;
use crate::error::{AppError, AppResult};

/// Process-global registry singleton — read once from
/// `NOETL_SECRET_BROKER_REGISTRY` env at startup.  Empty when the env is
/// unset / unparseable (legacy mode; pre-6e fail-closed behaviour).
pub fn registry() -> &'static BrokerRegistry {
    static R: OnceLock<BrokerRegistry> = OnceLock::new();
    R.get_or_init(BrokerRegistry::from_env)
}

/// `region → broker_url` map.  Empty if `NOETL_SECRET_BROKER_REGISTRY`
/// is unset or doesn't parse as a JSON object.
#[derive(Debug, Clone, Default)]
pub struct BrokerRegistry {
    inner: HashMap<String, String>,
}

impl BrokerRegistry {
    /// New registry from `NOETL_SECRET_BROKER_REGISTRY` env (JSON object,
    /// e.g. `{"eu-central-1":"https://noetl-broker-eu.example.com"}`).
    pub fn from_env() -> Self {
        let raw = match std::env::var("NOETL_SECRET_BROKER_REGISTRY") {
            Ok(s) if !s.is_empty() => s,
            _ => return Self::default(),
        };
        let parsed: HashMap<String, String> = match serde_json::from_str(&raw) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "NOETL_SECRET_BROKER_REGISTRY: failed to parse as JSON object; treating as empty"
                );
                return Self::default();
            }
        };
        // Filter out empty values / placeholders to keep the lookup honest.
        let inner = parsed.into_iter().filter(|(_, v)| !v.is_empty()).collect();
        Self { inner }
    }

    /// Test-only constructor with an explicit map.
    #[cfg(test)]
    pub fn from_map(inner: HashMap<String, String>) -> Self {
        Self { inner }
    }

    /// Look up the broker URL for a region.  Returns `None` when no
    /// broker is configured for that region (the caller treats this as
    /// "no fallback available" and bubbles the residency violation up).
    pub fn broker_for(&self, region: &str) -> Option<&str> {
        self.inner.get(region).map(|s| s.as_str())
    }

    /// Number of configured brokers.  Test + diagnostic use.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.len()
    }
}

/// Body of the cross-region resolution request.
///
/// Sent by the requesting server to the broker (the peer that owns the
/// credential's home region).  The broker validates the request,
/// resolves the credential locally, and returns a [`SealedEnvelope`]
/// addressed to the requesting worker (so the response never carries
/// cleartext across the region boundary).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CrossRegionResolveRequest {
    /// Credential identifier (alias or name) — same shape as the
    /// `identifier` path param on `/api/credentials/{identifier}/sealed`.
    pub alias: String,
    /// Base64 X25519 public key the broker should seal to.  This is the
    /// requesting worker's `runtime.worker_public_key` (Phase 5b);
    /// forwarded across the broker call so the broker doesn't need to
    /// know about the requesting cluster's worker pool.
    pub worker_public_key_b64: String,
    /// The requesting worker's identity (for audit / future
    /// `secret_audit` table in Phase 7).
    pub worker_id: String,
    /// Forwarded execution context — keeps traces correlated end to end.
    #[serde(default)]
    pub execution_id: Option<i64>,
    #[serde(default)]
    pub parent_execution_id: Option<i64>,
    /// The region the requesting server believes it's calling.  The
    /// broker compares this against its own `server_region()` and
    /// returns 403 on mismatch — defensive against a stale or
    /// misconfigured registry.
    pub expected_entry_region: String,
    /// The region the requesting server lives in (for the broker's
    /// audit log).
    #[serde(default)]
    pub requesting_region: String,
}

/// Client that forwards a cross-region resolution to a broker.
///
/// Holds a single `reqwest::Client` shared across calls — same pattern
/// as the Phase 5b `ControlPlaneClient` so the TLS handshake amortises.
#[derive(Debug, Clone)]
pub struct BrokerClient {
    http: reqwest::Client,
    /// Configured request timeout (mirrored from the env at construction
    /// time; kept here for span attribution and the test surface).
    #[allow(dead_code)]
    timeout: Duration,
}

impl BrokerClient {
    /// New client.  Uses the same rustls-tls backend as the rest of the
    /// stack so mTLS peer-cert handling reuses the existing config.
    pub fn new() -> AppResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(default_timeout_secs()))
            .build()
            .map_err(|e| AppError::Config(format!("cross-region broker: build client: {e}")))?;
        Ok(Self {
            http,
            timeout: Duration::from_secs(default_timeout_secs()),
        })
    }

    /// Call the peer broker.  Returns a [`SealedEnvelope`] on success
    /// or maps every failure mode (timeout, DNS, 5xx, parse) to
    /// [`AppError::CrossRegionUnreachable`] so the caller can decide
    /// whether to bubble up or fall back further.
    pub async fn resolve(
        &self,
        broker_url: &str,
        body: &CrossRegionResolveRequest,
    ) -> AppResult<SealedEnvelope> {
        let url = format!(
            "{}/api/internal/cross-region/resolve",
            broker_url.trim_end_matches('/')
        );
        let resp = self.http.post(&url).json(body).send().await.map_err(|e| {
            AppError::CrossRegionUnreachable {
                broker_url: broker_url.to_string(),
                cause: e.to_string(),
            }
        })?;
        let status = resp.status();
        if !status.is_success() {
            // Pull a short snippet of the response for diagnosis — never
            // assume the broker's error body is well-formed JSON.
            let body_snippet = resp
                .text()
                .await
                .unwrap_or_default()
                .chars()
                .take(200)
                .collect::<String>();
            return Err(AppError::CrossRegionUnreachable {
                broker_url: broker_url.to_string(),
                cause: format!("broker returned HTTP {status}: {body_snippet}"),
            });
        }
        resp.json::<SealedEnvelope>()
            .await
            .map_err(|e| AppError::CrossRegionUnreachable {
                broker_url: broker_url.to_string(),
                cause: format!("decode broker response: {e}"),
            })
    }

    /// Read-only access for tests / span attribution.
    #[cfg(test)]
    pub fn timeout(&self) -> Duration {
        self.timeout
    }
}

fn default_timeout_secs() -> u64 {
    std::env::var("NOETL_SECRET_BROKER_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(10)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_default_is_empty() {
        let r = BrokerRegistry::default();
        assert_eq!(r.len(), 0);
        assert!(r.broker_for("eu-central-1").is_none());
    }

    #[test]
    fn registry_from_map_lookup() {
        let mut m = HashMap::new();
        m.insert("eu-central-1".to_string(), "https://eu.example".to_string());
        m.insert("ap-south-1".to_string(), "https://ap.example".to_string());
        let r = BrokerRegistry::from_map(m);
        assert_eq!(r.len(), 2);
        assert_eq!(r.broker_for("eu-central-1"), Some("https://eu.example"));
        assert_eq!(r.broker_for("ap-south-1"), Some("https://ap.example"));
        assert!(r.broker_for("us-east-1").is_none());
    }

    #[test]
    fn registry_from_env_parses_json() {
        // Restore env after the test to avoid poisoning siblings.
        let saved = std::env::var("NOETL_SECRET_BROKER_REGISTRY").ok();
        unsafe {
            std::env::set_var(
                "NOETL_SECRET_BROKER_REGISTRY",
                r#"{"eu":"https://eu.example","ap":"https://ap.example"}"#,
            )
        };
        let r = BrokerRegistry::from_env();
        assert_eq!(r.len(), 2);
        assert_eq!(r.broker_for("eu"), Some("https://eu.example"));
        match saved {
            Some(v) => unsafe { std::env::set_var("NOETL_SECRET_BROKER_REGISTRY", v) },
            None => unsafe { std::env::remove_var("NOETL_SECRET_BROKER_REGISTRY") },
        }
    }

    #[test]
    fn registry_from_env_empty_when_unset() {
        let saved = std::env::var("NOETL_SECRET_BROKER_REGISTRY").ok();
        unsafe { std::env::remove_var("NOETL_SECRET_BROKER_REGISTRY") };
        let r = BrokerRegistry::from_env();
        assert_eq!(r.len(), 0);
        if let Some(v) = saved {
            unsafe { std::env::set_var("NOETL_SECRET_BROKER_REGISTRY", v) };
        }
    }

    #[test]
    fn registry_from_env_empty_when_invalid_json() {
        let saved = std::env::var("NOETL_SECRET_BROKER_REGISTRY").ok();
        unsafe { std::env::set_var("NOETL_SECRET_BROKER_REGISTRY", "not-json") };
        let r = BrokerRegistry::from_env();
        assert_eq!(r.len(), 0);
        match saved {
            Some(v) => unsafe { std::env::set_var("NOETL_SECRET_BROKER_REGISTRY", v) },
            None => unsafe { std::env::remove_var("NOETL_SECRET_BROKER_REGISTRY") },
        }
    }

    #[test]
    fn registry_drops_empty_values() {
        let saved = std::env::var("NOETL_SECRET_BROKER_REGISTRY").ok();
        unsafe {
            std::env::set_var(
                "NOETL_SECRET_BROKER_REGISTRY",
                r#"{"eu":"","ap":"https://ap.example"}"#,
            )
        };
        let r = BrokerRegistry::from_env();
        // Empty value for `eu` filtered out; only `ap` survives.
        assert_eq!(r.len(), 1);
        assert!(r.broker_for("eu").is_none());
        assert_eq!(r.broker_for("ap"), Some("https://ap.example"));
        match saved {
            Some(v) => unsafe { std::env::set_var("NOETL_SECRET_BROKER_REGISTRY", v) },
            None => unsafe { std::env::remove_var("NOETL_SECRET_BROKER_REGISTRY") },
        }
    }

    #[test]
    fn broker_client_builds() {
        // Just exercise the constructor — no real network round-trip.
        let _c = BrokerClient::new().expect("client builds");
    }

    #[test]
    fn broker_url_trailing_slash_handled() {
        // The client trims a trailing slash when building the path so
        // configurators don't have to be punctuation-perfect.  We can't
        // run a real HTTP call without a mock server, but we can spot-
        // check the format helper.
        let raw = "https://eu.example/";
        let url = format!(
            "{}/api/internal/cross-region/resolve",
            raw.trim_end_matches('/')
        );
        assert_eq!(url, "https://eu.example/api/internal/cross-region/resolve");
    }
}
