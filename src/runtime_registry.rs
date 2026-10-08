//! **The D8 runtime registry, reached** — server self-registration, leases, and topology
//! discovery (noetl/ai-meta#455 P1–P4).
//!
//! # Why this module exists
//!
//! ehdb v0.5.0 shipped `register_kind`, `discover`, `list_live_at` and `watch_since`, and
//! this process called **none** of them: measured on prod v3.124.0, all four had **zero**
//! callers in `noetl/server`. The registry existed, was linked, was released — and was
//! dormant. *Adopting a library is not reaching its capability.*
//!
//! So D8 held worker registrations written by workers and nothing else. A question as basic
//! as *"which server instances are live right now"* had no answer, and the topology vision —
//! every object **and service** registered and discoverable — had no server in it.
//!
//! # The lease is the mechanism, not a flag
//!
//! A registration carries `last_seen_micros` and is live only while
//! `now - last_seen < ttl`. Nothing marks an instance dead: a process that stops
//! heartbeating **expires**. That is what makes this a lease rather than a status column
//! somebody has to remember to update — and a status column nobody updates is exactly the
//! representation-drift shape this platform keeps finding.
//!
//! ⚠ **`liveness_coverage` is reported alongside the answer.** A pre-P1 record has
//! `last_seen_micros == 0`, which `list_live_at` treats as *unknown, not dead* — failing
//! open deliberately, because dropping a worker that simply predates the field would be
//! worse. But an operator must be able to SEE that the answer rests on incomplete
//! information rather than infer it, so the topology response carries the count.
//!
//! # Storage
//!
//! Its own dataset (`d8_runtime`) under the **same mounted root** as the embedded event
//! shadow, and it reuses that module's `durable_root_usable` guard. That is not tidiness:
//! `LocalFsSubstrate::new` calls `create_dir_all`, so an unmounted root opens
//! **successfully** onto the container's ephemeral layer and accumulates until the kubelet
//! evicts the pod (noetl/server#419). A registry that can evict the server it registers
//! would be a liability rather than a capability.
//!
//! ⓘ **Growth is bounded and small.** Every heartbeat appends one op, and D8 deliberately
//! does **not** opt into `supersede_key` compaction because `watch_since` reads history. At
//! the default 60 s interval that is ~1,440 ops/day/instance at roughly 200 B each —
//! ~0.3 MB/day, against the ~51 MB/day the event shadow already writes to the same volume.
//! Retention for D8 is a follow-up, not a blocker.

use std::sync::{Arc, Mutex, OnceLock};

use ehdb_l0::runtime::{RuntimeKind, RuntimeState, RuntimeStore};
use ehdb_l0::substrate::{DurableSubstrate, LocalFsSubstrate};

/// Opt-out. The registry follows the embedded engine, because it needs the same mounted
/// root; set this to `false` to keep it off while the shadow stays on.
pub const REGISTRY_ENV: &str = "NOETL_RUNTIME_REGISTRY";
/// Lease length. A registration not renewed within this window stops being discoverable.
pub const TTL_ENV: &str = "NOETL_RUNTIME_TTL_SECS";
/// Renewal interval. Kept well under the TTL so one missed tick is not an expiry.
pub const HEARTBEAT_ENV: &str = "NOETL_RUNTIME_HEARTBEAT_SECS";

const DEFAULT_TTL_SECS: u64 = 180;
const DEFAULT_HEARTBEAT_SECS: u64 = 60;

/// `true` unless explicitly disabled, and only when the embedded root is in play.
pub fn registry_enabled() -> bool {
    if !crate::handlers::ehdb_embedded::embedded_enabled() {
        return false;
    }
    !matches!(
        std::env::var(REGISTRY_ENV).ok().as_deref(),
        Some("false") | Some("0") | Some("off")
    )
}

fn env_secs(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

/// The lease window, in microseconds — the unit `list_live_at` takes.
pub fn ttl_micros() -> u64 {
    env_secs(TTL_ENV, DEFAULT_TTL_SECS).saturating_mul(1_000_000)
}

/// How often the heartbeat task renews.
pub fn heartbeat_interval_secs() -> u64 {
    let hb = env_secs(HEARTBEAT_ENV, DEFAULT_HEARTBEAT_SECS);
    let ttl = env_secs(TTL_ENV, DEFAULT_TTL_SECS);
    // ⚠ A heartbeat at or above the TTL expires the instance between its own renewals, so
    // the registry would flap for a perfectly healthy process. Clamp rather than trust the
    // operator to keep two knobs consistent.
    if hb >= ttl {
        (ttl / 3).max(1)
    } else {
        hb
    }
}

/// Wall clock in microseconds — the same base `list_live_at` compares against.
pub fn now_micros() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// This instance's registry id.
///
/// The pod name is the useful identity in Kubernetes (it is what `kubectl logs` takes), and
/// `HOSTNAME` is the pod name. The machine id is appended when set so two pods that somehow
/// share a hostname stay distinct.
///
/// ⚠ Constrained to D8's id charset `[A-Za-z0-9-_.:]`, because the id becomes a substrate
/// key and an index dimension. A `/` in a substrate key is a directory traversal waiting to
/// happen, so anything else is replaced rather than passed through.
pub fn self_id() -> String {
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "noetl-server".to_string());
    let machine = std::env::var("NOETL_SERVER_MACHINE_ID").unwrap_or_default();
    let raw = if machine.is_empty() {
        host
    } else {
        format!("{host}:{machine}")
    };
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// What this instance advertises about itself. Free-form by D8's contract, so it carries
/// the version a discovery consumer most often wants.
pub fn self_contract() -> String {
    format!("noetl-server/{}", env!("CARGO_PKG_VERSION"))
}

// ---------------------------------------------------------------------------
// The testable core: functions over a store, with no process-wide state.
// ---------------------------------------------------------------------------

/// Register `id` as a live instance of `kind`.
pub fn register_in(
    store: &mut RuntimeStore,
    kind: RuntimeKind,
    id: &str,
    contract: &str,
    now: u64,
) -> Result<(), String> {
    store
        .register_kind(kind, id, contract, now)
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Renew `id`'s lease. `Ok(false)` means there was nothing registered to renew.
pub fn heartbeat_in(store: &mut RuntimeStore, id: &str, now: u64) -> Result<bool, String> {
    Ok(store
        .heartbeat_at(id, now)
        .map_err(|e| e.to_string())?
        .is_some())
}

/// One kind's live members.
pub fn discover_in(
    store: &RuntimeStore,
    kind: RuntimeKind,
    now: u64,
    ttl: u64,
) -> Result<Vec<RuntimeState>, String> {
    store.discover(kind, now, ttl).map_err(|e| e.to_string())
}

/// Every live registration, grouped by kind, with the liveness-coverage caveat attached.
pub fn topology_in(store: &RuntimeStore, now: u64, ttl: u64) -> Result<Topology, String> {
    let mut groups: Vec<KindGroup> = Vec::new();
    for kind in [
        RuntimeKind::Server,
        RuntimeKind::Gateway,
        RuntimeKind::Ehdb,
        RuntimeKind::Worker,
        RuntimeKind::Playbook,
        RuntimeKind::Execution,
    ] {
        let members = store
            .discover(kind, now, ttl)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|s| Member {
                id: s.id().to_string(),
                contract: s.contract.clone(),
                last_seen_micros: s.last_seen_micros,
                age_secs: if s.last_seen_micros == 0 {
                    None
                } else {
                    Some(now.saturating_sub(s.last_seen_micros) / 1_000_000)
                },
            })
            .collect::<Vec<_>>();
        groups.push(KindGroup {
            kind: format!("{kind:?}"),
            count: members.len(),
            members,
        });
    }
    let (live, unknown_timestamp) = store
        .liveness_coverage(now, ttl)
        .map_err(|e| e.to_string())?;
    Ok(Topology {
        now_micros: now,
        ttl_secs: ttl / 1_000_000,
        live_total: live,
        unknown_timestamp,
        kinds: groups,
    })
}

/// One live member of the fleet.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Member {
    pub id: String,
    pub contract: String,
    pub last_seen_micros: u64,
    /// `None` when the registration predates `last_seen_micros` — *unknown*, not fresh.
    pub age_secs: Option<u64>,
}

/// Live members of one kind.
#[derive(Debug, Clone, serde::Serialize)]
pub struct KindGroup {
    pub kind: String,
    pub count: usize,
    pub members: Vec<Member>,
}

/// The live topology, as the discovery endpoint returns it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Topology {
    pub now_micros: u64,
    pub ttl_secs: u64,
    pub live_total: usize,
    /// How many of `live_total` have no timestamp, so an operator can see the answer rests
    /// on incomplete information instead of inferring it.
    pub unknown_timestamp: usize,
    pub kinds: Vec<KindGroup>,
}

// ---------------------------------------------------------------------------
// Process-wide glue.
// ---------------------------------------------------------------------------

static STORE: OnceLock<Option<Arc<Mutex<RuntimeStore>>>> = OnceLock::new();

fn open_store() -> Option<Arc<Mutex<RuntimeStore>>> {
    if !registry_enabled() {
        return None;
    }
    let dir = crate::handlers::ehdb_embedded::embedded_dir();
    // ⚠ The same mounted-root refusal the event shadow uses. See the module docs: an
    // unmounted root opens successfully onto ephemeral storage and ends in an eviction.
    if !crate::handlers::ehdb_embedded::durable_root_usable(std::path::Path::new(&dir)) {
        tracing::error!(target: "noetl_server::runtime_registry", dir = %dir,
            "runtime registry root is not on a mounted volume; registry stays off");
        return None;
    }
    let substrate: Arc<dyn DurableSubstrate> =
        match LocalFsSubstrate::new(format!("{dir}/runtime-substrate")) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::error!(target: "noetl_server::runtime_registry", error = %e,
                    "runtime registry substrate could not be opened; registry stays off");
                return None;
            }
        };
    match RuntimeStore::open(
        RuntimeStore::config(format!("{dir}/runtime-local")),
        substrate,
    ) {
        Ok(s) => {
            tracing::info!(target: "noetl_server::runtime_registry", dir = %dir,
                "runtime registry open");
            Some(Arc::new(Mutex::new(s)))
        }
        Err(e) => {
            tracing::error!(target: "noetl_server::runtime_registry", error = %e,
                "runtime registry open failed; registry stays off");
            None
        }
    }
}

/// The process-wide registry store, or `None` when it is off or unopenable.
pub fn store() -> Option<&'static Arc<Mutex<RuntimeStore>>> {
    STORE.get_or_init(open_store).as_ref()
}

/// Register this server instance. Called once at startup.
///
/// ⚠ Never returns an error and never panics. A registry that can fail a server start is a
/// liability, not a capability — the same posture `ehdb_embedded::shadow_append` takes on
/// the write path.
pub fn self_register() {
    let Some(store) = store() else { return };
    let id = self_id();
    let contract = self_contract();
    let mut guard = match store.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    match register_in(
        &mut guard,
        RuntimeKind::Server,
        &id,
        &contract,
        now_micros(),
    ) {
        Ok(()) => {
            tracing::info!(target: "noetl_server::runtime_registry",
                id = %id, contract = %contract, ttl_secs = ttl_micros() / 1_000_000,
                "server registered itself in the D8 runtime registry");
            crate::metrics::record_runtime_registry("registered");
        }
        Err(e) => {
            tracing::warn!(target: "noetl_server::runtime_registry", error = %e,
                "server self-registration failed; discovery will not list this instance");
            crate::metrics::record_runtime_registry("register_failed");
        }
    }
}

/// Renew this instance's lease forever, at `heartbeat_interval_secs()`.
pub fn spawn_heartbeat() {
    if store().is_none() {
        return;
    }
    let id = self_id();
    let every = heartbeat_interval_secs();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(every));
        // The first tick fires immediately; `self_register` already wrote a fresh
        // timestamp, so skip it rather than double-writing on startup.
        tick.tick().await;
        loop {
            tick.tick().await;
            let Some(store) = store() else { return };
            let renewed = {
                let mut guard = match store.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                heartbeat_in(&mut guard, &id, now_micros())
            };
            match renewed {
                Ok(true) => crate::metrics::record_runtime_registry("heartbeat"),
                // Nothing to renew: the registration is gone (a cold store, or an expiry
                // this process slept through). Re-register rather than silently stopping
                // being discoverable while still running.
                Ok(false) => {
                    tracing::warn!(target: "noetl_server::runtime_registry", id = %id,
                        "no registration to renew; re-registering");
                    crate::metrics::record_runtime_registry("reregistered");
                    self_register();
                }
                Err(e) => {
                    tracing::warn!(target: "noetl_server::runtime_registry", error = %e,
                        "heartbeat failed");
                    crate::metrics::record_runtime_registry("heartbeat_failed");
                }
            }
        }
    });
}

/// The live topology, for the discovery endpoint.
pub fn topology() -> Option<Topology> {
    let store = store()?;
    let guard = match store.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    topology_in(&guard, now_micros(), ttl_micros()).ok()
}

/// Topology changes after `after_op_seq`, with the cursor to resume from.
pub fn watch(after_op_seq: u64) -> Option<(Vec<WatchOp>, u64)> {
    let store = store()?;
    let guard = match store.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let (ops, cursor) = guard.watch_since(after_op_seq).ok()?;
    Some((
        ops.into_iter()
            .map(|op| WatchOp {
                op_seq: op.op_seq,
                id: op.worker_id,
                kind: format!("{:?}", op.kind),
                event: format!("{:?}", op.event),
                contract: op.contract,
            })
            .collect(),
        cursor,
    ))
}

/// One topology change, as the watch endpoint returns it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WatchOp {
    pub op_seq: u64,
    pub id: String,
    pub kind: String,
    pub event: String,
    pub contract: String,
}
