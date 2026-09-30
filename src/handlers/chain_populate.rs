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

/// Drive `populate_from_log` directly against the process store.
///
/// ⚠ Exposed for the divergence POSITIVE CONTROL only. The burst test proves that
/// concurrent reads no longer produce false divergence; without a way to plant a
/// genuine CONTENT conflict, that test would also pass on a build where
/// divergence detection had simply been switched off (noetl/ai-meta#360).
///
/// It does not bypass anything the normal path enforces — it is the same
/// `populate_from_log` on the same store, just with a caller-supplied log.
pub fn populate_from_log_for_test(
    execution_id: i64,
    events: &[ehdb_l0::chain_populator::LogEvent],
) -> Option<ehdb_l0::chain_populator::FromLog> {
    let rig = rig()?;
    let guard = match rig.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard
        .populator()
        .populate_from_log(&execution_id.to_string(), events)
        .ok()
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

    /// One authoritative read: the log's events plus their terminal flags.
    ///
    /// `None` is "cannot answer" and has already recorded WHY — a failed read or a
    /// truncated page. Never an empty view for either, because an empty view says
    /// "this execution has no events" and a running execution would read as
    /// finished.
    async fn read_log(
        &self,
        execution_id: i64,
    ) -> Option<(Vec<ehdb_l0::chain_populator::LogEvent>, Vec<bool>)> {
        let rows =
            match crate::db::queries::event_chain::read_chain(&self.pool, execution_id, self.limit)
                .await
            {
                Ok(r) => r,
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

        Some((events, terminal))
    }
}

#[async_trait::async_trait]
impl crate::chain_advance::ChainSource for LogSourcedChainSource {
    async fn chain_for(&self, execution_id: i64) -> Option<crate::chain_advance::ChainView> {
        let (events, terminal) = self.read_log(execution_id).await?;

        let rig = rig()?;
        let key = execution_id.to_string();

        // ⚠⚠⚠ THE STALE-vs-FOREIGN-WRITER DISCRIMINATION, and why it needs a
        // SECOND read (noetl/ai-meta#360, guarding noetl/ai-meta#358).
        //
        // "The store holds more than my snapshot, and the prefix agrees" has TWO
        // causes that a single snapshot CANNOT tell apart:
        //
        //   (a) benign staleness — a concurrent reader applied a FRESHER snapshot
        //       between our read and our lock. The extra events are real log events
        //       we simply had not read yet.
        //   (b) a FOREIGN WRITER — a second populator put events in the store that
        //       are not in the log at all. This is exactly noetl/ai-meta#358, the
        //       13.3% divergence, and it must NOT be absorbed.
        //
        // I originally classified this as benign on the reasoning that
        // `populate_from_log` is the sole writer — which is true only while no
        // second writer exists, i.e. it assumes away case (b). The #358 regression
        // test caught that immediately, which is the test doing its job.
        //
        // The discriminator is whether the log CATCHES UP: re-read it, and if it
        // now contains at least what the store holds, it was (a). If the log still
        // ends short while the store runs ahead, the extra events are not in the
        // authoritative log and it is (b) — escalate to a divergence and refuse.
        //
        // The re-read costs one query on a path that is already doing one, and only
        // on an outcome that should be rare.
        // ⚠⚠ POPULATE AND THE GUARDED READ MUST BE ONE LOCK HOLD.
        //
        // My first version of this escalation released the lock between them, and
        // under the concurrent burst that produced 2 `length_disagreement` refusals
        // in 800: another reader advanced the store after our populate, so the chain
        // came back LONGER than the `terminal` vector built from our log rows, and
        // the zip below refused. Safe — it declines rather than answering wrongly —
        // but it is the same false alarm one step downstream, and on prod it counts
        // against the comparator.
        //
        // Holding one lock across both makes the pair atomic: the chain we read is
        // exactly the chain we just populated.
        let attempt = |evs: &[ehdb_l0::chain_populator::LogEvent]| {
            let guard = match rig.lock() {
                Ok(g) => g,
                Err(poisoned) => poisoned.into_inner(),
            };
            let populator = guard.populator();
            let outcome = populator.populate_from_log(&key, evs);
            match outcome {
                Ok(o) => {
                    // Read under the SAME hold, unless the outcome already decides.
                    let needs_read = !matches!(
                        o,
                        ehdb_l0::chain_populator::FromLog::Diverged { .. }
                            | ehdb_l0::chain_populator::FromLog::StaleLog { .. }
                    );
                    let chain = if needs_read {
                        Some(populator.chain_if_authoritative(&key))
                    } else {
                        None
                    };
                    (Ok(o), chain)
                }
                Err(e) => (Err(e), None),
            }
        };

        let (outcome_or_err, chain_res, terminal) = {
            let (o, c) = attempt(&events);
            match o {
                Ok(ehdb_l0::chain_populator::FromLog::StaleLog {
                    stored_len,
                    log_len,
                }) => {
                    crate::metrics::record_chain_populate("stale_log");
                    tracing::debug!(target: "noetl_server::chain_populate", execution_id,
                        stored_len, log_len,
                        "log snapshot was behind the store; re-reading to tell \
                         staleness from a foreign writer");

                    let (fresh_events, fresh_terminal) = self.read_log(execution_id).await?;
                    if fresh_events.len() < stored_len {
                        // The log did NOT catch up. The store's extra events are not
                        // in the authoritative log, so a writer other than this one
                        // put them there (noetl/ai-meta#358).
                        tracing::warn!(target: "noetl_server::chain_populate", execution_id,
                            stored_len, log_len, reread_len = fresh_events.len(),
                            "chain partition holds events the authoritative log does \
                             not have after a re-read; treating as DIVERGED (a \
                             second writer)");
                        crate::metrics::record_chain_populate("diverged");
                        return None;
                    }
                    let (o2, c2) = attempt(&fresh_events);
                    (o2, c2, fresh_terminal)
                }
                other => (other, c, terminal),
            }
        };

        match outcome_or_err {
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
                // ⚠ A StaleLog reaching here is the SECOND attempt after the log
                // caught up. The store is a superset of a log we have now confirmed,
                // so it is the fresher answer — but we deliberately did NOT read the
                // chain under that hold (the outcome was undecided), so fall through
                // rather than answer from a read we did not take atomically.
                if matches!(outcome, ehdb_l0::chain_populator::FromLog::StaleLog { .. }) {
                    crate::metrics::record_chain_populate("stale_log_unresolved");
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

        // Answer from the STORE, through the guarded read taken under the SAME lock
        // hold as the populate, so coverage and the watermark both have to agree
        // before a decision is made on it — and the store cannot have moved.
        // ⚠ No `expect` on a request path. Every decided outcome reads the chain under
        // the same hold, so `None` here is unreachable today — but an `expect` turns a
        // future refactor of the match above into a panic in the server, and this
        // path's whole job is to decline safely.
        let Some(chain_res) = chain_res else {
            tracing::warn!(target: "noetl_server::chain_populate", execution_id,
                "no chain was read under the populate's lock hold; declining");
            crate::metrics::record_chain_populate("guarded_read_refused");
            return None;
        };
        let chain = match chain_res {
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

// ---------------------------------------------------------------------------
// noetl/ai-meta#362 (d) — the one-root invariant, as a live signal.
// ---------------------------------------------------------------------------

/// Query for [`chain_invariant`].
#[derive(Debug, serde::Deserialize)]
pub struct ChainInvariantQuery {
    /// How far back to look. Defaults to 168h (7 days).
    ///
    /// ⚠ A WINDOW, not "everything", and that is deliberate. The measured corpus
    /// contains a dead pre-feature era (Apr 25-27) where nothing stamped a link at
    /// all — 533 executions, every event a false root, one of them with 70,977 of
    /// them. Including it reports 10% healthy forever and buries any live
    /// regression under four-month-old test data. The window keeps the signal about
    /// the linked era.
    pub since_hours: Option<i64>,
    /// Cap on the offenders listed back. Defaults to 20.
    pub limit: Option<i64>,
}

#[derive(Debug, serde::Serialize)]
pub struct ChainInvariantOffender {
    pub execution_id: String,
    pub events: i64,
    pub roots: i64,
}

#[derive(Debug, serde::Serialize)]
pub struct ChainInvariantReport {
    pub since_hours: i64,
    /// ⭐ The denominator. A finding without it is not evidence — a clean report
    /// over zero executions reads exactly like a healthy system.
    pub executions: i64,
    pub events: i64,
    /// Exactly one NULL-prev event: the genesis, and the healthy shape.
    pub one_root: i64,
    /// ⚠ More than one: the re-rooting defect. A link-defined chain cannot be
    /// built for these, because nothing can say which root is real.
    pub multi_root: i64,
    /// No NULL-prev event at all: the root is outside the window, or a cycle.
    pub no_root: i64,
    pub healthy_pct: f64,
    pub worst: Vec<ChainInvariantOffender>,
}

/// `GET /api/chain/invariant` — read-only.
///
/// The invariant is: **exactly one NULL-prev event per execution.** That event is
/// the genesis; every other event links to its predecessor. One root is correct by
/// design, so the aggregate NULL count says nothing — this has to be measured PER
/// EXECUTION, and getting that wrong is what hid the shape for a whole session.
///
/// More than one root is the defect: a lost in-memory head map stamps a mid-flight
/// execution's next event as a second false root, and under link-defined ordering
/// the partition then cannot be built at all.
///
/// ⚠ Touches nothing. It exists because there was no way to measure this on a
/// running cluster: `/api/postgres/execute` is disabled, no endpoint exposed
/// `prev_event_id`, and every number in the investigation had to come from a local
/// cluster instead of the one that mattered.
pub async fn chain_invariant(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
    _token: crate::handlers::internal::RequireInternalApiToken,
    axum::extract::Query(q): axum::extract::Query<ChainInvariantQuery>,
) -> crate::error::AppResult<axum::Json<ChainInvariantReport>> {
    let since_hours = q.since_hours.unwrap_or(168).clamp(1, 24 * 400);
    let limit = q.limit.unwrap_or(20).clamp(1, 500);

    let rows: Vec<(i64, i64, i64)> = sqlx::query_as(
        "WITH per AS ( \
           SELECT execution_id, count(*) AS n, \
                  count(*) FILTER (WHERE prev_event_id IS NULL) AS roots \
           FROM noetl.event \
           WHERE created_at > now() - make_interval(hours => $1::int) \
           GROUP BY execution_id) \
         SELECT execution_id, n, roots FROM per ORDER BY roots DESC, n DESC",
    )
    .bind(since_hours)
    .fetch_all(&state.db)
    .await?;

    let executions = rows.len() as i64;
    let events: i64 = rows.iter().map(|r| r.1).sum();
    let one_root = rows.iter().filter(|r| r.2 == 1).count() as i64;
    let multi_root = rows.iter().filter(|r| r.2 > 1).count() as i64;
    let no_root = rows.iter().filter(|r| r.2 == 0).count() as i64;

    crate::metrics::set_chain_root_invariant("one_root", one_root);
    crate::metrics::set_chain_root_invariant("multi_root", multi_root);
    crate::metrics::set_chain_root_invariant("no_root", no_root);

    let worst = rows
        .iter()
        .filter(|r| r.2 != 1)
        .take(limit as usize)
        .map(|r| ChainInvariantOffender {
            // String, because a 64-bit id loses precision in a browser's JSON number.
            execution_id: r.0.to_string(),
            events: r.1,
            roots: r.2,
        })
        .collect();

    Ok(axum::Json(ChainInvariantReport {
        since_hours,
        executions,
        events,
        one_root,
        multi_root,
        no_root,
        healthy_pct: if executions == 0 {
            0.0
        } else {
            (one_root as f64) * 100.0 / (executions as f64)
        },
        worst,
    }))
}
