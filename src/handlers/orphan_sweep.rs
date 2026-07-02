//! Orphaned-command guardrail sweep (zombie-exec fix; refs
//! [#154](https://github.com/noetl/ai-meta/issues/154) /
//! [#161] / [#163]).
//!
//! ## The zombie
//!
//! When a worker pod is rolled or killed mid-execution, the step command it had
//! claimed (`command.claimed` carries the dead pod's `worker_id`) never gets a
//! `command.completed` / `command.failed` — the worker is gone.  The orchestrate
//! reconciler ([`crate::handlers::events::spawn_orchestrator_reconciler`]) then
//! re-drives `__orchestrate__` every 8s forever: the step is outstanding, 0 new
//! commands fan out, and the execution is wedged `RUNNING` permanently —
//! loading the system-pool drive and the `noetl.event` / `noetl.command`
//! reconcile queries.  Prod incident (2026-07-01): exec `330319143314137088`,
//! owned by dead worker-rust replicaset `5bbc55c678`, re-drove 111×/15min until
//! a human cleared it by hand.
//!
//! ## The guardrail
//!
//! Each tick this sweep finds RUNNING executions whose latest outstanding
//! `command.claimed` is owned by a `worker_id` that is **not live** (no
//! `noetl.runtime` row with a heartbeat within `orphan_worker_ttl_secs`) and
//! older than `orphan_sweep_grace_secs`, then terminates each **append-only**
//! with a `playbook.failed` event.  A worker roll becomes an automatic clean
//! failure within a bounded time instead of a permanent zombie, and the SPA
//! gets a terminal response instead of an infinite spinner.
//!
//! ## Why this shape (correctness first)
//!
//! * **Server-side only** — off the hot worker claim path, so it can never
//!   fail/re-queue a command a live worker is executing and never adds latency
//!   to the claim.  Aligns with the data-access boundary (the server owns
//!   `noetl.*`; this reads `noetl.runtime` + `noetl.event` and appends through
//!   the emit chokepoint).
//! * **Fail, not re-queue** — the dead worker released its slot when the pod
//!   died; emitting `playbook.failed` re-executes *nothing*, so there is zero
//!   risk of double-executing a side-effecting step.  Re-queueing an
//!   already-started command could double a side-effect, so v1 terminates
//!   (the user simply re-sends the turn → a fresh execution).  Safe re-queue of
//!   provably-not-yet-started commands is a possible future refinement.
//! * **Never fails live work** — an execution whose outstanding command is held
//!   by a live worker (heartbeat within TTL) is skipped every tick.
//! * **Append-only** — emits only `playbook.failed`; no `UPDATE` / `DELETE`.
//!   Routes through [`crate::handlers::event_write::emit_event`] so #103
//!   sole-writer ordering and idempotent-terminal (#118) dedup both hold — two
//!   replicas racing on the same orphan collapse to one terminal.
//! * **Rate-limited** — at most `orphan_sweep_max_per_tick` terminations per
//!   tick; a capped tick logs the deferred backlog (no silent truncation).
//! * **Flag-gated** — default OFF (`NOETL_ORPHAN_SWEEP_ENABLED`); the task
//!   spawns but returns immediately, so prod/default behavior is byte-identical.
//!   Instant rollback = flip the flag back to false.

use std::collections::HashSet;

use tracing::{info, warn};

use crate::db::DbPool;
use crate::error::AppResult;
use crate::state::AppState;

/// One outstanding claimed command that has not finished — the head of a
/// potentially-wedged execution.  Whether it is actually a zombie depends on
/// the liveness of [`Self::worker_id`], resolved against `noetl.runtime`.
#[derive(Debug, Clone)]
struct OrphanCandidate {
    execution_id: i64,
    catalog_id: i64,
    worker_id: String,
    node_name: String,
    /// The `command.claimed` event id — used as the causal parent of the
    /// terminal `playbook.failed`, keeping the event chain intact.
    claimed_event_id: i64,
}

/// Spawn the background orphaned-command sweep.  Safe to spawn unconditionally:
/// while the flag is off the task logs once and returns, scanning nothing.
pub fn spawn_orphan_command_sweep(state: AppState) {
    tokio::spawn(async move {
        if !state.config.orphan_sweep_enabled {
            info!(
                target: "noetl_server::orphan_sweep",
                "orphan-command sweep: disabled (NOETL_ORPHAN_SWEEP_ENABLED=false) — not scanning"
            );
            return;
        }
        let interval =
            std::time::Duration::from_secs(state.config.orphan_sweep_interval_secs.max(1));
        warn!(
            target: "noetl_server::orphan_sweep",
            interval_secs = state.config.orphan_sweep_interval_secs,
            grace_secs = state.config.orphan_sweep_grace_secs,
            worker_ttl_secs = state.config.orphan_worker_ttl_secs,
            max_per_tick = state.config.orphan_sweep_max_per_tick,
            "orphan-command sweep: ENABLED — dead-worker-orphaned RUNNING executions will be terminated append-only (playbook.failed)"
        );
        loop {
            tokio::time::sleep(interval).await;
            if let Err(e) = run_orphan_sweep(&state).await {
                warn!(target: "noetl_server::orphan_sweep", error = %e, "orphan-command sweep: tick failed");
                crate::metrics::record_orphan_sweep("error");
            }
        }
    });
}

/// Run one sweep tick: resolve the live-worker set, scan every shard for the
/// head outstanding claim per non-terminal execution, and terminate the ones
/// whose owner worker is dead (rate-limited).
async fn run_orphan_sweep(state: &AppState) -> AppResult<()> {
    let cfg = &state.config;
    let grace = cfg.orphan_sweep_grace_secs as i64;
    let ttl = cfg.orphan_worker_ttl_secs as i64;
    let lookback = cfg.orphan_sweep_lookback_secs as i64;
    let scan_limit = cfg.orphan_sweep_scan_limit;
    let max_per_tick = cfg.orphan_sweep_max_per_tick;

    // 1. Live-worker set.  A worker is LIVE iff a `noetl.runtime` row exists for
    //    its name with a heartbeat within TTL.  A gracefully-deregistered rolled
    //    pod is absent (row deleted on shutdown); a crashed pod that left a stale
    //    row fails the heartbeat cutoff.  `noetl.runtime` is a cluster-wide
    //    table, so it lives on the cluster pool.
    let live: HashSet<String> = sqlx::query_as::<_, (String,)>(
        r#"
        SELECT name FROM noetl.runtime
        WHERE kind = 'worker_pool'
          AND heartbeat >= NOW() - INTERVAL '1 second' * $1
        "#,
    )
    .bind(ttl)
    .fetch_all(state.pools.cluster())
    .await?
    .into_iter()
    .map(|(name,)| name)
    .collect();

    // 2. Candidate head-claims across all shards.
    let per_shard = state
        .pools
        .for_each_shard(|_idx, pool| async move {
            query_orphan_candidates(&pool, grace, lookback, scan_limit).await
        })
        .await?;

    let mut candidates: Vec<OrphanCandidate> =
        per_shard.into_iter().flat_map(|(_, v)| v).collect();

    // Execution affinity (RFC #116): only the owner replica acts, mirroring the
    // reconcile poller.  Inert with a single replica / affinity off.  Filter
    // before planning so the per-tick cap counts only this replica's work.
    if state.affinity.active() {
        candidates.retain(|c| state.affinity.owns(c.execution_id));
    }
    // Oldest claim first (event ids are time-ordered snowflakes) — clear the
    // longest-wedged executions before the per-tick cap bites.
    candidates.sort_by_key(|c| c.claimed_event_id);

    let plan = plan_dispositions(&candidates, &live, max_per_tick);
    let mut deferred = 0usize;
    for (cand, disp) in candidates.iter().zip(plan.iter()) {
        crate::metrics::record_orphan_sweep("candidate");
        match disp {
            // NEVER terminate an execution whose outstanding command is held by
            // a live worker — that is in-flight work, not a zombie.
            Disposition::SkippedLive => crate::metrics::record_orphan_sweep("skipped_live"),
            Disposition::Capped => {
                deferred += 1;
                crate::metrics::record_orphan_sweep("capped");
            }
            Disposition::Terminate => match emit_orphan_failed(state, cand).await {
                Ok(()) => {
                    crate::metrics::record_orphan_sweep("terminated");
                    info!(
                        target: "noetl_server::orphan_sweep",
                        execution_id = cand.execution_id,
                        dead_worker = %cand.worker_id,
                        node_name = %cand.node_name,
                        "orphan-command sweep: terminated dead-worker-orphaned execution (playbook.failed)"
                    );
                }
                Err(e) => {
                    warn!(
                        target: "noetl_server::orphan_sweep",
                        execution_id = cand.execution_id,
                        error = %e,
                        "orphan-command sweep: emit playbook.failed failed"
                    );
                    crate::metrics::record_orphan_sweep("error");
                }
            },
        }
    }

    if deferred > 0 {
        warn!(
            target: "noetl_server::orphan_sweep",
            deferred,
            max_per_tick,
            "orphan-command sweep: per-tick cap hit; deferred orphans to the next tick"
        );
    }
    Ok(())
}

/// Fate of one head-claim candidate this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Disposition {
    /// Owner worker is dead → emit `playbook.failed`.
    Terminate,
    /// Owner worker is live → in-flight work, leave untouched.
    SkippedLive,
    /// Owner is dead but the per-tick termination budget is spent → next tick.
    Capped,
}

/// Pure decision core (the safety-critical part).  Given the head-claim
/// candidates (already affinity-filtered and oldest-first), the live-worker
/// set, and the per-tick cap, decide each candidate's fate:
///
/// * a candidate whose `worker_id` is in `live` is **always** `SkippedLive` —
///   this is the invariant that we never fail work a live worker owns;
/// * otherwise the first `max_per_tick` dead-owned candidates `Terminate` and
///   the rest are `Capped` (deferred to a later tick).
///
/// Live-owned candidates never consume the termination budget.
fn plan_dispositions(
    candidates: &[OrphanCandidate],
    live: &HashSet<String>,
    max_per_tick: usize,
) -> Vec<Disposition> {
    let mut terminated = 0usize;
    candidates
        .iter()
        .map(|c| {
            if live.contains(&c.worker_id) {
                Disposition::SkippedLive
            } else if terminated < max_per_tick {
                terminated += 1;
                Disposition::Terminate
            } else {
                Disposition::Capped
            }
        })
        .collect()
}

/// The head outstanding `command.claimed` per non-terminal execution on one
/// shard, older than the grace period and within the lookback window.
///
/// A row is returned only when the claimed command is the execution's *latest*
/// claim (no newer `command.claimed`) AND that command never finished (no
/// `command.completed` / `command.failed` for the same step at-or-after the
/// claim) AND the execution has no terminal `playbook.*` event.  That is
/// precisely the dead-worker-wedge shape; an execution merely waiting on the
/// orchestrator to fan out the next step (a different stall the reconciler
/// already handles) is excluded because its head claim has finished.  Liveness
/// of the returned `worker_id` is decided by the caller against `noetl.runtime`.
async fn query_orphan_candidates(
    pool: &DbPool,
    grace_secs: i64,
    lookback_secs: i64,
    scan_limit: i64,
) -> AppResult<Vec<OrphanCandidate>> {
    // `command.claimed` carries the owner in EITHER the `worker_id` column (the
    // gate-off in-tx write, and the worker's own emit) OR `meta->>'worker_id'`
    // (the CQRS materialized path stores the owner in meta, leaving the column
    // NULL) — so read both, exactly as the claim-conflict check in
    // `handlers::events::claim_command` does.  Keying on the column alone misses
    // every materialized claim (the common prod shape under the publish-only
    // gate), so the sweep would find zero candidates.
    let rows = sqlx::query_as::<_, (i64, i64, Option<String>, Option<String>, i64)>(
        r#"
        SELECT
            c.execution_id,
            COALESCE(
                NULLIF(c.catalog_id, 0),
                (SELECT e2.catalog_id
                   FROM noetl.event e2
                  WHERE e2.execution_id = c.execution_id
                    AND e2.catalog_id <> 0
                  ORDER BY e2.event_id ASC
                  LIMIT 1),
                0
            ) AS catalog_id,
            COALESCE(c.worker_id, c.meta->>'worker_id') AS worker_id,
            c.node_name,
            c.event_id
        FROM noetl.event c
        WHERE c.event_type = 'command.claimed'
          AND COALESCE(c.worker_id, c.meta->>'worker_id') IS NOT NULL
          AND c.created_at <  NOW() - INTERVAL '1 second' * $1
          AND c.created_at >= NOW() - INTERVAL '1 second' * $2
          AND NOT EXISTS (
            SELECT 1 FROM noetl.event p
            WHERE p.execution_id = c.execution_id
              AND p.event_type IN ('playbook.completed', 'playbook.failed', 'playbook.cancelled')
          )
          AND NOT EXISTS (
            SELECT 1 FROM noetl.event c2
            WHERE c2.execution_id = c.execution_id
              AND c2.event_type = 'command.claimed'
              AND c2.created_at > c.created_at
          )
          AND NOT EXISTS (
            SELECT 1 FROM noetl.event t
            WHERE t.execution_id = c.execution_id
              AND t.node_name = c.node_name
              AND t.event_type IN ('command.completed', 'command.failed')
              AND t.created_at >= c.created_at
          )
        ORDER BY c.created_at ASC
        LIMIT $3
        "#,
    )
    .bind(grace_secs)
    .bind(lookback_secs)
    .bind(scan_limit)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(
            |(execution_id, catalog_id, worker_id, node_name, event_id)| {
                Some(OrphanCandidate {
                    execution_id,
                    catalog_id,
                    worker_id: worker_id?,
                    node_name: node_name.unwrap_or_default(),
                    claimed_event_id: event_id,
                })
            },
        )
        .collect())
}

/// Emit the terminal `playbook.failed` for an orphaned execution, append-only,
/// through the emit chokepoint (so #103 sole-writer + #118 idempotent-terminal
/// hold).  Parented on the orphaned `command.claimed` so the causal chain stays
/// intact.  `commands_generated: 0` in the meta mirrors the hand-clear shape.
async fn emit_orphan_failed(state: &AppState, cand: &OrphanCandidate) -> AppResult<()> {
    let event_id = state.snowflake.generate()?;
    let error = format!(
        "execution orphaned: command '{}' was claimed by worker '{}' which is no longer live \
         (pod rolled/crashed); terminated by the orphan-command guardrail",
        cand.node_name, cand.worker_id
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
        "emitted_by": "orphan_command_sweep",
        "reason": "orphaned_by_dead_worker",
        "dead_worker_id": cand.worker_id,
        "orphaned_node": cand.node_name,
        "commands_generated": 0,
        "error": error,
    }))
    .with_parent_event_id(cand.claimed_event_id);
    crate::handlers::event_write::emit_event(state, state.pools.pool_for(cand.execution_id), ev)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(execution_id: i64, worker: &str) -> OrphanCandidate {
        OrphanCandidate {
            execution_id,
            catalog_id: 42,
            worker_id: worker.to_string(),
            node_name: "extract_turn".to_string(),
            claimed_event_id: execution_id + 1,
        }
    }

    fn live_set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// A candidate owned by a dead worker (rolled/crashed pod, absent from the
    /// live set) is terminated.
    #[test]
    fn dead_worker_candidate_is_terminated() {
        let cands = vec![cand(100, "noetl-worker-rust-5bbc55c678-7vd5l")];
        let live = live_set(&["noetl-worker-rust-c95694c94-m7xvp"]);
        let plan = plan_dispositions(&cands, &live, 20);
        assert_eq!(plan, vec![Disposition::Terminate]);
    }

    /// SAFETY: a candidate owned by a LIVE worker is never terminated — even
    /// under a huge cap — so a normal in-flight execution is untouched.
    #[test]
    fn live_worker_candidate_is_never_terminated() {
        let cands = vec![
            cand(100, "noetl-worker-rust-c95694c94-m7xvp"), // live
            cand(200, "noetl-worker-rust-5bbc55c678-dead"), // dead
        ];
        let live = live_set(&["noetl-worker-rust-c95694c94-m7xvp"]);
        let plan = plan_dispositions(&cands, &live, 20);
        assert_eq!(
            plan,
            vec![Disposition::SkippedLive, Disposition::Terminate]
        );
    }

    /// The per-tick cap bounds terminations; the excess dead-owned candidates
    /// are deferred (Capped), not dropped.
    #[test]
    fn cap_defers_excess_dead_candidates() {
        let cands = vec![
            cand(100, "dead-a"),
            cand(200, "dead-b"),
            cand(300, "dead-c"),
        ];
        let live = live_set(&[]);
        let plan = plan_dispositions(&cands, &live, 2);
        assert_eq!(
            plan,
            vec![
                Disposition::Terminate,
                Disposition::Terminate,
                Disposition::Capped
            ]
        );
    }

    /// Live-owned candidates do NOT consume the termination budget — with a cap
    /// of 1, the single dead candidate still terminates even though a live one
    /// precedes it.
    #[test]
    fn live_candidates_do_not_consume_budget() {
        let cands = vec![
            cand(100, "live-a"),
            cand(200, "dead-b"),
            cand(300, "live-c"),
        ];
        let live = live_set(&["live-a", "live-c"]);
        let plan = plan_dispositions(&cands, &live, 1);
        assert_eq!(
            plan,
            vec![
                Disposition::SkippedLive,
                Disposition::Terminate,
                Disposition::SkippedLive
            ]
        );
    }

    /// A zero cap terminates nothing (all dead candidates deferred) — a safe
    /// "observe only" configuration.
    #[test]
    fn zero_cap_terminates_nothing() {
        let cands = vec![cand(100, "dead-a"), cand(200, "dead-b")];
        let live = live_set(&[]);
        let plan = plan_dispositions(&cands, &live, 0);
        assert_eq!(plan, vec![Disposition::Capped, Disposition::Capped]);
    }
}
