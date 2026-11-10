//! Push delivery (build plan item 9, `NOETL_INSTANCE_PUSH=true`): a `reach: push` instance
//! gets new tasks POSTed to its endpoint with a per-registration bearer, posts the result the
//! usual signed way, and a failed or disallowed push leaves the task in the pull inbox.
//! Real router and a real receiver on TCP, real ML-DSA-87 envelopes.

use std::sync::atomic::{AtomicI64, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::routing::post;
use axum::Router;
use noetl_server::handlers::agent_endpoint::CallbackPolicy;
use noetl_server::instances::gate::{payload, Gate, Trust};
use noetl_server::instances::identity::InstanceKey;
use noetl_server::instances::registry::Registry;
use noetl_server::instances::tasks::Board;
use noetl_server::instances::{admin_routes_for, routes_for, InstanceState};
use serde_json::{json, Value};

const AUDIENCE: &str = "noetl";
type Got = Arc<Mutex<Vec<(Option<String>, Value)>>>;

async fn serve(app: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    base
}

/// A push receiver standing in for instance B's endpoint. Answers `status`.
async fn receiver(status: Arc<AtomicU16>) -> (String, Got) {
    let got: Got = Arc::default();
    let sink = got.clone();
    let app = Router::new().route(
        "/push",
        post(
            move |h: axum::http::HeaderMap, axum::Json(v): axum::Json<Value>| {
                let (sink, status) = (sink.clone(), status.clone());
                async move {
                    let auth = h
                        .get("authorization")
                        .and_then(|x| x.to_str().ok())
                        .map(str::to_string);
                    sink.lock().unwrap().push((auth, v));
                    axum::http::StatusCode::from_u16(status.load(Ordering::SeqCst)).unwrap()
                }
            },
        ),
    );
    (serve(app).await, got)
}

struct Cp {
    base: String,
    http: reqwest::Client,
    clock: Arc<AtomicI64>,
}

async fn start(keys: &[&InstanceKey], push_origins: Option<&str>) -> Cp {
    let clock = Arc::new(AtomicI64::new(noetl_server::instances::now_ms()));
    let ids = keys.iter().map(|k| k.id().to_string()).collect();
    let mut state = InstanceState::new(
        Gate::new(Trust::Allowlist(ids), AUDIENCE, 120_000),
        Registry::new(60_000, 100),
        Board::new(30_000),
    );
    let c = clock.clone();
    state.clock = Arc::new(move || c.load(Ordering::SeqCst));
    state.push = push_origins.map(CallbackPolicy::parse);
    let base = serve(routes_for(state.clone()).merge(admin_routes_for(state))).await;
    Cp {
        base,
        http: reqwest::Client::new(),
        clock,
    }
}

impl Cp {
    async fn call(&self, k: &InstanceKey, route: &str, purpose: &str, body: Value) -> (u16, Value) {
        let env = k.sign(payload(
            purpose,
            k.id(),
            AUDIENCE,
            self.clock.load(Ordering::SeqCst),
            &noetl_server::instances::tasks::new_id("n"),
            body,
        ));
        let r = self
            .http
            .post(format!("{}{route}", self.base))
            .json(&env)
            .send()
            .await
            .unwrap();
        let s = r.status().as_u16();
        (s, r.json().await.unwrap_or(Value::Null))
    }
    async fn register(&self, k: &InstanceKey, push_endpoint: Option<&str>) -> Value {
        let mut caps = json!({"runtime_kind": "daemon", "version": "noetl-cli/6.0.0", "site": "home",
            "playbooks": ["edge/*"], "accepts_tasks": push_endpoint.is_some()});
        if let Some(e) = push_endpoint {
            caps["reach"] = json!("push");
            caps["endpoint"] = json!(e);
        }
        let (s, v) = self
            .call(k, "/api/instances/register", "register", caps)
            .await;
        assert_eq!(s, 200, "{v}");
        v
    }
    async fn submit(&self, a: &InstanceKey, b: &InstanceKey) -> String {
        let (s, v) = self
            .call(
                a,
                "/api/instances/tasks/submit",
                "task.submit",
                json!({"target": {"instance_id": b.id()}, "playbook": {"path": "edge/hello"}, "payload": {"who": "push"}}),
            )
            .await;
        assert_eq!(s, 202, "{v}");
        v["task_id"].as_str().unwrap().to_string()
    }
}

async fn wait_for(got: &Got, n: usize) {
    for _ in 0..100 {
        if got.lock().unwrap().len() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_push_instance_gets_the_task_and_answers_the_usual_way() {
    let a = InstanceKey::from_seed([50; 32]);
    let b = InstanceKey::from_seed([51; 32]);
    let (rx, got) = receiver(Arc::new(AtomicU16::new(200))).await;
    let cp = start(&[&a, &b], Some(&rx)).await;
    cp.register(&a, None).await;
    let reg = cp.register(&b, Some(&format!("{rx}/push"))).await;
    let push_token = reg["push_token"].as_str().expect("issued").to_string();
    let reg2 = cp.register(&b, Some(&format!("{rx}/push"))).await;
    assert_eq!(
        reg2["push_token"],
        push_token.as_str(),
        "stable across heartbeats"
    );

    // Discovery never shows the push bearer.
    let (_, found) = cp
        .call(
            &a,
            "/api/instances/discover",
            "discover",
            json!({"playbook": "edge/hello", "accepts_tasks": true}),
        )
        .await;
    assert_eq!(found["count"], 1, "{found}");
    assert_eq!(found["instances"][0]["reach"], "push");
    assert!(!found.to_string().contains(&push_token), "{found}");

    let task_id = cp.submit(&a, &b).await;
    wait_for(&got, 1).await;
    let (auth, body) = got.lock().unwrap()[0].clone();
    assert_eq!(
        auth.as_deref(),
        Some(format!("Bearer {push_token}").as_str())
    );
    let t = &body["tasks"][0];
    assert_eq!(t["task_id"], task_id.as_str());
    assert_eq!(t["payload"]["who"], "push");
    let claim = t["claim_id"].as_str().unwrap();

    // Nothing left to pull: the push holds the claim.
    let (_, v) = cp
        .call(&b, "/api/instances/inbox", "inbox.claim", json!({"max": 4}))
        .await;
    assert!(v["tasks"].as_array().unwrap().is_empty(), "{v}");

    let (s, v) = cp
        .call(
            &b,
            "/api/instances/tasks/result",
            "task.result",
            json!({"task_id": task_id, "claim_id": claim, "state": "completed", "result": {"hello": "push"}}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    let (_, v) = cp
        .call(
            &a,
            "/api/instances/tasks/status",
            "task.status",
            json!({"task_id": task_id}),
        )
        .await;
    assert_eq!(v["state"], "completed");
    assert_eq!(v["result"]["hello"], "push");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_push_goes_back_to_the_inbox() {
    let a = InstanceKey::from_seed([52; 32]);
    let b = InstanceKey::from_seed([53; 32]);
    let (rx, got) = receiver(Arc::new(AtomicU16::new(503))).await;
    let cp = start(&[&a, &b], Some(&rx)).await;
    cp.register(&a, None).await;
    cp.register(&b, Some(&format!("{rx}/push"))).await;
    let task_id = cp.submit(&a, &b).await;
    wait_for(&got, 1).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_, v) = cp
        .call(&b, "/api/instances/inbox", "inbox.claim", json!({"max": 4}))
        .await;
    assert_eq!(
        v["tasks"][0]["task_id"],
        task_id.as_str(),
        "released at once, pullable: {v}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_push_to_an_origin_that_is_not_allowed_or_when_push_is_off() {
    let a = InstanceKey::from_seed([54; 32]);
    let b = InstanceKey::from_seed([55; 32]);
    let (rx, got) = receiver(Arc::new(AtomicU16::new(200))).await;
    // Push on, but only for another origin.
    let cp = start(&[&a, &b], Some("https://elsewhere.example")).await;
    cp.register(&a, None).await;
    cp.register(&b, Some(&format!("{rx}/push"))).await;
    let task_id = cp.submit(&a, &b).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(got.lock().unwrap().is_empty(), "never sent");
    let (_, v) = cp
        .call(&b, "/api/instances/inbox", "inbox.claim", json!({"max": 4}))
        .await;
    assert_eq!(v["tasks"][0]["task_id"], task_id.as_str());

    // Push off: no token issued, nothing sent.
    let cp = start(&[&a, &b], None).await;
    cp.register(&a, None).await;
    let reg = cp.register(&b, Some(&format!("{rx}/push"))).await;
    assert!(reg["push_token"].is_null(), "{reg}");
    cp.submit(&a, &b).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(got.lock().unwrap().is_empty());
}
