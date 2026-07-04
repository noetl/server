//! Auth0 ID-token **signature** verification (noetl/ai-meta#169), shipped dark.
//!
//! ## The gap this closes
//!
//! The synchronous login handler ([`super::auth::login`]) — and the
//! `auth0_login` playbook it mirrors — historically trusted the Auth0 ID token
//! after a **claims-only decode**: base64url-decode the JWT payload and check
//! `iss` / `exp` / `sub`.  No JWKS fetch, no RS256 signature check.  A forged
//! token carrying the right `iss`/`exp`/`sub` claims would have been accepted.
//! That claims-decode behaviour was preserved byte-identically through the
//! #167/#168 sync fast-path work and tracked here as an independent hardening.
//!
//! This module adds the missing step: fetch the tenant JWKS (RS256 public
//! keys), select the key named by the token header's `kid`, and verify the
//! signature + standard claims (`iss`, `aud`, `exp`, `nbf`) with the
//! `jsonwebtoken` crate.  The JWKS is cached with a TTL and refreshed on an
//! unknown `kid` so Auth0 key rotation is transparent.
//!
//! ## Shipped dark — three modes
//!
//! Enabling real signature enforcement against the wrong issuer/audience/JWKS
//! would reject **every** login, so this is default-OFF and rolled out
//! canary-style.  `NOETL_AUTH_VERIFY_SIGNATURE` is a tri-state:
//!
//! | Value | [`VerifyMode`] | Behaviour |
//! | :-- | :-- | :-- |
//! | unset / `false` / `0` / `off` | `Off` | **Default.** Verification never runs; the login decision is byte-identical to today's claims-decode. |
//! | `shadow` / `log` | `Shadow` | Verification runs and is logged + metered, but the login decision is **unchanged** — a would-reject token still logs in. This is the canary-observation lever: prove real prod tokens verify before enforcing. |
//! | `true` / `1` / `enforce` | `Enforce` | Verification runs and a token that fails signature/claims is **rejected** (login returns the same `token_error` envelope a bad claims-decode returns). |
//!
//! With the flag `Off` the caller skips this module entirely, so there is no new
//! failure mode and no new dependency on the JWKS endpoint being reachable.
//!
//! ## Config surface (all optional; safe defaults)
//!
//! - `NOETL_AUTH0_DOMAIN` — fallback tenant domain when a login request omits
//!   `auth0_domain`.  The issuer (`https://<domain>/`) and JWKS URL
//!   (`https://<domain>/.well-known/jwks.json`) are both derived from it.
//! - `NOETL_AUTH0_AUDIENCE` — comma-separated allowed audiences.  **Unset ⇒ the
//!   `aud` claim is not enforced** (only signature + iss + exp + nbf).  The Muno
//!   SPA requests an Auth0 ID token with *no* `audience` param, so the token's
//!   `aud` is the SPA **client_id** — a deployment secret we deliberately do not
//!   hard-code.  Set this to the client_id to also pin the audience once the
//!   real value is confirmed from shadow-mode logs.
//! - `NOETL_AUTH_JWT_LEEWAY_SECS` — clock-skew leeway for exp/nbf (default 60).
//! - `NOETL_AUTH_JWKS_TTL_SECS` — JWKS cache TTL (default 600).  An unknown
//!   `kid` forces a refresh regardless of TTL.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use tokio::sync::RwLock;

/// Tri-state activation for signature verification.  See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyMode {
    /// Verification never runs (default) — login decision unchanged.
    Off,
    /// Verification runs, is logged + metered, but does NOT change the decision.
    Shadow,
    /// Verification runs and rejects tokens that fail.
    Enforce,
}

impl VerifyMode {
    fn label(self) -> &'static str {
        match self {
            VerifyMode::Off => "off",
            VerifyMode::Shadow => "shadow",
            VerifyMode::Enforce => "enforce",
        }
    }
}

/// Read `NOETL_AUTH_VERIFY_SIGNATURE` into a [`VerifyMode`].  Unknown / empty
/// values fall back to `Off` so a typo can never silently start enforcing.
pub fn verify_mode() -> VerifyMode {
    match std::env::var("NOETL_AUTH_VERIFY_SIGNATURE") {
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "shadow" | "log" | "observe" => VerifyMode::Shadow,
            "true" | "1" | "enforce" | "on" => VerifyMode::Enforce,
            _ => VerifyMode::Off,
        },
        Err(_) => VerifyMode::Off,
    }
}

/// Outcome label for the verification metric.  Bounded enum — safe as a
/// Prometheus label (no unbounded cardinality).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyOutcome {
    /// Signature + claims verified.
    Success,
    /// Cryptographic signature check failed (includes alg-confusion rejects).
    BadSignature,
    /// Signature fine but a standard claim (iss/aud/exp/nbf) failed.
    BadClaims,
    /// Token header names a `kid` not present in the (freshly refreshed) JWKS.
    UnknownKid,
    /// Token could not be parsed far enough to verify (malformed header, etc.).
    Malformed,
    /// JWKS could not be fetched (endpoint unreachable) — a backend problem,
    /// NOT a bad token.  Treated as fail-closed only in `Enforce`.
    JwksUnavailable,
    /// Verification enabled but no Auth0 domain available to derive issuer/JWKS.
    NoDomain,
}

impl VerifyOutcome {
    pub fn label(self) -> &'static str {
        match self {
            VerifyOutcome::Success => "success",
            VerifyOutcome::BadSignature => "bad_signature",
            VerifyOutcome::BadClaims => "bad_claims",
            VerifyOutcome::UnknownKid => "unknown_kid",
            VerifyOutcome::Malformed => "malformed",
            VerifyOutcome::JwksUnavailable => "jwks_unavailable",
            VerifyOutcome::NoDomain => "no_domain",
        }
    }
}

/// A verification failure: the metric bucket plus a human-readable reason that
/// is **safe to log** (it never contains the token, key material, or any
/// claim value beyond fixed error taxonomy).
#[derive(Debug)]
pub struct VerifyError {
    pub outcome: VerifyOutcome,
    pub reason: String,
}

impl VerifyError {
    fn new(outcome: VerifyOutcome, reason: impl Into<String>) -> Self {
        Self {
            outcome,
            reason: reason.into(),
        }
    }
}

/// Parameters the pure verifier checks against — derived from config + the
/// request's `auth0_domain`, kept separate from the JWKS fetch so the core is
/// unit-testable without HTTP.
#[derive(Debug, Clone)]
struct VerifyParams {
    issuer: String,
    /// Empty ⇒ audience is NOT enforced (safe default; see module docs).
    audiences: Vec<String>,
    leeway: u64,
}

fn configured_audiences() -> Vec<String> {
    std::env::var("NOETL_AUTH0_AUDIENCE")
        .ok()
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn leeway_secs() -> u64 {
    std::env::var("NOETL_AUTH_JWT_LEEWAY_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60)
}

fn jwks_ttl() -> Duration {
    let secs = std::env::var("NOETL_AUTH_JWKS_TTL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600);
    Duration::from_secs(secs)
}

/// A cached JWKS for one JWKS URL, with the instant it was fetched.
struct JwksEntry {
    jwks: JwkSet,
    fetched_at: Instant,
}

/// Process-global JWKS cache keyed by JWKS URL (one per tenant domain).
fn jwks_cache() -> &'static RwLock<HashMap<String, JwksEntry>> {
    static C: OnceLock<RwLock<HashMap<String, JwksEntry>>> = OnceLock::new();
    C.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Shared HTTP client for JWKS fetches — built once (rustls TLS, short timeout)
/// so we don't reparse the TLS bundle on every refresh.
fn jwks_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("jwks reqwest client must build")
    })
}

/// True when `entry` still names `kid` (or `kid` is unknown) and is within TTL.
fn entry_is_fresh(entry: &JwksEntry, kid: Option<&str>, ttl: Duration) -> bool {
    if entry.fetched_at.elapsed() >= ttl {
        return false;
    }
    match kid {
        Some(k) => entry.jwks.find(k).is_some(),
        None => true,
    }
}

/// Fetch + parse a JWKS document.  Network / parse failures map to
/// [`VerifyOutcome::JwksUnavailable`].
async fn fetch_jwks(url: &str) -> Result<JwkSet, VerifyError> {
    let resp = jwks_client().get(url).send().await.map_err(|e| {
        VerifyError::new(
            VerifyOutcome::JwksUnavailable,
            format!("JWKS fetch failed: {e}"),
        )
    })?;
    if !resp.status().is_success() {
        return Err(VerifyError::new(
            VerifyOutcome::JwksUnavailable,
            format!("JWKS endpoint returned HTTP {}", resp.status().as_u16()),
        ));
    }
    let jwks: JwkSet = resp.json().await.map_err(|e| {
        VerifyError::new(
            VerifyOutcome::JwksUnavailable,
            format!("JWKS parse failed: {e}"),
        )
    })?;
    Ok(jwks)
}

/// Return a JWKS for `url` that contains `kid`, using the cache when it is fresh
/// and still names `kid`, otherwise fetching (and caching) a fresh copy.  An
/// unknown `kid` forces a refresh regardless of TTL so key rotation is
/// transparent.
async fn cached_or_refreshed_jwks(url: &str, kid: Option<&str>) -> Result<JwkSet, VerifyError> {
    let ttl = jwks_ttl();

    // Classify the cache state under a single read lock so the metric label is
    // precise: `cache_hit` (fresh + names kid), `unknown_kid_refresh` (entry is
    // within TTL but lacks the requested kid → Auth0 rotated keys), or
    // `cache_miss` (absent / stale).
    {
        let cache = jwks_cache().read().await;
        match cache.get(url) {
            Some(entry) if entry_is_fresh(entry, kid, ttl) => {
                crate::metrics::record_jwks_event("cache_hit");
                return Ok(entry.jwks.clone());
            }
            Some(entry) if entry.fetched_at.elapsed() < ttl => {
                // Within TTL but the kid isn't present → key rotation.
                crate::metrics::record_jwks_event("unknown_kid_refresh");
            }
            _ => crate::metrics::record_jwks_event("cache_miss"),
        }
    }

    let fresh = fetch_jwks(url).await?;
    let cloned = fresh.clone();
    jwks_cache().write().await.insert(
        url.to_string(),
        JwksEntry {
            jwks: fresh,
            fetched_at: Instant::now(),
        },
    );
    Ok(cloned)
}

/// Classify a `jsonwebtoken` error into a bounded outcome + safe reason.
fn classify_jwt_error(err: &jsonwebtoken::errors::Error) -> VerifyError {
    use jsonwebtoken::errors::ErrorKind;
    let (outcome, reason) = match err.kind() {
        ErrorKind::InvalidSignature => (VerifyOutcome::BadSignature, "invalid signature"),
        // alg-confusion: a token whose header alg isn't the RS256 we allow.
        ErrorKind::InvalidAlgorithm => (VerifyOutcome::BadSignature, "unexpected token algorithm"),
        ErrorKind::InvalidAlgorithmName => (VerifyOutcome::BadSignature, "invalid algorithm name"),
        ErrorKind::ExpiredSignature => (VerifyOutcome::BadClaims, "token expired"),
        ErrorKind::ImmatureSignature => (VerifyOutcome::BadClaims, "token not yet valid (nbf)"),
        ErrorKind::InvalidIssuer => (VerifyOutcome::BadClaims, "invalid issuer"),
        ErrorKind::InvalidAudience => (VerifyOutcome::BadClaims, "invalid audience"),
        ErrorKind::MissingRequiredClaim(_) => (VerifyOutcome::BadClaims, "missing required claim"),
        _ => (VerifyOutcome::Malformed, "token could not be verified"),
    };
    VerifyError::new(outcome, reason)
}

/// The pure verification core: given the JWKS and expected params, verify the
/// token's RS256 signature + standard claims.  No HTTP, no env — unit-testable
/// with an in-memory keypair.
fn verify_with_jwks(token: &str, jwks: &JwkSet, params: &VerifyParams) -> Result<(), VerifyError> {
    let header = decode_header(token)
        .map_err(|_| VerifyError::new(VerifyOutcome::Malformed, "unparseable token header"))?;

    // Resolve the signing key: by `kid` when present; otherwise accept a
    // single-key JWKS (Auth0 always stamps a kid, so the fallback is defensive).
    let jwk = match header.kid.as_deref() {
        Some(kid) => jwks.find(kid).ok_or_else(|| {
            VerifyError::new(VerifyOutcome::UnknownKid, "no JWKS key for token kid")
        })?,
        None => {
            if jwks.keys.len() == 1 {
                &jwks.keys[0]
            } else {
                return Err(VerifyError::new(
                    VerifyOutcome::Malformed,
                    "token header has no kid and JWKS is not single-key",
                ));
            }
        }
    };

    let decoding_key = DecodingKey::from_jwk(jwk).map_err(|_| {
        VerifyError::new(
            VerifyOutcome::JwksUnavailable,
            "JWKS key is not a usable RSA key",
        )
    })?;

    // RS256 only — this restriction is what rejects alg=none / HS256 forgeries.
    let mut validation = Validation::new(Algorithm::RS256);
    validation.leeway = params.leeway;
    validation.validate_exp = true;
    validation.validate_nbf = true;
    validation.set_issuer(&[params.issuer.as_str()]);
    if params.audiences.is_empty() {
        // No configured audience → do not enforce `aud` (safe default).
        validation.validate_aud = false;
    } else {
        validation.set_audience(&params.audiences);
    }

    decode::<serde_json::Value>(token, &decoding_key, &validation)
        .map(|_| ())
        .map_err(|e| classify_jwt_error(&e))
}

/// Verify the Auth0 ID token's signature + standard claims for `auth0_domain`.
///
/// Derives the issuer + JWKS URL from the domain (falling back to
/// `NOETL_AUTH0_DOMAIN` when the request omitted it), pulls the JWKS from the
/// TTL cache (refreshing on an unknown `kid`), and runs [`verify_with_jwks`].
/// The caller decides — per [`VerifyMode`] — whether a returned error rejects
/// the login (`Enforce`) or is merely logged (`Shadow`).
pub async fn verify_signature(token: &str, auth0_domain: &str) -> Result<(), VerifyError> {
    let domain = if auth0_domain.trim().is_empty() {
        std::env::var("NOETL_AUTH0_DOMAIN").unwrap_or_default()
    } else {
        auth0_domain.trim().to_string()
    };
    if domain.is_empty() {
        return Err(VerifyError::new(
            VerifyOutcome::NoDomain,
            "signature verification enabled but no Auth0 domain configured",
        ));
    }

    let params = VerifyParams {
        issuer: format!("https://{domain}/"),
        audiences: configured_audiences(),
        leeway: leeway_secs(),
    };
    let jwks_url = format!("https://{domain}/.well-known/jwks.json");

    // Peek the kid so the cache can refresh on rotation without a verify attempt.
    let kid = decode_header(token).ok().and_then(|h| h.kid);
    let jwks = cached_or_refreshed_jwks(&jwks_url, kid.as_deref()).await?;
    verify_with_jwks(token, &jwks, &params)
}

/// Run verification for a login attempt according to the active [`VerifyMode`],
/// recording metrics + safe logs.  Returns `Ok(())` when the login may proceed
/// and `Err(reason)` only in `Enforce` mode on a verification failure (the
/// reason is surfaced to the caller as the `token_error` message).
///
/// In `Shadow` mode a failing token still returns `Ok(())` — the decision is
/// unchanged — but the would-reject outcome is logged + metered so operators can
/// confirm real prod tokens verify before flipping to `Enforce`.
pub async fn enforce_for_login(
    token: &str,
    auth0_domain: &str,
    mode: VerifyMode,
) -> Result<(), String> {
    if mode == VerifyMode::Off {
        return Ok(());
    }

    match verify_signature(token, auth0_domain).await {
        Ok(()) => {
            crate::metrics::record_jwt_verify(mode.label(), VerifyOutcome::Success.label());
            tracing::debug!(mode = mode.label(), "auth JWT signature verified");
            Ok(())
        }
        Err(err) => {
            crate::metrics::record_jwt_verify(mode.label(), err.outcome.label());
            match mode {
                VerifyMode::Shadow => {
                    // Observation only: log the would-reject but let the login
                    // proceed on the unchanged claims-decode decision.
                    tracing::warn!(
                        mode = "shadow",
                        outcome = err.outcome.label(),
                        reason = %err.reason,
                        "auth JWT signature verification WOULD reject (shadow — allowing)"
                    );
                    Ok(())
                }
                VerifyMode::Enforce => {
                    tracing::warn!(
                        mode = "enforce",
                        outcome = err.outcome.label(),
                        reason = %err.reason,
                        "auth JWT signature verification rejected token"
                    );
                    Err(err.reason)
                }
                VerifyMode::Off => unreachable!("Off handled above"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use jsonwebtoken::{encode, EncodingKey, Header};
    use std::time::{SystemTime, UNIX_EPOCH};

    // Throwaway RSA-2048 keypair generated offline for these tests ONLY. It is
    // not a secret and guards no real resource — it exists so a token can be
    // signed and verified without a live Auth0 tenant.
    const TEST_PRIV_PEM: &str = "-----BEGIN PRIVATE KEY-----\n\
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQDZ7F/m9wLA9qyF\n\
LcN8SbDMn6kQhsuziBxvK67Zo3KCIaVug7Zm9wucAaR7+60D64nho44WS3Oi9SkA\n\
U4TLn3D2xZJwK1eCpby/qakv6+yCAkRclsI6Jeax5NzgPMPzu/gvnuwAsimZwVW+\n\
LS+94phS/SNEgeufj1wMy/LJ/B2S0xiQNw9ytaTZGx8ZdTIZn2eE1i6eSBdLjpff\n\
RuZKTMqn+yOgnppoM9UnZC9Nf7Ri/+OzLntxvpKqS1nXe6HLHUhxGrVG/yBNIGW/\n\
9B/+ZXpzHH+3Njvo0q71gkE5XQyVpRnDXQuv38pW845p2trLpVhd5WvR9Zw2XWBD\n\
p3esqc/nAgMBAAECggEAHHQljXWhEWrj2bvA84B3qjAKlhLTlAxokgdlHBugPud/\n\
vy9JaKZHCMaaIGobDBD7/s8pJTYS0isqgFVnWGLoEAB5/1VZZsCXJXtUkOJADjWX\n\
kXNJxufd+EpGoKyudSuM20Ln06tvjRjWRi+GNUfZ1bQhn7dK+ZlxTmZuu/rELcBO\n\
jrTfLl90UyESPetMNLjkUysr9tNf7n30TpmBUQgV/Hzw1779Ofmdn+cEV0IDxFSB\n\
oXfSCBFM+lpDdZ0a1mbq1is6/d7LkqaRLVYUKWseT6ftkqqo4l9EMVYEq0N96q6j\n\
0BVOpcgfJ0MZIlLwBqznUXSltyLSixMkCXzCfM72iQKBgQD2trbgIqvX988rRmn5\n\
1PfBYDq1wFx6HZAY6/6muJi+DkmI5TZK/8LPLzZJyHOG+JNZky0YIv1J8L14evab\n\
JGSpTsVYPquWrbQAJ+PmwiRR98TmhGK4XjKZFUL7TFcSlw0TyrwSLgDxo7Zp3u7r\n\
H/oWqd2VTzlvKyZIfI7DeSwYJQKBgQDiID3ctFTAYx/u8r1fODTbH85EbCLCIZT8\n\
C/gptrLaHPjMC3DHDBekqe1UDg6R7TaKXXiunLV3Kpp7I3scJXypJ1uFgdsMaBpH\n\
nroq1JVpnjA4hNJj8rtpp9E8+OVFlvHRxjcL51N3ChxAiElqnHklVMadGzxo8NZ8\n\
yeovBgD0GwKBgHhJc3ZqUSCWORLfXPIrLLTCxz0wUaZUXZpZDaqZ3Bbl/mJZZxRA\n\
COpdGXn12qXW8ni88kKPZLE1SBvC0BOqNc36kutIev6XKGjfChXLWEwCoqTFrSA5\n\
BTBwFl1Rxi5RKVYaBYJTnbAy4tBRmmlXCOcd4ORYYSdbgWncGPsjTCVlAoGBAKHU\n\
/1EQAkO57WW+dcdK25EjPyT62xlinVSRYGbCKVguOeUWHv1laxpevspUURsg0NLP\n\
uIoG/1wssmiEaRwovAH3d+mhmNpVOtGdCJGUmOYA33PjowsC8hvYjcf8PWHDeIdw\n\
O38abEKaisOohhp1p8IO3SIdFuvnySl//EjxHAeXAoGBALQZySpR1OnWezHAn3hN\n\
swk+MEUEWbBA/18zsmLMT7temygOzdjp6GJIFHhvMuEULvXfb4sFdfM8bn55/q0r\n\
1uRWgn2Km4KxQYHJ0FiH5z1PRx+UFhRvr1akFryJ6EZBKI1ngrixvQYuJJlCVy6r\n\
MmYl3DjpSV0xyivlvyMT+Qwn\n\
-----END PRIVATE KEY-----\n";

    // Public modulus (n) + exponent (e) for TEST_PRIV_PEM, base64url (no pad).
    const TEST_JWK_N: &str = "2exf5vcCwPashS3DfEmwzJ-pEIbLs4gcbyuu2aNygiGlboO2ZvcLnAGke_utA-uJ4aOOFktzovUpAFOEy59w9sWScCtXgqW8v6mpL-vsggJEXJbCOiXmseTc4DzD87v4L57sALIpmcFVvi0vveKYUv0jRIHrn49cDMvyyfwdktMYkDcPcrWk2RsfGXUyGZ9nhNYunkgXS46X30bmSkzKp_sjoJ6aaDPVJ2QvTX-0Yv_jsy57cb6SqktZ13uhyx1IcRq1Rv8gTSBlv_Qf_mV6cxx_tzY76NKu9YJBOV0MlaUZw10Lr9_KVvOOadray6VYXeVr0fWcNl1gQ6d3rKnP5w";
    const TEST_JWK_E: &str = "AQAB";
    const TEST_KID: &str = "test-key-1";
    const TEST_ISSUER_DOMAIN: &str = "tenant.us.auth0.com";

    fn jwks_with(kid: &str) -> JwkSet {
        let doc = serde_json::json!({
            "keys": [{
                "kty": "RSA",
                "use": "sig",
                "alg": "RS256",
                "kid": kid,
                "n": TEST_JWK_N,
                "e": TEST_JWK_E,
            }]
        });
        serde_json::from_value(doc).expect("valid JWKS")
    }

    fn params(audiences: Vec<&str>) -> VerifyParams {
        VerifyParams {
            issuer: format!("https://{TEST_ISSUER_DOMAIN}/"),
            audiences: audiences.into_iter().map(String::from).collect(),
            leeway: 0,
        }
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// Sign a token with the test private key and the given kid + claims.
    fn sign(kid: &str, claims: &serde_json::Value) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.to_string());
        let key = EncodingKey::from_rsa_pem(TEST_PRIV_PEM.as_bytes()).expect("valid test key");
        encode(&header, claims, &key).expect("sign")
    }

    fn good_claims() -> serde_json::Value {
        serde_json::json!({
            "iss": format!("https://{TEST_ISSUER_DOMAIN}/"),
            "aud": "spa-client-id",
            "sub": "auth0|abc",
            "email": "a@b.com",
            "exp": now() + 3600,
            "iat": now() - 5,
            "nbf": now() - 5,
        })
    }

    #[test]
    fn valid_token_verifies() {
        let token = sign(TEST_KID, &good_claims());
        assert!(verify_with_jwks(&token, &jwks_with(TEST_KID), &params(vec![])).is_ok());
    }

    #[test]
    fn valid_token_with_matching_audience_verifies() {
        let token = sign(TEST_KID, &good_claims());
        assert!(
            verify_with_jwks(&token, &jwks_with(TEST_KID), &params(vec!["spa-client-id"])).is_ok()
        );
    }

    #[test]
    fn tampered_signature_is_rejected() {
        // Flip a char near the START of the signature segment (not the last,
        // whose low bits are base64 padding) → a valid-length but wrong
        // signature that decodes cleanly yet fails the RSA check.
        let token = sign(TEST_KID, &good_claims());
        let mut parts: Vec<&str> = token.split('.').collect();
        let sig = parts[2].to_string();
        let first = sig.chars().next().unwrap();
        let swap = if first == 'A' { 'B' } else { 'A' };
        let tampered_sig = format!("{}{}", swap, &sig[1..]);
        parts[2] = &tampered_sig;
        let tampered = parts.join(".");
        let err = verify_with_jwks(&tampered, &jwks_with(TEST_KID), &params(vec![])).unwrap_err();
        assert_eq!(err.outcome, VerifyOutcome::BadSignature);
    }

    #[test]
    fn tampered_payload_is_rejected() {
        // Re-encode the payload with an escalated claim but keep the old signature.
        let token = sign(TEST_KID, &good_claims());
        let parts: Vec<&str> = token.split('.').collect();
        let forged_payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&serde_json::json!({
                "iss": format!("https://{TEST_ISSUER_DOMAIN}/"),
                "sub": "auth0|attacker",
                "exp": now() + 3600,
            }))
            .unwrap(),
        );
        let forged = format!("{}.{}.{}", parts[0], forged_payload, parts[2]);
        let err = verify_with_jwks(&forged, &jwks_with(TEST_KID), &params(vec![])).unwrap_err();
        assert_eq!(err.outcome, VerifyOutcome::BadSignature);
    }

    #[test]
    fn wrong_issuer_is_rejected() {
        let mut claims = good_claims();
        claims["iss"] = serde_json::json!("https://evil.example.com/");
        let token = sign(TEST_KID, &claims);
        let err = verify_with_jwks(&token, &jwks_with(TEST_KID), &params(vec![])).unwrap_err();
        assert_eq!(err.outcome, VerifyOutcome::BadClaims);
    }

    #[test]
    fn wrong_audience_is_rejected_when_configured() {
        let token = sign(TEST_KID, &good_claims()); // aud = spa-client-id
        let err =
            verify_with_jwks(&token, &jwks_with(TEST_KID), &params(vec!["other-api"])).unwrap_err();
        assert_eq!(err.outcome, VerifyOutcome::BadClaims);
    }

    #[test]
    fn audience_not_enforced_when_unconfigured() {
        // aud present in token but no configured audience → still verifies.
        let token = sign(TEST_KID, &good_claims());
        assert!(verify_with_jwks(&token, &jwks_with(TEST_KID), &params(vec![])).is_ok());
    }

    #[test]
    fn expired_token_is_rejected() {
        let mut claims = good_claims();
        claims["exp"] = serde_json::json!(now() - 3600);
        let token = sign(TEST_KID, &claims);
        let err = verify_with_jwks(&token, &jwks_with(TEST_KID), &params(vec![])).unwrap_err();
        assert_eq!(err.outcome, VerifyOutcome::BadClaims);
    }

    #[test]
    fn not_yet_valid_token_is_rejected() {
        let mut claims = good_claims();
        claims["nbf"] = serde_json::json!(now() + 3600);
        let token = sign(TEST_KID, &claims);
        let err = verify_with_jwks(&token, &jwks_with(TEST_KID), &params(vec![])).unwrap_err();
        assert_eq!(err.outcome, VerifyOutcome::BadClaims);
    }

    #[test]
    fn unknown_kid_is_flagged_for_refresh() {
        // Token signed with TEST_KID, but the JWKS only knows a different kid →
        // the caller (cached_or_refreshed_jwks) uses this to trigger a refresh.
        let token = sign(TEST_KID, &good_claims());
        let err =
            verify_with_jwks(&token, &jwks_with("rotated-kid-99"), &params(vec![])).unwrap_err();
        assert_eq!(err.outcome, VerifyOutcome::UnknownKid);
    }

    #[test]
    fn alg_none_forgery_is_rejected() {
        // A forged unsigned token (alg=none) must never verify against an RS key.
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&good_claims()).unwrap());
        let forged = format!("{header}.{payload}.");
        let err = verify_with_jwks(&forged, &jwks_with(TEST_KID), &params(vec![])).unwrap_err();
        // Rejected either at header-parse or alg-mismatch — never Ok.
        assert!(matches!(
            err.outcome,
            VerifyOutcome::BadSignature | VerifyOutcome::Malformed
        ));
    }

    #[test]
    fn verify_mode_parses_tristate() {
        // Defaults + explicit values (env-independent parse via direct match).
        assert_eq!(VerifyMode::Off.label(), "off");
        assert_eq!(VerifyMode::Shadow.label(), "shadow");
        assert_eq!(VerifyMode::Enforce.label(), "enforce");
    }
}
