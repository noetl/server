//! **ehgraph `noetl-cluster`, embedded in this server** (noetl/ehgraph E2 + E4).
//!
//! The `noetl-cluster` topology graph runs in-process: one [`ehgraph::EhGraph`]
//! opened on this server's runtime, its MCP/HTTP routes mounted at [`MOUNT`], and
//! the handle available to server code through [`graph`] for in-process calls (no
//! network hop, same per-namespace bearer grants as HTTP). Other domains, e.g.
//! `network` (qnetmon), run standalone (`ehgraph-server`), and this module refuses
//! to embed them (E4).
//!
//! # Default OFF
//!
//! [`ENABLED_ENV`] must be exactly `true`. Unset, not a byte of ehgraph runs, no
//! route is mounted, and the server behaves exactly as the release before it.
//!
//! # Single writer (E2)
//!
//! An embedded ehgraph instance is a **single writer**. Its data-directory lock
//! only covers one volume: two server pods, each with its own PVC, would each take
//! their own lock and become two primaries for one graph. So this module never
//! relies on the lock across pods. It refuses to open unless:
//!
//! - [`HOST_REPLICAS_ENV`] is set, and set to `1`. There is no default: an embedded
//!   writer never starts on an assumption about the deployment.
//! - nothing in this server's own config says it is one of several replicas:
//!   `NOETL_SHARD_COUNT > 1` or `NOETL_EXECUTION_AFFINITY=true` both refuse.
//! - the pod's StatefulSet ordinal (from [`HOST_ORDINAL_ENV`], else the trailing
//!   `-N` of `HOSTNAME`) is 0. A scaled-up StatefulSet's extra pods refuse even if
//!   the replica count was not updated.
//!
//! ehgraph then applies its own checks (the declaration, a process-wide claim on
//! the `instance_id` and `domain`, and the directory lock).
//!
//! # A refusal does not take the control plane down
//!
//! If the graph is armed but refuses (or fails) to open, the server still serves
//! everything else. The refusal is logged at ERROR, and every request under
//! [`MOUNT`] answers `503` with the reason, so a `kind: Mcp` caller sees why
//! rather than a 404. The ehgraph open itself fails fast: no partial instance is
//! ever left running.

use std::sync::OnceLock;
use std::time::Duration;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Json, Router};
use ehgraph::{EhGraph, EmbeddedConfig, GraphConfig};

use crate::config::AppConfig;

/// Arms the embedded graph. Strict `== "true"`.
pub const ENABLED_ENV: &str = "NOETL_EHGRAPH_EMBEDDED";
/// Path to the ehgraph config: the `GraphConfig` TOML sub-table (instance,
/// data_dir, backup_prefix, budgets, `[[auth.grants]]`). It holds bearer tokens,
/// so mount it from a Secret.
pub const CONFIG_ENV: &str = "NOETL_EHGRAPH_CONFIG";
/// How many server replicas run with this configuration. Required; must be `1`.
pub const HOST_REPLICAS_ENV: &str = "NOETL_EHGRAPH_HOST_REPLICAS";
/// This pod's StatefulSet ordinal. Optional; derived from `HOSTNAME` when unset.
pub const HOST_ORDINAL_ENV: &str = "NOETL_EHGRAPH_HOST_ORDINAL";

/// The only domain this server embeds (E4).
pub const EMBEDDED_DOMAIN: &str = "noetl-cluster";
/// Where the graph's routes are mounted: `<server>/ehgraph/noetl-cluster/mcp`.
pub const MOUNT: &str = "/ehgraph/noetl-cluster";

/// How long [`Embedded::shutdown`] waits for in-flight graph calls.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

static GRAPH: OnceLock<EhGraph> = OnceLock::new();

/// The running embedded graph, for in-process calls. `None` when off or refused.
pub fn graph() -> Option<EhGraph> {
    GRAPH.get().cloned()
}

/// Is the embedded graph armed? Strict `== "true"`.
pub fn enabled() -> bool {
    std::env::var(ENABLED_ENV)
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// What this server knows about its own deployment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostTopology {
    /// [`HOST_REPLICAS_ENV`], if set.
    pub host_replicas: Option<u32>,
    /// [`HOST_ORDINAL_ENV`], else the `HOSTNAME` ordinal.
    pub host_ordinal: Option<u32>,
    /// `NOETL_SHARD_COUNT` (1 when unset).
    pub shard_count: u32,
    /// `NOETL_EXECUTION_AFFINITY`.
    pub execution_affinity: bool,
}

impl HostTopology {
    /// Read the topology from the environment and the server's config.
    ///
    /// ⚠ A set-but-unparseable value is an error, not `None`: a typo in
    /// `NOETL_EHGRAPH_HOST_REPLICAS` must not read as "not declared" in one place
    /// and something else in another.
    pub fn from_env(app: &AppConfig) -> Result<Self, String> {
        let number = |name: &str| -> Result<Option<u32>, String> {
            match std::env::var(name) {
                Err(_) => Ok(None),
                Ok(v) => v
                    .trim()
                    .parse::<u32>()
                    .map(Some)
                    .map_err(|_| format!("{name}={v:?} is not a non-negative integer")),
            }
        };
        let host_ordinal = match number(HOST_ORDINAL_ENV)? {
            Some(n) => Some(n),
            None => std::env::var("HOSTNAME")
                .ok()
                .and_then(|h| crate::affinity::shard_index_from_hostname(&h)),
        };
        Ok(Self {
            host_replicas: number(HOST_REPLICAS_ENV)?,
            host_ordinal,
            shard_count: app.shard_count.unwrap_or(1),
            execution_affinity: app.execution_affinity,
        })
    }
}

/// The host-side half of E2: refuse on any evidence that this server is one of
/// several replicas, then hand ehgraph the declaration it checks itself.
pub fn single_writer_declaration(topology: &HostTopology) -> Result<EmbeddedConfig, String> {
    let standalone =
        format!("an embedded ehgraph writer needs a single-replica server; run {EMBEDDED_DOMAIN} standalone instead (E2)");
    if topology.shard_count > 1 {
        return Err(format!(
            "NOETL_SHARD_COUNT={} means a multi-replica control plane; {standalone}",
            topology.shard_count
        ));
    }
    if topology.execution_affinity {
        return Err(format!(
            "NOETL_EXECUTION_AFFINITY=true means a multi-replica control plane; {standalone}"
        ));
    }
    let Some(replicas) = topology.host_replicas else {
        return Err(format!(
            "{HOST_REPLICAS_ENV} is not set; the deployment must declare it (=1). {standalone}"
        ));
    };
    Ok(EmbeddedConfig {
        host_replicas: Some(replicas),
        host_ordinal: topology.host_ordinal,
        leader: None,
    })
}

/// Parse the ehgraph config and apply the host's declaration.
///
/// The `[embedded]` declaration is the HOST's to make, from the deployment, so a
/// config file that carries its own is refused rather than silently overridden or
/// silently trusted.
pub fn prepare_config(toml: &str, declaration: EmbeddedConfig) -> Result<GraphConfig, String> {
    let mut config = GraphConfig::from_toml(toml).map_err(|e| e.to_string())?;
    if config.domain != EMBEDDED_DOMAIN {
        return Err(format!(
            "domain {} is not embedded in noetl-server; only {EMBEDDED_DOMAIN} is (E4). Run {} \
             standalone (ehgraph-server)",
            config.domain, config.domain
        ));
    }
    let e = &config.embedded;
    if e.host_replicas.is_some() || e.host_ordinal.is_some() || e.leader.is_some() {
        return Err(format!(
            "the ehgraph config must not carry an [embedded] section: the single-writer \
             declaration comes from the deployment ({HOST_REPLICAS_ENV}), not the file"
        ));
    }
    config.embedded = declaration;
    Ok(config)
}

/// Open the embedded graph from config text and a topology. The whole decision,
/// with no environment access, so every refusal is testable.
pub async fn open_with(toml: &str, topology: &HostTopology) -> Result<EhGraph, String> {
    let declaration = single_writer_declaration(topology)?;
    let config = prepare_config(toml, declaration)?;
    let graph = EhGraph::open_embedded_async(config)
        .await
        .map_err(|e| e.to_string())?;
    if let Err(err) = graph.start_maintenance() {
        graph.shutdown(SHUTDOWN_TIMEOUT).await;
        return Err(format!("ehgraph maintenance did not start: {err}"));
    }
    Ok(graph)
}

/// The embedded graph's state for this process.
pub enum Embedded {
    /// [`ENABLED_ENV`] is not `true`. Nothing runs, nothing is mounted.
    Off,
    /// Serving: in-process via [`graph`], over HTTP at [`MOUNT`].
    Running(EhGraph),
    /// Armed, but it refused or failed to open. [`MOUNT`] answers 503 with this.
    Refused(String),
}

impl Embedded {
    /// Start from the environment. Never fails the server: see the module doc.
    pub async fn start(app: &AppConfig) -> Self {
        if !enabled() {
            return Self::Off;
        }
        let opened = async {
            let path = std::env::var(CONFIG_ENV).map_err(|_| format!("{CONFIG_ENV} is not set"))?;
            let toml = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {CONFIG_ENV}={path}: {e}"))?;
            let topology = HostTopology::from_env(app)?;
            open_with(&toml, &topology).await
        }
        .await;
        Self::from_open(opened)
    }

    /// Record an open result, publishing the handle for [`graph`] on success.
    pub fn from_open(opened: Result<EhGraph, String>) -> Self {
        match opened {
            Ok(graph) => {
                tracing::info!(
                    instance_id = %graph.identity().instance_id,
                    domain = %graph.identity().domain,
                    mount = MOUNT,
                    "embedded ehgraph serving"
                );
                let _ = GRAPH.set(graph.clone());
                Self::Running(graph)
            }
            Err(reason) => {
                tracing::error!(
                    %reason,
                    mount = MOUNT,
                    "embedded ehgraph REFUSED to open; the control plane keeps serving, \
                     {MOUNT} answers 503"
                );
                Self::Refused(reason)
            }
        }
    }

    /// The router to mount at [`MOUNT`], or `None` when off.
    pub fn router(&self) -> Option<Router> {
        match self {
            Self::Off => None,
            Self::Running(graph) => Some(graph.router()),
            Self::Refused(reason) => Some(refused_router(reason.clone())),
        }
    }

    /// Stop maintenance, drain in-flight calls, release the data-directory lock.
    pub async fn shutdown(self) {
        if let Self::Running(graph) = self {
            let report = graph.shutdown(SHUTDOWN_TIMEOUT).await;
            tracing::info!(
                lock_released = report.lock_released,
                tasks_stopped = report.tasks_stopped,
                "embedded ehgraph shut down"
            );
        }
    }
}

fn refused_router(reason: String) -> Router {
    Router::new().fallback(move || {
        let reason = reason.clone();
        async move {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "ok": false,
                    "domain": EMBEDDED_DOMAIN,
                    "mode": "embedded",
                    "state": "refused",
                    "error": reason,
                })),
            )
                .into_response()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn single() -> HostTopology {
        HostTopology {
            host_replicas: Some(1),
            host_ordinal: None,
            shard_count: 1,
            execution_affinity: false,
        }
    }

    #[test]
    fn an_undeclared_replica_count_refuses() {
        let err = single_writer_declaration(&HostTopology {
            host_replicas: None,
            ..single()
        })
        .unwrap_err();
        assert!(err.contains(HOST_REPLICAS_ENV), "{err}");
        assert!(err.contains("standalone"), "{err}");
    }

    #[test]
    fn multi_replica_evidence_refuses_even_with_a_declared_single_host() {
        let err = single_writer_declaration(&HostTopology {
            shard_count: 2,
            ..single()
        })
        .unwrap_err();
        assert!(err.contains("NOETL_SHARD_COUNT=2"), "{err}");
        let err = single_writer_declaration(&HostTopology {
            execution_affinity: true,
            ..single()
        })
        .unwrap_err();
        assert!(err.contains("NOETL_EXECUTION_AFFINITY"), "{err}");
    }

    #[test]
    fn the_declaration_passes_replicas_and_ordinal_to_ehgraph() {
        let d = single_writer_declaration(&HostTopology {
            host_ordinal: Some(3),
            ..single()
        })
        .unwrap();
        assert_eq!(d.host_replicas, Some(1));
        // ehgraph refuses ordinal 3 itself; the host passes it through untouched.
        assert_eq!(d.host_ordinal, Some(3));
        assert!(d.check_single_writer(EMBEDDED_DOMAIN).is_err());
    }
}
