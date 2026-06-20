//! Multi-replica coherence for the off-server drive's per-execution state.
//!
//! RFC noetl/ai-meta#115 program-scale step (noetl/ai-meta#107).
//!
//! ## The problem
//!
//! The off-server drive edge keys two execution-scoped facts off in-memory
//! [`AppState`](crate::state::AppState) maps:
//!
//! - [`ChainHeads`](crate::state::ChainHeads) — the `prev_event_id` watermark
//!   the `emit_events` chokepoint stamps so per-execution events form a walkable
//!   singly-linked chain.
//! - [`ExecDescriptor`](crate::state::ExecDescriptor) — `catalog_id` + routing +
//!   the terminal flag, seeded at `playbook_started`, read by the stateless
//!   dispatch.
//!
//! Both carry a **single-replica locality assumption**: they live on whichever
//! replica handled the execution's first event.  With one server replica that is
//! always the same process.  With 2+ replicas behind a load balancer a later
//! trigger (a worker's `command.completed` POST, the reconcile poller) can land
//! on a *different* replica, which finds a **cold** slot and:
//!
//! - for the watermark: stamps the event as a chain root (`prev_event_id = NULL`)
//!   even though it continues an existing chain → a forked chain the off-server
//!   builder's walk can't reassemble; and
//! - for the descriptor: falls back to the server-built path that **reads
//!   `noetl.event`** to rebuild `WorkflowState` — correct, but neither scan-free
//!   nor coherent.
//!
//! ## The mechanism
//!
//! Under `NOETL_REPLICA_COHERENCE=nats_kv` both maps are backed by two JetStream
//! **KV buckets** (`noetl_chain_heads`, `noetl_exec_descriptors`) so any replica
//! resolves the same value:
//!
//! - The **head advance** is a **compare-and-swap** (`Store::update` against the
//!   read revision): two replicas emitting concurrently for one execution
//!   serialise through the CAS, so a single per-execution chain is preserved.
//! - The **descriptor** is a CAS read-modify-write so a `seed` (catalog_id +
//!   routing) and a `mark_terminal` from different replicas **merge** rather than
//!   clobber.
//!
//! The in-process maps stay as a **write-through cache** and a **degraded-mode
//! fallback**: when KV is unreachable (or `replica_coherence=local`, the
//! default), the methods behave exactly as the original in-memory implementation
//! — `local` is bit-identical to today, so single-replica + prod are unchanged.
//!
//! Each KV access is labelled on `noetl_replica_coherence_total{structure, op,
//! outcome}` (see [`crate::metrics::record_replica_coherence`]); the load-bearing
//! proof series is `outcome="kv_remote_hit"` — a descriptor/head that missed the
//! local map but hit KV, i.e. a cross-replica resolve that avoided a cold
//! server-built fallback.

use std::sync::Arc;

use async_nats::jetstream::{
    self,
    kv::{Operation, Store},
};
use bytes::Bytes;

use crate::config::ReplicaCoherence;
use crate::state::ExecDescriptor;

/// KV bucket holding `execution_id → chain-head event_id`.
const HEADS_BUCKET: &str = "noetl_chain_heads";
/// KV bucket holding `execution_id → ExecDescriptor` (JSON).
const DESC_BUCKET: &str = "noetl_exec_descriptors";
/// Bounded CAS retries before giving up and degrading to the in-process map.
const CAS_RETRIES: usize = 6;

/// Shared coherence backend injected into [`ChainHeads`](crate::state::ChainHeads)
/// and [`ExecDescriptors`](crate::state::ExecDescriptors).  Built once in
/// [`AppState::new`](crate::state::AppState::new); the KV buckets are created
/// lazily on first use (the same lazy shape as the event-stream publisher) so the
/// sync constructor stays sync and a NATS hiccup at startup doesn't wedge boot.
pub struct CoherenceKv {
    nats: Option<Arc<async_nats::Client>>,
    mode: ReplicaCoherence,
    /// Lazily-built KV handles.  `Mutex<Option<Store>>` (not a `OnceCell`) so a
    /// transient build failure is retried on the next access rather than cached
    /// as permanently-disabled.
    heads: tokio::sync::Mutex<Option<Store>>,
    desc: tokio::sync::Mutex<Option<Store>>,
}

impl Default for CoherenceKv {
    /// A disabled (local-only) backend — every method returns "not handled by
    /// KV", so the wrapping structs use their in-process map.  Used by
    /// `ChainHeads::default()` / `ExecDescriptors::default()` (tests + the
    /// single-pool legacy path).
    fn default() -> Self {
        Self {
            nats: None,
            mode: ReplicaCoherence::Local,
            heads: tokio::sync::Mutex::new(None),
            desc: tokio::sync::Mutex::new(None),
        }
    }
}

/// Outcome of a KV read that may distinguish a value present from a definitive
/// absence (cold) vs. KV being unavailable (degrade to local).
pub enum KvRead<T> {
    /// KV authoritatively returned this value.
    Hit(T),
    /// KV authoritatively has no entry (genuinely cold / evicted everywhere).
    Miss,
    /// KV is disabled / unreachable / errored → caller falls back to the
    /// in-process map (degraded mode == `local`).
    Unavailable,
}

impl CoherenceKv {
    /// Build the backend.  `nats` is the (optional) connected client; `mode`
    /// selects `local` (default) vs. `nats_kv`.  No I/O here — buckets are built
    /// on first access.
    pub fn new(nats: Option<Arc<async_nats::Client>>, mode: ReplicaCoherence) -> Self {
        Self {
            nats,
            mode,
            heads: tokio::sync::Mutex::new(None),
            desc: tokio::sync::Mutex::new(None),
        }
    }

    /// Whether KV backing is on AND a NATS client is present.  When false the
    /// wrapping structs skip KV entirely and behave as the original in-memory
    /// maps (the default, prod-unchanged path).
    pub fn enabled(&self) -> bool {
        matches!(self.mode, ReplicaCoherence::NatsKv) && self.nats.is_some()
    }

    /// Lazily build + cache a KV store handle for `bucket`.  Returns a clone of
    /// the [`Store`] (cheap — it's a handle) or `None` if unbuildable.
    async fn store(&self, bucket: &str, slot: &tokio::sync::Mutex<Option<Store>>) -> Option<Store> {
        if !self.enabled() {
            return None;
        }
        let mut guard = slot.lock().await;
        if let Some(s) = guard.as_ref() {
            return Some(s.clone());
        }
        let client = self.nats.as_ref()?;
        let js = jetstream::new((**client).clone());
        // Create is idempotent enough for our use — on "already exists" we fetch
        // the existing bucket.  Small values, history=1 (we only need latest),
        // file storage so the watermark survives a full cluster restart.
        let cfg = jetstream::kv::Config {
            bucket: bucket.to_string(),
            description: "NoETL multi-replica drive coherence (RFC #115)".to_string(),
            history: 1,
            ..Default::default()
        };
        let built = match js.create_key_value(cfg).await {
            Ok(s) => Some(s),
            Err(create_err) => match js.get_key_value(bucket).await {
                Ok(s) => Some(s),
                Err(get_err) => {
                    tracing::warn!(
                        bucket,
                        create_error = %create_err,
                        get_error = %get_err,
                        "replica coherence: KV bucket unbuildable; degrading to in-process map"
                    );
                    None
                }
            },
        };
        *guard = built.clone();
        built
    }

    async fn heads_store(&self) -> Option<Store> {
        self.store(HEADS_BUCKET, &self.heads).await
    }

    async fn desc_store(&self) -> Option<Store> {
        self.store(DESC_BUCKET, &self.desc).await
    }

    // ── chain head ──────────────────────────────────────────────────────────

    /// CAS-advance the chain head for `execution_id` to `new_head`; return the
    /// head **before** this advance (the `prev_event_id` for the first event of
    /// the batch).  `Hit(prev)` = KV is now authoritative; `Unavailable` = caller
    /// uses the in-process map.  `Miss` is never returned by an advance.
    pub async fn advance_head(&self, execution_id: i64, new_head: i64) -> KvRead<Option<i64>> {
        let Some(store) = self.heads_store().await else {
            return KvRead::Unavailable;
        };
        let key = execution_id.to_string();
        let val = Bytes::from(new_head.to_string());
        for _ in 0..CAS_RETRIES {
            let (cur, rev) = match store.entry(&key).await {
                Ok(Some(e)) if e.operation == Operation::Put => {
                    (parse_i64(&e.value), e.revision)
                }
                // Tombstone (Delete/Purge) — `update` at its revision revives it.
                Ok(Some(e)) => (None, e.revision),
                Ok(None) => (None, 0),
                Err(_) => return KvRead::Unavailable,
            };
            // `update(key, val, 0)` is exactly "create if absent" in async-nats,
            // so one path covers absent / live / tombstone.
            match store.update(&key, val.clone(), rev).await {
                Ok(_) => return KvRead::Hit(cur),
                Err(_) => {
                    crate::metrics::record_replica_coherence("chain_head", "advance", "cas_retry");
                    continue;
                }
            }
        }
        crate::metrics::record_replica_coherence("chain_head", "advance", "cas_exhausted");
        KvRead::Unavailable
    }

    /// Read the coherent chain head for `execution_id`.
    pub async fn get_head(&self, execution_id: i64) -> KvRead<i64> {
        let Some(store) = self.heads_store().await else {
            return KvRead::Unavailable;
        };
        match store.get(execution_id.to_string()).await {
            Ok(Some(b)) => match parse_i64(&b) {
                Some(v) => KvRead::Hit(v),
                None => KvRead::Miss,
            },
            Ok(None) => KvRead::Miss,
            Err(_) => KvRead::Unavailable,
        }
    }

    /// Delete the head entry (terminal eviction).  Best-effort.
    pub async fn evict_head(&self, execution_id: i64) {
        if let Some(store) = self.heads_store().await {
            let _ = store.delete(execution_id.to_string()).await;
        }
    }

    // ── descriptor ──────────────────────────────────────────────────────────

    /// Read the coherent descriptor for `execution_id`.
    pub async fn get_descriptor(&self, execution_id: i64) -> KvRead<ExecDescriptor> {
        let Some(store) = self.desc_store().await else {
            return KvRead::Unavailable;
        };
        match store.get(execution_id.to_string()).await {
            Ok(Some(b)) => match serde_json::from_slice::<ExecDescriptor>(&b) {
                Ok(d) => KvRead::Hit(d),
                Err(_) => KvRead::Miss,
            },
            Ok(None) => KvRead::Miss,
            Err(_) => KvRead::Unavailable,
        }
    }

    /// Merge `catalog_id` + `routing_meta` into the KV descriptor without
    /// clobbering a concurrently-stamped `terminal` flag (CAS read-modify-write).
    pub async fn seed_descriptor(
        &self,
        execution_id: i64,
        catalog_id: i64,
        routing_meta: Option<serde_json::Value>,
    ) {
        self.modify_descriptor(execution_id, |d| {
            if catalog_id != 0 {
                d.catalog_id = catalog_id;
            }
            if routing_meta.is_some() {
                d.routing_meta = routing_meta.clone();
            }
        })
        .await;
    }

    /// CAS-stamp the terminal flag on the KV descriptor (preserving catalog_id +
    /// routing).
    pub async fn mark_terminal_descriptor(&self, execution_id: i64) {
        self.modify_descriptor(execution_id, |d| d.terminal = true).await;
    }

    /// Delete the descriptor entry (terminal eviction).  Best-effort.
    pub async fn evict_descriptor(&self, execution_id: i64) {
        if let Some(store) = self.desc_store().await {
            let _ = store.delete(execution_id.to_string()).await;
        }
    }

    /// CAS read-modify-write of the KV descriptor.  Reads the current value (or
    /// `default` when absent/tombstone), applies `f`, writes back at the read
    /// revision; retries on contention.  Best-effort — a failure leaves the
    /// in-process map (the wrapper already wrote) as the degraded-mode truth.
    async fn modify_descriptor<F>(&self, execution_id: i64, mut f: F)
    where
        F: FnMut(&mut ExecDescriptor),
    {
        let Some(store) = self.desc_store().await else {
            return;
        };
        let key = execution_id.to_string();
        for _ in 0..CAS_RETRIES {
            let (mut desc, rev) = match store.entry(&key).await {
                Ok(Some(e)) if e.operation == Operation::Put => (
                    serde_json::from_slice::<ExecDescriptor>(&e.value).unwrap_or_default(),
                    e.revision,
                ),
                Ok(Some(e)) => (ExecDescriptor::default(), e.revision),
                Ok(None) => (ExecDescriptor::default(), 0),
                Err(_) => return,
            };
            f(&mut desc);
            let Ok(bytes) = serde_json::to_vec(&desc) else {
                return;
            };
            match store.update(&key, Bytes::from(bytes), rev).await {
                Ok(_) => return,
                Err(_) => {
                    crate::metrics::record_replica_coherence("descriptor", "modify", "cas_retry");
                    continue;
                }
            }
        }
        crate::metrics::record_replica_coherence("descriptor", "modify", "cas_exhausted");
    }
}

/// Parse a decimal `i64` from a KV value (the head is stored as its decimal
/// string for human-readable bucket dumps).
fn parse_i64(b: &[u8]) -> Option<i64> {
    std::str::from_utf8(b).ok()?.trim().parse::<i64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_i64_roundtrip() {
        assert_eq!(parse_i64(b"42"), Some(42));
        assert_eq!(parse_i64(b"-1"), Some(-1));
        assert_eq!(parse_i64(b"  7 "), Some(7));
        assert_eq!(parse_i64(b"not-a-number"), None);
        assert_eq!(parse_i64(b""), None);
    }

    #[tokio::test]
    async fn default_backend_is_disabled() {
        let c = CoherenceKv::default();
        assert!(!c.enabled());
        // Every op degrades cleanly with no NATS.
        assert!(matches!(c.advance_head(1, 10).await, KvRead::Unavailable));
        assert!(matches!(c.get_head(1).await, KvRead::Unavailable));
        assert!(matches!(c.get_descriptor(1).await, KvRead::Unavailable));
        // Writes/evicts are no-ops, not panics.
        c.seed_descriptor(1, 5, None).await;
        c.mark_terminal_descriptor(1).await;
        c.evict_head(1).await;
        c.evict_descriptor(1).await;
    }

    #[tokio::test]
    async fn local_mode_with_client_absent_is_disabled() {
        // nats_kv requested but no client → still disabled (degrades to local).
        let c = CoherenceKv::new(None, ReplicaCoherence::NatsKv);
        assert!(!c.enabled());
    }
}
