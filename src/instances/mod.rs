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
//! `NOETL_INSTANCE_TRUST=enroll` adds one-time enrollment tokens to the allowlist ([`enroll`]).
//! `NOETL_INSTANCE_PUSH=true` pushes new tasks to `reach: push` instances ([`InstanceState::push_task`]).

// The handlers return an early `Response` as their error, which is what axum hands back
// anyway; boxing it would only add an allocation per refusal.
#![allow(clippy::result_large_err)]

pub mod enroll;
pub mod gate;
pub mod graph;
pub mod identity;
pub mod registry;
pub mod store;
pub mod tasks;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, Query as QueryParams, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
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
/// Push delivery to `reach: push` instances. Strict `== "true"`.
pub const PUSH_ENV: &str = "NOETL_INSTANCE_PUSH";
/// Origins (`scheme://host[:port]`, comma-separated) a push may go to. Empty = none.
pub const PUSH_ORIGINS_ENV: &str = "NOETL_INSTANCE_PUSH_ORIGINS";
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
    /// Enrollment tokens and enrolled ids, when `NOETL_INSTANCE_TRUST=enroll`.
    pub enrollments: Option<Arc<dyn enroll::EnrollStore>>,
    /// Push delivery: the origins a push may reach. `None` = push off.
    pub push: Option<CallbackPolicy>,
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
            enrollments: None,
            push: None,
        }
    }

    /// Push one queued task to its `reach: push` target. Best effort: the task is claimed,
    /// POSTed to the target's endpoint with its push bearer, and released back to the inbox
    /// if the target does not answer 2xx. The target's result arrives the usual way.
    pub async fn push_task(&self, to: &str, task_id: &str) {
        let Some(policy) = &self.push else {
            return;
        };
        let target = {
            let r = self.registry.lock().unwrap_or_else(|p| p.into_inner());
            r.get(to).and_then(|rec| {
                let ep = rec.capabilities.endpoint.clone()?;
                let tok = rec.push_token.clone()?;
                (rec.capabilities.reach == registry::Reach::Push).then_some((ep, tok))
            })
        };
        let Some((endpoint, token)) = target else {
            return;
        };
        let url = match policy.check(&endpoint) {
            Ok(u) => u,
            Err(e) => {
                tracing::info!(instance_id = %to, error = %e, "push skipped: endpoint origin not allowed; the task waits in the inbox");
                return;
            }
        };
        let claimed = self
            .board
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .claim_one(to, task_id, self.now());
        let Some(c) = claimed else {
            return; // pulled already
        };
        self.flush().await;
        let sent = self
            .http
            .post(url)
            .bearer_auth(&token)
            .json(&json!({"tasks": [c]}))
            .send()
            .await;
        match sent {
            Ok(r) if r.status().is_success() => {
                crate::metrics::record_instance("task_pushed");
                tracing::info!(%task_id, instance_id = %to, "instance task pushed");
            }
            other => {
                let why = match other {
                    Ok(r) => format!("HTTP {}", r.status()),
                    Err(e) => e.to_string(),
                };
                crate::metrics::record_instance("push_failed");
                tracing::info!(%task_id, instance_id = %to, error = %why, "push failed; the task is back in the inbox");
                self.board
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .release(task_id, &c.claim_id);
                self.flush().await;
                self.waker(to).notify_waiters();
            }
        }
    }

    /// Load enrolled ids into the gate (start-up).
    pub async fn load_enrolled(&self) -> Result<usize, String> {
        let Some(e) = self.enrollments.clone() else {
            return Ok(0);
        };
        let all = e.list().await?;
        for x in &all {
            self.gate.enroll_id(&x.instance_id);
        }
        Ok(all.len())
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
        if std::env::var(PUSH_ENV).ok().as_deref() == Some("true") {
            s.push = Some(CallbackPolicy::parse(
                &std::env::var(PUSH_ORIGINS_ENV).unwrap_or_default(),
            ));
        }
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
                tracing::warn!(error = %e, "instance graph sweep failed");
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
        // Only for a `reach: push` registration while push is on: the bearer every push to
        // this instance carries. Returned to the instance itself, never in discovery.
        "push_token": if s.push.is_some() { rec.push_token.clone() } else { None },
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
    if s.push.is_some() {
        let (s2, to2, id2) = (s.clone(), to.clone(), task_id.clone());
        tokio::spawn(async move { s2.push_task(&to2, &id2).await });
    }
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
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnrollBody {
    token: String,
}

/// `POST /api/instances/enroll`: an untrusted instance redeems a token for its own key.
async fn enroll_route(State(s): State<InstanceState>, Json(env): Json<SignedEnvelope>) -> Response {
    let Some(book) = s.enrollments.clone() else {
        return error(
            StatusCode::FORBIDDEN,
            "enrollment is off here (NOETL_INSTANCE_TRUST is not `enroll`)",
        );
    };
    let a = match s.gate.admit_untrusted(&env, "enroll", s.now()) {
        Ok(a) => a,
        Err(r) => {
            crate::metrics::record_instance("refused");
            return refusal(r);
        }
    };
    let b: EnrollBody = tri!(body(&a));
    let label = match book
        .spend(&enroll::token_hash(b.token.trim()), s.now())
        .await
    {
        Ok(Some(l)) => l,
        Ok(None) => {
            crate::metrics::record_instance("refused");
            tracing::info!(instance_id = %a.instance_id, "enrollment refused: token invalid, expired or spent");
            return error(
                StatusCode::FORBIDDEN,
                "the enrollment token is invalid, expired or already used",
            );
        }
        Err(e) => {
            tracing::warn!(error = %e, "enrollment store failed");
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "enrollment is unavailable; retry",
            );
        }
    };
    let rec = enroll::Enrolled {
        instance_id: a.instance_id.clone(),
        label: label.clone(),
        enrolled_ms: s.now(),
    };
    if let Err(e) = book.add(&rec).await {
        tracing::warn!(error = %e, "enrollment store failed after spending the token");
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "enrollment could not be recorded; ask for a new token",
        );
    }
    s.gate.enroll_id(&a.instance_id);
    crate::metrics::record_instance("enrolled");
    tracing::info!(instance_id = %a.instance_id, %label, "instance enrolled");
    Json(json!({"instance_id": a.instance_id, "enrolled": true, "label": label})).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MintBody {
    label: String,
    #[serde(default)]
    ttl_secs: Option<i64>,
    #[serde(default)]
    uses: Option<i64>,
}

/// `POST /api/runtime/instances/enrollments` (internal-token gate): mint a token.
async fn mint_route(State(s): State<InstanceState>, Json(b): Json<MintBody>) -> Response {
    let Some(book) = s.enrollments.clone() else {
        return error(
            StatusCode::CONFLICT,
            "enrollment is off here (NOETL_INSTANCE_TRUST is not `enroll`)",
        );
    };
    match enroll::mint(book.as_ref(), &b.label, b.ttl_secs, b.uses, s.now()).await {
        Ok(m) => {
            tracing::info!(label = %m.label, uses = m.uses, expires_ms = m.expires_ms, "enrollment token minted");
            (StatusCode::CREATED, Json(json!(m))).into_response()
        }
        Err(e) => error(StatusCode::BAD_REQUEST, e),
    }
}

/// `GET /api/runtime/instances/enrolled` (internal-token gate).
async fn enrolled_route(State(s): State<InstanceState>) -> Response {
    let Some(book) = s.enrollments.clone() else {
        return Json(json!({"enroll": false, "enrolled": []})).into_response();
    };
    match book.list().await {
        Ok(v) => Json(json!({"enroll": true, "enrolled": v})).into_response(),
        Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, e),
    }
}

/// `DELETE /api/runtime/instances/enrolled/{id}` (internal-token gate): revoke.
async fn revoke_route(State(s): State<InstanceState>, Path(id): Path<String>) -> Response {
    let Some(book) = s.enrollments.clone() else {
        return error(StatusCode::CONFLICT, "enrollment is off here");
    };
    let stored = match book.remove(&id).await {
        Ok(b) => b,
        Err(e) => return error(StatusCode::SERVICE_UNAVAILABLE, e),
    };
    let live = s.gate.revoke_id(&id);
    s.registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .deregister(&id);
    if !(stored || live) {
        return error(StatusCode::NOT_FOUND, "not enrolled");
    }
    crate::metrics::record_instance("revoked");
    tracing::info!(instance_id = %id, "instance enrollment revoked");
    Json(json!({"instance_id": id, "revoked": true})).into_response()
}

pub fn routes_for(state: InstanceState) -> Router {
    Router::new()
        .route("/api/instances/register", post(register))
        .route("/api/instances/deregister", post(deregister))
        .route("/api/instances/discover", post(discover))
        .route("/api/instances/tasks/submit", post(submit))
        .route("/api/instances/inbox", post(inbox))
        .route("/api/instances/tasks/result", post(result))
        .route("/api/instances/tasks/status", post(status))
        .route("/api/instances/enroll", post(enroll_route))
        .with_state(state)
}

/// The operator listing. Enumerates instances, so it goes behind the internal-token gate.
pub fn admin_routes_for(state: InstanceState) -> Router {
    Router::new()
        .route("/api/runtime/instances", get(list))
        .route("/api/runtime/instances/enrollments", post(mint_route))
        .route("/api/runtime/instances/enrolled", get(enrolled_route))
        .route("/api/runtime/instances/enrolled/{id}", delete(revoke_route))
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
    if state.gate.enroll {
        let book: Arc<dyn enroll::EnrollStore> = if store::postgres_store_enabled() {
            let pool = pool.clone();
            match tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(enroll::PgEnroll::new(pool))
            }) {
                Ok(p) => Arc::new(p),
                Err(e) => {
                    tracing::error!(error = %e, "instance enrollment: tables could not be prepared; enrollments are IN MEMORY");
                    Arc::new(enroll::MemEnroll::default())
                }
            }
        } else {
            Arc::new(enroll::MemEnroll::default())
        };
        state.enrollments = Some(book);
        let s2 = state.clone();
        match tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(s2.load_enrolled())
        }) {
            Ok(n) => tracing::info!(
                enrolled = n,
                "instance enrollment on (NOETL_INSTANCE_TRUST=enroll)"
            ),
            Err(e) => {
                tracing::error!(error = %e, "instance enrollment: could not load enrolled ids")
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
