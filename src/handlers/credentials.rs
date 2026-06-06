//! Credential API handlers.
//!
//! Endpoints for managing encrypted credentials.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;

use crate::crypto::{sealed_seal, SealedEnvelope};
use crate::db::models::{CredentialCreateRequest, CredentialListResponse, CredentialResponse};
use crate::error::{AppError, AppResult};
use crate::services::{CredentialService, RuntimeService};

/// Query parameters for listing credentials.
#[derive(Debug, Deserialize, Default)]
pub struct ListCredentialsQuery {
    /// Filter by credential type
    #[serde(rename = "type")]
    pub credential_type: Option<String>,

    /// Free-text search
    pub q: Option<String>,
}

/// Query parameters for getting a credential.
#[derive(Debug, Deserialize, Default)]
pub struct GetCredentialQuery {
    /// Include decrypted data in response
    #[serde(default)]
    pub include_data: bool,

    /// Execution ID (for audit logging)
    pub execution_id: Option<i64>,

    /// Parent execution ID (for audit logging)
    pub parent_execution_id: Option<i64>,
}

/// Create or update a credential.
///
/// `POST /api/credentials`
///
/// # Request Body
///
/// ```json
/// {
///   "name": "my-database-creds",
///   "type": "postgres",
///   "data": {
///     "username": "admin",
///     "password": "secret123",
///     "host": "db.example.com"
///   },
///   "meta": {"environment": "production"},
///   "tags": ["database", "production"],
///   "description": "Production database credentials"
/// }
/// ```
///
/// # Response
///
/// ```json
/// {
///   "id": "123456789",
///   "name": "my-database-creds",
///   "type": "postgres",
///   "created_at": "2025-01-01T00:00:00Z",
///   "updated_at": "2025-01-01T00:00:00Z"
/// }
/// ```
pub async fn create_or_update(
    service: State<CredentialService>,
    request: Json<CredentialCreateRequest>,
) -> AppResult<(StatusCode, Json<CredentialResponse>)> {
    let started_at = std::time::Instant::now();
    let result = create_or_update_inner(service, request).await;
    let status_label = if result.is_ok() { "ok" } else { "error" };
    crate::metrics::record_write_request(
        crate::metrics::endpoint::CREDENTIALS_UPSERT,
        status_label,
        started_at.elapsed().as_secs_f64(),
    );
    result
}

async fn create_or_update_inner(
    State(service): State<CredentialService>,
    Json(request): Json<CredentialCreateRequest>,
) -> AppResult<(StatusCode, Json<CredentialResponse>)> {
    let response = service.create_or_update(request).await?;
    Ok((StatusCode::OK, Json(response)))
}

/// List credentials with optional filtering.
///
/// `GET /api/credentials`
///
/// # Query Parameters
///
/// - `type`: Filter by credential type
/// - `q`: Free-text search on name and description
///
/// # Response
///
/// ```json
/// {
///   "items": [...],
///   "filter": {"type": "postgres", "q": "production"}
/// }
/// ```
pub async fn list(
    State(service): State<CredentialService>,
    Query(query): Query<ListCredentialsQuery>,
) -> AppResult<Json<CredentialListResponse>> {
    let response = service
        .list(query.credential_type.as_deref(), query.q.as_deref())
        .await?;
    Ok(Json(response))
}

/// Get a credential by ID or name.
///
/// `GET /api/credentials/{identifier}`
///
/// # Path Parameters
///
/// - `identifier`: Credential ID (numeric) or name (string)
///
/// # Query Parameters
///
/// - `include_data`: If true, includes decrypted credential data
///
/// # Response
///
/// ```json
/// {
///   "id": "123456789",
///   "name": "my-database-creds",
///   "type": "postgres",
///   "data": {...},  // only if include_data=true
///   "created_at": "2025-01-01T00:00:00Z"
/// }
/// ```
pub async fn get(
    State(service): State<CredentialService>,
    Path(identifier): Path<String>,
    Query(query): Query<GetCredentialQuery>,
) -> AppResult<Json<CredentialResponse>> {
    let response = service
        .get(&identifier, query.include_data, query.execution_id)
        .await?;
    Ok(Json(response))
}

/// Delete a credential.
///
/// `DELETE /api/credentials/{identifier}`
///
/// # Path Parameters
///
/// - `identifier`: Credential ID (numeric) or name (string)
///
/// # Response
///
/// ```json
/// {
///   "message": "Credential deleted successfully",
///   "id": "123456789"
/// }
/// ```
pub async fn delete(
    State(service): State<CredentialService>,
    Path(identifier): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let id = service.delete(&identifier).await?;
    Ok(Json(serde_json::json!({
        "message": "Credential deleted successfully",
        "id": id
    })))
}

/// State extractor for the sealed-credential endpoint — bundles the two
/// services the handler needs (Secrets Wallet Phase 5b, noetl/ai-meta#61).
#[derive(Clone)]
pub struct SealedCredentialDeps {
    pub credentials: CredentialService,
    pub runtime: RuntimeService,
}

/// Query parameters for the sealed-credential endpoint.
#[derive(Debug, Deserialize, Default)]
pub struct GetSealedCredentialQuery {
    /// `name` of the worker_pool row in `noetl.runtime` whose registered
    /// public key the response is sealed to.
    pub worker_id: String,
    /// Forwarded to the underlying credential fetch — kept for audit
    /// correlation, NOT used as the seal recipient.
    pub execution_id: Option<i64>,
    /// Forwarded to the underlying credential fetch.
    pub parent_execution_id: Option<i64>,
}

/// Get a credential as a sealed payload addressed to a specific worker.
///
/// Secrets Wallet **Phase 5b** ([noetl/ai-meta#61]).  The credential payload
/// (the same JSON the plain `GET /api/credentials/{identifier}` returns with
/// `include_data=true`) is sealed with the X25519 public key the worker
/// registered with itself at startup.  The plaintext exists briefly inside
/// the server process at seal time; it never enters the response body, so an
/// operator with `kubectl exec` on the server pod sees only ciphertext.
///
/// `GET /api/credentials/{identifier}/sealed?worker_id=<name>`
///
/// The query MUST supply `worker_id` (the `name` of the `kind=worker_pool`
/// row in `noetl.runtime` that registered a sealing pubkey).  When the
/// worker exists but didn't register a key, returns `400 BadRequest`.
///
/// Response: a [`SealedEnvelope`] JSON (see `src/crypto/sealed.rs` for the
/// wire shape).  Phase 5c integrates the worker side (ephemeral X25519
/// keypair at startup + unseal + `zeroize` after the caller's tool dispatch).
///
/// [noetl/ai-meta#61]: https://github.com/noetl/ai-meta/issues/61
pub async fn get_sealed(
    State(deps): State<SealedCredentialDeps>,
    Path(identifier): Path<String>,
    Query(query): Query<GetSealedCredentialQuery>,
) -> AppResult<Json<SealedEnvelope>> {
    let span = tracing::info_span!(
        "credential.seal",
        worker_id = %query.worker_id,
        identifier = %identifier,
        execution_id = query.execution_id,
    );
    let _guard = span.enter();

    // Look up the worker's sealing pubkey first; this is the cheapest reject
    // and avoids decrypting + serialising the credential for a worker that
    // can't unseal it.
    let pubkey_bytes = match deps
        .runtime
        .get_worker_public_key(&query.worker_id)
        .await
    {
        Ok(Some(b)) => b,
        Ok(None) => {
            crate::metrics::record_credential_seal("no_pubkey");
            return Err(AppError::BadRequest(format!(
                "worker '{}' did not register a sealing pubkey (worker_public_key \
                 missing from the noetl.runtime row, or the worker_pool row \
                 doesn't exist)",
                query.worker_id
            )));
        }
        Err(e) => {
            crate::metrics::record_credential_seal("worker_not_found");
            return Err(e);
        }
    };
    let pubkey = x25519_dalek::PublicKey::from(pubkey_bytes);

    // Fetch the credential payload (with include_data=true — the whole point
    // of sealing is to deliver the resolved secret).
    let credential = match deps
        .credentials
        .get(&identifier, true, query.execution_id)
        .await
    {
        Ok(c) => c,
        Err(e) => {
            crate::metrics::record_credential_seal("credential_error");
            return Err(e);
        }
    };
    let plaintext = serde_json::to_vec(&credential).map_err(|e| {
        crate::metrics::record_credential_seal("seal_error");
        AppError::Internal(format!("sealed get: serialize credential: {e}"))
    })?;

    let envelope = sealed_seal(&pubkey, &plaintext).inspect_err(|_| {
        crate::metrics::record_credential_seal("seal_error");
    })?;
    crate::metrics::record_credential_seal("ok");
    Ok(Json(envelope))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::sealed_open;
    use x25519_dalek::{PublicKey, StaticSecret};

    /// End-to-end primitive contract: sealing a serialized credential JSON
    /// and opening it on the worker side round-trips losslessly via the
    /// sealed_seal / sealed_open primitives the handler uses.  Locks the
    /// shape `get_sealed` produces against drift in either crypto-side
    /// constant.
    #[test]
    fn sealed_credential_round_trips_via_primitives() {
        let recipient_sk = StaticSecret::random_from_rng(rand_core::OsRng);
        let recipient_pk = PublicKey::from(&recipient_sk);

        let credential = serde_json::json!({
            "id": "1234567890",
            "name": "duffel-token",
            "type": "bearer",
            "data": { "token": "sk-test-AbCdEf123" }
        });
        let plaintext = serde_json::to_vec(&credential).unwrap();

        let envelope = sealed_seal(&recipient_pk, &plaintext).unwrap();
        let opened = sealed_open(&recipient_sk, &envelope).unwrap();
        let opened_json: serde_json::Value = serde_json::from_slice(&opened).unwrap();

        assert_eq!(opened_json, credential);
    }

    /// Tampered envelope is rejected — the AEAD auth tag catches any flipped
    /// byte in the sealed-credential ciphertext (same guarantee Phase 5a's
    /// `sealed::tests` exercise, here pinned at the handler-payload layer).
    #[test]
    fn tampered_sealed_credential_is_rejected() {
        use base64::{engine::general_purpose::STANDARD as B64, Engine as _};

        let recipient_sk = StaticSecret::random_from_rng(rand_core::OsRng);
        let recipient_pk = PublicKey::from(&recipient_sk);
        let plaintext = br#"{"id":"1","name":"x","type":"bearer","data":{"token":"t"}}"#;
        let mut envelope = sealed_seal(&recipient_pk, plaintext).unwrap();

        let mut ct = B64.decode(&envelope.ciphertext).unwrap();
        ct[0] ^= 0x01;
        envelope.ciphertext = B64.encode(&ct);

        let err = sealed_open(&recipient_sk, &envelope).unwrap_err();
        assert!(format!("{err:?}").contains("AEAD verify/decrypt"));
    }
}
