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
    crate::metrics::record_chain_cert_observed(1);
    let mut evicted = Vec::new();
    if let Ok(mut c) = cache().lock() {
        c.observe_event(execution_id, event_id, event_type, node_name, status);
        crate::metrics::set_chain_cert_tracked(c.tracked_executions() as i64);
        // An execution that just ended will never fold again, so drop it now
        // rather than waiting for the bound to push it out. Uses the existing
        // classifier (`db::queries::event_chain::TERMINAL_EVENT_TYPES`) rather
        // than a second list, which would drift from it.
        if crate::db::queries::event_chain::is_terminal_event_type(event_type) {
            c.forget(execution_id);
            evicted.push(execution_id);
        }
        evicted.extend(c.take_evicted());
    }
    // Mirror the eviction into the parallel rebuild cache, or it outlives the
    // certificate cache and the bound buys nothing.
    if !evicted.is_empty() {
        if let Some(drop_fn) = REBUILD_EVICTOR.get() {
            for id in evicted {
                drop_fn(id);
            }
        }
    }
}

/// How this module drops an entry from the server's rebuild cache.
///
/// A function pointer rather than a direct call because `RebuildResult` is
/// private to `handlers::events`; that module registers its evictor at startup.
/// Without this, bounding the certificate cache would leave the rebuild cache —
/// the one holding whole `WorkflowState` values — growing unbounded anyway.
static REBUILD_EVICTOR: OnceLock<fn(i64)> = OnceLock::new();

/// Register the rebuild cache's eviction hook. Idempotent; first call wins.
pub fn register_rebuild_evictor(f: fn(i64)) {
    let _ = REBUILD_EVICTOR.set(f);
}

/// The `/metrics` label for a decision. Must cover every variant, or an
/// outcome lands in no series and the ramp cannot see it.
fn outcome_label(d: FoldDecision) -> &'static str {
    match d {
        FoldDecision::SkipValid => "skipped",
        FoldDecision::RefoldNoCachedState => "refold_no_cached_state",
        FoldDecision::RefoldNoCertificate => "refold_no_certificate",
        FoldDecision::RefoldChainAdvanced => "refold_chain_advanced",
        FoldDecision::RefoldCachedAhead => "refold_cached_ahead",
        FoldDecision::RefoldDigestMismatch => "refold_digest_mismatch",
        FoldDecision::RefoldUncalibrated => "refold_uncalibrated",
        FoldDecision::RefoldRevalidationDue => "refold_revalidation_due",
        FoldDecision::RefoldDivergenceDetected => "refold_divergence_detected",
    }
}

/// Whether a fold for `execution_id` can be skipped. Never skips with the flag
/// off, and never skips on a poisoned lock.
pub fn decide(execution_id: i64) -> FoldDecision {
    let d = match cache().lock() {
        Ok(c) => c.decide_for(execution_id),
        Err(_) => FoldDecision::RefoldNoCachedState,
    };
    // Exported, not just counted in-process: a ramp cannot read an atomic, and
    // `skipped` is the only thing that distinguishes "working" from "enabled
    // and inert".
    if chain_cert::fold_skip_enabled() {
        crate::metrics::record_chain_cert_decision(outcome_label(d));
    }
    d
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
    let ok = match cache().lock() {
        Ok(mut c) => c.reconcile_head(execution_id, actual_head),
        Err(_) => false,
    };
    if !ok {
        // THE rollback signal. Exported so a ramp can alert on it.
        crate::metrics::record_chain_cert_divergence();
    }
    ok
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

    /// A terminal event must drop the execution from BOTH caches.
    ///
    /// `forget` had no callers at all, so with the flag on every execution the
    /// process ever saw kept chain state — and the rebuild cache kept a whole
    /// `WorkflowState` beside it — for the process lifetime. On a server that
    /// runs for days that is a leak, and it would have surfaced during the ramp
    /// rather than in a test.
    #[tokio::test]
    async fn a_terminal_event_drops_the_execution_from_both_caches() {
        let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NOETL_CHAIN_CERT", "1");
        chain_cert::reset_counters();
        let exec = 90_010;
        forget(exec);

        observe_n(exec, 16);
        let runs = AtomicUsize::new(0);
        let _ = run_once(exec, 10_015, &runs).await.unwrap();
        assert_eq!(runs.load(AtOrd::Relaxed), 1);
        // Cached, so the next call would skip.
        let _ = run_once(exec, 10_015, &runs).await.unwrap();
        assert_eq!(
            runs.load(AtOrd::Relaxed),
            1,
            "precondition: it was skipping"
        );

        // The execution ends. Uses the real classifier, not a second list.
        assert!(
            crate::db::queries::event_chain::is_terminal_event_type("playbook.completed"),
            "fixture is vacuous unless this really is a terminal type"
        );
        observe(
            exec,
            10_016,
            "playbook.completed",
            Some("playbook"),
            "success",
        );

        // Chain state is gone, so a further call must rebuild rather than skip.
        let _ = run_once(exec, 10_016, &runs).await.unwrap();
        assert_eq!(
            runs.load(AtOrd::Relaxed),
            2,
            "after a terminal event the execution must be forgotten, so the next \
             call rebuilds instead of serving cached state"
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

        // ⚠ And the call must be REACHED, not merely present.
        //
        // Re-running the D10 control on merged main showed the weakness: a plant
        // that leaves `rebuild_or_skip` in the file and bypasses it with an
        // early `return rebuild_state_uncached(..)` passed the check above,
        // because a text search cannot see control flow. So require the skip to
        // be the FIRST statement of `rebuild_state`'s body — nothing may return
        // ahead of it.
        let body = code
            .split("async fn rebuild_state(")
            .nth(1)
            .and_then(|t| t.split_once(") -> AppResult<RebuildResult> {"))
            .map(|(_, rest)| rest.split("\nasync fn ").next().unwrap_or(rest))
            .unwrap_or("");
        assert!(
            !body.is_empty(),
            "could not locate `rebuild_state`'s body — the guard's parse is stale, \
             fix the guard rather than deleting it"
        );
        let first_stmt = body
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty() && !l.starts_with('#'))
            .unwrap_or("");
        assert!(
            first_stmt.contains("certified_fold::rebuild_or_skip"),
            "`rebuild_state` must reach the skip before anything else, but its \
             first statement is `{first_stmt}`. A `rebuild_or_skip` call that is \
             present in the file but jumped over is an inert feature that passes \
             a text check — which is how the D10 control slipped through on \
             merged main."
        );
    }

    /// The rebuild cache must register its eviction hook.
    ///
    /// Added because the D13 control — deleting the `register_rebuild_evictor`
    /// call — broke NOTHING. The terminal-eviction test above uses its own map,
    /// so it cannot see `REBUILD_CACHE`, and nothing else looked. Without the
    /// registration the certificate cache bounds itself while the rebuild cache
    /// (a whole `WorkflowState` per execution) grows unbounded anyway, which is
    /// most of the leak this work set out to close.
    ///
    /// Static, like the reachability guard, and for the same reason: the hook is
    /// installed lazily inside `rebuild_cache()`, and a `OnceLock` means a test
    /// cannot reliably install a competing one.
    #[test]
    fn the_rebuild_cache_registers_its_eviction_hook() {
        let events = include_str!("../handlers/events.rs");
        let code: String = events
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            code.contains("register_rebuild_evictor("),
            "`handlers::events` must register its rebuild-cache evictor with \
             `certified_fold`, or bounding the certificate cache leaves the \
             rebuild cache — which holds a whole WorkflowState per execution — \
             growing without limit."
        );
        assert!(
            code.contains("fn forget_rebuild("),
            "the registered evictor function is missing"
        );
    }

    /// ⚠ What the reachability guard above CANNOT prove.
    ///
    /// It is static analysis. It establishes that the skip is wired into a
    /// function with live callers and that nothing returns ahead of it. It does
    /// NOT execute `rebuild_state`, because that needs a database, so it cannot
    /// prove the skip fires against real traffic.
    ///
    /// That residual gap is covered by `the_wired_skip_actually_avoids_the_second_rebuild`
    /// (the mechanism skips) plus the ramp's own `skipped` counter, which is the
    /// only thing that proves it on real data. **A ramp whose `skipped` stays
    /// zero means the feature is inert, not that it is safe** — that is the
    /// reading to watch for, and it is recorded here because it is the exact
    /// mistake noetl/server#484 would have produced.
    #[test]
    fn the_runtime_proof_is_the_ramps_skipped_counter() {
        // The counters must exist and be readable, or the ramp has no way to
        // tell "working" from "inert".
        let (_observed, _decided, _skipped, _refolded, _div) = counters();
        // And `divergences` must be separately readable as the rollback signal.
        let _ = divergences();
    }

    /// Every decision outcome must have a PINNED series, or it is invisible.
    ///
    /// Two ways an outcome disappears from `/metrics`, both of which make a ramp
    /// unreadable: a variant whose label is missing from
    /// `metrics::CHAIN_CERT_OUTCOMES` is never pinned, so its family is pruned
    /// until it first fires — and "absent" then reads identically to "zero",
    /// which have opposite meanings here. This is the same trap
    /// `init_materialize_outcome_series` was written for.
    #[test]
    fn every_decision_outcome_has_a_pinned_series() {
        let all = [
            FoldDecision::SkipValid,
            FoldDecision::RefoldNoCachedState,
            FoldDecision::RefoldNoCertificate,
            FoldDecision::RefoldChainAdvanced,
            FoldDecision::RefoldCachedAhead,
            FoldDecision::RefoldDigestMismatch,
            FoldDecision::RefoldUncalibrated,
            FoldDecision::RefoldRevalidationDue,
            FoldDecision::RefoldDivergenceDetected,
        ];
        for d in all {
            let label = outcome_label(d);
            assert!(
                crate::metrics::CHAIN_CERT_OUTCOMES.contains(&label),
                "{d:?} maps to label `{label}`, which is not in \
                 CHAIN_CERT_OUTCOMES — so it is never pinned, its series is \
                 pruned until it first fires, and a ramp cannot tell `absent` \
                 from `zero`"
            );
        }
        // And the pinned set must not carry labels nothing produces, which would
        // read as a permanently-zero outcome that cannot happen.
        let produced: Vec<&str> = all.iter().map(|d| outcome_label(*d)).collect();
        for pinned in crate::metrics::CHAIN_CERT_OUTCOMES {
            assert!(
                produced.contains(pinned),
                "`{pinned}` is pinned but no FoldDecision produces it"
            );
        }
    }

    /// `skipped` on its own cannot tell "inert" from "off" — so the denominator
    /// must be exported too.
    #[test]
    fn the_inert_vs_off_discriminator_is_exported() {
        crate::metrics::init_chain_cert_series();
        let text = crate::metrics::gather_text().expect("metrics render");
        for required in [
            "noetl_chain_cert_decisions_total",
            "noetl_chain_cert_observed_total",
            "noetl_chain_cert_divergences_total",
        ] {
            assert!(
                text.contains(required),
                "`{required}` is not registered. Without `observed` beside \
                 `skipped`, a zero `skipped` cannot be told from the flag being \
                 off — which is the reading that would mistake an inert feature \
                 for a clean ramp."
            );
        }
        // And `skipped` specifically must be present at zero, not pruned.
        assert!(
            text.contains("noetl_chain_cert_decisions_total{outcome=\"skipped\"}"),
            "the `skipped` series must be PINNED and visible at zero; pruned, a \
             ramp reading no series would conclude the wrong thing"
        );
    }

    /// The server must pin the series AT STARTUP, not only in tests.
    ///
    /// Added because control D15 — deleting `init_chain_cert_series()` from
    /// `main.rs` — broke nothing: `the_inert_vs_off_discriminator_is_exported`
    /// calls `init_chain_cert_series()` itself, so it proves the function works,
    /// not that the server invokes it. Unpinned in the real binary, the families
    /// are pruned until they first fire, and a ramp reading no `skipped` series
    /// cannot tell that from a zero.
    ///
    /// ⚠ This is the THIRD time this exact shape has appeared here (D13 the
    /// rebuild evictor, D15 this): a startup registration that tests bypass by
    /// performing it themselves. A test that sets up the thing it is checking
    /// cannot check that anyone else sets it up.
    #[test]
    fn the_server_pins_the_series_at_startup() {
        let main_rs = include_str!("../main.rs");
        let code: String = main_rs
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            code.contains("init_chain_cert_series()"),
            "`main.rs` must call `metrics::init_chain_cert_series()` at startup. \
             Without it the chain-certificate families are pruned from /metrics \
             until they first fire, so a ramp cannot distinguish `absent` from \
             `zero` — which is the whole reason these metrics exist."
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
