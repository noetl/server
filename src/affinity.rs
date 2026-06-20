//! Execution-affinity routing — single-owner write ordering for the off-server
//! drive (RFC noetl/ai-meta#116, the multi-replica half of #115 program-scale /
//! #107 step 3).
//!
//! ## The problem affinity closes
//!
//! The multi-replica coherence DATA layer ([`crate::coherence`],
//! `NOETL_REPLICA_COHERENCE=nats_kv`) makes any replica resolve the **same**
//! chain-head watermark + execution descriptor from shared JetStream KV. That is
//! *necessary but not sufficient* for multi-replica off-server execution. The
//! `command.issued` predecessor is read from the chain head in
//! [`crate::handlers::execute`] and the head is CAS-advanced in
//! [`crate::handlers::event_write::emit_events`] as **two non-atomic steps**;
//! across two replicas a lifecycle emit for the same execution can advance the
//! head between the read and the advance, so a `command.issued` carries a stale
//! prev while the chain threads through a newer event → a forked chain the
//! off-server WAL builder cannot reassemble (it perpetually reports "worker WAL
//! incomplete" and the execution sticks RUNNING). The per-execution drive
//! serialisation (`OrchStateCache::orchestrate_in_flight`) is *also* per-replica,
//! so two replicas can drive the same execution concurrently — a second source of
//! the same fork, plus a double-issue of the next commands.
//!
//! ## The mechanism — one replica owns an execution
//!
//! Affinity makes the read-then-advance atomic **by construction**: every trigger
//! for an execution (`POST /api/events`, which carries both the worker-lifecycle
//! events AND synchronously fires the drive) is routed to the single replica that
//! [`crate::sharding::ShardConfig::owns`] it — `shard_for(execution_id)` over the
//! stable XxHash64 already used for the sharded data layout. On the owner:
//!
//! - only one process ever drives + emits for the execution, so the existing
//!   per-execution `orchestrate_in_flight` lock + the single-process in-memory
//!   [`ChainHeads`](crate::state::ChainHeads) serialise the read→advance with no
//!   distributed lock on the hot path; and
//! - the double-drive disappears too — only the owner's reconcile poller and only
//!   the owner's ingest path drive the execution.
//!
//! A non-owning replica that receives a trigger **forwards** it (a transparent
//! server-side reverse-proxy POST) to the owner and returns the owner's response.
//! KV coherence ([`crate::coherence`]) composes with affinity rather than being
//! replaced: it covers the *genesis-on-a-different-replica* case (an execution
//! whose `playbook_started` landed on a non-owner before any forwarding) and
//! ownership *handoff* (a replica restart / shard-map change) — the new owner
//! reads the coherent head/descriptor from KV instead of a cold local slot.
//!
//! ## Why forwarding, not a distributed drive lock
//!
//! Option (ii) from the #116 design space — a NATS-KV lease per drive plus a
//! CAS-time issuing-event derivation — adds lease-acquire latency to *every*
//! drive and carries lease-expiry edge cases (a slow owner whose lease lapses
//! mid-drive lets a second replica in). Affinity solves the chain fork AND the
//! double-drive with one mechanism (one owner for the whole execution) and reuses
//! the `src/sharding.rs` substrate verbatim — no new hash, no new lock on the hot
//! path.
//!
//! ## Default-safe
//!
//! - `NOETL_EXECUTION_AFFINITY` defaults **off** → no forwarding, prod unchanged.
//! - With one replica `ShardConfig::owns` is always `true` (shard_count<=1), so
//!   even with the flag on a single replica forwards nothing — bit-for-bit the
//!   current behavior.
//! - Forwarding requires a peer-URL template; absent it, affinity is inert.
//! - A forward that fails (owner unreachable) **degrades to local processing** —
//!   the worst case is the pre-affinity behavior (which under `nats_kv` is still
//!   coherent), never a dropped event.

use std::sync::Arc;
use std::time::Duration;

use axum::http::HeaderMap;

use crate::sharding::ShardConfig;

/// Header marking a `/api/events` POST that this replica already forwarded once.
/// The owner that receives it processes locally regardless of its own `owns()`
/// verdict — a single forwarding hop, never a loop.
pub const AFFINITY_FORWARDED_HEADER: &str = "x-noetl-affinity-forwarded";

/// Placeholder in [`AppConfig::peer_url_template`](crate::config::AppConfig)
/// replaced by the owner shard index when building the forward target.
const SHARD_PLACEHOLDER: &str = "{shard}";

/// Execution-affinity router. Built once in
/// [`AppState::new`](crate::state::AppState::new); cloned cheaply (everything is
/// behind an `Arc` / `Copy`).
pub struct ExecutionAffinity {
    /// `NOETL_EXECUTION_AFFINITY` — master switch. Off (default) → inert.
    enabled: bool,
    /// `NOETL_PEER_URL_TEMPLATE`, e.g.
    /// `http://noetl-server-rust-{shard}.noetl-server-rust-headless:8082`. The
    /// `{shard}` token is replaced by the owner's shard index. `None` → inert.
    peer_url_template: Option<String>,
    /// The cluster shard map (same `Arc` `AppState` holds).
    shard: Arc<ShardConfig>,
    /// Shared HTTP client for the reverse-proxy hop. Short timeout — the owner is
    /// an in-cluster peer; a slow forward should degrade to local, not hang the
    /// worker's POST.
    client: reqwest::Client,
}

impl ExecutionAffinity {
    /// Construct the router from config + the shared shard map.
    pub fn new(
        enabled: bool,
        peer_url_template: Option<String>,
        shard: Arc<ShardConfig>,
    ) -> Self {
        // 4s connect+request budget: forwarding to an in-cluster pod is sub-ms in
        // the happy path; a longer hang means the owner is unhealthy and we'd
        // rather degrade to local than block the worker.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(4))
            .build()
            .unwrap_or_default();
        Self {
            enabled,
            peer_url_template: peer_url_template.filter(|t| !t.trim().is_empty()),
            shard,
            client,
        }
    }

    /// Is affinity routing actually in force? Requires the flag on, more than one
    /// shard (single replica is a no-op), and a peer-URL template. When false
    /// every helper short-circuits and the caller behaves exactly as today.
    pub fn active(&self) -> bool {
        self.enabled && self.shard.shard_count > 1 && self.peer_url_template.is_some()
    }

    /// Does this replica own `execution_id`'s drive + chain writes?
    pub fn owns(&self, execution_id: i64) -> bool {
        self.shard.owns(execution_id)
    }

    /// Build the owner replica's base URL for `execution_id` from the template,
    /// substituting `{shard}` with `shard_for(execution_id)`. `None` if no
    /// template is configured.
    pub fn owner_base_url(&self, execution_id: i64) -> Option<String> {
        let template = self.peer_url_template.as_ref()?;
        let owner = crate::sharding::shard_for(execution_id, self.shard.shard_count);
        Some(template.replace(SHARD_PLACEHOLDER, &owner.to_string()))
    }

    /// Decide + perform forwarding for an incoming `/api/events` POST.
    ///
    /// Returns:
    /// - `Forwarded(resp)` — the owner handled it; `resp` is the owner's response
    ///   to return to the caller verbatim.
    /// - `ProcessLocally` — this replica is the owner (or affinity is inert, the
    ///   request was already forwarded once, the id is unparseable, or the
    ///   forward failed and we degrade) → run the normal handler body.
    pub async fn route_event(
        &self,
        headers: &HeaderMap,
        request: &crate::handlers::events::EventRequest,
    ) -> AffinityRoute {
        if !self.active() {
            return AffinityRoute::ProcessLocally;
        }
        // Loop guard: a request we already forwarded lands on the owner with this
        // header. Process it here even if owns() somehow disagrees (shard-map
        // skew) — one hop, never a loop.
        if headers.contains_key(AFFINITY_FORWARDED_HEADER) {
            crate::metrics::record_execution_affinity("forwarded_terminus");
            return AffinityRoute::ProcessLocally;
        }
        let Ok(execution_id) = request.execution_id.parse::<i64>() else {
            // Malformed id — let the normal handler reject it with its own error.
            return AffinityRoute::ProcessLocally;
        };
        if self.owns(execution_id) {
            crate::metrics::record_execution_affinity("owned_local");
            return AffinityRoute::ProcessLocally;
        }
        let Some(base) = self.owner_base_url(execution_id) else {
            return AffinityRoute::ProcessLocally;
        };
        let url = format!("{}/api/events", base.trim_end_matches('/'));
        match self
            .client
            .post(&url)
            .header(AFFINITY_FORWARDED_HEADER, "1")
            .json(request)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<crate::handlers::events::EventResponse>().await {
                    Ok(body) => {
                        crate::metrics::record_execution_affinity("forwarded_ok");
                        AffinityRoute::Forwarded(body)
                    }
                    Err(e) => {
                        tracing::warn!(
                            execution_id,
                            owner = %url,
                            error = %e,
                            "execution-affinity: owner response undecodable; degrading to local processing"
                        );
                        crate::metrics::record_execution_affinity("forward_decode_err");
                        AffinityRoute::ProcessLocally
                    }
                }
            }
            Ok(resp) => {
                tracing::warn!(
                    execution_id,
                    owner = %url,
                    status = %resp.status(),
                    "execution-affinity: owner returned non-success; degrading to local processing"
                );
                crate::metrics::record_execution_affinity("forward_http_err");
                AffinityRoute::ProcessLocally
            }
            Err(e) => {
                tracing::warn!(
                    execution_id,
                    owner = %url,
                    error = %e,
                    "execution-affinity: owner unreachable; degrading to local processing"
                );
                crate::metrics::record_execution_affinity("forward_unavailable");
                AffinityRoute::ProcessLocally
            }
        }
    }
}

/// Outcome of [`ExecutionAffinity::route_event`].
pub enum AffinityRoute {
    /// The owner replica handled the event; return this response verbatim.
    Forwarded(crate::handlers::events::EventResponse),
    /// Run the normal handler body on this replica.
    ProcessLocally,
}

/// Parse a StatefulSet pod ordinal from a hostname (`name-N` → `N`).
///
/// `NOETL_SHARD_INDEX_FROM_HOSTNAME=true` lets a single StatefulSet manifest give
/// each pod a distinct shard index from identical env — the pod's stable ordinal
/// hostname is the shard index. Returns `None` when the hostname has no trailing
/// `-<digits>` segment (so a plain Deployment pod falls back to the explicit
/// `NOETL_SHARD_INDEX` / single-shard default).
pub fn shard_index_from_hostname(hostname: &str) -> Option<u32> {
    let (_, ordinal) = hostname.rsplit_once('-')?;
    ordinal.parse::<u32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn affinity(enabled: bool, template: Option<&str>, index: u32, count: u32) -> ExecutionAffinity {
        ExecutionAffinity::new(
            enabled,
            template.map(|s| s.to_string()),
            Arc::new(ShardConfig::new(index, count).unwrap()),
        )
    }

    #[test]
    fn inert_when_disabled() {
        let a = affinity(false, Some("http://peer-{shard}:8082"), 0, 2);
        assert!(!a.active());
    }

    #[test]
    fn inert_on_single_shard() {
        // Flag on, but one shard → owns() is always true, nothing to forward.
        let a = affinity(true, Some("http://peer-{shard}:8082"), 0, 1);
        assert!(!a.active());
        assert!(a.owns(12345));
    }

    #[test]
    fn inert_without_template() {
        let a = affinity(true, None, 0, 2);
        assert!(!a.active());
        let a = affinity(true, Some("   "), 0, 2);
        assert!(!a.active(), "blank template is treated as unset");
    }

    #[test]
    fn active_when_configured() {
        let a = affinity(true, Some("http://peer-{shard}:8082"), 0, 2);
        assert!(a.active());
    }

    #[test]
    fn owner_url_substitutes_shard() {
        let a = affinity(true, Some("http://noetl-server-rust-{shard}.hl:8082"), 0, 4);
        for eid in [1_i64, 42, 320_816_801_799_737_344, i64::MAX, -7] {
            let owner = crate::sharding::shard_for(eid, 4);
            let url = a.owner_base_url(eid).unwrap();
            assert_eq!(url, format!("http://noetl-server-rust-{owner}.hl:8082"));
        }
    }

    #[test]
    fn owns_partitions_with_shard_for() {
        // Replica index 1 of 3 owns exactly the executions shard_for maps to 1.
        let a = affinity(true, Some("http://peer-{shard}:8082"), 1, 3);
        for eid in 0..500_i64 {
            let owns = a.owns(eid);
            assert_eq!(owns, crate::sharding::shard_for(eid, 3) == 1);
        }
    }

    #[test]
    fn hostname_ordinal_parsing() {
        assert_eq!(shard_index_from_hostname("noetl-server-rust-0"), Some(0));
        assert_eq!(shard_index_from_hostname("noetl-server-rust-1"), Some(1));
        assert_eq!(shard_index_from_hostname("noetl-server-rust-13"), Some(13));
        // No trailing ordinal (Deployment-style random suffix) → None.
        assert_eq!(shard_index_from_hostname("noetl-server-rust-6cdb8b7b6"), None);
        assert_eq!(shard_index_from_hostname("plainhost"), None);
        assert_eq!(shard_index_from_hostname(""), None);
    }
}
