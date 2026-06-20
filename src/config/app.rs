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

    /// References-in-state (noetl/ai-meta#101 → #115 Phase 1).  When true, the
    /// orchestrator stops resolving over-budget result references back to inline
    /// data — it keeps `{reference, extracted}` (+ the `_ref`/`_store` locator
    /// accessors) on the event so the state + command context carry references,
    /// not bulk payloads (a 1.7MB step output no longer balloons every
    /// `command.issued`, and the drive state stays bounded).  The orchestrator
    /// evaluates `when:`/`set:` off the small `extracted` predicate block; the
    /// worker resolves the full reference at render time **only** for inputs that
    /// bind the bulk (`resolve_context_references` selective consume side).  Envy
    /// maps `NOETL_REFS_IN_STATE`.  **Default true** — the consume side (worker
    /// selective resolve + `_ref`/`_store` surfacing) landed with #115 Phase 1,
    /// so references stay out of state/commands by default.  Set
    /// `NOETL_REFS_IN_STATE=false` to revert to the resolve-inline behavior.
    #[serde(default = "default_true")]
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

    /// CQRS write-path cutover (noetl/ai-meta#103 phase 2d-3).  When true, every
    /// server-originated `noetl.event` write goes through the `emit_event`
    /// chokepoint as a **publish** to the `noetl_events` JetStream stream
    /// (instead of a synchronous `INSERT`), so the `system/event_materializer`
    /// playbook becomes the **sole** `noetl.event` writer.  The orchestrator
    /// trigger then fires from the materializer's write endpoint
    /// (`/api/internal/events/project`) rather than the synchronous ingest, so
    /// the drive still advances when writes are asynchronous.  Envy maps
    /// `NOETL_EVENT_INGEST_PUBLISH_ONLY`.
    ///
    /// **Default false** — the ingest path INSERTs synchronously exactly as
    /// today (byte-identical); the materializer runs in shadow (idempotent
    /// duplicates).  Flip on only with the materializer deployed + a lag
    /// metric/alert, one revert away (the producer cutover is an operator
    /// decision).  Requires NATS; a no-op (stays on the synchronous INSERT
    /// path) if NATS is not connected.
    #[serde(default)]
    pub event_ingest_publish_only: bool,

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

    /// Maximum serialised size (bytes) of a `command.issued` event's `context`
    /// before it is offloaded to the result store (noetl/ai-meta#114).  When the
    /// off-server orchestrate drive (`orchestrate_plugin_drive`) builds the next
    /// step's command with `refs_in_state` **false**, the full resolved upstream
    /// context is embedded inline in `render_context` — a large-context fixture
    /// (e.g. `test_output_select`) produced a 1.32MB `command.issued` event that
    /// exceeds NATS `max_payload` (1MB) under the publish-only gate, so the
    /// publish never acks and the execution wedges.  When the serialised
    /// `{tool_config, args, render_context}` exceeds this threshold,
    /// `persist_engine_command(s)` offloads the whole context to
    /// `noetl.result_store` and writes a tiny `{ "__context_ref__": "noetl://…" }`
    /// marker onto the event + command row instead; `get_command` / `claim_command`
    /// resolve the ref before handing the command to the worker (same
    /// result-store pattern as the #113 drive-result fix).  Envy maps
    /// `NOETL_COMMAND_CONTEXT_MAX_BYTES`.  **Default 524288 (512KB)** — comfortably
    /// under the 1MB NATS ceiling with event/meta overhead, and large enough that
    /// ordinary commands never offload (their context is a few KB).
    #[serde(default = "default_command_context_max_bytes")]
    pub command_context_max_bytes: usize,

    /// How the orchestrator drive reconstructs `WorkflowState` for an execution
    /// (RFC noetl/ai-meta#115 Phase 3).  Envy maps `NOETL_STATE_BUILD_MODE`.
    ///
    /// - **`event_scan`** (default) — the established path: bounded rebuild from
    ///   the latest `projection_snapshot` + an `event_id`-range scan of
    ///   `noetl.event`, or a full `WHERE execution_id = $1` scan when no snapshot
    ///   exists.  Unchanged prod behavior.
    /// - **`chain_walk`** — follow the one-level `prev_event_id` chain (Phase 2)
    ///   from the in-memory chain head back to the genesis event, each hop a
    ///   `(execution_id, event_id)` **PK lookup** — never a `WHERE execution_id`
    ///   scan of `noetl.event`.  The collected events are fed to the SAME
    ///   `WorkflowState::from_events`, so the built state is equivalent to the
    ///   event-scan build (parity by construction).  When the chain head is cold
    ///   (server restart / different replica), a walked node isn't yet
    ///   materialized (materializer lag under the gate), or the walk's root is
    ///   not a genesis event, the build **falls back to `event_scan`** for that
    ///   trigger (counted via `noetl_state_build_total{outcome}`) — correctness
    ///   is never sacrificed.  This is the in-process proof of tenet 3/4 before
    ///   the off-server builder + cache (Phase 4).
    ///
    /// **Default `event_scan`** — prod/default behavior is unchanged; flipping to
    /// `chain_walk` is opt-in.
    #[serde(default)]
    pub state_build_mode: StateBuildMode,

    /// When true, the drive builds `WorkflowState` **both** ways (event-scan AND
    /// chain-walk) on each cold/rebuild and asserts they are equal, recording a
    /// mismatch via `noetl_state_build_parity_total{result}` — the drive still
    /// **uses the configured `state_build_mode` result** for its decision (the
    /// shadow build is observation-only, so a parity bug can't change behavior).
    /// Envy maps `NOETL_STATE_BUILD_PARITY_CHECK`.  **Default false** — a
    /// validation/diagnostic switch, off in prod.
    #[serde(default)]
    pub state_build_parity_check: bool,

    /// **Where** orchestrator `WorkflowState` is constructed (RFC
    /// noetl/ai-meta#115 Phase 4).  Envy maps `NOETL_STATE_BUILDER`.
    ///
    /// - **`server`** (default) — the server builds `WorkflowState` in-process
    ///   (via [`Self::state_build_mode`]: `event_scan` or the Phase-3
    ///   `chain_walk`) and, under the off-server *drive* (#108), hands the
    ///   already-built state to the worker plug-in's `run_state` entry.  Prod
    ///   behavior; unchanged.
    /// - **`offserver`** — state CONSTRUCTION moves to the system worker pool:
    ///   the drive obtains its `WorkflowState` from the pool-side off-server
    ///   builder (which walks the `prev_event_id` chain from the `noetl_events`
    ///   **WAL** stream and caches the built spine keyed by the immutable chain
    ///   head — `noetl-worker`'s `state_builder`), so the server stops building
    ///   state on the hot path.  Phase 4 remainder (noetl/ai-meta#107 step 2):
    ///   the server edge is now **stateless** on the drive path — with a warm
    ///   execute-time descriptor (catalog_id + routing seeded at
    ///   `playbook_started`, terminal stamped at the emit chokepoint, the
    ///   dispatch watermark read from the in-memory `ChainHeads`) the drive
    ///   routes the command performing ZERO `noetl.event` reads + ZERO state
    ///   rebuild; the worker self-sources the spine.  The server chain-walk +
    ///   event-scan stay as the fallbacks (cold descriptor after a restart).
    ///
    /// **Default `server`** — prod/default behavior is unchanged.
    #[serde(default)]
    pub state_builder: StateBuilder,

    /// **How the execution-lifecycle hot path reads `noetl.event`** (RFC
    /// noetl/ai-meta#115 Phase 6).  Envy maps `NOETL_EVENT_READ_PATH`.
    ///
    /// Phase 4 already removed the *drive*'s state-rebuild scan under
    /// `state_builder=offserver`.  This flag retires the **remaining**
    /// execution-scan readers of `noetl.event` on the hot path — the
    /// `WHERE execution_id = $1` replay class that runs *outside* the drive:
    /// the per-ingest `get_catalog_id` (`normalize_event_to_row`), the
    /// child-execution `inherit_parent_trace`, the subscription dedup-audit
    /// catalog lookup, and the container-callback existence + catalog reads.
    ///
    /// - **`event_scan`** (default) — those readers scan `noetl.event` exactly
    ///   as today.  Unchanged prod behavior.
    /// - **`audit_only`** — each reader is served from the in-memory
    ///   execute-time [`crate::state::ExecDescriptor`] (catalog_id + routing,
    ///   seeded at `playbook_started`).  A warm descriptor → ZERO `noetl.event`
    ///   read; a cold descriptor (server restart mid-execution) **falls back**
    ///   to the scan for that read (counted via
    ///   `noetl_event_hotpath_reads_total{outcome="scan"}`) — correctness is
    ///   never sacrificed.  `noetl.event` becomes **audit-only**: still written
    ///   by the materializer (#103) and read by operator/status/replay APIs,
    ///   never scanned by the execution lifecycle.  Pairs with
    ///   `state_builder=offserver` to reach the end-to-end never-scan invariant.
    ///
    /// **Default `event_scan`** — prod/default behavior is unchanged; flipping
    /// to `audit_only` is opt-in and staged.
    #[serde(default)]
    pub event_read_path: EventReadPath,
}

/// How the execution-lifecycle hot path reads `noetl.event` — see
/// [`AppConfig::event_read_path`] (RFC noetl/ai-meta#115 Phase 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventReadPath {
    /// Hot-path readers scan `noetl.event` (`WHERE execution_id = $1`) exactly
    /// as today.  Default — prod behavior.
    #[default]
    EventScan,
    /// Hot-path readers are served from the in-memory execute-time descriptor;
    /// `noetl.event` becomes audit-only (RFC #115 Phase 6).  Cold-descriptor
    /// reads fall back to the scan.
    AuditOnly,
}

/// Where orchestrator `WorkflowState` is constructed — see
/// [`AppConfig::state_builder`] (RFC noetl/ai-meta#115 Phase 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StateBuilder {
    /// The server builds `WorkflowState` in-process (event-scan or chain-walk).
    /// Default — prod behavior.
    #[default]
    Server,
    /// State construction runs off-server on the system worker pool, reading the
    /// `noetl_events` WAL with a pool-side cache (RFC #115 Phase 4).
    Offserver,
}

/// Strategy for reconstructing orchestrator `WorkflowState` — see
/// [`AppConfig::state_build_mode`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StateBuildMode {
    /// Bounded snapshot + `event_id`-range / full scan of `noetl.event` (the
    /// established path).  Default.
    #[default]
    EventScan,
    /// Walk the `prev_event_id` chain head→root by PK lookup (RFC #115 Phase 3).
    ChainWalk,
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

fn default_command_context_max_bytes() -> usize {
    // 512KB — half the NATS 1MB max_payload, leaving headroom for the event's
    // meta/envelope overhead while never tripping on ordinary (few-KB) commands.
    512 * 1024
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
            // noetl/ai-meta#115 Phase 1: references stay out of state/commands by
            // default now that the worker selective-resolve consume side landed.
            refs_in_state: true,
            projector_owns_snapshot: false,
            // noetl/ai-meta#108 (c): worker-driven drive is the default.
            orchestrate_plugin_drive: true,
            // noetl/ai-meta#103 2d-3: synchronous INSERT path by default.
            event_ingest_publish_only: false,
            auto_recreate_runtime: true,
            runtime_sweep_interval: default_sweep_interval(),
            runtime_offline_seconds: default_offline_seconds(),
            public_server_url: None,
            server_machine_id: None,
            shard_index: None,
            shard_count: None,
            command_context_max_bytes: default_command_context_max_bytes(),
            // noetl/ai-meta#115 Phase 3: event-scan is the default; chain_walk is opt-in.
            state_build_mode: StateBuildMode::EventScan,
            state_build_parity_check: false,
            // noetl/ai-meta#115 Phase 4: server-side build is the default; the
            // off-server builder cutover is opt-in (and staged).
            state_builder: StateBuilder::Server,
            // noetl/ai-meta#115 Phase 6: hot-path event-scan readers stay on the
            // event table by default; audit_only routes them to the descriptor.
            event_read_path: EventReadPath::EventScan,
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

    #[test]
    fn test_command_context_max_bytes_default_under_nats_ceiling() {
        // noetl/ai-meta#114: the offload threshold must sit safely below the
        // NATS 1MB max_payload so an offloaded `command.issued` event always
        // fits, with headroom for the meta/envelope overhead.
        let cfg = AppConfig::default();
        assert_eq!(cfg.command_context_max_bytes, 512 * 1024);
        assert!(cfg.command_context_max_bytes < 1024 * 1024);
    }

    #[test]
    fn test_state_build_mode_defaults_event_scan() {
        // noetl/ai-meta#115 Phase 3: prod/default behavior is unchanged — the
        // chain-walk builder is opt-in via NOETL_STATE_BUILD_MODE=chain_walk.
        let cfg = AppConfig::default();
        assert_eq!(cfg.state_build_mode, StateBuildMode::EventScan);
        assert!(!cfg.state_build_parity_check);
    }

    #[test]
    fn test_state_build_mode_deserializes_snake_case() {
        // Envy parses NOETL_STATE_BUILD_MODE through serde; the variants are
        // snake_case so `chain_walk` / `event_scan` map cleanly.
        let cw: StateBuildMode = serde_json::from_str("\"chain_walk\"").unwrap();
        assert_eq!(cw, StateBuildMode::ChainWalk);
        let es: StateBuildMode = serde_json::from_str("\"event_scan\"").unwrap();
        assert_eq!(es, StateBuildMode::EventScan);
    }

    #[test]
    fn test_state_builder_defaults_server() {
        // noetl/ai-meta#115 Phase 4: prod/default behavior is unchanged — state
        // construction stays on the server; the off-server builder is opt-in via
        // NOETL_STATE_BUILDER=offserver (and the cutover wiring is staged).
        let cfg = AppConfig::default();
        assert_eq!(cfg.state_builder, StateBuilder::Server);
    }

    #[test]
    fn test_state_builder_deserializes_snake_case() {
        // Envy parses NOETL_STATE_BUILDER through serde; variants are snake_case.
        let off: StateBuilder = serde_json::from_str("\"offserver\"").unwrap();
        assert_eq!(off, StateBuilder::Offserver);
        let srv: StateBuilder = serde_json::from_str("\"server\"").unwrap();
        assert_eq!(srv, StateBuilder::Server);
    }

    #[test]
    fn test_event_read_path_defaults_event_scan() {
        // noetl/ai-meta#115 Phase 6: prod/default behavior is unchanged — the
        // hot-path event readers keep scanning noetl.event; audit_only (route to
        // the descriptor, noetl.event becomes audit-only) is opt-in via
        // NOETL_EVENT_READ_PATH=audit_only.
        let cfg = AppConfig::default();
        assert_eq!(cfg.event_read_path, EventReadPath::EventScan);
    }

    #[test]
    fn test_event_read_path_deserializes_snake_case() {
        // Envy parses NOETL_EVENT_READ_PATH through serde; variants are snake_case.
        let ao: EventReadPath = serde_json::from_str("\"audit_only\"").unwrap();
        assert_eq!(ao, EventReadPath::AuditOnly);
        let es: EventReadPath = serde_json::from_str("\"event_scan\"").unwrap();
        assert_eq!(es, EventReadPath::EventScan);
    }
}
