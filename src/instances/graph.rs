//! **The instance graph**: the registry projected into ehgraph `noetl-cluster`.
//!
//! The server is the only writer (ehgraph E2): an instance never opens or writes the graph.
//! Every write uses the graph's own operations (`graph.upsert`, `graph.deactivate`), in
//! process, through the embedded instance.
//!
//! | Element | id | type |
//! |---|---|---|
//! | node | `control:<server id>` | `control_plane` |
//! | node | `instance:<instance id>` | `noetl_instance` |
//! | node | `pool:<name>` | `pool` |
//! | node | `playbook:<path or glob>` | `playbook` |
//! | edge | instance → control | `registered_with` |
//! | edge | instance → pool | `in_pool` |
//! | edge | instance → playbook | `can_run` |
//! | edge | instance → instance | `knows` (from `known_peers`) |
//! | edge | instance → instance | `delegated_to` (a task handoff) |
//!
//! A lapsed lease becomes an inactive node through `graph.deactivate` by
//! `source: instance-registry` and `not_seen_since`, which is ehgraph's own soft-deactivate.
//! History stays in the graph: who knew whom, and when, is still answerable after the
//! instance is gone.
//!
//! ⚠ ehgraph rejects an edge to a missing or inactive node, and rejects the whole batch with
//! it. So `knows` and `delegated_to` edges go in their own batches, one per edge: a peer whose
//! node is not in the graph yet costs that one edge, never the instance's own record.
//!
//! # Default OFF
//!
//! [`WRITER_ENV`] must be exactly `true`, the embedded graph must be running
//! (`NOETL_EHGRAPH_EMBEDDED=true`), and [`TOKEN_FILE_ENV`] must name a file holding a bearer
//! token with a write grant on [`NAMESPACE_ENV`]. Missing any of them, nothing is written and
//! the registry works exactly the same.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::registry::InstanceRecord;

pub const WRITER_ENV: &str = "NOETL_EHGRAPH_TOPOLOGY_WRITER";
pub const TOKEN_FILE_ENV: &str = "NOETL_EHGRAPH_TOPOLOGY_TOKEN_FILE";
pub const NAMESPACE_ENV: &str = "NOETL_EHGRAPH_TOPOLOGY_NAMESPACE";
pub const DEFAULT_NAMESPACE: &str = "topology";
/// The `source` every element carries, and the one the sweep deactivates by.
pub const SOURCE: &str = "instance-registry";
const ORIGIN: &str = "registered";

pub fn node_id(instance_id: &str) -> String {
    format!("instance:{instance_id}")
}

fn node(id: String, ty: &str, props: Value, ts: i64) -> Value {
    json!({"id": id, "type": ty, "props": props, "origin": ORIGIN, "last_seen": ts})
}

fn edge(src: &str, ty: &str, dst: &str, props: Value, ts: i64) -> Value {
    json!({"src": src, "dst": dst, "type": ty, "props": props, "origin": ORIGIN, "last_seen": ts})
}

fn request(
    namespace: &str,
    nodes: Vec<Value>,
    edges: Vec<Value>,
    execution_id: &str,
    ts: i64,
) -> Value {
    json!({
        "namespace": namespace,
        "nodes": nodes,
        "edges": edges,
        "source": SOURCE,
        "execution_id": execution_id,
        "ts_ms": ts,
    })
}

/// The batches one registration becomes: the instance's own record first, then one batch per
/// `knows` edge to a peer in `live_peers`.
pub fn projection(
    namespace: &str,
    control_id: &str,
    rec: &InstanceRecord,
    live_peers: &[String],
    now: i64,
) -> Vec<Value> {
    let c = &rec.capabilities;
    let me = node_id(&rec.instance_id);
    let control = format!("control:{control_id}");
    let labels: BTreeMap<_, _> = c
        .labels
        .iter()
        .map(|(k, v)| (k.clone(), json!(v)))
        .collect();
    let mut props = json!({
        "runtime_kind": c.runtime_kind,
        "version": c.version,
        "reach": c.reach,
        "accepts_tasks": c.accepts_tasks,
        "tool_kinds": c.tool_kinds,
        "pools": c.pools,
        "labels": labels,
        "first_seen_ms": rec.first_seen_ms,
    });
    for (k, v) in [
        ("name", &c.name),
        ("site", &c.site),
        ("endpoint", &c.endpoint),
    ] {
        if let Some(v) = v {
            props[k] = json!(v);
        }
    }
    let mut nodes = vec![
        node(control.clone(), "control_plane", json!({}), now),
        node(me.clone(), "noetl_instance", props, now),
    ];
    let mut edges = vec![edge(&me, "registered_with", &control, json!({}), now)];
    for p in &c.pools {
        let id = format!("pool:{p}");
        nodes.push(node(id.clone(), "pool", json!({"name": p}), now));
        edges.push(edge(&me, "in_pool", &id, json!({}), now));
    }
    for g in &c.playbooks {
        let id = format!("playbook:{g}");
        nodes.push(node(
            id.clone(),
            "playbook",
            json!({"path": g, "pattern": g.contains('*')}),
            now,
        ));
        edges.push(edge(&me, "can_run", &id, json!({}), now));
    }
    let exec = format!("instance-register:{}", rec.instance_id);
    let mut out = vec![request(namespace, nodes, edges, &exec, now)];
    for peer in c
        .known_peers
        .iter()
        .filter(|p| live_peers.contains(p) && **p != rec.instance_id)
    {
        out.push(request(
            namespace,
            vec![],
            vec![edge(&me, "knows", &node_id(peer), json!({}), now)],
            &exec,
            now,
        ));
    }
    out
}

/// One `delegated_to` edge for a task handoff.
pub fn delegation(
    namespace: &str,
    from: &str,
    to: &str,
    task_id: &str,
    playbook: &str,
    now: i64,
) -> Value {
    request(
        namespace,
        vec![],
        vec![edge(
            &node_id(from),
            "delegated_to",
            &node_id(to),
            json!({"last_task": task_id, "playbook": playbook}),
            now,
        )],
        &format!("instance-task:{task_id}"),
        now,
    )
}

/// Retire every element this writer owns that was not seen since `not_seen_since`.
pub fn sweep(namespace: &str, not_seen_since: i64, now: i64) -> Value {
    json!({
        "namespace": namespace,
        "source": SOURCE,
        "origin": ORIGIN,
        "not_seen_since": not_seen_since,
        "execution_id": format!("instance-sweep:{now}"),
        "ts_ms": now,
    })
}

/// Where projections go.
#[async_trait]
pub trait GraphSink: Send + Sync {
    /// Call one graph tool. `Err` carries the graph's own error text.
    async fn call(&self, tool: &str, args: Value) -> Result<Value, String>;
}

/// The embedded `noetl-cluster` graph, through its in-process client.
pub struct EmbeddedSink {
    pub graph: ehgraph::EhGraph,
    pub token: String,
}

#[async_trait]
impl GraphSink for EmbeddedSink {
    async fn call(&self, tool: &str, args: Value) -> Result<Value, String> {
        let env = self
            .graph
            .call_async(self.token.clone(), tool.to_string(), args)
            .await;
        let v = serde_json::to_value(&env).map_err(|e| e.to_string())?;
        if v["ok"].as_bool() == Some(true) {
            Ok(v)
        } else {
            Err(v["error"].to_string())
        }
    }
}

/// The configured writer, or `None` when the graph projection is off.
pub fn writer_from_env() -> Option<(Box<dyn GraphSink>, String)> {
    if std::env::var(WRITER_ENV).ok().as_deref() != Some("true") {
        return None;
    }
    let Some(graph) = crate::ehgraph_embedded::graph() else {
        tracing::error!("{WRITER_ENV}=true but the embedded ehgraph is not running; the instance graph is not written");
        return None;
    };
    let path = std::env::var(TOKEN_FILE_ENV).unwrap_or_default();
    let token = match std::fs::read_to_string(&path) {
        Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => {
            tracing::error!("{WRITER_ENV}=true but {TOKEN_FILE_ENV} does not name a readable token file; the instance graph is not written");
            return None;
        }
    };
    let ns = std::env::var(NAMESPACE_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_NAMESPACE.to_string());
    Some((Box::new(EmbeddedSink { graph, token }), ns))
}

#[cfg(test)]
mod tests {
    use super::super::registry::{tests::caps, InstanceRecord, RuntimeKind};
    use super::*;

    const A: &str = "ni-0000000000000000000000000000000a";
    const B: &str = "ni-0000000000000000000000000000000b";

    fn rec() -> InstanceRecord {
        let mut c = caps(RuntimeKind::Daemon);
        c.pools = vec!["edge".into()];
        c.known_peers = vec![B.into(), "ni-0000000000000000000000000000000c".into()];
        InstanceRecord {
            instance_id: A.into(),
            public_key: "pk".into(),
            capabilities: c,
            first_seen_ms: 1,
            last_seen_ms: 2,
            heartbeats: 0,
        }
    }

    #[test]
    fn a_registration_projects_its_record_then_one_batch_per_live_peer() {
        let b = projection("topology", "srv-0", &rec(), &[B.to_string()], 5);
        assert_eq!(
            b.len(),
            2,
            "the record, plus `knows` only for the peer that is live"
        );
        let core = &b[0];
        let types: Vec<_> = core["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["type"].as_str().unwrap())
            .collect();
        assert_eq!(
            types,
            ["control_plane", "noetl_instance", "pool", "playbook"]
        );
        let edges: Vec<_> = core["edges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["type"].as_str().unwrap())
            .collect();
        assert_eq!(edges, ["registered_with", "in_pool", "can_run"]);
        assert_eq!(core["nodes"][1]["props"]["site"], "home");
        assert_eq!(core["source"], SOURCE);
        assert_eq!(b[1]["edges"][0]["dst"], format!("instance:{B}"));
        assert_eq!(b[1]["edges"][0]["type"], "knows");
    }
}
