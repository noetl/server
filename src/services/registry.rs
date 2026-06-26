//! Registry service ([noetl/ai-meta#146](https://github.com/noetl/ai-meta/issues/146),
//! platform foundation **G3**).
//!
//! Wraps [`crate::db::queries::registry`] with the business logic the
//! `/api/internal/registry/*` handlers need: snowflake-id minting, monotonic
//! version assignment (with a retry on the version race), `registry://` URN
//! construction + parsing, and artifact-key derivation against the existing
//! [`ObjectBackend`] (the #104 result-tier substrate).
//!
//! ## URN scheme
//!
//! A registry entry is addressed by a `registry://` URN, resolvable like a
//! result-tier `noetl://` URN:
//!
//! - `registry://<kind>/<name>/<version>` — short form; `tenant` / `project`
//!   default to `default` (overridable per-request).
//! - `registry://<tenant>/<project>/<kind>/<name>/<version>` — fully-qualified.
//!
//! `<version>` is an integer or the literal `latest` (resolves to the highest
//! registered version).
//!
//! ## Artifact bytes
//!
//! The blob bytes never pass through this service. A playbook PUTs the artifact
//! to the object endpoint at the **canonical artifact key**
//! ([`RegistryService::artifact_key`]) and registers the entry pointing at that
//! key; resolve returns the entry (with `artifact_uri`), and the playbook GETs
//! the bytes back through the same object endpoint. GB-scale artifacts stream
//! through `/api/internal/objects/{*key}` and reuse the #104 backend (Postgres
//! BYTEA or GCS) unchanged.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::db::queries::registry as queries;
use crate::db::DbPool;
use crate::error::{AppError, AppResult};
use crate::services::object_backend::ObjectBackend;
use crate::snowflake::SnowflakeGenerator;

/// The registry kinds. Open set in the table (`kind` is TEXT), but the service
/// validates against this list so a typo can't silently create a new namespace.
pub const KINDS: &[&str] = &["model", "dataset", "eval", "release"];

pub const DEFAULT_TENANT: &str = "default";
pub const DEFAULT_PROJECT: &str = "default";

/// `POST /api/internal/registry/register` request body.
#[derive(Debug, Deserialize)]
pub struct RegisterBody {
    #[serde(default)]
    pub tenant: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    /// `model` | `dataset` | `eval` | `release`.
    pub kind: String,
    pub name: String,
    /// Object-store key where the artifact bytes already live (PUT via the
    /// object endpoint). Optional — a metadata-only entry omits it.
    #[serde(default)]
    pub artifact_uri: Option<String>,
    #[serde(default)]
    pub artifact_digest: Option<String>,
    #[serde(default)]
    pub artifact_bytes: Option<i64>,
    #[serde(default)]
    pub media_type: Option<String>,
    /// Free-form metrics / base-model / recipe metadata.
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
    /// Parent registry refs (`registry://…`) this entry derives from.
    #[serde(default)]
    pub lineage: Option<Vec<String>>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,
}

/// The resolved entry returned by register / resolve / list, plus the
/// `registry://` URN that addresses it.
#[derive(Debug, Serialize)]
pub struct RegistryEntry {
    pub r#ref: String,
    pub entry_id: i64,
    pub tenant: String,
    pub project: String,
    pub kind: String,
    pub name: String,
    pub version: i32,
    pub artifact_uri: Option<String>,
    pub artifact_digest: Option<String>,
    pub artifact_bytes: Option<i64>,
    pub media_type: Option<String>,
    pub metadata: serde_json::Value,
    pub lineage: serde_json::Value,
    pub tags: serde_json::Value,
    pub created_at: String,
}

impl RegistryEntry {
    fn from_row(row: queries::RegistryRow) -> Self {
        let r#ref = build_ref(&row.tenant, &row.project, &row.kind, &row.name, row.version);
        RegistryEntry {
            r#ref,
            entry_id: row.entry_id,
            tenant: row.tenant,
            project: row.project,
            kind: row.kind,
            name: row.name,
            version: row.version,
            artifact_uri: row.artifact_uri,
            artifact_digest: row.artifact_digest,
            artifact_bytes: row.artifact_bytes,
            media_type: row.media_type,
            metadata: row.metadata,
            lineage: row.lineage,
            tags: row.tags,
            created_at: row.created_at.to_rfc3339(),
        }
    }
}

/// Fully-qualified `registry://<tenant>/<project>/<kind>/<name>/<version>` URN.
pub fn build_ref(tenant: &str, project: &str, kind: &str, name: &str, version: i32) -> String {
    format!("registry://{tenant}/{project}/{kind}/{name}/{version}")
}

/// A parsed `registry://` reference. `version == None` means `latest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryRef {
    pub tenant: String,
    pub project: String,
    pub kind: String,
    pub name: String,
    pub version: Option<i32>,
}

/// Parse a `registry://` URN, accepting both the short
/// (`registry://<kind>/<name>/<version>`) and fully-qualified
/// (`registry://<tenant>/<project>/<kind>/<name>/<version>`) shapes.
///
/// `default_tenant` / `default_project` fill the short form. `<version>` is an
/// integer or `latest` (→ `None`). Returns a human-readable error so the handler
/// can answer 400 without panicking.
pub fn parse_ref(
    uri: &str,
    default_tenant: &str,
    default_project: &str,
) -> Result<RegistryRef, String> {
    let path = uri
        .strip_prefix("registry://")
        .ok_or_else(|| format!("URI must start with 'registry://', got: {uri:?}"))?;
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let (tenant, project, kind, name, version_str) = match parts.as_slice() {
        [kind, name, version] => (
            default_tenant.to_string(),
            default_project.to_string(),
            kind.to_string(),
            name.to_string(),
            version.to_string(),
        ),
        [tenant, project, kind, name, version] => (
            tenant.to_string(),
            project.to_string(),
            kind.to_string(),
            name.to_string(),
            version.to_string(),
        ),
        _ => {
            return Err(format!(
                "registry URI must be 'registry://<kind>/<name>/<version>' or \
                 'registry://<tenant>/<project>/<kind>/<name>/<version>', got {} segments: {uri:?}",
                parts.len()
            ))
        }
    };
    let version = if version_str.eq_ignore_ascii_case("latest") {
        None
    } else {
        Some(
            version_str
                .parse::<i32>()
                .map_err(|_| format!("version segment {version_str:?} is not an int or 'latest'"))?,
        )
    };
    Ok(RegistryRef {
        tenant,
        project,
        kind,
        name,
        version,
    })
}

/// Registry service — pool + snowflake + the resolved object backend (for
/// artifact stat on register and the canonical artifact-key derivation).
#[derive(Clone)]
pub struct RegistryService {
    pool: DbPool,
    snowflake: Arc<SnowflakeGenerator>,
    backend: ObjectBackend,
}

impl RegistryService {
    pub fn new(pool: DbPool, snowflake: Arc<SnowflakeGenerator>, backend: ObjectBackend) -> Self {
        Self {
            pool,
            snowflake,
            backend,
        }
    }

    pub fn backend_label(&self) -> &'static str {
        self.backend.label()
    }

    /// The canonical object-store key an artifact for this coordinate lives at.
    /// Playbooks PUT to `/api/internal/objects/<this-key>` and register the
    /// entry with `artifact_uri = <this-key>`. Stable + collision-free per
    /// `(tenant, project, kind, name, version, filename)`; shares the `noetl/`
    /// object-store root with the result tier but under a `registry/` segment so
    /// the GC sweep (which keys off `results/`) never touches it.
    pub fn artifact_key(
        tenant: &str,
        project: &str,
        kind: &str,
        name: &str,
        version: i32,
        filename: &str,
    ) -> String {
        format!("noetl/registry/{tenant}/{project}/{kind}/{name}/{version}/{filename}")
    }

    /// Register a new entry, assigning the next monotonic version. Retries once
    /// on a version race (the UNIQUE constraint rejecting two concurrent
    /// registers that computed the same next version).
    pub async fn register(&self, body: &RegisterBody) -> AppResult<RegistryEntry> {
        let tenant = normalize(body.tenant.as_deref(), DEFAULT_TENANT);
        let project = normalize(body.project.as_deref(), DEFAULT_PROJECT);
        let kind = body.kind.trim().to_string();
        let name = body.name.trim().to_string();

        if name.is_empty() {
            return Err(AppError::BadRequest("registry: name is required".into()));
        }
        if !KINDS.contains(&kind.as_str()) {
            return Err(AppError::BadRequest(format!(
                "registry: kind must be one of {KINDS:?}, got {kind:?}"
            )));
        }

        let metadata = body
            .metadata
            .clone()
            .unwrap_or_else(|| serde_json::json!({}));
        let lineage = serde_json::json!(body.lineage.clone().unwrap_or_default());
        let tags = serde_json::json!(body.tags.clone().unwrap_or_default());

        // A couple of attempts is enough to clear a same-millisecond version
        // race; persistent failure surfaces the underlying error.
        let mut last_err: Option<AppError> = None;
        for _ in 0..3 {
            let entry_id = self
                .snowflake
                .generate()
                .map_err(|e| AppError::Internal(format!("registry: snowflake: {e}")))?;
            let res = queries::insert_next_version(
                &self.pool,
                entry_id,
                &tenant,
                &project,
                &kind,
                &name,
                body.artifact_uri.as_deref(),
                body.artifact_digest.as_deref(),
                body.artifact_bytes,
                body.media_type.as_deref(),
                &metadata,
                &lineage,
                &tags,
            )
            .await;
            match res {
                Ok((version, created_at)) => {
                    return Ok(RegistryEntry {
                        r#ref: build_ref(&tenant, &project, &kind, &name, version),
                        entry_id,
                        tenant,
                        project,
                        kind,
                        name,
                        version,
                        artifact_uri: body.artifact_uri.clone(),
                        artifact_digest: body.artifact_digest.clone(),
                        artifact_bytes: body.artifact_bytes,
                        media_type: body.media_type.clone(),
                        metadata,
                        lineage,
                        tags,
                        created_at: created_at.to_rfc3339(),
                    });
                }
                Err(e) if queries::is_version_conflict(&e) => {
                    last_err = Some(e);
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            AppError::Internal("registry: version assignment failed after retries".into())
        }))
    }

    /// Resolve a parsed `registry://` ref to its entry (specific version, or the
    /// latest when `ref.version` is `None`). `None` → 404.
    pub async fn resolve(&self, r: &RegistryRef) -> AppResult<Option<RegistryEntry>> {
        let row = match r.version {
            Some(v) => {
                queries::get_version(&self.pool, &r.tenant, &r.project, &r.kind, &r.name, v).await?
            }
            None => {
                queries::get_latest(&self.pool, &r.tenant, &r.project, &r.kind, &r.name).await?
            }
        };
        Ok(row.map(RegistryEntry::from_row))
    }

    /// List entries under a tenant/project, optionally filtered.
    pub async fn list(
        &self,
        tenant: &str,
        project: &str,
        kind: Option<&str>,
        name: Option<&str>,
        limit: i64,
    ) -> AppResult<Vec<RegistryEntry>> {
        let rows = queries::list(&self.pool, tenant, project, kind, name, limit).await?;
        Ok(rows.into_iter().map(RegistryEntry::from_row).collect())
    }
}

fn normalize(v: Option<&str>, default: &str) -> String {
    match v.map(str::trim) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => default.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_short_form_uses_defaults() {
        let r = parse_ref("registry://model/intent_extractor/3", "default", "default").unwrap();
        assert_eq!(r.tenant, "default");
        assert_eq!(r.project, "default");
        assert_eq!(r.kind, "model");
        assert_eq!(r.name, "intent_extractor");
        assert_eq!(r.version, Some(3));
    }

    #[test]
    fn parse_qualified_form() {
        let r = parse_ref(
            "registry://acme/support/dataset/triage/12",
            "default",
            "default",
        )
        .unwrap();
        assert_eq!(r.tenant, "acme");
        assert_eq!(r.project, "support");
        assert_eq!(r.kind, "dataset");
        assert_eq!(r.name, "triage");
        assert_eq!(r.version, Some(12));
    }

    #[test]
    fn parse_latest_is_none() {
        let r = parse_ref("registry://eval/golden/latest", "t", "p").unwrap();
        assert_eq!(r.kind, "eval");
        assert_eq!(r.version, None);
        // case-insensitive
        let r2 = parse_ref("registry://eval/golden/LATEST", "t", "p").unwrap();
        assert_eq!(r2.version, None);
    }

    #[test]
    fn parse_rejects_bad_scheme_and_arity() {
        assert!(parse_ref("noetl://model/x/1", "t", "p").is_err());
        assert!(parse_ref("registry://x/1", "t", "p").is_err());
        assert!(parse_ref("registry://a/b/c/d", "t", "p").is_err());
        assert!(parse_ref("registry://model/x/notanint", "t", "p").is_err());
    }

    #[test]
    fn build_ref_round_trips_through_parser() {
        let uri = build_ref("acme", "support", "model", "router", 7);
        assert_eq!(uri, "registry://acme/support/model/router/7");
        let r = parse_ref(&uri, "default", "default").unwrap();
        assert_eq!(r.tenant, "acme");
        assert_eq!(r.project, "support");
        assert_eq!(r.kind, "model");
        assert_eq!(r.name, "router");
        assert_eq!(r.version, Some(7));
    }

    #[test]
    fn artifact_key_is_under_registry_root() {
        let k =
            RegistryService::artifact_key("default", "default", "model", "router", 2, "adapter.safetensors");
        assert_eq!(
            k,
            "noetl/registry/default/default/model/router/2/adapter.safetensors"
        );
        // Distinct from the result tier's `results/` keys, so the #104 GC sweep
        // (which keys off `…/results/…`) never reclaims a registry artifact.
        assert!(k.contains("/registry/"));
        assert!(!k.contains("/results/"));
    }
}
