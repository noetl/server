//! **NoETL instances: register, discover, hand off work** (noetl/ai-meta#544 item 9; RFC
//! ai-meta#539 Phases 3-4).
//!
//! A NoETL *instance* is any NoETL that runs playbooks: a laptop `noetl` CLI, an edge
//! `noetl subscribe` daemon, or a whole server + worker cluster (through a sidecar). Each has
//! an ML-DSA-87 key and an id derived from it ([`identity`]). This module lets them:
//!
//! 1. **register** with a control plane, outbound, on a timer that doubles as the heartbeat;
//! 2. **discover** each other by capability, pool, site, label, playbook or liveness;
//! 3. **hand off** a playbook run to a peer, which **pulls** it from its inbox, runs it and
//!    posts the result back, so the peer needs no inbound connectivity.
//!
//! The control plane also projects who registered, what each can run and who handed work to
//! whom into ehgraph `noetl-cluster` ([`graph`]), when that writer is armed.
//!
//! # Routes
//!
//! Every instance route is a `POST` of a [`identity::SignedEnvelope`]. The signature is the
//! authentication, and the signed payload names the route's purpose, this control plane's
//! audience, a timestamp and a nonce, so a captured request cannot be replayed, re-aimed at
//! another route or sent to another control plane ([`gate`]).
//!
//! | Route | Purpose | Who |
//! |---|---|---|
//! | `POST /api/instances/register` | `register` | any trusted instance |
//! | `POST /api/instances/deregister` | `deregister` | itself |
//! | `POST /api/instances/discover` | `discover` | a live registered instance |
//! | `POST /api/instances/tasks/submit` | `task.submit` | a live registered instance |
//! | `POST /api/instances/inbox` | `inbox.claim` | the target, long-polling |
//! | `POST /api/instances/tasks/result` | `task.result` | the target |
//! | `POST /api/instances/tasks/status` | `task.status` | the submitter or the target |
//! | `GET /api/runtime/instances` | — | operators and the orchestrator, behind the internal-token gate |
//!
//! # Default OFF
//!
//! Nothing is mounted unless [`REGISTRY_ENV`] is exactly `true`. When it is, the default
//! trust admits **nobody** until their ids are listed in `NOETL_INSTANCE_TRUSTED_IDS`
//! (or `NOETL_INSTANCE_TRUST=open`, for kind and local development).
//!
//! The task board survives a restart only with `NOETL_INSTANCE_STORE=postgres` ([`store`]).

// The handlers return an early `Response` as their error, which is what axum hands back
// anyway; boxing it would only add an allocation per refusal.
#![allow(clippy::result_large_err)]

pub mod gate;
pub mod graph;
pub mod identity;
pub mod registry;
pub mod store;
pub mod tasks;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Query as QueryParams, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Notify;

use crate::handlers::agent_endpoint::{
    deliver_callback, new_callback_token, CallbackPolicy, WatchTiming,
};
use gate::{Admitted, Gate, Refusal};
use graph::GraphSink;
use identity::SignedEnvelope;
use registry::{Capabilities, Query, Registry, Upsert};
use tasks::{Board, Completion, NewTask, PlaybookRef};

/// Mounts the instance routes. Strict `== "true"`.
pub const REGISTRY_ENV: &str = "NOETL_INSTANCE_REGISTRY";
/// Longest inbox long-poll the server will hold open.
pub const MAX_WAIT_MS: u64 = 25_000;
/// How often expiries, forgetting and the graph sweep run.
pub const SWEEP_EVERY: Duration = Duration::from_secs(15);

pub fn registry_enabled() -> bool {
    std::env::var(REGISTRY_ENV).ok().as_deref() == Some("true")
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Everything the routes share. Cheap to clone.
#[derive(Clone)]
pub struct InstanceState {
    pub gate: Arc<Gate>,
    pub registry: Arc<Mutex<Registry>>,
    pub board: Arc<Mutex<Board>>,
    wakers: Arc<Mutex<HashMap<String, Arc<Notify>>>>,
    pub graph: Option<(Arc<dyn GraphSink>, String)>,
    /// This control plane's id in the graph.
    pub control_id: String,
    pub callbacks: CallbackPolicy,
    pub timing: WatchTiming,
    pub http: reqwest::Client,
    /// Overridable clock, for tests.
    pub clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// The durable inbox, when `NOETL_INSTANCE_STORE=postgres`.
    pub store: Option<Arc<dyn store::TaskStore>>,
}

impl InstanceState {
    pub fn new(gate: Gate, registry: Registry, board: Board) -> Self {
        InstanceState {
            gate: Arc::new(gate),
            registry: Arc::new(Mutex::new(registry)),
            board: Arc::new(Mutex::new(board)),
            wakers: Arc::default(),
            graph: None,
            control_id: crate::runtime_registry::self_id(),
            callbacks: CallbackPolicy::default(),
            timing: WatchTiming::default(),
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_default(),
            clock: Arc::new(now_ms),
            store: None,
        }
    }

    /// Write the board's changes through to the durable store. True when there is no store,
    /// nothing changed, or the write landed; on failure the changes are kept for the next
    /// write.
    pub async fn flush(&self) -> bool {
        let Some(store) = self.store.clone() else {
            return true;
        };
        let changes = self
            .board
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take_changes();
        if changes.is_empty() {
            return true;
        }
        match store.write(&changes).await {
            Ok(()) => true,
            Err(e) => {
                crate::metrics::record_instance("store_failed");
                tracing::warn!(error = %e, "instance task store write failed; retrying with the next write");
                self.board
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .retry_changes(&changes);
                false
            }
        }
    }

    /// Rebuild the board from the durable store. Returns how many tasks came back.
    pub async fn restore(&self) -> Result<usize, String> {
        let Some(store) = self.store.clone() else {
            return Ok(0);
        };
        let rows = store.load().await?;
        let n = self
            .board
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .restore(rows);
        if n > 0 {
            crate::metrics::instance_total()
                .with_label_values(&["store_restored"])
                .inc_by(n as u64);
        }
        Ok(n)
    }

    pub fn from_env() -> Self {
        let mut s = InstanceState::new(
            Gate::from_env(),
            Registry::from_env(),
            Board::new(tasks::DEFAULT_VISIBILITY_MS),
        );
        s.callbacks = CallbackPolicy::from_env();
        s.graph = graph::writer_from_env().map(|(sink, ns)| (Arc::from(sink), ns));
        s
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    fn waker(&self, id: &str) -> Arc<Notify> {
        let mut w = self.wakers.lock().unwrap_or_else(|p| p.into_inner());
        w.entry(id.to_string()).or_default().clone()
    }

    fn admit(&self, env: &SignedEnvelope, purpose: &str) -> Result<Admitted, Response> {
        self.gate.admit(env, purpose, self.now()).map_err(|r| {
            crate::metrics::record_instance("refused");
            tracing::info!(purpose, status = r.status, reason = %r.reason, "instance request refused");
            refusal(r)
        })
    }

    /// Admit, and require the caller to hold a live registration.
    fn admit_live(&self, env: &SignedEnvelope, purpose: &str) -> Result<Admitted, Response> {
        let a = self.admit(env, purpose)?;
        let live = self
            .registry
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_live(&a.instance_id, self.now());
        if !live {
            return Err(error(
                StatusCode::FORBIDDEN,
                "register (and keep heartbeating) before using this route",
            ));
        }
        Ok(a)
    }

    fn write_graph(&self, batches: Vec<Value>) {
        let Some((sink, _)) = self.graph.clone() else {
            return;
        };
        tokio::spawn(async move {
            for b in batches {
                match sink.call("graph.upsert", b).await {
                    Ok(_) => crate::metrics::record_instance("graph_written"),
                    Err(e) => {
                        crate::metrics::record_instance("graph_failed");
                        tracing::warn!(error = %e, "instance graph write failed; the registry is unaffected");
                    }
                }
            }
        });
    }

    /// One sweep: expire overdue tasks (and send their callbacks), forget long-gone instances,
    /// and deactivate lapsed ones in the graph.
    pub async fn sweep_once(&self) {
        let now = self.now();
        let expired = self
            .board
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .expire(now);
        for t in expired {
            crate::metrics::record_instance("task_expired");
            self.send_callback(&t.task_id).await;
        }
        self.board
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .prune(now);
        self.flush().await;
        let ttl = {
            let mut r = self.registry.lock().unwrap_or_else(|p| p.into_inner());
            r.forget_expired(now);
            r.ttl_ms
        };
        if let Some((sink, ns)) = &self.graph {
            if let Err(e) = sink
                .call("graph.deactivate", graph::sweep(ns, now - ttl, now))
                .await
            {
                if sweep_found_nothing(&e) {
                    // The namespace appears with the first registration's projection; until
                    // then there is nothing to deactivate.
                    tracing::debug!("instance graph sweep: no topology namespace yet");
                } else {
                    tracing::warn!(error = %e, "instance graph sweep failed");
                }
            }
        }
    }

    async fn send_callback(&self, task_id: &str) {
        let due = self
            .board
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take_reply(task_id);
        if let Some(((url, token), view)) = due {
            let ok = deliver_callback(&self.http, &url, &token, task_id, &view, &self.timing).await;
            crate::metrics::record_instance(if ok {
                "callback_delivered"
            } else {
                "callback_failed"
            });
        }
    }
}

/// A sweep error that only means "nothing has been written here yet": ehgraph answers
/// `NOT_FOUND` for a namespace no upsert has created. Not a failure, so not a WARN every
/// sweep (2026-11-10: prod logged one per 15 s while no instance had registered).
fn sweep_found_nothing(e: &str) -> bool {
    serde_json::from_str::<Value>(e).is_ok_and(|v| {
        v["code"] == "NOT_FOUND"
            && v["message"]
                .as_str()
                .is_some_and(|m| m.starts_with("namespace ") && m.ends_with(" not found"))
    })
}

#[cfg(test)]
mod sweep_tests {
    use super::sweep_found_nothing;

    #[test]
    fn a_missing_namespace_is_not_a_sweep_failure_but_other_errors_are() {
        assert!(sweep_found_nothing(
            r#"{"code":"NOT_FOUND","message":"namespace topology not found"}"#
        ));
        assert!(!sweep_found_nothing(
            r#"{"code":"NOT_FOUND","message":"node instance:x not found"}"#
        ));
        assert!(!sweep_found_nothing(
            r#"{"code":"UNAUTHORIZED","message":"unknown credential"}"#
        ));
        assert!(!sweep_found_nothing("connection reset"));
    }
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": message.into()}))).into_response()
}

fn refusal(r: Refusal) -> Response {
    error(
        StatusCode::from_u16(r.status).unwrap_or(StatusCode::BAD_REQUEST),
        r.reason,
    )
}

fn body<T: for<'de> Deserialize<'de>>(a: &Admitted) -> Result<T, Response> {
    let v = if a.body.is_null() {
        json!({})
    } else {
        a.body.clone()
    };
    serde_json::from_value(v).map_err(|e| error(StatusCode::BAD_REQUEST, format!("body: {e}")))
}

macro_rules! tri {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(r) => return r,
        }
    };
}

async fn register(State(s): State<InstanceState>, Json(env): Json<SignedEnvelope>) -> Response {
    let a = tri!(s.admit(&env, "register"));
    let caps: Capabilities = tri!(body(&a));
    let now = s.now();
    let (outcome, rec, live_peers, ttl) = {
        let mut r = s.registry.lock().unwrap_or_else(|p| p.into_inner());
        let outcome = match r.upsert(&a.instance_id, &a.public_key_b64, caps, now) {
            Ok(o) => o,
            Err(e) => return error(StatusCode::BAD_REQUEST, e),
        };
        let rec = r.get(&a.instance_id).cloned().expect("just upserted");
        let live: Vec<String> = rec
            .capabilities
            .known_peers
            .iter()
            .filter(|p| r.is_live(p, now))
            .cloned()
            .collect();
        (outcome, rec, live, r.ttl_ms)
    };
    crate::metrics::record_instance(match outcome {
        Upsert::Registered => "registered",
        Upsert::Renewed => "renewed",
    });
    if outcome == Upsert::Registered {
        tracing::info!(instance_id = %a.instance_id, kind = ?rec.capabilities.runtime_kind,
            version = %rec.capabilities.version, "instance registered");
    }
    if let Some((_, ns)) = &s.graph {
        s.write_graph(graph::projection(ns, &s.control_id, &rec, &live_peers, now));
    }
    Json(json!({
        "instance_id": a.instance_id,
        "registered": outcome == Upsert::Registered,
        "renewed": outcome == Upsert::Renewed,
        "ttl_secs": ttl / 1000,
        // Renew at a third of the lease, the D8 rule, so two missed beats still leave margin.
        "heartbeat_secs": (ttl / 3000).max(1),
        "control_plane": s.control_id,
    }))
    .into_response()
}

async fn deregister(State(s): State<InstanceState>, Json(env): Json<SignedEnvelope>) -> Response {
    let a = tri!(s.admit(&env, "deregister"));
    let gone = s
        .registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .deregister(&a.instance_id);
    if gone {
        crate::metrics::record_instance("deregistered");
    }
    Json(json!({"instance_id": a.instance_id, "deregistered": gone})).into_response()
}

fn discovery(s: &InstanceState, q: &Query) -> Value {
    let found = s
        .registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .query(q, s.now());
    crate::metrics::record_instance("discover");
    json!({"count": found.len(), "instances": found})
}

async fn discover(State(s): State<InstanceState>, Json(env): Json<SignedEnvelope>) -> Response {
    let a = tri!(s.admit_live(&env, "discover"));
    let q: Query = tri!(body(&a));
    Json(discovery(&s, &q)).into_response()
}

/// `GET /api/runtime/instances?tool_kind=shell&label=site=home&playbook=edge/probe&...`
async fn list(
    State(s): State<InstanceState>,
    QueryParams(pairs): QueryParams<Vec<(String, String)>>,
) -> Response {
    let mut q = Query::default();
    for (k, v) in pairs {
        match k.as_str() {
            "runtime_kind" => match serde_json::from_value(json!(v)) {
                Ok(kind) => q.runtime_kind = Some(kind),
                Err(_) => {
                    return error(
                        StatusCode::BAD_REQUEST,
                        "runtime_kind: cli | daemon | distributed",
                    )
                }
            },
            "tool_kind" => q.tool_kind.push(v),
            "pool" => q.pool = Some(v),
            "site" => q.site = Some(v),
            "label" => q.label.push(v),
            "playbook" => q.playbook = Some(v),
            "accepts_tasks" => q.accepts_tasks = Some(v == "true"),
            "include_stale" => q.include_stale = v == "true",
            other => return error(StatusCode::BAD_REQUEST, format!("unknown filter `{other}`")),
        }
    }
    Json(discovery(&s, &q)).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubmitBody {
    target: Target,
    playbook: PlaybookRef,
    #[serde(default)]
    payload: Value,
    #[serde(default)]
    deadline_ms: Option<i64>,
    #[serde(default)]
    callback_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Target {
    InstanceId(String),
    /// The freshest live instance matching the query that accepts tasks and lists the playbook.
    Selector(Query),
}

async fn submit(State(s): State<InstanceState>, Json(env): Json<SignedEnvelope>) -> Response {
    let a = tri!(s.admit_live(&env, "task.submit"));
    let b: SubmitBody = tri!(body(&a));
    let now = s.now();
    let refused = |code: StatusCode, m: String| {
        crate::metrics::record_instance("task_refused");
        error(code, m)
    };
    let to = {
        let r = s.registry.lock().unwrap_or_else(|p| p.into_inner());
        let ok_for = |id: &str| -> Result<(), String> {
            let rec = r
                .get(id)
                .filter(|_| r.is_live(id, now))
                .ok_or(format!("{id} is not a live instance"))?;
            if !rec.capabilities.accepts_tasks {
                return Err(format!("{id} does not accept tasks"));
            }
            if !rec
                .capabilities
                .playbooks
                .iter()
                .any(|g| registry::glob_match(g, &b.playbook.path))
            {
                return Err(format!("{id} does not run `{}`", b.playbook.path));
            }
            Ok(())
        };
        match &b.target {
            Target::InstanceId(id) => match ok_for(id) {
                Ok(()) => id.clone(),
                Err(e) => return refused(StatusCode::CONFLICT, e),
            },
            Target::Selector(q) => {
                let mut q = q.clone();
                q.include_stale = false;
                q.accepts_tasks = Some(true);
                q.playbook = Some(b.playbook.path.clone());
                match r
                    .query(&q, now)
                    .into_iter()
                    .find(|f| f.record.instance_id != a.instance_id)
                {
                    Some(f) => f.record.instance_id,
                    None => {
                        return refused(
                            StatusCode::CONFLICT,
                            "no live instance matches the selector".into(),
                        )
                    }
                }
            }
        }
    };
    let reply = match &b.callback_url {
        None => None,
        Some(u) => match s.callbacks.check(u) {
            Ok(url) => Some((url, new_callback_token())),
            Err(e) => return refused(StatusCode::BAD_REQUEST, e),
        },
    };
    let token = reply.as_ref().map(|(_, t)| t.clone());
    let submitted = s.board.lock().unwrap_or_else(|p| p.into_inner()).submit(
        NewTask {
            from: a.instance_id.clone(),
            to: to.clone(),
            playbook: b.playbook.clone(),
            payload: b.payload,
            deadline_ms: b.deadline_ms,
            reply,
        },
        now,
    );
    let task_id = match submitted {
        Ok(id) => id,
        Err(e) => {
            return refused(
                StatusCode::from_u16(e.status).unwrap_or(StatusCode::BAD_REQUEST),
                e.reason,
            )
        }
    };
    if !s.flush().await {
        // Not durable, so not accepted: drop it and let the submitter retry.
        s.board
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .discard(&task_id);
        return refused(
            StatusCode::SERVICE_UNAVAILABLE,
            "the task could not be stored; retry".into(),
        );
    }
    crate::metrics::record_instance("task_submitted");
    tracing::info!(%task_id, from = %a.instance_id, %to, playbook = %b.playbook.path, "instance task queued");
    s.waker(&to).notify_waiters();
    if let Some((_, ns)) = &s.graph {
        s.write_graph(vec![graph::delegation(
            ns,
            &a.instance_id,
            &to,
            &task_id,
            &b.playbook.path,
            now,
        )]);
    }
    let mut reply = json!({"task_id": task_id, "to": to, "state": "queued"});
    if let Some(t) = token {
        // The bearer the callback will carry, so the submitter can tell it from a forgery.
        reply["callback_token"] = json!(t);
    }
    (StatusCode::ACCEPTED, Json(reply)).into_response()
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ClaimBody {
    #[serde(default)]
    max: Option<usize>,
    #[serde(default)]
    wait_ms: Option<u64>,
}

async fn inbox(State(s): State<InstanceState>, Json(env): Json<SignedEnvelope>) -> Response {
    let a = tri!(s.admit_live(&env, "inbox.claim"));
    let b: ClaimBody = tri!(body(&a));
    let max = b.max.unwrap_or(1).clamp(1, 16);
    let wait = Duration::from_millis(b.wait_ms.unwrap_or(0).min(MAX_WAIT_MS));
    let until = tokio::time::Instant::now() + wait;
    let waker = s.waker(&a.instance_id);
    loop {
        // Arm the wake-up before looking, so a submit between the look and the wait is not lost.
        let notified = waker.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let claimed =
            s.board
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .claim(&a.instance_id, max, s.now());
        if !claimed.is_empty() || tokio::time::Instant::now() >= until {
            if !claimed.is_empty() {
                s.flush().await;
            }
            for _ in &claimed {
                crate::metrics::record_instance("task_claimed");
            }
            return Json(json!({"tasks": claimed})).into_response();
        }
        // Wake on a submit, at the deadline, or every few seconds to pick up a lapsed lease.
        let tick = (until - tokio::time::Instant::now()).min(Duration::from_secs(5));
        let _ = tokio::time::timeout(tick, notified).await;
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResultBody {
    task_id: String,
    claim_id: String,
    state: String,
    #[serde(default)]
    result: Value,
    #[serde(default)]
    error: Option<String>,
}

async fn result(State(s): State<InstanceState>, Json(env): Json<SignedEnvelope>) -> Response {
    let a = tri!(s.admit(&env, "task.result"));
    let b: ResultBody = tri!(body(&a));
    let outcome = match b.state.as_str() {
        "completed" => Ok(b.result),
        "failed" => Err(b
            .error
            .unwrap_or_else(|| "the target reported failure".into())),
        other => {
            return error(
                StatusCode::BAD_REQUEST,
                format!("state `{other}`: completed | failed"),
            )
        }
    };
    let failed = outcome.is_err();
    let done = s.board.lock().unwrap_or_else(|p| p.into_inner()).complete(
        &a.instance_id,
        &b.task_id,
        &b.claim_id,
        outcome,
        s.now(),
    );
    match done {
        Ok(Completion::Recorded) => {
            s.flush().await;
            crate::metrics::record_instance(if failed {
                "task_failed"
            } else {
                "task_completed"
            });
            let s2 = s.clone();
            let id = b.task_id.clone();
            tokio::spawn(async move { s2.send_callback(&id).await });
            Json(json!({"task_id": b.task_id, "recorded": true})).into_response()
        }
        Ok(c) => {
            crate::metrics::record_instance("task_stale");
            let why = if c == Completion::Stale {
                "stale"
            } else {
                "duplicate"
            };
            (
                StatusCode::ACCEPTED,
                Json(json!({"task_id": b.task_id, "recorded": false, "reason": why})),
            )
                .into_response()
        }
        Err(e) => error(
            StatusCode::from_u16(e.status).unwrap_or(StatusCode::NOT_FOUND),
            e.reason,
        ),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusBody {
    task_id: String,
}

async fn status(State(s): State<InstanceState>, Json(env): Json<SignedEnvelope>) -> Response {
    let a = tri!(s.admit(&env, "task.status"));
    let b: StatusBody = tri!(body(&a));
    let board = s.board.lock().unwrap_or_else(|p| p.into_inner());
    match board.get(&a.instance_id, &b.task_id) {
        Ok(t) => Json(t.view()).into_response(),
        Err(e) => error(StatusCode::NOT_FOUND, e.reason),
    }
}

/// The signed instance routes. Their own authentication; see the module docs.
pub fn routes_for(state: InstanceState) -> Router {
    Router::new()
        .route("/api/instances/register", post(register))
        .route("/api/instances/deregister", post(deregister))
        .route("/api/instances/discover", post(discover))
        .route("/api/instances/tasks/submit", post(submit))
        .route("/api/instances/inbox", post(inbox))
        .route("/api/instances/tasks/result", post(result))
        .route("/api/instances/tasks/status", post(status))
        .with_state(state)
}

/// The operator listing. Enumerates instances, so it goes behind the internal-token gate.
pub fn admin_routes_for(state: InstanceState) -> Router {
    Router::new()
        .route("/api/runtime/instances", get(list))
        .with_state(state)
}

/// Spawn the periodic sweep. Needs a tokio runtime.
pub fn spawn_sweeper(state: InstanceState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_EVERY);
        loop {
            tick.tick().await;
            state.sweep_once().await;
        }
    });
}

/// `(signed routes, gated routes)` when [`REGISTRY_ENV`] is on, else two empty routers.
/// With `NOETL_INSTANCE_STORE=postgres` the board is restored from `noetl.instance_tasks`
/// before the routes are returned, so the first claim already sees the old tasks. Needs the
/// multi-threaded runtime (the server's `#[tokio::main]`).
pub fn routes(pool: &crate::db::DbPool) -> (Router, Router) {
    if !registry_enabled() {
        return (Router::new(), Router::new());
    }
    let mut state = InstanceState::from_env();
    if store::postgres_store_enabled() {
        let pool = pool.clone();
        let opened = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(store::PgTaskStore::new(pool))
        });
        match opened {
            Ok(st) => {
                state.store = Some(Arc::new(st));
                let s2 = state.clone();
                match tokio::task::block_in_place(|| {
                    tokio::runtime::Handle::current().block_on(s2.restore())
                }) {
                    Ok(n) => tracing::info!(
                        restored = n,
                        "instance inbox: durable (noetl.instance_tasks)"
                    ),
                    Err(e) => {
                        tracing::error!(error = %e, "instance inbox: could not read noetl.instance_tasks; starting empty, writes continue")
                    }
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "instance inbox: NOETL_INSTANCE_STORE=postgres but the table could not be prepared; the inbox is IN MEMORY")
            }
        }
    }
    if matches!(state.gate.trust, gate::Trust::Open) {
        tracing::warn!("NOETL_INSTANCE_TRUST=open: any instance with a valid key can register. Development only.");
    }
    spawn_sweeper(state.clone());
    tracing::info!(
        graph = state.graph.is_some(),
        "instance registry serving at /api/instances"
    );
    (routes_for(state.clone()), admin_routes_for(state))
}
