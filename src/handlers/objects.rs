//! Object-store endpoints (noetl/ai-meta#105 Round 5) — the server-mediated
//! backend for a plug-in's `noetl.object_put` capability (the Feather tier).
//!
//! - `PUT /api/internal/objects/{*key}` — write an object (raw body) at the §7
//!   physical key; the server computes the SHA-256 digest and stores it.
//! - `GET /api/internal/objects/{*key}` — read the object bytes back.
//!
//! `{*key}` is a catch-all so the slash-bearing §7 key
//! (`noetl/env=…/region=…/cell=…/shard=…/…/results/<step>/<frame>/<row>/<attempt>.feather`)
//! resolves. Under `/api/internal/*`, the service-account-gated family —
//! workers reach NoETL-owned data only through the server API.

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::db::{queries::object_store, DbPool};
use crate::error::AppResult;

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
    State(pool): State<DbPool>,
    Path(key): Path<String>,
    Query(q): Query<PutQuery>,
    body: Bytes,
) -> AppResult<Json<PutResponse>> {
    let digest = hex::encode(Sha256::digest(&body));
    let media_type = q
        .media_type
        .unwrap_or_else(|| "application/octet-stream".to_string());
    object_store::put(&pool, &key, &digest, &media_type, &body).await?;
    tracing::debug!(object_key = %key, digest = %digest, bytes = body.len(), "stored object");
    Ok(Json(PutResponse {
        key,
        digest,
        bytes: body.len(),
    }))
}

/// `GET /api/internal/objects/{*key}` — serve the object bytes, content-typed by
/// its media type with the digest as an ETag.
pub async fn get(
    State(pool): State<DbPool>,
    Path(key): Path<String>,
) -> AppResult<Response> {
    let Some(row) = object_store::get(&pool, &key).await? else {
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
