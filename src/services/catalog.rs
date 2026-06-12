//! Catalog service for managing playbooks and resources.

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};

use crate::db::models::{
    CatalogEntries, CatalogEntry, CatalogEntryRequest, CatalogEntryResponse,
    CatalogRegisterRequest, CatalogRegisterResponse,
};
use crate::db::queries::catalog as queries;
use crate::db::DbPool;
use crate::error::{AppError, AppResult};

/// Service for catalog operations.
#[derive(Clone)]
pub struct CatalogService {
    pool: DbPool,
}

impl CatalogService {
    /// Create a new catalog service.
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Register a new resource in the catalog.
    pub async fn register(
        &self,
        request: CatalogRegisterRequest,
    ) -> AppResult<CatalogRegisterResponse> {
        // Decode content if base64 encoded
        let content = self.decode_content(&request.content)?;

        // Parse YAML to extract metadata
        let yaml: serde_yaml::Value = serde_yaml::from_str(&content)
            .map_err(|e| AppError::Validation(format!("Invalid YAML: {}", e)))?;

        // Extract metadata
        let metadata = yaml
            .get("metadata")
            .ok_or_else(|| AppError::Validation("Missing 'metadata' section".to_string()))?;

        let path = metadata
            .get("path")
            .and_then(|v| v.as_str())
            .or_else(|| metadata.get("name").and_then(|v| v.as_str()))
            .ok_or_else(|| {
                AppError::Validation("Missing 'path' or 'name' in metadata".to_string())
            })?
            .to_string();

        let kind = yaml
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or(&request.resource_type)
            .to_lowercase();

        // A `kind: Subscription` entry is a first-class catalog type
        // (noetl/ai-meta#90 Phase 2) — it is never dispatched as a step
        // DAG, so it is validated against the source/mode/dispatch schema
        // instead of a `workflow:` block.  Reject a malformed spec at
        // registration so the continuous runtime never activates a broken
        // subscription.
        if kind == "subscription" {
            validate_subscription_spec(&yaml)?;
        }

        // Get next version
        let version = queries::get_next_version(&self.pool, &path).await?;

        // Extract optional fields
        let payload = yaml
            .get("workload")
            .map(|v| serde_json::to_value(v).unwrap_or(serde_json::Value::Null));
        let layout = yaml
            .get("workflow")
            .map(|v| serde_json::to_value(v).unwrap_or(serde_json::Value::Null));
        let meta = metadata
            .get("labels")
            .map(|v| serde_json::to_value(v).unwrap_or(serde_json::Value::Null));

        // Insert into database
        let catalog_id = queries::insert_catalog_entry(
            &self.pool,
            &path,
            &kind,
            version,
            &content,
            layout.as_ref(),
            payload.as_ref(),
            meta.as_ref(),
        )
        .await?;

        Ok(CatalogRegisterResponse {
            status: "success".to_string(),
            message: format!("Resource '{}' version '{}' registered.", path, version),
            path,
            version,
            catalog_id: catalog_id.to_string(),
            kind,
        })
    }

    /// List catalog entries.
    pub async fn list(&self, resource_type: Option<&str>) -> AppResult<CatalogEntries> {
        let entries = queries::list_catalog_entries(&self.pool, resource_type).await?;

        let responses: Vec<CatalogEntryResponse> = entries.into_iter().map(|e| e.into()).collect();

        Ok(CatalogEntries { entries: responses })
    }

    /// Get a specific catalog resource.
    pub async fn get_resource(&self, request: CatalogEntryRequest) -> AppResult<CatalogEntry> {
        // Priority: catalog_id > path + version
        if let Some(catalog_id) = &request.catalog_id {
            let id: i64 = catalog_id
                .parse()
                .map_err(|_| AppError::Validation("Invalid catalog_id".to_string()))?;

            return queries::get_catalog_by_id(&self.pool, id)
                .await?
                .ok_or_else(|| {
                    AppError::NotFound(format!("Catalog entry '{}' not found", catalog_id))
                });
        }

        if let Some(path) = &request.path {
            // Check for specific version or "latest"
            if let Some(version_str) = &request.version {
                if version_str == "latest" {
                    return queries::get_catalog_latest(&self.pool, path)
                        .await?
                        .ok_or_else(|| {
                            AppError::NotFound(format!("Catalog entry '{}' not found", path))
                        });
                }

                let version: i16 = version_str
                    .parse()
                    .map_err(|_| AppError::Validation("Invalid version number".to_string()))?;

                return queries::get_catalog_by_path_version(&self.pool, path, version)
                    .await?
                    .ok_or_else(|| {
                        AppError::NotFound(format!(
                            "Catalog entry '{}' version {} not found",
                            path, version
                        ))
                    });
            }

            // Default to latest if no version specified
            return queries::get_catalog_latest(&self.pool, path)
                .await?
                .ok_or_else(|| AppError::NotFound(format!("Catalog entry '{}' not found", path)));
        }

        Err(AppError::Validation(
            "Either 'catalog_id' or 'path' must be provided".to_string(),
        ))
    }

    /// Decode content that may be base64 encoded.
    fn decode_content(&self, content: &str) -> AppResult<String> {
        // Try to decode as base64 first
        if let Ok(decoded) = BASE64.decode(content) {
            if let Ok(s) = String::from_utf8(decoded) {
                return Ok(s);
            }
        }

        // Return as-is if not valid base64
        Ok(content.to_string())
    }
}

/// Source backends a `kind: Subscription` may declare.
const SUBSCRIPTION_SOURCES: &[&str] = &["pubsub", "nats", "kafka", "webhook"];
/// Activation modes for a pull subscription.
const SUBSCRIPTION_ACTIVATIONS: &[&str] = &["continuous", "scheduled"];

/// Validate a `kind: Subscription` catalog entry against the RFC §4.1 schema
/// (noetl/ai-meta#90).  This is the **type-level** isolation of the
/// subscription workload class: it requires the `source` / `mode` / `dispatch`
/// shape and explicitly does **not** require a `workflow:` step DAG (a
/// subscription is activated on a runtime, never dispatched as a one-shot
/// DAG).
fn validate_subscription_spec(yaml: &serde_yaml::Value) -> AppResult<()> {
    let spec = yaml
        .get("spec")
        .ok_or_else(|| AppError::Validation("kind: Subscription requires a 'spec' block".into()))?;

    // source — required, from the supported set.
    let source = spec
        .get("source")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::Validation("subscription spec requires 'source'".into()))?;
    if !SUBSCRIPTION_SOURCES.contains(&source) {
        return Err(AppError::Validation(format!(
            "subscription 'source' must be one of {:?}, got '{}'",
            SUBSCRIPTION_SOURCES, source
        )));
    }

    // mode — required, pull | push.
    let mode = spec
        .get("mode")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::Validation("subscription spec requires 'mode' (pull | push)".into()))?;
    match mode {
        "pull" => {
            // activation defaults to continuous; validate if present.
            if let Some(act) = spec.get("activation").and_then(|v| v.as_str()) {
                if !SUBSCRIPTION_ACTIVATIONS.contains(&act) {
                    return Err(AppError::Validation(format!(
                        "subscription 'activation' must be one of {:?}, got '{}'",
                        SUBSCRIPTION_ACTIVATIONS, act
                    )));
                }
            }
        }
        "push" => {
            // Push ingress (Mode C) lands in Phase 3; the type accepts the
            // shape now so a spec can be registered ahead of the gateway.
        }
        other => {
            return Err(AppError::Validation(format!(
                "subscription 'mode' must be 'pull' or 'push', got '{}'",
                other
            )));
        }
    }

    // dispatch.playbook — required: the ordinary playbook run per message.
    let dispatch = spec
        .get("dispatch")
        .ok_or_else(|| AppError::Validation("subscription spec requires a 'dispatch' block".into()))?;
    dispatch
        .get("playbook")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AppError::Validation("subscription 'dispatch.playbook' is required".into())
        })?;

    // headers (directive allowlist) — optional; structural-only check here.
    // The strict allowlist/value validation lives in the worker's
    // noetl-tools directive engine (RFC §7); the server only checks shape so
    // a malformed block is caught early.
    if let Some(headers) = spec.get("headers") {
        if !headers.is_mapping() {
            return Err(AppError::Validation(
                "subscription 'headers' must be a mapping".into(),
            ));
        }
        if let Some(directives) = headers.get("directives") {
            let seq = directives.as_sequence().ok_or_else(|| {
                AppError::Validation("subscription 'headers.directives' must be a list".into())
            })?;
            for (i, d) in seq.iter().enumerate() {
                let has_header = d.get("header").and_then(|v| v.as_str()).is_some();
                let has_controls = d.get("controls").and_then(|v| v.as_str()).is_some();
                if !has_header || !has_controls {
                    return Err(AppError::Validation(format!(
                        "subscription 'headers.directives[{}]' requires 'header' and 'controls'",
                        i
                    )));
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn yaml(s: &str) -> serde_yaml::Value {
        serde_yaml::from_str(s).unwrap()
    }

    #[test]
    fn valid_pull_continuous_subscription() {
        let v = yaml(
            r#"
apiVersion: noetl.io/v1
kind: Subscription
metadata: { name: iot, path: subscriptions/iot }
spec:
  source: pubsub
  mode: pull
  activation: continuous
  auth: pubsub_iot
  subscription: "projects/acme/subscriptions/sensors"
  dispatch: { playbook: domain/ingest, payload_from: message.json, execution_pool: iot }
  headers:
    directives:
      - header: x-noetl-pool
        controls: dispatch.execution_pool
        allowed: ["iot"]
"#,
        );
        assert!(validate_subscription_spec(&v).is_ok());
    }

    #[test]
    fn valid_push_subscription() {
        let v = yaml(
            r#"
kind: Subscription
metadata: { name: stripe, path: subscriptions/stripe }
spec:
  source: webhook
  mode: push
  dispatch: { playbook: domain/handle_stripe }
"#,
        );
        assert!(validate_subscription_spec(&v).is_ok());
    }

    #[test]
    fn missing_spec_rejected() {
        let v = yaml("kind: Subscription\nmetadata: { name: x, path: p }\n");
        let err = validate_subscription_spec(&v).unwrap_err();
        assert!(format!("{err}").contains("'spec'"));
    }

    #[test]
    fn bad_source_rejected() {
        let v = yaml(
            "kind: Subscription\nspec:\n  source: rabbitmq\n  mode: pull\n  dispatch: { playbook: p }\n",
        );
        let err = validate_subscription_spec(&v).unwrap_err();
        assert!(format!("{err}").contains("source"));
    }

    #[test]
    fn missing_dispatch_playbook_rejected() {
        let v = yaml("kind: Subscription\nspec:\n  source: nats\n  mode: pull\n  dispatch: {}\n");
        let err = validate_subscription_spec(&v).unwrap_err();
        assert!(format!("{err}").contains("dispatch.playbook"));
    }

    #[test]
    fn bad_mode_rejected() {
        let v = yaml(
            "kind: Subscription\nspec:\n  source: nats\n  mode: streaming\n  dispatch: { playbook: p }\n",
        );
        let err = validate_subscription_spec(&v).unwrap_err();
        assert!(format!("{err}").contains("mode"));
    }

    #[test]
    fn bad_activation_rejected() {
        let v = yaml(
            "kind: Subscription\nspec:\n  source: nats\n  mode: pull\n  activation: bogus\n  dispatch: { playbook: p }\n",
        );
        let err = validate_subscription_spec(&v).unwrap_err();
        assert!(format!("{err}").contains("activation"));
    }

    #[test]
    fn malformed_directives_rejected() {
        let v = yaml(
            r#"
kind: Subscription
spec:
  source: nats
  mode: pull
  dispatch: { playbook: p }
  headers:
    directives:
      - controls: dispatch.playbook
"#,
        );
        let err = validate_subscription_spec(&v).unwrap_err();
        assert!(format!("{err}").contains("header"));
    }

    #[test]
    fn no_workflow_dag_required() {
        // A subscription with NO workflow/steps block validates fine — the
        // type does not require a step DAG.
        let v = yaml(
            "kind: Subscription\nspec:\n  source: kafka\n  mode: pull\n  dispatch: { playbook: domain/x }\n",
        );
        assert!(validate_subscription_spec(&v).is_ok());
    }
}
