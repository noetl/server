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
            // Push ingress (Mode C, noetl/ai-meta#90 Phase 3): the spec must
            // declare how the gateway verifies inbound deliveries before any
            // directive is honored (RFC §6 / §7.5).  Validate the `ingress`
            // block shape here so a misconfigured push subscription is
            // rejected at registration, not at the first webhook.
            validate_push_ingress(spec)?;
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

    // spool (store-and-forward) — optional; structural check (noetl/ai-meta#90
    // Phase 4, RFC §8).  The strict cross-field validation lives in the
    // worker's noetl-tools `SpoolSpec::parse`; the server rejects an
    // obviously-malformed block at registration so a broken outage buffer is
    // caught before the first downstream failure, not during one.
    if let Some(spool) = spec.get("spool") {
        validate_spool_config(spool)?;
    }

    Ok(())
}

/// `spool.mode` values (RFC §8.2).
const SPOOL_MODES: &[&str] = &["off", "buffer_and_ack", "hybrid"];
/// `spool.backend` values (RFC §8.3).
const SPOOL_BACKENDS: &[&str] = &["nats_object", "local_disk", "gcs", "s3"];
/// `spool.ordering` values (RFC §8.3).
const SPOOL_ORDERINGS: &[&str] = &["global", "per_key", "none"];

/// Validate the `spec.spool` block (noetl/ai-meta#90 Phase 4).
///
/// Structural guarantees the worker runtime needs to stand the spool up:
/// a valid `mode`/`backend`/`ordering`, and the backend's required target
/// (a `bucket` for `nats_object`/`gcs`/`s3`, a `path` for `local_disk`).
///
/// For `gcs`/`s3` the `credential` keychain alias is **optional**: present →
/// a tenant-owned external bucket resolved by alias (`data-access-boundary.md`);
/// absent → the platform's own bucket reached via "already-in-place trust" —
/// ADC / Workload Identity (gcs) or the instance profile (s3) — which is the
/// out-of-cluster Cloud Run path (RFC #90 Phase 5, `execution-model.md`).
fn validate_spool_config(spool: &serde_yaml::Value) -> AppResult<()> {
    if !spool.is_mapping() {
        return Err(AppError::Validation("subscription 'spool' must be a mapping".into()));
    }
    let mode = spool.get("mode").and_then(|v| v.as_str()).unwrap_or("off");
    if !SPOOL_MODES.contains(&mode) {
        return Err(AppError::Validation(format!(
            "subscription 'spool.mode' must be one of {:?}, got '{}'",
            SPOOL_MODES, mode
        )));
    }
    // `off` needs no backend config — pure stop-acking.
    if mode == "off" {
        return Ok(());
    }

    let backend = spool
        .get("backend")
        .and_then(|v| v.as_str())
        .unwrap_or("nats_object");
    if !SPOOL_BACKENDS.contains(&backend) {
        return Err(AppError::Validation(format!(
            "subscription 'spool.backend' must be one of {:?}, got '{}'",
            SPOOL_BACKENDS, backend
        )));
    }
    let nonempty = |key: &str| -> bool {
        spool
            .get(key)
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    };
    match backend {
        "nats_object" | "gcs" | "s3" => {
            if !nonempty("bucket") {
                return Err(AppError::Validation(format!(
                    "subscription 'spool.backend' '{}' requires a non-empty 'bucket'",
                    backend
                )));
            }
        }
        "local_disk" => {
            if !nonempty("path") {
                return Err(AppError::Validation(
                    "subscription 'spool.backend' 'local_disk' requires a non-empty 'path'".into(),
                ));
            }
        }
        _ => {}
    }
    // `credential` is optional for gcs/s3 — absent means ADC / Workload
    // Identity (the Cloud Run platform-bucket path); present means a keychain
    // alias for a tenant-owned external bucket. Either is valid.

    if let Some(ordering) = spool.get("ordering").and_then(|v| v.as_str()) {
        if !SPOOL_ORDERINGS.contains(&ordering) {
            return Err(AppError::Validation(format!(
                "subscription 'spool.ordering' must be one of {:?}, got '{}'",
                SPOOL_ORDERINGS, ordering
            )));
        }
        // interleave drain is order-unsafe with global ordering.
        let on_recovery = spool
            .get("drain")
            .and_then(|d| d.get("on_recovery"))
            .and_then(|v| v.as_str());
        if ordering == "global" && on_recovery == Some("interleave") {
            return Err(AppError::Validation(
                "subscription 'spool.drain.on_recovery: interleave' is unsafe with \
                 'ordering: global'; use 'ordered_then_live' or 'per_key'/'none'"
                    .into(),
            ));
        }
    }
    Ok(())
}

/// Verify schemes a push `kind: Subscription` ingress may declare (RFC §6).
/// `none` is intentionally absent — push ingress always verifies.
const PUSH_VERIFY_TYPES: &[&str] = &["hmac_sha256", "bearer", "pubsub_oidc"];

/// Validate the push-mode `spec.ingress` block (noetl/ai-meta#90 Phase 3).
///
/// The gateway is the only component that terminates untrusted inbound
/// traffic, so a push subscription must declare *how* the gateway verifies a
/// delivery (RFC §6) and *where* a verification secret is resolved from the
/// Secrets Wallet (by alias — never a gateway env var).  This check is
/// structural: it guarantees the gateway's config endpoint
/// (`GET /api/internal/ingress/{listener}`) can build a complete verifier.
///
/// Per-scheme requirements:
/// - `hmac_sha256` — `header` (the signature header) + `secret` (Wallet alias).
/// - `bearer`      — `secret` (Wallet alias for the expected token).
/// - `pubsub_oidc` — `audience` + `service_account` (Google-signed JWT check;
///   the JWKS is public, so no Wallet secret).
fn validate_push_ingress(spec: &serde_yaml::Value) -> AppResult<()> {
    let ingress = spec.get("ingress").ok_or_else(|| {
        AppError::Validation(
            "subscription 'mode: push' requires an 'ingress' block (gateway_path + verify)".into(),
        )
    })?;

    let gateway_path = ingress
        .get("gateway_path")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AppError::Validation("subscription 'ingress.gateway_path' is required for push".into())
        })?;
    if !gateway_path.starts_with('/') {
        return Err(AppError::Validation(format!(
            "subscription 'ingress.gateway_path' must be an absolute path starting with '/', got '{}'",
            gateway_path
        )));
    }

    let verify = ingress.get("verify").ok_or_else(|| {
        AppError::Validation("subscription 'ingress' requires a 'verify' block (push always verifies)".into())
    })?;
    let vtype = verify
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::Validation("subscription 'ingress.verify.type' is required".into()))?;
    if !PUSH_VERIFY_TYPES.contains(&vtype) {
        return Err(AppError::Validation(format!(
            "subscription 'ingress.verify.type' must be one of {:?}, got '{}' \
             ('none' is not allowed — push ingress always verifies)",
            PUSH_VERIFY_TYPES, vtype
        )));
    }

    let require = |field: &str| -> AppResult<()> {
        verify
            .get(field)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|_| ())
            .ok_or_else(|| {
                AppError::Validation(format!(
                    "subscription 'ingress.verify.{}' is required for verify.type '{}'",
                    field, vtype
                ))
            })
    };

    match vtype {
        "hmac_sha256" => {
            require("header")?;
            require("secret")?;
        }
        "bearer" => {
            require("secret")?;
        }
        "pubsub_oidc" => {
            require("audience")?;
            require("service_account")?;
        }
        _ => unreachable!("verify.type already validated against PUSH_VERIFY_TYPES"),
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
    fn valid_pull_with_spool_buffer_and_ack() {
        let v = yaml(
            r#"
kind: Subscription
spec:
  source: nats
  mode: pull
  stream: IOT
  consumer: iot-drain
  dispatch: { playbook: domain/ingest, execution_pool: iot }
  spool:
    mode: buffer_and_ack
    backend: nats_object
    bucket: noetl_spool_iot
    ordering: per_key
    ordering_key: device_id
    circuit:
      trip_after: 3
      probe_after_ms: 5000
      downstream:
        - { name: warehouse, type: http, target: "http://wh/health" }
    drain: { on_recovery: ordered_then_live }
"#,
        );
        assert!(validate_subscription_spec(&v).is_ok());
    }

    #[test]
    fn spool_off_needs_no_backend() {
        let v = yaml(
            "kind: Subscription\nspec:\n  source: nats\n  mode: pull\n  stream: S\n  consumer: C\n  dispatch: { playbook: p }\n  spool: { mode: off }\n",
        );
        assert!(validate_subscription_spec(&v).is_ok());
    }

    #[test]
    fn spool_buffer_and_ack_requires_bucket() {
        let v = yaml(
            "kind: Subscription\nspec:\n  source: nats\n  mode: pull\n  stream: S\n  consumer: C\n  dispatch: { playbook: p }\n  spool: { mode: buffer_and_ack, backend: nats_object }\n",
        );
        let err = validate_subscription_spec(&v).unwrap_err();
        assert!(format!("{err}").contains("bucket"));
    }

    #[test]
    fn spool_gcs_requires_bucket() {
        // gcs (like nats_object/s3) requires a bucket...
        let v = yaml(
            "kind: Subscription\nspec:\n  source: nats\n  mode: pull\n  stream: S\n  consumer: C\n  dispatch: { playbook: p }\n  spool: { mode: buffer_and_ack, backend: gcs }\n",
        );
        let err = validate_subscription_spec(&v).unwrap_err();
        assert!(format!("{err}").contains("bucket"));
    }

    #[test]
    fn spool_gcs_without_credential_is_valid_adc() {
        // ...but `credential` is optional: a gcs spool with just a bucket is
        // valid (ADC / Workload Identity — the Cloud Run path, #90 Phase 5).
        let v = yaml(
            "kind: Subscription\nspec:\n  source: nats\n  mode: pull\n  stream: S\n  consumer: C\n  dispatch: { playbook: p }\n  spool: { mode: buffer_and_ack, backend: gcs, bucket: b }\n",
        );
        assert!(validate_subscription_spec(&v).is_ok());
    }

    #[test]
    fn spool_interleave_global_rejected() {
        let v = yaml(
            "kind: Subscription\nspec:\n  source: nats\n  mode: pull\n  stream: S\n  consumer: C\n  dispatch: { playbook: p }\n  spool: { mode: buffer_and_ack, backend: local_disk, path: /tmp/s, ordering: global, drain: { on_recovery: interleave } }\n",
        );
        let err = validate_subscription_spec(&v).unwrap_err();
        assert!(format!("{err}").contains("interleave"));
    }

    #[test]
    fn spool_bad_mode_rejected() {
        let v = yaml(
            "kind: Subscription\nspec:\n  source: nats\n  mode: pull\n  stream: S\n  consumer: C\n  dispatch: { playbook: p }\n  spool: { mode: bogus }\n",
        );
        assert!(validate_subscription_spec(&v).is_err());
    }

    #[test]
    fn valid_push_subscription_hmac() {
        let v = yaml(
            r#"
kind: Subscription
metadata: { name: stripe, path: subscriptions/stripe }
spec:
  source: webhook
  mode: push
  ingress:
    gateway_path: /ingress/stripe
    verify:
      type: hmac_sha256
      header: "Stripe-Signature"
      secret: "stripe_webhook_secret"
  dispatch: { playbook: domain/handle_stripe }
"#,
        );
        assert!(validate_subscription_spec(&v).is_ok());
    }

    #[test]
    fn valid_push_subscription_bearer() {
        let v = yaml(
            r#"
kind: Subscription
spec:
  source: webhook
  mode: push
  ingress: { gateway_path: /ingress/hook, verify: { type: bearer, secret: "hook_token" } }
  dispatch: { playbook: domain/handle }
"#,
        );
        assert!(validate_subscription_spec(&v).is_ok());
    }

    #[test]
    fn valid_push_subscription_pubsub_oidc() {
        let v = yaml(
            r#"
kind: Subscription
spec:
  source: pubsub
  mode: push
  ingress:
    gateway_path: /ingress/billing
    verify:
      type: pubsub_oidc
      audience: "https://gw.noetl.acme/ingress/billing"
      service_account: "pubsub-push@acme.iam.gserviceaccount.com"
  dispatch: { playbook: domain/handle_billing }
"#,
        );
        assert!(validate_subscription_spec(&v).is_ok());
    }

    #[test]
    fn push_without_ingress_rejected() {
        let v = yaml(
            "kind: Subscription\nspec:\n  source: webhook\n  mode: push\n  dispatch: { playbook: p }\n",
        );
        let err = validate_subscription_spec(&v).unwrap_err();
        assert!(format!("{err}").contains("ingress"));
    }

    #[test]
    fn push_verify_none_rejected() {
        let v = yaml(
            r#"
kind: Subscription
spec:
  source: webhook
  mode: push
  ingress: { gateway_path: /ingress/x, verify: { type: none } }
  dispatch: { playbook: p }
"#,
        );
        let err = validate_subscription_spec(&v).unwrap_err();
        assert!(format!("{err}").contains("verify.type"));
    }

    #[test]
    fn push_hmac_missing_secret_rejected() {
        let v = yaml(
            r#"
kind: Subscription
spec:
  source: webhook
  mode: push
  ingress: { gateway_path: /ingress/x, verify: { type: hmac_sha256, header: X-Sig } }
  dispatch: { playbook: p }
"#,
        );
        let err = validate_subscription_spec(&v).unwrap_err();
        assert!(format!("{err}").contains("verify.secret"));
    }

    #[test]
    fn push_oidc_missing_audience_rejected() {
        let v = yaml(
            r#"
kind: Subscription
spec:
  source: pubsub
  mode: push
  ingress: { gateway_path: /ingress/x, verify: { type: pubsub_oidc, service_account: sa@x.iam } }
  dispatch: { playbook: p }
"#,
        );
        let err = validate_subscription_spec(&v).unwrap_err();
        assert!(format!("{err}").contains("verify.audience"));
    }

    #[test]
    fn push_relative_gateway_path_rejected() {
        let v = yaml(
            r#"
kind: Subscription
spec:
  source: webhook
  mode: push
  ingress: { gateway_path: ingress/x, verify: { type: bearer, secret: t } }
  dispatch: { playbook: p }
"#,
        );
        let err = validate_subscription_spec(&v).unwrap_err();
        assert!(format!("{err}").contains("absolute path"));
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
