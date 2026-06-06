//! Cross-region credential broker endpoint (Secrets Wallet Phase 6e,
//! [`noetl/ai-meta#61`](https://github.com/noetl/ai-meta/issues/61)).
//!
//! `POST /api/internal/cross-region/resolve` is the peer-server side of
//! the cross-region broker.  Called by a sibling server whose local
//! residency policy denied a credential, this endpoint:
//!
//! 1. Verifies the request is targeting this server's region (defensive
//!    against a misconfigured `NOETL_SECRET_BROKER_REGISTRY` on the
//!    peer — using the wrong broker for a region should reject, not
//!    silently leak the credential).
//! 2. Resolves the credential locally (subject to this server's own
//!    residency + provider chain).
//! 3. Seals the resolved payload to the **requesting worker's** pubkey
//!    using the Phase-5a sealing primitives.
//! 4. Returns the [`SealedEnvelope`] directly to the peer.
//!
//! The cleartext never leaves this server's process memory; the peer
//! receives only the sealed envelope, which only the addressed worker
//! can open.
//!
//! The route is registered under `/api/internal/*` — same access
//! pattern as the existing internal-only endpoints
//! (`/api/internal/outbox/...`, `/api/internal/events/project`).
//! Production deployments gate this surface to peer servers via mTLS
//! client cert + ServiceAccount token; this round ships the handler
//! plumbing and leaves the peer-cert PKI bootstrap to ops.

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};

use crate::crypto::{SealedEnvelope, sealed_seal};
use crate::error::{AppError, AppResult};
use crate::secrets::broker::CrossRegionResolveRequest;
use crate::secrets::server_region;
use crate::services::credential::CredentialService;

/// State bundle for the cross-region resolve handler.  Holds the
/// credential service the handler uses to resolve locally.
#[derive(Clone)]
pub struct CrossRegionDeps {
    pub credentials: CredentialService,
}

/// `POST /api/internal/cross-region/resolve`.
///
/// Body: [`CrossRegionResolveRequest`].  Response on success: a
/// [`SealedEnvelope`] addressed to the requesting worker's pubkey.
///
/// Errors:
/// - `403 Forbidden` — the request's `expected_entry_region` doesn't
///   match this server's region.  Defensive: the peer's broker registry
///   is out of date and trying to route to the wrong region.
/// - `400 BadRequest` — the worker pubkey is not valid base64 / not the
///   expected 32-byte X25519 length.
/// - `404 NotFound` — the requested alias doesn't exist on this server.
/// - `502 BadGateway` — the local credential service errored out (the
///   peer reports this as cross-region-unreachable too).
pub async fn resolve(
    State(deps): State<CrossRegionDeps>,
    Json(body): Json<CrossRegionResolveRequest>,
) -> AppResult<Json<SealedEnvelope>> {
    let span = tracing::info_span!(
        "credential.cross_region_resolve",
        alias = %body.alias,
        worker_id = %body.worker_id,
        execution_id = body.execution_id,
        expected_entry_region = %body.expected_entry_region,
        requesting_region = %body.requesting_region,
    );
    let _g = span.enter();

    // (1) — region check.  Compare the peer's `expected_entry_region`
    // against this server's own home region.  A peer whose broker
    // registry was misconfigured (or whose state lagged behind a
    // credential's residency-region change) MUST NOT have its request
    // silently honoured — that's exactly the failure mode the residency
    // gate protects against.
    let my_region = server_region();
    if body.expected_entry_region != my_region {
        crate::metrics::record_cross_region_broker_call(
            &body.expected_entry_region,
            "wrong_region",
        );
        return Err(AppError::Forbidden(format!(
            "cross-region broker: this server is in region '{}', not '{}' as the \
             requesting peer expected",
            my_region, body.expected_entry_region
        )));
    }

    // (2) — decode the worker pubkey.  32 bytes after base64.
    let pubkey_bytes = decode_pubkey(&body.worker_public_key_b64).map_err(|e| {
        crate::metrics::record_cross_region_broker_call(my_region, "bad_pubkey");
        AppError::BadRequest(format!("cross-region: invalid worker_public_key_b64: {e}"))
    })?;
    let recipient = x25519_dalek::PublicKey::from(pubkey_bytes);

    // (3) — resolve the credential locally.  This runs through the same
    // residency / provider chain as the local `get_sealed` handler;
    // anything the local policy blocks is reported as a normal error
    // (the cross-region broker honours its own policies even when
    // serving peer requests).
    let credential = deps
        .credentials
        .get(&body.alias, true, body.execution_id)
        .await
        .map_err(|e| {
            crate::metrics::record_cross_region_broker_call(my_region, "resolve_error");
            e
        })?;

    // (4) — seal.  Identical to the Phase-5b primitive: serialise the
    // credential payload, ChaCha20-Poly1305 with the Phase-5a derived
    // key + nonce, addressed to the requesting worker's pubkey.  The
    // peer receives only the sealed envelope; cleartext never enters
    // the response body.
    let plaintext = serde_json::to_vec(&credential).map_err(|e| {
        crate::metrics::record_cross_region_broker_call(my_region, "serialize_error");
        AppError::Internal(format!("cross-region: serialise credential: {e}"))
    })?;
    let envelope = sealed_seal(&recipient, &plaintext).map_err(|e| {
        crate::metrics::record_cross_region_broker_call(my_region, "seal_error");
        e
    })?;
    crate::metrics::record_cross_region_broker_call(my_region, "ok");
    Ok(Json(envelope))
}

/// Decode a 32-byte X25519 public key from base64.  Same shape as the
/// Phase-5b decoder in the worker — anything else is rejected loudly so
/// the peer knows the issue is on its end (not silently sealing to a
/// bad key).
fn decode_pubkey(s: &str) -> Result<[u8; 32], String> {
    use base64::Engine as _;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .map_err(|e| format!("base64 decode: {e}"))?;
    if raw.len() != 32 {
        return Err(format!("expected 32 bytes, got {}", raw.len()));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&raw);
    Ok(out)
}

/// Stub `IntoResponse` impl in case axum's stack ever wants to consume
/// the deps directly — keeps `CrossRegionDeps` ergonomically composable
/// with the rest of the router.  Routes never produce this type as a
/// response; the impl exists purely to satisfy the trait bound when the
/// type is used as a `State`.
impl IntoResponse for CrossRegionDeps {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "CrossRegionDeps is state-only",
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x25519_dalek::{PublicKey, StaticSecret};

    #[test]
    fn decode_pubkey_round_trips_x25519() {
        let sk = StaticSecret::random_from_rng(rand_core::OsRng);
        let pk = PublicKey::from(&sk);
        let b64 = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(pk.as_bytes())
        };
        let decoded = decode_pubkey(&b64).expect("decode ok");
        assert_eq!(decoded, *pk.as_bytes());
    }

    #[test]
    fn decode_pubkey_rejects_wrong_length() {
        // 31 bytes (b64 encoded) — close enough to be a credible typo.
        let raw = vec![0u8; 31];
        let b64 = {
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(&raw)
        };
        let err = decode_pubkey(&b64).expect_err("must reject");
        assert!(err.contains("expected 32 bytes"), "wrong error: {err}");
    }

    #[test]
    fn decode_pubkey_rejects_non_base64() {
        let err = decode_pubkey("not-base64-!!@#").expect_err("must reject");
        assert!(err.contains("base64 decode"), "wrong error: {err}");
    }

    #[test]
    fn cross_region_resolve_request_round_trips_json() {
        // Phase 6e wire shape — locks the JSON field names against drift.
        // The peer broker MUST be able to deserialise what the
        // requesting server serialises (and vice-versa).
        let req = CrossRegionResolveRequest {
            alias: "eu-token".to_string(),
            worker_public_key_b64: "AAAA".to_string(),
            worker_id: "noetl-worker-rust-abc".to_string(),
            execution_id: Some(12345),
            parent_execution_id: None,
            expected_entry_region: "eu-central-1".to_string(),
            requesting_region: "us-east-1".to_string(),
        };
        let json = serde_json::to_string(&req).unwrap();
        let back: CrossRegionResolveRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.alias, req.alias);
        assert_eq!(back.worker_id, req.worker_id);
        assert_eq!(back.expected_entry_region, req.expected_entry_region);
    }
}
