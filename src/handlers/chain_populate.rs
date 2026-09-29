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

/// ⭐⭐ **The chain source built on the authoritative log.**
///
/// Reads the execution's events from `noetl.event`, populates the chain partition
/// from them (edges recomputed from log order), then answers from the **store**.
///
/// # Why populate-then-read rather than read-the-store
///
/// The store is a derived structure. Reading it directly makes it the source of
/// truth for a decision, and it can only be as complete as whatever populated it —
/// which is how a partition holding an execution's 3rd-event-onward read as
/// authoritative. Populating from the log first makes the answer **correct by
/// construction**, and correctness is the thing being bought here; the O(1)
/// parent lookup the RFC is ultimately after is a later increment on the same
/// structure.
///
/// # Why answer from the store rather than from the rows just read
///
/// Because that is what proves the store is fit to serve. The rows are available;
/// returning them would make the store's content untested and the eventual repoint
/// unjustified. Answering from the store means every decision this source makes is
/// a decision the store was capable of.
pub struct LogSourcedChainSource {
    pool: crate::db::DbPool,
    limit: i64,
}

impl LogSourcedChainSource {
    pub fn new(pool: crate::db::DbPool, limit: i64) -> Self {
        Self { pool, limit }
    }
}

#[async_trait::async_trait]
impl crate::chain_advance::ChainSource for LogSourcedChainSource {
    async fn chain_for(&self, execution_id: i64) -> Option<crate::chain_advance::ChainView> {
        let rows =
            match crate::db::queries::event_chain::read_chain(&self.pool, execution_id, self.limit)
                .await
            {
                Ok(r) => r,
                // ⚠ `None`, never an empty view. A failed read is "cannot answer"; an
                // empty view would say "this execution has no events", and the caller
                // would treat a running execution as finished.
                Err(e) => {
                    tracing::warn!(target: "noetl_server::chain_populate", execution_id, error = %e,
                    "authoritative chain read failed; falling through");
                    crate::metrics::record_chain_populate("log_read_failed");
                    return None;
                }
            };

        // ⚠⚠ A full page is NOT a complete log. Answering from a truncated read
        // would rebuild the truncation defect on the other end: the partition would
        // be populated with a prefix and reported complete.
        if rows.len() as i64 >= self.limit {
            tracing::warn!(target: "noetl_server::chain_populate", execution_id,
                rows = rows.len(), limit = self.limit,
                "authoritative chain read hit the row limit; refusing to populate a \
                 partial log");
            crate::metrics::record_chain_populate("log_truncated");
            return None;
        }

        let terminal: Vec<bool> = rows
            .iter()
            .map(|r| crate::db::queries::event_chain::is_terminal_event_type(&r.event_type))
            .collect();

        let events: Vec<ehdb_l0::chain_populator::LogEvent> = rows
            .iter()
            .map(|r| ehdb_l0::chain_populator::LogEvent {
                event_id: r.event_id.to_string(),
                parent_execution_id: r.parent_execution_id.map(|v| v.to_string()),
                payload: String::new(),
            })
            .collect();

        let rig = rig()?;
        let guard = match rig.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let populator = guard.populator();
        let key = execution_id.to_string();

        match populator.populate_from_log(&key, &events) {
            Ok(outcome) => {
                crate::metrics::record_chain_populate(outcome.label());
                if let ehdb_l0::chain_populator::FromLog::Diverged {
                    at_position,
                    ref stored,
                    ref log,
                } = outcome
                {
                    tracing::warn!(target: "noetl_server::chain_populate", execution_id,
                        at_position, stored = %stored, log = %log,
                        "chain partition DIVERGED from the authoritative log");
                    return None;
                }
            }
            Err(e) => {
                tracing::warn!(target: "noetl_server::chain_populate", execution_id, error = %e,
                    "populate from log failed");
                crate::metrics::record_chain_populate("populate_failed");
                return None;
            }
        }

        // Answer from the STORE, through the guarded read, so coverage and the
        // watermark both have to agree before a decision is made on it.
        let chain = match populator.chain_if_authoritative(&key) {
            Ok(Some(c)) => c,
            Ok(None) => {
                crate::metrics::record_chain_populate("guarded_read_refused");
                return None;
            }
            Err(e) => {
                tracing::warn!(target: "noetl_server::chain_populate", execution_id, error = %e,
                    "guarded read failed");
                crate::metrics::record_chain_populate("guarded_read_failed");
                return None;
            }
        };

        // ⚠ The store does not carry `terminal`; it is a property of the event TYPE,
        // which lives in the log. Zip by position — safe because the guarded read
        // returned a chain whose length the coverage record pinned to `events.len()`,
        // and `terminal` was built from the same rows.
        if chain.len() != terminal.len() {
            crate::metrics::record_chain_populate("length_disagreement");
            return None;
        }

        let events = chain
            .iter()
            .zip(terminal.iter())
            .map(|(e, &t)| crate::chain_advance::ChainEventView {
                exec_seq: e.exec_seq,
                event_id: e.event_id.clone(),
                prev_event_id: e.prev_event_id.clone(),
                execution_id: e.execution_id.clone(),
                parent_execution_id: e.parent_execution_id.clone(),
                terminal: t,
            })
            .collect();

        Some(crate::chain_advance::ChainView {
            execution_id: execution_id.to_string(),
            events,
        })
    }

    fn source_name(&self) -> &'static str {
        "log_sourced_chain_store"
    }
}

#[cfg(test)]
mod tests {
    use super::*;




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

#[cfg(test)]
mod label_denominator_tests {
    /// ⚠⚠ **Print the denominator.** Every label this module actually records must
    /// be in the pinned set, and the set is checked against the SOURCE rather than
    /// against a hand-written list — because the hand-written list was already
    /// short by six, and a missing label is an absent series that reads exactly
    /// like a build predating the metric.
    ///
    /// Found by reading `/metrics` on a running server, not by reading the code.
    #[test]
    fn every_recorded_label_is_pinned() {
        let src = include_str!("chain_populate.rs");
        let non_test = src.split("#[cfg(test)]").next().unwrap();
        assert!(
            non_test.len() > 4000,
            "extracted {} bytes — implausibly small; a guard measuring nothing \
             passes",
            non_test.len()
        );

        // Literal `record_chain_populate("x")` call sites.
        let mut found: Vec<String> = Vec::new();
        let needle = "record_chain_populate(\"";
        let mut at = 0usize;
        while let Some(i) = non_test[at..].find(needle) {
            let start = at + i + needle.len();
            let end = non_test[start..]
                .find('"')
                .map(|j| start + j)
                .expect("unterminated label literal");
            found.push(non_test[start..end].to_string());
            at = end;
        }
        assert!(
            found.len() >= 9,
            "found only {} literal label call sites — the extraction is probably \
             wrong, and a short denominator reports a false clean: {found:?}",
            found.len()
        );

        for l in &found {
            assert!(
                crate::metrics::CHAIN_POPULATE_OUTCOMES.contains(&l.as_str()),
                "label {l:?} is recorded but NOT pinned, so it is an absent series \
                 until it first fires — which reads identically to a binary that \
                 has no such metric.\nrecorded={found:?}\npinned={:?}",
                crate::metrics::CHAIN_POPULATE_OUTCOMES
            );
        }

        // And the labels reached indirectly, through `FromLog::label()`.
        for l in ehdb_l0::chain_populator::FromLog::ALL_LABELS {
            assert!(
                crate::metrics::CHAIN_POPULATE_OUTCOMES.contains(&l),
                "FromLog label {l:?} is recorded via outcome.label() but not pinned"
            );
        }

        eprintln!(
            "denominator: {} literal labels + {} FromLog labels, {} pinned",
            found.len(),
            ehdb_l0::chain_populator::FromLog::ALL_LABELS.len(),
            crate::metrics::CHAIN_POPULATE_OUTCOMES.len()
        );
    }
}
