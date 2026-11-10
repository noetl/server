//! Enrollment tokens (build plan item 9, `NOETL_INSTANCE_TRUST=enroll`): an operator mints a
//! token, a new instance redeems it with a signed envelope and becomes trusted, the token is
//! single use and bound to the request that spent it, and revocation takes trust away again.
//! Real router on TCP, real ML-DSA-87 envelopes.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use noetl_server::instances::enroll::MemEnroll;
use noetl_server::instances::gate::{payload, Gate, Trust};
use noetl_server::instances::identity::{InstanceKey, SignedEnvelope};
use noetl_server::instances::registry::Registry;
use noetl_server::instances::tasks::Board;
use noetl_server::instances::{admin_routes_for, routes_for, InstanceState};
use serde_json::{json, Value};

const AUDIENCE: &str = "noetl";

struct Cp {
    base: String,
    http: reqwest::Client,
    clock: Arc<AtomicI64>,
}

async fn start(enroll: bool) -> Cp {
    let clock = Arc::new(AtomicI64::new(noetl_server::instances::now_ms()));
    let mut gate = Gate::new(Trust::Allowlist(Default::default()), AUDIENCE, 120_000);
    gate.enroll = enroll;
    let mut state = InstanceState::new(gate, Registry::new(60_000, 100), Board::new(30_000));
    let c = clock.clone();
    state.clock = Arc::new(move || c.load(Ordering::SeqCst));
    if enroll {
        state.enrollments = Some(Arc::new(MemEnroll::default()));
    }
    let app = routes_for(state.clone()).merge(admin_routes_for(state));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Cp {
        base,
        http: reqwest::Client::new(),
        clock,
    }
}

impl Cp {
    fn sign(&self, k: &InstanceKey, purpose: &str, body: Value) -> SignedEnvelope {
        k.sign(payload(
            purpose,
            k.id(),
            AUDIENCE,
            self.clock.load(Ordering::SeqCst),
            &noetl_server::instances::tasks::new_id("n"),
            body,
        ))
    }
    async fn post<T: serde::Serialize>(&self, route: &str, b: &T) -> (u16, Value) {
        let r = self
            .http
            .post(format!("{}{route}", self.base))
            .json(b)
            .send()
            .await
            .unwrap();
        let s = r.status().as_u16();
        (s, r.json().await.unwrap_or(Value::Null))
    }
    async fn register(&self, k: &InstanceKey) -> (u16, Value) {
        let env = self.sign(
            k,
            "register",
            json!({"runtime_kind": "cli", "version": "noetl-cli/6.0.0", "site": "lab"}),
        );
        self.post("/api/instances/register", &env).await
    }
    async fn mint(&self, label: &str, uses: Option<i64>) -> String {
        let (s, v) = self
            .post(
                "/api/runtime/instances/enrollments",
                &json!({"label": label, "ttl_secs": 600, "uses": uses}),
            )
            .await;
        assert_eq!(s, 201, "{v}");
        assert_eq!(v["label"], label);
        v["token"].as_str().unwrap().to_string()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mint_enroll_register_spend_and_revoke() {
    let cp = start(true).await;
    let c = InstanceKey::from_seed([40; 32]);
    let d = InstanceKey::from_seed([41; 32]);

    let (s, v) = cp.register(&c).await;
    assert_eq!(s, 403, "untrusted before enrolling: {v}");
    assert!(v["error"].as_str().unwrap().contains("enroll"), "{v}");

    let token = cp.mint("edge-c", None).await;
    let env = cp.sign(&c, "enroll", json!({"token": token}));
    let (s, v) = cp.post("/api/instances/enroll", &env).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["instance_id"], c.id());
    assert_eq!(v["label"], "edge-c");

    let (s, v) = cp.register(&c).await;
    assert_eq!(s, 200, "trusted after enrolling: {v}");

    // The same envelope again is a replay; the token is spent anyway.
    let (s, _) = cp.post("/api/instances/enroll", &env).await;
    assert_eq!(s, 409, "replayed enroll envelope");
    let (s, v) = cp
        .post(
            "/api/instances/enroll",
            &cp.sign(&d, "enroll", json!({"token": token})),
        )
        .await;
    assert_eq!(s, 403, "a spent token does not enroll a second key: {v}");
    let (s, _) = cp
        .post(
            "/api/instances/enroll",
            &cp.sign(&d, "enroll", json!({"token": "nie-guess"})),
        )
        .await;
    assert_eq!(s, 403);
    // A token signed into another purpose's envelope is refused by the gate.
    let fresh = cp.mint("edge-d", Some(2)).await;
    let (s, _) = cp
        .post(
            "/api/instances/enroll",
            &cp.sign(&d, "register", json!({"token": fresh})),
        )
        .await;
    assert_eq!(s, 400, "purpose mismatch");
    let (s, _) = cp
        .post(
            "/api/instances/enroll",
            &cp.sign(&d, "enroll", json!({"token": fresh})),
        )
        .await;
    assert_eq!(s, 200, "a two-use token still has a use");

    let (s, v) = cp
        .http
        .get(format!("{}/api/runtime/instances/enrolled", cp.base))
        .send()
        .await
        .map(|r| (r.status().as_u16(), r))
        .unwrap();
    let v: Value = v.json().await.unwrap();
    assert_eq!(s, 200);
    assert_eq!(v["enrolled"].as_array().unwrap().len(), 2, "{v}");

    let r = cp
        .http
        .delete(format!(
            "{}/api/runtime/instances/enrolled/{}",
            cp.base,
            c.id()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    let (s, _) = cp.register(&c).await;
    assert_eq!(s, 403, "revoked");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enrollment_is_off_unless_trust_is_enroll() {
    let cp = start(false).await;
    let c = InstanceKey::from_seed([42; 32]);
    let (s, _) = cp
        .post(
            "/api/instances/enroll",
            &cp.sign(&c, "enroll", json!({"token": "nie-x"})),
        )
        .await;
    assert_eq!(s, 403);
    let (s, _) = cp
        .post("/api/runtime/instances/enrollments", &json!({"label": "x"}))
        .await;
    assert_eq!(s, 409);
}
