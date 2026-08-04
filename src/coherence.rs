//! **Cross-replica coherence — local-only since the NATS removal.**
//!
//! RFC noetl/ai-meta#115 program-scale step (noetl/ai-meta#107).
//!
//! ## The problem this exists for
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
//! replica handled the execution's first event. With 2+ replicas behind a load
//! balancer a later trigger can land elsewhere and miss them.
//!
//! ## Why this is a shell now
//!
//! The shared backing was two NATS JetStream KV buckets under
//! `NOETL_REPLICA_COHERENCE=nats_kv`. **That backing went with NATS**
//! (noetl/ai-meta#212). The mode was never enabled here —
//! `NOETL_REPLICA_COHERENCE` is unset (→ `Local`) and the control plane runs a
//! **single replica** — so removing it changes no observed behaviour.
//!
//! **The API is deliberately kept.** [`crate::state`] branches on
//! [`KvRead::Hit`] / [`KvRead::Miss`] / [`KvRead::Unavailable`] on live paths,
//! and those call sites encode the "is this genuinely cold, or is the shared
//! store merely unreachable?" distinction that any future multi-replica backing
//! needs again. Collapsing them into `Option` now would delete a real
//! distinction to save a few lines, and re-deriving it later is exactly the sort
//! of subtlety that gets it wrong the second time.
//!
//! So every read returns [`KvRead::Unavailable`] — "no shared store; use the
//! in-process map" — which is what `Local` mode always did. When a shared
//! backing is wanted again the natural home is an EHDB L0 KV dataset over the
//! writer's KV face (`ehdb-feed::serve_kv`), the same store the gateway moved
//! its buckets onto (noetl/ai-meta#214).

use crate::state::ExecDescriptor;

/// Outcome of a coherence read that distinguishes a value present from a
/// definitive absence (cold) vs. the shared store being unavailable.
pub enum KvRead<T> {
    /// The shared store authoritatively returned this value.
    Hit(T),
    /// The shared store authoritatively has no entry (genuinely cold).
    Miss,
    /// No shared store is configured / it is unreachable → the caller falls back
    /// to the in-process map. **The only variant produced today.**
    Unavailable,
}

/// The cross-replica coherence backend.
///
/// A shell since the NATS removal: it holds no connection and every read is
/// [`KvRead::Unavailable`], so callers use their in-process maps. Retained as a
/// seam rather than deleted — see the module docs.
#[derive(Default)]
pub struct CoherenceKv;

impl CoherenceKv {
    /// Build the backend. No I/O and no configuration: there is no shared store.
    pub fn new() -> Self {
        Self
    }

    /// Whether a shared backing is active. Always `false` — callers keep using
    /// their in-process maps, which is what `Local` mode always did.
    pub fn enabled(&self) -> bool {
        false
    }

    // ── chain head ──────────────────────────────────────────────────────────

    /// Advance the shared head. No-op without a shared store.
    pub async fn advance_head(&self, _execution_id: i64, _new_head: i64) -> KvRead<Option<i64>> {
        KvRead::Unavailable
    }

    /// Read the shared head.
    pub async fn get_head(&self, _execution_id: i64) -> KvRead<i64> {
        KvRead::Unavailable
    }

    /// Evict the shared head (terminal eviction). No-op.
    pub async fn evict_head(&self, _execution_id: i64) {}

    // ── descriptor ──────────────────────────────────────────────────────────

    /// Read the shared descriptor.
    pub async fn get_descriptor(&self, _execution_id: i64) -> KvRead<ExecDescriptor> {
        KvRead::Unavailable
    }

    /// Seed the shared descriptor. No-op.
    pub async fn seed_descriptor(
        &self,
        _execution_id: i64,
        _catalog_id: i64,
        _routing_meta: Option<serde_json::Value>,
    ) {
    }

    /// Mark the shared descriptor terminal. No-op.
    pub async fn mark_terminal_descriptor(&self, _execution_id: i64) {}

    /// Evict the shared descriptor. No-op.
    pub async fn evict_descriptor(&self, _execution_id: i64) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every read must report `Unavailable`, **not** `Miss`. The difference is
    /// load-bearing at the call sites: `Miss` asserts the shared store
    /// authoritatively has no entry, which would let a caller treat a warm
    /// execution as definitively cold. With no shared store the honest answer is
    /// "cannot say — use your local map".
    #[tokio::test]
    async fn every_read_is_unavailable_not_miss() {
        let c = CoherenceKv::new();
        assert!(!c.enabled());
        assert!(matches!(c.get_head(1).await, KvRead::Unavailable));
        assert!(matches!(c.advance_head(1, 2).await, KvRead::Unavailable));
        assert!(matches!(c.get_descriptor(1).await, KvRead::Unavailable));
    }

    /// The mutating no-ops are called on live paths, so they must not panic.
    #[tokio::test]
    async fn mutations_are_harmless_no_ops() {
        let c = CoherenceKv::new();
        c.evict_head(1).await;
        c.seed_descriptor(1, 2, None).await;
        c.mark_terminal_descriptor(1).await;
        c.evict_descriptor(1).await;
    }
}
