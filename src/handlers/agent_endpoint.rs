//! Playbooks as agents other systems call, with a completion callback:
//! build plan item 7 (noetl/ai-meta `docs/rfc/interop-missing-tools-build-plan.md`),
//! the "agent endpoint with a callback" design in the interop RFC (§3.3/§3.4,
//! Phase 6 agent cards).
//!
//! - `GET  /api/agents`: the cards of every playbook that opts in with
//!   `metadata.expose.agent`.
//! - `GET  /api/agents/cards/{*path}`: one card.
//! - `POST /api/agents/send`: `{agent, input, callback_url?}` starts the playbook
//!   and answers `202 {task_id, status, callback_token?}`.
//! - `GET  /api/agents/tasks/{task_id}`: `{task_id, agent, status, result | error}`.
//!
//! The task id is the execution id, so everything that already works on an
//! execution (the event log, `/api/executions/{id}`, the gateway's owner check)
//! works on a task.
//!
//! # Opt-in, hand-written input schema
//!
//! ```yaml
//! metadata:
//!   path: qnetmon/troubleshoot/ask_agent
//!   expose:
//!     agent:
//!       card:
//!         name: qnetmon-troubleshooter         # required
//!         description: Answers network questions.  # required
//!         version: "1"
//!         skills:
//!           - { id: network-troubleshooting, name: Network troubleshooting }
//!       input_schema:                          # required (or expose.mcp.input_schema)
//!         type: object
//!         properties: { question: { type: string } }
//!         required: [question]
//!       result_step: analyze                   # optional
//! ```
//!
//! The input schema is authored, never derived from `workload`, and is the
//! argument allowlist (same rule and code as `/api/mcp`,
//! [`super::mcp_server::check_arguments`]). With no `expose.agent.input_schema`
//! the playbook's `expose.mcp.input_schema` is used, so a playbook exposed both
//! ways states its schema once.
//!
//! # Callbacks
//!
//! With `callback_url`, the server POSTs the final task (the `GET` shape) to that
//! URL once the execution ends, with `Authorization: Bearer <callback_token>`.
//! The token is random, minted for this task alone and returned only in the
//! `202`: the caller checks the callback by comparing it, and needs no key shared
//! with the server. A token can match exactly one task's callback.
//!
//! - **Destinations are an allowlist.** `callback_url`'s origin
//!   (`scheme://host[:port]`) must be listed in `NOETL_AGENT_CALLBACK_ORIGINS`
//!   (comma-separated). Empty or unset refuses every `callback_url`. Redirects are
//!   not followed. This keeps the server from being used to reach arbitrary
//!   internal addresses (SSRF).
//! - **Delivery is best effort and in-process.** The watcher polls the execution
//!   and delivers with retries; it does not survive a server restart. The caller
//!   can always fall back to `GET /api/agents/tasks/{task_id}`. A durable
//!   callback belongs to RFC Phase 4 (`/api/callbacks`, `RemoteTask`).
//! - At most [`MAX_WATCHERS`] callbacks are pending at once; past that, a send
//!   with `callback_url` is refused with 503 before anything starts.
//!
//! # Off by default
//!
//! Mounted only when `NOETL_AGENT_ENDPOINT=true` (strict).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rand::RngCore;
use serde_json::{json, Map, Value};

use super::mcp_server::{
    check_arguments, is_terminal, problem, select_result, yaml_to_json, McpBackend, Problem,
};
use crate::sanitize::sanitize_sensitive_data;

pub const AGENT_ENDPOINT_ENV: &str = "NOETL_AGENT_ENDPOINT";
pub const CALLBACK_ORIGINS_ENV: &str = "NOETL_AGENT_CALLBACK_ORIGINS";

/// Pending callbacks at once, per process.
pub const MAX_WATCHERS: usize = 256;
/// How long a watcher waits for an execution to end.
pub const DEFAULT_DEADLINE_SECS: u64 = 15 * 60;
pub const MAX_DEADLINE_SECS: u64 = 60 * 60;

pub fn agent_endpoint_enabled() -> bool {
    agent_endpoint_enabled_from(|k| std::env::var(k).ok())
}

pub fn agent_endpoint_enabled_from(get: impl Fn(&str) -> Option<String>) -> bool {
    get(AGENT_ENDPOINT_ENV).as_deref() == Some("true")
}

/// One playbook exposed as an agent.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentDecl {
    pub playbook: String,
    pub name: String,
    pub description: String,
    pub version: Option<String>,
    pub skills: Vec<Value>,
    pub input_schema: Value,
    pub result_step: Option<String>,
}

/// A playbook's agent exposure. `None` when it does not opt in.
pub fn agent_from_playbook(path: &str, content: &str) -> Option<Result<AgentDecl, Problem>> {
    let doc = yaml_to_json(content).ok()?;
    let spec = doc.pointer("/metadata/expose/agent")?;
    Some(parse_agent(path, spec, doc.pointer("/metadata/expose/mcp/input_schema")))
}

fn parse_agent(path: &str, spec: &Value, mcp_schema: Option<&Value>) -> Result<AgentDecl, Problem> {
    let card = spec
        .get("card")
        .and_then(Value::as_object)
        .ok_or_else(|| problem(path, "`expose.agent.card` (a mapping) is required"))?;
    let text = |k: &str| {
        card.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let name = text("name").ok_or_else(|| problem(path, "`expose.agent.card.name` is required"))?;
    let description =
        text("description").ok_or_else(|| problem(path, "`expose.agent.card.description` is required"))?;
    let version = card.get("version").and_then(|v| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    });
    let skills = match card.get("skills") {
        None => Vec::new(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|s| match s {
                Value::String(id) => Ok(json!({"id": id, "name": id})),
                Value::Object(o) if o.get("id").and_then(Value::as_str).is_some() => Ok(s.clone()),
                _ => Err(problem(path, "each skill is a string or a mapping with an `id`")),
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(problem(path, "`expose.agent.card.skills` must be a list")),
    };
    let input_schema = spec
        .get("input_schema")
        .or(mcp_schema)
        .cloned()
        .ok_or_else(|| {
            problem(
                path,
                "`expose.agent.input_schema` is required and is written by hand (it is not derived from `workload`)",
            )
        })?;
    if input_schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err(problem(path, "`input_schema.type` must be `object`"));
    }
    Ok(AgentDecl {
        playbook: path.to_string(),
        name,
        description,
        version,
        skills,
        input_schema,
        result_step: spec.get("result_step").and_then(Value::as_str).map(str::to_string),
    })
}

/// The card a caller reads before sending a task. Shaped after A2A's AgentCard
/// (name, description, version, skills, input modes) without claiming A2A
/// conformance: the RFC's open question 4 leans MCP-first, with the card as
/// metadata.
pub fn card(a: &AgentDecl) -> Value {
    json!({
        "name": a.name,
        "description": a.description,
        "version": a.version,
        "agent": a.playbook,
        "skills": a.skills,
        "inputSchema": a.input_schema,
        "defaultInputModes": ["application/json"],
        "defaultOutputModes": ["application/json"],
        "capabilities": {"callbacks": true, "streaming": false},
        "endpoints": {
            "send": "/api/agents/send",
            "task": "/api/agents/tasks/{task_id}",
        },
    })
}

/// Where callbacks may go. Entries are origins: `scheme://host[:port]`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CallbackPolicy {
    origins: Vec<String>,
}

impl CallbackPolicy {
    pub fn from_env() -> Self {
        Self::parse(&std::env::var(CALLBACK_ORIGINS_ENV).unwrap_or_default())
    }

    pub fn parse(value: &str) -> Self {
        CallbackPolicy {
            origins: value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .filter_map(|s| reqwest::Url::parse(s).ok())
                .map(|u| u.origin().ascii_serialization())
                .filter(|o| o != "null")
                .collect(),
        }
    }

    /// The URL to call, when its origin is allowed.
    pub fn check(&self, url: &str) -> Result<reqwest::Url, String> {
        let u = reqwest::Url::parse(url).map_err(|e| format!("callback_url is not a URL: {e}"))?;
        if !matches!(u.scheme(), "https" | "http") {
            return Err("callback_url must be http(s)".into());
        }
        if !u.username().is_empty() || u.password().is_some() {
            return Err("callback_url must not carry credentials".into());
        }
        if self.origins.is_empty() {
            return Err(format!(
                "callbacks are not enabled on this server ({CALLBACK_ORIGINS_ENV} is empty); poll the task instead"
            ));
        }
        let origin = u.origin().ascii_serialization();
        if !self.origins.contains(&origin) {
            return Err(format!("callback_url origin {origin} is not in {CALLBACK_ORIGINS_ENV}"));
        }
        Ok(u)
    }
}

/// A random 256-bit token, URL-safe.
pub fn new_callback_token() -> String {
    use base64::Engine;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// NoETL execution status → task state (the A2A vocabulary).
pub fn task_state(status: &str) -> &'static str {
    match status {
        "COMPLETED" => "completed",
        "FAILED" => "failed",
        "CANCELLED" => "canceled",
        "PENDING" | "STARTED" => "submitted",
        _ => "working",
    }
}

/// What the agent endpoint needs beyond [`McpBackend`]'s start / status /
/// detail.
#[async_trait]
pub trait AgentBackend: McpBackend {
    async fn agents(&self) -> Result<Vec<Result<AgentDecl, Problem>>, String>;
}

#[async_trait]
impl AgentBackend for super::mcp_server::ServerBackend {
    async fn agents(&self) -> Result<Vec<Result<AgentDecl, Problem>>, String> {
        let rows = crate::db::queries::catalog::list_latest_playbooks_mentioning(
            self.state.pools.cluster(),
            "expose",
        )
        .await
        .map_err(|e| e.to_string())?;
        Ok(rows
            .into_iter()
            .filter_map(|(path, content)| agent_from_playbook(&path, &content))
            .collect())
    }
}

/// The task as `GET /api/agents/tasks/{id}` and the callback report it.
pub async fn task_view(backend: &dyn McpBackend, task_id: i64, result_step: Option<&str>) -> Result<Value, String> {
    let status = backend.status(task_id).await?;
    let raw = status.get("status").and_then(Value::as_str).unwrap_or("").to_string();
    let mut view = json!({"task_id": task_id.to_string(), "status": task_state(&raw)});
    if !is_terminal(&raw) {
        return Ok(view);
    }
    let detail = backend.detail(task_id).await?;
    if let Some(agent) = detail.get("path").and_then(Value::as_str) {
        view["agent"] = json!(agent);
    }
    if raw == "COMPLETED" {
        view["result"] = sanitize_sensitive_data(&select_result(&detail, result_step).unwrap_or(Value::Null));
    } else {
        let err = detail
            .get("events")
            .and_then(Value::as_array)
            .and_then(|evs| evs.iter().find_map(|e| e.get("error").and_then(Value::as_str)))
            .unwrap_or("the task did not complete");
        view["error"] = sanitize_sensitive_data(&json!(err));
    }
    Ok(view)
}

/// Timing for the callback watcher; tests shorten it.
#[derive(Debug, Clone, Copy)]
pub struct WatchTiming {
    pub poll: Duration,
    pub retry_base: Duration,
    pub attempts: u32,
}

impl Default for WatchTiming {
    fn default() -> Self {
        WatchTiming {
            poll: Duration::from_secs(1),
            retry_base: Duration::from_secs(1),
            attempts: 4,
        }
    }
}

#[derive(Clone)]
pub struct AgentState<B> {
    pub backend: B,
    pub callbacks: CallbackPolicy,
    pub timing: WatchTiming,
    pub watchers: Arc<AtomicUsize>,
    pub http: reqwest::Client,
}

impl<B> AgentState<B> {
    pub fn new(backend: B, callbacks: CallbackPolicy, timing: WatchTiming) -> Self {
        AgentState {
            backend,
            callbacks,
            timing,
            watchers: Arc::new(AtomicUsize::new(0)),
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
        }
    }
}

/// Decrements the pending-callback count however the watcher ends.
struct WatcherSlot(Arc<AtomicUsize>);

impl Drop for WatcherSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

async fn find_agent<B: AgentBackend>(backend: &B, path: &str) -> Result<Option<AgentDecl>, String> {
    Ok(backend
        .agents()
        .await?
        .into_iter()
        .filter_map(Result::ok)
        .find(|a| a.playbook == path))
}

async fn list_cards<B: AgentBackend>(State(s): State<AgentState<B>>) -> Response {
    match s.backend.agents().await {
        Ok(all) => {
            let (ok, bad): (Vec<_>, Vec<_>) = all.into_iter().partition(Result::is_ok);
            let cards: Vec<Value> = ok.into_iter().filter_map(Result::ok).map(|a| card(&a)).collect();
            let not_served: Vec<Value> = bad
                .into_iter()
                .filter_map(Result::err)
                .map(|p| json!({"source": p.source, "reason": p.reason}))
                .collect();
            Json(json!({"agents": cards, "not_served": not_served})).into_response()
        }
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn get_card<B: AgentBackend>(State(s): State<AgentState<B>>, Path(path): Path<String>) -> Response {
    match find_agent(&s.backend, path.trim_matches('/')).await {
        Ok(Some(a)) => Json(card(&a)).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "no such agent"),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

async fn send<B: AgentBackend + Clone + 'static>(State(s): State<AgentState<B>>, Json(body): Json<Value>) -> Response {
    let Some(agent_path) = body.get("agent").and_then(Value::as_str) else {
        return error(StatusCode::BAD_REQUEST, "`agent` (the playbook path) is required");
    };
    let agent = match find_agent(&s.backend, agent_path.trim_matches('/')).await {
        Ok(Some(a)) => a,
        Ok(None) => return error(StatusCode::NOT_FOUND, "no such agent"),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, e),
    };
    let input: Map<String, Value> = match check_arguments(&agent.input_schema, body.get("input")) {
        Ok(m) => m,
        Err(e) => return error(StatusCode::UNPROCESSABLE_ENTITY, e),
    };
    let deadline_secs = match body.get("deadline_secs") {
        None => DEFAULT_DEADLINE_SECS,
        Some(v) => match v.as_u64() {
            Some(d) if (1..=MAX_DEADLINE_SECS).contains(&d) => d,
            _ => return error(StatusCode::BAD_REQUEST, format!("`deadline_secs` must be 1..={MAX_DEADLINE_SECS}")),
        },
    };

    // Everything about the callback is checked BEFORE the execution starts, so a
    // refused callback never leaves an orphan run behind.
    let callback = match body.get("callback_url").and_then(Value::as_str) {
        None => None,
        Some(url) => {
            let url = match s.callbacks.check(url) {
                Ok(u) => u,
                Err(e) => return error(StatusCode::BAD_REQUEST, e),
            };
            if s.watchers.fetch_add(1, Ordering::SeqCst) >= MAX_WATCHERS {
                s.watchers.fetch_sub(1, Ordering::SeqCst);
                return error(StatusCode::SERVICE_UNAVAILABLE, "too many pending callbacks; poll the task instead");
            }
            Some((url, WatcherSlot(s.watchers.clone())))
        }
    };

    let task_id = match s.backend.start(&agent.playbook, input).await {
        Ok(id) => id,
        Err(e) => return error(StatusCode::BAD_GATEWAY, format!("could not start `{}`: {e}", agent.playbook)),
    };

    let mut reply = json!({"task_id": task_id.to_string(), "agent": agent.playbook, "status": "submitted"});
    if let Some((url, slot)) = callback {
        let token = new_callback_token();
        reply["callback_token"] = json!(token);
        let st = s.clone();
        let step = agent.result_step.clone();
        tokio::spawn(async move {
            let _slot = slot;
            watch_and_deliver(&st, task_id, step.as_deref(), url, token, Duration::from_secs(deadline_secs)).await;
        });
    }
    (StatusCode::ACCEPTED, Json(reply)).into_response()
}

/// Wait for the task to end (or the deadline), then deliver the callback.
pub async fn watch_and_deliver<B: AgentBackend>(
    s: &AgentState<B>,
    task_id: i64,
    result_step: Option<&str>,
    url: reqwest::Url,
    token: String,
    deadline: Duration,
) {
    let until = tokio::time::Instant::now() + deadline;
    let view = loop {
        match task_view(&s.backend, task_id, result_step).await {
            Ok(v) if matches!(v["status"].as_str(), Some("completed" | "failed" | "canceled")) => break v,
            _ if tokio::time::Instant::now() >= until => {
                break json!({"task_id": task_id.to_string(), "status": "working",
                             "error": "the callback deadline passed before the task ended; poll the task"})
            }
            _ => tokio::time::sleep(s.timing.poll).await,
        }
    };
    if !deliver_callback(&s.http, &url, &token, &task_id.to_string(), &view, &s.timing).await {
        tracing::error!(task_id, "agent callback not delivered; the caller must poll");
    }
}

/// POST `body` to `url` with the single-use bearer `token`, retrying with exponential backoff.
///
/// Shared by agent callbacks and instance task callbacks (`crate::instances`). `true` when the
/// receiver accepted it, or refused it with a 4xx that a retry cannot change (other than
/// 408/429): either way the caller must not send it again.
pub async fn deliver_callback(
    http: &reqwest::Client,
    url: &reqwest::Url,
    token: &str,
    task_id: &str,
    body: &Value,
    timing: &WatchTiming,
) -> bool {
    for attempt in 0..timing.attempts {
        let sent = http
            .post(url.clone())
            .bearer_auth(token)
            .header("x-noetl-task-id", task_id)
            .json(body)
            .send()
            .await;
        match sent {
            Ok(r) if r.status().is_success() => return true,
            // A 4xx other than 408/429 will not change on retry.
            Ok(r) if r.status().is_client_error() && !matches!(r.status().as_u16(), 408 | 429) => {
                tracing::warn!(task_id, status = %r.status(), "callback refused by the receiver");
                return true;
            }
            other => {
                let why = match other {
                    Ok(r) => r.status().to_string(),
                    Err(e) => e.to_string(),
                };
                tracing::warn!(task_id, attempt, "callback failed: {why}");
                tokio::time::sleep(timing.retry_base * 2u32.pow(attempt)).await;
            }
        }
    }
    false
}

async fn get_task<B: AgentBackend>(State(s): State<AgentState<B>>, Path(task_id): Path<String>) -> Response {
    let Some(id) = task_id.parse::<i64>().ok().filter(|i| *i > 0) else {
        return error(StatusCode::BAD_REQUEST, "task_id must be a positive integer");
    };
    // Read with the agent's own `result_step`, resolved from the execution's
    // playbook path, so polling and the callback report the same result.
    let step = match s.backend.detail(id).await {
        Ok(d) => match d.get("path").and_then(Value::as_str) {
            Some(p) => find_agent(&s.backend, p).await.ok().flatten().and_then(|a| a.result_step),
            None => None,
        },
        Err(_) => None,
    };
    match task_view(&s.backend, id, step.as_deref()).await {
        Ok(v) => Json(v).into_response(),
        Err(e) if e.contains("not found") => error(StatusCode::NOT_FOUND, "no such task"),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e),
    }
}

pub fn routes_for<B: AgentBackend + Clone + 'static>(state: AgentState<B>) -> Router {
    Router::new()
        .route("/api/agents", get(list_cards::<B>))
        .route("/api/agents/cards/{*path}", get(get_card::<B>))
        .route("/api/agents/send", post(send::<B>))
        .route("/api/agents/tasks/{task_id}", get(get_task::<B>))
        .with_state(state)
}

pub fn routes(state: crate::state::AppState, executions: crate::services::ExecutionService) -> Router {
    routes_for(AgentState::new(
        super::mcp_server::ServerBackend { state, executions },
        CallbackPolicy::from_env(),
        WatchTiming::default(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::mcp_server::Registry;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::Mutex;
    use tower::ServiceExt;

    const ASK: &str = r#"
apiVersion: noetl.io/v2
kind: Playbook
metadata:
  name: ask
  path: qnetmon/troubleshoot/ask_agent
  expose:
    agent:
      card:
        name: qnetmon-troubleshooter
        description: Answers network questions.
        version: 1
        skills: [network-troubleshooting]
      input_schema:
        type: object
        properties: { question: { type: string } }
        required: [question]
      result_step: analyze
workflow: []
"#;

    type Calls = Arc<Mutex<Vec<(String, Map<String, Value>)>>>;

    #[derive(Clone)]
    struct Fake {
        docs: Vec<(&'static str, &'static str)>,
        statuses: Arc<Mutex<Vec<&'static str>>>,
        started: Calls,
    }

    impl Fake {
        fn new(statuses: Vec<&'static str>) -> Self {
            Fake {
                docs: vec![("qnetmon/troubleshoot/ask_agent", ASK)],
                statuses: Arc::new(Mutex::new(statuses)),
                started: Arc::default(),
            }
        }
    }

    #[async_trait]
    impl McpBackend for Fake {
        async fn registry(&self, _: Option<&str>) -> Result<Option<Registry>, String> {
            Ok(None)
        }
        async fn start(&self, playbook: &str, workload: Map<String, Value>) -> Result<i64, String> {
            self.started.lock().unwrap().push((playbook.into(), workload));
            Ok(42)
        }
        async fn status(&self, id: i64) -> Result<Value, String> {
            if id != 42 {
                return Err(format!("Execution not found: {id}"));
            }
            let mut s = self.statuses.lock().unwrap();
            let cur = if s.len() > 1 { s.remove(0) } else { s[0] };
            Ok(json!({"status": cur}))
        }
        async fn detail(&self, _: i64) -> Result<Value, String> {
            Ok(json!({"path": "qnetmon/troubleshoot/ask_agent", "events": [
                {"event_type": "call.done", "node_name": "analyze", "status": "COMPLETED",
                 "result": {"context": {"data": {"answer": "AP-3 is offline", "api_key": "sk-live-123"}}}},
                {"event_type": "call.done", "node_name": "post", "status": "COMPLETED",
                 "result": {"context": {"data": {"posted": true}}}},
            ]}))
        }
        async fn cancel(&self, _: i64) -> Result<(), String> {
            Ok(())
        }
    }

    #[async_trait]
    impl AgentBackend for Fake {
        async fn agents(&self) -> Result<Vec<Result<AgentDecl, Problem>>, String> {
            Ok(self.docs.iter().filter_map(|(p, c)| agent_from_playbook(p, c)).collect())
        }
    }

    fn fast() -> WatchTiming {
        WatchTiming { poll: Duration::from_millis(2), retry_base: Duration::from_millis(2), attempts: 3 }
    }

    async fn call(app: Router, req: Request<Body>) -> (StatusCode, Value) {
        let res = app.oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    fn send_req(body: Value) -> Request<Body> {
        Request::post("/api/agents/send")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    /// A callback receiver on loopback: records (authorization, body).
    /// `/redirect` answers 307 to this receiver's own `/hook`, so a client that
    /// followed redirects would show up as a recorded delivery.
    async fn receiver(status: StatusCode) -> (String, Arc<Mutex<Vec<(String, Value)>>>) {
        let got: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
        let g = got.clone();
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let hook = format!("{base}/hook");
        let app = Router::new()
            .route(
                "/hook",
                post(move |headers: axum::http::HeaderMap, Json(v): Json<Value>| {
                    let g = g.clone();
                    async move {
                        let auth = headers.get("authorization").and_then(|h| h.to_str().ok()).unwrap_or("").to_string();
                        g.lock().unwrap().push((auth, v));
                        status
                    }
                }),
            )
            .route(
                "/redirect",
                post(move || {
                    let hook = hook.clone();
                    async move { (StatusCode::TEMPORARY_REDIRECT, [("location", hook)]) }
                }),
            );
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (base, got)
    }

    async fn wait_for<T>(f: impl Fn() -> Option<T>) -> T {
        for _ in 0..500 {
            if let Some(v) = f() {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("timed out");
    }

    #[test]
    fn flag_is_strict() {
        assert!(agent_endpoint_enabled_from(|_| Some("true".into())));
        assert!(!agent_endpoint_enabled_from(|_| Some("on".into())));
        assert!(!agent_endpoint_enabled_from(|_| None));
    }

    #[test]
    fn declaration_and_card() {
        let a = agent_from_playbook("qnetmon/troubleshoot/ask_agent", ASK).unwrap().unwrap();
        assert_eq!(a.version.as_deref(), Some("1"));
        assert_eq!(a.result_step.as_deref(), Some("analyze"));
        let c = card(&a);
        assert_eq!(c["name"], "qnetmon-troubleshooter");
        assert_eq!(c["skills"][0], json!({"id": "network-troubleshooting", "name": "network-troubleshooting"}));
        assert_eq!(c["inputSchema"]["required"], json!(["question"]));
        assert_eq!(agent_from_playbook("x", "metadata: { name: x }\nworkflow: []\n"), None);
    }

    #[test]
    fn schema_is_authored_or_borrowed_from_expose_mcp_never_workload() {
        let no_schema = "metadata:\n  name: x\n  expose:\n    agent:\n      card: { name: n, description: d }\nworkload: { q: 1 }\nworkflow: []\n";
        let e = agent_from_playbook("x", no_schema).unwrap().unwrap_err();
        assert!(e.reason.contains("not derived from `workload`"), "{}", e.reason);
        let borrowed = "metadata:\n  name: x\n  expose:\n    mcp: { tool: t, description: d, input_schema: { type: object, properties: { q: { type: string } } } }\n    agent:\n      card: { name: n, description: d }\nworkflow: []\n";
        let a = agent_from_playbook("x", borrowed).unwrap().unwrap();
        assert!(a.input_schema["properties"]["q"].is_object());
        let no_card = "metadata:\n  name: x\n  expose:\n    agent: { input_schema: { type: object } }\nworkflow: []\n";
        assert!(agent_from_playbook("x", no_card).unwrap().unwrap_err().reason.contains("card"));
    }

    #[test]
    fn callback_origins_are_an_allowlist() {
        let p = CallbackPolicy::parse("https://hooks.example.com, http://127.0.0.1:9000");
        assert!(p.check("https://hooks.example.com/a/b?x=1").is_ok());
        assert!(p.check("http://127.0.0.1:9000/hook").is_ok());
        assert!(p.check("http://hooks.example.com/a").unwrap_err().contains("not in"), "scheme is part of the origin");
        assert!(p.check("https://hooks.example.com:8443/a").is_err(), "port is part of the origin");
        assert!(p.check("http://169.254.169.254/latest").is_err());
        assert!(p.check("https://user:pw@hooks.example.com/").unwrap_err().contains("credentials"));
        assert!(p.check("file:///etc/passwd").is_err());
        assert!(CallbackPolicy::default().check("https://hooks.example.com/").unwrap_err().contains("not enabled"));
    }

    #[test]
    fn tokens_are_random_and_long() {
        let (a, b) = (new_callback_token(), new_callback_token());
        assert_ne!(a, b);
        assert_eq!(a.len(), 43);
    }

    #[tokio::test]
    async fn cards_list_and_lookup() {
        let app = routes_for(AgentState::new(Fake::new(vec!["RUNNING"]), CallbackPolicy::default(), fast()));
        let (s, v) = call(app.clone(), Request::get("/api/agents").body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["agents"][0]["agent"], "qnetmon/troubleshoot/ask_agent");
        let (s, v) = call(app.clone(), Request::get("/api/agents/cards/qnetmon/troubleshoot/ask_agent").body(Body::empty()).unwrap()).await;
        assert_eq!((s, v["name"].as_str()), (StatusCode::OK, Some("qnetmon-troubleshooter")));
        let (s, _) = call(app, Request::get("/api/agents/cards/nope").body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn send_validates_input_then_poll_returns_the_agents_result_step() {
        let fake = Fake::new(vec!["RUNNING", "COMPLETED"]);
        let app = routes_for(AgentState::new(fake.clone(), CallbackPolicy::default(), fast()));

        let (s, v) = call(app.clone(), send_req(json!({"agent": "qnetmon/troubleshoot/ask_agent", "input": {"question": "?", "endpoint": "x"}}))).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
        assert!(fake.started.lock().unwrap().is_empty());

        let (s, v) = call(app.clone(), send_req(json!({"agent": "qnetmon/troubleshoot/ask_agent", "input": {"question": "why?"}}))).await;
        assert_eq!(s, StatusCode::ACCEPTED);
        assert_eq!(v["task_id"], "42");
        assert!(v.get("callback_token").is_none(), "no callback asked, no token");

        let get = || Request::get("/api/agents/tasks/42").body(Body::empty()).unwrap();
        let (_, v) = call(app.clone(), get()).await;
        assert_eq!(v["status"], "working");
        let (_, v) = call(app.clone(), get()).await;
        assert_eq!(v["status"], "completed");
        // `analyze` (result_step), not the later `post` step; secrets redacted.
        assert_eq!(v["result"]["answer"], "AP-3 is offline");
        assert_ne!(v["result"]["api_key"], "sk-live-123");

        let (s, _) = call(app, Request::get("/api/agents/tasks/7").body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn callback_is_delivered_once_with_the_minted_token() {
        let (base, got) = receiver(StatusCode::OK).await;
        let st = AgentState::new(Fake::new(vec!["RUNNING", "RUNNING", "COMPLETED"]), CallbackPolicy::parse(&base), fast());
        let watchers = st.watchers.clone();
        let app = routes_for(st);

        let (s, v) = call(app, send_req(json!({"agent": "qnetmon/troubleshoot/ask_agent",
            "input": {"question": "why?"}, "callback_url": format!("{base}/hook")}))).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{v}");
        let token = v["callback_token"].as_str().unwrap().to_string();

        let (auth, body) = wait_for(|| got.lock().unwrap().first().cloned()).await;
        assert_eq!(auth, format!("Bearer {token}"));
        assert_eq!(body["task_id"], "42");
        assert_eq!(body["status"], "completed");
        assert_eq!(body["result"]["answer"], "AP-3 is offline");
        wait_for(|| (watchers.load(Ordering::SeqCst) == 0).then_some(())).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(got.lock().unwrap().len(), 1, "delivered exactly once");
    }

    #[tokio::test]
    async fn refused_callbacks_start_nothing() {
        let fake = Fake::new(vec!["COMPLETED"]);
        let app = routes_for(AgentState::new(fake.clone(), CallbackPolicy::parse("https://hooks.example.com"), fast()));
        let (s, v) = call(app, send_req(json!({"agent": "qnetmon/troubleshoot/ask_agent",
            "input": {"question": "?"}, "callback_url": "http://10.0.0.5/internal"}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
        assert!(fake.started.lock().unwrap().is_empty(), "a refused callback leaves no orphan execution");
    }

    #[tokio::test]
    async fn redirects_are_not_followed_and_4xx_is_not_retried() {
        let (base, got) = receiver(StatusCode::GONE).await;
        let st = AgentState::new(Fake::new(vec!["COMPLETED"]), CallbackPolicy::parse(&base), fast());
        let url = reqwest::Url::parse(&format!("{base}/redirect")).unwrap();
        st.watchers.fetch_add(1, Ordering::SeqCst);
        watch_and_deliver(&st, 42, None, url, "t".into(), Duration::from_secs(1)).await;
        assert!(got.lock().unwrap().is_empty(), "the 307 to /hook must not be followed");

        let url = reqwest::Url::parse(&format!("{base}/hook")).unwrap();
        watch_and_deliver(&st, 42, None, url, "t".into(), Duration::from_secs(1)).await;
        assert_eq!(got.lock().unwrap().len(), 1, "410 Gone is final: one attempt");
    }

    #[tokio::test]
    async fn watcher_capacity_is_enforced_before_starting() {
        let (base, _) = receiver(StatusCode::OK).await;
        let fake = Fake::new(vec!["RUNNING"]);
        let st = AgentState::new(fake.clone(), CallbackPolicy::parse(&base), fast());
        st.watchers.store(MAX_WATCHERS, Ordering::SeqCst);
        let app = routes_for(st);
        let (s, _) = call(app, send_req(json!({"agent": "qnetmon/troubleshoot/ask_agent",
            "input": {"question": "?"}, "callback_url": format!("{base}/hook")}))).await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
        assert!(fake.started.lock().unwrap().is_empty());
    }
}
