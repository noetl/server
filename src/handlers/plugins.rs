//! Plug-in module registry endpoints (noetl/ai-meta#105 Round 4) — the live
//! `PluginSource` backend the system worker pool's wasmtime host fetches from.
//!
//! - `POST /api/internal/plugins/{*path}?version=N` — register a compiled module
//!   (raw wasm/WAT body); the server computes the SHA-256 digest and stores it.
//! - `GET  /api/internal/plugins/{*path}?version=N[&digest=X]` — serve the module
//!   bytes; if `digest` is supplied it must match (the worker pins it in its
//!   cache key), else 409.
//!
//! `{*path}` is a catch-all so plug-in paths with slashes (`system/materialiser`)
//! resolve. These live under `/api/internal/*`, the same service-account-gated
//! family as the other internal endpoints — per
//! [the data-access boundary](https://github.com/noetl/noetl/blob/main/agents/rules/data-access-boundary.md)
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

use crate::db::{queries::plugin_module, DbPool};
use crate::error::AppResult;

#[derive(Debug, Deserialize)]
pub struct RegisterQuery {
    pub version: i32,
    #[serde(default)]
    pub media_type: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RegisterResponse {
    pub path: String,
    pub version: i32,
    pub digest: String,
    pub bytes: usize,
}

/// `POST /api/internal/plugins/{*path}?version=N` — register a module from the
/// raw request body. The server is the digest authority: it hashes the bytes
/// and stores `(path, version, digest, bytes)`.
pub async fn register(
    State(pool): State<DbPool>,
    Path(path): Path<String>,
    Query(q): Query<RegisterQuery>,
    body: Bytes,
) -> AppResult<Json<RegisterResponse>> {
    let digest = hex::encode(Sha256::digest(&body));
    let media_type = q
        .media_type
        .unwrap_or_else(|| "application/wasm".to_string());
    plugin_module::upsert(&pool, &path, q.version, &digest, &media_type, &body).await?;
    tracing::info!(
        plugin_path = %path,
        version = q.version,
        digest = %digest,
        bytes = body.len(),
        "registered plug-in module"
    );
    Ok(Json(RegisterResponse {
        path,
        version: q.version,
        digest,
        bytes: body.len(),
    }))
}

#[derive(Debug, Deserialize)]
pub struct FetchQuery {
    pub version: i32,
    #[serde(default)]
    pub digest: Option<String>,
}

/// `GET /api/internal/plugins/{*path}?version=N[&digest=X]` — serve the module
/// bytes, content-typed by its media type and carrying the digest as an ETag.
pub async fn fetch(
    State(pool): State<DbPool>,
    Path(path): Path<String>,
    Query(q): Query<FetchQuery>,
) -> AppResult<Response> {
    let Some(row) = plugin_module::get(&pool, &path, q.version).await? else {
        return Ok((
            StatusCode::NOT_FOUND,
            format!("plug-in {path}@{} not found", q.version),
        )
            .into_response());
    };
    if let Some(expected) = q.digest.as_deref() {
        if expected != row.digest {
            return Ok((
                StatusCode::CONFLICT,
                format!(
                    "digest mismatch for {path}@{}: stored {}, requested {}",
                    q.version, row.digest, expected
                ),
            )
                .into_response());
        }
    }
    Ok((
        [
            (header::CONTENT_TYPE, row.media_type),
            (header::ETAG, format!("\"{}\"", row.digest)),
        ],
        row.bytes,
    )
        .into_response())
}
