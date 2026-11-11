//! Playbooks served over MCP: interop RFC Phase 5 (noetl/ai-meta#539,
//! `docs/rfc/interop-topology-and-ai-mcp-playbooks.md` §3.3).
//!
//! - `POST /api/mcp`: one MCP server whose tools are every playbook that opts in
//!   with `metadata.expose.mcp`.
//! - `POST /api/mcp/{*path}`: the MCP server described by a catalog `kind: Mcp`
//!   resource at `path` with `spec.serve: true` and a `spec.tools` list.
//!
//! The transport is MCP Streamable HTTP without a server-initiated stream: every
//! request is one JSON-RPC 2.0 message POSTed as JSON, answered with one JSON
//! body; notifications are answered `202 Accepted` with no body; `GET` is `405`.
//! The server is stateless and issues no `Mcp-Session-Id`.
//!
//! Methods: `initialize`, `ping`, `tools/list`, `tools/call`. Besides the exposed
//! playbooks every server lists three helpers, `noetl.execution.status`,
//! `noetl.execution.result` and `noetl.execution.cancel`, for `mode: async` tools.
//!
//! # Opt-in, hand-written schema
//!
//! A playbook is never exposed by default. It opts in with:
//!
//! ```yaml
//! metadata:
//!   path: qnetmon/tools/findings_in_window
//!   expose:
//!     mcp:
//!       tool: qnetmon_findings          # MCP tool name, [A-Za-z0-9_.-]{1,64}
//!       description: Current findings.  # required
//!       input_schema:                   # required, written by hand
//!         type: object
//!         properties: { minutes: { type: integer } }
//!       mode: sync                      # sync (default) | async
//!       timeout: 30                     # sync wait, seconds (default 30, max 120)
//!       result_step: dispatch           # optional: the step whose result is returned
//! ```
//!
//! `input_schema` is **authored, never derived from `workload`** (RFC open question
//! 5, decided for this slice as the safer default). It is also the allowlist for
//! `tools/call` arguments: a key not under `properties` is rejected unless the
//! schema says `additionalProperties: true`, so an MCP caller cannot override a
//! `workload` key the author did not expose.
//!
//! # Off by default
//!
//! Mounted only when `NOETL_MCP_SERVER=true` (strict). Unset, the routes do not
//! exist and nothing else changes.
//!
//! # Exposure
//!
//! The same capability as `POST /api/execute` plus `GET /api/executions/{id}`,
//! narrowed to playbooks that opted in, so it sits in the same class as
//! `execution_routes` (`tests/auth_gate_wiring.rs` `PUBLIC_BY_DESIGN`). Per-user
//! filtering (`tools/list` limited to what the caller `may_execute`) belongs to
//! the gateway, which already owns that policy. Results are passed through
//! [`sanitize_sensitive_data`] before they leave the server.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use async_trait::async_trait;
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::{json, Map, Value};

use crate::sanitize::sanitize_sensitive_data;

/// The arming flag. Strict `== "true"`.
pub const MCP_SERVER_ENV: &str = noetl_orchestrate_core::dsl::EXPOSE_MCP_ENV;

/// Is the playbook MCP server mounted on this process?
pub fn mcp_server_enabled() -> bool {
    mcp_server_enabled_from(|k| std::env::var(k).ok())
}

/// [`mcp_server_enabled`] from a lookup, so tests never touch the process env.
pub fn mcp_server_enabled_from(get: impl Fn(&str) -> Option<String>) -> bool {
    get(MCP_SERVER_ENV).as_deref() == Some("true")
}

/// Protocol revisions this server speaks, newest first. `initialize` echoes the
/// client's revision when it is one of these and answers with the newest
/// otherwise, as the MCP lifecycle spec requires.
pub const SUPPORTED_PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// Tool names under this prefix are the built-in helpers. A playbook may not
/// claim one.
pub const HELPER_PREFIX: &str = "noetl.";
pub const HELPER_STATUS: &str = "noetl.execution.status";
pub const HELPER_RESULT: &str = "noetl.execution.result";
pub const HELPER_CANCEL: &str = "noetl.execution.cancel";

pub const DEFAULT_SYNC_TIMEOUT_SECS: u64 = 30;
/// A sync call holds an HTTP request open while it polls; past this, use
/// `mode: async`.
pub const MAX_SYNC_TIMEOUT_SECS: u64 = 120;
/// Request bodies are single JSON-RPC messages; 1 MiB is generous.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

const SERVER_NAME: &str = "noetl-playbooks";

// JSON-RPC 2.0 error codes.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

/// `sync` waits for the execution to finish (up to `timeout`); `async` returns
/// the execution id at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Sync,
    Async,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Sync => "sync",
            Mode::Async => "async",
        }
    }
}

/// One playbook served as one MCP tool.
#[derive(Debug, Clone, PartialEq)]
pub struct ExposedTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// Catalog path of the playbook a call executes.
    pub playbook: String,
    pub mode: Mode,
    pub timeout_secs: u64,
    pub result_step: Option<String>,
}

/// A declaration that was not served, and why. Reported in the `tools/list`
/// result's `_meta` so an author can see why their tool is missing, instead of
/// the tool silently not appearing.
#[derive(Debug, Clone, PartialEq)]
pub struct Problem {
    pub source: String,
    pub reason: String,
}

/// The tools one MCP server serves.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Registry {
    pub tools: Vec<ExposedTool>,
    pub problems: Vec<Problem>,
}

impl Registry {
    /// Build from declarations, rejecting duplicate names. Every holder of a
    /// duplicated name is excluded: "first one wins" would hide the second
    /// declaration, which is the shadowing failure this codebase has paid for.
    pub fn build(declared: Vec<Result<ExposedTool, Problem>>) -> Registry {
        let mut problems = Vec::new();
        let mut tools = Vec::new();
        for d in declared {
            match d {
                Ok(t) => tools.push(t),
                Err(p) => problems.push(p),
            }
        }
        let mut by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for t in &tools {
            by_name.entry(t.name.clone()).or_default().push(t.playbook.clone());
        }
        let duplicated: HashMap<&String, &Vec<String>> =
            by_name.iter().filter(|(_, v)| v.len() > 1).collect();
        let mut kept = Vec::new();
        for t in tools {
            if let Some(holders) = duplicated.get(&t.name) {
                problems.push(Problem {
                    source: t.playbook.clone(),
                    reason: format!(
                        "tool name `{}` is declared by more than one playbook ({}); none of them is served",
                        t.name,
                        holders.join(", ")
                    ),
                });
            } else {
                kept.push(t);
            }
        }
        kept.sort_by(|a, b| a.name.cmp(&b.name));
        Registry {
            tools: kept,
            problems,
        }
    }

    fn find(&self, name: &str) -> Option<&ExposedTool> {
        self.tools.iter().find(|t| t.name == name)
    }
}

fn valid_tool_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

pub(crate) fn problem(source: &str, reason: impl Into<String>) -> Problem {
    Problem {
        source: source.to_string(),
        reason: reason.into(),
    }
}

/// Parse one tool declaration: a playbook's `metadata.expose.mcp` block, or one
/// entry of a `kind: Mcp` resource's `spec.tools` (which names its `playbook`).
///
/// `fallback` is the referenced playbook's own declaration, used by `kind: Mcp`
/// entries for `description` / `input_schema` they do not state themselves.
pub fn parse_declaration(
    spec: &Value,
    playbook: &str,
    name_key: &str,
    fallback: Option<&ExposedTool>,
) -> Result<ExposedTool, Problem> {
    let obj = spec
        .as_object()
        .ok_or_else(|| problem(playbook, "the MCP declaration must be a mapping"))?;

    let name = obj
        .get(name_key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| fallback.map(|f| f.name.clone()))
        .ok_or_else(|| problem(playbook, format!("`{name_key}` (the MCP tool name) is required")))?;
    if !valid_tool_name(&name) {
        return Err(problem(
            playbook,
            format!("tool name `{name}` must be 1-64 characters of [A-Za-z0-9_.-]"),
        ));
    }
    if name.starts_with(HELPER_PREFIX) {
        return Err(problem(
            playbook,
            format!("tool name `{name}`: the `{HELPER_PREFIX}` prefix is reserved for the built-in helpers"),
        ));
    }

    let description = obj
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| fallback.map(|f| f.description.clone()))
        .ok_or_else(|| problem(playbook, format!("tool `{name}`: `description` is required")))?;

    // Authored only: never derived from `workload`.
    let input_schema = match obj.get("input_schema") {
        Some(s) => s.clone(),
        None => fallback
            .map(|f| f.input_schema.clone())
            .ok_or_else(|| {
                problem(
                    playbook,
                    format!("tool `{name}`: `input_schema` is required and is written by hand (it is not derived from `workload`)"),
                )
            })?,
    };
    if input_schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err(problem(
            playbook,
            format!("tool `{name}`: `input_schema.type` must be `object`"),
        ));
    }
    if let Some(p) = input_schema.get("properties") {
        if !p.is_object() {
            return Err(problem(
                playbook,
                format!("tool `{name}`: `input_schema.properties` must be a mapping"),
            ));
        }
    }

    let mode = match obj.get("mode").and_then(Value::as_str) {
        None => fallback.map(|f| f.mode).unwrap_or(Mode::Sync),
        Some("sync") => Mode::Sync,
        Some("async") => Mode::Async,
        Some(other) => {
            return Err(problem(
                playbook,
                format!("tool `{name}`: mode `{other}` is not `sync` or `async`"),
            ))
        }
    };

    let timeout_secs = match obj.get("timeout") {
        None => fallback
            .map(|f| f.timeout_secs)
            .unwrap_or(DEFAULT_SYNC_TIMEOUT_SECS),
        Some(v) => match v.as_u64() {
            Some(t) if (1..=MAX_SYNC_TIMEOUT_SECS).contains(&t) => t,
            _ => {
                return Err(problem(
                    playbook,
                    format!("tool `{name}`: `timeout` must be an integer from 1 to {MAX_SYNC_TIMEOUT_SECS} seconds"),
                ))
            }
        },
    };

    let result_step = obj
        .get("result_step")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| fallback.and_then(|f| f.result_step.clone()));

    Ok(ExposedTool {
        name,
        description,
        input_schema,
        playbook: playbook.to_string(),
        mode,
        timeout_secs,
        result_step,
    })
}

pub(crate) fn yaml_to_json(content: &str) -> Result<Value, String> {
    let yaml: serde_yaml::Value = serde_yaml::from_str(content).map_err(|e| e.to_string())?;
    serde_json::to_value(yaml).map_err(|e| e.to_string())
}

/// A playbook's own exposure. `None` when it does not opt in.
pub fn exposure_from_playbook(path: &str, content: &str) -> Option<Result<ExposedTool, Problem>> {
    let doc = match yaml_to_json(content) {
        Ok(d) => d,
        // The SQL prefilter matched the word `expose`; a body that does not parse
        // cannot be exposed, and a playbook that does not parse is not ours to
        // report on here.
        Err(_) => return None,
    };
    let spec = doc.pointer("/metadata/expose/mcp")?;
    Some(parse_declaration(spec, path, "tool", None))
}

/// The tools of a `kind: Mcp` resource with `spec.serve: true`.
///
/// `Err` when the resource does not serve (it describes an MCP server NoETL
/// *calls*, which is all `kind: Mcp` meant before this phase).
pub fn tools_from_mcp_resource(
    path: &str,
    content: &str,
    exposed: &HashMap<String, ExposedTool>,
) -> Result<Vec<Result<ExposedTool, Problem>>, String> {
    let doc = yaml_to_json(content).map_err(|e| format!("`{path}` does not parse: {e}"))?;
    if doc.pointer("/spec/serve").and_then(Value::as_bool) != Some(true) {
        return Err(format!(
            "`{path}` is not served: set `spec.serve: true` to build an MCP server from playbooks"
        ));
    }
    let entries = doc
        .pointer("/spec/tools")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("`{path}`: `spec.tools` must be a list"))?;
    Ok(entries
        .iter()
        .map(|e| {
            let playbook = e
                .get("playbook")
                .and_then(Value::as_str)
                .ok_or_else(|| problem(path, "every `spec.tools` entry needs a `playbook` path"))?;
            parse_declaration(e, playbook, "name", exposed.get(playbook))
        })
        .collect())
}

/// Check `tools/call` arguments against the authored schema and return the
/// workload to execute with.
///
/// Deliberately small: object shape, `required`, `properties` as an allowlist,
/// and the JSON type of each declared property when it is a single `type`
/// string. It does not implement the rest of JSON Schema (formats, nested
/// constraints, `oneOf`); the playbook still owns validating its own input.
pub fn check_arguments(schema: &Value, args: Option<&Value>) -> Result<Map<String, Value>, String> {
    let args = match args {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(m)) => m.clone(),
        Some(_) => return Err("`arguments` must be an object".into()),
    };
    let props = schema
        .get("properties")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let open = schema.get("additionalProperties").and_then(Value::as_bool) == Some(true);

    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        let missing: Vec<&str> = required
            .iter()
            .filter_map(Value::as_str)
            .filter(|k| !args.contains_key(*k))
            .collect();
        if !missing.is_empty() {
            return Err(format!("missing required argument(s): {}", missing.join(", ")));
        }
    }

    for (key, value) in &args {
        match props.get(key) {
            None if open => {}
            None => {
                return Err(format!(
                    "argument `{key}` is not declared in the tool's input_schema"
                ))
            }
            Some(prop) => {
                if let Some(ty) = prop.get("type").and_then(Value::as_str) {
                    if !json_type_matches(ty, value) {
                        return Err(format!("argument `{key}` must be of type `{ty}`"));
                    }
                }
            }
        }
    }
    Ok(args)
}

fn json_type_matches(ty: &str, v: &Value) -> bool {
    match ty {
        "string" => v.is_string(),
        "integer" => v.is_i64() || v.is_u64(),
        "number" => v.is_number(),
        "boolean" => v.is_boolean(),
        "object" => v.is_object(),
        "array" => v.is_array(),
        "null" => v.is_null(),
        // An unknown type keyword is the author's problem, not the caller's.
        _ => true,
    }
}

/// The value a `call.done` event carries as the step's output. Same unwrapping
/// as noetl-tools `PlaybookTool::extract_child_payload` (`return_result`), so a
/// tool called over MCP returns what a parent `kind: playbook` step would read.
pub fn extract_step_payload(event_result: &Value) -> Value {
    let mut current = event_result;
    for _ in 0..5 {
        let Value::Object(map) = current else { break };
        if let Some(inner) = map.get("context").filter(|v| v.is_object()) {
            current = inner;
            continue;
        }
        if let Some(inner) = map.get("result").filter(|v| v.is_object()) {
            current = inner;
            continue;
        }
        break;
    }
    if let Value::Object(map) = current {
        if let Some(data) = map.get("data") {
            return data.clone();
        }
    }
    current.clone()
}

/// The execution's result from an `/api/executions/{id}` detail payload: the
/// latest `call.done` of `result_step` when named, else the latest non-terminal
/// `call.done` with a payload. Same selection as noetl-tools `return_result`.
pub fn select_result(detail: &Value, result_step: Option<&str>) -> Option<Value> {
    let events = detail.get("events").and_then(Value::as_array)?;
    let is_call_done = |ev: &Value| ev.get("event_type").and_then(Value::as_str) == Some("call.done");
    let node = |ev: &Value| ev.get("node_name").and_then(Value::as_str).map(str::to_string);
    let payload = |ev: &Value| {
        ev.get("result")
            .filter(|r| !r.is_null())
            .map(extract_step_payload)
            .filter(|p| !p.is_null())
    };
    if let Some(step) = result_step {
        return events
            .iter()
            .rev()
            .filter(|ev| is_call_done(ev) && node(ev).as_deref() == Some(step))
            .find_map(payload);
    }
    events
        .iter()
        .rev()
        .filter(|ev| is_call_done(ev) && node(ev).as_deref() != Some("playbook"))
        .find_map(payload)
        .or_else(|| events.iter().rev().filter(|ev| is_call_done(ev)).find_map(payload))
}

pub(crate) fn is_terminal(status: &str) -> bool {
    matches!(status, "COMPLETED" | "FAILED" | "CANCELLED")
}

/// What the MCP endpoint needs from the rest of the server. The real
/// implementation is [`ServerBackend`]; tests use a scripted one.
#[async_trait]
pub trait McpBackend: Send + Sync {
    /// Tools of the server at `server` (`None` = the all-exposed-playbooks
    /// server). `Ok(None)` = no such MCP server.
    async fn registry(&self, server: Option<&str>) -> Result<Option<Registry>, String>;
    /// Start `playbook` with `workload`; returns the execution id.
    async fn start(&self, playbook: &str, workload: Map<String, Value>) -> Result<i64, String>;
    /// `GET /api/executions/{id}/status` as JSON.
    async fn status(&self, execution_id: i64) -> Result<Value, String>;
    /// `GET /api/executions/{id}` as JSON.
    async fn detail(&self, execution_id: i64) -> Result<Value, String>;
    async fn cancel(&self, execution_id: i64) -> Result<(), String>;
}

/// How often a sync call polls the execution status.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub poll: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            poll: Duration::from_millis(250),
        }
    }
}

fn rpc_result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: &Value, code: i64, message: impl Into<String>) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message.into()}})
}

/// A tool result carrying JSON: text for every client, `structuredContent` for
/// clients that read it (only when the value is an object, as the spec requires).
fn tool_result(value: &Value, is_error: bool, meta: Value) -> Value {
    let text = match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let mut out = json!({
        "content": [{"type": "text", "text": text}],
        "isError": is_error,
        "_meta": meta,
    });
    if value.is_object() {
        out["structuredContent"] = value.clone();
    }
    out
}

fn tool_error(message: impl Into<String>, meta: Value) -> Value {
    tool_result(&Value::String(message.into()), true, meta)
}

fn helper_tools() -> Vec<Value> {
    let id_schema = |extra: Value| {
        let mut props = json!({"execution_id": {"type": "string", "description": "The execution id a mode: async tool returned."}});
        if let (Some(p), Some(e)) = (props.as_object_mut(), extra.as_object()) {
            p.extend(e.clone());
        }
        json!({"type": "object", "properties": props, "required": ["execution_id"]})
    };
    vec![
        json!({"name": HELPER_STATUS, "description": "Status of a NoETL execution started by a tool call.",
               "inputSchema": id_schema(json!({}))}),
        json!({"name": HELPER_RESULT, "description": "Result of a finished NoETL execution started by a tool call.",
               "inputSchema": id_schema(json!({"result_step": {"type": "string", "description": "Return this step's result instead of the last one."}}))}),
        json!({"name": HELPER_CANCEL, "description": "Cancel a NoETL execution started by a tool call.",
               "inputSchema": id_schema(json!({}))}),
    ]
}

fn list_result(registry: &Registry) -> Value {
    let mut tools: Vec<Value> = registry
        .tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.input_schema,
                "_meta": {"noetl/playbook": t.playbook, "noetl/mode": t.mode.as_str()},
            })
        })
        .collect();
    tools.extend(helper_tools());
    let mut out = json!({"tools": tools});
    if !registry.problems.is_empty() {
        out["_meta"] = json!({
            "noetl/not_served": registry.problems.iter()
                .map(|p| json!({"source": p.source, "reason": p.reason}))
                .collect::<Vec<_>>()
        });
    }
    out
}

/// Parse an execution id given as a string (ids exceed 2^53, so they travel as
/// strings, as `/api/execute` returns them) or as an integer.
fn execution_id_arg(args: &Value) -> Result<i64, String> {
    match args.get("execution_id") {
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        Some(Value::Number(n)) => n.as_i64(),
        _ => None,
    }
    .filter(|id| *id > 0)
    .ok_or_else(|| "`execution_id` is required (a positive integer, as a string)".to_string())
}

async fn finished_result(
    backend: &dyn McpBackend,
    execution_id: i64,
    status: &str,
    result_step: Option<&str>,
) -> Value {
    let meta = json!({"noetl/execution_id": execution_id.to_string(), "noetl/status": status});
    let detail = match backend.detail(execution_id).await {
        Ok(d) => d,
        Err(e) => return tool_error(format!("execution {execution_id}: {e}"), meta),
    };
    if status == "COMPLETED" {
        let value = select_result(&detail, result_step).unwrap_or(Value::Null);
        tool_result(&sanitize_sensitive_data(&value), false, meta)
    } else {
        // The first error an event carries is the most useful one to show.
        let reason = detail
            .get("events")
            .and_then(Value::as_array)
            .and_then(|evs| {
                evs.iter()
                    .find_map(|e| e.get("error").and_then(Value::as_str).map(str::to_string))
            })
            .unwrap_or_else(|| format!("execution ended {status}"));
        tool_error(
            sanitize_sensitive_data(&Value::String(reason))
                .as_str()
                .unwrap_or_default()
                .to_string(),
            meta,
        )
    }
}

async fn call_exposed(
    backend: &dyn McpBackend,
    tool: &ExposedTool,
    args: Option<&Value>,
    timing: Timing,
) -> Result<Value, (i64, String)> {
    let workload =
        check_arguments(&tool.input_schema, args).map_err(|e| (INVALID_PARAMS, e))?;
    let execution_id = match backend.start(&tool.playbook, workload).await {
        Ok(id) => id,
        // Failing to start is a tool error the model can see and react to, not
        // a protocol error.
        Err(e) => {
            return Ok(tool_error(
                format!("could not start `{}`: {e}", tool.playbook),
                json!({"noetl/playbook": tool.playbook}),
            ))
        }
    };
    let id_str = execution_id.to_string();
    if tool.mode == Mode::Async {
        let started = json!({"execution_id": id_str, "status": "started"});
        return Ok(tool_result(
            &started,
            false,
            json!({"noetl/execution_id": id_str, "noetl/status": "STARTED"}),
        ));
    }

    let deadline = tokio::time::Instant::now() + Duration::from_secs(tool.timeout_secs);
    loop {
        let status = backend
            .status(execution_id)
            .await
            .ok()
            .and_then(|s| s.get("status").and_then(Value::as_str).map(str::to_string));
        if let Some(s) = status.as_deref().filter(|s| is_terminal(s)) {
            return Ok(finished_result(backend, execution_id, s, tool.result_step.as_deref()).await);
        }
        if tokio::time::Instant::now() >= deadline {
            let running = json!({
                "execution_id": id_str,
                "status": status.unwrap_or_else(|| "RUNNING".into()),
                "message": format!("still running after {}s; poll {HELPER_STATUS} / {HELPER_RESULT}", tool.timeout_secs),
            });
            return Ok(tool_result(
                &running,
                false,
                json!({"noetl/execution_id": id_str, "noetl/status": "RUNNING"}),
            ));
        }
        tokio::time::sleep(timing.poll).await;
    }
}

async fn call_helper(
    backend: &dyn McpBackend,
    name: &str,
    args: &Value,
) -> Result<Value, (i64, String)> {
    let id = execution_id_arg(args).map_err(|e| (INVALID_PARAMS, e))?;
    let meta = json!({"noetl/execution_id": id.to_string()});
    match name {
        HELPER_STATUS => Ok(match backend.status(id).await {
            Ok(s) => tool_result(&sanitize_sensitive_data(&s), false, meta),
            Err(e) => tool_error(e, meta),
        }),
        HELPER_RESULT => {
            let status = match backend.status(id).await {
                Ok(s) => s.get("status").and_then(Value::as_str).unwrap_or("").to_string(),
                Err(e) => return Ok(tool_error(e, meta)),
            };
            if !is_terminal(&status) {
                let pending = json!({"execution_id": id.to_string(), "status": status});
                return Ok(tool_result(&pending, false, meta));
            }
            let step = args.get("result_step").and_then(Value::as_str);
            Ok(finished_result(backend, id, &status, step).await)
        }
        HELPER_CANCEL => Ok(match backend.cancel(id).await {
            Ok(()) => tool_result(
                &json!({"execution_id": id.to_string(), "status": "cancel_requested"}),
                false,
                meta,
            ),
            Err(e) => tool_error(e, meta),
        }),
        _ => Err((INVALID_PARAMS, format!("unknown tool `{name}`"))),
    }
}

/// Handle one JSON-RPC message. `None` = a notification (no response body).
pub async fn handle_message(
    backend: &dyn McpBackend,
    server: Option<&str>,
    message: &Value,
    timing: Timing,
) -> Option<Value> {
    if message.is_array() {
        // JSON-RPC batching was removed from MCP in 2025-06-18.
        return Some(rpc_error(&Value::Null, INVALID_REQUEST, "batch requests are not supported"));
    }
    let Some(obj) = message.as_object() else {
        return Some(rpc_error(&Value::Null, INVALID_REQUEST, "a JSON-RPC message must be an object"));
    };
    let method = obj.get("method").and_then(Value::as_str);
    let id = obj.get("id").cloned();
    let Some(id) = id.filter(|v| !v.is_null()) else {
        // A notification (no id) or a response: nothing to answer. Both
        // `notifications/initialized` and `notifications/cancelled` land here.
        return None;
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Some(rpc_error(&id, INVALID_REQUEST, "`jsonrpc` must be \"2.0\""));
    }
    let Some(method) = method else {
        return Some(rpc_error(&id, INVALID_REQUEST, "`method` is required"));
    };
    let params = obj.get("params").cloned().unwrap_or(Value::Null);

    match method {
        "initialize" => {
            let requested = params.get("protocolVersion").and_then(Value::as_str);
            let version = requested
                .filter(|v| SUPPORTED_PROTOCOL_VERSIONS.contains(v))
                .unwrap_or(SUPPORTED_PROTOCOL_VERSIONS[0]);
            Some(rpc_result(
                &id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
                    "instructions": "Each tool runs a NoETL playbook. mode: async tools return an execution_id; poll noetl.execution.status and noetl.execution.result.",
                }),
            ))
        }
        "ping" => Some(rpc_result(&id, json!({}))),
        "tools/list" => Some(match backend.registry(server).await {
            Ok(Some(reg)) => rpc_result(&id, list_result(&reg)),
            Ok(None) => rpc_error(&id, INVALID_PARAMS, "no such MCP server"),
            Err(e) => rpc_error(&id, INTERNAL_ERROR, e),
        }),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(rpc_error(&id, INVALID_PARAMS, "`params.name` is required"));
            };
            let args = params.get("arguments");
            let outcome = if name.starts_with(HELPER_PREFIX) {
                call_helper(backend, name, args.unwrap_or(&Value::Null)).await
            } else {
                match backend.registry(server).await {
                    Ok(Some(reg)) => match reg.find(name) {
                        Some(tool) => call_exposed(backend, tool, args, timing).await,
                        None => Err((INVALID_PARAMS, format!("unknown tool `{name}`"))),
                    },
                    Ok(None) => Err((INVALID_PARAMS, "no such MCP server".into())),
                    Err(e) => Err((INTERNAL_ERROR, e)),
                }
            };
            Some(match outcome {
                Ok(result) => rpc_result(&id, result),
                Err((code, msg)) => rpc_error(&id, code, msg),
            })
        }
        other => Some(rpc_error(&id, METHOD_NOT_FOUND, format!("method `{other}` is not supported"))),
    }
}

/// Parse an HTTP body and answer it.
pub async fn handle_body(
    backend: &dyn McpBackend,
    server: Option<&str>,
    body: &[u8],
    timing: Timing,
) -> Response {
    let message: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(e) => {
            return Json(rpc_error(&Value::Null, PARSE_ERROR, format!("invalid JSON: {e}")))
                .into_response()
        }
    };
    // An unknown `kind: Mcp` path is a 404 for every method, so a client
    // pointed at the wrong URL learns that at `initialize`.
    if server.is_some() {
        match backend.registry(server).await {
            Ok(Some(_)) => {}
            Ok(None) => return (StatusCode::NOT_FOUND, "no such MCP server").into_response(),
            Err(e) => {
                return (StatusCode::NOT_FOUND, e).into_response();
            }
        }
    }
    match handle_message(backend, server, &message, timing).await {
        Some(reply) => Json(reply).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

// ---------------------------------------------------------------------------
// The real backend.
// ---------------------------------------------------------------------------

/// [`McpBackend`] over the catalog, `execute_one` and [`ExecutionService`].
#[derive(Clone)]
pub struct ServerBackend {
    pub state: crate::state::AppState,
    pub executions: crate::services::ExecutionService,
}

impl ServerBackend {
    async fn exposed_playbooks(&self) -> Result<Vec<Result<ExposedTool, Problem>>, String> {
        let rows = crate::db::queries::catalog::list_latest_playbooks_mentioning(
            self.state.pools.cluster(),
            "expose",
        )
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .filter_map(|(path, content)| exposure_from_playbook(&path, &content))
            .collect())
    }
}

#[async_trait]
impl McpBackend for ServerBackend {
    async fn registry(&self, server: Option<&str>) -> Result<Option<Registry>, String> {
        let declared = self.exposed_playbooks().await?;
        let Some(server) = server else {
            return Ok(Some(Registry::build(declared)));
        };
        let entry = crate::db::queries::catalog::get_catalog_latest(self.state.pools.cluster(), server)
            .await
            .map_err(|e| e.to_string())?;
        let Some(entry) = entry.filter(|e| e.kind.eq_ignore_ascii_case("mcp")) else {
            return Ok(None);
        };
        let content = entry.content.unwrap_or_default();
        let exposed: HashMap<String, ExposedTool> = declared
            .into_iter()
            .filter_map(Result::ok)
            .map(|t| (t.playbook.clone(), t))
            .collect();
        match tools_from_mcp_resource(server, &content, &exposed) {
            Ok(tools) => Ok(Some(Registry::build(tools))),
            // A `kind: Mcp` resource that does not serve is a server NoETL
            // calls, not one it is: no such MCP server here.
            Err(_) => Ok(None),
        }
    }

    async fn start(&self, playbook: &str, workload: Map<String, Value>) -> Result<i64, String> {
        let request = crate::handlers::execute::ExecuteRequest {
            path: Some(playbook.to_string()),
            catalog_id: None,
            payload: workload.into_iter().collect(),
            parent_execution_id: None,
            execution_pool: None,
            trace: None,
            dedup: None,
        };
        crate::handlers::execute::execute_one(&self.state, request, "mcp")
            .await
            .map(|o| o.execution_id)
            .map_err(|e| e.to_string())
    }

    async fn status(&self, execution_id: i64) -> Result<Value, String> {
        let s = self
            .executions
            .get_status(execution_id)
            .await
            .map_err(|e| e.to_string())?;
        serde_json::to_value(s).map_err(|e| e.to_string())
    }

    async fn detail(&self, execution_id: i64) -> Result<Value, String> {
        let d = self.executions.get(execution_id).await.map_err(|e| e.to_string())?;
        serde_json::to_value(d).map_err(|e| e.to_string())
    }

    async fn cancel(&self, execution_id: i64) -> Result<(), String> {
        self.executions.cancel(execution_id).await.map_err(|e| e.to_string())
    }
}

/// The routes, for any backend (tests mount a scripted one).
pub fn routes_for<B: McpBackend + Clone + 'static>(backend: B, timing: Timing) -> Router {
    let root = backend.clone();
    let named = backend;
    let not_get = || async { (StatusCode::METHOD_NOT_ALLOWED, "POST JSON-RPC messages; this server opens no SSE stream") };
    Router::new()
        .route(
            "/api/mcp",
            post(move |body: Bytes| {
                let b = root.clone();
                async move { handle_body(&b, None, &body, timing).await }
            })
            .get(not_get),
        )
        .route(
            "/api/mcp/{*server}",
            post(move |Path(server): Path<String>, body: Bytes| {
                let b = named.clone();
                async move { handle_body(&b, Some(server.trim_matches('/')), &body, timing).await }
            })
            .get(not_get),
        )
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
}

/// The production routes.
pub fn routes(state: crate::state::AppState, executions: crate::services::ExecutionService) -> Router {
    routes_for(ServerBackend { state, executions }, Timing::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;

    const FINDINGS: &str = r#"
apiVersion: noetl.io/v2
kind: Playbook
metadata:
  name: findings
  path: qnetmon/tools/findings_in_window
  expose:
    mcp:
      tool: qnetmon_findings
      description: Current findings.
      input_schema:
        type: object
        properties:
          minutes: { type: integer }
        required: [minutes]
workflow:
  - step: start
    tool: { kind: noop }
"#;

    const SLOW: &str = r#"
apiVersion: noetl.io/v2
kind: Playbook
metadata:
  name: slow
  path: tools/slow
  expose:
    mcp:
      tool: slow_report
      description: A long report.
      input_schema: { type: object }
      mode: async
workflow:
  - step: start
    tool: { kind: noop }
"#;

    type Started = Arc<Mutex<Vec<(String, Map<String, Value>)>>>;

    /// Scripted backend: execution N's status sequence and final detail.
    #[derive(Clone, Default)]
    struct Fake {
        registry: Registry,
        named: Option<(String, Registry)>,
        statuses: Arc<Mutex<Vec<&'static str>>>,
        detail: Value,
        started: Started,
        cancelled: Arc<Mutex<Vec<i64>>>,
        start_error: Option<String>,
    }

    #[async_trait]
    impl McpBackend for Fake {
        async fn registry(&self, server: Option<&str>) -> Result<Option<Registry>, String> {
            match server {
                None => Ok(Some(self.registry.clone())),
                Some(s) => Ok(self
                    .named
                    .as_ref()
                    .filter(|(n, _)| n == s)
                    .map(|(_, r)| r.clone())),
            }
        }
        async fn start(&self, playbook: &str, workload: Map<String, Value>) -> Result<i64, String> {
            if let Some(e) = &self.start_error {
                return Err(e.clone());
            }
            self.started.lock().unwrap().push((playbook.to_string(), workload));
            Ok(9_007_199_254_740_993) // > 2^53: must travel as a string
        }
        async fn status(&self, id: i64) -> Result<Value, String> {
            let mut s = self.statuses.lock().unwrap();
            let current = if s.len() > 1 { s.remove(0) } else { s[0] };
            Ok(json!({"execution_id": id, "status": current}))
        }
        async fn detail(&self, _id: i64) -> Result<Value, String> {
            Ok(self.detail.clone())
        }
        async fn cancel(&self, id: i64) -> Result<(), String> {
            self.cancelled.lock().unwrap().push(id);
            Ok(())
        }
    }

    fn fast() -> Timing {
        Timing {
            poll: Duration::from_millis(1),
        }
    }

    fn registry_of(docs: &[(&str, &str)]) -> Registry {
        Registry::build(
            docs.iter()
                .filter_map(|(p, c)| exposure_from_playbook(p, c))
                .collect(),
        )
    }

    fn call_done(node: &str, data: Value) -> Value {
        json!({"event_type": "call.done", "node_name": node, "status": "COMPLETED",
               "result": {"command_id": "1", "result": {"status": "ok", "context": {"status": "ok", "data": data}}}})
    }

    fn fake() -> Fake {
        Fake {
            registry: registry_of(&[
                ("qnetmon/tools/findings_in_window", FINDINGS),
                ("tools/slow", SLOW),
            ]),
            statuses: Arc::new(Mutex::new(vec!["RUNNING", "COMPLETED"])),
            detail: json!({"events": [
                call_done("start", json!(null)),
                call_done("dispatch", json!({"findings": [{"rule_id": "ap_offline"}], "password": "hunter2"})),
            ]}),
            ..Default::default()
        }
    }

    async fn rpc(b: &Fake, method: &str, params: Value) -> Value {
        let msg = json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params});
        handle_message(b, None, &msg, fast()).await.expect("a request gets a reply")
    }

    // ---- flag ----

    #[test]
    fn flag_is_strict() {
        assert!(mcp_server_enabled_from(|_| Some("true".into())));
        for v in ["TRUE", "1", "yes", ""] {
            assert!(!mcp_server_enabled_from(|_| Some(v.into())), "{v}");
        }
        assert!(!mcp_server_enabled_from(|_| None));
    }

    // ---- declarations ----

    #[test]
    fn playbook_without_expose_is_not_a_tool() {
        let doc = "apiVersion: noetl.io/v2\nkind: Playbook\nmetadata: { name: x, description: we expose nothing }\nworkflow: []\n";
        assert_eq!(exposure_from_playbook("x", doc), None);
    }

    #[test]
    fn declaration_parses_with_defaults() {
        let t = exposure_from_playbook("qnetmon/tools/findings_in_window", FINDINGS)
            .unwrap()
            .unwrap();
        assert_eq!(t.name, "qnetmon_findings");
        assert_eq!(t.playbook, "qnetmon/tools/findings_in_window");
        assert_eq!(t.mode, Mode::Sync);
        assert_eq!(t.timeout_secs, DEFAULT_SYNC_TIMEOUT_SECS);
    }

    #[test]
    fn input_schema_is_required_and_never_derived_from_workload() {
        let doc = r#"
apiVersion: noetl.io/v2
kind: Playbook
metadata:
  name: x
  expose: { mcp: { tool: x, description: d } }
workload: { minutes: 60 }
workflow: []
"#;
        let err = exposure_from_playbook("p/x", doc).unwrap().unwrap_err();
        assert!(err.reason.contains("input_schema"), "{}", err.reason);
        assert!(err.reason.contains("not derived from `workload`"), "{}", err.reason);
    }

    #[test]
    fn bad_declarations_are_reported_not_served() {
        let base = |mcp: &str| format!("apiVersion: noetl.io/v2\nkind: Playbook\nmetadata:\n  name: x\n  expose:\n    mcp: {mcp}\nworkflow: []\n");
        for (mcp, want) in [
            ("{ description: d, input_schema: { type: object } }", "`tool`"),
            ("{ tool: 'has space', description: d, input_schema: { type: object } }", "1-64"),
            ("{ tool: noetl.execution.status, description: d, input_schema: { type: object } }", "reserved"),
            ("{ tool: t, input_schema: { type: object } }", "description"),
            ("{ tool: t, description: d, input_schema: { type: string } }", "type` must be `object"),
            ("{ tool: t, description: d, input_schema: { type: object }, mode: later }", "mode"),
            ("{ tool: t, description: d, input_schema: { type: object }, timeout: 999 }", "timeout"),
        ] {
            let err = exposure_from_playbook("p/x", &base(mcp)).unwrap().unwrap_err();
            assert!(err.reason.contains(want), "{mcp}: {}", err.reason);
        }
    }

    #[test]
    fn duplicate_tool_names_exclude_every_holder() {
        let a = exposure_from_playbook("a", FINDINGS).unwrap();
        let b = exposure_from_playbook("b", FINDINGS).unwrap();
        let slow = exposure_from_playbook("tools/slow", SLOW).unwrap();
        let reg = Registry::build(vec![a, b, slow]);
        assert_eq!(reg.tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["slow_report"]);
        assert_eq!(reg.problems.len(), 2);
        assert!(reg.problems[0].reason.contains("a, b"), "{:?}", reg.problems);
    }

    #[test]
    fn mcp_resource_serves_only_with_serve_true_and_falls_back_to_playbook_declaration() {
        let exposed: HashMap<String, ExposedTool> = [exposure_from_playbook(
            "qnetmon/tools/findings_in_window",
            FINDINGS,
        )
        .unwrap()
        .unwrap()]
        .into_iter()
        .map(|t| (t.playbook.clone(), t))
        .collect();

        let calls_only = "apiVersion: noetl.io/v2\nkind: Mcp\nmetadata: { name: g, path: mcp/g }\nspec: { url: 'http://x' }\n";
        assert!(tools_from_mcp_resource("mcp/g", calls_only, &exposed).is_err());

        let serving = r#"
apiVersion: noetl.io/v2
kind: Mcp
metadata: { name: qnetmon, path: mcp/qnetmon }
spec:
  serve: true
  tools:
    - { name: findings, playbook: qnetmon/tools/findings_in_window }
    - { name: neighbors, playbook: qnetmon/tools/graph_neighbors, description: Neighbors.,
        input_schema: { type: object, properties: { node_id: { type: string } } } }
    - { name: bare, playbook: qnetmon/tools/unexposed }
"#;
        let reg = Registry::build(tools_from_mcp_resource("mcp/qnetmon", serving, &exposed).unwrap());
        let names: Vec<&str> = reg.tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["findings", "neighbors"]);
        // `findings` took description + schema from the playbook's own declaration.
        assert_eq!(reg.tools[0].description, "Current findings.");
        // `bare` names a playbook with no declaration and states no schema itself.
        assert_eq!(reg.problems.len(), 1);
        assert!(reg.problems[0].reason.contains("description"), "{:?}", reg.problems);
    }

    // ---- arguments ----

    #[test]
    fn arguments_are_an_allowlist() {
        let schema = json!({"type": "object", "properties": {"minutes": {"type": "integer"}}, "required": ["minutes"]});
        assert_eq!(check_arguments(&schema, Some(&json!({"minutes": 60}))).unwrap()["minutes"], 60);
        assert!(check_arguments(&schema, Some(&json!({}))).unwrap_err().contains("minutes"));
        assert!(check_arguments(&schema, Some(&json!({"minutes": "60"}))).unwrap_err().contains("integer"));
        let err = check_arguments(&schema, Some(&json!({"minutes": 1, "api_url": "http://evil"}))).unwrap_err();
        assert!(err.contains("api_url"), "an undeclared workload key must not pass: {err}");
        assert!(check_arguments(&schema, Some(&json!([1]))).is_err());

        let open = json!({"type": "object", "additionalProperties": true});
        assert!(check_arguments(&open, Some(&json!({"anything": 1}))).is_ok());
        assert!(check_arguments(&json!({"type": "object"}), None).unwrap().is_empty());
    }

    // ---- result selection ----

    #[test]
    fn result_selection_matches_return_result() {
        let detail = json!({"events": [
            call_done("first", json!({"a": 1})),
            call_done("dispatch", json!({"b": 2})),
            call_done("end", json!(null)),
            call_done("playbook", json!({"terminal": true})),
        ]});
        assert_eq!(select_result(&detail, None), Some(json!({"b": 2})));
        assert_eq!(select_result(&detail, Some("first")), Some(json!({"a": 1})));
        assert_eq!(select_result(&detail, Some("missing")), None);
    }

    // ---- protocol ----

    #[tokio::test]
    async fn initialize_negotiates_the_protocol_version() {
        let b = fake();
        let r = rpc(&b, "initialize", json!({"protocolVersion": "2025-03-26"})).await;
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        assert!(r["result"]["capabilities"]["tools"].is_object());
        let r = rpc(&b, "initialize", json!({"protocolVersion": "1999-01-01"})).await;
        assert_eq!(r["result"]["protocolVersion"], SUPPORTED_PROTOCOL_VERSIONS[0]);
        assert_eq!(rpc(&b, "ping", json!({})).await["result"], json!({}));
    }

    #[tokio::test]
    async fn notifications_get_no_reply_and_errors_are_jsonrpc() {
        let b = fake();
        let note = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert_eq!(handle_message(&b, None, &note, fast()).await, None);
        assert_eq!(rpc(&b, "resources/list", json!({})).await["error"]["code"], METHOD_NOT_FOUND);
        let batch = json!([{"jsonrpc": "2.0", "id": 1, "method": "ping"}]);
        assert_eq!(handle_message(&b, None, &batch, fast()).await.unwrap()["error"]["code"], INVALID_REQUEST);
        let v1 = json!({"jsonrpc": "1.0", "id": 1, "method": "ping"});
        assert_eq!(handle_message(&b, None, &v1, fast()).await.unwrap()["error"]["code"], INVALID_REQUEST);
    }

    #[tokio::test]
    async fn tools_list_has_exposed_playbooks_then_helpers() {
        let r = rpc(&fake(), "tools/list", json!({})).await;
        let tools = r["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["qnetmon_findings", "slow_report", HELPER_STATUS, HELPER_RESULT, HELPER_CANCEL]);
        assert_eq!(tools[0]["inputSchema"]["required"], json!(["minutes"]));
        assert_eq!(tools[0]["_meta"]["noetl/playbook"], "qnetmon/tools/findings_in_window");
        assert_eq!(tools[1]["_meta"]["noetl/mode"], "async");
    }

    #[tokio::test]
    async fn tools_list_reports_what_was_not_served() {
        let mut b = fake();
        b.registry.problems.push(Problem { source: "p/x".into(), reason: "why".into() });
        let r = rpc(&b, "tools/list", json!({})).await;
        assert_eq!(r["result"]["_meta"]["noetl/not_served"][0]["source"], "p/x");
    }

    #[tokio::test]
    async fn sync_call_waits_and_returns_the_sanitized_result() {
        let b = fake();
        let r = rpc(&b, "tools/call", json!({"name": "qnetmon_findings", "arguments": {"minutes": 120}})).await;
        let res = &r["result"];
        assert_eq!(res["isError"], false, "{r}");
        assert_eq!(res["structuredContent"]["findings"][0]["rule_id"], "ap_offline");
        assert_ne!(res["structuredContent"]["password"], "hunter2", "results are sanitized");
        assert!(!res["content"][0]["text"].as_str().unwrap().contains("hunter2"));
        assert_eq!(res["_meta"]["noetl/execution_id"], "9007199254740993");
        assert_eq!(res["_meta"]["noetl/status"], "COMPLETED");
        let started = b.started.lock().unwrap();
        assert_eq!(started[0].0, "qnetmon/tools/findings_in_window");
        assert_eq!(started[0].1["minutes"], 120);
    }

    #[tokio::test]
    async fn sync_call_on_a_failed_execution_is_a_tool_error() {
        let mut b = fake();
        b.statuses = Arc::new(Mutex::new(vec!["FAILED"]));
        b.detail = json!({"events": [{"event_type": "command.failed", "node_name": "x", "status": "FAILED", "error": "boom"}]});
        let r = rpc(&b, "tools/call", json!({"name": "qnetmon_findings", "arguments": {"minutes": 1}})).await;
        assert_eq!(r["result"]["isError"], true);
        assert_eq!(r["result"]["content"][0]["text"], "boom");
    }

    #[tokio::test]
    async fn sync_call_past_its_timeout_returns_the_execution_id() {
        let mut b = fake();
        b.statuses = Arc::new(Mutex::new(vec!["RUNNING"]));
        b.registry.tools[0].timeout_secs = 1;
        let r = rpc(&b, "tools/call", json!({"name": "qnetmon_findings", "arguments": {"minutes": 1}})).await;
        assert_eq!(r["result"]["isError"], false);
        assert_eq!(r["result"]["structuredContent"]["status"], "RUNNING");
        assert_eq!(r["result"]["structuredContent"]["execution_id"], "9007199254740993");
    }

    #[tokio::test]
    async fn async_call_returns_at_once_and_helpers_follow_up() {
        let b = fake();
        let r = rpc(&b, "tools/call", json!({"name": "slow_report", "arguments": {}})).await;
        assert_eq!(r["result"]["structuredContent"]["status"], "started");
        let id = r["result"]["structuredContent"]["execution_id"].clone();

        // statuses: RUNNING then COMPLETED
        let s = rpc(&b, "tools/call", json!({"name": HELPER_RESULT, "arguments": {"execution_id": id}})).await;
        assert_eq!(s["result"]["structuredContent"]["status"], "RUNNING");
        let s = rpc(&b, "tools/call", json!({"name": HELPER_RESULT, "arguments": {"execution_id": id}})).await;
        assert_eq!(s["result"]["structuredContent"]["findings"][0]["rule_id"], "ap_offline");

        let c = rpc(&b, "tools/call", json!({"name": HELPER_CANCEL, "arguments": {"execution_id": id}})).await;
        assert_eq!(c["result"]["structuredContent"]["status"], "cancel_requested");
        assert_eq!(*b.cancelled.lock().unwrap(), vec![9_007_199_254_740_993]);

        let bad = rpc(&b, "tools/call", json!({"name": HELPER_STATUS, "arguments": {}})).await;
        assert_eq!(bad["error"]["code"], INVALID_PARAMS);
    }

    #[tokio::test]
    async fn unknown_tool_and_bad_arguments_are_invalid_params() {
        let b = fake();
        let r = rpc(&b, "tools/call", json!({"name": "nope"})).await;
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
        let r = rpc(&b, "tools/call", json!({"name": "qnetmon_findings", "arguments": {"minutes": 1, "extra": 1}})).await;
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
        assert!(b.started.lock().unwrap().is_empty(), "nothing starts on bad arguments");
    }

    #[tokio::test]
    async fn a_start_failure_is_a_tool_error_the_model_can_see() {
        let mut b = fake();
        b.start_error = Some("Catalog entry 'qnetmon/tools/findings_in_window' not found".into());
        let r = rpc(&b, "tools/call", json!({"name": "qnetmon_findings", "arguments": {"minutes": 1}})).await;
        assert_eq!(r["result"]["isError"], true);
        assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("not found"));
    }

    // ---- HTTP ----

    async fn post(app: Router, uri: &str, body: &str) -> (StatusCode, String) {
        let res = app
            .oneshot(
                Request::post(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn http_routes_answer_json_rpc() {
        let mut b = fake();
        b.named = Some(("mcp/qnetmon".into(), registry_of(&[("tools/slow", SLOW)])));
        let app = routes_for(b, fast());

        let (s, body) = post(app.clone(), "/api/mcp", r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains("qnetmon_findings"), "{body}");

        let (s, body) = post(app.clone(), "/api/mcp/mcp/qnetmon", r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains("slow_report") && !body.contains("qnetmon_findings"), "{body}");

        let (s, _) = post(app.clone(), "/api/mcp/mcp/missing", r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#).await;
        assert_eq!(s, StatusCode::NOT_FOUND);

        let (s, body) = post(app.clone(), "/api/mcp", r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).await;
        assert_eq!((s, body.as_str()), (StatusCode::ACCEPTED, ""));

        let (s, body) = post(app.clone(), "/api/mcp", "{not json").await;
        assert_eq!(s, StatusCode::OK);
        assert!(body.contains("-32700"), "{body}");

        let res = app
            .oneshot(Request::get("/api/mcp").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
}
