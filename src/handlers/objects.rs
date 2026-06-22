//! Object-store endpoints (noetl/ai-meta#105 Round 5; backend selector added in
//! [noetl/ai-meta#104](https://github.com/noetl/ai-meta/issues/104) Phase C).
//!
//! - `PUT /api/internal/objects/{*key}` — write an object (raw body) at the §7
//!   physical key; the server computes the SHA-256 digest and stores it.
//! - `GET /api/internal/objects/{*key}` — read the object bytes back.
//!
//! `{*key}` is a catch-all so the slash-bearing §7 key
//! (`noetl/env=…/region=…/cell=…/shard=…/…/results/<step>/<frame>/<row>/<attempt>.feather`)
//! resolves. Under `/api/internal/*`, the service-account-gated family —
//! workers reach NoETL-owned data only through the server API.
//!
//! The bytes land in whichever [`ObjectBackend`] the server resolved at startup
//! (`NOETL_OBJECT_STORE_BACKEND`): Postgres `BYTEA` (Phase B default) or a GCS
//! bucket (Phase C). The HTTP contract is identical across backends.

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::db::DbPool;
use crate::error::AppResult;
use crate::services::object_backend::ObjectBackend;

/// Injected object-store deps: the Postgres pool (for the Postgres backend) plus
/// the resolved backend selector.
#[derive(Clone)]
pub struct ObjectStoreDeps {
    pub pool: DbPool,
    pub backend: ObjectBackend,
}

#[derive(Debug, Deserialize)]
pub struct PutQuery {
    #[serde(default)]
    pub media_type: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PutResponse {
    pub key: String,
    pub digest: String,
    pub bytes: usize,
}

/// `PUT /api/internal/objects/{*key}` — store the raw body at `key`.
pub async fn put(
    State(deps): State<ObjectStoreDeps>,
    Path(key): Path<String>,
    Query(q): Query<PutQuery>,
    body: Bytes,
) -> AppResult<Json<PutResponse>> {
    let digest = hex::encode(Sha256::digest(&body));
    let media_type = q
        .media_type
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let outcome = deps
        .backend
        .put(&deps.pool, &key, &digest, &media_type, &body)
        .await;
    crate::metrics::record_object_store_op(deps.backend.label(), "put", outcome.is_ok());
    outcome?;
    tracing::debug!(object_key = %key, digest = %digest, bytes = body.len(), backend = deps.backend.label(), "stored object");
    Ok(Json(PutResponse {
        key,
        digest,
        bytes: body.len(),
    }))
}

/// `GET /api/internal/objects/{*key}` — serve the object bytes, content-typed by
/// its media type with the digest as an ETag.
pub async fn get(State(deps): State<ObjectStoreDeps>, Path(key): Path<String>) -> AppResult<Response> {
    let fetched = deps.backend.get(&deps.pool, &key).await;
    crate::metrics::record_object_store_op(deps.backend.label(), "get", fetched.is_ok());
    let Some(row) = fetched? else {
        return Ok((StatusCode::NOT_FOUND, format!("object {key} not found")).into_response());
    };
    Ok((
        [
            (header::CONTENT_TYPE, row.media_type),
            (header::ETAG, format!("\"{}\"", row.digest)),
        ],
        row.bytes,
    )
        .into_response())
}
