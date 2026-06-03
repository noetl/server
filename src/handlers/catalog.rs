//! Catalog API handlers.
//!
//! Endpoints for managing playbooks, tools, and other resources
//! in the NoETL catalog.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use chrono::Utc;
use serde::Deserialize;

use crate::db::models::{
    CatalogEntries, CatalogEntriesRequest, CatalogEntryRequest, CatalogEntryResponse,
    CatalogRegisterRequest, CatalogRegisterResponse,
};
use crate::error::{AppError, AppResult};
use crate::services::ui_schema::{infer_ui_schema, UiSchemaResponse};
use crate::services::CatalogService;

/// Register a new catalog resource.
///
/// `POST /api/catalog/register`
///
/// # Request Body
///
/// ```json
/// {
///   "content": "apiVersion: noetl.io/v1\nkind: Playbook\n...",
///   "resource_type": "Playbook"
/// }
/// ```
///
/// # Response
///
/// ```json
/// {
///   "status": "success",
///   "message": "Resource 'path/to/playbook' version '1' registered.",
///   "path": "path/to/playbook",
///   "version": 1,
///   "catalog_id": "123456789",
///   "kind": "Playbook"
/// }
/// ```
pub async fn register(
    service: State<CatalogService>,
    request: Json<CatalogRegisterRequest>,
) -> AppResult<(StatusCode, Json<CatalogRegisterResponse>)> {
    let started_at = std::time::Instant::now();
    let result = register_inner(service, request).await;
    let status_label = if result.is_ok() { "ok" } else { "error" };
    crate::metrics::record_write_request(
        crate::metrics::endpoint::CATALOG_REGISTER,
        status_label,
        started_at.elapsed().as_secs_f64(),
    );
    result
}

async fn register_inner(
    State(service): State<CatalogService>,
    Json(request): Json<CatalogRegisterRequest>,
) -> AppResult<(StatusCode, Json<CatalogRegisterResponse>)> {
    let response = service.register(request).await?;
    Ok((StatusCode::OK, Json(response)))
}

/// List all catalog resources.
///
/// `POST /api/catalog/list`
///
/// # Request Body
///
/// ```json
/// {
///   "resource_type": "Playbook"  // optional filter
/// }
/// ```
///
/// # Response
///
/// ```json
/// {
///   "entries": [
///     {
///       "catalog_id": "123456789",
///       "path": "path/to/playbook",
///       "kind": "Playbook",
///       "version": 1,
///       "created_at": "2025-01-01T00:00:00Z"
///     }
///   ]
/// }
/// ```
pub async fn list(
    State(service): State<CatalogService>,
    Json(request): Json<CatalogEntriesRequest>,
) -> AppResult<Json<CatalogEntries>> {
    let entries = service.list(request.resource_type.as_deref()).await?;
    Ok(Json(entries))
}

/// Get a specific catalog resource.
///
/// `POST /api/catalog/resource`
///
/// # Request Body
///
/// Lookup by catalog_id:
/// ```json
/// {
///   "catalog_id": "123456789"
/// }
/// ```
///
/// Lookup by path and version:
/// ```json
/// {
///   "path": "path/to/playbook",
///   "version": "latest"
/// }
/// ```
///
/// # Response
///
/// ```json
/// {
///   "catalog_id": "123456789",
///   "path": "path/to/playbook",
///   "kind": "Playbook",
///   "version": 1,
///   "content": "apiVersion: noetl.io/v1...",
///   "created_at": "2025-01-01T00:00:00Z"
/// }
/// ```
pub async fn get_resource(
    State(service): State<CatalogService>,
    Json(request): Json<CatalogEntryRequest>,
) -> AppResult<Json<CatalogEntryResponse>> {
    let entry = service.get_resource(request).await?;
    Ok(Json(entry.into()))
}

/// Query parameters for the `ui_schema` endpoint.
#[derive(Debug, Deserialize)]
pub struct UiSchemaQuery {
    /// Catalog version to inspect. Defaults to `"latest"` to match
    /// the Python `noetl-server` contract.
    #[serde(default)]
    pub version: Option<String>,
}

/// Inferred workload form for a Playbook / Agent / Mcp resource.
///
/// `GET /api/catalog/{*path}` — the handler routes by suffix.  Only
/// requests whose tail ends with `/ui_schema` are served; everything
/// else falls through as 404.  This is the Rust port of the Python
/// route `GET /api/catalog/{path:path}/ui_schema` (FastAPI's
/// `{path:path}` accepts slash-bearing paths; axum's wildcard
/// `{*tail}` is the equivalent).
///
/// # Path
///
/// - `tail`: everything after `/api/catalog/`.  Expected shape is
///   `<catalog-path>/ui_schema` where `<catalog-path>` may contain
///   slashes (e.g. `system/outbox_publisher/ui_schema`).
///
/// # Query
///
/// - `version`: catalog version (numeric string or `"latest"`).
///   Defaults to `"latest"`.
///
/// # Response
///
/// `UiSchemaResponse` — see [`crate::services::ui_schema`] for the
/// shape.  Byte-identical to the Python `noetl-server` for the same
/// input per noetl/ai-meta#49 constraint #2.
pub async fn ui_schema(
    State(service): State<CatalogService>,
    Path(tail): Path<String>,
    Query(query): Query<UiSchemaQuery>,
) -> AppResult<Json<UiSchemaResponse>> {
    // The wildcard route catches all `/api/catalog/*` GETs; the
    // ui_schema variant is the one whose tail ends with `/ui_schema`.
    // Anything else from this route is treated as not-found so it
    // doesn't accidentally swallow paths we haven't ported yet.
    let suffix = "/ui_schema";
    let Some(catalog_path) = tail.strip_suffix(suffix) else {
        return Err(AppError::NotFound(format!(
            "no route matched GET /api/catalog/{tail}"
        )));
    };
    if catalog_path.is_empty() {
        return Err(AppError::Validation(
            "ui_schema path must not be empty".to_string(),
        ));
    }

    let request = CatalogEntryRequest {
        catalog_id: None,
        path: Some(catalog_path.to_string()),
        version: Some(
            query
                .version
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "latest".to_string()),
        ),
    };

    let entry = service.get_resource(request).await?;

    // Parse YAML for metadata. Forgiving — if the parse fails, return
    // empty metadata rather than 500, mirroring Python's behaviour.
    let metadata = parse_metadata(&entry.content);

    let fields = infer_ui_schema(&entry.content);

    let response = UiSchemaResponse {
        path: entry.path,
        version: entry.version,
        kind: entry.kind.to_lowercase(),
        title: metadata.name,
        description_markdown: metadata.description,
        exposed_in_ui: metadata.exposed_in_ui,
        fields,
        generated_at: Utc::now(),
    };
    Ok(Json(response))
}

#[derive(Debug, Default)]
struct CatalogMetadata {
    name: Option<String>,
    description: Option<String>,
    exposed_in_ui: bool,
}

/// Extract the `metadata` block from a YAML document.  Returns an
/// empty `CatalogMetadata` when the document doesn't parse or has no
/// `metadata` mapping — matches Python's forgiving handling.
fn parse_metadata(yaml_text: &str) -> CatalogMetadata {
    let parsed: serde_yaml::Value = match serde_yaml::from_str(yaml_text) {
        Ok(v) => v,
        Err(_) => return CatalogMetadata::default(),
    };
    let metadata = match parsed.get("metadata") {
        Some(serde_yaml::Value::Mapping(m)) => m,
        _ => return CatalogMetadata::default(),
    };
    CatalogMetadata {
        name: metadata
            .get(serde_yaml::Value::String("name".to_string()))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        description: metadata
            .get(serde_yaml::Value::String("description".to_string()))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        exposed_in_ui: metadata
            .get(serde_yaml::Value::String("exposed_in_ui".to_string()))
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_metadata_name_description_flag() {
        let yaml = "\
apiVersion: v1
kind: Playbook
metadata:
  name: test-playbook
  description: A test
  exposed_in_ui: true
workload:
  foo: bar
";
        let meta = parse_metadata(yaml);
        assert_eq!(meta.name.as_deref(), Some("test-playbook"));
        assert_eq!(meta.description.as_deref(), Some("A test"));
        assert!(meta.exposed_in_ui);
    }

    #[test]
    fn parse_metadata_missing_block_returns_default() {
        let yaml = "workload:\n  foo: bar\n";
        let meta = parse_metadata(yaml);
        assert!(meta.name.is_none());
        assert!(meta.description.is_none());
        assert!(!meta.exposed_in_ui);
    }

    #[test]
    fn parse_metadata_malformed_yaml_returns_default() {
        let yaml = "metadata:\n  name: 'unterminated\n";
        let meta = parse_metadata(yaml);
        assert!(meta.name.is_none());
    }
}
