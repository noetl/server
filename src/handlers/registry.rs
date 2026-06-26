//! Registry HTTP handlers ([noetl/ai-meta#146](https://github.com/noetl/ai-meta/issues/146),
//! platform foundation **G3**).
//!
//! The versioned model / dataset / eval / release registry the SLM MLOps stages
//! (`finetune` / `package` / `eval`) write to. All routes are under
//! `/api/internal/registry/*` and service-account-gated like the rest of the
//! internal family ([data-access-boundary.md](https://github.com/noetl/ai-meta/blob/main/agents/rules/data-access-boundary.md))
//! — workers / playbooks reach the `noetl.registry` table only through here.
//!
//! - `POST /api/internal/registry/register` — register a new entry (mints the
//!   next monotonic version). Returns the entry + its `registry://` URN.
//! - `GET  /api/internal/registry/list?tenant=&project=&kind=&name=&limit=` —
//!   list entries newest-first.
//! - `GET  /api/internal/registry/resolve?ref=registry://…&tenant=&project=` —
//!   resolve a `registry://` URN (supports a `latest` version) to its entry.
//!
//! The whole group is **mounted only when `NOETL_REGISTRY_ENABLED=true`** (the
//! flag also gates the table creation), so a default deployment carries no new
//! route or schema. Artifact bytes themselves go through the existing object
//! endpoint (`PUT/GET /api/internal/objects/{*key}`); this index records where
//! they live (`artifact_uri`).

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

use crate::error::AppResult;
use crate::handlers::internal::RequireInternalApiToken;
use crate::services::registry::{
    parse_ref, RegisterBody, RegistryService, DEFAULT_PROJECT, DEFAULT_TENANT,
};

/// Injected registry dep: the service (pool + snowflake + object backend).
#[derive(Clone)]
pub struct RegistryDeps {
    pub service: RegistryService,
}

/// `POST /api/internal/registry/register`.
#[tracing::instrument(skip(deps, _token, body), fields(kind = %body.kind, name = %body.name))]
pub async fn register(
    State(deps): State<RegistryDeps>,
    _token: RequireInternalApiToken,
    Json(body): Json<RegisterBody>,
) -> AppResult<impl IntoResponse> {
    let outcome = deps.service.register(&body).await;
    crate::metrics::record_registry_op("register", outcome.is_ok());
    let entry = outcome?;
    tracing::info!(
        registry_ref = %entry.r#ref,
        entry_id = entry.entry_id,
        version = entry.version,
        artifact = entry.artifact_uri.is_some(),
        "registry: registered entry (#146 G3)"
    );
    Ok((StatusCode::OK, Json(entry)))
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub tenant: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// `GET /api/internal/registry/list`.
#[tracing::instrument(skip(deps, _token))]
pub async fn list(
    State(deps): State<RegistryDeps>,
    _token: RequireInternalApiToken,
    Query(q): Query<ListQuery>,
) -> AppResult<impl IntoResponse> {
    let tenant = q.tenant.as_deref().unwrap_or(DEFAULT_TENANT);
    let project = q.project.as_deref().unwrap_or(DEFAULT_PROJECT);
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let outcome = deps
        .service
        .list(
            tenant,
            project,
            q.kind.as_deref(),
            q.name.as_deref(),
            limit,
        )
        .await;
    crate::metrics::record_registry_op("list", outcome.is_ok());
    let entries = outcome?;
    Ok((StatusCode::OK, Json(serde_json::json!({ "entries": entries }))))
}

#[derive(Debug, Deserialize)]
pub struct ResolveQuery {
    pub r#ref: String,
    #[serde(default)]
    pub tenant: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
}

/// `GET /api/internal/registry/resolve`.
#[tracing::instrument(skip(deps, _token), fields(registry_ref = %q.r#ref))]
pub async fn resolve(
    State(deps): State<RegistryDeps>,
    _token: RequireInternalApiToken,
    Query(q): Query<ResolveQuery>,
) -> AppResult<impl IntoResponse> {
    let tenant = q.tenant.as_deref().unwrap_or(DEFAULT_TENANT);
    let project = q.project.as_deref().unwrap_or(DEFAULT_PROJECT);
    let parsed = match parse_ref(&q.r#ref, tenant, project) {
        Ok(r) => r,
        Err(msg) => {
            crate::metrics::record_registry_op("resolve", false);
            return Ok((StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": msg })))
                .into_response());
        }
    };
    let outcome = deps.service.resolve(&parsed).await;
    crate::metrics::record_registry_op("resolve", outcome.is_ok());
    match outcome? {
        Some(entry) => Ok((StatusCode::OK, Json(entry)).into_response()),
        None => Ok((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("registry entry not found: {}", q.r#ref) })),
        )
            .into_response()),
    }
}
