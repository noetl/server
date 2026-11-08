//! The DSL knowledge base over the API (ai-meta RFC `docs/rfc/dsl-knowledge-base.md`,
//! Phase 1).
//!
//! - `GET /api/dsl/version`: the version stamp (grammar `apiVersion`, schema digest,
//!   server + orchestrate-core versions, gate states).
//! - `GET /api/dsl/schema`: the playbook JSON Schema this process accepts, with
//!   `ETag: "<digest>"` and `If-None-Match` → 304.
//! - `GET /api/dsl/tools`: the tool-kind manifest under this process's gates.
//!
//! Everything is a **projection of code that is already linked in**: the schema
//! is derived from `noetl_orchestrate_core::playbook` (the same types
//! `parse_playbook` deserialises) and narrowed to [`ToolKindGates::current`] (the
//! same gates `ToolSpec.kind` applies). So it answers "what does THIS server
//! accept", not "what did the docs say".
//!
//! # Off by default
//!
//! Mounted only when `NOETL_DSL_KB=true` (strict, like the tool-kind gates). With
//! it unset the routes do not exist and nothing else changes.
//!
//! # Exposure
//!
//! Read-only, stateless (no DB, no events), and carries no tenant data or
//! credential material: it reflects compiled grammar. It is therefore in the
//! same class as `/api/health` and is listed in `tests/auth_gate_wiring.rs`'s
//! `PUBLIC_BY_DESIGN` with that reason.
//!
//! # What it does not claim
//!
//! `executable` is `"not_reported"`: this process does not know which kinds a
//! worker has registered until workers report capabilities (RFC Phase 3).

use std::sync::OnceLock;

use axum::{
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use noetl_orchestrate_core::dsl;
use noetl_orchestrate_core::playbook::{ToolKind, ToolKindGates};
use serde_json::{json, Value};

/// The arming flag. Strict `== "true"`, read per call to [`dsl_kb_enabled`]
/// (which `main` calls once while building the router).
pub const DSL_KB_ENV: &str = "NOETL_DSL_KB";

/// Is the knowledge base mounted on this server?
pub fn dsl_kb_enabled() -> bool {
    dsl_kb_enabled_from(|k| std::env::var(k).ok())
}

/// [`dsl_kb_enabled`] from a lookup, so tests never touch the process env.
pub fn dsl_kb_enabled_from(get: impl Fn(&str) -> Option<String>) -> bool {
    get(DSL_KB_ENV).as_deref() == Some("true")
}

/// Everything the endpoints serve, computed once per process.
///
/// The gates are read once per process too ([`ToolKindGates::current`]), so the
/// schema and its digest are fixed for the life of the process; they change
/// only on a rollout or a flag flip, both of which restart it.
#[derive(Debug, Clone)]
pub struct DslKb {
    pub schema: Value,
    pub schema_digest: String,
    pub manifest: Value,
    pub version: Value,
}

impl DslKb {
    /// Build the knowledge base for `gates`, stamped with `server_version`.
    pub fn build(gates: ToolKindGates, server_version: &str) -> Self {
        let schema = dsl::playbook_schema(gates);
        let schema_digest = dsl::digest(&schema);
        let gate_states: serde_json::Map<String, Value> = ToolKind::ALL
            .iter()
            .filter_map(|k| {
                k.gate_env()
                    .map(|env| (env.to_string(), Value::Bool(gates.allows(k))))
            })
            .collect();
        let version = json!({
            "api_version": dsl::API_VERSION,
            "document_kind": dsl::PLAYBOOK_KIND,
            "schema_digest": schema_digest,
            "schema_id": dsl::SCHEMA_ID,
            "server_version": server_version,
            "orchestrate_core_version": dsl::CORE_VERSION,
            "gates": gate_states,
            "executable": "not_reported",
        });
        let mut manifest = dsl::manifest(gates);
        if let Some(m) = manifest.as_object_mut() {
            m.insert("schema_digest".into(), Value::String(schema_digest.clone()));
            m.insert(
                "server_version".into(),
                Value::String(server_version.to_string()),
            );
        }
        DslKb {
            schema,
            schema_digest,
            manifest,
            version,
        }
    }

    /// This process's knowledge base.
    pub fn current() -> &'static DslKb {
        static KB: OnceLock<DslKb> = OnceLock::new();
        KB.get_or_init(|| DslKb::build(ToolKindGates::current(), env!("CARGO_PKG_VERSION")))
    }

    fn etag(&self) -> HeaderValue {
        HeaderValue::from_str(&format!("\"{}\"", self.schema_digest))
            .expect("a sha256 digest is a valid header value")
    }
}

/// The `/api/dsl/*` routes over `kb`.
pub fn routes_for(kb: &'static DslKb) -> Router {
    Router::new()
        .route(
            "/api/dsl/version",
            get(move || async move { Json(kb.version.clone()) }),
        )
        .route(
            "/api/dsl/schema",
            get(move |headers: HeaderMap| async move { schema_response(kb, &headers) }),
        )
        .route(
            "/api/dsl/tools",
            get(move || async move { Json(kb.manifest.clone()) }),
        )
}

/// The `/api/dsl/*` routes over this process's knowledge base.
pub fn routes() -> Router {
    routes_for(DslKb::current())
}

fn schema_response(kb: &DslKb, headers: &HeaderMap) -> Response {
    let etag = kb.etag();
    let not_modified = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .any(|t| t.trim() == etag.to_str().unwrap_or(""))
        });
    if not_modified {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response();
    }
    (
        [
            (header::ETAG, etag),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        Json(kb.schema.clone()),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn kb(gates: ToolKindGates) -> &'static DslKb {
        Box::leak(Box::new(DslKb::build(gates, "0.0.0-test")))
    }

    async fn get_json(app: Router, uri: &str) -> (StatusCode, HeaderMap, Value) {
        let resp = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, headers, v)
    }

    #[test]
    fn flag_is_strict_and_off_by_default() {
        assert!(!dsl_kb_enabled_from(|_| None));
        for v in ["1", "TRUE", "yes", "on", " true", ""] {
            assert!(!dsl_kb_enabled_from(|_| Some(v.to_string())), "{v:?}");
        }
        assert!(dsl_kb_enabled_from(
            |k| (k == DSL_KB_ENV).then(|| "true".into())
        ));
    }

    #[tokio::test]
    async fn version_is_stamped_with_the_schema_digest() {
        let kb = kb(ToolKindGates::OFF);
        let (status, _, v) = get_json(routes_for(kb), "/api/dsl/version").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["api_version"], "noetl.io/v2");
        assert_eq!(v["schema_digest"], kb.schema_digest);
        assert_eq!(v["server_version"], "0.0.0-test");
        assert_eq!(v["orchestrate_core_version"], dsl::CORE_VERSION);
        assert_eq!(v["gates"]["NOETL_TOOL_KIND_MCP"], false);
        assert_eq!(v["gates"]["NOETL_TOOL_KIND_AGENT"], false);
        assert_eq!(v["executable"], "not_reported");
    }

    #[tokio::test]
    async fn schema_carries_an_etag_and_honours_if_none_match() {
        let kb = kb(ToolKindGates::OFF);
        let (status, headers, schema) = get_json(routes_for(kb), "/api/dsl/schema").await;
        assert_eq!(status, StatusCode::OK);
        let etag = headers
            .get(header::ETAG)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(etag, format!("\"{}\"", kb.schema_digest));
        assert_eq!(
            dsl::digest(&schema),
            kb.schema_digest,
            "digest is of the body served"
        );

        let resp = routes_for(kb)
            .oneshot(
                Request::builder()
                    .uri("/api/dsl/schema")
                    .header(header::IF_NONE_MATCH, &etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    }

    /// The served vocabulary is the registration validator's vocabulary.
    #[tokio::test]
    async fn tools_agree_with_the_registration_oracle_under_each_gate() {
        for gates in [ToolKindGates::OFF, ToolKindGates::ALL_ON] {
            let kb = kb(gates);
            let (_, _, m) = get_json(routes_for(kb), "/api/dsl/tools").await;
            let enabled: Vec<String> = m["enabled_tool_kinds"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect();
            let mut want: Vec<String> = ToolKind::valid_set_with(gates)
                .split(", ")
                .map(str::to_string)
                .collect();
            want.sort();
            assert_eq!(enabled, want, "under {gates:?}");
            assert_eq!(m["schema_digest"], kb.schema_digest);
        }
    }

    #[test]
    fn opening_a_gate_changes_the_digest() {
        assert_ne!(
            DslKb::build(ToolKindGates::OFF, "x").schema_digest,
            DslKb::build(ToolKindGates::ALL_ON, "x").schema_digest
        );
    }

    /// The default-gates schema this server would serve is the committed artifact,
    /// so docs and release assets built from the artifact describe this binary.
    #[test]
    fn served_default_schema_is_the_committed_artifact() {
        let committed: Value = serde_json::from_str(include_str!(
            "../../orchestrate-core/schema/playbook.schema.json"
        ))
        .unwrap();
        assert_eq!(
            dsl::digest(&committed),
            DslKb::build(ToolKindGates::OFF, "x").schema_digest
        );
    }
}
