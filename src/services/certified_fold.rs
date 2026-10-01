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

static CACHE: OnceLock<Mutex<chain_cert::CertifiedFoldCache>> = OnceLock::new();

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

/// Calibrate / check the roller against the chain HEAD a real rebuild saw.
/// Returns `false` when this process is proven to have missed events.
pub fn reconcile_head(execution_id: i64, actual_head: i64) -> bool {
    if !chain_cert::fold_skip_enabled() {
        return true;
    }
    match cache().lock() {
        Ok(mut c) => c.reconcile_head(execution_id, actual_head),
        Err(_) => false,
    }
}

/// Drop an execution's chain state (it completed, or its slot was evicted).
pub fn forget(execution_id: i64) {
    if let Ok(mut c) = cache().lock() {
        c.forget(execution_id);
    }
}

/// Rebuild `execution_id`'s state, or skip the rebuild when the certificate
/// proves the chain has not moved since the cached one.
///
/// The orchestrator's per-drive rebuild is the hot refold — the ~19,840 refolds
/// the spec measured — so this is where the win is actually taken. `head_of`
/// extracts the chain head from whatever the rebuild returns, which is the
/// signal `reconcile_head` checks the precondition against.
///
/// ⚠ Generic over the rebuild's result type on purpose: `RebuildResult` is
/// private to `handlers::events`, and this module must not grow a dependency on
/// it just to hold a cache.
pub async fn rebuild_or_skip<T, E, F, Fut>(
    execution_id: i64,
    cached: &Mutex<HashMap<i64, T>>,
    head_of: impl Fn(&T) -> i64,
    do_rebuild: F,
) -> Result<T, E>
where
    T: Clone,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    if chain_cert::fold_skip_enabled() && decide(execution_id).skips_fold() {
        let hit = cached
            .lock()
            .ok()
            .and_then(|m| m.get(&execution_id).cloned());
        if let Some(c) = hit {
            note_skip(execution_id);
            return Ok(c);
        }
    }

    let built = do_rebuild().await?;

    if chain_cert::fold_skip_enabled() {
        if reconcile_head(execution_id, head_of(&built)) {
            record_fold(execution_id);
            if let Ok(mut m) = cached.lock() {
                m.insert(execution_id, built.clone());
            }
        } else if let Ok(mut m) = cached.lock() {
            m.remove(&execution_id);
        }
    }
    Ok(built)
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

    /// Stand-in for the rebuild's result: all `rebuild_or_skip` needs is a
    /// `Clone` value and a chain head to check the precondition against.
    #[derive(Clone, PartialEq, Debug)]
    struct Rebuilt {
        head: i64,
    }

    fn rebuilt(head: i64) -> Rebuilt {
        Rebuilt { head }
    }

    static CACHED: OnceLock<Mutex<HashMap<i64, Rebuilt>>> = OnceLock::new();
    fn cached() -> &'static Mutex<HashMap<i64, Rebuilt>> {
        CACHED.get_or_init(|| Mutex::new(HashMap::new()))
    }

    async fn run_once(
        exec: i64,
        head: i64,
        counter: &AtomicUsize,
    ) -> Result<Rebuilt, std::convert::Infallible> {
        rebuild_or_skip(
            exec,
            cached(),
            |r: &Rebuilt| r.head,
            || {
                counter.fetch_add(1, AtOrd::Relaxed);
                async move { Ok(rebuilt(head)) }
            },
        )
        .await
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
    /// prove the wiring takes the skip: a `rebuild_or_skip` that always calls
    /// `do_rebuild` passes every one of them, and would ship as a feature that
    /// is on, pays its observe, and saves nothing. So count the rebuilds and
    /// require the second call not to run one.
    #[tokio::test]
    async fn the_wired_skip_actually_avoids_the_second_rebuild() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NOETL_CHAIN_CERT", "1");
        chain_cert::reset_counters();
        let exec = 90_001;
        forget(exec);
        let runs = AtomicUsize::new(0);

        observe_n(exec, 16);
        let head = 10_000 + 15; // the highest id `observe_n` used

        let a = run_once(exec, head, &runs).await.unwrap();
        assert_eq!(runs.load(AtOrd::Relaxed), 1, "the first call must rebuild");

        let b = run_once(exec, head, &runs).await.unwrap();
        assert_eq!(
            runs.load(AtOrd::Relaxed),
            1,
            "an unchanged chain must SKIP the rebuild — it ran again, so the skip \
             is wired but inert"
        );
        assert_eq!(a, b, "the skip must serve the cached rebuild");

        let (_o, _d, skipped, _r, div) = counters();
        assert!(skipped >= 1, "the skip must be COUNTED, got {skipped}");
        assert_eq!(div, 0, "no divergence on a complete roller");

        forget(exec);
        chain_cert::reset_counters();
        std::env::remove_var("NOETL_CHAIN_CERT");
    }

    #[tokio::test]
    async fn new_events_force_the_rebuild_to_run_again() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NOETL_CHAIN_CERT", "1");
        chain_cert::reset_counters();
        let exec = 90_002;
        forget(exec);
        let runs = AtomicUsize::new(0);

        observe_n(exec, 16);
        let _ = run_once(exec, 10_015, &runs).await.unwrap();
        assert_eq!(runs.load(AtOrd::Relaxed), 1);

        // Eight more events arrive, so the digest advances.
        for i in 16..24 {
            observe(
                exec,
                10_000 + i as i64,
                "step.enter",
                Some("step_a"),
                "success",
            );
        }
        let _ = run_once(exec, 10_023, &runs).await.unwrap();
        assert_eq!(
            runs.load(AtOrd::Relaxed),
            2,
            "a chain that advanced MUST rebuild"
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
        let runs = AtomicUsize::new(0);
        for _ in 0..3 {
            let _ = run_once(exec, 10_015, &runs).await.unwrap();
        }
        assert_eq!(
            runs.load(AtOrd::Relaxed),
            3,
            "with the flag off every call must rebuild"
        );
        forget(exec);
    }

    /// A process that missed events must stop skipping, through the wiring —
    /// and it must be DETECTED within the bounded run of skips, because
    /// `reconcile_head` only runs on a real rebuild.
    #[tokio::test]
    async fn a_missed_event_is_detected_through_the_wiring() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NOETL_CHAIN_CERT", "1");
        chain_cert::reset_counters();
        let exec = 90_004;
        forget(exec);
        let runs = AtomicUsize::new(0);

        // The roller saw up to id 10_015, but the real chain head is 10_099 —
        // on the FIRST rebuild that is a late start, not a miss.
        observe_n(exec, 16);
        let _ = run_once(exec, 10_099, &runs).await.unwrap();
        assert_eq!(chain_cert::divergences(), 0, "a late start is not a miss");

        // The roller now sees nothing more while the chain keeps growing. The
        // certificate is unchanged, so the skip would run forever and the
        // detector could never fire — which is what MAX_CONSECUTIVE_SKIPS
        // exists to close. Drive past the budget.
        for _ in 0..=chain_cert::MAX_CONSECUTIVE_SKIPS {
            let _ = run_once(exec, 10_200, &runs).await.unwrap();
            if chain_cert::divergences() > 0 {
                break;
            }
        }
        assert_eq!(
            chain_cert::divergences(),
            1,
            "a roller that missed events must be DETECTED within \
             MAX_CONSECUTIVE_SKIPS ({}) skips — otherwise a process that stopped \
             seeing events skips forever and serves stale state",
            chain_cert::MAX_CONSECUTIVE_SKIPS
        );

        forget(exec);
        chain_cert::reset_counters();
        std::env::remove_var("NOETL_CHAIN_CERT");
    }

    /// ⚠ THE SKIP MUST BE REACHABLE FROM A LIVE CALL PATH.
    ///
    /// This guard exists because I shipped the exact defect it catches.
    /// noetl/server#484 wired the skip into
    /// `ehdb_projection_fold::fold_from_postgres` — a function with **no
    /// callers**, referenced only from doc comments. The feature was
    /// mechanically correct, fully tested, merged, and **inert in production**.
    ///
    /// `the_wired_skip_actually_avoids_the_second_fold` did not catch it because
    /// it calls the wrapper directly: it proved the mechanism works, not that
    /// anything reaches it. That is the same "exists but never runs" shape this
    /// whole workstream is about, committed inside the change meant to avoid it.
    #[test]
    fn the_fold_skip_is_reachable_from_a_live_call_path() {
        let events = include_str!("../handlers/events.rs");
        // ⚠ Scans the WHOLE file, deliberately. The first version of this guard
        // used the `split("#[cfg(test)]").next()` idiom the sibling guard in
        // `ehdb_eventlog_mirror` uses — and `events.rs` has test modules
        // interleaved from line 2104 while the wiring sits at 2300, so the
        // guard silently measured a truncated file and reported the wiring
        // missing. A guard that strips more than it means to is the same class
        // of bug it is here to catch. (That blind spot still applies to the
        // sibling guard for any file with a mid-file test module.)
        let code: String = events
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            code.contains("certified_fold::rebuild_or_skip"),
            "the skip must be wired into the orchestrator's per-drive rebuild \
             (`handlers::events::rebuild_state`) — that is the refold that \
             actually runs. Wiring it anywhere with no caller ships an inert \
             feature, which is what noetl/server#484 did."
        );

        // `rebuild_state(` appears for its own definition plus the
        // `rebuild_state_uncached` wrapper call; real callers push it higher.
        // Too few means nothing calls it and the skip can never fire.
        let mentions = code.matches("rebuild_state(").count();
        assert!(
            mentions > 2,
            "`rebuild_state` is wired for the skip but has NO CALLERS \
             ({mentions} mention, the definition), so the skip can never fire. \
             This is the noetl/server#484 defect."
        );
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
