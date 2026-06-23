//! Result-tier garbage collection ([noetl/ai-meta#104](https://github.com/noetl/ai-meta/issues/104)
//! Phase F).
//!
//! The Feather/JSON result tier (Phase B–D) is durable object storage addressed
//! by the §7 physical key. Like any durable store it accrues objects whose
//! executions have aged out of retention, plus the occasional orphan (an object
//! written by the materializer for an execution whose events were later pruned).
//! This module is the **safe, conservative, flag-gated sweeper** that reclaims
//! only provably-dead objects.
//!
//! ## Safety invariant (the load-bearing property)
//!
//! **The sweep never deletes a live-referenced object.** An object is *live* when
//! its execution still has at least one surviving row in `noetl.event` — i.e. the
//! execution is still within retention and reachable from the audit log. A live
//! object is skipped before any deletion is even considered, in dry-run and
//! delete mode alike. The proof is the unit test [`tests::live_object_is_never_dead`]:
//! [`decide`] returns [`Decision::SkipLive`] for any age / grace whenever the
//! execution has surviving events.
//!
//! An object is a GC candidate (*dead*) only when **all** of:
//!
//! 1. its key parses to an `execution=<eid>` segment (unparseable → skipped, never
//!    deleted — we never reason about a key we don't understand);
//! 2. the execution has **no** surviving events in `noetl.event` (aged out of the
//!    event-retention window the `system/scheduled_cleanup` playbook enforces, or
//!    an orphan never referenced by a surviving event);
//! 3. the object is **older than the grace window** (derived from the execution_id
//!    snowflake mint time) — so a just-minted execution whose events have not yet
//!    materialized is never mistaken for dead.
//!
//! ## What this sweep deliberately does NOT do
//!
//! - It does **not** reap superseded attempt versions (keep-every URN, RFC OQ1) —
//!   that policy is still open; the sweep is attempt-agnostic, reclaiming whole
//!   aged-out executions, never one attempt of a still-live execution.
//! - It does **not** drive `noetl.result_store` retirement (RFC OQ5). Tier GC
//!   keys off **execution retention** (surviving events), independent of the
//!   dual-write fallback's lifecycle. The two are decoupled by design.
//!
//! Gated by `NOETL_RESULT_TIER_GC` (default off → the endpoint is a no-op).

use serde::{Deserialize, Serialize};

use crate::db::DbPool;
use crate::error::AppResult;
use crate::services::object_backend::ObjectBackend;

/// `NOETL_RESULT_TIER_GC` — master switch for the result-tier sweep. Default off
/// → the GC endpoint returns a no-op report and deletes nothing.
pub fn gc_enabled() -> bool {
    matches!(
        std::env::var("NOETL_RESULT_TIER_GC")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Default number of objects a single sweep examines.
fn default_limit() -> usize {
    1000
}

/// Default grace window (seconds) before an unreferenced object is eligible.
/// One day — comfortably longer than any materializer lag, so an in-flight
/// execution whose events have not yet landed is never mistaken for dead.
fn default_grace_seconds() -> i64 {
    86_400
}

/// Default key prefix the sweep scans — the §7 object-key root.
fn default_prefix() -> String {
    "noetl/".to_string()
}

/// GC sweep request (POST body). All fields optional; an empty body sweeps the
/// `noetl/` prefix in **dry-run** with the default limit + grace.
#[derive(Debug, Clone, Deserialize)]
pub struct GcRequest {
    /// When `true` (the default) the sweep lists dead candidates but deletes
    /// nothing. Set `false` to actually delete the candidates.
    #[serde(default = "default_dry_run")]
    pub dry_run: bool,
    /// Object-key prefix to scan (default `noetl/`).
    #[serde(default = "default_prefix")]
    pub prefix: String,
    /// Maximum objects to examine in one sweep.
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Minimum object age (seconds, from the execution_id mint time) before an
    /// unreferenced object is eligible for deletion.
    #[serde(default = "default_grace_seconds")]
    pub grace_seconds: i64,
}

fn default_dry_run() -> bool {
    true
}

impl Default for GcRequest {
    fn default() -> Self {
        Self {
            dry_run: default_dry_run(),
            prefix: default_prefix(),
            limit: default_limit(),
            grace_seconds: default_grace_seconds(),
        }
    }
}

/// One dead object the sweep identified.
#[derive(Debug, Clone, Serialize)]
pub struct GcCandidate {
    pub key: String,
    pub execution_id: i64,
    /// Why the object is dead (`unreferenced` — no surviving event for its
    /// execution; covers both aged-out and orphan).
    pub reason: &'static str,
    /// Whether this candidate was actually deleted (`false` in dry-run).
    pub deleted: bool,
}

/// GC sweep report (POST response).
#[derive(Debug, Clone, Serialize)]
pub struct GcReport {
    /// Echo of the `NOETL_RESULT_TIER_GC` gate. When `false` the sweep is a
    /// no-op and every other field is zero/empty.
    pub enabled: bool,
    pub dry_run: bool,
    pub prefix: String,
    /// Objects examined.
    pub scanned: usize,
    /// Objects deleted (always 0 in dry-run).
    pub deleted: usize,
    /// Objects skipped because their execution still has surviving events.
    pub skipped_live: usize,
    /// Objects skipped because they are younger than the grace window.
    pub skipped_grace: usize,
    /// Objects skipped because the key did not parse to an `execution=` segment.
    pub skipped_unparseable: usize,
    /// Delete failures (non-fatal; counted, the sweep continues).
    pub errors: usize,
    /// The dead objects (deleted, or would-be-deleted in dry-run).
    pub candidates: Vec<GcCandidate>,
}

impl GcReport {
    fn disabled() -> Self {
        Self {
            enabled: false,
            dry_run: true,
            prefix: String::new(),
            scanned: 0,
            deleted: 0,
            skipped_live: 0,
            skipped_grace: 0,
            skipped_unparseable: 0,
            errors: 0,
            candidates: Vec::new(),
        }
    }
}

/// The per-object verdict. A pure function of the object's parsed execution_id,
/// whether that execution has surviving events, the object's age, and the grace
/// window — isolated from any I/O so the safety invariant is unit-testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Skip: the key did not parse to an `execution=` segment.
    SkipUnparseable,
    /// Skip: the execution still has surviving events (live-referenced).
    SkipLive,
    /// Skip: the object is younger than the grace window.
    SkipGrace,
    /// Dead: reclaim.
    Dead,
}

/// Decide one object's fate. **Live always wins** — when `has_live_events` is
/// true the verdict is [`Decision::SkipLive`] regardless of age or grace, which
/// is the safety invariant the sweep rests on. Order: unparseable → live →
/// grace → dead.
pub fn decide(
    execution_id: Option<i64>,
    has_live_events: bool,
    age_seconds: Option<i64>,
    grace_seconds: i64,
) -> Decision {
    let Some(_eid) = execution_id else {
        return Decision::SkipUnparseable;
    };
    if has_live_events {
        return Decision::SkipLive;
    }
    // A young object whose events have not yet materialized is protected.
    // An undecodable age (shouldn't happen for a real eid) is treated as young.
    match age_seconds {
        Some(age) if age >= grace_seconds => Decision::Dead,
        _ => Decision::SkipGrace,
    }
}

/// Parse the `execution=<eid>` segment out of a §7 physical object key. Returns
/// `None` for any key without a numeric `execution=` segment (never panics).
pub fn parse_execution_id(key: &str) -> Option<i64> {
    key.split('/')
        .find_map(|seg| seg.strip_prefix("execution="))
        .and_then(|v| v.parse::<i64>().ok())
}

/// Seconds since an execution_id was minted, derived from the snowflake
/// timestamp (`ms = (id >> 22) + NOETL_EPOCH_MS`). `None` for a non-positive or
/// future-dated id, or before the NoETL epoch — the caller then treats the
/// object as young (protected by grace).
pub fn age_seconds(execution_id: i64, now_ms: u64) -> Option<i64> {
    if execution_id <= 0 {
        return None;
    }
    let mint_ms = ((execution_id as u64) >> 22) + crate::snowflake::NOETL_EPOCH_MS;
    if mint_ms > now_ms {
        return None;
    }
    Some(((now_ms - mint_ms) / 1000) as i64)
}

/// Whether `execution_id` has at least one surviving row in `noetl.event` — the
/// liveness signal. The execution-level check is intentionally the most
/// conservative: an object is live as long as *any* event of its execution
/// survives, so the sweep reclaims an object only once its whole execution has
/// aged out of the audit log.
async fn execution_has_events(pool: &DbPool, execution_id: i64) -> AppResult<bool> {
    let row: (bool,) =
        sqlx::query_as("SELECT EXISTS(SELECT 1 FROM noetl.event WHERE execution_id = $1)")
            .bind(execution_id)
            .fetch_one(pool)
            .await?;
    Ok(row.0)
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Run one result-tier GC sweep. No-op (and deletes nothing) unless
/// `NOETL_RESULT_TIER_GC` is set. Best-effort per object: a delete failure is
/// counted on `errors` and the sweep continues.
pub async fn sweep(pool: &DbPool, backend: &ObjectBackend, req: &GcRequest) -> AppResult<GcReport> {
    if !gc_enabled() {
        return Ok(GcReport::disabled());
    }

    let keys = backend.list(pool, &req.prefix, req.limit).await?;
    let now = now_ms();

    let mut report = GcReport {
        enabled: true,
        dry_run: req.dry_run,
        prefix: req.prefix.clone(),
        scanned: 0,
        deleted: 0,
        skipped_live: 0,
        skipped_grace: 0,
        skipped_unparseable: 0,
        errors: 0,
        candidates: Vec::new(),
    };

    for key in keys {
        report.scanned += 1;
        let eid = parse_execution_id(&key);

        // The liveness lookup only runs once we have an eid — an unparseable key
        // is never deletable, so we short-circuit without touching the DB.
        let has_live = match eid {
            Some(id) => execution_has_events(pool, id).await?,
            None => false,
        };
        let age = eid.and_then(|id| age_seconds(id, now));

        match decide(eid, has_live, age, req.grace_seconds) {
            Decision::SkipUnparseable => {
                report.skipped_unparseable += 1;
            }
            Decision::SkipLive => {
                report.skipped_live += 1;
            }
            Decision::SkipGrace => {
                report.skipped_grace += 1;
            }
            Decision::Dead => {
                let id = eid.expect("Dead verdict implies a parsed execution_id");
                let mut deleted = false;
                if !req.dry_run {
                    match backend.delete(pool, &key).await {
                        Ok(_) => {
                            report.deleted += 1;
                            deleted = true;
                        }
                        Err(e) => {
                            report.errors += 1;
                            tracing::warn!(execution_id = id, object_key = %key, error = %e, "result-tier GC delete failed");
                            continue;
                        }
                    }
                }
                report.candidates.push(GcCandidate {
                    key,
                    execution_id: id,
                    reason: "unreferenced",
                    deleted,
                });
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_execution_id_from_physical_key() {
        let key = "noetl/env=dev/region=local/cell=local-0/shard=s0053/tenant=default/\
                   project=default/date=2026-06-22/execution=325/results/start/0/0/1.feather";
        assert_eq!(parse_execution_id(key), Some(325));
    }

    #[test]
    fn unparseable_keys_yield_none() {
        // No execution= segment.
        assert_eq!(parse_execution_id("noetl/env=dev/results/x/0/0/1.feather"), None);
        // Non-numeric execution.
        assert_eq!(parse_execution_id("noetl/execution=abc/results/x.json"), None);
        assert_eq!(parse_execution_id(""), None);
    }

    // THE safety invariant: a live-referenced object is NEVER dead, for any age
    // or grace window, in dry-run or delete mode (the verdict gates both).
    #[test]
    fn live_object_is_never_dead() {
        for age in [Some(-10), Some(0), Some(1), Some(1_000_000), None] {
            for grace in [0, 1, 86_400, i64::MAX] {
                assert_eq!(
                    decide(Some(42), true, age, grace),
                    Decision::SkipLive,
                    "live object must skip (age={age:?}, grace={grace})"
                );
            }
        }
    }

    #[test]
    fn dead_only_when_unreferenced_and_past_grace() {
        // Unreferenced + old enough → Dead.
        assert_eq!(decide(Some(7), false, Some(100_000), 86_400), Decision::Dead);
        // Unreferenced but too young → protected by grace.
        assert_eq!(decide(Some(7), false, Some(10), 86_400), Decision::SkipGrace);
        // Unreferenced, age exactly at the grace boundary → Dead (>=).
        assert_eq!(decide(Some(7), false, Some(86_400), 86_400), Decision::Dead);
        // Unreferenced, undecodable age → treated as young, protected.
        assert_eq!(decide(Some(7), false, None, 86_400), Decision::SkipGrace);
        // Unparseable key → never deletable, regardless of liveness/age.
        assert_eq!(decide(None, false, Some(100_000), 0), Decision::SkipUnparseable);
    }

    #[test]
    fn age_seconds_handles_edges() {
        // Non-positive id → None (treated as young).
        assert_eq!(age_seconds(0, 10_000_000_000_000), None);
        assert_eq!(age_seconds(-1, 10_000_000_000_000), None);
        // A future-minted id (now before mint) → None.
        let future_eid = 1i64 << 22; // mint_ms = 1 + epoch
        assert_eq!(age_seconds(future_eid, 0), None);
        // A real-ish past id → positive age.
        // eid encoding 1000 ms after epoch: ((1000) << 22).
        let eid = 1000i64 << 22;
        let now = crate::snowflake::NOETL_EPOCH_MS + 5_000; // 5s later
        assert_eq!(age_seconds(eid, now), Some(4)); // (5000-1000)/1000 = 4
    }

    #[test]
    fn default_request_is_dry_run_safe() {
        let req = GcRequest::default();
        assert!(req.dry_run, "default must be dry-run");
        assert_eq!(req.prefix, "noetl/");
        assert_eq!(req.grace_seconds, 86_400);
    }

    #[test]
    fn disabled_report_is_inert() {
        let r = GcReport::disabled();
        assert!(!r.enabled);
        assert_eq!(r.deleted, 0);
        assert!(r.candidates.is_empty());
    }
}
