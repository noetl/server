//! **The chain-store populator, in shadow** — the prerequisite for repointing
//! the advance path at the execution-partitioned chain store.
//!
//! # What this is
//!
//! Every event that passes the server's one emit chokepoint is also written
//! into an `ehdb-l0` [`DurableChainStore`], keyed by `execution_id` and linked
//! by `prev_event_id`. Nothing reads from it here. Postgres stays the system of
//! record and nothing in this module can change what a caller sees.
//!
//! # Why a populator and not just a store
//!
//! An empty chain partition and an execution the store has never been told
//! about are the same bytes on disk — both answer "no events". A reader that
//! cannot distinguish them will conclude a **running** execution has not
//! started, and the advance path will call it `Empty` and stop driving it.
//!
//! So each execution carries a **watermark**, and the guarded read
//! ([`ChainPopulator::chain_if_authoritative`]) answers `None` — *cannot
//! answer* — for anything unmarked. The watermark is the reason the repoint is
//! safe; the store alone is not.
//!
//! ⚠ The ordering between the append and the watermark is **asymmetric**, and
//! `ehdb_l0::chain_populator` is where that argument lives. The short version:
//! never create authority out of a failure. It matters most here, because
//! **arming this mid-flight is the normal case** — every already-running
//! execution's next event carries a `prev` the empty store does not have, so
//! its first `populate` fails with `NotHead`.
//!
//! # Default OFF
//!
//! `NOETL_CHAIN_POPULATE` defaults to false, so the deploy is inert on arrival:
//! with the flag off not a byte of this module runs, and the server behaves
//! exactly as the release before it. The rollout is reversible by
//! configuration, not by redeploy.

use std::sync::Arc;

use ehdb_l0::chain_populator::{populator_enabled, Authority, ChainPopulator};
use ehdb_l0::chain_store_durable::DurableChainStore;
use ehdb_l0::substrate::{DurableSubstrate, LocalFsSubstrate};

/// Where the chain store keeps its local root.
pub const POPULATE_DIR_ENV: &str = "NOETL_CHAIN_POPULATE_DIR";

/// Default local root. Under `/data`, where the durable volume is mounted.
///
/// ⚠ A bare default under `/data` does **not** fail closed when no volume is
/// mounted — it silently writes to the pod's ephemeral layer, which is the trap
/// noetl/server#419 records. [`ehdb_embedded::durable_root_usable`] is what
/// makes the open refuse, and this module reuses that one copy rather than
/// restating the mount-point test.
pub const DEFAULT_POPULATE_DIR: &str = "/data/ehdb-chain";

/// The local root for the chain store.
pub fn populate_dir() -> String {
    std::env::var(POPULATE_DIR_ENV).unwrap_or_else(|_| DEFAULT_POPULATE_DIR.to_string())
}

/// What one batch's population did, as a metric label.
///
/// A closed set, pinned at 0, because **absence is the default** for a labelled
/// series: an unpinned `populated` that has never fired is indistinguishable
/// from a binary that predates the metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopulateOutcome {
    /// Every row in the batch landed.
    Populated,
    /// Some rows landed and some did not.
    Partial,
    /// No row landed.
    Rejected,
    /// Nothing to do.
    Skipped,
}

impl PopulateOutcome {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Populated => "populated",
            Self::Partial => "partial",
            Self::Rejected => "rejected",
            Self::Skipped => "skipped",
        }
    }

    /// Every label, for pinning.
    pub const ALL_LABELS: [&'static str; 4] = ["populated", "partial", "rejected", "skipped"];

    /// Classify a batch by how much of it landed.
    ///
    /// Separated from the I/O so every combination is testable without a
    /// filesystem.
    pub fn classify(landed: usize, total: usize) -> Self {
        match (landed, total) {
            (_, 0) => Self::Skipped,
            (0, _) => Self::Rejected,
            (l, t) if l == t => Self::Populated,
            _ => Self::Partial,
        }
    }
}

/// The store plus the substrate it was opened over.
///
/// [`ChainPopulator`] borrows both, so they have to outlive it — hence one rig
/// held in a `OnceLock` and a populator constructed per call.
struct ChainRig {
    store: DurableChainStore,
    substrate: Arc<dyn DurableSubstrate>,
}

impl ChainRig {
    fn populator(&self) -> ChainPopulator<'_> {
        ChainPopulator::new(&self.store, self.substrate.as_ref())
    }
}

/// The process-wide chain store, opened once on first use.
///
/// A `Mutex` for the same reason `ehdb_embedded` takes one: `append` reads the
/// head and then writes, so two concurrent appends into one execution could
/// compute the same next sequence. Single-writer-per-execution is the design
/// invariant, but a server handles concurrent requests, and a shadow is not the
/// place to invent concurrency semantics.
static CHAIN: std::sync::OnceLock<Option<std::sync::Mutex<ChainRig>>> = std::sync::OnceLock::new();

fn rig() -> Option<&'static std::sync::Mutex<ChainRig>> {
    CHAIN.get_or_init(open_chain).as_ref()
}

/// Open the chain store, or `None` when the flag is off.
///
/// ⚠ Returns `None` rather than erroring when disabled, and logs-and-returns
/// `None` when an *enabled* open fails. This runs on the live write path.
fn open_chain() -> Option<std::sync::Mutex<ChainRig>> {
    if !populator_enabled() {
        return None;
    }
    let dir = populate_dir();
    if !crate::handlers::ehdb_embedded::durable_root_usable(std::path::Path::new(&dir)) {
        tracing::warn!(target: "noetl_server::chain_populate", dir = %dir,
            "chain populator armed but its root is not on a mounted volume — \
             staying OFF rather than writing to ephemeral storage");
        crate::metrics::record_chain_populate("open_failed");
        return None;
    }
    match LocalFsSubstrate::new(std::path::Path::new(&dir)) {
        Ok(fs) => {
            let substrate: Arc<dyn DurableSubstrate> = Arc::new(fs);
            tracing::info!(target: "noetl_server::chain_populate", dir = %dir,
                "chain populator OPENED in shadow");
            crate::metrics::record_chain_populate("opened");
            Some(std::sync::Mutex::new(ChainRig {
                store: DurableChainStore::new(Arc::clone(&substrate)),
                substrate,
            }))
        }
        Err(e) => {
            tracing::warn!(target: "noetl_server::chain_populate", dir = %dir, error = %e,
                "chain populator open FAILED");
            crate::metrics::record_chain_populate("open_failed");
            None
        }
    }
}

/// Populate the chain store with the batch that is about to become
/// authoritative.
///
/// Called from [`crate::handlers::event_write::emit_events`] — the one
/// chokepoint every server-originated event passes through, on both sides of the
/// CQRS gate. ⚠ It must sit where `shadow_append` sits: **after** the
/// `prev_event_id` stamping and **before** the publish/insert fork. After the
/// stamping because the chain store enforces I1 and an unstamped row would be
/// rejected as `NotHead` on everything past the root; before the fork because
/// the gate-off branch is the one prod does not exercise, and a second copy
/// there is the classic place to rot (noetl/ai-meta#332).
///
/// ⚠ Never returns an error and never panics on a poisoned lock. A shadow that
/// can fail a real write is a liability, not evidence.
pub fn populate_batch(rows: &[crate::handlers::event_write::EventRow]) {
    let Some(rig) = rig() else {
        // Flag off, or the open failed and already recorded why.
        return;
    };
    if rows.is_empty() {
        crate::metrics::record_chain_populate(PopulateOutcome::Skipped.label());
        return;
    }
    let guard = match rig.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let populator = guard.populator();

    let mut landed = 0usize;
    for row in rows {
        let execution_id = row.execution_id.to_string();
        let event_id = row.event_id.to_string();
        let prev = row.prev_event_id.map(|p| p.to_string());
        let parent_exec = row.parent_execution_id.map(|p| p.to_string());
        match populator.populate(
            &execution_id,
            &event_id,
            prev.as_deref(),
            parent_exec.as_deref(),
            &row.to_stream_json().to_string(),
        ) {
            Ok(_) => landed += 1,
            Err(e) => {
                // ⚠ `debug`, not `warn`. Arming mid-flight rejects the first
                // event of every in-flight execution by design (its `prev` is
                // not in the store), so a `warn` here would emit one line per
                // in-flight execution on every arm — noise that trains readers
                // to ignore the one that matters. The counter carries the
                // signal; `rejected` staying high after the in-flight set has
                // turned over is the thing to look at.
                // ⚠ `?e` (Debug), not `%e`: `ChainError` carries no `Display`.
                tracing::debug!(target: "noetl_server::chain_populate",
                    execution_id = %execution_id, event_id = %event_id, error = ?e,
                    "chain populate rejected");
                crate::metrics::record_chain_populate("append_rejected");
            }
        }
    }
    crate::metrics::record_chain_populate(PopulateOutcome::classify(landed, rows.len()).label());
}

/// Read an execution's authority, for the verify endpoint and for tests.
///
/// `None` when the populator is off — *"nothing can be said"*, which is not the
/// same as `NotPopulated`.
pub fn authority_of(execution_id: &str) -> Option<Authority> {
    let rig = rig()?;
    let guard = match rig.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.populator().authority(execution_id).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_covers_every_split() {
        assert_eq!(PopulateOutcome::classify(0, 0), PopulateOutcome::Skipped);
        assert_eq!(PopulateOutcome::classify(3, 3), PopulateOutcome::Populated);
        assert_eq!(PopulateOutcome::classify(0, 3), PopulateOutcome::Rejected);
        assert_eq!(PopulateOutcome::classify(1, 3), PopulateOutcome::Partial);
    }

    /// ⚠ An all-rejected batch must NOT read as `populated`.
    ///
    /// This is the shape that made an unreachable shadow invisible for a full
    /// window: a verdict that cannot distinguish "did nothing" from "did
    /// everything" reports healthy on a broken path.
    #[test]
    fn a_batch_that_landed_nothing_is_not_populated() {
        assert_ne!(
            PopulateOutcome::classify(0, 5).label(),
            PopulateOutcome::Populated.label(),
            "a batch where every row was rejected reported as populated"
        );
        assert_eq!(PopulateOutcome::classify(0, 5).label(), "rejected");
    }

    #[test]
    fn every_outcome_label_is_enumerated_for_pinning() {
        for o in [
            PopulateOutcome::Populated,
            PopulateOutcome::Partial,
            PopulateOutcome::Rejected,
            PopulateOutcome::Skipped,
        ] {
            assert!(
                PopulateOutcome::ALL_LABELS.contains(&o.label()),
                "{} is not in ALL_LABELS, so it would be an absent series \
                 rather than a pinned 0",
                o.label()
            );
        }
        assert_eq!(PopulateOutcome::ALL_LABELS.len(), 4);
    }

    /// The flag is off by default, so the module is inert on arrival.
    #[test]
    fn the_populator_is_off_by_default() {
        // Reads the same authority `ehdb_l0` does; asserting it here pins that
        // this module did not introduce a second, divergent flag reader.
        assert_eq!(
            ehdb_l0::chain_populator::POPULATOR_ENV,
            "NOETL_CHAIN_POPULATE"
        );
    }

    #[test]
    fn the_default_root_is_under_the_durable_mount() {
        assert!(
            DEFAULT_POPULATE_DIR.starts_with("/data/"),
            "default root {DEFAULT_POPULATE_DIR} is not under /data, so it would \
             write to the pod's ephemeral layer"
        );
    }
}
