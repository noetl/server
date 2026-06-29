//! Application state for the NoETL Control Plane server.
//!
//! This module defines the shared application state that is
//! passed to all handlers via Axum's state management.

use crate::config::AppConfig;
use crate::db::{DbPool, DbPoolMap};
use crate::sharding::ShardConfig;
use crate::snowflake::{derive_machine_id, SnowflakeGenerator};
use std::sync::Arc;

/// Shared application state.
///
/// This struct holds all shared resources that handlers need access to.
/// It is wrapped in an `Arc` and passed to handlers via Axum's state.
#[derive(Clone)]
pub struct AppState {
    /// Legacy database connection pool.
    ///
    /// In single-pool fallback mode (Phase F R4-1's
    /// `NOETL_SHARDS` empty), this IS the only pool — every
    /// handler that hasn't migrated to [`Self::pools`] uses it.
    /// In sharded mode, `db` is the cluster-wide pool (the
    /// always-master pool for catalog / credential / keychain /
    /// runtime / etc.) so handlers that read cluster-wide tables
    /// keep working without R4-3 touching them.
    ///
    /// Phase F R4-3 migrates per-execution call sites to
    /// `self.pools.pool_for(execution_id)`.  Until that round
    /// lands, every handler reads from `db` regardless of which
    /// table they touch — which is correct in fallback mode
    /// (one pool everywhere) and incorrect-but-tolerated in
    /// sharded mode (per-execution tables would still go to the
    /// cluster master; this is why the kind validation in R4-5
    /// only fires after R4-3 ships).
    pub db: DbPool,

    /// Sharded pool map — N per-shard pools + 1 cluster pool.
    ///
    /// Phase F R4-2 of
    /// [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)
    /// added this.  Use [`DbPoolMap::pool_for`] for per-execution
    /// tables (`event`, `command`, `execution`, `outbox`,
    /// `transient`, `stage`, `frame`, `projection`,
    /// `projection_snapshot`, `result_ref`) and
    /// [`DbPoolMap::cluster`] for cluster-wide tables
    /// (`catalog`, `credential`, `keychain`, `runtime`,
    /// `schedule`, `resource`, `manifest`, `manifest_part`).
    ///
    /// In single-pool fallback mode (NOETL_SHARDS empty), every
    /// accessor returns the same pool as [`Self::db`] — handlers
    /// that opt into `pools` get bit-identical behaviour to the
    /// legacy path.
    pub pools: DbPoolMap,

    /// Application configuration
    pub config: Arc<AppConfig>,

    /// NATS client (optional)
    pub nats: Option<Arc<async_nats::Client>>,

    /// Application-side snowflake ID generator.  Phase F R1.5 of
    /// [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)
    /// moved id generation out of the DB-side `noetl.snowflake_id()`
    /// function into this generator so (a) spans see ids before the
    /// DB round-trip, (b) retries stay idempotent, (c) the upcoming
    /// sharded layout (R4) can pin `machine_id` per pod via the
    /// deployment manifest.  See `src/snowflake.rs` for the id
    /// layout and migration rationale.
    pub snowflake: Arc<SnowflakeGenerator>,

    /// Shard routing configuration.  Phase F R2 of
    /// [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)
    /// added this.  Single-shard default (no enforcement) when
    /// `NOETL_SHARD_INDEX` + `NOETL_SHARD_COUNT` are unset, so
    /// current deployments continue working unchanged.  See
    /// `src/sharding.rs` for the hash-function choice and the
    /// routing semantics; the cross-component design lives on
    /// the [noetl/server wiki sharding-design page](https://github.com/noetl/server/wiki/sharding-design).
    pub shard: Arc<ShardConfig>,

    /// Execution-affinity router (RFC noetl/ai-meta#116).  Routes every trigger
    /// for an execution (`POST /api/events`) to the single replica that
    /// [`ShardConfig::owns`] it, so the off-server drive's chain-head read +
    /// advance are atomic per execution and never fork across replicas.  Inert
    /// (no forwarding) unless `NOETL_EXECUTION_AFFINITY=true` AND `shard_count >
    /// 1` AND `NOETL_PEER_URL_TEMPLATE` is set — so single-replica / prod are
    /// unchanged.  See [`crate::affinity`].
    pub affinity: Arc<crate::affinity::ExecutionAffinity>,

    /// Server start time for uptime calculation
    pub start_time: std::time::Instant,

    /// Per-execution orchestrator state cache (noetl/ai-meta#100 perf).  The
    /// orchestrator advances a cached `WorkflowState` by applying only NEW
    /// events each trigger instead of reloading + replaying the whole event
    /// log (the per-trigger O(n) rebuild + the concurrent-completion memory
    /// spike were the scaling bottleneck — a high-concurrency run OOM'd the
    /// server).  The per-execution lock inside also serialises a single
    /// execution's concurrent completion triggers.
    pub orch_cache: Arc<OrchStateCache>,

    /// Per-execution chain head for the one-level event chain (RFC #115 Phase
    /// 2, noetl/ai-meta#115 §4).  The event-write chokepoint reads + advances it
    /// to stamp each row's `prev_event_id`, so per-execution events form a
    /// walkable singly-linked list.  See [`ChainHeads`].
    pub chain_heads: Arc<ChainHeads>,

    /// Per-execution event-tail ring for the off-server drive tail-attach
    /// accelerator (noetl/ai-meta#156).  Populated at the `noetl_events` publish
    /// chokepoint with the payloads the server just published, drained at
    /// off-server drive dispatch so the worker advances its WAL index without
    /// waiting on the global-stream drain.  Only touched when
    /// `config.offserver_attach_tail` is on; otherwise it stays empty.  See
    /// [`ChainTails`].
    pub chain_tails: Arc<ChainTails>,

    /// Execute-time descriptors for the stateless off-server drive edge (RFC
    /// #115 Phase 4 remainder, noetl/ai-meta#107 step 2).  Carries the
    /// execution-scoped, immutable facts the drive dispatch needs —
    /// `catalog_id` + `routing_meta` — seeded once when the execution starts, so
    /// under `NOETL_STATE_BUILDER=offserver` the drive routes the command
    /// WITHOUT rebuilding `WorkflowState` (ZERO `noetl.event` reads on the drive
    /// path; state CONSTRUCTION runs on the worker pool from the WAL).  Plus a
    /// `terminal` flag stamped by the [`crate::handlers::event_write::emit_events`]
    /// chokepoint when a terminal event (cancel / finalize / playbook
    /// completed|failed) is written, so the drive stops re-dispatching a terminal
    /// execution without reading state.  See [`ExecDescriptors`].
    pub exec_descriptors: Arc<ExecDescriptors>,

    /// Recently-finalized executions, so the [`crate::handlers::event_write`]
    /// chokepoint enforces exactly one terminal event per execution and a
    /// duplicate finalize can't orphan the chain with a NULL-`prev_event_id`
    /// second root (noetl/ai-meta#118).  Bounded + in-memory; see
    /// [`FinalizedGuard`].
    pub finalized_guard: Arc<FinalizedGuard>,

    /// CQRS write-path publisher (noetl/ai-meta#103 phase 2d-3).  Lazily built
    /// on first use from [`Self::nats`] so the `emit_event` chokepoint can
    /// publish to the `noetl_events` stream when
    /// `config.event_ingest_publish_only` is on.  `OnceCell` so the gate-off
    /// (default) path never builds it and the gate-on path ensures the stream
    /// exactly once.  `None` inner stays uninitialised until the first publish.
    pub event_stream_publisher: Arc<tokio::sync::OnceCell<crate::nats::EventStreamPublisher>>,
}

/// Cached orchestrator state for one execution, advanced incrementally.
#[derive(Default)]
pub struct ExecOrchState {
    /// The reconstructed workflow state (None until first built).
    pub state: Option<crate::engine::state::WorkflowState>,
    /// Highest event_id applied to `state`.
    pub last_event_id: i64,
    /// Number of events applied — compared against the live event COUNT to
    /// detect a straggler (an event with id <= last_event_id inserted late);
    /// on mismatch the cache falls back to a full rebuild for correctness.
    pub applied_count: i64,
    /// The `playbook_started` event's meta (pool segment + W3C trace routing),
    /// cached so follow-up command dispatch needn't reload the first event.
    pub routing_meta: Option<serde_json::Value>,
    /// Highest `event_id` folded into the last persisted
    /// `projection_snapshot` (noetl/ai-meta#101 block b).  A rebuild loads
    /// that snapshot + only events newer than this, so the rebuild cost is
    /// bounded by the snapshot interval instead of the whole (growing) event
    /// log — which is what OOM'd the server at scale.
    pub snapshot_version: i64,
    /// Last time the O(events) consistency `COUNT(*)` ran for this execution
    /// (noetl/ai-meta#101 block b throughput).  That count grows with the log
    /// (≈27ms at 60k events) and would dominate the hot path if run on every
    /// trigger, so it's throttled; between checks the incremental apply + the
    /// immediate `trigger_event_id`-straggler handling carry correctness.  Not
    /// serialized (in-memory only).
    pub last_count_check: Option<std::time::Instant>,
    /// Worker-driven drive (noetl/ai-meta#108 slice 3): true while an
    /// `system/orchestrate` command is dispatched to the pool but its result
    /// has not yet been applied.  Set when the scheduler dispatches, cleared
    /// when the completion is applied.  Serialises drives per execution so two
    /// near-simultaneous triggers (or the reconcile poller) don't dispatch two
    /// orchestrate commands → double-issue the same next commands.  In-memory
    /// only; a server restart re-derives the drive from the event log.
    pub orchestrate_in_flight: bool,
}


/// Per-execution chain head for the one-level event chain (RFC #115 Phase 2,
/// noetl/ai-meta#115 §4).  Maps `execution_id → event_id of the last event
/// appended to the chain`.  The event-write chokepoint
/// ([`crate::handlers::event_write::emit_events`]) reads the head as the next
/// row's `prev_event_id`, then advances it — so the per-execution events form a
/// singly-linked list walkable pointer-by-pointer, no `noetl.event` scan.
///
/// In-memory only and **no DB read on the hot path**: a cold slot (execution's
/// first event, or an execution whose earlier events were handled by a
/// different replica / before a restart) yields `None`, i.e. a chain root.
/// This shares the locality assumption the existing [`OrchStateCache`] already
/// relies on — an execution's drive is serialised on one replica — so the chain
/// is continuous for the single-writer / single-replica topology the kind gate
/// validates.  Restart-spanning repair is a Phase 3 builder concern (it
/// re-walks from a durable head); nothing reads `prev_event_id` yet, so a chain
/// restart here is additive metadata, never a regression.
pub struct ChainHeads {
    map: std::sync::Mutex<std::collections::HashMap<i64, i64>>,
    /// Multi-replica coherence backend (RFC #115 program-scale).  `local`
    /// (default) → disabled, the `map` is the whole story (today's behavior).
    /// `nats_kv` → the head is CAS-advanced in a shared KV bucket so 2+ replicas
    /// agree, and `map` becomes a write-through cache / degraded-mode fallback.
    coherence: Arc<crate::coherence::CoherenceKv>,
}

impl Default for ChainHeads {
    fn default() -> Self {
        Self {
            map: std::sync::Mutex::new(std::collections::HashMap::new()),
            coherence: Arc::new(crate::coherence::CoherenceKv::default()),
        }
    }
}

impl ChainHeads {
    /// Build with a shared coherence backend (used by [`AppState::new`]).
    pub fn with_coherence(coherence: Arc<crate::coherence::CoherenceKv>) -> Self {
        Self {
            map: std::sync::Mutex::new(std::collections::HashMap::new()),
            coherence,
        }
    }

    /// Stamp the chain link for a batch of event ids emitted (in order) for one
    /// execution, advancing the head as it goes.  Returns, for each id, the
    /// `prev_event_id` it should carry (`None` for a chain root).
    ///
    /// Under `local` this is one short `std::Mutex` critical section (no `await`
    /// held).  Under `nats_kv` the head before the batch is obtained by a KV
    /// **compare-and-swap** so concurrent emits on different replicas serialise
    /// into a single per-execution chain; the `std::Mutex` is never held across
    /// the KV `await`.
    ///
    /// An empty `event_ids` is a no-op returning an empty vec.
    pub async fn link_batch(&self, execution_id: i64, event_ids: &[i64]) -> Vec<Option<i64>> {
        if event_ids.is_empty() {
            return Vec::new();
        }
        let new_head = *event_ids.last().expect("non-empty checked above");
        if self.coherence.enabled() {
            if let crate::coherence::KvRead::Hit(prev_head) =
                self.coherence.advance_head(execution_id, new_head).await
            {
                // Write-through the local cache (best-effort; KV is the truth).
                self.map.lock().unwrap().insert(execution_id, new_head);
                crate::metrics::record_replica_coherence("chain_head", "link_batch", "kv_ok");
                return Self::prevs_from(prev_head, event_ids);
            }
            crate::metrics::record_replica_coherence("chain_head", "link_batch", "kv_unavailable");
            // Fall through to the in-process map (degraded mode == local).
        }
        let mut map = self.map.lock().unwrap();
        let head = map.get(&execution_id).copied();
        let prevs = Self::prevs_from(head, event_ids);
        map.insert(execution_id, new_head);
        prevs
    }

    /// Given the head *before* a batch + the batch's ids (in order), the
    /// `prev_event_id` each id carries: the first points at the prior head, each
    /// subsequent at its predecessor in the batch.
    fn prevs_from(prev_head: Option<i64>, event_ids: &[i64]) -> Vec<Option<i64>> {
        let mut head = prev_head;
        let mut prevs = Vec::with_capacity(event_ids.len());
        for &eid in event_ids {
            prevs.push(head);
            head = Some(eid);
        }
        prevs
    }

    /// Drop a terminal execution's head (frees memory + the KV entry).  Called
    /// alongside [`OrchStateCache::evict`] on the terminal-event paths.
    pub async fn evict(&self, execution_id: i64) {
        self.map.lock().unwrap().remove(&execution_id);
        if self.coherence.enabled() {
            self.coherence.evict_head(execution_id).await;
        }
    }

    /// Current head for an execution, if any.  Under `nats_kv` the coherent KV
    /// value is authoritative (any replica resolves the same head); a KV miss is
    /// a genuine cold slot, KV-unavailable degrades to the in-process map.
    pub async fn head(&self, execution_id: i64) -> Option<i64> {
        if self.coherence.enabled() {
            match self.coherence.get_head(execution_id).await {
                crate::coherence::KvRead::Hit(v) => {
                    let local_had = self
                        .map
                        .lock()
                        .unwrap()
                        .insert(execution_id, v)
                        .is_some();
                    crate::metrics::record_replica_coherence(
                        "chain_head",
                        "head",
                        if local_had { "kv_local_hit" } else { "kv_remote_hit" },
                    );
                    return Some(v);
                }
                crate::coherence::KvRead::Miss => {
                    crate::metrics::record_replica_coherence("chain_head", "head", "kv_miss");
                    return None;
                }
                crate::coherence::KvRead::Unavailable => {
                    crate::metrics::record_replica_coherence(
                        "chain_head",
                        "head",
                        "kv_unavailable",
                    );
                    // Fall through to local.
                }
            }
        }
        self.map.lock().unwrap().get(&execution_id).copied()
    }
}

/// Per-execution **event tail** ring for the off-server drive tail-attach
/// accelerator (noetl/ai-meta#156).  In-memory only, populated by the server at
/// the `noetl_events` publish chokepoint ([`crate::handlers::event_write::emit_events`])
/// and drained at off-server drive dispatch ([`crate::handlers::events`]).
///
/// ## Why this exists
///
/// The off-server drive's per-hop latency is today coupled to **global**
/// `noetl_events` WAL volume, not to one execution's work: the worker serves a
/// hop only once the pool-side WAL drain (one ephemeral `DeliverAll` consumer
/// racing the entire stream under one mutex) has independently pulled + indexed
/// this hop's `expected_head`.  Under load the drain lags past the worker's ~1s
/// drive-retry budget and the hop drops to the server's 8s reconcile tick — the
/// per-hop variance #156 pins.
///
/// The server is the **producer** of every event the worker needs, and stamps
/// the same `to_stream_json()` payload it publishes to `noetl_events`.  Keeping
/// the most-recent `cap` of those payloads per execution lets the dispatch carry
/// the new tail to the worker directly, so the worker advances its WAL index
/// without waiting on the drain.  The ring is the only new state; it never reads
/// the DB and holds at most `cap` events per non-terminal execution.
///
/// ## Bounded + best-effort
///
/// The ring evicts oldest-first at `cap` and is dropped wholesale on a terminal
/// event (alongside [`ChainHeads::evict`]).  It is an **accelerator, not a source
/// of truth**: the worker's WAL index + drain remain authoritative.  If the
/// attached tail is insufficient to complete the chain to genesis (a gap, or a
/// post-restart cold worker index whose genesis predates the ring), the worker's
/// build is `Incomplete` and falls through to exactly today's retry/drain/
/// reconcile path — so correctness is preserved, worst case equals today.
#[derive(Default)]
pub struct ChainTails {
    map: std::sync::Mutex<std::collections::HashMap<i64, std::collections::VecDeque<serde_json::Value>>>,
}

impl ChainTails {
    /// Append the just-published `noetl_events` payloads for one execution,
    /// evicting oldest-first so the ring never exceeds `cap`.  `cap == 0` is a
    /// no-op (the accelerator stays disabled regardless of the flag).  Called from
    /// the publish branch of the event-write chokepoint, in emit order.
    pub fn push(&self, execution_id: i64, payloads: &[serde_json::Value], cap: usize) {
        if cap == 0 || payloads.is_empty() {
            return;
        }
        let mut map = self.map.lock().unwrap();
        let ring = map.entry(execution_id).or_default();
        for p in payloads {
            ring.push_back(p.clone());
        }
        while ring.len() > cap {
            ring.pop_front();
        }
    }

    /// Snapshot the current tail for an execution (oldest→newest), or an empty
    /// vec when nothing is buffered.  Cloned out under the lock so the caller can
    /// attach it to the dispatch without holding the mutex across an `await`.
    pub fn snapshot(&self, execution_id: i64) -> Vec<serde_json::Value> {
        self.map
            .lock()
            .unwrap()
            .get(&execution_id)
            .map(|ring| ring.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Drop a terminal execution's ring — frees memory.  Called alongside
    /// [`ChainHeads::evict`] on the terminal-event paths.
    pub fn evict(&self, execution_id: i64) {
        self.map.lock().unwrap().remove(&execution_id);
    }
}

/// Execute-time descriptor for the stateless off-server drive edge (RFC #115
/// Phase 4 remainder).  See the [`AppState::exec_descriptors`] field doc.
///
/// `Serialize`/`Deserialize` so the multi-replica coherence backend
/// ([`crate::coherence::CoherenceKv`]) can store it in a JetStream KV bucket
/// under `NOETL_REPLICA_COHERENCE=nats_kv`.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct ExecDescriptor {
    /// The execution's catalog id (immutable for the run).  Sourced from the
    /// `playbook_started` event at execute-time instead of from a rebuilt
    /// `WorkflowState`, so the drive needn't scan `noetl.event` for it.
    pub catalog_id: i64,
    /// The `playbook_started` event's `meta` (pool segment + W3C trace) — the
    /// same shape [`crate::handlers::execute::CommandRouting::from_started_meta`]
    /// reads, cached so follow-up command dispatch needn't reload the first
    /// event.
    pub routing_meta: Option<serde_json::Value>,
    /// Set once a terminal event (cancel / finalize / playbook completed|failed)
    /// is emitted for this execution.  The stateless drive checks this first and
    /// stops re-dispatching a terminal execution — replacing the
    /// `WorkflowState::is_terminal()` guard that needed a rebuilt state.
    pub terminal: bool,
}

/// Per-execution execute-time descriptor cache for the stateless off-server
/// drive edge (RFC #115 Phase 4 remainder, noetl/ai-meta#107 step 2).  In-memory
/// only and no DB read on the hot path: seeded at execute, read by the drive,
/// terminal-stamped at the emit chokepoint.  A cold slot (e.g. a server restart
/// mid-execution) yields `None`, and the drive falls back to the server-built
/// state path for that trigger — which re-seeds the descriptor — so correctness
/// never regresses below the server-built drive.
pub struct ExecDescriptors {
    map: std::sync::Mutex<std::collections::HashMap<i64, ExecDescriptor>>,
    /// Multi-replica coherence backend (RFC #115 program-scale).  `local`
    /// (default) → disabled (today's behavior).  `nats_kv` → the descriptor is a
    /// shared KV entry so a trigger landing on a replica that didn't seed the
    /// execution resolves it (and the terminal flag) coherently instead of
    /// taking a server-built cold fallback.
    coherence: Arc<crate::coherence::CoherenceKv>,
}

impl Default for ExecDescriptors {
    fn default() -> Self {
        Self {
            map: std::sync::Mutex::new(std::collections::HashMap::new()),
            coherence: Arc::new(crate::coherence::CoherenceKv::default()),
        }
    }
}

impl ExecDescriptors {
    /// Build with a shared coherence backend (used by [`AppState::new`]).
    pub fn with_coherence(coherence: Arc<crate::coherence::CoherenceKv>) -> Self {
        Self {
            map: std::sync::Mutex::new(std::collections::HashMap::new()),
            coherence,
        }
    }

    /// Seed `catalog_id` + `routing_meta` at execute-time (or re-seed from the
    /// server-built fallback path after a cold-descriptor drive).  Fills the
    /// load-bearing fields without clobbering a `terminal` flag a concurrent
    /// cancel may already have stamped.  Under `nats_kv` the merge also rides a
    /// KV CAS so a seed + a terminal-stamp from different replicas converge.
    pub async fn seed(
        &self,
        execution_id: i64,
        catalog_id: i64,
        routing_meta: Option<serde_json::Value>,
    ) {
        {
            let mut map = self.map.lock().unwrap();
            let e = map.entry(execution_id).or_default();
            if catalog_id != 0 {
                e.catalog_id = catalog_id;
            }
            if routing_meta.is_some() {
                e.routing_meta = routing_meta.clone();
            }
        }
        if self.coherence.enabled() {
            self.coherence
                .seed_descriptor(execution_id, catalog_id, routing_meta)
                .await;
            crate::metrics::record_replica_coherence("descriptor", "seed", "kv_ok");
        }
    }

    /// Current descriptor snapshot for an execution, if seeded.  Under `nats_kv`
    /// the coherent KV value is authoritative: a KV hit that the local map
    /// missed is a **cross-replica resolve** (`kv_remote_hit`) — the descriptor
    /// another replica seeded, found without a server-built cold fallback.  A KV
    /// miss is a genuine cold slot (never-seeded or evicted everywhere); KV
    /// unavailable degrades to the in-process map.
    pub async fn get(&self, execution_id: i64) -> Option<ExecDescriptor> {
        let local = self.map.lock().unwrap().get(&execution_id).cloned();
        if self.coherence.enabled() {
            match self.coherence.get_descriptor(execution_id).await {
                crate::coherence::KvRead::Hit(desc) => {
                    crate::metrics::record_replica_coherence(
                        "descriptor",
                        "get",
                        if local.is_some() { "kv_local_hit" } else { "kv_remote_hit" },
                    );
                    self.map.lock().unwrap().insert(execution_id, desc.clone());
                    return Some(desc);
                }
                crate::coherence::KvRead::Miss => {
                    crate::metrics::record_replica_coherence("descriptor", "get", "kv_miss");
                    return None;
                }
                crate::coherence::KvRead::Unavailable => {
                    crate::metrics::record_replica_coherence(
                        "descriptor",
                        "get",
                        "kv_unavailable",
                    );
                    return local;
                }
            }
        }
        local
    }

    /// Stamp the terminal flag (emit chokepoint).  Creates the slot if cold so a
    /// cancel that lands before any drive still records the stop signal.
    pub async fn mark_terminal(&self, execution_id: i64) {
        self.map
            .lock()
            .unwrap()
            .entry(execution_id)
            .or_default()
            .terminal = true;
        if self.coherence.enabled() {
            self.coherence.mark_terminal_descriptor(execution_id).await;
            crate::metrics::record_replica_coherence("descriptor", "mark_terminal", "kv_ok");
        }
    }

    /// Drop a terminal execution's descriptor (frees memory + the KV entry).
    /// Called alongside [`OrchStateCache::evict`] on the terminal paths.
    pub async fn evict(&self, execution_id: i64) {
        self.map.lock().unwrap().remove(&execution_id);
        if self.coherence.enabled() {
            self.coherence.evict_descriptor(execution_id).await;
        }
    }
}

/// Bounded set of execution ids that have already emitted a terminal event
/// (`playbook.completed` / `playbook_failed` / `playbook_cancelled`), so the
/// event-write chokepoint can keep the **exactly-one-terminal-per-execution**
/// invariant under the duplicate-finalize race (noetl/ai-meta#118).
///
/// The race: under `NOETL_STATE_BUILDER=offserver` + `PUBLISH_ONLY` on a
/// **single replica**, a high-concurrency fan-out can drive the same execution
/// to terminal twice — the first drive emits the terminal event (chain-linked to
/// the head) and then evicts the chain head + descriptor; a straggler/late
/// trigger then falls through to the server-built path, rebuilds from the
/// materialized WAL that *hasn't caught up* (so the rebuilt state isn't terminal
/// yet — the materializer-lag window), drives again, and emits a SECOND terminal
/// event.  That second event reaches [`ChainHeads::link_batch`] after the head
/// was evicted → `prev_event_id = NULL` → a second chain root (the orphan), and
/// the off-server spine walk then can't reach it → a benign `event_scan`
/// fallback.  Multi-replica execution-affinity (noetl/ai-meta#116) already
/// serialises every finalize to the owning replica, so it never forks — this
/// guard is the single-replica equivalent: the first terminal wins, any later
/// terminal for the same execution is suppressed at the chokepoint *before* it
/// touches the chain.
///
/// In-memory + **bounded** (FIFO eviction past `capacity`) so it never
/// reintroduces the unbounded per-execution growth RFC #115 Phase 6 removed: the
/// only window that matters is the straggler + materializer-lag horizon
/// (seconds), so a few thousand recent ids is ample.  Process-local, like the
/// sibling [`OrchStateCache`] / [`ExecDescriptors`] in-memory maps — multi-replica
/// correctness comes from affinity, not from sharing this set.  A server restart
/// drops the set, but a post-restart straggler is then caught by the
/// server-built terminal guard (the rebuilt state is terminal once the WAL has
/// materialized), so correctness never regresses below today.
pub struct FinalizedGuard {
    inner: std::sync::Mutex<FinalizedInner>,
    capacity: usize,
}

struct FinalizedInner {
    set: std::collections::HashSet<i64>,
    queue: std::collections::VecDeque<i64>,
}

impl Default for FinalizedGuard {
    fn default() -> Self {
        // 8192 recent finalized executions — far past the straggler /
        // materializer-lag horizon, trivially small in memory (≈64 KiB of i64s).
        Self::new(8192)
    }
}

impl FinalizedGuard {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: std::sync::Mutex::new(FinalizedInner {
                set: std::collections::HashSet::new(),
                queue: std::collections::VecDeque::new(),
            }),
            capacity,
        }
    }

    /// Record `execution_id` as finalized.  Returns `true` when it was **newly**
    /// inserted — i.e. this is the FIRST terminal event for the execution and the
    /// caller should write it.  Returns `false` when the execution was already
    /// finalized — a DUPLICATE terminal the caller must suppress so it can't
    /// orphan the chain.  FIFO-evicts the oldest id once `capacity` is exceeded.
    pub fn mark(&self, execution_id: i64) -> bool {
        let mut g = self.inner.lock().unwrap();
        if !g.set.insert(execution_id) {
            return false;
        }
        g.queue.push_back(execution_id);
        while g.queue.len() > self.capacity {
            if let Some(old) = g.queue.pop_front() {
                g.set.remove(&old);
            }
        }
        true
    }

    /// True if a terminal event has already been recorded for `execution_id`
    /// (within the bounded window).  Test/observability helper; the write path
    /// uses [`Self::mark`] for the atomic check-and-record.
    pub fn contains(&self, execution_id: i64) -> bool {
        self.inner.lock().unwrap().set.contains(&execution_id)
    }
}

/// Per-execution orchestrator state cache.  The outer `std::Mutex` guards a
/// short get-or-insert; the inner per-execution `tokio::Mutex` is held across
/// the orchestrator's DB round-trips so a single execution's triggers serialise
/// (different executions never contend).
#[derive(Default)]
pub struct OrchStateCache {
    map: std::sync::Mutex<
        std::collections::HashMap<i64, Arc<tokio::sync::Mutex<ExecOrchState>>>,
    >,
}

impl OrchStateCache {
    /// Get (or create) the per-execution state slot.
    pub fn entry(&self, execution_id: i64) -> Arc<tokio::sync::Mutex<ExecOrchState>> {
        self.map
            .lock()
            .unwrap()
            .entry(execution_id)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(ExecOrchState::default())))
            .clone()
    }

    /// Drop a terminal execution's cached state (frees memory).
    pub fn evict(&self, execution_id: i64) {
        self.map.lock().unwrap().remove(&execution_id);
    }

    /// Snapshot of the currently-cached (i.e. non-terminal, not-yet-evicted)
    /// execution ids.  The background reconcile poller iterates these to
    /// force-advance any execution that got stuck — e.g. a cursor that missed a
    /// non-triggering straggler and stopped emitting events, so no trigger would
    /// otherwise retry.  Cheap: a short lock + clone of the keys.
    pub fn active_executions(&self) -> Vec<i64> {
        self.map.lock().unwrap().keys().copied().collect()
    }
}

impl AppState {
    /// Create a new application state.
    ///
    /// # Arguments
    ///
    /// * `db` - Legacy database connection pool (kept for handlers
    ///   not yet migrated to [`Self::pools`] — see field doc).
    /// * `pools` - Sharded pool map.  In single-pool fallback
    ///   mode this is constructed from the same `db` connection
    ///   via [`DbPoolMap::new`] with an empty [`crate::config::ShardingConfig`];
    ///   callers should use [`AppState::new_legacy`] when they
    ///   don't have a separate `ShardingConfig` to pass.
    /// * `config` - Application configuration
    /// * `nats` - Optional NATS client
    ///
    /// Reads `server_machine_id` from `config.server_machine_id`
    /// (envy: `NOETL_SERVER_MACHINE_ID`).  When unset, derives a
    /// 10-bit id from the process hostname via FNV-1a — fine for
    /// local dev; the deployment manifest should set the env var
    /// explicitly per replica in production.
    ///
    /// # Returns
    ///
    /// A new `AppState` instance.
    ///
    /// # Panics
    ///
    /// Panics if the configured `server_machine_id` exceeds the
    /// 10-bit max (1023).  The caller should validate at
    /// config-load time; this is the last-resort guard.
    pub fn new(
        db: DbPool,
        pools: DbPoolMap,
        config: AppConfig,
        nats: Option<async_nats::Client>,
    ) -> Self {
        let machine_id = config.server_machine_id.unwrap_or_else(|| {
            let hostname = std::env::var("HOSTNAME")
                .or_else(|_| std::env::var("COMPUTERNAME"))
                .unwrap_or_else(|_| "noetl-server-local".to_string());
            derive_machine_id(&hostname)
        });
        let snowflake = SnowflakeGenerator::new(machine_id)
            .expect("server_machine_id must fit in 10 bits; validate config at startup");
        tracing::info!(
            machine_id = snowflake.machine_id(),
            source = if config.server_machine_id.is_some() {
                "NOETL_SERVER_MACHINE_ID"
            } else {
                "derived from HOSTNAME"
            },
            "Snowflake generator initialized"
        );

        // Phase F R2: shard configuration.  Single-shard default
        // (no enforcement) when neither env var is set — that
        // keeps current single-replica deployments working
        // without any change.  Validation: shard_index <
        // shard_count must hold; startup panics otherwise so we
        // fail fast on a config bug rather than silently
        // mis-routing requests.
        let shard_count = config.shard_count.unwrap_or(1);
        // noetl/ai-meta#116: derive shard_index from the pod's hostname ordinal
        // (StatefulSet `name-N`) when asked, so one manifest with identical env
        // gives each pod a distinct shard.  An explicit NOETL_SHARD_INDEX always
        // wins; a hostname with no trailing ordinal falls back to 0.
        let shard_index = config.shard_index.unwrap_or_else(|| {
            if config.shard_index_from_hostname {
                let hostname = std::env::var("HOSTNAME")
                    .or_else(|_| std::env::var("COMPUTERNAME"))
                    .unwrap_or_default();
                crate::affinity::shard_index_from_hostname(&hostname).unwrap_or(0)
            } else {
                0
            }
        });
        let shard = ShardConfig::new(shard_index, shard_count).unwrap_or_else(|e| {
            panic!("invalid shard config (NOETL_SHARD_INDEX / NOETL_SHARD_COUNT): {e}")
        });
        tracing::info!(
            shard_index = shard.shard_index,
            shard_count = shard.shard_count,
            sharding_enabled = shard.shard_count > 1,
            source = if config.shard_index.is_some() || config.shard_count.is_some() {
                "NOETL_SHARD_INDEX / NOETL_SHARD_COUNT"
            } else {
                "default (no sharding)"
            },
            "Shard configuration initialized"
        );

        let shard = Arc::new(shard);

        // Execution-affinity router (RFC noetl/ai-meta#116).  Single-owner write
        // ordering for the off-server drive: every trigger for an execution is
        // routed to the replica that `owns()` it.  Inert unless the flag is on,
        // shard_count > 1, and a peer template is set — so prod is unchanged.
        let affinity = Arc::new(crate::affinity::ExecutionAffinity::new(
            config.execution_affinity,
            config.peer_url_template.clone(),
            shard.clone(),
        ));
        tracing::info!(
            execution_affinity = config.execution_affinity,
            affinity_active = affinity.active(),
            peer_url_template = config.peer_url_template.as_deref().unwrap_or("(unset)"),
            "Execution-affinity router initialized"
        );

        // Multi-replica coherence backend (RFC #115 program-scale,
        // noetl/ai-meta#107).  `local` (default) → disabled, so `ChainHeads` +
        // `ExecDescriptors` are the in-process maps (prod-unchanged).  `nats_kv`
        // → both are backed by shared JetStream KV buckets so 2+ replicas resolve
        // the same watermark/descriptor.  One backend shared by both structures.
        let nats = nats.map(Arc::new);
        let coherence = Arc::new(crate::coherence::CoherenceKv::new(
            nats.clone(),
            config.replica_coherence,
        ));
        tracing::info!(
            replica_coherence = ?config.replica_coherence,
            coherence_enabled = coherence.enabled(),
            "Replica coherence backend initialized"
        );

        Self {
            db,
            pools,
            config: Arc::new(config),
            nats,
            snowflake: Arc::new(snowflake),
            shard,
            affinity,
            start_time: std::time::Instant::now(),
            orch_cache: Arc::new(OrchStateCache::default()),
            chain_heads: Arc::new(ChainHeads::with_coherence(coherence.clone())),
            chain_tails: Arc::new(ChainTails::default()),
            exec_descriptors: Arc::new(ExecDescriptors::with_coherence(coherence)),
            finalized_guard: Arc::new(FinalizedGuard::default()),
            event_stream_publisher: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    /// Convenience constructor for tests + paths that haven't
    /// loaded a [`ShardingConfig`] yet.  Wraps the legacy `db`
    /// pool in a single-pool [`DbPoolMap`] (no per-shard pools,
    /// no separate cluster pool — the same `db` handle covers
    /// every accessor).
    ///
    /// `main.rs` uses the full [`AppState::new`] with a pool map
    /// built from [`ShardingConfig::from_env`] so the production
    /// path honors `NOETL_SHARDS` if set.  Test code that
    /// already has a `DbPool` in hand uses this shim.
    pub fn new_legacy(
        db: DbPool,
        config: AppConfig,
        nats: Option<async_nats::Client>,
    ) -> Self {
        let pools = DbPoolMap::from_single_pool(db.clone());
        Self::new(db, pools, config, nats)
    }

    /// Get the server uptime in seconds.
    pub fn uptime_seconds(&self) -> u64 {
        self.start_time.elapsed().as_secs()
    }

    /// Check if NATS is configured and connected.
    pub fn has_nats(&self) -> bool {
        self.nats.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::{ChainHeads, ChainTails, ExecDescriptors, FinalizedGuard};

    // Note: Full tests require a database connection
    // These are placeholder tests for documentation

    #[test]
    fn test_uptime() {
        // AppState::new requires a real DB pool, so we can't easily test here
        // This is a documentation placeholder
    }

    // noetl/ai-meta#156: the per-execution event-tail ring is a bounded,
    // oldest-first FIFO, snapshot-able, evicted on terminal.
    #[test]
    fn chain_tails_ring_is_bounded_fifo() {
        let tails = ChainTails::default();
        let ev = |id: i64| serde_json::json!({ "event_id": id });

        // cap == 0 → disabled, nothing buffered regardless of the flag.
        tails.push(1, &[ev(10)], 0);
        assert!(tails.snapshot(1).is_empty(), "cap 0 must buffer nothing");

        // Append within cap → snapshot is oldest→newest.
        tails.push(1, &[ev(10), ev(11)], 3);
        tails.push(1, &[ev(12)], 3);
        let ids: Vec<i64> = tails
            .snapshot(1)
            .iter()
            .map(|v| v["event_id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![10, 11, 12]);

        // Exceeding cap evicts oldest-first.
        tails.push(1, &[ev(13), ev(14)], 3);
        let ids: Vec<i64> = tails
            .snapshot(1)
            .iter()
            .map(|v| v["event_id"].as_i64().unwrap())
            .collect();
        assert_eq!(ids, vec![12, 13, 14], "ring keeps the newest `cap` events");

        // Per-execution isolation.
        assert!(tails.snapshot(2).is_empty());

        // Terminal eviction frees the ring.
        tails.evict(1);
        assert!(tails.snapshot(1).is_empty());
    }

    // noetl/ai-meta#118: exactly-one-terminal-per-execution.  The chokepoint
    // calls `mark()` for every terminal event; the FIRST returns true (write it),
    // any later one returns false (suppress the duplicate before it can orphan
    // the chain).
    #[test]
    fn finalized_guard_first_terminal_wins_duplicate_suppressed() {
        let g = FinalizedGuard::new(8);
        assert!(!g.contains(7), "cold execution is not finalized");
        assert!(g.mark(7), "first terminal for an execution is newly inserted");
        assert!(g.contains(7), "now recorded as finalized");
        // Every subsequent terminal for the SAME execution is a duplicate.
        assert!(!g.mark(7), "second terminal is a suppressed duplicate");
        assert!(!g.mark(7), "third terminal is still a suppressed duplicate");
        // A different execution is independent.
        assert!(g.mark(8), "a different execution's first terminal still wins");
        assert!(!g.mark(8));
    }

    #[test]
    fn finalized_guard_is_bounded_fifo() {
        // Capacity 2: inserting a third id evicts the oldest, so its terminal
        // would be (correctly) treated as fresh again — acceptable because the
        // window only needs to span the seconds-long straggler horizon, far
        // inside any real capacity.  This asserts the bound holds (no unbounded
        // growth) rather than infinite memory.
        let g = FinalizedGuard::new(2);
        assert!(g.mark(1));
        assert!(g.mark(2));
        assert!(g.mark(3)); // evicts id 1 (oldest)
        assert!(!g.contains(1), "oldest id evicted past capacity");
        assert!(g.contains(2));
        assert!(g.contains(3));
        // id 1 fell out of the window, so it is seen as fresh again.
        assert!(g.mark(1), "evicted id is fresh again (bounded window)");
    }

    // RFC #115 Phase 4 remainder: the execute-time descriptor carries catalog_id
    // + routing so the off-server drive routes without rebuilding state.
    // `ChainHeads::default()` / `ExecDescriptors::default()` are
    // coherence-disabled (local), so these tests assert the in-process
    // behavior — the prod/default path the `nats_kv` backing must stay
    // bit-compatible with.
    #[tokio::test]
    async fn exec_descriptor_seed_get_and_terminal() {
        let d = ExecDescriptors::default();
        assert!(d.get(7).await.is_none(), "cold execution has no descriptor");

        d.seed(7, 42, Some(serde_json::json!({"execution_pool": "default"}))).await;
        let got = d.get(7).await.expect("seeded");
        assert_eq!(got.catalog_id, 42);
        assert_eq!(got.routing_meta.unwrap()["execution_pool"], "default");
        assert!(!got.terminal, "freshly seeded is not terminal");

        // A terminal stamp (emit chokepoint) flips the flag without clobbering
        // the seeded facts.
        d.mark_terminal(7).await;
        let got = d.get(7).await.expect("still present");
        assert!(got.terminal, "terminal flag set");
        assert_eq!(got.catalog_id, 42, "catalog_id preserved across terminal stamp");

        d.evict(7).await;
        assert!(d.get(7).await.is_none(), "evicted");
    }

    #[tokio::test]
    async fn exec_descriptor_seed_preserves_terminal_and_does_not_zero_catalog() {
        let d = ExecDescriptors::default();
        // A cancel can stamp terminal before any drive seeded catalog_id.
        d.mark_terminal(9).await;
        // A late seed (e.g. the recovery rebuild) must keep the terminal flag and
        // must not zero catalog_id when passed 0.
        d.seed(9, 0, None).await;
        let got = d.get(9).await.expect("present");
        assert!(got.terminal, "terminal flag survives a seed");
        assert_eq!(got.catalog_id, 0, "catalog_id stays 0 (nothing real to seed yet)");
        // A real seed fills catalog_id, still keeping terminal.
        d.seed(9, 55, Some(serde_json::json!({"x": 1}))).await;
        let got = d.get(9).await.expect("present");
        assert_eq!(got.catalog_id, 55);
        assert!(got.terminal);
    }

    // RFC #115 §4: the per-execution chain head turns a stream of emitted event
    // ids into a walkable singly-linked list.
    #[tokio::test]
    async fn chain_head_links_first_event_is_root() {
        let heads = ChainHeads::default();
        // First batch: the execution's first event has no predecessor (root).
        let prevs = heads.link_batch(7, &[100]).await;
        assert_eq!(prevs, vec![None], "execution's first event is a chain root");
        assert_eq!(heads.head(7).await, Some(100));
    }

    #[tokio::test]
    async fn chain_head_links_batch_in_order() {
        let heads = ChainHeads::default();
        // A multi-row batch (e.g. a cursor fan-out's N command.issued events)
        // links each row to the previous one in order; the first to the head.
        let prevs = heads.link_batch(7, &[10, 11, 12]).await;
        assert_eq!(prevs, vec![None, Some(10), Some(11)]);
        assert_eq!(heads.head(7).await, Some(12), "head advances to the last id");
        // A following batch continues the chain from the advanced head.
        let prevs = heads.link_batch(7, &[20, 21]).await;
        assert_eq!(prevs, vec![Some(12), Some(20)]);
        assert_eq!(heads.head(7).await, Some(21));
    }

    #[tokio::test]
    async fn chain_head_is_per_execution() {
        let heads = ChainHeads::default();
        heads.link_batch(1, &[10, 11]).await;
        heads.link_batch(2, &[90]).await;
        // Distinct executions never share a head.
        assert_eq!(heads.head(1).await, Some(11));
        assert_eq!(heads.head(2).await, Some(90));
        let p1 = heads.link_batch(1, &[12]).await;
        let p2 = heads.link_batch(2, &[91]).await;
        assert_eq!(p1, vec![Some(11)]);
        assert_eq!(p2, vec![Some(90)]);
    }

    #[tokio::test]
    async fn chain_head_walk_reconstructs_full_sequence_no_gaps() {
        // Property the kind validation asserts in SQL: walking prev_event_id
        // from the head reconstructs the full per-execution sequence with no
        // gaps.  Model it over the in-memory linker.
        let heads = ChainHeads::default();
        let ids = [5_i64, 6, 7, 8, 9];
        let prevs = heads.link_batch(42, &ids).await;
        // Build the prev map the chain would persist.
        let mut prev_of = std::collections::HashMap::new();
        for (id, prev) in ids.iter().zip(&prevs) {
            prev_of.insert(*id, *prev);
        }
        // Walk backward from the head.
        let mut walked = Vec::new();
        let mut cur = heads.head(42).await;
        while let Some(id) = cur {
            walked.push(id);
            cur = prev_of.get(&id).copied().flatten();
        }
        walked.reverse();
        assert_eq!(walked, ids, "pointer walk == full emit sequence, no gaps");
    }

    #[tokio::test]
    async fn chain_head_evict_drops_state() {
        let heads = ChainHeads::default();
        heads.link_batch(7, &[10]).await;
        assert_eq!(heads.head(7).await, Some(10));
        heads.evict(7).await;
        assert_eq!(heads.head(7).await, None, "evicted execution starts a fresh chain");
        // After eviction the next event is treated as a root again.
        let prevs = heads.link_batch(7, &[20]).await;
        assert_eq!(prevs, vec![None]);
    }

    #[tokio::test]
    async fn chain_head_empty_batch_is_noop() {
        let heads = ChainHeads::default();
        assert_eq!(heads.link_batch(7, &[]).await, Vec::<Option<i64>>::new());
        assert_eq!(heads.head(7).await, None);
    }

    /// Walk `prev_event_id` backward from `head`, returning the reconstructed
    /// sequence (root-first).  Returns `None` if the walk hits a non-genesis
    /// event whose `prev` is `NULL` — exactly the off-server `chain_walk_from`
    /// →`build_spine_to` `Incomplete` condition (the WAL-chain-incomplete loop).
    /// `genesis` is the lowest id (`playbook_started`) whose `NULL` prev is the
    /// legitimate chain root.
    fn walk_chain(
        head: Option<i64>,
        prev_of: &std::collections::HashMap<i64, Option<i64>>,
        genesis: i64,
    ) -> Option<Vec<i64>> {
        let mut walked = Vec::new();
        let mut cur = head;
        while let Some(id) = cur {
            walked.push(id);
            match prev_of.get(&id).copied().flatten() {
                Some(prev) => cur = Some(prev),
                None if id == genesis => cur = None, // reached the real root
                None => return None,                // orphaned non-genesis head
            }
        }
        walked.reverse();
        Some(walked)
    }

    /// noetl/ai-meta#121: a `command.claimed` written through a path that
    /// bypasses the chain-link chokepoint (the gate-off in-tx claim INSERT)
    /// gets `prev_event_id = NULL` AND never advances the head, so the next
    /// event (`command.started`) links back to `command.issued`, skipping the
    /// orphaned claim.  The off-server spine walk from the orphan head then
    /// can't reach genesis → `Incomplete` → re-drive loop.  This test models
    /// the orphan (the bug) and the linked sequence (the fix), asserting the
    /// pointer walk only completes once the claim is linked.
    #[tokio::test]
    async fn chain_head_claim_orphan_vs_linked() {
        // Snowflake-ish ascending ids for one execution.
        let (started, issued, claimed, cmd_started) = (100_i64, 101, 102, 103);

        // --- BUG: command.claimed bypasses link_batch (raw in-tx INSERT). ---
        // The chokepoint links playbook_started, command.issued, then (skipping
        // the claim) command.started off the un-advanced head (= command.issued).
        let buggy = ChainHeads::default();
        let mut prev_of = std::collections::HashMap::new();
        for (id, prev) in [started, issued]
            .iter()
            .zip(buggy.link_batch(7, &[started, issued]).await)
        {
            prev_of.insert(*id, prev);
        }
        // The claim is written WITHOUT the chokepoint: NULL prev, head untouched.
        prev_of.insert(claimed, None);
        // command.started links off the still-at-`issued` head, skipping claimed.
        for (id, prev) in [cmd_started]
            .iter()
            .zip(buggy.link_batch(7, &[cmd_started]).await)
        {
            prev_of.insert(*id, prev);
        }
        assert_eq!(prev_of[&cmd_started], Some(issued), "started skips claimed");
        assert_eq!(prev_of[&claimed], None, "claimed is an orphan (NULL prev)");
        // If the orphan ever becomes the head, the off-server walk can't complete.
        assert_eq!(
            walk_chain(Some(claimed), &prev_of, started),
            None,
            "orphan claim head → WAL chain incomplete (the re-drive loop)"
        );

        // --- FIX: command.claimed flows through link_batch like every event. ---
        let fixed = ChainHeads::default();
        let mut prev_of = std::collections::HashMap::new();
        let seq = [started, issued, claimed, cmd_started];
        for (id, prev) in seq.iter().zip(fixed.link_batch(7, &seq).await) {
            prev_of.insert(*id, prev);
        }
        assert_eq!(prev_of[&claimed], Some(issued), "claim links to command.issued");
        assert_eq!(prev_of[&cmd_started], Some(claimed), "started links to claim");
        assert_eq!(fixed.head(7).await, Some(cmd_started));
        // The off-server spine walk now reconstructs the full sequence — complete.
        assert_eq!(
            walk_chain(fixed.head(7).await, &prev_of, started),
            Some(seq.to_vec()),
            "linked claim → off-server chain-walk completes (no re-drive loop)"
        );
    }
}
