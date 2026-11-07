//! Embedded ehgraph `noetl-cluster` (noetl/ehgraph E2/E4): the open decision, the
//! mounted routes, in-process calls, the single-writer refusals, and the wiring in
//! `main.rs`.
//!
//! ⚠ Every test that opens the graph is `#[serial]`. An embedded primary claims its
//! `instance_id` and `domain` process-wide, and the domain is always
//! `noetl-cluster`, so two such tests in parallel would refuse each other. That is
//! exactly the E2 guard working, and one test below relies on it.

use std::path::Path;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use noetl_server::ehgraph_embedded::{
    open_with, prepare_config, single_writer_declaration, Embedded, HostTopology, EMBEDDED_DOMAIN,
    MOUNT,
};
use serde_json::{json, Value};
use serial_test::serial;
use tower::ServiceExt;

const TOKEN: &str = "noetl-server-internal";

fn toml(root: &Path, domain: &str, extra: &str) -> String {
    format!(
        r#"
instance_id = "ehgraph-{domain}"
domain = "{domain}"
data_dir = "{data}"
backup_prefix = "{backup}"
{extra}

[[auth.grants]]
token = "{TOKEN}"
principal = "noetl-server"
namespace = "*"
read = true
write = true
"#,
        data = root.join("data").display(),
        backup = root.join("backup").display(),
    )
}

fn single() -> HostTopology {
    HostTopology {
        host_replicas: Some(1),
        host_ordinal: Some(0),
        shard_count: 1,
        execution_affinity: false,
    }
}

async fn send(app: &Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("authorization", format!("Bearer {TOKEN}"));
    let body = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
#[serial]
async fn noetl_cluster_serves_in_process_and_under_the_mount() {
    let dir = tempfile::tempdir().unwrap();
    let graph = open_with(&toml(dir.path(), EMBEDDED_DOMAIN, ""), &single())
        .await
        .unwrap();
    let embedded = Embedded::Running(graph.clone());
    let app = Router::new().nest(MOUNT, embedded.router().unwrap());

    // In-process write: no network hop, same grants as HTTP.
    let wrote = graph
        .client(TOKEN)
        .unwrap()
        .call_async(
            "graph.upsert",
            json!({
                "namespace": "cluster",
                "execution_id": "wiring-test",
                "source": "noetl-server",
                "nodes": [{"id": "svc:server", "type": "Service"}, {"id": "svc:worker", "type": "Service"}],
                "edges": [{"src": "svc:server", "dst": "svc:worker", "type": "DISPATCHES_TO"}],
            }),
        )
        .await;
    assert!(wrote.ok, "{wrote:?}");

    let (status, health) = send(&app, "GET", &format!("{MOUNT}/healthz"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(health["mode"], "embedded");
    assert_eq!(health["domain"], EMBEDDED_DOMAIN);

    // The same instance over MCP, as a `kind: Mcp` playbook reaches it.
    let (status, rpc) = send(
        &app,
        "POST",
        &format!("{MOUNT}/mcp"),
        Some(json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "graph.node", "arguments": {"namespace": "cluster", "id": "svc:worker"}},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rpc["result"]["isError"], false, "{rpc}");

    // No token, no data: the mount is not an unauthenticated side door.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("{MOUNT}/v1/tools/graph.node"))
                .body(Body::from(r#"{"namespace":"cluster","id":"svc:worker"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    embedded.shutdown().await;
}

#[tokio::test]
#[serial]
async fn a_second_embedded_noetl_cluster_in_this_process_fails_fast() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let first = open_with(&toml(a.path(), EMBEDDED_DOMAIN, ""), &single())
        .await
        .unwrap();
    // Another directory, so the data-dir lock cannot see it; the domain claim does.
    let mut other = toml(b.path(), EMBEDDED_DOMAIN, "");
    other = other.replace("ehgraph-noetl-cluster", "ehgraph-noetl-cluster-2");
    let err = open_with(&other, &single()).await.err().unwrap();
    assert!(err.contains("already open in this process"), "{err}");
    assert!(first.shutdown(Duration::from_secs(5)).await.lock_released);
}

#[tokio::test]
#[serial]
async fn every_multi_writer_topology_refuses_before_touching_disk() {
    let dir = tempfile::tempdir().unwrap();
    let text = toml(dir.path(), EMBEDDED_DOMAIN, "");
    let cases = [
        (
            HostTopology {
                host_replicas: None,
                ..single()
            },
            "NOETL_EHGRAPH_HOST_REPLICAS",
        ),
        (
            HostTopology {
                host_replicas: Some(2),
                ..single()
            },
            "2 replicas",
        ),
        (
            HostTopology {
                host_ordinal: Some(1),
                ..single()
            },
            "ordinal 1",
        ),
        (
            HostTopology {
                shard_count: 4,
                ..single()
            },
            "NOETL_SHARD_COUNT=4",
        ),
        (
            HostTopology {
                execution_affinity: true,
                ..single()
            },
            "NOETL_EXECUTION_AFFINITY",
        ),
    ];
    for (topology, expect) in cases {
        let err = open_with(&text, &topology).await.err().unwrap();
        assert!(err.contains(expect), "{topology:?}: {err}");
        assert!(err.contains("standalone"), "{topology:?}: {err}");
    }
    assert!(
        !dir.path().join("data").exists(),
        "a refusal must not create the data dir"
    );
}

#[test]
fn only_noetl_cluster_embeds() {
    let dir = tempfile::tempdir().unwrap();
    let declaration = single_writer_declaration(&single()).unwrap();
    let err = prepare_config(&toml(dir.path(), "network", ""), declaration).unwrap_err();
    assert!(err.contains("E4"), "{err}");
    assert!(err.contains("network standalone"), "{err}");
}

#[test]
fn the_config_file_cannot_declare_its_own_single_writer() {
    let dir = tempfile::tempdir().unwrap();
    let text = toml(
        dir.path(),
        EMBEDDED_DOMAIN,
        "\n[embedded]\nhost_replicas = 1\n",
    );
    let declaration = single_writer_declaration(&single()).unwrap();
    let err = prepare_config(&text, declaration).unwrap_err();
    assert!(err.contains("deployment"), "{err}");
}

#[tokio::test]
async fn a_refused_graph_answers_503_with_the_reason() {
    let embedded = Embedded::Refused("NOETL_EHGRAPH_HOST_REPLICAS is not set".into());
    let app = Router::new().nest(MOUNT, embedded.router().unwrap());
    for path in ["/healthz", "/mcp", "/v1/tools/graph.node"] {
        let (status, body) = send(&app, "GET", &format!("{MOUNT}{path}"), None).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{path}");
        assert_eq!(body["state"], "refused");
        assert!(body["error"].as_str().unwrap().contains("HOST_REPLICAS"));
    }
    assert!(Embedded::Off.router().is_none(), "off must mount nothing");
}

/// ⚠ The both-halves check: a module nobody calls is the noetl/ai-meta#459 shape
/// (every flag reads as working and nothing runs). `main.rs` must start the graph,
/// pass its router into `build_router`, mount it, and shut it down after the
/// listener exits.
#[test]
fn main_starts_mounts_and_shuts_down_the_graph() {
    let main =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs")).unwrap();
    for needle in [
        "ehgraph_embedded::Embedded::start(&app_config).await",
        "let ehgraph_routes = ehgraph.router();",
        "ehgraph_routes: Option<Router>,",
        "app = app.nest(noetl_server::ehgraph_embedded::MOUNT, ehgraph_routes);",
        "ehgraph.shutdown().await;",
    ] {
        assert!(main.contains(needle), "main.rs lost `{needle}`");
    }
    let start = main.find("Embedded::start(").unwrap();
    let serve = main.find("axum::serve(listener, app)").unwrap();
    let stop = main.find("ehgraph.shutdown().await").unwrap();
    assert!(
        start < serve && serve < stop,
        "start before serving, shut down after"
    );
}

// ── ehgraph 760f572: budgets from the host's memory limit (C11), backup handle (C19) ──

const MIB: u64 = 1 << 20;

async fn instance_budgets(graph: &ehgraph::EhGraph) -> Value {
    let info = graph
        .client(TOKEN)
        .unwrap()
        .call_async("graph.instance.info", json!({}))
        .await;
    assert!(info.ok, "{info:?}");
    info.data.unwrap()["budgets"].clone()
}

/// Unset budgets come out of the host's limit. noetl-server sets none, so in a pod
/// they come from the container's cgroup limit; `host_memory_bytes` stands in for it
/// here so the test does not depend on the machine it runs on.
#[tokio::test]
#[serial]
async fn embedded_budgets_are_derived_from_the_host_memory_limit() {
    let dir = tempfile::tempdir().unwrap();
    let extra = format!("[budgets]\nhost_memory_bytes = {}", 1600 * MIB);
    let graph = open_with(&toml(dir.path(), EMBEDDED_DOMAIN, &extra), &single())
        .await
        .unwrap();
    let budgets = instance_budgets(&graph).await;
    assert_eq!(
        budgets["resident_graph_bytes"],
        160 * MIB,
        "graph: 10% of the host"
    );
    assert_eq!(
        budgets["global_bytes"],
        160 * MIB,
        "sessions: 10% of the host"
    );
    // (Each derived budget has a 16 MiB floor, so a small host would not show the ratios.)
    assert_eq!(
        budgets["per_session_bytes"],
        40 * MIB,
        "a quarter of the sessions"
    );
    assert!(graph.shutdown(Duration::from_secs(5)).await.lock_released);
}

/// Explicit budgets that would take more than half the host refuse the open, so the
/// graph answers 503 instead of letting the server be OOM-killed.
#[tokio::test]
#[serial]
async fn embedded_budgets_over_half_the_host_refuse_to_open() {
    let dir = tempfile::tempdir().unwrap();
    let extra = format!(
        "[budgets]\nhost_memory_bytes = {}\nglobal_bytes = {}\nresident_graph_bytes = {}",
        100 * MIB,
        30 * MIB,
        30 * MIB
    );
    let err = open_with(&toml(dir.path(), EMBEDDED_DOMAIN, &extra), &single())
        .await
        .err()
        .expect("budgets over half the host must refuse");
    assert!(err.contains("at most half"), "{err}");
}

/// A write that would grow the resident graph past its budget is refused before
/// anything is logged, in-process just as over HTTP.
#[tokio::test]
#[serial]
async fn a_write_past_the_resident_graph_budget_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let extra = format!(
        "[budgets]\nhost_memory_bytes = {}\nresident_graph_bytes = 4096",
        400 * MIB
    );
    let graph = open_with(&toml(dir.path(), EMBEDDED_DOMAIN, &extra), &single())
        .await
        .unwrap();
    let client = graph.client(TOKEN).unwrap();
    let nodes: Vec<Value> = (0..64)
        .map(|i| json!({"id": format!("svc:{i}"), "type": "Service", "properties": {"pad": "x".repeat(256)}}))
        .collect();
    let wrote = client
        .call_async(
            "graph.upsert",
            json!({"namespace": "cluster", "execution_id": "budget", "source": "noetl-server", "nodes": nodes}),
        )
        .await;
    assert!(!wrote.ok, "{wrote:?}");
    let code = wrote.error.unwrap().code.to_string();
    assert_eq!(code, "MEMORY_BUDGET_EXCEEDED");
    let read = client
        .call_async("graph.node", json!({"namespace": "cluster", "id": "svc:0"}))
        .await;
    assert!(!read.ok, "nothing was logged: {read:?}");
    assert!(graph.shutdown(Duration::from_secs(5)).await.lock_released);
}

/// `backup_prefix` (a directory here, `gs://…` in a pod) is now opened as a backup
/// target behind a handle; `backup_now` ships through it.
#[tokio::test]
#[serial]
async fn backup_now_ships_through_the_configured_target() {
    let dir = tempfile::tempdir().unwrap();
    let graph = open_with(&toml(dir.path(), EMBEDDED_DOMAIN, ""), &single())
        .await
        .unwrap();
    let wrote = graph
        .client(TOKEN)
        .unwrap()
        .call_async(
            "graph.upsert",
            json!({"namespace": "cluster", "execution_id": "backup", "source": "noetl-server",
                   "nodes": [{"id": "svc:server", "type": "Service"}]}),
        )
        .await;
    assert!(wrote.ok, "{wrote:?}");
    let g = graph.clone();
    let checkpoint = tokio::task::spawn_blocking(move || g.backup_now())
        .await
        .unwrap()
        .unwrap();
    assert!(
        checkpoint.namespaces.contains_key("cluster"),
        "{checkpoint:?}"
    );
    assert!(std::fs::read_dir(dir.path().join("backup"))
        .unwrap()
        .next()
        .is_some());
    assert!(graph.shutdown(Duration::from_secs(5)).await.lock_released);
}
