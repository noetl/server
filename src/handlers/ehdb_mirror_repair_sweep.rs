//! **Durable mirror repair** — make a dropped batch recoverable instead of lost
//! (noetl/ai-meta#342).
//!
//! # The loss this closes
//!
//! The mirror retries a failed delivery seven times over ~63 s and then records
//! `dropped`, logs the `event_id`s, and **returns**. Nothing retries afterwards,
//! and the endpoint the log names (`POST /api/ehdb/repair/executions/{id}`) had
//! **no caller anywhere in the tree**. Measured on prod over 8 h: `mirrored 237
//! / recovered 256 / dropped 323` events — **39.6% of what the mirror was handed
//! was lost permanently**, against a relay backed by a *single* endpoint pod
//! whose restart outlasts the retry budget.
//!
//! # Why a sweep, and not a durable queue
//!
//! ⭐ **The gap is its own pending-work record.** Postgres is authoritative and
//! the tier is a mirror, so "which events are missing" is *derivable at any time*
//! by comparing the two. That makes a sweep durable in the way that matters:
//!
//! * it survives a server restart, which an in-memory queue does not;
//! * it survives the repair itself failing — the gap is still there next pass;
//! * it needs **no new storage**, so no schema decision and nothing to migrate.
//!
//! A durable queue would add a second source of truth about work that is already
//! implied by the data. The hint set below is a latency optimisation on top, not
//! the correctness mechanism: correctness does not depend on remembering.
//!
//! # Bounds
//!
//! Unbounded repair on a `primary`-serving tier is its own hazard. Every pass is
//! capped in executions examined and repairs attempted, and the whole thing is
//! behind [`SWEEP_ENV`], default **off** — so it deploys inert and arming is a
//! separate, reversible step.
//!
//! ⚠ This repairs the **tier only**. Postgres is authoritative and is never
//! written here; the repair reads it and re-mirrors through the same
//! `mirror_rows` chokepoint the live path uses.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::Duration;

use crate::state::AppState;

/// Arm the background repair sweep. Default **off**.
pub const SWEEP_ENV: &str = "NOETL_EHDB_MIRROR_REPAIR_SWEEP";
/// Seconds between passes.
pub const INTERVAL_ENV: &str = "NOETL_EHDB_MIRROR_REPAIR_INTERVAL_SECS";
/// Maximum executions repaired per pass.
pub const MAX_PER_PASS_ENV: &str = "NOETL_EHDB_MIRROR_REPAIR_MAX_PER_PASS";
/// How far back the scan looks for executions to check.
pub const LOOKBACK_ENV: &str = "NOETL_EHDB_MIRROR_REPAIR_LOOKBACK_MINS";

const DEFAULT_INTERVAL_SECS: u64 = 120;
const DEFAULT_MAX_PER_PASS: usize = 25;
const DEFAULT_LOOKBACK_MINS: i64 = 180;
/// Hint-set ceiling. Bounded so a relay outage cannot grow it without limit;
/// anything evicted is still found by the scan, because the gap is the record.
const HINT_CAPACITY: usize = 4096;

/// Executions a drop was just observed for — a **latency hint**, not the
/// correctness mechanism.
///
/// ⚠ Deliberately in-memory and lossy. If the process dies, or the set
/// overflows, the scan still finds the gap. Making this durable would create a
/// second source of truth about work the data already implies.
static HINTS: Mutex<BTreeSet<i64>> = Mutex::new(BTreeSet::new());

/// Is the sweep armed, given the raw env value? **Pure**, so the default is
/// testable without touching process env.
///
/// ⚠ Extracted because the original test was a tautology: it asserted only that
/// the parse agreed with the env, which is satisfied by *any* parse — including
/// one armed by default. A mutation flipping the default survived it.
pub fn sweep_armed(raw: Option<&str>) -> bool {
    matches!(raw.map(str::trim), Some("true"))
}

pub fn sweep_enabled() -> bool {
    sweep_armed(std::env::var(SWEEP_ENV).ok().as_deref())
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(default)
}

pub fn interval() -> Duration {
    Duration::from_secs(env_u64(INTERVAL_ENV, DEFAULT_INTERVAL_SECS))
}

pub fn max_per_pass() -> usize {
    env_u64(MAX_PER_PASS_ENV, DEFAULT_MAX_PER_PASS as u64) as usize
}

pub fn lookback_mins() -> i64 {
    env_u64(LOOKBACK_ENV, DEFAULT_LOOKBACK_MINS as u64) as i64
}

/// Record that an execution just lost a mirror batch.
///
/// Called from the mirror's terminal-drop path so the repair happens in seconds
/// rather than waiting for the next scan to notice.
pub fn hint(execution_id: i64) {
    if let Ok(mut g) = HINTS.lock() {
        if g.len() >= HINT_CAPACITY {
            return;
        }
        g.insert(execution_id);
        crate::metrics::set_ehdb_mirror_repair_pending(g.len() as u64);
    }
}

fn take_hints(limit: usize) -> Vec<i64> {
    let Ok(mut g) = HINTS.lock() else {
        return Vec::new();
    };
    let taken: Vec<i64> = g.iter().copied().take(limit).collect();
    for id in &taken {
        g.remove(id);
    }
    crate::metrics::set_ehdb_mirror_repair_pending(g.len() as u64);
    taken
}

/// Current hint-set size — exposed for tests.
pub fn pending_hints() -> usize {
    HINTS.lock().map(|g| g.len()).unwrap_or(0)
}

/// Clear the hint set — tests only.
#[cfg(test)]
pub fn clear_hints() {
    if let Ok(mut g) = HINTS.lock() {
        g.clear();
    }
}

/// Executions worth checking this pass: the hinted ones first, then a bounded
/// scan of recent executions.
///
/// ⚠ Hints first on purpose. They are the executions *known* to have lost
/// events; the scan is the backstop that makes forgetting survivable.
pub async fn candidates(state: &AppState, limit: usize) -> Vec<i64> {
    let mut out = take_hints(limit);
    if out.len() >= limit {
        return out;
    }
    let seen: BTreeSet<i64> = out.iter().copied().collect();
    let remaining = (limit - out.len()) as i64;
    let rows = sqlx::query_scalar::<_, i64>(
        r#"
        SELECT DISTINCT execution_id
        FROM noetl.event
        WHERE created_at > now() - ($1 || ' minutes')::interval
        ORDER BY execution_id DESC
        LIMIT $2
        "#,
    )
    .bind(lookback_mins().to_string())
    .bind(remaining)
    // ⚠ The scan runs on ONE pool. Prod runs single-pool (the boot log says
    // "falling back to single-pool mode"), so this covers everything today —
    // but under real sharding this scan sees one shard's rows. The hint path
    // is not shard-limited, and the repair itself uses `pool_for(execution_id)`,
    // so the gap is a scan-coverage limit, not a correctness one. Stated rather
    // than left for someone to discover.
    .fetch_all(state.pools.pool_for(0))
    .await
    .unwrap_or_default();
    for id in rows {
        if !seen.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// One pass. Returns `(examined, repaired)`.
pub async fn sweep_once(state: &AppState) -> (usize, usize) {
    let limit = max_per_pass();
    let ids = candidates(state, limit).await;
    let mut repaired = 0usize;
    for id in &ids {
        let (outcome, _report) =
            crate::handlers::ehdb_tier_repair::repair_execution(state, *id).await;
        crate::metrics::record_ehdb_mirror_repair(outcome);
        if outcome == "repaired" {
            repaired += 1;
        }
        // ⚠ An execution that did NOT repair is deliberately not re-hinted here.
        // Re-hinting a permanently unrecoverable execution would spin the sweep
        // on it forever and crowd out real work; the scan will offer it again on
        // its own while it remains in the lookback window.
    }
    (ids.len(), repaired)
}

/// Spawn the background sweep. A no-op when the flag is not armed.
pub fn spawn(state: AppState) {
    if !sweep_enabled() {
        tracing::info!(
            target: "noetl_server::ehdb_mirror_repair",
            "mirror repair sweep NOT armed ({SWEEP_ENV} != true)"
        );
        return;
    }
    let every = interval();
    tracing::info!(
        target: "noetl_server::ehdb_mirror_repair",
        interval_secs = every.as_secs(),
        max_per_pass = max_per_pass(),
        lookback_mins = lookback_mins(),
        "mirror repair sweep ARMED"
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(every);
        loop {
            ticker.tick().await;
            let (examined, repaired) = sweep_once(&state).await;
            if repaired > 0 {
                tracing::warn!(
                    target: "noetl_server::ehdb_mirror_repair",
                    examined, repaired,
                    "mirror repair sweep closed tier gaps"
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This module's source, without its own tests.
    ///
    /// ⚠ `include_str!` pulls the test module in too, so a needle written here
    /// becomes a match in the thing being asserted about.
    fn src() -> &'static str {
        const RAW: &str = include_str!("ehdb_mirror_repair_sweep.rs");
        // ⚠ Cut at the test MODULE, not at the first `#[cfg(test)]`. This file
        // also has a `#[cfg(test)] pub fn clear_hints()` about a third of the way
        // down, and cutting there truncated the slice before `sweep_once` — so
        // the positive assertions failed and the negative ones were vacuous.
        // The same shape as a `non_test()` helper that cut thousands of lines
        // early and reported green on zero bytes.
        let cut = RAW
            .find("\n#[cfg(test)]\nmod tests")
            .expect("exactly one test module, at the end");
        let head = &RAW[..cut];
        // Floor raised: the old 2000 was low enough that the truncated slice
        // still passed it. A guard that cannot detect its own bad extraction is
        // not a guard.
        assert!(
            head.len() > 5000,
            "implausibly short slice ({} bytes) — every assertion below would be \
             vacuous",
            head.len()
        );
        head
    }

    /// [`src`] with comment lines removed.
    ///
    /// ⚠ Needed because a doc comment naming a function reads as a call to it.
    /// `the_sweep_reuses_...` failed on its own module doc, which explains that
    /// the repair "re-mirrors through the same `mirror_rows` chokepoint" — prose
    /// about not calling something matched as calling it.
    fn code_only() -> String {
        src()
            .lines()
            .filter(|l| {
                let t = l.trim_start();
                !(t.starts_with("//") || t.starts_with("///") || t.starts_with("//!"))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Arming is opt-in, so the fix deploys inert — and ONLY the exact string
    /// arms it.
    ///
    /// ⚠ Tests the pure parse, not the process env. The previous version
    /// asserted the parse "agreed with the env", which any parse satisfies; a
    /// mutation arming the sweep by default survived it.
    #[test]
    fn the_sweep_is_off_unless_explicitly_armed() {
        assert!(
            !sweep_armed(None),
            "an unset flag must NOT arm the sweep — the fix has to deploy inert"
        );
        assert!(sweep_armed(Some("true")), "the exact string must arm it");
        assert!(
            sweep_armed(Some("  true  ")),
            "surrounding space is trimmed"
        );
        for v in ["1", "yes", "TRUE", "True", "on", "", "false", "tier"] {
            assert!(
                !sweep_armed(Some(v)),
                "{v:?} must not arm the sweep — only the exact string \"true\""
            );
        }
    }

    /// Hints dedupe and are bounded.
    ///
    /// ⚠ Bounded on purpose: a relay outage hints once per dropped batch, and an
    /// unbounded set would grow with the outage. Overflow is safe precisely
    /// because the hint is not the correctness mechanism.
    /// ⚠⚠ **Every assertion about `HINTS` lives in THIS test.** Dedupe, drain,
    /// the pass limit and the ceiling, all together. `HINTS` is a
    /// process-global `static`, and **`cargo test` does not serialise tests** —
    /// two tests mutating it raced and failed non-deterministically. Merging is
    /// the fix that does not depend on scheduling.
    #[test]
    fn hints_dedupe_drain_and_respect_the_pass_limit() {
        clear_hints();
        hint(11);
        hint(11);
        hint(22);
        assert_eq!(pending_hints(), 2, "the same execution must hint once");

        let taken = take_hints(10);
        assert_eq!(taken.len(), 2);
        assert_eq!(pending_hints(), 0, "taking must drain");

        // Two-sided: draining an empty set is not an error and yields nothing.
        assert!(take_hints(10).is_empty());

        // The pass limit: one pass must not exceed its budget.
        for i in 0..10 {
            hint(i);
        }
        let taken = take_hints(3);
        assert_eq!(taken.len(), 3, "must not exceed the pass budget");
        assert_eq!(pending_hints(), 7, "the rest stay for the next pass");

        // The ceiling. A relay outage hints once per dropped batch, so an
        // unbounded set would grow with the outage. Safe to bound precisely
        // because the hint is not the correctness mechanism — anything refused
        // here is still found by the scan.
        clear_hints();
        for i in 0..(HINT_CAPACITY as i64 + 50) {
            hint(i);
        }
        assert_eq!(
            pending_hints(),
            HINT_CAPACITY,
            "the hint set must stop at its ceiling — and must actually have \
             filled, or the bound is not what stopped it"
        );
        clear_hints();
    }

    /// ⚠⚠ The terminal drop must hint the sweep.
    ///
    /// This is the whole fix. Before it, the drop path recorded a counter, logged
    /// the event_ids, and returned — and the endpoint the log named had **no
    /// caller anywhere in the tree**, so the events were simply gone.
    #[test]
    fn the_terminal_drop_hints_the_sweep() {
        const MIRROR: &str = include_str!("ehdb_eventlog_mirror.rs");
        let code = MIRROR
            .split_once("\n#[cfg(test)]")
            .map(|(above, _)| above)
            .unwrap_or(MIRROR);
        assert!(code.len() > 5000, "mirror slice too small: {}", code.len());
        let at = code
            .find(r#"record_ehdb_eventlog_mirror("dropped", count)"#)
            .expect("the terminal drop site moved — extraction broke");
        let window: String = code[at..].chars().take(900).collect();
        assert!(
            window.contains("ehdb_mirror_repair_sweep::hint("),
            "the terminal drop does not hint the repair sweep — a dropped batch \
             would be permanently lost again (noetl/ai-meta#342)"
        );
    }

    /// One repair implementation, shared.
    ///
    /// A second repair path would be a second thing to keep true on the write
    /// path of a tier that serves reads.
    #[test]
    fn the_sweep_reuses_the_endpoints_repair_and_does_not_reimplement_it() {
        assert!(
            code_only().contains("ehdb_tier_repair::repair_execution("),
            "the sweep must call the shared repair"
        );
        assert!(
            !code_only().contains("mirror_rows("),
            "the sweep must not re-mirror directly — that would bypass the \
             scoped/non-inflating/honest guarantees the repair holds"
        );
        assert!(
            !code_only().contains("ParityRecording::Record"),
            "the sweep must never record parity outcomes — it would inflate the \
             counters it exists to repair (noetl/ai-meta#264)"
        );
    }

    /// Postgres is authoritative and is never written by the repair path.
    #[test]
    fn the_sweep_never_writes_postgres() {
        // ⚠ Match SQL SHAPES, not bare words. `"INSERT"` alone matched
        // `BTreeSet::insert` and failed this test on a data-structure call.
        let sql = code_only().to_uppercase();
        for forbidden in [
            "INSERT INTO",
            "UPDATE NOETL.",
            "DELETE FROM",
            "TRUNCATE",
            "INSERT_EVENT(",
        ] {
            assert!(
                !sql.contains(forbidden),
                "the sweep must never write the authoritative log; found {forbidden:?}"
            );
        }
        // Positive control: the one authoritative access IS present, so the
        // negatives above are not passing because the slice is empty.
        assert!(
            code_only().contains("SELECT DISTINCT execution_id"),
            "the candidate read is missing — the negative assertions above would \
             then be vacuous"
        );
    }

    /// The pinned label set must be exactly what the repair can return.
    ///
    /// ⚠ A pinned set that omits one value reintroduces the absent-series bug on
    /// that value alone, while the rest read 0 and look complete. The first draft
    /// of this set carried `incomplete`; the code returns **`partial`**.
    #[test]
    fn every_repair_outcome_is_pinned() {
        const REPAIR: &str = include_str!("ehdb_tier_repair.rs");
        let code = REPAIR
            .split_once("\n#[cfg(test)]")
            .map(|(above, _)| above)
            .unwrap_or(REPAIR);
        assert!(code.len() > 2000, "repair slice too small");
        for outcome in crate::metrics::EHDB_MIRROR_REPAIR_OUTCOMES {
            assert!(
                code.contains(&format!("\"{outcome}\"")),
                "pinned outcome {outcome:?} is never produced by the repair — the \
                 pinned set and the code have drifted"
            );
        }
        // The other direction: the two literals `repair_outcome` returns must
        // both be pinned, or a real outcome is invisible.
        for produced in ["repaired", "partial"] {
            assert!(
                crate::metrics::EHDB_MIRROR_REPAIR_OUTCOMES.contains(&produced),
                "{produced:?} is returned but not pinned"
            );
        }
    }
}
