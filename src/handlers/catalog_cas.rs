//! `/api/catalog/{resolve,blob,sources,pin}` (noetl/ai-meta#548).
//!
//! Mounted only when `NOETL_CATALOG_CAS=true` **and** its tables were found
//! ([`crate::services::catalog_cas::ready`]). Otherwise a GET on these paths
//! falls through to the `ui_schema` wildcard, which returns 404 for any tail not
//! ending in `/ui_schema`.
//!
//! | route | what |
//! |---|---|
//! | `POST /api/catalog/resolve` | `{ref, execution_id?}` → the entry a reference resolves to, its digest and sources. With `execution_id`, resolves inside that execution's pin, exactly as a child call would. |
//! | `GET /api/catalog/blob/{digest}` | the body, `ETag: "<digest>"`, immutable |
//! | `POST /api/catalog/sources` | `{ref, uri}` → declare another source of a body (internal token) |
//! | `GET /api/catalog/pin/{execution_id}` | what an execution was pinned to |

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use noetl_orchestrate_core::catalog as core;
use serde::{Deserialize, Serialize};

use crate::db::DbPool;
use crate::error::{AppError, AppResult};
use crate::services::catalog_cas as cas;

pub fn routes(pool: DbPool) -> Router {
    Router::new()
        .route("/api/catalog/resolve", post(resolve))
        .route("/api/catalog/blob/{digest}", get(blob))
        .route("/api/catalog/sources", post(add_source))
        .route("/api/catalog/pin/{execution_id}", get(pin))
        .with_state(pool)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveBody {
    #[serde(rename = "ref")]
    pub reference: String,
    /// Resolve as a child of this execution would.
    #[serde(default)]
    pub execution_id: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct ResolveReply {
    pub catalog_id: String,
    pub path: String,
    pub version: i32,
    pub kind: String,
    pub digest: String,
    pub reason: String,
    pub sources: Vec<cas::SourceRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned_by: Option<String>,
}

fn bad(e: core::CatalogError) -> AppError {
    match e {
        core::CatalogError::NotFound(m) => AppError::NotFound(m),
        other => AppError::Validation(other.to_string()),
    }
}

async fn resolve(
    State(pool): State<DbPool>,
    Json(body): Json<ResolveBody>,
) -> AppResult<Json<ResolveReply>> {
    let r = core::PlaybookRef::parse(&body.reference).map_err(bad)?;
    let pin = match body.execution_id {
        Some(id) => Some(
            cas::get_pin(&pool, id)
                .await?
                .ok_or_else(|| AppError::NotFound(format!("execution {id} has no catalog pin")))?
                .inherited(),
        ),
        None => None,
    };
    let (lookup, reason) = core::plan(&r, pin.as_ref(), cas::hot_reload_enabled()).map_err(bad)?;
    let row = cas::lookup(&pool, &lookup).await?.ok_or_else(|| {
        AppError::NotFound(format!("Playbook not found: {}", core::describe(&lookup)))
    })?;
    let digest = cas::digest_for(&pool, &row).await?;
    let entry = core::Entry {
        catalog_id: row.catalog_id,
        path: row.path.clone(),
        version: row.version as i32,
        digest,
    };
    let res = core::finish(&lookup, reason, Some(entry)).map_err(bad)?;
    let sources = cas::sources(&pool, &res.entry.digest).await?;
    Ok(Json(ResolveReply {
        catalog_id: res.entry.catalog_id.to_string(),
        path: res.entry.path,
        version: res.entry.version,
        kind: row.kind,
        digest: res.entry.digest,
        reason: res.reason.as_str().to_string(),
        sources,
        pinned_by: pin.map(|p| p.root_execution_id.to_string()),
    }))
}

async fn blob(
    State(pool): State<DbPool>,
    Path(digest): Path<String>,
    headers: HeaderMap,
) -> AppResult<Response> {
    if !core::is_digest(&digest) {
        return Err(AppError::Validation(format!(
            "'{digest}' is not sha256: + 64 lowercase hex"
        )));
    }
    let etag = format!("\"{digest}\"");
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        == Some(etag.as_str())
    {
        return Ok(StatusCode::NOT_MODIFIED.into_response());
    }
    let body = cas::blob(&pool, &digest)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("no body with digest {digest}")))?;
    let mut resp = body.into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/yaml"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    Ok(resp)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceBody {
    /// A digest, or a name (`a/b@3`) whose body the source holds.
    #[serde(rename = "ref")]
    pub reference: String,
    pub uri: String,
}

async fn add_source(
    State(pool): State<DbPool>,
    _token: crate::handlers::internal::RequireInternalApiToken,
    Json(body): Json<SourceBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let r = core::PlaybookRef::parse(&body.reference).map_err(bad)?;
    if matches!(r, core::PlaybookRef::Source { .. }) {
        return Err(AppError::Validation(
            "ref must be a digest or a catalog name, not a URI".into(),
        ));
    }
    let (lookup, reason) = core::plan(&r, None, false).map_err(bad)?;
    let row = cas::lookup(&pool, &lookup).await?.ok_or_else(|| {
        AppError::NotFound(format!("Playbook not found: {}", core::describe(&lookup)))
    })?;
    let digest = cas::digest_for(&pool, &row).await?;
    core::finish(
        &lookup,
        reason,
        Some(core::Entry {
            catalog_id: row.catalog_id,
            path: row.path,
            version: row.version as i32,
            digest: digest.clone(),
        }),
    )
    .map_err(bad)?;
    let uri = cas::check_declared_source(&body.uri, &digest)?;
    let added = cas::add_source(&pool, &digest, &uri, "declared", false).await?;
    Ok((
        StatusCode::OK,
        Json(
            serde_json::json!({ "digest": digest, "uri": uri, "added": added, "verified": false }),
        ),
    ))
}

async fn pin(
    State(pool): State<DbPool>,
    Path(execution_id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    cas::get_pin(&pool, execution_id)
        .await?
        .map(|p| Json(p.api_json()))
        .ok_or_else(|| AppError::NotFound(format!("execution {execution_id} has no catalog pin")))
}
