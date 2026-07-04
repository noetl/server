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
    /// Object class (`result` / `state_open` / `state_sealed` / `other`) — lets
    /// an operator see whether a candidate is a result object or a state shard
    /// (noetl/ai-meta#166 Phase 5).
    pub class: &'static str,
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
    /// Echo of the `NOETL_STATE_SHARD_GC` opt-in guard (noetl/ai-meta#166 Phase 5).
    pub state_shard_guard: bool,
    /// Open state shards that `decide` ruled dead but the guard held back for the
    /// extended open-shard grace. Always 0 when the guard is off.
    pub state_open_guard_protected: usize,
    /// State shards (open + sealed) among the dead candidates — the subset of
    /// `candidates` whose class is `state_open` / `state_sealed`.
    pub state_shard_candidates: usize,
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
            state_shard_guard: false,
            state_open_guard_protected: 0,
            state_shard_candidates: 0,
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

/// The kind of tier object a key addresses (noetl/ai-meta#166 Phase 5). Result
/// bytes and state shards co-locate under the same `execution=<eid>/` prefix
/// (§3.1) — they diverge only in the trailing `results/` vs `state/<seal>`
/// segment. Classifying lets the sweep report per-class counts and apply the
/// opt-in state-shard guard without changing the liveness invariant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectClass {
    /// `.../execution=<eid>/results/...` — a #104 result-tier object.
    Result,
    /// `.../execution=<eid>/state/open.<ext>` — an in-progress (never-sealed)
    /// state shard (noetl/ai-meta#166 Phase 2).
    StateOpen,
    /// `.../execution=<eid>/state/sealed.<ext>` — a sealed (terminal) state shard.
    StateSealed,
    /// Anything else under the prefix (neither a result nor a recognised state
    /// shard). Treated exactly like a result object for GC — classification only
    /// drives reporting + the open-shard guard, never deletability on its own.
    Other,
}

impl ObjectClass {
    /// Stable metric/report label.
    pub fn label(self) -> &'static str {
        match self {
            ObjectClass::Result => "result",
            ObjectClass::StateOpen => "state_open",
            ObjectClass::StateSealed => "state_sealed",
            ObjectClass::Other => "other",
        }
    }
}

/// Classify a §7 physical object key. Pure string inspection — never touches the
/// DB, never panics. Keys the state-shard writer emits end in
/// `/state/open.<ext>` or `/state/sealed.<ext>`
/// (`noetl/worker` `state_locator::StateCoordinates::physical_key`); result
/// objects carry a `/results/` segment.
pub fn classify_object(key: &str) -> ObjectClass {
    if key.contains("/state/open.") {
        ObjectClass::StateOpen
    } else if key.contains("/state/sealed.") {
        ObjectClass::StateSealed
    } else if key.contains("/results/") {
        ObjectClass::Result
    } else {
        ObjectClass::Other
    }
}

/// `NOETL_STATE_SHARD_GC` — opt-in stricter policy for **open** state shards
/// (noetl/ai-meta#166 Phase 5). Default off → state shards follow the exact same
/// policy as result objects (today's behavior; the #104 sweep already reclaims
/// them identically). On → an open state shard that `decide` marked `Dead` is
/// held for an extended grace (`grace_seconds × open_grace_multiplier`) —
/// belt-and-suspenders against a late cold-load racing retention on a
/// never-sealed execution. Sealed shards + result objects are unaffected.
pub fn state_shard_gc_enabled() -> bool {
    matches!(
        std::env::var("NOETL_STATE_SHARD_GC")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Default multiplier applied to the grace window for an **open** state shard
/// when the [`state_shard_gc_enabled`] guard is on. `7` → open shards survive
/// seven grace windows past their execution aging out.
fn default_open_grace_multiplier() -> i64 {
    std::env::var("NOETL_STATE_SHARD_OPEN_GRACE_MULTIPLIER")
        .ok()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|m| *m >= 1)
        .unwrap_or(7)
}

/// Class-aware GC verdict. Wraps the pure [`decide`] core and applies the opt-in
/// state-shard guard: when `state_shard_guard` is on, an [`ObjectClass::StateOpen`]
/// object that `decide` ruled `Dead` is downgraded to [`Decision::SkipGrace`]
/// unless it is *also* past `grace_seconds × open_grace_multiplier`. Every other
/// class uses `decide` verbatim. **The `SkipLive` invariant is never weakened** —
/// the guard can only make the sweep *more* conservative, never less: it never
/// turns a `Skip*` into a `Dead`. `open_grace_multiplier` is clamped to `>= 1`.
#[allow(clippy::too_many_arguments)]
pub fn decide_object(
    class: ObjectClass,
    execution_id: Option<i64>,
    has_live_events: bool,
    age_seconds: Option<i64>,
    grace_seconds: i64,
    state_shard_guard: bool,
    open_grace_multiplier: i64,
) -> Decision {
    let base = decide(execution_id, has_live_events, age_seconds, grace_seconds);
    // The guard only ever protects further; it never promotes to Dead.
    if !state_shard_guard || class != ObjectClass::StateOpen || base != Decision::Dead {
        return base;
    }
    let mult = open_grace_multiplier.max(1);
    let extended = grace_seconds.saturating_mul(mult);
    match age_seconds {
        Some(age) if age >= extended => Decision::Dead,
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
    // noetl/ai-meta#166 Phase 5: opt-in stricter open-state-shard guard. Off by
    // default → state shards follow the exact result-object policy (today).
    let state_shard_guard = state_shard_gc_enabled();
    let open_grace_multiplier = default_open_grace_multiplier();

    let mut report = GcReport {
        enabled: true,
        dry_run: req.dry_run,
        prefix: req.prefix.clone(),
        scanned: 0,
        deleted: 0,
        skipped_live: 0,
        skipped_grace: 0,
        skipped_unparseable: 0,
        state_shard_guard,
        state_open_guard_protected: 0,
        state_shard_candidates: 0,
        errors: 0,
        candidates: Vec::new(),
    };

    for key in keys {
        report.scanned += 1;
        let eid = parse_execution_id(&key);
        let class = classify_object(&key);

        // The liveness lookup only runs once we have an eid — an unparseable key
        // is never deletable, so we short-circuit without touching the DB.
        let has_live = match eid {
            Some(id) => execution_has_events(pool, id).await?,
            None => false,
        };
        let age = eid.and_then(|id| age_seconds(id, now));

        // Whether the open-shard guard *would* have deleted this object under the
        // base policy — used to count objects the guard specifically protected.
        let guard_downgraded = state_shard_guard
            && class == ObjectClass::StateOpen
            && decide(eid, has_live, age, req.grace_seconds) == Decision::Dead;

        match decide_object(
            class,
            eid,
            has_live,
            age,
            req.grace_seconds,
            state_shard_guard,
            open_grace_multiplier,
        ) {
            Decision::SkipUnparseable => {
                report.skipped_unparseable += 1;
                crate::metrics::record_result_tier_gc_object(class.label(), "skip_unparseable");
            }
            Decision::SkipLive => {
                report.skipped_live += 1;
                crate::metrics::record_result_tier_gc_object(class.label(), "skip_live");
            }
            Decision::SkipGrace => {
                report.skipped_grace += 1;
                if guard_downgraded {
                    // The base policy said Dead; the open-shard guard held it back.
                    report.state_open_guard_protected += 1;
                    crate::metrics::record_result_tier_gc_object(class.label(), "guard_protected");
                } else {
                    crate::metrics::record_result_tier_gc_object(class.label(), "skip_grace");
                }
            }
            Decision::Dead => {
                let id = eid.expect("Dead verdict implies a parsed execution_id");
                if class == ObjectClass::StateOpen || class == ObjectClass::StateSealed {
                    report.state_shard_candidates += 1;
                }
                let mut deleted = false;
                if !req.dry_run {
                    match backend.delete(pool, &key).await {
                        Ok(_) => {
                            report.deleted += 1;
                            deleted = true;
                            crate::metrics::record_result_tier_gc_object(class.label(), "deleted");
                        }
                        Err(e) => {
                            report.errors += 1;
                            tracing::warn!(execution_id = id, object_key = %key, class = class.label(), error = %e, "result-tier GC delete failed");
                            continue;
                        }
                    }
                } else {
                    crate::metrics::record_result_tier_gc_object(class.label(), "dead_dryrun");
                }
                report.candidates.push(GcCandidate {
                    key,
                    execution_id: id,
                    class: class.label(),
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

    // ---- classify_object (noetl/ai-meta#166 Phase 5) -----------------

    #[test]
    fn classifies_result_and_state_shard_keys() {
        let root = "noetl/env=prod/region=usc1/cell=usc1-a/shard=s0053/\
                    tenant=muno/project=travel/date=2026-06-30/execution=325";
        assert_eq!(
            classify_object(&format!("{root}/results/start/0/0/1.feather")),
            ObjectClass::Result
        );
        assert_eq!(
            classify_object(&format!("{root}/state/open.feather")),
            ObjectClass::StateOpen
        );
        assert_eq!(
            classify_object(&format!("{root}/state/sealed.feather")),
            ObjectClass::StateSealed
        );
        // A key under neither results/ nor a recognised state seal → Other.
        assert_eq!(
            classify_object(&format!("{root}/misc/thing.json")),
            ObjectClass::Other
        );
        assert_eq!(classify_object(""), ObjectClass::Other);
    }

    #[test]
    fn object_class_labels_are_stable() {
        assert_eq!(ObjectClass::Result.label(), "result");
        assert_eq!(ObjectClass::StateOpen.label(), "state_open");
        assert_eq!(ObjectClass::StateSealed.label(), "state_sealed");
        assert_eq!(ObjectClass::Other.label(), "other");
    }

    // ---- decide_object guard -----------------------------------------

    #[test]
    fn decide_object_is_decide_when_guard_off() {
        // Guard off → every class uses `decide` verbatim, for all inputs.
        for class in [
            ObjectClass::Result,
            ObjectClass::StateOpen,
            ObjectClass::StateSealed,
            ObjectClass::Other,
        ] {
            for (eid, live, age, grace) in [
                (Some(7_i64), false, Some(100_000_i64), 86_400_i64),
                (Some(7), false, Some(10), 86_400),
                (Some(7), true, Some(100_000), 86_400),
                (None, false, Some(100_000), 0),
            ] {
                assert_eq!(
                    decide_object(class, eid, live, age, grace, false, 7),
                    decide(eid, live, age, grace),
                    "guard-off must equal decide (class={class:?})"
                );
            }
        }
    }

    #[test]
    fn guard_protects_open_shard_within_extended_grace() {
        // Base policy: Dead (unreferenced, age 100k >= grace 86_400).
        assert_eq!(
            decide(Some(7), false, Some(100_000), 86_400),
            Decision::Dead
        );
        // Guard on: an OPEN shard at that age is held (100k < 86_400*7=604_800).
        assert_eq!(
            decide_object(
                ObjectClass::StateOpen,
                Some(7),
                false,
                Some(100_000),
                86_400,
                true,
                7
            ),
            Decision::SkipGrace
        );
        // Past the extended grace → Dead again.
        assert_eq!(
            decide_object(
                ObjectClass::StateOpen,
                Some(7),
                false,
                Some(700_000),
                86_400,
                true,
                7
            ),
            Decision::Dead
        );
    }

    #[test]
    fn guard_does_not_touch_sealed_or_result_or_live() {
        // Sealed shard + result object: guard on, but they follow the base policy.
        for class in [ObjectClass::StateSealed, ObjectClass::Result, ObjectClass::Other] {
            assert_eq!(
                decide_object(class, Some(7), false, Some(100_000), 86_400, true, 7),
                Decision::Dead,
                "guard must not protect {class:?}"
            );
        }
        // Live is never weakened, even for an open shard with the guard on — and
        // the guard can NEVER promote a Skip to Dead.
        assert_eq!(
            decide_object(ObjectClass::StateOpen, Some(7), true, Some(700_000), 86_400, true, 7),
            Decision::SkipLive
        );
    }

    #[test]
    fn guard_multiplier_is_clamped_to_at_least_one() {
        // A pathological multiplier of 0 must not make the extended grace 0
        // (which would delete every aged-out open shard immediately). Clamped
        // to 1 → behaves like the base grace.
        assert_eq!(
            decide_object(ObjectClass::StateOpen, Some(7), false, Some(100_000), 86_400, true, 0),
            Decision::Dead
        );
    }
}
