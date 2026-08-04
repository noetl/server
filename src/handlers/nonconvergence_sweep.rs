//! Systemic non-convergence sweep (refs
//! [#227](https://github.com/noetl/ai-meta/issues/227) part B).
//!
//! ## What this is for, and why the orphan sweep is not enough
//!
//! [`crate::handlers::orphan_sweep`] (#171) terminates exactly one shape: a
//! command that was **claimed** by a worker which then died.  That is the
//! zombie it was built for, and it handles it correctly.  It is blind to every
//! other way an execution stops converging, for two structural reasons:
//!
//! 1. its predicate requires an outstanding `command.claimed`, and
//! 2. its candidate scan is bounded to a 48h lookback.
//!
//! A census of `shastaratech-noetl-prod` on 2026-08-04 found **3359**
//! executions with a start event and no terminal event, the oldest 160 days
//! old.  Sampling them turns up four unrelated causes, only one of which the
//! orphan sweep can even see:
//!
//! | shape | last event | orphan sweep sees it? |
//! | :-- | :-- | :-- |
//! | all branches evaluated false, no successor issued | `step.skipped` | no — no claim |
//! | command issued, never claimed | `command.issued` | no — no claim |
//! | already cancelled, mis-projected as RUNNING | `execution.cancelled` | no — and must not |
//! | history bulk-imported from another cluster | anything | no — and never will |
//!
//! That last row is the largest cohort: 1699 of the 3359 share a single
//! `ingest_time` to the microsecond (2026-05-18 04:31:47.130085), the
//! `noetl-demo-19700101` → `shastaratech-noetl-prod` migration loading 570,623
//! events in one transaction.  Those executions were already non-terminal in
//! the *source* cluster.  Importing an event log cannot resurrect an in-flight
//! execution — there is no command, no worker, no bus entry, only history — so
//! they are non-convergent by construction and no runtime repair will ever move
//! them.
//!
//! ## The predicate: progress, not shape
//!
//! Because the causes are unrelated, keying on any one of them would clear
//! almost none of the backlog.  This sweep keys on the one thing they share:
//! **the execution's watermark has stopped moving and nothing can move it.**
//!
//! An execution is eligible only when **all** of the following hold.  Each is a
//! separate `AND`; there is no scoring, no heuristic weighting, no "two out of
//! three".
//!
//! 1. **It has started and has no terminal event** — no `playbook.completed`,
//!    `playbook.failed`, `playbook.cancelled` (or their legacy `playbook_*`
//!    spellings).
//! 2. **Its watermark is stale** — the newest event of *any* type on the
//!    execution is older than `nonconvergence_grace_secs` (default 24h).  This
//!    is the forward-progress signal: an execution that emitted anything at all
//!    recently is, by definition, converging.
//! 3. **No live worker holds it** — if the execution has an outstanding
//!    `command.claimed` (no matching `command.completed` / `command.failed`)
//!    whose owner is in the live set, it is skipped unconditionally.  This is
//!    #171's "never fail live work" guard, reused verbatim and *not* relaxed.
//! 4. **It is not parked awaiting an external callback** — see §Callbacks.
//! 5. **It was not already cancelled** — an execution carrying
//!    `execution.cancelled` is left alone; it needs the status projection
//!    fixed, not a `playbook.failed` written over the top of a deliberate
//!    cancellation.
//!
//! ## Callbacks: the case that would otherwise be terminated wrongly
//!
//! The execution model explicitly allows a step to park indefinitely while an
//! external system works (`agents/rules/execution-model.md`, "Callback / hook
//! rule").  On that path the worker returns and frees its slot *without*
//! emitting `call.done`; the terminal arrives later from
//! `POST /api/internal/container-callback/…`.
//!
//! From the event log alone, a healthy execution parked on a 6-hour callback is
//! **indistinguishable** from one whose DAG ran out of successors: both have a
//! `command.completed`, no outstanding claim, and an arbitrarily old watermark.
//! Condition 3 does not save it, because there is no claim to check.  A naive
//! staleness predicate would terminate healthy work here — this is the sweep's
//! sharpest edge.
//!
//! So the worker now stamps `pending_callback: true` into the `command.completed`
//! it emits on that path (`repos/worker/src/executor/command.rs`), and condition
//! 4 excludes any execution whose newest `command.completed` carries the marker
//! with no `call.done` after it.  A positive signal, not an inference.
//!
//! Executions that predate the marker cannot carry it.  That is acceptable
//! today and the reason is measurable, not hopeful: prod contains **zero**
//! events mentioning `pending_callback` and one mentioning a container job
//! handle, because `Tool::Container` is the only producer and it is effectively
//! unused (see [#186](https://github.com/noetl/ai-meta/issues/186)).  Should
//! that change, the marker is already in place for everything emitted from now
//! on.
//!
//! ## What this sweep deliberately does NOT do
//!
//! * It does not terminate an execution held by a live worker.  The user-facing
//!   ask was to distinguish "held by a live worker but provably stuck" from
//!   "held by a live worker and progressing".  **That is not provable today.**
//!   The only per-command progress signal that ever existed,
//!   `command.heartbeat`, was emitted by the retired Python worker; the newest
//!   one on prod is dated 2026-05-23 and the Rust worker emits none.  Worker
//!   liveness in `noetl.runtime` is pool-level, not command-level, so it cannot
//!   answer the question either.  Rather than approximate a proof with a
//!   timeout, the timeout is exposed as `nonconvergence_stuck_claim_secs`,
//!   **default 0 = off**, and documented as a timeout.  Reinstating a
//!   per-command progress signal is the honest prerequisite.
//! * It does not re-queue anything.  Like #171 it terminates, so no
//!   side-effecting step is ever re-executed.
//! * It does not delete or update a single row.  `noetl.event` is append-only;
//!   the sweep appends one `playbook.failed` and nothing else.
//!
//! ## Performance
//!
//! The hard requirement is **zero added synchronous cost on the hot path**.
//! Nothing here touches publish, append, claim, ack or dispatch: it is a
//! background task on its own interval (default 300s), exactly like #171.
//!
//! The candidate query is written candidate-first, the shape #62 established:
//! stage `starts` picks executions from the `event_type` index (~11k start
//! events, not the 966k-row table), the terminal-existence check rides
//! `idx_event_exec_type (execution_id, event_type, event_id DESC)`, and the
//! per-execution watermark / claim / callback lookups are `LATERAL` probes over
//! `idx_event_execution_id` on an already-bounded candidate set.  There is no
//! aggregate over the full table.
//!
//! ## Safety posture
//!
//! Flag-gated, **default off** (`NOETL_NONCONVERGENCE_SWEEP_ENABLED`); the task
//! spawns, logs once and returns, so default behaviour is byte-identical.
//! Rate-limited per tick.  Affinity-filtered so only the owning replica acts.
//! Terminates through [`crate::handlers::event_write::emit_event`], so #103
//! sole-writer ordering and #118 idempotent-terminal dedup both hold and two
//! replicas racing the same execution collapse to one terminal.  No state is
//! carried between ticks, so rollback is flipping the flag.

use std::collections::HashSet;

use tracing::{info, warn};

use crate::db::DbPool;
use crate::error::AppResult;
use crate::state::AppState;

/// One execution whose watermark has stopped moving, with everything the
/// disposition decision needs.  Assembled by [`query_nonconvergence_candidates`];
/// judged by [`plan_dispositions`], which is pure.
#[derive(Debug, Clone)]
pub(crate) struct StalledExecution {
    pub execution_id: i64,
    pub catalog_id: i64,
    /// Newest event on the execution — the causal parent of the terminal
    /// `playbook.failed`, so the chain stays intact.
    pub last_event_id: i64,
    /// Type of that newest event, purely for the operator-facing reason string.
    pub last_event_type: String,
    /// Age of that newest event in seconds at query time.
    pub stalled_secs: i64,
    /// Owner of an outstanding (unfinished) `command.claimed`, if any.
    pub claim_worker_id: Option<String>,
    /// Age of that outstanding claim in seconds, if any.
    pub claim_age_secs: Option<i64>,
    /// The execution's newest `command.completed` carries `pending_callback`
    /// and no `call.done` follows it — parked by design, not stalled.
    pub awaiting_callback: bool,
}

/// The grace period actually used, never below
/// [`crate::config::MIN_NONCONVERGENCE_GRACE_SECS`].
///
/// Applied at both the log site and the query site so the number an operator
/// reads in the startup line is the number the predicate uses.
fn effective_grace_secs(configured: u64) -> u64 {
    configured.max(crate::config::MIN_NONCONVERGENCE_GRACE_SECS)
}

/// Fate of one candidate this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
    /// Nothing can move it → emit `playbook.failed`.
    Terminate,
    /// An outstanding command is held by a live worker → in-flight work.
    SkippedLive,
    /// Parked on an external callback by design → not stalled.
    SkippedAwaitingCallback,
    /// Eligible, but this tick's termination budget is spent.
    Capped,
}

impl Disposition {
    fn metric_label(self) -> &'static str {
        match self {
            Disposition::Terminate => "terminated",
            Disposition::SkippedLive => "skipped_live",
            Disposition::SkippedAwaitingCallback => "skipped_awaiting_callback",
            Disposition::Capped => "capped",
        }
    }
}

/// Spawn the background non-convergence sweep.  Safe to spawn unconditionally:
/// while the flag is off the task logs once and returns, scanning nothing.
pub fn spawn_nonconvergence_sweep(state: AppState) {
    tokio::spawn(async move {
        if !state.config.nonconvergence_sweep_enabled {
            info!(
                target: "noetl_server::nonconvergence_sweep",
                "non-convergence sweep: disabled (NOETL_NONCONVERGENCE_SWEEP_ENABLED=false) — not scanning"
            );
            return;
        }
        let interval =
            std::time::Duration::from_secs(state.config.nonconvergence_sweep_interval_secs.max(1));
        // Enforce the grace floor here rather than trusting the deployment.  A
        // grace shorter than the orchestrator's finalization tail terminates
        // executions that ran every step successfully and were merely waiting to
        // be finalized — measured, not theorised: see
        // `MIN_NONCONVERGENCE_GRACE_SECS`.
        let grace = effective_grace_secs(state.config.nonconvergence_grace_secs);
        if grace != state.config.nonconvergence_grace_secs {
            warn!(
                target: "noetl_server::nonconvergence_sweep",
                configured = state.config.nonconvergence_grace_secs,
                effective = grace,
                "non-convergence sweep: NOETL_NONCONVERGENCE_GRACE_SECS is below the safe floor and has been RAISED — a shorter grace terminates executions that are merely awaiting finalization"
            );
        }
        warn!(
            target: "noetl_server::nonconvergence_sweep",
            interval_secs = state.config.nonconvergence_sweep_interval_secs,
            grace_secs = grace,
            max_per_tick = state.config.nonconvergence_sweep_max_per_tick,
            stuck_claim_secs = state.config.nonconvergence_stuck_claim_secs,
            "non-convergence sweep: ENABLED — executions whose watermark has not moved for the grace period will be terminated append-only (playbook.failed)"
        );
        loop {
            tokio::time::sleep(interval).await;
            if let Err(e) = run_nonconvergence_sweep(&state).await {
                warn!(target: "noetl_server::nonconvergence_sweep", error = %e, "non-convergence sweep: tick failed");
                crate::metrics::record_nonconvergence_sweep("error");
            }
        }
    });
}

/// Run one sweep tick.
async fn run_nonconvergence_sweep(state: &AppState) -> AppResult<()> {
    let cfg = &state.config;
    let grace = effective_grace_secs(cfg.nonconvergence_grace_secs) as i64;
    let scan_limit = cfg.nonconvergence_sweep_scan_limit;
    let max_per_tick = cfg.nonconvergence_sweep_max_per_tick;
    let stuck_claim = cfg.nonconvergence_stuck_claim_secs as i64;

    // Live-worker set — the same `noetl.runtime` question the orphan sweep asks,
    // and the same answer, so the two sweeps can never disagree about whether a
    // worker is alive.
    let live: HashSet<String> = sqlx::query_as::<_, (String,)>(
        r#"
        SELECT name FROM noetl.runtime
        WHERE kind = 'worker_pool'
          AND heartbeat >= NOW() - INTERVAL '1 second' * $1
        "#,
    )
    .bind(cfg.orphan_worker_ttl_secs as i64)
    .fetch_all(state.pools.cluster())
    .await?
    .into_iter()
    .map(|(name,)| name)
    .collect();

    let per_shard = state
        .pools
        .for_each_shard(|_idx, pool| async move {
            query_nonconvergence_candidates(&pool, grace, scan_limit).await
        })
        .await?;

    let mut candidates: Vec<StalledExecution> =
        per_shard.into_iter().flat_map(|(_, v)| v).collect();

    // Execution affinity (RFC #116): only the owner replica acts, so the
    // per-tick cap counts this replica's work and two replicas never double-plan
    // the same backlog.  Inert with a single replica / affinity off.
    if state.affinity.active() {
        candidates.retain(|c| state.affinity.owns(c.execution_id));
    }
    // Longest-stalled first, so the per-tick cap always makes progress on the
    // oldest end of the backlog instead of oscillating.
    candidates.sort_by_key(|c| std::cmp::Reverse(c.stalled_secs));

    let plan = plan_dispositions(&candidates, &live, max_per_tick, stuck_claim);
    let mut deferred = 0usize;
    for (cand, disp) in candidates.iter().zip(plan.iter()) {
        crate::metrics::record_nonconvergence_sweep("candidate");
        crate::metrics::record_nonconvergence_sweep(disp.metric_label());
        match disp {
            Disposition::SkippedLive | Disposition::SkippedAwaitingCallback => {}
            Disposition::Capped => deferred += 1,
            Disposition::Terminate => match emit_nonconvergent_failed(state, cand).await {
                Ok(()) => info!(
                    target: "noetl_server::nonconvergence_sweep",
                    execution_id = cand.execution_id,
                    stalled_secs = cand.stalled_secs,
                    last_event_type = %cand.last_event_type,
                    "non-convergence sweep: terminated permanently-stalled execution (playbook.failed)"
                ),
                Err(e) => {
                    warn!(
                        target: "noetl_server::nonconvergence_sweep",
                        execution_id = cand.execution_id,
                        error = %e,
                        "non-convergence sweep: emit playbook.failed failed"
                    );
                    crate::metrics::record_nonconvergence_sweep("error");
                }
            },
        }
    }

    if deferred > 0 {
        warn!(
            target: "noetl_server::nonconvergence_sweep",
            deferred,
            max_per_tick,
            "non-convergence sweep: per-tick cap hit; deferred to the next tick"
        );
    }
    Ok(())
}

/// Pure decision core — the safety-critical part, kept free of I/O so it can be
/// exhaustively tested.
///
/// Invariants, in precedence order:
///
/// 1. A candidate parked on an external callback is **always**
///    `SkippedAwaitingCallback`.  Checked first, because a parked execution
///    whose worker also happens to be dead is still parked, not stalled.
/// 2. A candidate whose outstanding claim is held by a **live** worker is
///    `SkippedLive`, unless `stuck_claim_secs > 0` (opt-in, default off) and the
///    claim is older than it.
/// 3. Otherwise the first `max_per_tick` candidates `Terminate`; the rest are
///    `Capped`.
///
/// Skipped candidates never consume the termination budget.
pub(crate) fn plan_dispositions(
    candidates: &[StalledExecution],
    live: &HashSet<String>,
    max_per_tick: usize,
    stuck_claim_secs: i64,
) -> Vec<Disposition> {
    let mut terminated = 0usize;
    candidates
        .iter()
        .map(|c| {
            if c.awaiting_callback {
                return Disposition::SkippedAwaitingCallback;
            }
            let held_by_live = c.claim_worker_id.as_ref().is_some_and(|w| live.contains(w));
            if held_by_live {
                // Opt-in override: a live-held claim older than the configured
                // ceiling.  Default 0 disables this entirely — see the config
                // docs for why this is a timeout and not a proof.
                let overridden =
                    stuck_claim_secs > 0 && c.claim_age_secs.is_some_and(|a| a > stuck_claim_secs);
                if !overridden {
                    return Disposition::SkippedLive;
                }
            }
            if terminated < max_per_tick {
                terminated += 1;
                Disposition::Terminate
            } else {
                Disposition::Capped
            }
        })
        .collect()
}

/// Candidate query for one shard.
///
/// Candidate-first (the #62 shape), so nothing here aggregates over the full
/// event table:
///
/// * `starts` selects from the `event_type` index — ~11k start events on prod,
///   already narrowed by `created_at`, since an execution cannot have a
///   watermark older than `grace` unless it also *started* before then.  That
///   prefilter is necessary-but-not-sufficient and is only there to shrink the
///   scan; condition 2 is enforced for real on the watermark below.
/// * the terminal-existence check rides
///   `idx_event_exec_type (execution_id, event_type, event_id DESC)`.
/// * `watermark`, `claim` and `callback` are `LATERAL` probes over
///   `idx_event_execution_id`, evaluated only for the already-bounded candidate
///   set.
async fn query_nonconvergence_candidates(
    pool: &DbPool,
    grace_secs: i64,
    scan_limit: i64,
) -> AppResult<Vec<StalledExecution>> {
    type Row = (
        i64,
        i64,
        i64,
        String,
        i64,
        Option<String>,
        Option<i64>,
        bool,
    );
    let rows = sqlx::query_as::<_, Row>(
        r#"
        WITH starts AS (
            SELECT s.execution_id, MIN(s.created_at) AS started_at
            FROM noetl.event s
            WHERE s.event_type IN ('playbook.initialized', 'playbook_started', 'playbook.started')
              AND s.created_at < NOW() - INTERVAL '1 second' * $1
            GROUP BY s.execution_id
        ),
        open AS (
            SELECT st.execution_id, st.started_at
            FROM starts st
            WHERE NOT EXISTS (
                SELECT 1 FROM noetl.event t
                WHERE t.execution_id = st.execution_id
                  AND t.event_type IN (
                      'playbook.completed', 'playbook_completed',
                      'playbook.failed',    'playbook_failed',
                      'playbook.cancelled', 'playbook_cancelled',
                      -- An execution cancelled through the API carries
                      -- `execution.cancelled`.  It is excluded here rather than
                      -- terminated: writing playbook.failed over a deliberate
                      -- cancellation would turn a clean CANCELLED into a
                      -- misleading FAILED.  The projection gap that leaves it
                      -- reading RUNNING is fixed in ExecutionService::list.
                      'execution.cancelled'
                  )
            )
            -- Match the status projection exactly.  `ExecutionService::list`
            -- reports an execution FAILED as soon as ANY of its events carries
            -- status='FAILED', even with no terminal event present — 78 prod
            -- executions are in that state.  Terminating one would append a
            -- redundant terminal to something operators already see as failed,
            -- so the sweep leaves them alone and the two views stay consistent.
            AND NOT EXISTS (
                SELECT 1 FROM noetl.event f
                WHERE f.execution_id = st.execution_id
                  AND f.status = 'FAILED'
            )
            ORDER BY st.started_at ASC
            LIMIT $2
        )
        SELECT
            o.execution_id,
            COALESCE(
                (SELECT e2.catalog_id FROM noetl.event e2
                  WHERE e2.execution_id = o.execution_id AND e2.catalog_id <> 0
                  ORDER BY e2.event_id ASC LIMIT 1),
                0
            ) AS catalog_id,
            w.event_id                                        AS last_event_id,
            w.event_type                                      AS last_event_type,
            FLOOR(EXTRACT(EPOCH FROM (NOW() - w.created_at)))::BIGINT AS stalled_secs,
            claim.worker_id                                   AS claim_worker_id,
            FLOOR(EXTRACT(EPOCH FROM (NOW() - claim.created_at)))::BIGINT AS claim_age_secs,
            (cb.execution_id IS NOT NULL)                     AS awaiting_callback
        FROM open o
        CROSS JOIN LATERAL (
            SELECT e.event_id, e.event_type, e.created_at
            FROM noetl.event e
            WHERE e.execution_id = o.execution_id
            ORDER BY e.created_at DESC, e.event_id DESC
            LIMIT 1
        ) w
        LEFT JOIN LATERAL (
            SELECT COALESCE(c.worker_id, c.meta->>'worker_id') AS worker_id, c.created_at
            FROM noetl.event c
            WHERE c.execution_id = o.execution_id
              AND c.event_type = 'command.claimed'
              AND COALESCE(c.worker_id, c.meta->>'worker_id') IS NOT NULL
              AND NOT EXISTS (
                  SELECT 1 FROM noetl.event t
                  WHERE t.execution_id = c.execution_id
                    AND t.node_name = c.node_name
                    AND t.event_type IN ('command.completed', 'command.failed')
                    AND t.created_at >= c.created_at
              )
            ORDER BY c.created_at DESC
            LIMIT 1
        ) claim ON TRUE
        LEFT JOIN LATERAL (
            -- Parked on an external callback: the newest `command.completed`
            -- carries the worker's `pending_callback` marker and no `call.done`
            -- has landed for that step since.  A positive signal — absence of
            -- the marker is never read as "parked".
            SELECT cc.execution_id
            FROM noetl.event cc
            WHERE cc.execution_id = o.execution_id
              AND cc.event_type = 'command.completed'
              -- Where the marker actually lands, verified against prod rows
              -- rather than assumed: the worker hands `emit_event_via` a
              -- `context` JSON which the control plane stores under
              -- **`result.context`** — the `context` column itself is NULL on
              -- every `command.completed` on prod.  The other two spellings are
              -- accepted so a future storage-path change cannot silently turn
              -- the exclusion off.
              --
              -- Text comparison rather than ::boolean on purpose: a cast would
              -- raise on any unexpected value and take the whole sweep tick
              -- down with it.  A malformed marker must read as "not parked
              -- (unknown)", never as a crash — and it is the *presence* of a
              -- literal true that grants the exclusion, so an unparseable value
              -- simply fails to match.
              AND (
                    cc.result->'context'->>'pending_callback' = 'true'
                 OR cc.context->>'pending_callback' = 'true'
                 OR cc.meta->>'pending_callback' = 'true'
                  )
              AND NOT EXISTS (
                  SELECT 1 FROM noetl.event d
                  WHERE d.execution_id = cc.execution_id
                    AND d.node_name = cc.node_name
                    AND d.event_type = 'call.done'
                    AND d.created_at >= cc.created_at
              )
            ORDER BY cc.created_at DESC
            LIMIT 1
        ) cb ON TRUE
        WHERE w.created_at < NOW() - INTERVAL '1 second' * $1
        "#,
    )
    .bind(grace_secs)
    .bind(scan_limit)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(
                execution_id,
                catalog_id,
                last_event_id,
                last_event_type,
                stalled_secs,
                claim_worker_id,
                claim_age_secs,
                awaiting_callback,
            )| StalledExecution {
                execution_id,
                catalog_id,
                last_event_id,
                last_event_type,
                stalled_secs,
                claim_worker_id,
                claim_age_secs,
                awaiting_callback,
            },
        )
        .collect())
}

/// Emit the terminal `playbook.failed`, append-only, through the emit
/// chokepoint (so #103 sole-writer + #118 idempotent-terminal hold).  Parented
/// on the execution's newest event so the causal chain stays intact.
///
/// The reason is machine-readable (`meta.reason`) as well as human-readable, so
/// an operator can tell these apart from orphan-sweep terminals and from real
/// application failures without parsing prose.
async fn emit_nonconvergent_failed(state: &AppState, cand: &StalledExecution) -> AppResult<()> {
    let event_id = state.snowflake.generate()?;
    let error = format!(
        "execution did not converge: no event has been emitted for {}s (newest event '{}'), \
         no live worker holds an outstanding command, and no callback is pending — \
         terminated by the non-convergence sweep",
        cand.stalled_secs, cand.last_event_type
    );
    let ev = crate::handlers::event_write::EventRow::new(
        event_id,
        cand.execution_id,
        cand.catalog_id,
        "playbook.failed",
        "FAILED",
        chrono::Utc::now(),
    )
    .with_node("playbook")
    .with_result(serde_json::json!({"status": "FAILED", "context": {"error": error}}))
    .with_meta(serde_json::json!({
        "emitted_by": "nonconvergence_sweep",
        "reason": "watermark_stalled_beyond_grace",
        "stalled_secs": cand.stalled_secs,
        "last_event_type": cand.last_event_type,
        "last_event_id": cand.last_event_id,
        "commands_generated": 0,
        "error": error,
    }))
    .with_parent_event_id(cand.last_event_id);
    crate::handlers::event_write::emit_event(state, state.pools.pool_for(cand.execution_id), ev)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stalled(execution_id: i64) -> StalledExecution {
        StalledExecution {
            execution_id,
            catalog_id: 42,
            last_event_id: execution_id + 1,
            last_event_type: "step.skipped".to_string(),
            stalled_secs: 3_456_000,
            claim_worker_id: None,
            claim_age_secs: None,
            awaiting_callback: false,
        }
    }

    fn held_by(execution_id: i64, worker: &str, claim_age_secs: i64) -> StalledExecution {
        StalledExecution {
            claim_worker_id: Some(worker.to_string()),
            claim_age_secs: Some(claim_age_secs),
            last_event_type: "command.claimed".to_string(),
            ..stalled(execution_id)
        }
    }

    fn live_set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// The base case: an execution nothing holds, stalled far beyond grace.
    #[test]
    fn unheld_stalled_execution_is_terminated() {
        let c = vec![stalled(100)];
        let plan = plan_dispositions(&c, &live_set(&[]), 20, 0);
        assert_eq!(plan, vec![Disposition::Terminate]);
    }

    /// SAFETY: an outstanding claim held by a LIVE worker is never terminated,
    /// no matter how long it has been stalled or how large the cap.  This is
    /// #171's guard and it is not relaxed here.
    #[test]
    fn live_held_execution_is_never_terminated() {
        let c = vec![held_by(100, "noetl-worker-rust-abc-1", 999_999)];
        let plan = plan_dispositions(&c, &live_set(&["noetl-worker-rust-abc-1"]), 9999, 0);
        assert_eq!(plan, vec![Disposition::SkippedLive]);
    }

    /// A claim whose owner is gone is fair game — that is the orphan shape,
    /// reachable here when it is older than the orphan sweep's 48h lookback.
    #[test]
    fn dead_held_execution_is_terminated() {
        let c = vec![held_by(100, "noetl-worker-rust-dead-9", 999_999)];
        let plan = plan_dispositions(&c, &live_set(&["noetl-worker-rust-abc-1"]), 20, 0);
        assert_eq!(plan, vec![Disposition::Terminate]);
    }

    /// SAFETY: an execution parked on an external callback is never terminated,
    /// however old.  Time in the external system is free (execution-model
    /// "Callback / hook rule") and the watermark is meaningless there.
    #[test]
    fn callback_parked_execution_is_never_terminated() {
        let mut c = stalled(100);
        c.awaiting_callback = true;
        c.stalled_secs = 90 * 24 * 3600;
        let plan = plan_dispositions(&[c], &live_set(&[]), 9999, 0);
        assert_eq!(plan, vec![Disposition::SkippedAwaitingCallback]);
    }

    /// SAFETY: the callback check has precedence over every other rule — a
    /// parked execution whose worker also died is still parked.  The callback
    /// path frees the worker slot by design, so a rolled pod is expected there
    /// and must not turn a park into a termination.
    #[test]
    fn callback_check_outranks_dead_worker() {
        let mut c = held_by(100, "noetl-worker-rust-dead-9", 999_999);
        c.awaiting_callback = true;
        let plan = plan_dispositions(&[c], &live_set(&[]), 20, 0);
        assert_eq!(plan, vec![Disposition::SkippedAwaitingCallback]);
    }

    /// The opt-in live-held override stays inert at its default of 0 even when
    /// the claim is ancient.  Default-off has to mean off.
    #[test]
    fn stuck_claim_override_is_inert_at_zero() {
        let c = vec![held_by(100, "live-worker", i64::MAX / 2)];
        let plan = plan_dispositions(&c, &live_set(&["live-worker"]), 20, 0);
        assert_eq!(plan, vec![Disposition::SkippedLive]);
    }

    /// When an operator opts in, a live-held claim older than the ceiling is
    /// terminated — and one younger than it still is not.
    #[test]
    fn stuck_claim_override_respects_its_threshold() {
        let c = vec![
            held_by(100, "live-worker", 7200),
            held_by(200, "live-worker", 60),
        ];
        let plan = plan_dispositions(&c, &live_set(&["live-worker"]), 20, 3600);
        assert_eq!(plan, vec![Disposition::Terminate, Disposition::SkippedLive]);
    }

    /// Skipped candidates must not consume the termination budget, or one live
    /// worker holding many executions would starve the sweep.
    #[test]
    fn skipped_candidates_do_not_consume_the_budget() {
        let c = vec![
            held_by(1, "live-worker", 100),
            held_by(2, "live-worker", 100),
            held_by(3, "live-worker", 100),
            stalled(4),
            stalled(5),
        ];
        let plan = plan_dispositions(&c, &live_set(&["live-worker"]), 2, 0);
        assert_eq!(
            plan,
            vec![
                Disposition::SkippedLive,
                Disposition::SkippedLive,
                Disposition::SkippedLive,
                Disposition::Terminate,
                Disposition::Terminate,
            ]
        );
    }

    /// The per-tick cap bounds terminations and defers the rest.
    #[test]
    fn cap_bounds_terminations_per_tick() {
        let c: Vec<_> = (1..=5).map(stalled).collect();
        let plan = plan_dispositions(&c, &live_set(&[]), 2, 0);
        assert_eq!(
            plan,
            vec![
                Disposition::Terminate,
                Disposition::Terminate,
                Disposition::Capped,
                Disposition::Capped,
                Disposition::Capped,
            ]
        );
    }

    /// A zero cap terminates nothing — a usable kill switch that still lets the
    /// sweep report what it *would* have done.
    #[test]
    fn zero_cap_terminates_nothing() {
        let c: Vec<_> = (1..=3).map(stalled).collect();
        let plan = plan_dispositions(&c, &live_set(&[]), 0, 0);
        assert!(plan.iter().all(|d| *d == Disposition::Capped));
    }

    /// SAFETY: a grace below the floor is raised, not honoured.  A validation
    /// run at grace=120 terminated 30 executions that had run every step
    /// successfully and were still inside the finalization tail (measured p50
    /// 206s, max 393s); this is the guard that stops that configuration
    /// reaching production.
    #[test]
    fn grace_below_the_floor_is_raised() {
        use crate::config::MIN_NONCONVERGENCE_GRACE_SECS;
        assert_eq!(effective_grace_secs(0), MIN_NONCONVERGENCE_GRACE_SECS);
        assert_eq!(effective_grace_secs(120), MIN_NONCONVERGENCE_GRACE_SECS);
        assert_eq!(
            effective_grace_secs(MIN_NONCONVERGENCE_GRACE_SECS - 1),
            MIN_NONCONVERGENCE_GRACE_SECS
        );
    }

    /// A grace at or above the floor is passed through untouched — the floor is
    /// a floor, not an override.
    #[test]
    fn grace_at_or_above_the_floor_is_honoured() {
        use crate::config::MIN_NONCONVERGENCE_GRACE_SECS;
        assert_eq!(
            effective_grace_secs(MIN_NONCONVERGENCE_GRACE_SECS),
            MIN_NONCONVERGENCE_GRACE_SECS
        );
        assert_eq!(effective_grace_secs(86_400), 86_400);
    }

    /// The default must satisfy its own floor.
    #[test]
    fn default_grace_clears_the_floor() {
        let cfg = crate::config::AppConfig::default();
        assert!(cfg.nonconvergence_grace_secs >= crate::config::MIN_NONCONVERGENCE_GRACE_SECS);
    }

    /// Every disposition maps to a distinct metric label, so a dashboard can
    /// separate "we skipped live work" from "we ran out of budget".
    #[test]
    fn disposition_metric_labels_are_distinct() {
        let all = [
            Disposition::Terminate,
            Disposition::SkippedLive,
            Disposition::SkippedAwaitingCallback,
            Disposition::Capped,
        ];
        let labels: HashSet<&str> = all.iter().map(|d| d.metric_label()).collect();
        assert_eq!(labels.len(), all.len());
    }
}
