//! NoETL instances end to end (build plan item 9): two instances register, one discovers the
//! other and hands it a playbook run, the other pulls it from its inbox and posts the result,
//! and the submitter gets it by callback and by polling. Every request is a real ML-DSA-87
//! signed envelope against the real router, served on a TCP port.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::routing::post;
use axum::Router;
use noetl_server::instances::gate::{payload, Gate, Trust};
use noetl_server::instances::identity::{InstanceKey, SignedEnvelope};
use noetl_server::instances::registry::Registry;
use noetl_server::instances::tasks::Board;
use noetl_server::instances::{admin_routes_for, routes_for, InstanceState};
use serde_json::{json, Value};

const AUDIENCE: &str = "noetl";

type Received = Arc<Mutex<Vec<(Option<String>, Value)>>>;

struct Harness {
    base: String,
    http: reqwest::Client,
    clock: Arc<AtomicI64>,
    state: InstanceState,
}

fn nonce() -> String {
    noetl_server::instances::tasks::new_id("n")
}

impl Harness {
    async fn start(trusted: &[&InstanceKey], callback_origin: Option<&str>) -> Harness {
        let ids = trusted.iter().map(|k| k.id().to_string()).collect();
        let clock = Arc::new(AtomicI64::new(noetl_server::instances::now_ms()));
        let mut state = InstanceState::new(
            Gate::new(Trust::Allowlist(ids), AUDIENCE, 120_000),
            Registry::new(60_000, 100),
            Board::new(30_000),
        );
        let c = clock.clone();
        state.clock = Arc::new(move || c.load(Ordering::SeqCst));
        if let Some(o) = callback_origin {
            state.callbacks = noetl_server::handlers::agent_endpoint::CallbackPolicy::parse(o);
        }
        let app = routes_for(state.clone()).merge(admin_routes_for(state.clone()));
        let base = serve(app).await;
        Harness {
            base,
            http: reqwest::Client::new(),
            clock,
            state,
        }
    }

    fn sign(&self, k: &InstanceKey, purpose: &str, body: Value) -> SignedEnvelope {
        k.sign(payload(
            purpose,
            k.id(),
            AUDIENCE,
            self.clock.load(Ordering::SeqCst),
            &nonce(),
            body,
        ))
    }

    async fn post_env(&self, route: &str, env: &SignedEnvelope) -> (u16, Value) {
        let r = self
            .http
            .post(format!("{}{route}", self.base))
            .json(env)
            .send()
            .await
            .unwrap();
        let status = r.status().as_u16();
        (status, r.json().await.unwrap_or(Value::Null))
    }

    async fn call(&self, k: &InstanceKey, route: &str, purpose: &str, body: Value) -> (u16, Value) {
        let env = self.sign(k, purpose, body);
        self.post_env(route, &env).await
    }
}

async fn serve(app: Router) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

fn daemon(playbooks: &[&str], accepts: bool, peers: &[&str]) -> Value {
    json!({
        "runtime_kind": "daemon",
        "version": "noetl-cli/6.1.0",
        "site": "home",
        "labels": {"arch": "arm64"},
        "tool_kinds": ["shell", "http"],
        "playbooks": playbooks,
        "accepts_tasks": accepts,
        "known_peers": peers,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn register_discover_hand_off_pull_and_call_back() {
    let a = InstanceKey::from_seed([10; 32]);
    let b = InstanceKey::from_seed([11; 32]);

    // A callback receiver standing in for instance A's own endpoint.
    let got: Received = Arc::default();
    let sink = got.clone();
    let receiver = Router::new().route(
        "/hook",
        post(
            move |headers: axum::http::HeaderMap, axum::Json(v): axum::Json<Value>| {
                let sink = sink.clone();
                async move {
                    let bearer = headers
                        .get("authorization")
                        .and_then(|h| h.to_str().ok())
                        .map(str::to_string);
                    sink.lock().unwrap().push((bearer, v));
                    "ok"
                }
            },
        ),
    );
    let hook_base = serve(receiver).await;
    let h = Harness::start(&[&a, &b], Some(&hook_base)).await;

    // Register both. A second register is a renewal, not a new arrival.
    let (s, r) = h
        .call(
            &b,
            "/api/instances/register",
            "register",
            daemon(&["edge/*"], true, &[]),
        )
        .await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["registered"], true);
    assert_eq!(r["heartbeat_secs"], 20);
    let (s, r) = h
        .call(
            &a,
            "/api/instances/register",
            "register",
            daemon(&[], false, &[b.id()]),
        )
        .await;
    assert_eq!((s, r["registered"].clone()), (200, json!(true)));
    let (_, r) = h
        .call(
            &a,
            "/api/instances/register",
            "register",
            daemon(&[], false, &[b.id()]),
        )
        .await;
    assert_eq!(r["renewed"], true);

    // Discovery: by tool kind + playbook, signed, as a registered instance.
    let (s, r) = h
        .call(
            &a,
            "/api/instances/discover",
            "discover",
            json!({"tool_kind": ["shell"], "playbook": "edge/probe"}),
        )
        .await;
    assert_eq!(s, 200, "{r}");
    assert_eq!(r["count"], 1);
    assert_eq!(r["instances"][0]["instance_id"], b.id());
    assert_eq!(r["instances"][0]["live"], true);
    // The operator listing answers the same question over a GET.
    let r: Value = h
        .http
        .get(format!(
            "{}/api/runtime/instances?playbook=edge/probe&label=arch=arm64",
            h.base
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(r["count"], 1);

    // B opens a long-poll BEFORE anything is queued: the submit must wake it.
    let poll = {
        let env = h.sign(&b, "inbox.claim", json!({"wait_ms": 10_000}));
        let (http, url) = (h.http.clone(), format!("{}/api/instances/inbox", h.base));
        tokio::spawn(async move {
            http.post(url)
                .json(&env)
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()
        })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;

    // A hands off by selector, asking for a callback.
    let (s, sub) = h
        .call(
            &a,
            "/api/instances/tasks/submit",
            "task.submit",
            json!({
                "target": {"selector": {"site": "home"}},
                "playbook": {"path": "edge/probe"},
                "payload": {"host": "10.0.0.1"},
                "callback_url": format!("{hook_base}/hook"),
            }),
        )
        .await;
    assert_eq!(s, 202, "{sub}");
    assert_eq!(sub["to"], b.id());
    let task_id = sub["task_id"].as_str().unwrap().to_string();
    let token = sub["callback_token"].as_str().unwrap().to_string();

    let claimed = tokio::time::timeout(Duration::from_secs(5), poll)
        .await
        .expect("long-poll woke")
        .unwrap();
    let t = &claimed["tasks"][0];
    assert_eq!(t["task_id"], task_id.as_str());
    assert_eq!(t["from"], a.id());
    assert_eq!(t["payload"]["host"], "10.0.0.1");
    let claim_id = t["claim_id"].as_str().unwrap().to_string();

    // A cannot complete B's task; B can, once.
    let body = json!({"task_id": task_id, "claim_id": claim_id, "state": "completed", "result": {"up": true}});
    let (s, _) = h
        .call(
            &a,
            "/api/instances/tasks/result",
            "task.result",
            body.clone(),
        )
        .await;
    assert_eq!(s, 404);
    let env = h.sign(&b, "task.result", body.clone());
    let (s, r) = h.post_env("/api/instances/tasks/result", &env).await;
    assert_eq!((s, r["recorded"].clone()), (200, json!(true)));
    // The same signed request again is a replay.
    let (s, _) = h.post_env("/api/instances/tasks/result", &env).await;
    assert_eq!(s, 409);
    // A fresh duplicate is accepted and ignored.
    let (s, r) = h
        .call(&b, "/api/instances/tasks/result", "task.result", body)
        .await;
    assert_eq!((s, r["reason"].clone()), (202, json!("duplicate")));

    // A polls (outbound) and also receives the callback with the token it was given.
    let (s, v) = h
        .call(
            &a,
            "/api/instances/tasks/status",
            "task.status",
            json!({"task_id": task_id}),
        )
        .await;
    assert_eq!(s, 200);
    assert_eq!(v["state"], "completed");
    assert_eq!(v["result"], json!({"up": true}));
    for _ in 0..50 {
        if !got.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let calls = got.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "exactly one callback");
    assert_eq!(
        calls[0].0.as_deref(),
        Some(format!("Bearer {token}").as_str())
    );
    assert_eq!(calls[0].1["state"], "completed");
    assert_eq!(calls[0].1["task_id"], task_id.as_str());
    assert!(calls[0].1.get("claim_id").is_none());

    // A stranger can read nothing.
    let c = InstanceKey::from_seed([12; 32]);
    let (s, r) = h
        .call(
            &c,
            "/api/instances/register",
            "register",
            daemon(&[], false, &[]),
        )
        .await;
    assert_eq!(s, 403, "untrusted: {r}");
    let (s, _) = h
        .call(
            &c,
            "/api/instances/tasks/status",
            "task.status",
            json!({"task_id": task_id}),
        )
        .await;
    assert_eq!(s, 403);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_server_refuses_what_the_target_did_not_offer() {
    let a = InstanceKey::from_seed([20; 32]);
    let b = InstanceKey::from_seed([21; 32]);
    let h = Harness::start(&[&a, &b], None).await;

    // Not registered yet: no discovery, no submit.
    let (s, _) = h
        .call(&a, "/api/instances/discover", "discover", json!({}))
        .await;
    assert_eq!(s, 403);

    h.call(
        &a,
        "/api/instances/register",
        "register",
        daemon(&[], false, &[]),
    )
    .await;
    h.call(
        &b,
        "/api/instances/register",
        "register",
        daemon(&["edge/*"], false, &[]),
    )
    .await;
    let submit = |target: Value, path: &str| json!({"target": target, "playbook": {"path": path}, "payload": {}});
    // B does not accept tasks.
    let (s, r) = h
        .call(
            &a,
            "/api/instances/tasks/submit",
            "task.submit",
            submit(json!({"instance_id": b.id()}), "edge/x"),
        )
        .await;
    assert_eq!(s, 409, "{r}");
    assert!(r["error"]
        .as_str()
        .unwrap()
        .contains("does not accept tasks"));

    h.call(
        &b,
        "/api/instances/register",
        "register",
        daemon(&["edge/*"], true, &[]),
    )
    .await;
    // Outside B's playbooks.
    let (s, r) = h
        .call(
            &a,
            "/api/instances/tasks/submit",
            "task.submit",
            submit(json!({"instance_id": b.id()}), "core/x"),
        )
        .await;
    assert_eq!(s, 409, "{r}");
    // A callback to an origin that is not allowed (none is configured).
    let mut body = submit(json!({"instance_id": b.id()}), "edge/x");
    body["callback_url"] = json!("http://169.254.169.254/latest");
    let (s, _) = h
        .call(&a, "/api/instances/tasks/submit", "task.submit", body)
        .await;
    assert_eq!(s, 400);

    // Accepted without a callback; then B's lease lapses and B can no longer claim.
    let (s, sub) = h
        .call(
            &a,
            "/api/instances/tasks/submit",
            "task.submit",
            submit(json!({"instance_id": b.id()}), "edge/x"),
        )
        .await;
    assert_eq!(s, 202, "{sub}");
    h.clock.fetch_add(61_000, Ordering::SeqCst);
    let (s, _) = h
        .call(&b, "/api/instances/inbox", "inbox.claim", json!({}))
        .await;
    assert_eq!(
        s, 403,
        "a lapsed instance must re-register before it claims"
    );
    let (s, r) = h
        .call(&a, "/api/instances/discover", "discover", json!({}))
        .await;
    assert_eq!(s, 403, "{r}");

    // Re-registering brings B back, and the queued task is still there.
    h.call(
        &b,
        "/api/instances/register",
        "register",
        daemon(&["edge/*"], true, &[]),
    )
    .await;
    let (s, r) = h
        .call(&b, "/api/instances/inbox", "inbox.claim", json!({"max": 4}))
        .await;
    assert_eq!(s, 200);
    assert_eq!(r["tasks"].as_array().unwrap().len(), 1);

    // Past the deadline the sweep expires it.
    h.clock.fetch_add(16 * 60_000, Ordering::SeqCst);
    h.state.sweep_once().await;
    let (_, v) = h
        .call(
            &a,
            "/api/instances/tasks/status",
            "task.status",
            json!({"task_id": sub["task_id"]}),
        )
        .await;
    assert_eq!(v["state"], "expired");
}

/// The instance graph, written into a real embedded `noetl-cluster` ehgraph.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_instance_graph_records_who_knows_whom_and_retires_lapsed_instances() {
    use noetl_server::ehgraph_embedded::{open_with, HostTopology, EMBEDDED_DOMAIN};
    use noetl_server::instances::graph::{EmbeddedSink, GraphSink};

    const TOKEN: &str = "instance-graph-writer";
    let dir = tempfile::tempdir().unwrap();
    let toml = format!(
        r#"
instance_id = "ehgraph-test"
domain = "{EMBEDDED_DOMAIN}"
data_dir = "{data}"
backup_prefix = "{backup}"

[[auth.grants]]
token = "{TOKEN}"
principal = "noetl-server"
namespace = "*"
read = true
write = true
"#,
        data = dir.path().join("data").display(),
        backup = dir.path().join("backup").display(),
    );
    let topo = HostTopology {
        host_replicas: Some(1),
        host_ordinal: Some(0),
        shard_count: 1,
        execution_affinity: false,
    };
    let graph = open_with(&toml, &topo).await.expect("embedded graph opens");
    let sink: Arc<dyn GraphSink> = Arc::new(EmbeddedSink {
        graph: graph.clone(),
        token: TOKEN.into(),
    });

    let a = InstanceKey::from_seed([30; 32]);
    let b = InstanceKey::from_seed([31; 32]);
    let mut h = Harness::start(&[&a, &b], None).await;
    // Install the sink on the running state (the routes share it through Arc'd state, so
    // rebuild the harness router with the graph set).
    h.state.graph = Some((sink.clone(), "topology".into()));
    h.state.control_id = "srv-0".into();
    let app = routes_for(h.state.clone());
    h.base = serve(app).await;

    let mut caps_b = daemon(&["edge/*"], true, &[]);
    caps_b["pools"] = json!(["edge"]);
    assert_eq!(
        h.call(&b, "/api/instances/register", "register", caps_b)
            .await
            .0,
        200
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        h.call(
            &a,
            "/api/instances/register",
            "register",
            daemon(&[], false, &[b.id()])
        )
        .await
        .0,
        200
    );
    let (s, _) = h
        .call(
            &a,
            "/api/instances/tasks/submit",
            "task.submit",
            json!({"target": {"instance_id": b.id()}, "playbook": {"path": "edge/probe"}}),
        )
        .await;
    assert_eq!(s, 202);

    let a_node = format!("instance:{}", a.id());
    let b_node = format!("instance:{}", b.id());
    let mut out = Value::Null;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        out = sink
            .call(
                "graph.neighbors",
                json!({"namespace": "topology", "node_id": a_node, "direction": "out"}),
            )
            .await
            .unwrap_or(Value::Null);
        let s = out.to_string();
        if s.contains("delegated_to") && s.contains("knows") {
            break;
        }
    }
    let text = out.to_string();
    for want in [
        "registered_with",
        "knows",
        "delegated_to",
        b_node.as_str(),
        "control:srv-0",
    ] {
        assert!(
            text.contains(want),
            "A's out-edges should include {want}: {text}"
        );
    }
    let b_out = sink
        .call(
            "graph.neighbors",
            json!({"namespace": "topology", "node_id": b_node, "direction": "out"}),
        )
        .await
        .unwrap()
        .to_string();
    for want in ["in_pool", "pool:edge", "can_run", "playbook:edge/*"] {
        assert!(
            b_out.contains(want),
            "B's out-edges should include {want}: {b_out}"
        );
    }

    // Both leases lapse; the sweep retires them in the graph without deleting them.
    h.clock.fetch_add(61_000, Ordering::SeqCst);
    h.state.sweep_once().await;
    let node = sink
        .call("graph.node", json!({"namespace": "topology", "id": a_node}))
        .await
        .unwrap();
    assert_eq!(node["data"]["active"], false, "{node}");
    graph.shutdown(Duration::from_secs(5)).await;
}
