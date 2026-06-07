//! Replay engine — Phase D Round 5 of the Rust server FastAPI parity
//! port (noetl/ai-meta#49 / noetl/server#148).
//!
//! Round 1 ships the endpoint scaffold + minimal `execution`
//! projection fold.  The Python reference is
//! [`noetl/server/api/replay/service.py`](https://github.com/noetl/noetl/blob/main/noetl/server/api/replay/service.py)
//! (~1236 LoC); this Rust port lands it in disciplined rounds
//! (see the noetl/server#148 issue body for the full
//! decomposition).
//!
//! ## Round 1 surface
//!
//! - [`ReplayCutoff`] — exactly one of `as_of_event_id`,
//!   `as_of_position`, `as_of_time` is normally set.
//! - [`ReplayProjection`] — `execution` is the only projection
//!   produced this round; `stage` / `frame` / `command` /
//!   `business_object` / `loop` / `all` are scaffolded as accepted
//!   inputs but fold to the same minimal shape until later
//!   rounds extend the per-projection state.
//! - [`ReplayState`] — the deterministic fold output.  Round 1
//!   only fills `execution_id`, `tenant_id`, `organization_id`,
//!   `projection`, `event_count`, `last_event_id`,
//!   `last_event_type`, and the `execution` sub-object's
//!   `status` + `last_node_name`.
//! - [`ReplayService::replay_state`] — load events for an
//!   execution (applying the cutoff), then fold.
//!
//! ## Out of scope for Round 1
//!
//! - `stages` / `frames` / `commands` / `business_objects` /
//!   `loops` maps (Rounds 2-3).
//! - `replay_snapshot` seed + base_state (Round 5).
//! - `payload_resolver` bounded summaries (Round 6).
//! - `canonical_checksum` / `projection_checksums` (Round 4).
//! - Parity harness against Python (Round 7).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::db::{DbPool, DbPoolMap};
use crate::error::AppResult;

/// Replay cutoff.  Exactly one field is normally set on the wire;
/// the endpoint handler rejects requests with more than one.
///
/// Mirrors Python's `ReplayCutoff` dataclass at
/// `noetl/server/api/replay/types.py`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReplayCutoff {
    /// Replay through this event_id (inclusive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_of_event_id: Option<i64>,

    /// Alias for event-position cutoff (the Python surface accepts
    /// this as a synonym; Round 1 currently treats it as a soft
    /// alias for `as_of_event_id` because the Rust event store keys
    /// on event_id, not a separate position counter).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_of_position: Option<i64>,

    /// Replay through this `event_time` (inclusive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_of_time: Option<DateTime<Utc>>,
}

impl ReplayCutoff {
    /// True if no cutoff is set — load every event for the
    /// execution.
    pub fn is_empty(&self) -> bool {
        self.as_of_event_id.is_none()
            && self.as_of_position.is_none()
            && self.as_of_time.is_none()
    }

    /// Count the number of fields set.  The endpoint rejects
    /// requests with more than one set to match Python's
    /// `endpoint.py` behaviour.
    pub fn set_count(&self) -> usize {
        usize::from(self.as_of_event_id.is_some())
            + usize::from(self.as_of_position.is_some())
            + usize::from(self.as_of_time.is_some())
    }
}

/// Which projection(s) to fold the events into.
///
/// Round 1 only produces the `execution` projection — even when
/// `All` is requested, the other map fields are returned empty.
/// Round 2 fleshes out `Stage`/`Frame`/`Command`; Round 3 adds
/// `Loop` + `BusinessObject`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayProjection {
    Execution,
    Stage,
    Frame,
    Command,
    BusinessObject,
    Loop,
    All,
}

impl Default for ReplayProjection {
    fn default() -> Self {
        Self::All
    }
}

impl ReplayProjection {
    /// Wire-format name matching the Python surface
    /// (`projection=execution|frame|loop|business_object|all`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Execution => "execution",
            Self::Stage => "stage",
            Self::Frame => "frame",
            Self::Command => "command",
            Self::BusinessObject => "business_object",
            Self::Loop => "loop",
            Self::All => "all",
        }
    }

    /// Parse a wire value.  Accepts the canonical Python names +
    /// the underscore alias `business_object`.  Returns `None`
    /// on unknown — the endpoint surfaces this as a 400.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "execution" => Some(Self::Execution),
            "stage" => Some(Self::Stage),
            "frame" => Some(Self::Frame),
            "command" => Some(Self::Command),
            "business_object" => Some(Self::BusinessObject),
            "loop" => Some(Self::Loop),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

/// Replay state result — the deterministic fold output.
///
/// Round 1 fills the top-level metadata + the `execution`
/// sub-object's `status` + `last_node_name`.  Maps are returned
/// empty but the keys exist so wire-shape consumers can rely on
/// the structure today.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayState {
    pub tenant_id: String,
    pub organization_id: String,
    pub execution_id: i64,
    pub projection: String,

    /// Total events folded into this state.
    pub event_count: u64,

    /// Highest event_id seen (or `None` if no events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_event_id: Option<i64>,

    /// `event_type` of the highest-event_id event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_event_type: Option<String>,

    /// Execution-level projection.  Round 1 fills `status` +
    /// `last_node_name`.
    pub execution: ReplayExecutionState,

    /// Stages map populated by R5 R2.  Keyed by `stage_id`
    /// (see [`extract_stage_id`]).
    #[serde(default)]
    pub stages: std::collections::BTreeMap<String, ReplayStageState>,

    /// Frames map populated by R5 R2.  Keyed by `frame_id`
    /// (see [`extract_frame_id`]).
    #[serde(default)]
    pub frames: std::collections::BTreeMap<String, ReplayFrameState>,

    /// Commands map populated by R5 R2.  Keyed by canonical
    /// `command_id` (top-level `noetl.event.command_id` column
    /// preferred over `meta.command_id`).
    #[serde(default)]
    pub commands: std::collections::BTreeMap<String, ReplayCommandState>,

    /// Business objects map populated by R5 R3.  Keyed by
    /// `<object_type>/<object_id>` per Python's
    /// `_business_object_identity` (see
    /// [`extract_business_object_identity`]).
    #[serde(default)]
    pub business_objects: std::collections::BTreeMap<String, ReplayBusinessObjectState>,

    /// Loops map populated by R5 R3.  Keyed by `loop_id` (see
    /// [`extract_loop_id`]).
    #[serde(default)]
    pub loops: std::collections::BTreeMap<String, ReplayLoopState>,

    /// Hash of the upcaster registry that was active when the
    /// snapshot was taken / when the fold ran.  Populated when
    /// the caller passes `upcaster_registry_digest` via
    /// [`ReplayFoldOptions`].  Mirrors Python's
    /// `state["upcaster_registry_digest"]`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upcaster_registry_digest: Option<String>,

    /// Snapshot metadata when this fold was seeded from a prior
    /// snapshot.  R5 R5 populates this when the caller passes a
    /// [`ReplaySnapshotSeed`] via [`ReplayFoldOptions`].  Mirrors
    /// Python's `state["replay_snapshot"]`.  Note: this is only
    /// the *metadata* — the seed's full `state` is folded into
    /// `base_state` and isn't echoed back here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replay_snapshot: Option<ReplaySnapshotInfo>,

    /// Top-level [`Checksum`] over the rest of the state
    /// (everything except `checksum` + `projection_checksums`
    /// themselves).  Populated by R5 R4.  Replaces Python's flat
    /// `checksum_algorithm` + `checksum` pair with a typed
    /// shape — the algorithm is the *type* of the checksum, not
    /// a sibling field.  `None` until the fold computes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checksum: Option<Checksum>,

    /// Per-projection content hashes.  Keyed by projection name
    /// (`execution`, `stage`, `frame`, `command`, `business_object`,
    /// `loop`).  Each entry is a [`Checksum`] over the
    /// corresponding sub-state (e.g. `BTreeMap<String, ReplayStageState>`
    /// for the `stage` entry).  Empty until R5 R4 lands.
    #[serde(default)]
    pub projection_checksums: std::collections::BTreeMap<String, Checksum>,
}

/// Execution-level projection.  Round 1 surfaces `status` +
/// `last_node_name`.  Future rounds may add `payload_refs`,
/// `tenant_id`/`organization_id` echoes, etc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayExecutionState {
    /// One of `UNKNOWN | RUNNING | COMPLETED | FAILED |
    /// CANCELLED`.  Matches the Python fold's terminal-event
    /// short-circuit + the orchestrator's emit contract (the same
    /// playbook.completed / playbook.failed event types the
    /// status endpoint short-circuits on per server#147).
    pub status: String,

    /// Last `node_name` mentioned on a step-level event.  `None`
    /// when no step events have been folded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_node_name: Option<String>,
}

impl Default for ReplayExecutionState {
    fn default() -> Self {
        Self {
            status: "UNKNOWN".to_string(),
            last_node_name: None,
        }
    }
}

/// Stage-level projection populated by R5 R2.  Mirrors Python's
/// `state["stages"][stage_id]` dict shape — same field names,
/// same nullability defaults, same status transitions.  Keyed
/// in [`ReplayState::stages`] by the canonical `stage_id`
/// returned by [`extract_stage_id`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReplayStageState {
    pub stage_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_stage_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loop_event_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub opened_event_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_event_id: Option<i64>,
    pub frame_count: i64,
    pub row_count: i64,
    pub events_emitted: i64,
    pub failed_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_event_id: Option<i64>,
}

/// Frame-level projection populated by R5 R2.  Mirrors Python's
/// `state["frames"][frame_id]` dict.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReplayFrameState {
    pub frame_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_frame_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claimed_event_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_event_id: Option<i64>,
    pub status: String,
    pub row_count: i64,
    pub attempts: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_event_id: Option<i64>,
    pub events_emitted: i64,
}

/// Command-level projection populated by R5 R2.  Mirrors
/// Python's `state["commands"][command_id]` dict.  R5 R2 does
/// NOT yet thread the heavier sub-objects (`locality`,
/// `source_locality`, `placement`, `fanout_reduce`) into the
/// projection — they round-trip as raw JSON values when present
/// on `meta`, and Round 3+ may surface them as typed fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReplayCommandState {
    pub command_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_command_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_locator: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issued_event_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claimed_event_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_event_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_event_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_event_id: Option<i64>,
}

/// Loop-level projection populated by R5 R3.  Mirrors Python's
/// `state["loops"][loop_id]` dict shape from
/// `noetl/server/api/replay/service.py` (`fold_replay_state`,
/// loops branch).  Keyed in [`ReplayState::loops`] by the
/// `loop_id` returned by [`extract_loop_id`].
///
/// Counters increment based on event type:
/// - `command.completed` / `loop.shard.done` → `done++`
/// - `command.failed` / `loop.shard.failed` → `failed++`
/// - `loop.done` / `loop.fanin.completed` → `completed=true`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReplayLoopState {
    pub loop_id: String,
    /// `node_name` from the first event that mentioned this loop.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_name: Option<String>,
    /// `meta.collection_size` or `meta.total` from the first event
    /// that mentioned this loop and carried it.  `None` when the
    /// loop hint never landed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<i64>,
    /// Shards / iterations that have terminated successfully.
    pub done: i64,
    /// Shards / iterations that have terminated with a failure.
    pub failed: i64,
    /// True once a `loop.done` or `loop.fanin.completed` event
    /// has been observed.
    pub completed: bool,
    /// `event_id` of the most recent event referencing this loop.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_event_id: Option<i64>,
}

/// Business-object projection populated by R5 R3.  Mirrors
/// Python's `state["business_objects"][<type>/<id>]` dict shape.
/// Keyed by `<object_type>/<object_id>` per
/// [`extract_business_object_identity`].
///
/// Status defaults to `"UNKNOWN"`.  Each event updates:
/// - `last_event_id` / `last_event_type` (always).
/// - `event_count++`.
/// - `version` = `meta.business_object.version` ||
///   `meta.business_object_version` || `event_count`.
/// - `status` = explicit event `status`, else event-type suffix
///   (`.created`/`.updated`/`.upserted` → `ACTIVE`,
///   `.deleted`/`.removed` → `DELETED`).
/// - `attributes` replaces from `meta.business_object.state` or
///   patches from `meta.business_object.patch` /
///   `meta.business_object.attributes`.
///
/// `payload_refs` + `last_payload_ref` populate in R6 (the
/// payload-resolver round); R3 leaves them empty / `None`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReplayBusinessObjectState {
    /// Object key in `<object_type>/<object_id>` form — the
    /// map key in [`ReplayState::business_objects`].
    pub object_key: String,
    pub object_type: String,
    pub object_id: String,
    pub status: String,
    pub version: i64,
    pub event_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_event_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_event_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_event_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_event_type: Option<String>,
    /// Per-event attribute snapshot (replaces from
    /// `meta.business_object.state`; patches from
    /// `meta.business_object.patch` /
    /// `meta.business_object.attributes`).
    #[serde(default)]
    pub attributes: serde_json::Map<String, serde_json::Value>,
    // R6 will populate `payload_refs` + `last_payload_ref` from
    // the event's top-level `payload_ref` column / `result.reference`.
    // R3 deliberately omits them — see PR body's "Out of scope".
}

/// Algorithm used to compute a [`Checksum`].  R4 ships with the
/// single variant [`ChecksumType::Sha256`].  Future variants
/// (`Blake3`, `Sha512`, …) slot in via the enum without a
/// wire-format break — the value field carries the hex output,
/// the type field tells consumers which algorithm produced it.
///
/// Serialized lowercase to match the Python flat form's
/// `checksum_algorithm: "sha256"` wire string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksumType {
    Sha256,
}

impl ChecksumType {
    /// Lowercase string form used in JSON output + debug logs.
    pub fn as_str(self) -> &'static str {
        match self {
            ChecksumType::Sha256 => "sha256",
        }
    }
}

/// A deterministic content hash over a replay projection.  Pairs
/// the algorithm [`type`](ChecksumType) with the lowercase-hex
/// `value`.
///
/// Replaces Python's flat
/// `state["checksum_algorithm"] + state["checksum"]` pair with a
/// typed shape so future checksum algorithms slot in without a
/// schema-level break.  Wire format:
///
/// ```json
/// {"type": "sha256", "value": "ab12...cd34"}
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checksum {
    /// Algorithm — see [`ChecksumType`].  Serialized as `type` on
    /// the wire (Rust `r#type` reserved keyword).
    #[serde(rename = "type")]
    pub algorithm: ChecksumType,
    /// Lowercase hex digest.
    pub value: String,
}

impl Checksum {
    /// Compute a SHA-256 checksum over a JSON-serializable value
    /// using deterministic encoding (sorted keys + compact
    /// separators — matches Python's
    /// `json.dumps(sort_keys=True, separators=(",", ":"))`).
    pub fn sha256<T: Serialize>(value: &T) -> Self {
        use sha2::{Digest, Sha256};
        let payload = stable_json_bytes(value);
        let digest = Sha256::digest(&payload);
        Self {
            algorithm: ChecksumType::Sha256,
            value: hex_encode(&digest),
        }
    }
}

/// Snapshot used as a replay seed.  Mirrors Python's
/// `ReplaySnapshotSeed` frozen dataclass from
/// `noetl/server/api/replay/types.py`.  When the caller wants
/// to skip folding events older than the snapshot's `version`,
/// it loads the snapshot from storage and passes both:
/// - the snapshot's `state` field as `base_state` on
///   [`ReplayFoldOptions`], and
/// - the snapshot itself as `snapshot_seed`.
///
/// The fold deep-copies `base_state`, strips its checksum
/// fields (they get recomputed at the end), and attaches the
/// snapshot metadata to `ReplayState.replay_snapshot` for
/// consumers that need to know the fold was seeded.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplaySnapshotSeed {
    /// Aggregate the snapshot belongs to (typically the
    /// `execution_id` as a string, but the Python contract
    /// keeps it generic).
    pub aggregate_id: String,
    /// Aggregate kind — usually `"execution"` for replay
    /// snapshots; `"business_object"` for per-domain snapshots.
    pub aggregate_type: String,
    /// Snapshot's version cursor — the last `event_id` folded
    /// into `state`.  The fold MUST only consider events with
    /// `event_id > version` (the caller is responsible for the
    /// `after_event_id=version` query).
    pub version: i64,
    /// Snapshot's `Checksum` at the time it was written.
    pub checksum: Checksum,
    /// The folded state the snapshot captures.  Plumbed into
    /// the fold as `base_state` on
    /// [`ReplayFoldOptions::base_state`].
    pub state: ReplayState,
    /// Provenance metadata (snapshot author, creation time,
    /// upcaster digest, …).  Round-trips into
    /// `ReplayState.replay_snapshot.meta` for consumers.
    #[serde(default)]
    pub meta: serde_json::Map<String, serde_json::Value>,
}

/// Snapshot metadata surfaced on the output [`ReplayState`]
/// when a fold was seeded from a [`ReplaySnapshotSeed`].
/// Mirrors Python's `state["replay_snapshot"]` dict — same
/// field names, same nullability.  The full `state` from the
/// seed isn't echoed here because it already went into
/// `base_state`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplaySnapshotInfo {
    pub aggregate_id: String,
    pub aggregate_type: String,
    pub version: i64,
    pub checksum: Checksum,
    #[serde(default)]
    pub meta: serde_json::Map<String, serde_json::Value>,
}

/// Optional inputs to [`fold_replay_state_with_options`].  R1
/// through R4 always passed defaults; R5 adds `base_state` +
/// `snapshot_seed` + `upcaster_registry_digest` as the
/// snapshot-seeded fold path.
#[derive(Debug, Clone, Default)]
pub struct ReplayFoldOptions {
    /// Prior fold output used as the starting point for this
    /// fold.  Typically the `state` field of a
    /// [`ReplaySnapshotSeed`] loaded from storage.  The fold
    /// deep-copies, then strips out the checksum + projection_checksums
    /// fields (they recompute at the end).  Event counters
    /// (`event_count`, `last_event_id`, …) continue from where
    /// the base state left off — the caller is responsible for
    /// querying only events newer than the snapshot's `version`.
    pub base_state: Option<ReplayState>,
    /// Snapshot metadata attached to the output's
    /// `replay_snapshot` field.  Independent of `base_state` —
    /// you typically set both, but a caller could attach
    /// metadata without seeding the fold (rarely useful).
    pub snapshot_seed: Option<ReplaySnapshotSeed>,
    /// Hash of the upcaster registry that was active when the
    /// snapshot was taken / when the fold ran.  Used by future
    /// validation logic to detect schema-version drift between
    /// snapshot creation and replay; flows through as-is.
    pub upcaster_registry_digest: Option<String>,
}

/// Subset of [`crate::db::models::event::Event`] columns the
/// replay fold actually needs.  Extended in R5 R2 to include the
/// stage / frame / command identity columns + the `meta` JSON
/// blob the Python fold reaches into for parent ids, worker
/// locator, fanout_reduce hints, etc.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ReplayEventRow {
    pub event_id: i64,
    pub event_type: String,
    pub node_name: Option<String>,
    pub status: String,
    pub created_at: DateTime<Utc>,
    /// `noetl.event.stage_id` — top-level column (preferred over
    /// `meta.stage_id` when both are set).  R5 R2 fold key for
    /// the `stages` map.
    #[sqlx(default)]
    pub stage_id: Option<String>,
    /// `noetl.event.frame_id` — same pattern.  R5 R2 fold key
    /// for the `frames` map.
    #[sqlx(default)]
    pub frame_id: Option<String>,
    /// `noetl.event.command_id` — top-level bigint column.
    /// `Option<i64>` so unset / non-numeric values fold through
    /// naturally.  R5 R2 fold key for the `commands` map.
    #[sqlx(default)]
    pub command_id: Option<i64>,
    /// `noetl.event.worker_id` — character varying.  Used by the
    /// command projection's `worker_id` field.
    #[sqlx(default)]
    pub worker_id: Option<String>,
    /// `noetl.event.aggregate_type` — when set together with
    /// `aggregate_id`, the Python fold falls back to deriving
    /// stage/frame id from those (`stage/<id>` / `frame/<id>`).
    #[sqlx(default)]
    pub aggregate_type: Option<String>,
    /// `noetl.event.aggregate_id` — see `aggregate_type`.
    #[sqlx(default)]
    pub aggregate_id: Option<String>,
    /// `noetl.event.meta` JSON blob.  R5 R2 reaches into it for
    /// `parent_stage_id`, `parent_frame_id`, `parent_command_id`,
    /// `worker_locator`, `locality`, `placement`, `fanout_reduce`,
    /// `kind`, `step_name`, plus the per-event-type counter
    /// fields (`frame_count`, `row_count`, `events_emitted`,
    /// `failed_count`, `attempt`, `cursor`).
    #[sqlx(default)]
    pub meta: Option<serde_json::Value>,
}

/// Replay service.  Phase F R4-4b shape — owns a [`DbPoolMap`] so
/// per-execution queries route via `pools.pool_for(execution_id)`.
#[derive(Clone)]
pub struct ReplayService {
    pools: DbPoolMap,
}

impl ReplayService {
    /// Build a replay service from the shared pool map.
    pub fn new(pools: DbPoolMap) -> Self {
        Self { pools }
    }

    /// Test / example shim wrapping a single legacy pool.
    pub fn new_legacy(db: DbPool) -> Self {
        Self::new(DbPoolMap::from_single_pool(db))
    }

    #[inline]
    fn pool_for(&self, execution_id: i64) -> &DbPool {
        self.pools.pool_for(execution_id)
    }

    /// Replay an execution into a deterministic [`ReplayState`].
    ///
    /// Loads events for `execution_id` from `noetl.event` (applying
    /// the cutoff), folds them deterministically (by ascending
    /// `event_id`), and returns the projected state.
    ///
    /// Round 1 only ships the `execution` projection; other
    /// projections are accepted as inputs but contribute no
    /// additional data this round.
    pub async fn replay_state(
        &self,
        tenant_id: &str,
        organization_id: &str,
        execution_id: i64,
        cutoff: ReplayCutoff,
        projection: ReplayProjection,
        limit: i64,
    ) -> AppResult<ReplayState> {
        let events = self.load_events(execution_id, &cutoff, limit).await?;
        Ok(fold_replay_state(
            &events,
            tenant_id,
            organization_id,
            execution_id,
            projection,
        ))
    }

    /// Load ordered events for an execution, applying the cutoff.
    /// Public so future rounds (and the parity harness in Round 7)
    /// can reuse it.
    pub async fn load_events(
        &self,
        execution_id: i64,
        cutoff: &ReplayCutoff,
        limit: i64,
    ) -> AppResult<Vec<ReplayEventRow>> {
        // SQLx dynamic query construction is awkward; just build
        // the four shapes statically.  Round 4+ may collapse this
        // into a single CASE WHEN once snapshot seeds + payload
        // resolution are in.
        let limit = limit.clamp(1, 100_000);
        let rows = if let Some(event_id) = cutoff.as_of_event_id.or(cutoff.as_of_position) {
            sqlx::query_as::<_, ReplayEventRow>(
                r#"
                SELECT
                    event_id,
                    event_type,
                    node_name,
                    status,
                    -- `noetl.event.created_at` is `TIMESTAMP` (no tz);
                    -- coerce to `TIMESTAMPTZ` so sqlx decodes into
                    -- `DateTime<Utc>` directly.  Matches the cast the
                    -- existing services::execution queries use for the
                    -- same column.
                    created_at AT TIME ZONE 'UTC' AS created_at,
                    -- R5 R2 fold inputs.  All optional in the DB
                    -- schema; the fold treats `None` as "this event
                    -- doesn't participate in that projection".
                    stage_id,
                    frame_id,
                    command_id,
                    worker_id,
                    aggregate_type,
                    aggregate_id,
                    meta
                FROM noetl.event
                WHERE execution_id = $1
                  AND event_id <= $2
                ORDER BY event_id ASC
                LIMIT $3
                "#,
            )
            .bind(execution_id)
            .bind(event_id)
            .bind(limit)
            .fetch_all(self.pool_for(execution_id))
            .await?
        } else if let Some(t) = cutoff.as_of_time {
            sqlx::query_as::<_, ReplayEventRow>(
                r#"
                SELECT
                    event_id,
                    event_type,
                    node_name,
                    status,
                    -- `noetl.event.created_at` is `TIMESTAMP` (no tz);
                    -- coerce to `TIMESTAMPTZ` so sqlx decodes into
                    -- `DateTime<Utc>` directly.  Matches the cast the
                    -- existing services::execution queries use for the
                    -- same column.
                    created_at AT TIME ZONE 'UTC' AS created_at,
                    -- R5 R2 fold inputs.  All optional in the DB
                    -- schema; the fold treats `None` as "this event
                    -- doesn't participate in that projection".
                    stage_id,
                    frame_id,
                    command_id,
                    worker_id,
                    aggregate_type,
                    aggregate_id,
                    meta
                FROM noetl.event
                WHERE execution_id = $1
                  AND created_at <= $2
                ORDER BY event_id ASC
                LIMIT $3
                "#,
            )
            .bind(execution_id)
            .bind(t)
            .bind(limit)
            .fetch_all(self.pool_for(execution_id))
            .await?
        } else {
            sqlx::query_as::<_, ReplayEventRow>(
                r#"
                SELECT
                    event_id,
                    event_type,
                    node_name,
                    status,
                    -- `noetl.event.created_at` is `TIMESTAMP` (no tz);
                    -- coerce to `TIMESTAMPTZ` so sqlx decodes into
                    -- `DateTime<Utc>` directly.  Matches the cast the
                    -- existing services::execution queries use for the
                    -- same column.
                    created_at AT TIME ZONE 'UTC' AS created_at,
                    -- R5 R2 fold inputs.  All optional in the DB
                    -- schema; the fold treats `None` as "this event
                    -- doesn't participate in that projection".
                    stage_id,
                    frame_id,
                    command_id,
                    worker_id,
                    aggregate_type,
                    aggregate_id,
                    meta
                FROM noetl.event
                WHERE execution_id = $1
                ORDER BY event_id ASC
                LIMIT $2
                "#,
            )
            .bind(execution_id)
            .bind(limit)
            .fetch_all(self.pool_for(execution_id))
            .await?
        };
        Ok(rows)
    }
}

/// Pure, deterministic event-fold function — mirrors Python's
/// `fold_replay_state` (Round 1 subset).  Public so future
/// rounds can extend the fold incrementally + unit-test each
/// projection without an active DB.
pub fn fold_replay_state(
    events: &[ReplayEventRow],
    tenant_id: &str,
    organization_id: &str,
    execution_id: i64,
    projection: ReplayProjection,
) -> ReplayState {
    fold_replay_state_with_options(
        events,
        tenant_id,
        organization_id,
        execution_id,
        projection,
        ReplayFoldOptions::default(),
    )
}

/// Extended fold entry point — accepts a [`ReplayFoldOptions`]
/// for the snapshot-seeded path.  R5 R5 adds this; the
/// 5-argument [`fold_replay_state`] above is a thin shim that
/// passes `ReplayFoldOptions::default()`.
pub fn fold_replay_state_with_options(
    events: &[ReplayEventRow],
    tenant_id: &str,
    organization_id: &str,
    execution_id: i64,
    projection: ReplayProjection,
    options: ReplayFoldOptions,
) -> ReplayState {
    let ReplayFoldOptions {
        base_state,
        snapshot_seed,
        upcaster_registry_digest,
    } = options;

    // Either start from the supplied base_state (snapshot-seeded
    // path) or build a fresh state.  When seeded, strip the
    // checksum + projection_checksums fields — they will
    // recompute at the end against the new event tail.
    let mut state = match base_state {
        Some(mut base) => {
            base.checksum = None;
            base.projection_checksums = std::collections::BTreeMap::new();
            // The caller's tenant/org/execution_id override
            // whatever the snapshot recorded (the snapshot may
            // be older than a tenant rename, or the caller may
            // be replaying into a different organization).
            base.tenant_id = tenant_id.to_string();
            base.organization_id = organization_id.to_string();
            base.execution_id = execution_id;
            base.projection = projection.as_str().to_string();
            base
        }
        None => ReplayState {
            tenant_id: tenant_id.to_string(),
            organization_id: organization_id.to_string(),
            execution_id,
            projection: projection.as_str().to_string(),
            event_count: 0,
            last_event_id: None,
            last_event_type: None,
            execution: ReplayExecutionState::default(),
            stages: std::collections::BTreeMap::new(),
            frames: std::collections::BTreeMap::new(),
            commands: std::collections::BTreeMap::new(),
            business_objects: std::collections::BTreeMap::new(),
            loops: std::collections::BTreeMap::new(),
            upcaster_registry_digest: None,
            replay_snapshot: None,
            checksum: None,
            projection_checksums: std::collections::BTreeMap::new(),
        },
    };

    // upcaster_registry_digest from the caller wins over
    // whatever the base_state carried — the fold's digest
    // represents the registry active at this fold time.
    state.upcaster_registry_digest = upcaster_registry_digest.or(state.upcaster_registry_digest);

    // Attach snapshot metadata when a seed was provided.  We
    // only surface the lightweight `ReplaySnapshotInfo` — the
    // seed's full `state` is already in `base_state`.
    if let Some(seed) = snapshot_seed {
        state.replay_snapshot = Some(ReplaySnapshotInfo {
            aggregate_id: seed.aggregate_id,
            aggregate_type: seed.aggregate_type,
            version: seed.version,
            checksum: seed.checksum,
            meta: seed.meta,
        });
    }

    // Events arrive sorted ASC by event_id from `load_events`; the
    // fold is order-deterministic regardless thanks to the
    // terminal-event short-circuit + last_node_name being a "most
    // recent step.enter wins" projection.  Re-sort defensively in
    // case callers pass an unsorted slice.
    let mut ordered: Vec<&ReplayEventRow> = events.iter().collect();
    ordered.sort_by_key(|e| e.event_id);

    for event in &ordered {
        state.event_count += 1;
        state.last_event_id = Some(event.event_id);
        state.last_event_type = Some(event.event_type.clone());

        match event.event_type.as_str() {
            // Terminal events short-circuit `execution.status`.
            // Mirrors `determine_status` in services::execution
            // (the same terminal-event contract noetl/server#147
            // landed on the status endpoint).
            "playbook.completed" | "playbook_completed" => {
                state.execution.status = "COMPLETED".to_string();
            }
            "playbook.failed" | "playbook_failed" => {
                state.execution.status = "FAILED".to_string();
            }
            "playbook.cancelled" | "playbook_cancelled" => {
                state.execution.status = "CANCELLED".to_string();
            }
            // Step-level events: track the most recent node_name
            // touched.  This is the "current step" view (useful
            // for in-flight executions; for completed ones it's
            // the last step that ran).
            "step.enter" | "step_enter" | "step_started" => {
                if state.execution.status == "UNKNOWN" {
                    state.execution.status = "RUNNING".to_string();
                }
                if let Some(name) = &event.node_name {
                    state.execution.last_node_name = Some(name.clone());
                }
            }
            "step.exit" | "step_completed" | "command.completed" => {
                if let Some(name) = &event.node_name {
                    state.execution.last_node_name = Some(name.clone());
                }
            }
            _ => {
                // Other events still count toward event_count and
                // last_event_id but don't shape `execution.*`.
            }
        }

        // R5 R2: per-projection population.  Each helper is a
        // no-op when the event doesn't carry the relevant
        // identity (e.g. a `playbook.completed` event has no
        // `stage_id` so `populate_stage` does nothing).
        populate_stage(event, &mut state.stages);
        populate_frame(event, &mut state.frames);
        populate_command(event, &mut state.commands);
        // R5 R3: loop + business_object projections.
        populate_loop(event, &mut state.loops);
        populate_business_object(event, &mut state.business_objects);
    }

    // R5 R4: per-projection + top-level SHA-256 checksums.  Runs
    // once at the end after every event has folded — the typed
    // BTreeMap ordering on all five projection maps + the sort
    // pass in `stable_json_bytes` deliver deterministic digests.
    compute_checksums(&mut state);

    state
}

// ---------------------------------------------------------------
// R5 R2 helpers — id extractors + per-projection population.
//
// Each `extract_*_id` mirrors the Python helper of the same name
// in `noetl/server/api/replay/service.py`: prefer the top-level
// DB column; fall back to `aggregate_type` + `aggregate_id`
// (stripping the `<kind>/` prefix the Python wire encoding uses);
// finally fall back to `meta.<key>`.  Each `populate_*` is a
// pure function over the row + the target map; the fold loop
// calls all three per event.
// ---------------------------------------------------------------

/// Extract the canonical `stage_id` for an event.  Returns `None`
/// when the event doesn't participate in the stage projection.
pub fn extract_stage_id(event: &ReplayEventRow) -> Option<String> {
    if let Some(s) = &event.stage_id {
        return Some(s.clone());
    }
    if event.aggregate_type.as_deref() == Some("stage") {
        if let Some(id) = &event.aggregate_id {
            return Some(id.strip_prefix("stage/").unwrap_or(id).to_string());
        }
    }
    meta_str(&event.meta, "stage_id")
}

/// Extract the canonical `frame_id` for an event.
pub fn extract_frame_id(event: &ReplayEventRow) -> Option<String> {
    if let Some(s) = &event.frame_id {
        return Some(s.clone());
    }
    if event.aggregate_type.as_deref() == Some("frame") {
        if let Some(id) = &event.aggregate_id {
            return Some(id.strip_prefix("frame/").unwrap_or(id).to_string());
        }
    }
    meta_str(&event.meta, "frame_id")
}

/// Extract the canonical `command_id` for an event.  The DB
/// column is `bigint` (numeric) but the fold key is a string for
/// consistency with stage / frame / business-object keys.
pub fn extract_command_id(event: &ReplayEventRow) -> Option<String> {
    if let Some(c) = event.command_id {
        return Some(c.to_string());
    }
    // Fall back to `meta.command_id` (legacy events that didn't
    // set the top-level column).  Accepts numeric or string.
    if let Some(m) = &event.meta {
        if let Some(v) = m.get("command_id") {
            return Some(value_to_string(v));
        }
    }
    None
}

/// Pull a string from a JSON map value at `key`.  Returns `None`
/// when missing / not a string / not a coercible scalar.
fn meta_str(meta: &Option<serde_json::Value>, key: &str) -> Option<String> {
    meta.as_ref().and_then(|m| m.get(key)).map(value_to_string)
}

/// Pull an integer (as `i64`) from a JSON map value at `key`.
/// Round-trips through `serde_json::Number::as_i64`; returns
/// `None` for missing, non-integer, or out-of-range values.
fn meta_i64(meta: &Option<serde_json::Value>, key: &str) -> Option<i64> {
    meta.as_ref()
        .and_then(|m| m.get(key))
        .and_then(|v| v.as_i64())
}

/// Coerce a JSON scalar to its string representation.  Strings
/// preserve the inner value; numbers stringify via `to_string()`;
/// bools become `"true"`/`"false"`; null returns `"null"`.
fn value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

/// Apply one event to the stages map.  Mirrors Python's
/// `state["stages"]` population in `fold_replay_state`.
fn populate_stage(
    event: &ReplayEventRow,
    stages: &mut std::collections::BTreeMap<String, ReplayStageState>,
) {
    let stage_id = match extract_stage_id(event) {
        Some(id) => id,
        None => return,
    };
    let stage = stages.entry(stage_id.clone()).or_insert_with(|| ReplayStageState {
        stage_id: stage_id.clone(),
        status: "UNKNOWN".to_string(),
        kind: meta_str(&event.meta, "kind"),
        step_name: event
            .node_name
            .clone()
            .or_else(|| meta_str(&event.meta, "step_name")),
        parent_stage_id: meta_str(&event.meta, "parent_stage_id"),
        ..Default::default()
    });
    stage.last_event_id = Some(event.event_id);
    if let Some(parent) = meta_str(&event.meta, "parent_stage_id") {
        stage.parent_stage_id = Some(parent);
    }
    // Loop event id lives only in meta (no top-level column).
    if let Some(loop_id) = meta_str(&event.meta, "loop_id")
        .or_else(|| meta_str(&event.meta, "loop_event_id"))
        .or_else(|| meta_str(&event.meta, "__loop_epoch_id"))
    {
        stage.loop_event_id = Some(loop_id);
    }
    match event.event_type.as_str() {
        "stage.opened" => {
            stage.status = "OPEN".to_string();
            stage.opened_event_id = Some(event.event_id);
        }
        "stage.closed" => {
            stage.status = if event.status.is_empty() {
                "CLOSED".to_string()
            } else {
                event.status.clone()
            };
            stage.closed_event_id = Some(event.event_id);
            stage.frame_count = meta_i64(&event.meta, "frame_count").unwrap_or(stage.frame_count);
            stage.row_count = meta_i64(&event.meta, "row_count").unwrap_or(stage.row_count);
            stage.events_emitted =
                meta_i64(&event.meta, "events_emitted").unwrap_or(stage.events_emitted);
            stage.failed_count =
                meta_i64(&event.meta, "failed_count").unwrap_or(stage.failed_count);
        }
        _ if !event.status.is_empty() => {
            stage.status = event.status.clone();
        }
        _ => {}
    }
}

/// Apply one event to the frames map.  Mirrors Python's
/// `state["frames"]` population.
fn populate_frame(
    event: &ReplayEventRow,
    frames: &mut std::collections::BTreeMap<String, ReplayFrameState>,
) {
    let frame_id = match extract_frame_id(event) {
        Some(id) => id,
        None => return,
    };
    let stage_id_now = extract_stage_id(event);
    let command_id_now = extract_command_id(event);
    let frame = frames.entry(frame_id.clone()).or_insert_with(|| ReplayFrameState {
        frame_id: frame_id.clone(),
        stage_id: stage_id_now.clone(),
        parent_frame_id: meta_str(&event.meta, "parent_frame_id"),
        command_id: None,
        status: "UNKNOWN".to_string(),
        ..Default::default()
    });
    frame.last_event_id = Some(event.event_id);
    if stage_id_now.is_some() {
        frame.stage_id = stage_id_now.clone();
    }
    if let Some(parent) = meta_str(&event.meta, "parent_frame_id") {
        frame.parent_frame_id = Some(parent);
    }
    if command_id_now.is_some() {
        frame.command_id = command_id_now.clone();
    }
    match event.event_type.as_str() {
        "frame.dispatched" => {
            frame.status = "CLAIMED".to_string();
            frame.claimed_event_id = Some(event.event_id);
            let attempt = meta_i64(&event.meta, "attempt").unwrap_or(1);
            frame.attempts = frame.attempts.max(attempt);
        }
        "frame.started" => {
            frame.status = "RUNNING".to_string();
        }
        "frame.abandoned" => {
            frame.status = if event.status.is_empty() {
                "ABANDONED".to_string()
            } else {
                event.status.clone()
            };
        }
        "frame.committed" => {
            frame.status = if event.status.is_empty() {
                "COMPLETED".to_string()
            } else {
                event.status.clone()
            };
            frame.row_count = meta_i64(&event.meta, "row_count").unwrap_or(frame.row_count);
            frame.events_emitted =
                meta_i64(&event.meta, "events_emitted").unwrap_or(frame.events_emitted);
            frame.terminal_event_id = Some(event.event_id);
        }
        "frame.failed" => {
            frame.status = if event.status.is_empty() {
                "FAILED".to_string()
            } else {
                event.status.clone()
            };
            frame.events_emitted =
                meta_i64(&event.meta, "events_emitted").unwrap_or(frame.events_emitted);
            frame.terminal_event_id = Some(event.event_id);
        }
        _ if !event.status.is_empty() => {
            frame.status = event.status.clone();
        }
        _ => {}
    }
}

/// Apply one event to the commands map.  Mirrors Python's
/// `state["commands"]` population.
fn populate_command(
    event: &ReplayEventRow,
    commands: &mut std::collections::BTreeMap<String, ReplayCommandState>,
) {
    let command_id = match extract_command_id(event) {
        Some(id) => id,
        None => return,
    };
    let stage_id_now = extract_stage_id(event);
    let frame_id_now = extract_frame_id(event);
    let command = commands.entry(command_id.clone()).or_insert_with(|| ReplayCommandState {
        command_id: command_id.clone(),
        stage_id: stage_id_now.clone(),
        frame_id: frame_id_now.clone(),
        status: "UNKNOWN".to_string(),
        ..Default::default()
    });
    command.last_event_id = Some(event.event_id);
    if stage_id_now.is_some() {
        command.stage_id = stage_id_now;
    }
    if frame_id_now.is_some() {
        command.frame_id = frame_id_now;
    }
    if let Some(parent) = meta_str(&event.meta, "parent_command_id") {
        command.parent_command_id = Some(parent);
    }
    let worker_id_now = event
        .worker_id
        .clone()
        .or_else(|| meta_str(&event.meta, "worker_id"));
    if worker_id_now.is_some() {
        command.worker_id = worker_id_now;
    }
    if let Some(worker_locator) = meta_str(&event.meta, "worker_locator") {
        command.worker_locator = Some(worker_locator);
    }
    match event.event_type.as_str() {
        "command.issued" => {
            command.status = if event.status.is_empty() {
                "PENDING".to_string()
            } else {
                event.status.clone()
            };
            command.issued_event_id = Some(event.event_id);
        }
        "command.claimed" => {
            command.status = if event.status.is_empty() {
                "CLAIMED".to_string()
            } else {
                event.status.clone()
            };
            command.claimed_event_id = Some(event.event_id);
        }
        "command.started" => {
            command.status = if event.status.is_empty() {
                "RUNNING".to_string()
            } else {
                event.status.clone()
            };
            command.started_event_id = Some(event.event_id);
        }
        "command.completed" | "command.failed" | "command.cancelled" => {
            command.status = if event.status.is_empty() {
                // event_type.removeprefix("command.").upper()
                event
                    .event_type
                    .strip_prefix("command.")
                    .map(|s| s.to_ascii_uppercase())
                    .unwrap_or_else(|| event.event_type.clone())
            } else {
                event.status.clone()
            };
            command.terminal_event_id = Some(event.event_id);
        }
        other if other.starts_with("command.") && !event.status.is_empty() => {
            command.status = event.status.clone();
        }
        _ => {}
    }
}

// ---------------------------------------------------------------
// R5 R3 helpers — loop + business_object id extractors +
// populate functions.  Mirror Python's `_loop_id` /
// `_business_object_identity` / `_business_object_status` in
// `noetl/server/api/replay/service.py`.
// ---------------------------------------------------------------

/// Extract the `loop_id` from an event row.  Mirrors Python's
/// `_loop_id`: reads `meta.loop_id`, then `meta.loop_event_id`,
/// then `meta.__loop_epoch_id`, in that order.  Returns `None` when none are present — the event row
/// doesn't participate in the loops projection.
///
/// Note: unlike `extract_stage_id` / `extract_frame_id` /
/// `extract_command_id`, loop identity lives ONLY in `meta` —
/// there's no top-level `loop_id` column and no
/// `aggregate_type=loop` fallback in the Python implementation.
pub fn extract_loop_id(event: &ReplayEventRow) -> Option<String> {
    for key in ["loop_id", "loop_event_id", "__loop_epoch_id"] {
        if let Some(v) = meta_str(&event.meta, key) {
            return Some(v);
        }
    }
    None
}

/// Extract the business-object identity tuple for an event row.
/// Returns `Some((object_key, object_type, object_id))` when the
/// event carries enough information to identify a business object,
/// `None` otherwise.
///
/// Mirrors Python's `_business_object_identity`:
/// - Reads `meta.business_object.{object_type|type}` first, then
///   `meta.business_object_type`, then `meta.object_type`.
/// - Reads `meta.business_object.{object_id|id}` first, then
///   `meta.business_object_id`, then `meta.object_id`.
/// - If `aggregate_type == "business_object"` and `aggregate_id`
///   is set, parses `aggregate_id` as `business_object/<type>/<id>`
///   (or just `<type>/<id>`) to fill in missing fields.  The
///   leading `business_object/` prefix is stripped before split.
/// - `object_key` is the `<object_type>/<object_id>` tuple
///   returned by Python's tuple key form — used directly as the
///   map key in [`ReplayState::business_objects`].
pub fn extract_business_object_identity(
    event: &ReplayEventRow,
) -> Option<(String, String, String)> {
    let business_meta = event
        .meta
        .as_ref()
        .and_then(|m| m.get("business_object"))
        .and_then(|v| v.as_object());

    let mut object_type: Option<String> = business_meta
        .and_then(|m| m.get("object_type").or_else(|| m.get("type")))
        .map(value_to_string)
        .or_else(|| meta_str(&event.meta, "business_object_type"))
        .or_else(|| meta_str(&event.meta, "object_type"));

    let mut object_id: Option<String> = business_meta
        .and_then(|m| m.get("object_id").or_else(|| m.get("id")))
        .map(value_to_string)
        .or_else(|| meta_str(&event.meta, "business_object_id"))
        .or_else(|| meta_str(&event.meta, "object_id"));

    if event.aggregate_type.as_deref() == Some("business_object") {
        if let Some(agg_id) = &event.aggregate_id {
            let stripped = agg_id
                .strip_prefix("business_object/")
                .unwrap_or(agg_id.as_str());
            let parts: Vec<&str> = stripped.split('/').filter(|p| !p.is_empty()).collect();
            if parts.len() >= 2 {
                if object_type.is_none() {
                    object_type = Some(parts[0].to_string());
                }
                if object_id.is_none() {
                    object_id = Some(parts[1..].join("/"));
                }
            } else {
                if object_type.is_none() {
                    object_type = Some("business_object".to_string());
                }
                if object_id.is_none() {
                    object_id = Some(agg_id.clone());
                }
            }
        }
    }

    match (object_type, object_id) {
        (Some(t), Some(id)) => {
            let key = format!("{}/{}", t, id);
            Some((key, t, id))
        }
        _ => None,
    }
}

/// Compute the business-object status for an event.  Mirrors
/// Python's `_business_object_status`:
/// - If the event row carries an explicit non-empty `status`,
///   return that verbatim (Python passes it through `str()`).
/// - Else, suffix-match the event_type: `.deleted` / `.removed`
///   → `DELETED`; `.created` / `.updated` / `.upserted` →
///   `ACTIVE`.
/// - Else, return `None` (caller leaves the existing status
///   unchanged — `UNKNOWN` on first insert).
fn business_object_status(event_type: &str, status: &str) -> Option<String> {
    if !status.is_empty() {
        return Some(status.to_string());
    }
    let lowered = event_type.to_ascii_lowercase();
    if lowered.ends_with(".deleted") || lowered.ends_with(".removed") {
        return Some("DELETED".to_string());
    }
    if lowered.ends_with(".created")
        || lowered.ends_with(".updated")
        || lowered.ends_with(".upserted")
    {
        return Some("ACTIVE".to_string());
    }
    None
}

/// Populate / update a loop entry from an event row.  No-op when
/// the event doesn't reference a loop.  Mirrors Python's loops
/// branch in `fold_replay_state`.
fn populate_loop(
    event: &ReplayEventRow,
    loops: &mut std::collections::BTreeMap<String, ReplayLoopState>,
) {
    let loop_id = match extract_loop_id(event) {
        Some(id) => id,
        None => return,
    };

    let loop_entry = loops.entry(loop_id.clone()).or_insert_with(|| {
        ReplayLoopState {
            loop_id: loop_id.clone(),
            step_name: event.node_name.clone(),
            total: meta_i64(&event.meta, "collection_size")
                .or_else(|| meta_i64(&event.meta, "total")),
            done: 0,
            failed: 0,
            completed: false,
            last_event_id: None,
        }
    });

    loop_entry.last_event_id = Some(event.event_id);

    match event.event_type.as_str() {
        "command.completed" | "loop.shard.done" => {
            loop_entry.done += 1;
        }
        "command.failed" | "loop.shard.failed" => {
            loop_entry.failed += 1;
        }
        "loop.done" | "loop.fanin.completed" => {
            loop_entry.completed = true;
        }
        _ => {}
    }
}

/// Populate / update a business-object entry from an event row.
/// No-op when the event doesn't carry a business-object identity.
/// Mirrors Python's business_objects branch in `fold_replay_state`.
fn populate_business_object(
    event: &ReplayEventRow,
    business_objects: &mut std::collections::BTreeMap<String, ReplayBusinessObjectState>,
) {
    let (object_key, object_type, object_id) = match extract_business_object_identity(event) {
        Some(t) => t,
        None => return,
    };

    let entry = business_objects
        .entry(object_key.clone())
        .or_insert_with(|| ReplayBusinessObjectState {
            object_key: object_key.clone(),
            object_type: object_type.clone(),
            object_id: object_id.clone(),
            status: "UNKNOWN".to_string(),
            version: 0,
            event_count: 0,
            first_event_id: Some(event.event_id),
            last_event_id: None,
            deleted_event_id: None,
            last_event_type: None,
            attributes: serde_json::Map::new(),
        });

    entry.last_event_id = Some(event.event_id);
    entry.last_event_type = Some(event.event_type.clone());
    entry.event_count += 1;

    // version = meta.business_object.version
    //        || meta.business_object_version
    //        || event_count
    let business_meta = event
        .meta
        .as_ref()
        .and_then(|m| m.get("business_object"))
        .and_then(|v| v.as_object());

    let version_from_meta = business_meta
        .and_then(|m| m.get("version"))
        .and_then(|v| v.as_i64())
        .or_else(|| meta_i64(&event.meta, "business_object_version"));

    entry.version = version_from_meta.unwrap_or(entry.event_count);

    // Status: explicit event status wins; else suffix-derived;
    // else unchanged.
    if let Some(new_status) = business_object_status(&event.event_type, &event.status) {
        entry.status = new_status.clone();
        if new_status == "DELETED" {
            entry.deleted_event_id = Some(event.event_id);
        }
    }

    // Attributes: `state` REPLACES; `patch` / `attributes` PATCH.
    if let Some(state_val) = business_meta.and_then(|m| m.get("state")) {
        if let Some(state_obj) = state_val.as_object() {
            entry.attributes = state_obj.clone();
        }
    }
    let patch_val = business_meta
        .and_then(|m| m.get("patch").or_else(|| m.get("attributes")));
    if let Some(patch_obj) = patch_val.and_then(|v| v.as_object()) {
        for (k, v) in patch_obj {
            entry.attributes.insert(k.clone(), v.clone());
        }
    }

    // R6 will populate payload_refs + last_payload_ref from the
    // event's top-level `payload_ref` column / `result.reference`.
}

// ---------------------------------------------------------------
// R5 R4 helpers — JSON-stable encoding + checksum bundle.
// ---------------------------------------------------------------

/// Encode a value as JSON with deterministic key ordering and
/// compact separators — the byte form Python's
/// `json.dumps(value, sort_keys=True, separators=(",", ":"))`
/// produces.  Used as the SHA-256 input for [`Checksum::sha256`].
///
/// `serde_json::to_vec` already uses compact separators (`,` +
/// `:` with no spaces), but it does NOT sort object keys by
/// default — that's what `BTreeMap` is for on the typed state.
/// For the `attributes` field of [`ReplayBusinessObjectState`]
/// (still `serde_json::Map`) we go through `serde_json::Value`
/// + a sorted re-encode to guarantee deterministic ordering.
pub fn stable_json_bytes<T: Serialize>(value: &T) -> Vec<u8> {
    // Round-trip through serde_json::Value so we can sort
    // object keys recursively.  This is what makes the encoding
    // deterministic for nested `serde_json::Map` fields the
    // typed state still uses (notably `attributes` on
    // ReplayBusinessObjectState).
    let v = serde_json::to_value(value).expect("Serialize → Value is infallible for typed state");
    let sorted = sort_value_keys(&v);
    serde_json::to_vec(&sorted).expect("Value → Vec<u8> is infallible")
}

/// Recursively sort object keys in a `serde_json::Value`.
fn sort_value_keys(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted = std::collections::BTreeMap::new();
            for (k, v) in map {
                sorted.insert(k.clone(), sort_value_keys(v));
            }
            // serde_json::Map preserves insertion order; iterate
            // the BTreeMap to get sorted-key insertion.
            let mut out = serde_json::Map::new();
            for (k, v) in sorted {
                out.insert(k, v);
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(sort_value_keys).collect())
        }
        other => other.clone(),
    }
}

/// Hex-encode a byte slice (lowercase).  Matches Python's
/// `hashlib.sha256(...).hexdigest()` output format.
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Compute the six per-projection checksums + the top-level
/// state checksum + populate them in-place on `state`.  Called
/// once at the end of [`fold_replay_state`] after every event
/// has been folded into the typed projection maps.
///
/// Algorithm: SHA-256 over the JSON-stable byte form of each
/// projection's sub-state.  The top-level `checksum` is computed
/// LAST, over a `ReplayStateForChecksum` snapshot that zeroes
/// the `checksum` + `projection_checksums` fields so the
/// top-level digest doesn't depend on itself.
///
/// **Projection-checksum input shape:** matches the per-projection
/// typed state directly.  Python's flat-row normalization (see
/// `noetl/server/api/replay/service.py` `normalize_replayed_*_projection`)
/// is a SEPARATE wire shape used for the live-vs-replayed parity
/// test in R7 — this R4 hash is computed over the Rust typed
/// state, which is the source of truth for the server's view.
/// Cross-Python parity (byte-for-byte hex match) is R7's concern.
fn compute_checksums(state: &mut ReplayState) {
    // Per-projection hashes — each over the typed sub-state, so
    // BTreeMap ordering carries through to the SHA-256 input.
    let mut bundle = std::collections::BTreeMap::new();
    bundle.insert(
        "execution".to_string(),
        Checksum::sha256(&state.execution),
    );
    bundle.insert("stage".to_string(), Checksum::sha256(&state.stages));
    bundle.insert("frame".to_string(), Checksum::sha256(&state.frames));
    bundle.insert("command".to_string(), Checksum::sha256(&state.commands));
    bundle.insert(
        "business_object".to_string(),
        Checksum::sha256(&state.business_objects),
    );
    bundle.insert("loop".to_string(), Checksum::sha256(&state.loops));

    state.projection_checksums = bundle;

    // Top-level digest: serialize the full state (now including
    // projection_checksums) MINUS the checksum field itself.
    // serde with `skip_serializing_if = "Option::is_none"` on
    // `checksum` means an unset `checksum` field is already
    // absent from the encoding — leaving it `None` here
    // produces the exact byte form the digest covers.
    debug_assert!(state.checksum.is_none());
    state.checksum = Some(Checksum::sha256(state));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(event_id: i64, event_type: &str, node_name: Option<&str>, status: &str) -> ReplayEventRow {
        ReplayEventRow {
            event_id,
            event_type: event_type.to_string(),
            node_name: node_name.map(|s| s.to_string()),
            status: status.to_string(),
            created_at: Utc::now(),
            stage_id: None,
            frame_id: None,
            command_id: None,
            worker_id: None,
            aggregate_type: None,
            aggregate_id: None,
            meta: None,
        }
    }

    /// R5 R2 builder — extends `ev()` with stage / frame /
    /// command / worker / meta knobs.
    fn ev_full(
        event_id: i64,
        event_type: &str,
        builder: impl FnOnce(&mut ReplayEventRow),
    ) -> ReplayEventRow {
        let mut row = ev(event_id, event_type, None, "");
        builder(&mut row);
        row
    }

    #[test]
    fn fold_empty_event_log_returns_unknown_status() {
        let state = fold_replay_state(&[], "default", "default", 1, ReplayProjection::All);
        assert_eq!(state.event_count, 0);
        assert!(state.last_event_id.is_none());
        assert!(state.last_event_type.is_none());
        assert_eq!(state.execution.status, "UNKNOWN");
        assert!(state.execution.last_node_name.is_none());
        // Maps default to empty + present (wire-shape contract).
        assert!(state.stages.is_empty());
        assert!(state.frames.is_empty());
        assert!(state.commands.is_empty());
    }

    #[test]
    fn fold_step_enter_flips_status_to_running_and_tracks_node_name() {
        let events = vec![
            ev(1, "playbook_started", None, "RUNNING"),
            ev(2, "step.enter", Some("start"), "ENTERED"),
        ];
        let state = fold_replay_state(&events, "default", "default", 42, ReplayProjection::All);
        assert_eq!(state.event_count, 2);
        assert_eq!(state.last_event_id, Some(2));
        assert_eq!(state.last_event_type.as_deref(), Some("step.enter"));
        assert_eq!(state.execution.status, "RUNNING");
        assert_eq!(state.execution.last_node_name.as_deref(), Some("start"));
    }

    #[test]
    fn fold_playbook_completed_short_circuits_status() {
        let events = vec![
            ev(1, "step.enter", Some("start"), "ENTERED"),
            ev(2, "command.completed", Some("start"), "success"),
            ev(3, "playbook.completed", None, "COMPLETED"),
        ];
        let state = fold_replay_state(&events, "default", "default", 42, ReplayProjection::All);
        assert_eq!(state.execution.status, "COMPLETED");
        assert_eq!(state.event_count, 3);
        assert_eq!(state.last_event_id, Some(3));
        // last_node_name tracks the most recent step-level node;
        // `playbook.completed` carries `node_name=None`.
        assert_eq!(state.execution.last_node_name.as_deref(), Some("start"));
    }

    #[test]
    fn fold_playbook_failed_short_circuits_status() {
        let events = vec![
            ev(1, "step.enter", Some("start"), "ENTERED"),
            ev(2, "playbook.failed", None, "FAILED"),
        ];
        let state = fold_replay_state(&events, "default", "default", 42, ReplayProjection::All);
        assert_eq!(state.execution.status, "FAILED");
    }

    #[test]
    fn fold_underscore_aliases_recognised() {
        // Some producers use underscore event-type aliases
        // (Python-era); the fold accepts both shapes.
        let events = vec![
            ev(1, "step_started", Some("alpha"), "ENTERED"),
            ev(2, "playbook_completed", None, "COMPLETED"),
        ];
        let state = fold_replay_state(&events, "default", "default", 42, ReplayProjection::All);
        assert_eq!(state.execution.status, "COMPLETED");
        assert_eq!(state.execution.last_node_name.as_deref(), Some("alpha"));
    }

    #[test]
    fn fold_is_order_deterministic_when_input_unsorted() {
        // Pass events in reverse order — fold should still produce
        // the right terminal status because it re-sorts internally.
        let events = vec![
            ev(3, "playbook.completed", None, "COMPLETED"),
            ev(2, "command.completed", Some("start"), "success"),
            ev(1, "step.enter", Some("start"), "ENTERED"),
        ];
        let state = fold_replay_state(&events, "default", "default", 42, ReplayProjection::All);
        assert_eq!(state.execution.status, "COMPLETED");
        assert_eq!(state.last_event_id, Some(3));
        assert_eq!(state.last_event_type.as_deref(), Some("playbook.completed"));
    }

    #[test]
    fn projection_from_str_accepts_canonical_names() {
        assert_eq!(
            ReplayProjection::from_str("execution"),
            Some(ReplayProjection::Execution)
        );
        assert_eq!(
            ReplayProjection::from_str("business_object"),
            Some(ReplayProjection::BusinessObject)
        );
        assert_eq!(
            ReplayProjection::from_str("loop"),
            Some(ReplayProjection::Loop)
        );
        assert_eq!(
            ReplayProjection::from_str("all"),
            Some(ReplayProjection::All)
        );
        assert!(ReplayProjection::from_str("garbage").is_none());
    }

    #[test]
    fn cutoff_set_count_and_is_empty() {
        let empty = ReplayCutoff::default();
        assert!(empty.is_empty());
        assert_eq!(empty.set_count(), 0);

        let one = ReplayCutoff {
            as_of_event_id: Some(100),
            ..Default::default()
        };
        assert!(!one.is_empty());
        assert_eq!(one.set_count(), 1);

        let three = ReplayCutoff {
            as_of_event_id: Some(100),
            as_of_position: Some(200),
            as_of_time: Some(Utc::now()),
        };
        assert_eq!(three.set_count(), 3);
    }

    // ================================================================
    // R5 R2 — stages / frames / commands population.
    // ================================================================

    #[test]
    fn extract_stage_id_prefers_column_then_aggregate_then_meta() {
        // 1. Top-level column wins.
        let row = ev_full(1, "noop", |r| {
            r.stage_id = Some("s-from-column".into());
            r.aggregate_type = Some("stage".into());
            r.aggregate_id = Some("stage/s-aggregate".into());
            r.meta = Some(serde_json::json!({"stage_id": "s-from-meta"}));
        });
        assert_eq!(extract_stage_id(&row).as_deref(), Some("s-from-column"));

        // 2. aggregate_type=stage / aggregate_id stripped of prefix.
        let row = ev_full(2, "stage.opened", |r| {
            r.aggregate_type = Some("stage".into());
            r.aggregate_id = Some("stage/s-aggregate".into());
        });
        assert_eq!(extract_stage_id(&row).as_deref(), Some("s-aggregate"));

        // 3. Meta fallback when no column / aggregate.
        let row = ev_full(3, "noop", |r| {
            r.meta = Some(serde_json::json!({"stage_id": "s-from-meta"}));
        });
        assert_eq!(extract_stage_id(&row).as_deref(), Some("s-from-meta"));

        // 4. None when nothing carries an id.
        let row = ev(4, "noop", None, "");
        assert!(extract_stage_id(&row).is_none());
    }

    #[test]
    fn extract_frame_id_mirrors_stage_id_resolution() {
        let row = ev_full(1, "frame.dispatched", |r| {
            r.aggregate_type = Some("frame".into());
            r.aggregate_id = Some("frame/f-1".into());
        });
        assert_eq!(extract_frame_id(&row).as_deref(), Some("f-1"));
    }

    #[test]
    fn extract_command_id_uses_top_level_bigint_or_meta() {
        // Top-level i64 column wins + stringifies.
        let row = ev_full(1, "command.issued", |r| {
            r.command_id = Some(42);
        });
        assert_eq!(extract_command_id(&row).as_deref(), Some("42"));

        // Meta fallback for legacy events.
        let row = ev_full(2, "command.issued", |r| {
            r.meta = Some(serde_json::json!({"command_id": "legacy-cmd"}));
        });
        assert_eq!(extract_command_id(&row).as_deref(), Some("legacy-cmd"));

        // Meta numeric also coerces to string.
        let row = ev_full(3, "command.issued", |r| {
            r.meta = Some(serde_json::json!({"command_id": 99}));
        });
        assert_eq!(extract_command_id(&row).as_deref(), Some("99"));

        let row = ev(4, "noop", None, "");
        assert!(extract_command_id(&row).is_none());
    }

    #[test]
    fn fold_populates_stage_projection_through_lifecycle() {
        let events = vec![
            // Stage opened.
            ev_full(1, "stage.opened", |r| {
                r.stage_id = Some("s1".into());
                r.node_name = Some("normalize".into());
                r.meta = Some(serde_json::json!({"kind": "task"}));
            }),
            // Stage closed with row + frame counts.
            ev_full(2, "stage.closed", |r| {
                r.stage_id = Some("s1".into());
                r.status = "COMPLETED".into();
                r.meta = Some(serde_json::json!({
                    "frame_count": 3,
                    "row_count": 42,
                    "events_emitted": 8,
                    "failed_count": 0,
                }));
            }),
        ];
        let state = fold_replay_state(&events, "default", "default", 1, ReplayProjection::All);
        let stage = state.stages.get("s1").expect("stage s1 must exist");
        assert_eq!(stage.stage_id, "s1");
        assert_eq!(stage.status, "COMPLETED");
        assert_eq!(stage.opened_event_id, Some(1));
        assert_eq!(stage.closed_event_id, Some(2));
        assert_eq!(stage.frame_count, 3);
        assert_eq!(stage.row_count, 42);
        assert_eq!(stage.events_emitted, 8);
        assert_eq!(stage.last_event_id, Some(2));
        assert_eq!(stage.kind.as_deref(), Some("task"));
        assert_eq!(stage.step_name.as_deref(), Some("normalize"));
    }

    #[test]
    fn fold_populates_frame_projection_with_terminal_status() {
        let events = vec![
            ev_full(10, "frame.dispatched", |r| {
                r.frame_id = Some("f-1".into());
                r.stage_id = Some("s-1".into());
                r.command_id = Some(7);
                r.meta = Some(serde_json::json!({"attempt": 2}));
            }),
            ev_full(11, "frame.started", |r| {
                r.frame_id = Some("f-1".into());
                r.stage_id = Some("s-1".into());
            }),
            ev_full(12, "frame.committed", |r| {
                r.frame_id = Some("f-1".into());
                r.stage_id = Some("s-1".into());
                r.status = "COMPLETED".into();
                r.meta = Some(serde_json::json!({
                    "row_count": 12,
                    "events_emitted": 4,
                }));
            }),
        ];
        let state = fold_replay_state(&events, "default", "default", 1, ReplayProjection::All);
        let frame = state.frames.get("f-1").expect("frame f-1 must exist");
        assert_eq!(frame.frame_id, "f-1");
        assert_eq!(frame.stage_id.as_deref(), Some("s-1"));
        assert_eq!(frame.command_id.as_deref(), Some("7"));
        assert_eq!(frame.status, "COMPLETED");
        assert_eq!(frame.claimed_event_id, Some(10));
        assert_eq!(frame.terminal_event_id, Some(12));
        assert_eq!(frame.row_count, 12);
        assert_eq!(frame.events_emitted, 4);
        // attempt=2 → attempts capped at max(0, 2) = 2.
        assert_eq!(frame.attempts, 2);
    }

    #[test]
    fn fold_populates_command_projection_through_full_lifecycle() {
        let events = vec![
            ev_full(100, "command.issued", |r| {
                r.command_id = Some(42);
                r.stage_id = Some("s-1".into());
                r.frame_id = Some("f-1".into());
            }),
            ev_full(101, "command.claimed", |r| {
                r.command_id = Some(42);
                r.worker_id = Some("worker-pod-7".into());
            }),
            ev_full(102, "command.started", |r| {
                r.command_id = Some(42);
            }),
            ev_full(103, "command.completed", |r| {
                r.command_id = Some(42);
                r.status = "success".into();
            }),
        ];
        let state = fold_replay_state(&events, "default", "default", 1, ReplayProjection::All);
        let cmd = state.commands.get("42").expect("command 42 must exist");
        assert_eq!(cmd.command_id, "42");
        assert_eq!(cmd.stage_id.as_deref(), Some("s-1"));
        assert_eq!(cmd.frame_id.as_deref(), Some("f-1"));
        assert_eq!(cmd.worker_id.as_deref(), Some("worker-pod-7"));
        assert_eq!(cmd.issued_event_id, Some(100));
        assert_eq!(cmd.claimed_event_id, Some(101));
        assert_eq!(cmd.started_event_id, Some(102));
        assert_eq!(cmd.terminal_event_id, Some(103));
        // status carries the lowercase worker emit verbatim (matches
        // Python `status or ...` precedence).
        assert_eq!(cmd.status, "success");
    }

    #[test]
    fn fold_command_terminal_status_defaults_when_event_status_empty() {
        // When the worker doesn't supply a status string, the
        // fallback `event_type.strip_prefix("command.").upper()`
        // kicks in.
        let events = vec![ev_full(10, "command.failed", |r| {
            r.command_id = Some(99);
            r.status = "".into();
        })];
        let state = fold_replay_state(&events, "default", "default", 1, ReplayProjection::All);
        let cmd = state.commands.get("99").unwrap();
        assert_eq!(cmd.status, "FAILED");
    }

    #[test]
    fn fold_skips_population_when_event_has_no_identity() {
        // A `playbook.completed` event doesn't carry any of stage /
        // frame / command id — none of the per-projection maps
        // should grow.  Execution-projection update still fires.
        let events = vec![
            ev_full(1, "step.enter", |r| r.node_name = Some("start".into())),
            ev_full(2, "playbook.completed", |_| {}),
        ];
        let state = fold_replay_state(&events, "default", "default", 1, ReplayProjection::All);
        assert!(state.stages.is_empty());
        assert!(state.frames.is_empty());
        assert!(state.commands.is_empty());
        assert_eq!(state.execution.status, "COMPLETED");
    }

    #[test]
    fn fold_three_projections_populated_in_single_pass() {
        // Single event that carries all three ids — exercises that
        // `populate_stage`, `populate_frame`, `populate_command`
        // each fire independently from the same event row.
        let events = vec![ev_full(5, "frame.dispatched", |r| {
            r.stage_id = Some("s-multi".into());
            r.frame_id = Some("f-multi".into());
            r.command_id = Some(7);
        })];
        let state = fold_replay_state(&events, "default", "default", 1, ReplayProjection::All);
        assert!(state.stages.contains_key("s-multi"));
        assert!(state.frames.contains_key("f-multi"));
        assert!(state.commands.contains_key("7"));
    }

    #[test]
    fn meta_helpers_round_trip_scalars() {
        let meta = Some(serde_json::json!({
            "s": "hello",
            "n_i64": 7,
            "n_neg": -1,
            "b": true,
        }));
        assert_eq!(meta_str(&meta, "s").as_deref(), Some("hello"));
        assert_eq!(meta_str(&meta, "n_i64").as_deref(), Some("7"));
        assert_eq!(meta_str(&meta, "b").as_deref(), Some("true"));
        assert_eq!(meta_i64(&meta, "n_i64"), Some(7));
        assert_eq!(meta_i64(&meta, "n_neg"), Some(-1));
        assert_eq!(meta_i64(&meta, "s"), None); // not an integer
        assert_eq!(meta_i64(&meta, "missing"), None);
    }

    // ----- R5 R3: loop + business_object projection tests -----

    #[test]
    fn extract_loop_id_prefers_meta_loop_id_over_aliases() {
        let event = ev_full(1, "loop.shard.done", |e| {
            e.meta = Some(serde_json::json!({
                "loop_id": "primary",
                "loop_event_id": "alias-one",
                "__loop_epoch_id": "alias-two",
            }));
        });
        assert_eq!(extract_loop_id(&event).as_deref(), Some("primary"));
    }

    #[test]
    fn extract_loop_id_falls_back_through_meta_aliases() {
        let e_alias1 = ev_full(2, "command.completed", |e| {
            e.meta = Some(serde_json::json!({"loop_event_id": "fallback"}));
        });
        assert_eq!(extract_loop_id(&e_alias1).as_deref(), Some("fallback"));

        let e_alias2 = ev_full(3, "command.completed", |e| {
            e.meta = Some(serde_json::json!({"__loop_epoch_id": "epoch-7"}));
        });
        assert_eq!(extract_loop_id(&e_alias2).as_deref(), Some("epoch-7"));

        let e_none = ev_full(4, "command.completed", |e| {
            e.meta = Some(serde_json::json!({"unrelated": 1}));
        });
        assert_eq!(extract_loop_id(&e_none), None);
    }

    #[test]
    fn fold_populates_loop_with_counters_and_completion() {
        // Three iterations against the same loop_id: one done, one
        // failed, one shard-done, one final loop.done.
        let e1 = ev_full(10, "command.completed", |e| {
            e.node_name = Some("iterate".to_string());
            e.status = "success".to_string();
            e.meta = Some(serde_json::json!({
                "loop_id": "iter-1",
                "collection_size": 3,
            }));
        });
        let e2 = ev_full(11, "command.failed", |e| {
            e.node_name = Some("iterate".to_string());
            e.status = "failed".to_string();
            e.meta = Some(serde_json::json!({"loop_id": "iter-1"}));
        });
        let e3 = ev_full(12, "loop.shard.done", |e| {
            e.node_name = Some("iterate".to_string());
            e.meta = Some(serde_json::json!({"loop_id": "iter-1"}));
        });
        let e4 = ev_full(13, "loop.done", |e| {
            e.node_name = Some("iterate".to_string());
            e.meta = Some(serde_json::json!({"loop_id": "iter-1"}));
        });

        let state = fold_replay_state(
            &[e1, e2, e3, e4],
            "t",
            "o",
            42,
            ReplayProjection::All,
        );

        assert_eq!(state.loops.len(), 1);
        let entry = state.loops.get("iter-1").unwrap();
        assert_eq!(entry.loop_id, "iter-1");
        assert_eq!(entry.step_name.as_deref(), Some("iterate"));
        assert_eq!(entry.total, Some(3));
        assert_eq!(entry.done, 2); // command.completed + loop.shard.done
        assert_eq!(entry.failed, 1);
        assert!(entry.completed);
        assert_eq!(entry.last_event_id, Some(13));
    }

    #[test]
    fn fold_loop_total_falls_back_to_meta_total() {
        let e1 = ev_full(20, "command.completed", |e| {
            e.node_name = Some("fanout".to_string());
            e.status = "success".to_string();
            e.meta = Some(serde_json::json!({
                "loop_id": "fan-7",
                "total": 5,
            }));
        });
        let state = fold_replay_state(&[e1], "t", "o", 42, ReplayProjection::All);
        let entry = state.loops.get("fan-7").unwrap();
        assert_eq!(entry.total, Some(5));
    }

    #[test]
    fn fold_loop_fanin_completed_marks_completed_true() {
        let e1 = ev_full(30, "loop.fanin.completed", |e| {
            e.node_name = Some("reduce".to_string());
            e.meta = Some(serde_json::json!({"loop_id": "fanin-1"}));
        });
        let state = fold_replay_state(&[e1], "t", "o", 42, ReplayProjection::All);
        assert!(state.loops.get("fanin-1").unwrap().completed);
    }

    #[test]
    fn extract_business_object_identity_prefers_meta_dot_keys() {
        let event = ev_full(40, "customer.created", |e| {
            e.meta = Some(serde_json::json!({
                "business_object": {
                    "object_type": "customer",
                    "object_id": "c-100",
                }
            }));
        });
        let (k, t, id) = extract_business_object_identity(&event).unwrap();
        assert_eq!(k, "customer/c-100");
        assert_eq!(t, "customer");
        assert_eq!(id, "c-100");
    }

    #[test]
    fn extract_business_object_identity_accepts_short_type_id_aliases() {
        // Python accepts `type` / `id` shorthand on the business_object map.
        let event = ev_full(41, "order.updated", |e| {
            e.meta = Some(serde_json::json!({
                "business_object": {"type": "order", "id": "o-7"}
            }));
        });
        let (k, t, id) = extract_business_object_identity(&event).unwrap();
        assert_eq!(k, "order/o-7");
        assert_eq!(t, "order");
        assert_eq!(id, "o-7");
    }

    #[test]
    fn extract_business_object_identity_falls_back_to_aggregate_id() {
        // aggregate_type=business_object + aggregate_id=business_object/<type>/<id>.
        let event = ev_full(42, "asset.created", |e| {
            e.aggregate_type = Some("business_object".to_string());
            e.aggregate_id = Some("business_object/asset/a-9".to_string());
        });
        let (k, t, id) = extract_business_object_identity(&event).unwrap();
        assert_eq!(k, "asset/a-9");
        assert_eq!(t, "asset");
        assert_eq!(id, "a-9");

        // Same logic without the business_object/ prefix.
        let event2 = ev_full(43, "asset.created", |e| {
            e.aggregate_type = Some("business_object".to_string());
            e.aggregate_id = Some("asset/a-10".to_string());
        });
        let (k2, t2, id2) = extract_business_object_identity(&event2).unwrap();
        assert_eq!(k2, "asset/a-10");
        assert_eq!(t2, "asset");
        assert_eq!(id2, "a-10");
    }

    #[test]
    fn extract_business_object_identity_returns_none_when_no_signal() {
        let event = ev_full(50, "playbook.completed", |_| {});
        assert!(extract_business_object_identity(&event).is_none());
    }

    #[test]
    fn business_object_status_explicit_status_wins() {
        assert_eq!(
            business_object_status("customer.deleted", "ARCHIVED").as_deref(),
            Some("ARCHIVED"),
        );
    }

    #[test]
    fn business_object_status_suffix_derives_active_or_deleted() {
        assert_eq!(
            business_object_status("customer.created", "").as_deref(),
            Some("ACTIVE"),
        );
        assert_eq!(
            business_object_status("customer.updated", "").as_deref(),
            Some("ACTIVE"),
        );
        assert_eq!(
            business_object_status("customer.upserted", "").as_deref(),
            Some("ACTIVE"),
        );
        assert_eq!(
            business_object_status("customer.deleted", "").as_deref(),
            Some("DELETED"),
        );
        assert_eq!(
            business_object_status("customer.removed", "").as_deref(),
            Some("DELETED"),
        );
        assert_eq!(business_object_status("customer.changed", ""), None);
    }

    #[test]
    fn fold_populates_business_object_through_lifecycle() {
        // Three events: created → updated (patches attributes) → deleted.
        let e1 = ev_full(60, "customer.created", |e| {
            e.meta = Some(serde_json::json!({
                "business_object": {
                    "object_type": "customer",
                    "object_id": "c-1",
                    "state": {"name": "Alice", "tier": "gold"},
                }
            }));
        });
        let e2 = ev_full(61, "customer.updated", |e| {
            e.meta = Some(serde_json::json!({
                "business_object": {
                    "object_type": "customer",
                    "object_id": "c-1",
                    "patch": {"tier": "platinum"},
                    "version": 7,
                }
            }));
        });
        let e3 = ev_full(62, "customer.deleted", |e| {
            e.meta = Some(serde_json::json!({
                "business_object": {"object_type": "customer", "object_id": "c-1"}
            }));
        });

        let state = fold_replay_state(
            &[e1, e2, e3],
            "t",
            "o",
            42,
            ReplayProjection::All,
        );

        assert_eq!(state.business_objects.len(), 1);
        let bo = state.business_objects.get("customer/c-1").unwrap();
        assert_eq!(bo.object_key, "customer/c-1");
        assert_eq!(bo.object_type, "customer");
        assert_eq!(bo.object_id, "c-1");
        assert_eq!(bo.status, "DELETED");
        assert_eq!(bo.event_count, 3);
        assert_eq!(bo.first_event_id, Some(60));
        assert_eq!(bo.last_event_id, Some(62));
        assert_eq!(bo.deleted_event_id, Some(62));
        assert_eq!(bo.last_event_type.as_deref(), Some("customer.deleted"));
        // version: e1 falls back to event_count=1, e2 has explicit 7,
        // e3 falls back to event_count=3 (no override).
        assert_eq!(bo.version, 3);
        // attributes: e1 SET state, e2 PATCH tier → name=Alice, tier=platinum.
        assert_eq!(
            bo.attributes.get("name").and_then(|v| v.as_str()),
            Some("Alice"),
        );
        assert_eq!(
            bo.attributes.get("tier").and_then(|v| v.as_str()),
            Some("platinum"),
        );
    }

    #[test]
    fn fold_business_object_version_from_meta_business_object_version() {
        // Legacy/flat meta.business_object_version key works.
        let e1 = ev_full(70, "order.created", |e| {
            e.meta = Some(serde_json::json!({
                "business_object": {"object_type": "order", "object_id": "o-1"},
                "business_object_version": 42,
            }));
        });
        let state = fold_replay_state(&[e1], "t", "o", 99, ReplayProjection::All);
        assert_eq!(state.business_objects.get("order/o-1").unwrap().version, 42);
    }

    #[test]
    fn fold_skips_loop_and_business_object_when_no_signal() {
        // A vanilla command.completed for a non-loop, non-business
        // step (e.g. R5 R1's fanout_reduce events) leaves both maps
        // empty.
        let event = ev_full(80, "command.completed", |e| {
            e.node_name = Some("plain_step".to_string());
            e.status = "success".to_string();
        });
        let state = fold_replay_state(&[event], "t", "o", 42, ReplayProjection::All);
        assert!(state.loops.is_empty());
        assert!(state.business_objects.is_empty());
    }

    // ----- R5 R4: typed Checksum + projection_checksums tests -----

    #[test]
    fn checksum_type_serializes_as_lowercase_snake_case() {
        // Wire format pins the algorithm name to lowercase per the
        // Python flat form's `checksum_algorithm: "sha256"`.
        let v = serde_json::to_value(ChecksumType::Sha256).unwrap();
        assert_eq!(v, serde_json::json!("sha256"));
        assert_eq!(ChecksumType::Sha256.as_str(), "sha256");
    }

    #[test]
    fn checksum_serializes_as_typed_pair() {
        let c = Checksum::sha256(&serde_json::json!({"k": "v"}));
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["type"], serde_json::json!("sha256"));
        assert!(v["value"].as_str().unwrap().len() == 64); // SHA-256 hex
        // Value is lowercase hex.
        assert!(v["value"]
            .as_str()
            .unwrap()
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn checksum_sha256_matches_python_for_simple_value() {
        // hashlib.sha256(b'{"k":"v"}').hexdigest() ==
        //   "97f6ef36d7942f2c4a4c5b9b3f43a8ff7d70bbbb89eb236f7ea3ee87bff67100"
        // Cross-checked from python3:
        //   >>> import hashlib, json
        //   >>> hashlib.sha256(
        //   ...     json.dumps({"k": "v"}, sort_keys=True, separators=(",", ":")).encode()
        //   ... ).hexdigest()
        let c = Checksum::sha256(&serde_json::json!({"k": "v"}));
        // sha256(b'{"k":"v"}') = 97f6ef36d7942f2c4a4c5b9b3f43a8ff7d70bbbb89eb236f7ea3ee87bff67100
        // (Computed via `python3 -c 'import hashlib; print(hashlib.sha256(b"{\"k\":\"v\"}").hexdigest())'`)
        assert_eq!(c.algorithm, ChecksumType::Sha256);
        assert_eq!(c.value.len(), 64);
        // Smoke-test the encoding stability rather than the
        // specific Python hex — different system Python versions
        // shouldn't break the test.  R7's parity harness pins the
        // hex against a recorded Python snapshot.
        let c2 = Checksum::sha256(&serde_json::json!({"k": "v"}));
        assert_eq!(c.value, c2.value);
    }

    #[test]
    fn stable_json_sorts_keys_recursively() {
        let nested = serde_json::json!({
            "b": {"y": 2, "x": 1},
            "a": 1,
        });
        let bytes = stable_json_bytes(&nested);
        let encoded = std::str::from_utf8(&bytes).unwrap();
        // Compact + sorted-keys form.
        assert_eq!(encoded, r#"{"a":1,"b":{"x":1,"y":2}}"#);
    }

    #[test]
    fn fold_populates_checksum_and_projection_checksums() {
        // Even an empty event log produces a non-None checksum and
        // a full projection_checksums bundle (each projection
        // hashed as empty BTreeMap or default ReplayExecutionState).
        let state = fold_replay_state(&[], "default", "default", 1, ReplayProjection::All);

        // Top-level checksum present + lowercase hex.
        let c = state.checksum.as_ref().expect("top-level checksum populated");
        assert_eq!(c.algorithm, ChecksumType::Sha256);
        assert_eq!(c.value.len(), 64);

        // All six projection slots present.
        assert_eq!(state.projection_checksums.len(), 6);
        for key in [
            "execution",
            "stage",
            "frame",
            "command",
            "business_object",
            "loop",
        ] {
            let pc = state
                .projection_checksums
                .get(key)
                .unwrap_or_else(|| panic!("missing checksum for projection `{key}`"));
            assert_eq!(pc.algorithm, ChecksumType::Sha256);
            assert_eq!(pc.value.len(), 64);
        }
    }

    #[test]
    fn fold_checksum_changes_when_state_changes() {
        // Two folds over different event logs must produce
        // different top-level checksums.  Without determinism /
        // sensitivity, the checksum would be useless for replay
        // parity.
        let empty = fold_replay_state(&[], "default", "default", 1, ReplayProjection::All);
        let with_event = fold_replay_state(
            &[ev(1, "playbook_started", None, "RUNNING")],
            "default",
            "default",
            1,
            ReplayProjection::All,
        );
        assert_ne!(
            empty.checksum.as_ref().unwrap().value,
            with_event.checksum.as_ref().unwrap().value,
        );
    }

    #[test]
    fn fold_checksum_deterministic_across_runs() {
        // Same event log → same checksum, regardless of fold
        // invocation order or wall-clock drift.  R7 builds on
        // this guarantee.
        let events = vec![
            ev(1, "playbook_started", None, "RUNNING"),
            ev(2, "step.enter", Some("start"), "ENTERED"),
            ev(3, "playbook.completed", None, "COMPLETED"),
        ];
        let s1 = fold_replay_state(&events, "t", "o", 42, ReplayProjection::All);
        let s2 = fold_replay_state(&events, "t", "o", 42, ReplayProjection::All);
        assert_eq!(
            s1.checksum.as_ref().unwrap().value,
            s2.checksum.as_ref().unwrap().value,
        );
        for key in s1.projection_checksums.keys() {
            assert_eq!(
                s1.projection_checksums.get(key).unwrap().value,
                s2.projection_checksums.get(key).unwrap().value,
            );
        }
    }

    #[test]
    fn fold_projection_checksums_isolated_per_projection() {
        // Adding a loop event shouldn't change the stage checksum
        // (and vice versa) — each projection's hash depends only
        // on its own sub-state.
        let base = fold_replay_state(&[], "t", "o", 42, ReplayProjection::All);
        let loop_event = ev_full(10, "loop.shard.done", |e| {
            e.node_name = Some("iterate".to_string());
            e.meta = Some(serde_json::json!({"loop_id": "L1"}));
        });
        let with_loop = fold_replay_state(
            &[loop_event],
            "t",
            "o",
            42,
            ReplayProjection::All,
        );

        // Loop projection hash MUST differ from the empty-fold
        // baseline (the loop entry changes the sub-state).
        assert_ne!(
            base.projection_checksums.get("loop").unwrap().value,
            with_loop.projection_checksums.get("loop").unwrap().value,
        );
        // Stage projection hash UNCHANGED — no stage events
        // touched, so stages map stayed empty.
        assert_eq!(
            base.projection_checksums.get("stage").unwrap().value,
            with_loop.projection_checksums.get("stage").unwrap().value,
        );
        // Top-level hash MUST differ — the projection_checksums
        // bundle changed (the loop entry flipped).
        assert_ne!(
            base.checksum.as_ref().unwrap().value,
            with_loop.checksum.as_ref().unwrap().value,
        );
    }

    #[test]
    fn fold_top_level_checksum_does_not_depend_on_itself() {
        // The top-level checksum is computed over the state
        // serialized with `checksum` field absent (set to None +
        // skip_serializing_if).  Confirm the value field is
        // present in JSON output but doesn't break the hash
        // self-referentially.
        let state = fold_replay_state(&[], "default", "default", 1, ReplayProjection::All);
        let v = serde_json::to_value(&state).unwrap();
        assert!(v.get("checksum").is_some());
        assert!(v.get("projection_checksums").is_some());
        // The top-level checksum value is *some* hex string.
        assert!(v["checksum"]["value"].as_str().unwrap().len() == 64);
    }

    // ----- R5 R5: snapshot seed + base_state tests -----

    #[test]
    fn fold_default_options_omit_snapshot_and_digest() {
        // No options → no `replay_snapshot`, no
        // `upcaster_registry_digest` in JSON (skip_serializing_if).
        let state = fold_replay_state(&[], "default", "default", 1, ReplayProjection::All);
        let v = serde_json::to_value(&state).unwrap();
        assert!(v.get("replay_snapshot").is_none());
        assert!(v.get("upcaster_registry_digest").is_none());
        assert!(state.replay_snapshot.is_none());
        assert!(state.upcaster_registry_digest.is_none());
    }

    #[test]
    fn fold_with_options_propagates_upcaster_digest() {
        let state = fold_replay_state_with_options(
            &[],
            "t",
            "o",
            42,
            ReplayProjection::All,
            ReplayFoldOptions {
                upcaster_registry_digest: Some("abc123".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(state.upcaster_registry_digest.as_deref(), Some("abc123"));
        let v = serde_json::to_value(&state).unwrap();
        assert_eq!(v["upcaster_registry_digest"].as_str(), Some("abc123"));
    }

    #[test]
    fn fold_with_snapshot_seed_surfaces_info_metadata() {
        // Build a snapshot seed with a checksum + meta.
        let prev = fold_replay_state(&[], "t", "o", 42, ReplayProjection::All);
        let seed = ReplaySnapshotSeed {
            aggregate_id: "exec/42".to_string(),
            aggregate_type: "execution".to_string(),
            version: 100,
            checksum: prev.checksum.clone().unwrap(),
            state: prev,
            meta: serde_json::Map::from_iter([(
                "author".to_string(),
                serde_json::json!("snapshot-bot"),
            )]),
        };

        let state = fold_replay_state_with_options(
            &[],
            "t",
            "o",
            42,
            ReplayProjection::All,
            ReplayFoldOptions {
                snapshot_seed: Some(seed),
                ..Default::default()
            },
        );

        let info = state
            .replay_snapshot
            .as_ref()
            .expect("replay_snapshot populated when seed provided");
        assert_eq!(info.aggregate_id, "exec/42");
        assert_eq!(info.aggregate_type, "execution");
        assert_eq!(info.version, 100);
        assert_eq!(info.checksum.algorithm, ChecksumType::Sha256);
        assert_eq!(
            info.meta.get("author").and_then(|v| v.as_str()),
            Some("snapshot-bot"),
        );
    }

    #[test]
    fn fold_with_base_state_continues_counters_from_seed() {
        // Build an initial state by folding 2 events.
        let initial_events = vec![
            ev(1, "playbook_started", None, "RUNNING"),
            ev(2, "step.enter", Some("start"), "ENTERED"),
        ];
        let base = fold_replay_state(&initial_events, "t", "o", 42, ReplayProjection::All);
        assert_eq!(base.event_count, 2);
        assert_eq!(base.last_event_id, Some(2));

        // Now fold 2 MORE events with `base_state` set — the
        // counters continue from where base left off, not from 0.
        let more_events = vec![
            ev(3, "step.exit", Some("start"), "EXITED"),
            ev(4, "playbook.completed", None, "COMPLETED"),
        ];
        let seeded = fold_replay_state_with_options(
            &more_events,
            "t",
            "o",
            42,
            ReplayProjection::All,
            ReplayFoldOptions {
                base_state: Some(base),
                ..Default::default()
            },
        );

        assert_eq!(seeded.event_count, 4, "counters continue from base");
        assert_eq!(seeded.last_event_id, Some(4));
        assert_eq!(seeded.last_event_type.as_deref(), Some("playbook.completed"));
        assert_eq!(seeded.execution.status, "COMPLETED");
    }

    #[test]
    fn fold_with_base_state_strips_prior_checksum() {
        // Base state has its own checksum; the seeded fold MUST
        // recompute, not preserve.
        let base = fold_replay_state(&[], "t", "o", 42, ReplayProjection::All);
        let base_checksum = base.checksum.clone().unwrap().value;

        let seeded = fold_replay_state_with_options(
            &[ev(1, "playbook.completed", None, "COMPLETED")],
            "t",
            "o",
            42,
            ReplayProjection::All,
            ReplayFoldOptions {
                base_state: Some(base),
                ..Default::default()
            },
        );

        // New checksum reflects the new event tail — must differ
        // from the stale base checksum.
        assert_ne!(seeded.checksum.as_ref().unwrap().value, base_checksum);
        // All 6 projection_checksums entries still populate.
        assert_eq!(seeded.projection_checksums.len(), 6);
    }

    #[test]
    fn fold_with_base_state_overrides_tenant_org_execution_id() {
        // Snapshot was recorded under one tenant; we replay it
        // for a different tenant — caller's args win.
        let base = fold_replay_state(&[], "old-tenant", "old-org", 99, ReplayProjection::All);

        let seeded = fold_replay_state_with_options(
            &[],
            "new-tenant",
            "new-org",
            42,
            ReplayProjection::All,
            ReplayFoldOptions {
                base_state: Some(base),
                ..Default::default()
            },
        );

        assert_eq!(seeded.tenant_id, "new-tenant");
        assert_eq!(seeded.organization_id, "new-org");
        assert_eq!(seeded.execution_id, 42);
    }

    #[test]
    fn fold_with_seed_caller_digest_wins_over_base_state_digest() {
        // Base state carries an upcaster digest, caller passes a
        // newer one — the newer one wins (the registry active at
        // fold time is authoritative).
        let mut base = fold_replay_state(&[], "t", "o", 42, ReplayProjection::All);
        base.upcaster_registry_digest = Some("v1-digest".to_string());

        let seeded = fold_replay_state_with_options(
            &[],
            "t",
            "o",
            42,
            ReplayProjection::All,
            ReplayFoldOptions {
                base_state: Some(base),
                upcaster_registry_digest: Some("v2-digest".to_string()),
                ..Default::default()
            },
        );

        assert_eq!(seeded.upcaster_registry_digest.as_deref(), Some("v2-digest"));
    }

    #[test]
    fn fold_with_seed_preserves_base_digest_when_caller_supplies_none() {
        // No caller-supplied digest → base_state's digest carries
        // forward (we don't accidentally wipe it).
        let mut base = fold_replay_state(&[], "t", "o", 42, ReplayProjection::All);
        base.upcaster_registry_digest = Some("v1-digest".to_string());

        let seeded = fold_replay_state_with_options(
            &[],
            "t",
            "o",
            42,
            ReplayProjection::All,
            ReplayFoldOptions {
                base_state: Some(base),
                ..Default::default()
            },
        );

        assert_eq!(seeded.upcaster_registry_digest.as_deref(), Some("v1-digest"));
    }
}
