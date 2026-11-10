//! Durable inbox (build plan item 9, `NOETL_INSTANCE_STORE=postgres`): the task board is
//! written through to a store and rebuilt from it, so a control-plane restart loses no queued
//! task, keeps a claim valid, and keeps a finished result readable. Exercised against the real
//! router on a TCP port with [`MemStore`], which has the Postgres store's contract (rev guard,
//! deletes). `instances_durable_inbox_pg` covers the same flow on a real Postgres when
//! `NOETL_TEST_PG_URL` is set.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use noetl_server::instances::gate::{payload, Gate, Trust};
use noetl_server::instances::identity::InstanceKey;
use noetl_server::instances::registry::Registry;
use noetl_server::instances::store::{MemStore, TaskStore};
use noetl_server::instances::tasks::Board;
use noetl_server::instances::{admin_routes_for, routes_for, InstanceState};
use serde_json::{json, Value};

const AUDIENCE: &str = "noetl";

struct Cp {
    base: String,
    http: reqwest::Client,
    clock: Arc<AtomicI64>,
    state: InstanceState,
}

/// One control-plane "process": fresh registry and board, the given store, restored first.
async fn start(trusted: &[&InstanceKey], store: Arc<dyn TaskStore>, clock: Arc<AtomicI64>) -> Cp {
    let ids = trusted.iter().map(|k| k.id().to_string()).collect();
    let mut state = InstanceState::new(
        Gate::new(Trust::Allowlist(ids), AUDIENCE, 120_000),
        Registry::new(60_000, 100),
        Board::new(30_000),
    );
    let c = clock.clone();
    state.clock = Arc::new(move || c.load(Ordering::SeqCst));
    state.store = Some(store);
    state.restore().await.unwrap();
    let app = routes_for(state.clone()).merge(admin_routes_for(state.clone()));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Cp {
        base,
        http: reqwest::Client::new(),
        clock,
        state,
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

    async fn register(&self, k: &InstanceKey, accepts: bool) {
        let (s, v) = self
            .call(
                k,
                "/api/instances/register",
                "register",
                json!({"runtime_kind": "daemon", "version": "noetl-cli/6.0.0", "site": "home",
                    "playbooks": ["edge/*"], "accepts_tasks": accepts}),
            )
            .await;
        assert_eq!(s, 200, "{v}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_keeps_queued_and_claimed_tasks_and_results() {
    let a = InstanceKey::from_seed([30; 32]);
    let b = InstanceKey::from_seed([31; 32]);
    let store: Arc<MemStore> = Arc::default();
    let clock = Arc::new(AtomicI64::new(noetl_server::instances::now_ms()));

    // Process 1: two tasks for B; B claims the first.
    let cp1 = start(&[&a, &b], store.clone(), clock.clone()).await;
    cp1.register(&a, false).await;
    cp1.register(&b, true).await;
    let mut ids = Vec::new();
    for who in ["one", "two"] {
        let (s, v) = cp1
            .call(
                &a,
                "/api/instances/tasks/submit",
                "task.submit",
                json!({"target": {"instance_id": b.id()}, "playbook": {"path": "edge/hello"}, "payload": {"who": who}}),
            )
            .await;
        assert_eq!(s, 202, "{v}");
        ids.push(v["task_id"].as_str().unwrap().to_string());
    }
    let (_, v) = cp1
        .call(&b, "/api/instances/inbox", "inbox.claim", json!({"max": 1}))
        .await;
    let first = v["tasks"][0].clone();
    assert_eq!(first["task_id"], ids[0].as_str());
    let claim1 = first["claim_id"].as_str().unwrap().to_string();
    assert_eq!(
        store.load().await.unwrap().len(),
        2,
        "both rows written through"
    );
    drop(cp1); // the old process's routes keep serving on their port, but nothing uses them

    // Process 2: same store, empty memory. Instances re-register (their heartbeat).
    let cp2 = start(&[&a, &b], store.clone(), clock.clone()).await;
    cp2.register(&a, false).await;
    cp2.register(&b, true).await;

    // The claim made before the restart is still the current one: B's result lands.
    let (s, v) = cp2
        .call(
            &b,
            "/api/instances/tasks/result",
            "task.result",
            json!({"task_id": ids[0], "claim_id": claim1, "state": "completed", "result": {"ok": 1}}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["recorded"], true);

    // The second task is still queued and is claimable now.
    let (_, v) = cp2
        .call(&b, "/api/instances/inbox", "inbox.claim", json!({"max": 4}))
        .await;
    let claimed: Vec<&str> = v["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["task_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        claimed,
        vec![ids[1].as_str()],
        "only the queued task, not the finished one"
    );

    // A restart again: the result of the first task is still readable by the submitter.
    let cp3 = start(&[&a, &b], store.clone(), clock.clone()).await;
    cp3.register(&a, false).await;
    let (s, v) = cp3
        .call(
            &a,
            "/api/instances/tasks/status",
            "task.status",
            json!({"task_id": ids[0]}),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["state"], "completed");
    assert_eq!(v["result"]["ok"], 1);
    assert!(
        v.get("claim_id").is_none(),
        "the submitter never sees the claim id"
    );
    let _ = cp2.state.clone();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_submit_that_cannot_be_stored_is_refused_and_not_queued() {
    let a = InstanceKey::from_seed([32; 32]);
    let b = InstanceKey::from_seed([33; 32]);
    let store: Arc<MemStore> = Arc::default();
    let clock = Arc::new(AtomicI64::new(noetl_server::instances::now_ms()));
    let cp = start(&[&a, &b], store.clone(), clock).await;
    cp.register(&a, false).await;
    cp.register(&b, true).await;
    store.fail.store(true, Ordering::SeqCst);
    let (s, v) = cp
        .call(
            &a,
            "/api/instances/tasks/submit",
            "task.submit",
            json!({"target": {"instance_id": b.id()}, "playbook": {"path": "edge/hello"}, "payload": {}}),
        )
        .await;
    assert_eq!(s, 503, "{v}");
    store.fail.store(false, Ordering::SeqCst);
    let (_, v) = cp
        .call(&b, "/api/instances/inbox", "inbox.claim", json!({"max": 4}))
        .await;
    assert_eq!(
        v["tasks"].as_array().unwrap().len(),
        0,
        "nothing half-accepted"
    );
    assert!(store.load().await.unwrap().is_empty());
}
