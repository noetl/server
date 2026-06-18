//! Application configuration for the NoETL Control Plane server.

use serde::Deserialize;

/// Application configuration loaded from environment variables.
///
/// Environment variables are prefixed with `NOETL_`:
/// - `NOETL_HOST`: Server bind address (default: "0.0.0.0")
/// - `NOETL_PORT`: Server port (default: 8082)
/// - `NOETL_WORKERS`: Number of worker threads (optional)
/// - `NOETL_DEBUG`: Enable debug mode (default: false)
/// - `NOETL_SERVER_NAME`: Server name for identification
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    /// Server bind address
    #[serde(default = "default_host")]
    pub host: String,

    /// Server port
    #[serde(default = "default_port")]
    pub port: u16,

    /// Number of worker threads (optional, defaults to CPU count)
    pub workers: Option<usize>,

    /// Enable debug mode
    #[serde(default)]
    pub debug: bool,

    /// Server name for identification
    #[serde(default = "default_server_name")]
    pub server_name: String,

    /// NATS URL (optional)
    #[serde(default)]
    pub nats_url: Option<String>,

    /// Enable GCP token API endpoint
    #[serde(default = "default_true")]
    pub enable_gcp_token_api: bool,

    /// Disable metrics endpoint
    #[serde(default)]
    pub disable_metrics: bool,

    /// References-in-state (noetl/ai-meta#101 phase 2).  When true, the
    /// orchestrator stops resolving over-budget result references back to inline
    /// data — it keeps `{reference, extracted}` on the event so the state +
    /// command context carry references, not bulk payloads (a 1.7MB step output
    /// no longer balloons every `command.issued`).  The orchestrator evaluates
    /// `when:`/`set:` off the small `extracted` predicate block; the worker
    /// resolves the full reference at render time.  Envy maps
    /// `NOETL_REFS_IN_STATE`.  **Default false** — preserves block-b's
    /// resolve-inline behavior exactly until the consume side (worker resolve +
    /// cursor-claim handling) is in place.
    #[serde(default)]
    pub refs_in_state: bool,

    /// CQRS read-model ownership (noetl/ai-meta#103 phase 2b).  When true, the
    /// `system/projector` playbook owns `noetl.projection_snapshot` (it folds
    /// the `noetl_events` stream and advances the snapshot via
    /// `POST /api/internal/projection/advance`), so the orchestrator **stops
    /// self-writing** the snapshot in `trigger_orchestrator` and only reads it.
    /// Envy maps `NOETL_PROJECTOR_OWNS_SNAPSHOT`.  **Default false** — the
    /// orchestrator self-writes exactly as block-b does today; flip on only once
    /// the projector is confirmed running, or the snapshot stops advancing and
    /// rebuild cost grows.
    #[serde(default)]
    pub projector_owns_snapshot: bool,

    /// Worker-driven orchestrator drive (noetl/ai-meta#108 slice 3).  When true,
    /// on a triggering event the server issues the `system/orchestrate` plug-in
    /// as a command to the worker pool (step `__orchestrate__`, `entry:
    /// run_state`) instead of driving in-process; the worker runs the drive and
    /// its completion is applied via `apply_orchestration_result`.  Envy maps
    /// `NOETL_ORCHESTRATE_PLUGIN_DRIVE`.
    ///
    /// **Default true** (noetl/ai-meta#108 (c) — the deliberate default-flip,
    /// after the scale soak proved the drive runs off-server with zero
    /// `noetl.event` burst and full system-pool isolation).  The deployment must
    /// carry a system worker pool with the `wasm-plugin` feature (on by default)
    /// and the seeded `system/orchestrate@1` plug-in; the standard ops manifests
    /// provide both.  **Revert:** set `NOETL_ORCHESTRATE_PLUGIN_DRIVE=false` to
    /// fall back to the in-process drive (`trigger_orchestrator_inner`, kept as
    /// the untouched fallback below) — no rebuild needed, per-deployment and
    /// immediate.
    #[serde(default = "default_true")]
    pub orchestrate_plugin_drive: bool,

    /// Auto recreate runtime if missing
    #[serde(default = "default_true")]
    pub auto_recreate_runtime: bool,

    /// Runtime sweep interval in seconds
    #[serde(default = "default_sweep_interval")]
    pub runtime_sweep_interval: u64,

    /// Runtime offline threshold in seconds
    #[serde(default = "default_offline_seconds")]
    pub runtime_offline_seconds: u64,

    /// Publicly-reachable URL for this server, embedded in NATS
    /// command notifications so workers know where to call back
    /// (`GET /api/commands/{event_id}`).  Envy maps
    /// `NOETL_PUBLIC_SERVER_URL`.  When unset, a localhost
    /// fallback is used — fine for unit tests, won't work
    /// cross-pod in kind / GKE so the deployment manifest must
    /// override.
    #[serde(default)]
    pub public_server_url: Option<String>,

    /// 10-bit machine id for the application-side snowflake
    /// generator.  Envy maps `NOETL_SERVER_MACHINE_ID`.  Each
    /// noetl-server pod in a deployment must have a distinct
    /// value (1024 distinct values possible).  When unset, the
    /// id is derived from the pod hostname at startup — fine
    /// for local dev / single-node deployments; the deployment
    /// manifest should set it explicitly per replica in
    /// production to avoid hash collisions.
    ///
    /// Phase F R1.5 of noetl/ai-meta#49 introduced this.  See
    /// `src/snowflake.rs` for the id layout and the migration
    /// rationale.  The field name (`server_machine_id`) maps to
    /// the env var `NOETL_SERVER_MACHINE_ID` via the
    /// `envy::prefixed("NOETL_")` shape — more specific than a
    /// bare `NOETL_MACHINE_ID` and easier to grep for in
    /// deployment manifests.
    #[serde(default)]
    pub server_machine_id: Option<u16>,

    /// Phase F R2 of noetl/ai-meta#49 — shard index this replica
    /// owns.  Envy maps `NOETL_SHARD_INDEX`.  When unset, defaults
    /// to `0` (single-shard, no enforcement).  Must satisfy
    /// `shard_index < shard_count`; startup validates and panics
    /// otherwise.
    #[serde(default)]
    pub shard_index: Option<u32>,

    /// Phase F R2 of noetl/ai-meta#49 — total cluster shard count.
    /// Envy maps `NOETL_SHARD_COUNT`.  When unset (or `1`),
    /// sharding is disabled — every replica owns every
    /// execution_id and `ShardConfig::owns` short-circuits to
    /// `true`.  Set cluster-wide; every replica MUST agree on
    /// this value or routing diverges.  See
    /// [sharding-design](https://github.com/noetl/server/wiki/sharding-design)
    /// for the layout.
    #[serde(default)]
    pub shard_count: Option<u32>,
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}

fn default_port() -> u16 {
    8082
}

fn default_server_name() -> String {
    "noetl-control-plane".to_string()
}

fn default_true() -> bool {
    true
}

fn default_sweep_interval() -> u64 {
    30
}

fn default_offline_seconds() -> u64 {
    60
}

impl AppConfig {
    /// Load configuration from environment variables.
    ///
    /// Environment variables are prefixed with `NOETL_`.
    pub fn from_env() -> Result<Self, envy::Error> {
        envy::prefixed("NOETL_").from_env::<AppConfig>()
    }

    /// Get the server bind address as a string suitable for `TcpListener::bind`.
    pub fn bind_address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            workers: None,
            debug: false,
            server_name: default_server_name(),
            nats_url: None,
            enable_gcp_token_api: true,
            disable_metrics: false,
            refs_in_state: false,
            projector_owns_snapshot: false,
            // noetl/ai-meta#108 (c): worker-driven drive is the default.
            orchestrate_plugin_drive: true,
            auto_recreate_runtime: true,
            runtime_sweep_interval: default_sweep_interval(),
            runtime_offline_seconds: default_offline_seconds(),
            public_server_url: None,
            server_machine_id: None,
            shard_index: None,
            shard_count: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert_eq!(config.host, "0.0.0.0");
        assert_eq!(config.port, 8082);
        assert!(!config.debug);
    }

    #[test]
    fn test_bind_address() {
        let config = AppConfig::default();
        assert_eq!(config.bind_address(), "0.0.0.0:8082");
    }

    #[test]
    fn test_orchestrate_drive_defaults_on() {
        // noetl/ai-meta#108 (c): the worker-driven orchestrator drive is the
        // default.  Revert is `NOETL_ORCHESTRATE_PLUGIN_DRIVE=false`.
        assert!(AppConfig::default().orchestrate_plugin_drive);
    }
}
