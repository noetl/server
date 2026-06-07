//! Wallet KEK rotation HTTP endpoints (Secrets Wallet Phase 7a.2,
//! [`noetl/ai-meta#61`](https://github.com/noetl/ai-meta/issues/61)).
//!
//! Internal-only surface — gated to peer access via the same mTLS /
//! ServiceAccount-token pattern as the other `/api/internal/*` routes.
//!
//! - `POST /api/internal/wallet/rotate-kek` — kick off a synchronous
//!   batched re-wrap pass across `noetl.credential` + `noetl.keychain`.
//!   Returns per-table counts.
//! - `GET /api/internal/wallet/key-status` — diagnostic, per-table
//!   per-version row counts.  Operator confirms a rotation completed
//!   before retiring the old KEK version in the KMS.

use axum::{
    extract::State,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::AppResult;
use crate::services::wallet_rotate::{
    KeyStatusSummary, RotateSummary, WalletRotateService, WalletTable,
};

/// State bundle.  Only the service — no other dependencies, so the
/// handler is trivially testable in isolation.
#[derive(Clone)]
pub struct WalletRotateDeps {
    pub service: WalletRotateService,
}

/// Request body for `POST /api/internal/wallet/rotate-kek`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct RotateRequest {
    /// Rows per batch (default 100).  Tune down for low-memory pods,
    /// up for high-throughput rotations.
    pub batch_size: Option<i64>,
    /// Cap on number of batches per request (default 1000).
    /// Prevents an accidentally-huge request from monopolising the server.
    pub max_batches: Option<i64>,
}

/// Response for `POST /api/internal/wallet/rotate-kek`.
#[derive(Debug, Clone, Serialize)]
pub struct RotateResponse {
    pub credential: RotateSummary,
    pub keychain: RotateSummary,
}

/// `POST /api/internal/wallet/rotate-kek`.
pub async fn rotate_kek(
    State(deps): State<WalletRotateDeps>,
    Json(req): Json<RotateRequest>,
) -> AppResult<Json<RotateResponse>> {
    let span = tracing::info_span!(
        "wallet.rotate_kek",
        batch_size = req.batch_size,
        max_batches = req.max_batches,
    );
    let _guard = span.enter();
    let credential = deps
        .service
        .rotate_table(WalletTable::Credential, req.batch_size, req.max_batches)
        .await?;
    let keychain = deps
        .service
        .rotate_table(WalletTable::Keychain, req.batch_size, req.max_batches)
        .await?;
    tracing::info!(
        cred_processed = credential.processed,
        cred_rewrapped = credential.rewrapped,
        cred_skipped = credential.skipped,
        cred_failed = credential.failed,
        kc_processed = keychain.processed,
        kc_rewrapped = keychain.rewrapped,
        kc_skipped = keychain.skipped,
        kc_failed = keychain.failed,
        "wallet.rotate_kek completed"
    );
    Ok(Json(RotateResponse {
        credential,
        keychain,
    }))
}

/// Response for `GET /api/internal/wallet/key-status`.
#[derive(Debug, Clone, Serialize)]
pub struct KeyStatusResponse {
    pub credential: KeyStatusSummary,
    pub keychain: KeyStatusSummary,
}

/// `GET /api/internal/wallet/key-status`.
pub async fn key_status(
    State(deps): State<WalletRotateDeps>,
) -> AppResult<Json<KeyStatusResponse>> {
    let credential = deps.service.key_status_table(WalletTable::Credential).await?;
    let keychain = deps.service.key_status_table(WalletTable::Keychain).await?;
    Ok(Json(KeyStatusResponse {
        credential,
        keychain,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotate_request_round_trips_json() {
        let req: RotateRequest = serde_json::from_str(r#"{"batch_size":50,"max_batches":10}"#).unwrap();
        assert_eq!(req.batch_size, Some(50));
        assert_eq!(req.max_batches, Some(10));
    }

    #[test]
    fn rotate_request_accepts_empty_body() {
        // Defaults: batch_size + max_batches both None.  The service
        // layer's `unwrap_or(DEFAULT_*)` decides.
        let req: RotateRequest = serde_json::from_str("{}").unwrap();
        assert!(req.batch_size.is_none());
        assert!(req.max_batches.is_none());
    }

    #[test]
    fn rotate_response_serialises_with_per_table_keys() {
        let r = RotateResponse {
            credential: RotateSummary {
                processed: 5,
                rewrapped: 3,
                skipped: 2,
                failed: 0,
                last_id: 100,
            },
            keychain: RotateSummary::default(),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"credential\""));
        assert!(json.contains("\"keychain\""));
        assert!(json.contains("\"rewrapped\":3"));
    }
}
