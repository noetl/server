//! The DSL knowledge base over the API (ai-meta RFC `docs/rfc/dsl-knowledge-base.md`,
//! Phase 1).
//!
//! - `GET /api/dsl/version`: the version stamp (grammar `apiVersion`, schema digest,
//!   server + orchestrate-core versions, gate states).
//! - `GET /api/dsl/schema`: the playbook JSON Schema this process accepts, with
//!   `ETag: "<digest>"` and `If-None-Match` → 304.
//! - `GET /api/dsl/tools`: the tool-kind manifest under this process's gates.
//! - `POST /api/dsl/validate` (Phase 2): dry-run registration plus `parse_playbook`,
//!   with rule-tagged findings. No DB access, no event, no insert.
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
    body::Bytes,
    extract::DefaultBodyLimit,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
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
        .route(
            "/api/dsl/validate",
            post(move |headers: HeaderMap, body: Bytes| async move {
                validate_response(kb, &headers, &body)
            }),
        )
        .layer(DefaultBodyLimit::max(VALIDATE_MAX_BYTES))
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

/// Largest document `POST /api/dsl/validate` accepts.
pub const VALIDATE_MAX_BYTES: usize = 1024 * 1024;

/// One problem the validator found, tagged with a stable rule id.
fn finding(rule: &str, message: String, step: Option<String>, path: Option<String>) -> Value {
    let mut f = json!({ "rule": rule, "message": message });
    if let Some(s) = step {
        f["step"] = Value::String(s);
    }
    if let Some(p) = path {
        f["path"] = Value::String(p);
    }
    f
}

/// Turn a request body into the YAML text registration would see.
///
/// - `{"content": "<yaml or base64>"}`: the `POST /api/catalog/register` shape.
/// - any other JSON object: the playbook itself, as JSON.
/// - anything else: the body is the YAML text.
fn validate_input(headers: &HeaderMap, body: &[u8]) -> Result<String, String> {
    let is_json = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("json"));
    let text = std::str::from_utf8(body).map_err(|_| "body is not UTF-8".to_string())?;
    if !is_json {
        return Ok(text.to_string());
    }
    let v: Value = serde_json::from_str(text).map_err(|e| format!("invalid JSON body: {e}"))?;
    if let Some(c) = v.get("content").and_then(Value::as_str) {
        // Mirrors `CatalogService::decode_content`: base64 when it decodes to UTF-8.
        use base64::Engine;
        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(c) {
            if let Ok(s) = String::from_utf8(bytes) {
                return Ok(s);
            }
        }
        return Ok(c.to_string());
    }
    serde_yaml::to_string(&v).map_err(|e| format!("could not convert JSON body: {e}"))
}

/// Validate one document exactly as registration and execute would see it.
///
/// - `register_verdict` comes from the SAME functions `CatalogService::register`
///   calls (`registration_identity` + `registration_checks`), so it cannot
///   disagree with registration.
/// - `findings` lists every problem, each check run independently, so a
///   generator can repair them all in one round instead of one per call.
/// - `execute_verdict` adds `parse_playbook`, which registration does not run
///   today (it runs at execute): a document can register and still be
///   unexecutable, and the validator says so.
pub fn validate_document(kb: &DslKb, content: &str) -> Value {
    use crate::services::catalog as cat;
    let mut findings: Vec<Value> = Vec::new();

    let (register_verdict, kind, path) = match cat::registration_identity(content, "playbook") {
        Err(e) => {
            let rule = if e.to_string().contains("Invalid YAML") {
                "yaml"
            } else {
                "metadata"
            };
            findings.push(finding(rule, e.to_string(), None, None));
            ("reject", None, None)
        }
        Ok((yaml, path, kind)) => {
            let verdict = match cat::registration_checks(&yaml, &kind) {
                Ok(()) => "accept",
                Err(_) => "reject",
            };
            if kind == "subscription" {
                if let Err(e) = cat::validate_subscription_spec(&yaml) {
                    findings.push(finding("subscription_spec", e.to_string(), None, None));
                }
            }
            for (step, k, gate) in cat::tool_kind_offenders(&yaml) {
                let (rule, msg) = match gate {
                    Some(env) => (
                        "gated_tool_kind",
                        format!("tool kind '{k}' is recognised but disabled on this server; {env}=true enables it"),
                    ),
                    None => (
                        "unknown_tool_kind",
                        format!("unknown tool kind '{k}'. Valid kinds: {}", ToolKind::valid_set()),
                    ),
                };
                findings.push(finding(rule, msg, Some(step), None));
            }
            for f in crate::services::secret_material::scan(&yaml).findings {
                findings.push(finding(
                    "inline_secret",
                    format!(
                        "inline secret material ({}); reference a keychain entry instead",
                        f.marker.as_str()
                    ),
                    None,
                    Some(f.path),
                ));
            }
            (verdict, Some(kind), Some(path))
        }
    };

    let mut execute_verdict = register_verdict;
    if let Some(k) = kind.as_deref() {
        if let Some((rule, msg)) = cat::parse_verdict(k, content) {
            findings.push(finding(rule, msg, None, None));
            execute_verdict = "reject";
        }
    }
    json!({
        "valid": findings.is_empty(),
        "register_verdict": register_verdict,
        "execute_verdict": execute_verdict,
        "kind": kind,
        "path": path,
        "findings": findings,
        "schema_digest": kb.schema_digest,
        "api_version": dsl::API_VERSION,
        "note": "Dry run: nothing was registered, stored or executed.",
    })
}

fn validate_response(kb: &DslKb, headers: &HeaderMap, body: &[u8]) -> Response {
    let content = match validate_input(headers, body) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "bad_request", "message": e })),
            )
                .into_response()
        }
    };
    let mut report = validate_document(kb, &content);
    if let Some(client) = headers
        .get("x-noetl-schema-digest")
        .and_then(|v| v.to_str().ok())
    {
        if client != kb.schema_digest {
            report["schema_drift"] = json!({
                "client_digest": client,
                "server_digest": kb.schema_digest,
                "message": "the schema you generated against is not the one this server serves; refetch GET /api/dsl/schema",
            });
        }
    }
    (StatusCode::OK, Json(report)).into_response()
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

    // ---- Phase 2: POST /api/dsl/validate ----

    const GOOD: &str = "apiVersion: noetl.io/v2\nkind: Playbook\nmetadata:\n  name: demo\n  path: demo/ok\nworkflow:\n- step: start\n  tool:\n    kind: python\n    code: \"result = 1\"\n  next: end\n- step: end\n  tool:\n    kind: noop\n";

    fn rules(report: &Value) -> Vec<String> {
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["rule"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn a_valid_playbook_is_valid_everywhere() {
        let r = validate_document(kb(ToolKindGates::OFF), GOOD);
        assert_eq!(r["valid"], true, "{r}");
        assert_eq!(r["register_verdict"], "accept");
        assert_eq!(r["execute_verdict"], "accept");
        assert_eq!(r["kind"], "playbook");
        assert_eq!(r["path"], "demo/ok");
    }

    /// The gap the validator exists to expose: registration accepts it, execute would not.
    #[test]
    fn missing_start_registers_but_cannot_execute() {
        let doc = GOOD.replace("step: start", "step: begin");
        let r = validate_document(kb(ToolKindGates::OFF), &doc);
        assert_eq!(r["register_verdict"], "accept", "{r}");
        assert_eq!(r["execute_verdict"], "reject");
        assert_eq!(rules(&r), vec!["playbook_rule"]);
    }

    #[test]
    fn a_shape_error_is_playbook_parse() {
        let doc = GOOD.replace("  tool:\n    kind: noop\n", "");
        let r = validate_document(kb(ToolKindGates::OFF), &doc);
        assert_eq!(rules(&r), vec!["playbook_parse"], "{r}");
    }

    #[test]
    fn unknown_and_gated_kinds_are_named_per_step_and_reject_registration() {
        let doc = GOOD
            .replace("kind: python", "kind: totally_bogus_kind")
            .replace("kind: noop", "kind: mcp");
        let r = validate_document(kb(ToolKindGates::OFF), &doc);
        assert_eq!(r["register_verdict"], "reject", "{r}");
        let f = r["findings"].as_array().unwrap();
        let unknown = f
            .iter()
            .find(|x| x["rule"] == "unknown_tool_kind")
            .expect("unknown");
        assert_eq!(unknown["step"], "start");
        let gated = f
            .iter()
            .find(|x| x["rule"] == "gated_tool_kind")
            .expect("gated");
        assert_eq!(gated["step"], "end");
        assert!(gated["message"]
            .as_str()
            .unwrap()
            .contains("NOETL_TOOL_KIND_MCP=true"));
    }

    #[test]
    fn inline_secret_material_is_flagged_by_path_never_by_value() {
        let token = format!("ghp_{}", "a".repeat(36));
        let doc = GOOD.replace("code: \"result = 1\"", &format!("code: \"{token}\""));
        let r = validate_document(kb(ToolKindGates::OFF), &doc);
        assert_eq!(r["register_verdict"], "reject", "{r}");
        let f = r["findings"].as_array().unwrap();
        let s = f
            .iter()
            .find(|x| x["rule"] == "inline_secret")
            .expect("secret finding");
        assert!(s["path"].as_str().is_some());
        assert!(
            !r.to_string().contains(&token),
            "the report must never echo the value"
        );
    }

    #[test]
    fn invalid_yaml_and_missing_metadata_have_their_own_rules() {
        let r = validate_document(kb(ToolKindGates::OFF), "a: [unclosed");
        assert_eq!(rules(&r), vec!["yaml"]);
        let r = validate_document(
            kb(ToolKindGates::OFF),
            "apiVersion: noetl.io/v2\nkind: Playbook\nworkflow: []\n",
        );
        assert_eq!(rules(&r), vec!["metadata"]);
        assert_eq!(r["register_verdict"], "reject");
    }

    #[test]
    fn a_broken_subscription_spec_is_reported() {
        let doc = "apiVersion: noetl.io/v2\nkind: Subscription\nmetadata:\n  name: s\nspec:\n  source: carrier_pigeon\n  mode: pull\n";
        let r = validate_document(kb(ToolKindGates::OFF), doc);
        assert_eq!(r["register_verdict"], "reject", "{r}");
        assert!(rules(&r).contains(&"subscription_spec".to_string()));
    }

    #[tokio::test]
    async fn validate_route_accepts_yaml_json_and_register_shaped_bodies() {
        let kb = kb(ToolKindGates::OFF);
        let pb: Value = serde_yaml::from_str(GOOD).unwrap();
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(GOOD);
        let bodies: Vec<(&str, String)> = vec![
            ("application/yaml", GOOD.to_string()),
            ("application/json", pb.to_string()),
            ("application/json", json!({ "content": GOOD }).to_string()),
            ("application/json", json!({ "content": b64 }).to_string()),
        ];
        for (ct, body) in bodies {
            let resp = routes_for(kb)
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/dsl/validate")
                        .header(header::CONTENT_TYPE, ct)
                        .header("x-noetl-schema-digest", "sha256:stale")
                        .body(Body::from(body))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let r: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(r["valid"], true, "{ct}: {r}");
            assert_eq!(r["schema_drift"]["server_digest"], kb.schema_digest);
        }
    }

    #[test]
    fn parse_on_register_is_strictly_shadow_or_off() {
        use crate::services::catalog::ParseOnRegister as P;
        assert_eq!(P::from_value(None), P::Off);
        assert_eq!(P::from_value(Some("shadow")), P::Shadow);
        for v in ["enforce", "SHADOW", "true", "on", ""] {
            assert_eq!(
                P::from_value(Some(v)),
                P::Off,
                "{v:?} must not arm anything"
            );
        }
    }
}
