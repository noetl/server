//! Process-wide chain-certificate observer and fold-skip decisions.
//!
//! The server is the producer for `noetl.event`, so it advances a per-execution
//! rolling digest as it writes and can then decide in O(1) whether a cached
//! fold is still valid — instead of re-running `WorkflowState::from_events`.
//! See [`noetl_orchestrate_core::chain_cert`] for the construction and the
//! fail-closed rules.
//!
//! # ⚠ Every event-insert site must call [`observe`]
//!
//! There is no single chokepoint. `handlers::event_write::emit_events` describes
//! itself as "the one chokepoint every authoritative event passes through" and
//! **it is not**: two sites in `handlers::events` write `noetl.event` directly,
//! in-transaction, on the branch `should_publish` takes when it returns false —
//! which is the ONLY branch a system-pool execution can take. That already
//! shipped as a defect (noetl/ai-meta#263: `system/*` playbooks mirrored 11 of
//! 13 events on every hourly run), and
//! `handlers::ehdb_eventlog_mirror` carries a guard that counts INSERT sites
//! for exactly that reason.
//!
//! So this module is wired at **every** site, and
//! `every_event_insert_site_observes_the_chain` counts INSERT sites against
//! `certified_fold::observe` calls so a seventh site cannot be added without
//! one. A missed site does not corrupt state — `reconcile` detects the short
//! roller and fails closed — but it silently disables the feature, which is the
//! same bug wearing a different hat.
//!
//! # Cost when the flag is off
//!
//! [`observe`] checks the flag before taking the lock, so a disabled process
//! pays one relaxed atomic read per event and never contends.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use noetl_orchestrate_core::chain_cert::{self, FoldDecision};

use crate::handlers::ehdb_projection_fold::{FoldRefusal, FoldedState};

static CACHE: OnceLock<Mutex<chain_cert::CertifiedFoldCache>> = OnceLock::new();

/// The folds a skip is allowed to serve.
///
/// Separate from the certificate cache on purpose: `chain_cert` stays free of
/// server types, and the only thing that can license reuse of an entry here is
/// a [`FoldDecision::SkipValid`] from there.
static FOLDS: OnceLock<Mutex<HashMap<i64, FoldedState>>> = OnceLock::new();

fn folds() -> &'static Mutex<HashMap<i64, FoldedState>> {
    FOLDS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache() -> &'static Mutex<chain_cert::CertifiedFoldCache> {
    CACHE.get_or_init(|| Mutex::new(chain_cert::CertifiedFoldCache::new()))
}

/// Advance `execution_id`'s chain with one event. No-op when the flag is off.
///
/// Takes the fields the fold keys on rather than a serialised body: six call
/// sites each composing their own byte string is six chances to compose it
/// differently, and a digest that differs by call site reports change that did
/// not happen.
pub fn observe(
    execution_id: i64,
    event_id: i64,
    event_type: &str,
    node_name: Option<&str>,
    status: &str,
) {
    if !chain_cert::fold_skip_enabled() {
        return;
    }
    if let Ok(mut c) = cache().lock() {
        c.observe_event(execution_id, event_id, event_type, node_name, status);
    }
}

/// Whether a fold for `execution_id` can be skipped. Never skips with the flag
/// off, and never skips on a poisoned lock.
pub fn decide(execution_id: i64) -> FoldDecision {
    match cache().lock() {
        Ok(c) => c.decide_for(execution_id),
        Err(_) => FoldDecision::RefoldNoCachedState,
    }
}

/// Note that a skip was taken, for the revalidation budget.
pub fn note_skip(execution_id: i64) {
    if let Ok(mut c) = cache().lock() {
        c.note_skip(execution_id);
    }
}

/// Record that a real fold just happened, so the next decision can match it.
pub fn record_fold(execution_id: i64) {
    if !chain_cert::fold_skip_enabled() {
        return;
    }
    if let Ok(mut c) = cache().lock() {
        c.record_fold(execution_id);
    }
}

/// Calibrate / check the roller against the length a real refold just read.
/// Returns `false` when this process is proven to have missed events, which
/// fails the skip closed globally.
pub fn reconcile(execution_id: i64, actual_len: u32) -> bool {
    if !chain_cert::fold_skip_enabled() {
        return true;
    }
    match cache().lock() {
        Ok(mut c) => c.reconcile(execution_id, actual_len),
        Err(_) => false,
    }
}

/// Drop an execution's chain state (it completed, or its slot was evicted).
pub fn forget(execution_id: i64) {
    if let Ok(mut c) = cache().lock() {
        c.forget(execution_id);
    }
    if let Ok(mut f) = folds().lock() {
        f.remove(&execution_id);
    }
}

/// Fold `execution_id`, or skip the fold when the certificate proves the chain
/// has not moved since the cached fold was built.
///
/// This is where the O(1) win is actually taken. `do_fold` is the real fold and
/// runs on every path except an exact certificate match.
///
/// ⚠ The order matters. `reconcile` runs on the REAL fold's `applied_count`,
/// which is the only moment this process learns the chain's true length — so
/// calibration and miss-detection both happen there, before any skip is ever
/// licensed for that execution.
pub async fn fold_or_skip<F, Fut>(execution_id: i64, do_fold: F) -> Result<FoldedState, FoldRefusal>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<FoldedState, FoldRefusal>>,
{
    if chain_cert::fold_skip_enabled() && decide(execution_id).skips_fold() {
        let hit = folds()
            .lock()
            .ok()
            .and_then(|f| f.get(&execution_id).cloned());
        if let Some(cached) = hit {
            note_skip(execution_id);
            return Ok(cached);
        }
        // Decision said skip but no fold is held (process restart, eviction).
        // Fall through and do the real work rather than invent a result.
    }

    let folded = do_fold().await?;

    if chain_cert::fold_skip_enabled() {
        // The real chain length, straight from the fold that just read it.
        if reconcile(execution_id, folded.applied_count as u32) {
            record_fold(execution_id);
            if let Ok(mut f) = folds().lock() {
                f.insert(execution_id, folded.clone());
            }
        } else {
            // This process missed events. `reconcile` already failed the skip
            // closed globally; drop any fold we might serve from.
            if let Ok(mut f) = folds().lock() {
                f.remove(&execution_id);
            }
        }
    }
    Ok(folded)
}

/// `(observed, decided, skipped, refolded, divergences)` for `/metrics` and for
/// a ramp to watch.
pub fn counters() -> (u64, u64, u64, u64, u64) {
    chain_cert::counters()
}

/// Non-zero once this process is proven to have missed an event. **The ramp's
/// rollback signal.**
pub fn divergences() -> u64 {
    chain_cert::divergences()
}

#[cfg(test)]
// The serialisation lock below is deliberately held across `.await`: these tests
// drive PROCESS-GLOBAL state (the cache, the counters, and `NOETL_CHAIN_CERT`),
// so they must not interleave. Production `fold_or_skip` takes no guard across
// an await — every lock there is released before `do_fold` runs — so this allow
// is scoped to the test module and must not be widened.
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AtOrd};

    /// The globals here are process-wide, so these tests serialise.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn folded(applied: usize) -> FoldedState {
        FoldedState {
            source: crate::handlers::ehdb_projection_fold::FoldSource::Postgres,
            version: applied as i64,
            applied_count: applied,
            digest: format!("digest-of-{applied}"),
        }
    }

    /// Feed `n` events for `exec` through the real observer.
    fn observe_n(exec: i64, n: usize) {
        for i in 0..n {
            observe(
                exec,
                10_000 + i as i64,
                "step.enter",
                Some("step_a"),
                "success",
            );
        }
    }

    /// ⚠ THE SYSTEM-LEVEL "exists but never runs" CONTROL.
    ///
    /// The unit tests in `chain_cert` prove the decision logic. They cannot
    /// prove the wiring takes the skip: a `fold_or_skip` that always calls
    /// `do_fold` passes every one of them, and would show up in production as a
    /// feature that is on, costs its observe, and saves nothing.
    ///
    /// So count the folds and require the second call NOT to run one.
    #[tokio::test]
    async fn the_wired_skip_actually_avoids_the_second_fold() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NOETL_CHAIN_CERT", "1");
        chain_cert::reset_counters();
        let exec = 90_001;
        forget(exec);

        let folds_run = AtomicUsize::new(0);
        let run = || {
            folds_run.fetch_add(1, AtOrd::Relaxed);
            async { Ok(folded(16)) }
        };

        observe_n(exec, 16);

        // First call: nothing cached, and uncalibrated -> a real fold, which is
        // also what calibrates the roller.
        let a = fold_or_skip(exec, run).await.expect("fold");
        assert_eq!(
            folds_run.load(AtOrd::Relaxed),
            1,
            "the first call must fold"
        );

        // Second call, chain unchanged: the fold must NOT run again.
        let b = fold_or_skip(exec, run).await.expect("skip");
        assert_eq!(
            folds_run.load(AtOrd::Relaxed),
            1,
            "an unchanged chain must SKIP the fold — it ran again, so the skip is              wired but inert"
        );
        assert_eq!(a.digest, b.digest, "the skip must serve the cached fold");

        let (_obs, _dec, skipped, _ref, div) = counters();
        assert!(skipped >= 1, "the skip must be COUNTED, got {skipped}");
        assert_eq!(div, 0, "no divergence expected on a complete roller");

        forget(exec);
        chain_cert::reset_counters();
        std::env::remove_var("NOETL_CHAIN_CERT");
    }

    #[tokio::test]
    async fn new_events_force_the_fold_to_run_again() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NOETL_CHAIN_CERT", "1");
        chain_cert::reset_counters();
        let exec = 90_002;
        forget(exec);

        let folds_run = AtomicUsize::new(0);
        observe_n(exec, 16);
        let _ = fold_or_skip(exec, || {
            folds_run.fetch_add(1, AtOrd::Relaxed);
            async { Ok(folded(16)) }
        })
        .await
        .expect("fold");
        assert_eq!(folds_run.load(AtOrd::Relaxed), 1);

        // Eight more events arrive.
        observe_n(exec, 8);
        let _ = fold_or_skip(exec, || {
            folds_run.fetch_add(1, AtOrd::Relaxed);
            async { Ok(folded(24)) }
        })
        .await
        .expect("refold");
        assert_eq!(
            folds_run.load(AtOrd::Relaxed),
            2,
            "a chain that advanced MUST refold"
        );

        forget(exec);
        chain_cert::reset_counters();
        std::env::remove_var("NOETL_CHAIN_CERT");
    }

    #[tokio::test]
    async fn the_flag_off_never_skips_through_the_wiring() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("NOETL_CHAIN_CERT");
        let exec = 90_003;
        forget(exec);
        let folds_run = AtomicUsize::new(0);
        for _ in 0..3 {
            let _ = fold_or_skip(exec, || {
                folds_run.fetch_add(1, AtOrd::Relaxed);
                async { Ok(folded(16)) }
            })
            .await
            .expect("fold");
        }
        assert_eq!(
            folds_run.load(AtOrd::Relaxed),
            3,
            "with the flag off every call must fold"
        );
        forget(exec);
    }

    /// A process that missed events must stop skipping, through the wiring.
    #[tokio::test]
    async fn a_missed_event_detected_by_the_fold_stops_the_skipping() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NOETL_CHAIN_CERT", "1");
        chain_cert::reset_counters();
        let exec = 90_004;
        forget(exec);

        // The roller sees 16, but the real fold reads 24 on its FIRST fold —
        // that is a late start, so it calibrates rather than diverging.
        observe_n(exec, 16);
        let folds_run = AtomicUsize::new(0);
        let _ = fold_or_skip(exec, || {
            folds_run.fetch_add(1, AtOrd::Relaxed);
            async { Ok(folded(24)) }
        })
        .await
        .expect("fold");
        assert_eq!(chain_cert::divergences(), 0, "a late start is not a miss");

        // The roller now sees nothing more while the chain grows. The skip would
        // run forever on an unchanging certificate and the detector would never
        // fire — which is exactly the flaw `MAX_CONSECUTIVE_SKIPS` exists to
        // close. Drive past the budget and require the forced fold to catch it.
        for _ in 0..=chain_cert::MAX_CONSECUTIVE_SKIPS {
            let _ = fold_or_skip(exec, || {
                folds_run.fetch_add(1, AtOrd::Relaxed);
                async { Ok(folded(32)) }
            })
            .await
            .expect("fold");
            if chain_cert::divergences() > 0 {
                break;
            }
        }
        assert_eq!(
            chain_cert::divergences(),
            1,
            "a roller that missed events must be DETECTED through the wiring              within MAX_CONSECUTIVE_SKIPS ({}) skips — otherwise a process that              stopped seeing events skips forever and serves stale state",
            chain_cert::MAX_CONSECUTIVE_SKIPS
        );

        // And from here nothing may skip, for any execution.
        let before = folds_run.load(AtOrd::Relaxed);
        let _ = fold_or_skip(exec, || {
            folds_run.fetch_add(1, AtOrd::Relaxed);
            async { Ok(folded(32)) }
        })
        .await
        .expect("fold");
        assert_eq!(
            folds_run.load(AtOrd::Relaxed),
            before + 1,
            "after a divergence every call must fold"
        );

        forget(exec);
        chain_cert::reset_counters();
        std::env::remove_var("NOETL_CHAIN_CERT");
    }

    /// Every direct `noetl.event` INSERT must also advance the chain.
    ///
    /// Shaped after `ehdb_eventlog_mirror::every_in_tx_event_insert_is_mirrored`,
    /// which exists because a previous "one chokepoint" assumption was false and
    /// shipped noetl/ai-meta#263. Counting sites rather than naming them is the
    /// point: the failure this catches is a SEVENTH site added later, and a test
    /// listing the six it already knows about cannot catch that.
    #[test]
    fn every_event_insert_site_observes_the_chain() {
        // (file, source) — every file in the server that writes noetl.event.
        let files: &[(&str, &str)] = &[
            (
                "handlers/event_write.rs",
                include_str!("../handlers/event_write.rs"),
            ),
            ("handlers/events.rs", include_str!("../handlers/events.rs")),
            (
                "handlers/internal.rs",
                include_str!("../handlers/internal.rs"),
            ),
            (
                "db/queries/event.rs",
                include_str!("../db/queries/event.rs"),
            ),
            ("services/internal.rs", include_str!("internal.rs")),
        ];

        let mut total_inserts = 0usize;
        let mut problems = Vec::new();
        for (name, src) in files {
            // Strip tests and comments so the guard measures the CODE, not
            // itself or its own documentation.
            let code = src.split("#[cfg(test)]").next().unwrap_or("");
            let code: String = code
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");

            // `noetl.event_dead_letter` is a different table and is not part of
            // any execution's chain.
            let inserts = code.matches("INSERT INTO noetl.event (").count()
                + code.matches("INSERT INTO noetl.event\n").count();
            let observes = code.matches("certified_fold::observe(").count();
            total_inserts += inserts;
            if inserts != observes {
                problems.push(format!(
                    "{name}: {inserts} INSERT site(s) but {observes} observe call(s)"
                ));
            }
        }

        assert!(
            problems.is_empty(),
            "every direct `noetl.event` INSERT owes the chain a \
             `certified_fold::observe` — without it that execution's roller runs \
             short, `reconcile` fails the skip closed, and the feature silently \
             disables itself (the noetl/ai-meta#263 shape).\n  {}",
            problems.join("\n  ")
        );

        // Self-check: a guard that found nothing is not measuring anything.
        assert!(
            total_inserts >= 5,
            "expected at least 5 INSERT sites across the server; found \
             {total_inserts}. Either the guard's file list is stale or the \
             inserts moved — fix the guard, do not lower the number."
        );
    }
}
