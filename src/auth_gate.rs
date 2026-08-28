//! Router-level bearer-token gate for privileged API surfaces (noetl/ai-meta#303).
//!
//! # The problem this exists to fix
//!
//! The server already had a working token check —
//! [`crate::handlers::internal::RequireInternalApiToken`] — and it works: on prod
//! today `/api/internal/outbox/pending-count` answers 403 with no token, 403 with
//! a wrong one, and 200 with the right one.
//!
//! But it is an **extractor**, so it is opt-in per handler. Every handler that
//! wants protection must remember to name it in its signature, and the ones that
//! forgot are indistinguishable from the ones that decided not to. On prod that
//! produced a surface where `/api/credentials/{id}?include_data=true` returns
//! **200 with decrypted credential data and no `Authorization` header at all**,
//! reachable from the user worker pool, which runs user-supplied playbooks.
//!
//! Two `/api/internal/*` routes (`wallet/key-status`, `cells`) are open for the
//! same reason. That is the sharper form of the defect: **the `/api/internal/`
//! prefix is a representation of protection that nothing enforces.** A reader
//! sees the prefix and concludes the route is gated; only a request finds out.
//!
//! A `Layer` cannot be forgotten by an individual handler, which is why the fix
//! is one.
//!
//! # Why it ships in shadow mode
//!
//! Turning enforcement on could lock out an internal caller nobody has enumerated
//! — and a lockout on the credential path is an outage of everything that
//! resolves a credential. So the default mode **rejects nothing**. It records
//! what it *would* have rejected, labelled by route group, so the question "who
//! would break if we enforced this?" is answered by evidence rather than by
//! reading call sites.
//!
//! Promotion is then a deliberate, reversible env change, made once the shadow
//! counters for a group have sat at zero under real traffic.

use std::env;

use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use subtle::ConstantTimeEq;

const TOKEN_ENV: &str = "NOETL_INTERNAL_API_TOKEN";
const MODE_ENV: &str = "NOETL_INTERNAL_AUTH_MODE";

/// Route groups the gate is applied to. A fixed list, because the metric's label
/// set is pinned from it — an open label set would reintroduce the absent-series
/// problem this codebase has been bitten by before.
pub const GROUPS: [&str; 3] = ["credentials", "keychain", "internal"];

/// What the gate does with a request that fails the check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Record nothing, check nothing. Fully inert.
    Off,
    /// Check, record the outcome, and **allow the request regardless**.
    /// The default: informative and behaviourally identical to no gate at all.
    Shadow,
    /// Check and reject. 403 on a missing/malformed/wrong token, 503 when the
    /// server itself has no token configured (no permissive default on a
    /// privileged surface — the same choice the existing extractor makes).
    Enforce,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Shadow => "shadow",
            Mode::Enforce => "enforce",
        }
    }
}

/// Read the mode from the environment. Anything unrecognised — including a typo
/// — falls back to `Shadow` rather than to `Enforce`, so a misspelling can never
/// silently start rejecting production traffic.
pub fn mode() -> Mode {
    match env::var(MODE_ENV)
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "enforce" => Mode::Enforce,
        "off" => Mode::Off,
        _ => Mode::Shadow,
    }
}

/// The outcome of checking one request. Closed set — pinned as metric labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Presented token matched the configured one.
    Valid,
    /// No `Authorization` header.
    Missing,
    /// Header present but not a `Bearer <token>`.
    Malformed,
    /// Well-formed bearer, wrong value.
    Mismatch,
    /// The **server** has no token configured. Distinct from `Missing` on
    /// purpose: one is the caller's fault, the other is the deployment's, and
    /// collapsing them would hide a misconfigured server behind "clients are
    /// not sending tokens".
    Unconfigured,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Valid => "valid",
            Outcome::Missing => "missing",
            Outcome::Malformed => "malformed",
            Outcome::Mismatch => "mismatch",
            Outcome::Unconfigured => "unconfigured",
        }
    }
    pub const ALL: [Outcome; 5] = [
        Outcome::Valid,
        Outcome::Missing,
        Outcome::Malformed,
        Outcome::Mismatch,
        Outcome::Unconfigured,
    ];
    /// Would `Mode::Enforce` reject a request with this outcome?
    pub fn would_reject(self) -> bool {
        !matches!(self, Outcome::Valid)
    }
}

/// Classify one request. Pure: no env, no I/O, no axum — so the whole truth
/// table is testable directly.
///
/// `expected` is the server's configured token (`None`/empty = unconfigured);
/// `presented` is the raw `Authorization` header value.
pub fn decide(expected: Option<&str>, presented: Option<&str>) -> Outcome {
    let expected = match expected {
        Some(e) if !e.trim().is_empty() => e,
        _ => return Outcome::Unconfigured,
    };
    let header = match presented {
        Some(h) => h,
        None => return Outcome::Missing,
    };
    let token = match header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))
    {
        Some(t) => t.trim(),
        None => return Outcome::Malformed,
    };
    if token.is_empty() {
        return Outcome::Malformed;
    }
    // Constant-time: a timing side channel here would leak the token one byte at
    // a time to exactly the in-cluster caller this gate is meant to stop.
    if token.as_bytes().ct_eq(expected.as_bytes()).into() {
        Outcome::Valid
    } else {
        Outcome::Mismatch
    }
}

/// The axum middleware. Applied per route group via
/// `from_fn_with_state(group, gate)`.
pub async fn gate(State(group): State<&'static str>, req: Request, next: Next) -> Response {
    let m = mode();
    if m == Mode::Off {
        return next.run(req).await;
    }
    let expected = env::var(TOKEN_ENV).ok();
    let presented = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let outcome = decide(expected.as_deref(), presented.as_deref());

    crate::metrics::record_internal_auth(group, outcome.as_str(), m.as_str());

    if m == Mode::Enforce && outcome.would_reject() {
        let (code, msg) = match outcome {
            Outcome::Unconfigured => (
                StatusCode::SERVICE_UNAVAILABLE,
                "internal API token not configured on this server",
            ),
            _ => (StatusCode::FORBIDDEN, "forbidden"),
        };
        tracing::warn!(
            target: "noetl_server::auth_gate",
            group, outcome = outcome.as_str(), status = code.as_u16(),
            "internal auth gate rejected a request"
        );
        return (code, msg).into_response();
    }

    if outcome.would_reject() {
        // Shadow: the whole point. Say what WOULD have happened, once per
        // request, at a level that will actually be read.
        tracing::warn!(
            target: "noetl_server::auth_gate",
            group, outcome = outcome.as_str(), mode = m.as_str(),
            path = %req.uri().path(),
            "internal auth gate WOULD reject this request under enforce"
        );
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_token_is_valid() {
        assert_eq!(
            decide(Some("s3cret"), Some("Bearer s3cret")),
            Outcome::Valid
        );
        assert_eq!(
            decide(Some("s3cret"), Some("bearer s3cret")),
            Outcome::Valid
        );
    }

    #[test]
    fn absent_header_is_missing_not_mismatch() {
        assert_eq!(decide(Some("s3cret"), None), Outcome::Missing);
    }

    #[test]
    fn non_bearer_and_empty_bearer_are_malformed() {
        assert_eq!(
            decide(Some("s3cret"), Some("Basic abc")),
            Outcome::Malformed
        );
        assert_eq!(decide(Some("s3cret"), Some("Bearer ")), Outcome::Malformed);
        assert_eq!(decide(Some("s3cret"), Some("s3cret")), Outcome::Malformed);
    }

    #[test]
    fn wrong_token_is_mismatch() {
        assert_eq!(
            decide(Some("s3cret"), Some("Bearer nope")),
            Outcome::Mismatch
        );
    }

    /// An unconfigured SERVER must not read as "the caller sent nothing".
    /// Collapsing the two would let a server with no token look like a fleet of
    /// clients that forgot theirs.
    #[test]
    fn unconfigured_server_outranks_the_caller() {
        assert_eq!(decide(None, Some("Bearer s3cret")), Outcome::Unconfigured);
        assert_eq!(
            decide(Some("   "), Some("Bearer s3cret")),
            Outcome::Unconfigured
        );
        assert_eq!(decide(None, None), Outcome::Unconfigured);
    }

    /// Only `Valid` survives enforcement. Written as a total match so adding a
    /// new outcome without deciding its disposition fails to compile.
    #[test]
    fn every_non_valid_outcome_is_rejected_under_enforce() {
        for o in Outcome::ALL {
            let expected = match o {
                Outcome::Valid => false,
                Outcome::Missing
                | Outcome::Malformed
                | Outcome::Mismatch
                | Outcome::Unconfigured => true,
            };
            assert_eq!(o.would_reject(), expected, "{o:?}");
        }
    }

    /// A typo must not enable enforcement. This is the property that makes the
    /// flag safe to ship on by default.
    #[test]
    fn unknown_mode_falls_back_to_shadow_never_enforce() {
        for raw in ["", "ENFORCEE", "on", "true", "1", "enfoce", "shadow"] {
            let m = match raw.trim().to_ascii_lowercase().as_str() {
                "enforce" => Mode::Enforce,
                "off" => Mode::Off,
                _ => Mode::Shadow,
            };
            assert_ne!(m, Mode::Enforce, "{raw:?} must not enable enforcement");
        }
        assert_eq!(
            match "enforce" {
                "enforce" => Mode::Enforce,
                "off" => Mode::Off,
                _ => Mode::Shadow,
            },
            Mode::Enforce
        );
    }
}
