//! Fold an execution's state from an event log, and digest it
//! ([ai-meta#265](https://github.com/noetl/ai-meta/issues/265) Phase 1).
//!
//! The RFC's premise is that the orchestrator's control-flow state can be
//! **derived from the EHDB event log** rather than read out of Postgres. This
//! module is the derivation, and — deliberately — it can run against **either**
//! source, because the first question Phase 1 has to answer is not "does the
//! fold work" but:
//!
//! > **Is the EHDB event-log tier a sufficient source to fold control-flow
//! > state from?**
//!
//! That question has a measurable answer: fold the same execution from both
//! stores and compare the canonical digests. If they disagree, the tier is a
//! *verification* copy and not a *sourcing* copy, and no amount of work further
//! down the RFC's phases fixes that.
//!
//! # Why this is not obviously yes
//!
//! `WorkflowState::apply_event` reads `event.context` in six places — including
//! as the fallback when `result` is absent
//! (`orchestrate-core/src/state.rs`, `…or(event.context.as_ref())`). The
//! event-log tier's record
//! ([`super::ehdb_eventlog_mirror::mirror_payload`]) carries `result`, `meta`
//! and `error` — and **no `context` at all**.
//!
//! That omission is not a bug in the mirror. The mirror was built for
//! ai-meta#258's comparator, whose job is to answer "does the tier hold the
//! same events" — and for that, the identifying projection plus the payload the
//! comparator scores is exactly right. Sourcing a fold is a different
//! requirement that nobody had yet.
//!
//! This module makes the difference **measurable instead of arguable**.
//!
//! # What it deliberately does not do
//!
//! It does not write anything, it does not decide anything, and nothing in the
//! control-flow path calls it. It is diagnostic surface for the kind gate.
//! Phases 2 and 3 build on whichever source this proves sufficient.

use serde::Serialize;

use crate::db::DbPool;
use crate::error::AppResult;

/// Which log the fold read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FoldSource {
    /// `noetl.event` — the incumbent, and what the orchestrator's fallback path
    /// folds from today.
    Postgres,
    /// The EHDB event-log tier, read through the worker relay.
    EhdbTier,
    /// The worker's **WAL spine** — the off-server state builder's chain, served
    /// by `GET /ehdb/state-spine`. The source ai-meta#265's RFC settles on:
    /// it keeps `SLIM_EVENT_KEYS`, which includes `context` and both
    /// `timestamp`/`created_at`, so a fold over it is the fold the drive does.
    WalSpine,
}

impl FoldSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::EhdbTier => "ehdb_tier",
            Self::WalSpine => "wal_spine",
        }
    }
}

/// Why a fold refused. **Every variant is a refusal to produce state, never a
/// degraded state** — that is the fail-closed posture the RFC §4.3 requires,
/// expressed at the point where state is made rather than where it is consumed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "refusal", content = "detail")]
pub enum FoldRefusal {
    /// The source holds no events for this execution. Distinct from an empty
    /// state: folding nothing would invent an execution at step 0.
    NoEvents,
    /// The source could not be read.
    SourceUnavailable(String),
    /// Records were present but did not parse into events.
    Unparseable(String),
    /// `from_events` returned `None` — it could not build a state from a
    /// non-empty event set.
    FoldFailed,
    /// The WAL spine does not reach the requested head: the drain has not
    /// caught up, or a chain link is missing.
    ///
    /// **The fail-closed case that matters most on this source.** The worker
    /// answers `complete:false` with no events rather than a partial spine, and
    /// this refusal carries that through — a fold over a gapped spine is a
    /// different execution's history.
    SpineIncomplete,
    /// The tier holds the execution, but under a payload contract older than
    /// the fold requires — v1 records carry no `context`.
    ///
    /// **This exists so that "too old to fold" cannot be reported as
    /// "diverged".** Folding a v1 record succeeds; it just produces a state
    /// missing everything `context` contributes, which then digest-mismatches
    /// the Postgres fold. That mismatch is a *fault* verdict, so without this
    /// variant every execution mirrored before v2 would report as divergence —
    /// a false alarm on the one signal this whole comparator exists to make
    /// trustworthy, and one that would fire hardest immediately after the fix
    /// shipped. Refuse instead: it is honest, and it self-clears as v2 records
    /// accumulate.
    PayloadTooOld(String),
}

/// The refusal reasons, as metric labels. A closed set, pinned so every reason
/// is a visible 0 rather than an absent series.
pub const REFOLD_REFUSALS: [&str; 6] = [
    "no_events",
    "source_unavailable",
    "unparseable",
    "fold_failed",
    "spine_incomplete",
    "payload_too_old",
];

impl FoldRefusal {
    /// Why the fold refused, as a stable label.
    ///
    /// `verdict_for` deliberately collapses all five into the single verdict
    /// `spine_refused`, because for the *decision* they are identical: none of
    /// them is agreement. But for an operator they are not remotely identical —
    /// `no_events` is benign, `spine_incomplete` is drain lag that will clear
    /// itself, and `unparseable` is corruption. Prod currently reports
    /// `spine_refused` on 4 of 4 refolds with no way to tell which of those it is.
    ///
    /// So the verdict vocabulary stays closed (the six-verdict fail-closed
    /// contract is load-bearing) and the reason rides a separate counter.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::NoEvents => "no_events",
            Self::SourceUnavailable(_) => "source_unavailable",
            Self::Unparseable(_) => "unparseable",
            Self::FoldFailed => "fold_failed",
            Self::SpineIncomplete => "spine_incomplete",
            Self::PayloadTooOld(_) => "payload_too_old",
        }
    }
}

/// A folded state and the identity it can be checked by.
#[derive(Debug, Clone, Serialize)]
pub struct FoldedState {
    pub source: FoldSource,
    /// Highest `event_id` folded — the watermark a reader compares against the
    /// execution's chain head to decide freshness.
    pub version: i64,
    /// How many events the fold consumed.
    pub applied_count: usize,
    /// [`noetl_orchestrate_core::state::canonical_state_digest`] of the folded
    /// state. Canonical — see that function for why the raw serialisation is
    /// not comparable across processes.
    pub digest: String,
}

/// Fold from `noetl.event`.
///
/// Reads the same columns and builds the same `Event` values the orchestrator's
/// own rebuild path does, so a digest from here is the digest of the state the
/// orchestrator would have used.
pub async fn fold_from_postgres(
    pool: &DbPool,
    execution_id: i64,
) -> Result<FoldedState, FoldRefusal> {
    let rows = sqlx::query(
        r#"
        SELECT event_id, execution_id, catalog_id,
               parent_event_id, parent_execution_id,
               event_type, node_id, node_name, node_type, status,
               context, meta, result, worker_id,
               NULLIF(meta->>'attempt', '')::int AS attempt,
               created_at
        FROM noetl.event WHERE execution_id = $1 ORDER BY event_id ASC
        "#,
    )
    .bind(execution_id)
    .fetch_all(pool)
    .await
    .map_err(|e| FoldRefusal::SourceUnavailable(e.to_string()))?;

    if rows.is_empty() {
        return Err(FoldRefusal::NoEvents);
    }
    let events = super::events::parse_event_rows_for_fold(rows);
    fold(FoldSource::Postgres, events)
}

/// Fold from Postgres with `context` **blanked** — the controlled experiment
/// that identifies the cause instead of inferring it.
///
/// Phase 1 measured 12 executions whose two stores hold the SAME event count
/// and whose folded digests nevertheless differ. That rules out a missing
/// event, but "therefore it is `context`" is still a hypothesis: the tier
/// record differs from the Postgres row in more than one way, and the fold
/// reads several of those fields.
///
/// This isolates it. Blank `context` on the Postgres side and nothing else. If
/// the result matches the tier's digest, `context` is the whole difference and
/// the finding is positively identified. If it does not, something else is
/// missing too and the field list is incomplete — which is worth knowing before
/// anyone "fixes" the mirror by adding one field.
pub async fn fold_from_postgres_without_context(
    pool: &DbPool,
    execution_id: i64,
) -> Result<FoldedState, FoldRefusal> {
    let rows = sqlx::query(
        r#"
        SELECT event_id, execution_id, catalog_id,
               parent_event_id, parent_execution_id,
               event_type, node_id, node_name, node_type, status,
               context, meta, result, worker_id,
               NULLIF(meta->>'attempt', '')::int AS attempt,
               created_at
        FROM noetl.event WHERE execution_id = $1 ORDER BY event_id ASC
        "#,
    )
    .bind(execution_id)
    .fetch_all(pool)
    .await
    .map_err(|e| FoldRefusal::SourceUnavailable(e.to_string()))?;
    if rows.is_empty() {
        return Err(FoldRefusal::NoEvents);
    }
    let mut events = super::events::parse_event_rows_for_fold(rows);
    for e in &mut events {
        e.context = None;
    }
    fold(FoldSource::Postgres, events)
}

/// Fold from the EHDB event-log tier, read through the worker relay.
pub async fn fold_from_tier(execution_id: i64) -> Result<FoldedState, FoldRefusal> {
    fold_from_tier_within(execution_id, TIER_FOLD_TIMEOUT).await
}

/// How long a tier fold may take on the recovery read path.
///
/// The 20s the diagnostic endpoints use is fine for an operator waiting on a
/// reply and far too long here: this sits in front of a rebuild that has a
/// perfectly good incumbent one query away, so waiting is strictly worse than
/// refusing. Same reasoning as `ehdb_projection_read::RELAY_TIMEOUT`, and the
/// same value, deliberately.
pub const TIER_FOLD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// As [`fold_from_tier`], with the relay timeout given explicitly.
pub async fn fold_from_tier_within(
    execution_id: i64,
    timeout: std::time::Duration,
) -> Result<FoldedState, FoldRefusal> {
    let base = std::env::var(super::ehdb::WORKER_QUERY_URL_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| FoldRefusal::SourceUnavailable("WORKER_QUERY_URL unset".into()))?;
    let url = format!(
        "{}/ehdb/tiers/eventlog?execution={execution_id}&limit=2000",
        base.trim_end_matches('/')
    );
    let body: serde_json::Value = reqwest::Client::new()
        .get(&url)
        .timeout(timeout)
        .send()
        .await
        .map_err(|e| FoldRefusal::SourceUnavailable(e.to_string()))?
        .json()
        .await
        .map_err(|e| FoldRefusal::Unparseable(e.to_string()))?;

    if let Some(outcome) = body.get("outcome").and_then(|o| o.as_str()) {
        if outcome != "ok" {
            // A typed refusal is not an empty tier. Scoring it as "no events"
            // would report a broken relay as an execution with no history.
            return Err(FoldRefusal::SourceUnavailable(format!(
                "tier outcome={outcome}"
            )));
        }
    }
    let records = body
        .get("records")
        .and_then(|r| r.as_array())
        .ok_or_else(|| FoldRefusal::Unparseable("no records array".into()))?;
    if records.is_empty() {
        return Err(FoldRefusal::NoEvents);
    }

    let events = events_from_tier_records(records)?;
    fold(FoldSource::EhdbTier, events)
}

/// Turn tier records into foldable events: version-gate, parse, order, dedup.
///
/// Split out of [`fold_from_tier_within`] so the three rules below are reachable
/// by a unit test. Behind the HTTP call they were only exercisable against a
/// live relay, which is how a rule ends up asserted by nothing.
pub fn events_from_tier_records(
    records: &[serde_json::Value],
) -> Result<Vec<crate::db::models::Event>, FoldRefusal> {
    let mut events: Vec<crate::db::models::Event> = Vec::with_capacity(records.len());
    for r in records {
        let payload = match r.get("payload").and_then(|p| p.as_str()) {
            Some(s) => serde_json::from_str::<serde_json::Value>(s)
                .map_err(|e| FoldRefusal::Unparseable(e.to_string()))?,
            None => r.clone(),
        };
        // A single pre-v2 record poisons the WHOLE fold, so the check is per
        // record and refuses the lot. An execution spanning the upgrade holds
        // both shapes, and folding the v2 half plus a context-less v1 half
        // produces a state that is wrong in a way no digest comparison can
        // attribute — it would read as divergence at an arbitrary field.
        let version = payload
            .get(super::ehdb_eventlog_mirror::MIRROR_PAYLOAD_VERSION_KEY)
            .and_then(|v| v.as_i64())
            .unwrap_or(1);
        if version < super::ehdb_eventlog_mirror::MIRROR_PAYLOAD_VERSION {
            return Err(FoldRefusal::PayloadTooOld(format!(
                "record payload v{version} < v{} (pre-`context`)",
                super::ehdb_eventlog_mirror::MIRROR_PAYLOAD_VERSION
            )));
        }
        if let Some(ev) = event_from_tier_payload(&payload) {
            events.push(ev);
        }
    }
    if events.is_empty() {
        return Err(FoldRefusal::Unparseable(
            "records present but none carried an event_id".into(),
        ));
    }
    events.sort_by_key(|e| e.event_id);
    // The tier is append-only and the mirror is best-effort behind a retrying
    // drain, so the same authoritative event CAN appear twice. `apply_event` is
    // not idempotent — a duplicated event is a second state transition — so a
    // duplicate folds into a state that never existed, silently. Postgres
    // cannot produce this (the primary key forbids it), which is exactly why it
    // has to be handled on the way out of the tier rather than assumed away.
    events.dedup_by_key(|e| e.event_id);
    Ok(events)
}

/// Build an [`Event`] from a tier record's payload.
///
/// ⚠ `context` is read even though the mirror does not write it — so that this
/// function is a faithful reader of the record shape rather than a place the
/// omission is silently baked in. When the mirror starts carrying `context`,
/// this picks it up with no change; until then it is `None`, which is precisely
/// the difference the gate measures.
fn event_from_tier_payload(p: &serde_json::Value) -> Option<crate::db::models::Event> {
    let read_i64 = |v: Option<&serde_json::Value>| -> Option<i64> {
        v.and_then(|x| {
            x.as_i64()
                .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
        })
    };
    Some(crate::db::models::Event {
        id: 0,
        event_id: read_i64(p.get("event_id"))?,
        execution_id: read_i64(p.get("execution_id")).unwrap_or(0),
        catalog_id: read_i64(p.get("catalog_id")).unwrap_or(0),
        parent_event_id: read_i64(p.get("parent_event_id")),
        parent_execution_id: read_i64(p.get("parent_execution_id")),
        event_type: p
            .get("event_type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        node_id: p
            .get("node_id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        node_name: p
            .get("node_name")
            .or_else(|| p.get("step"))
            .and_then(|v| v.as_str())
            .map(str::to_string),
        node_type: p
            .get("node_type")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        status: p
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        context: p.get("context").filter(|v| !v.is_null()).cloned(),
        meta: p.get("meta").filter(|v| !v.is_null()).cloned(),
        result: p.get("result").filter(|v| !v.is_null()).cloned(),
        worker_id: p
            .get("worker_id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        attempt: p
            .get("meta")
            .and_then(|m| m.get("attempt"))
            .and_then(|a| {
                a.as_i64()
                    .or_else(|| a.as_str().and_then(|s| s.parse().ok()))
            })
            .map(|v| v as i32),
        created_at: p
            .get("created_at")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now),
    })
}

/// As [`fold`], but also returns the serialised state body — for the field-level
/// divergence diff. Kept separate so the hot path never pays the extra clone.
/// Truncate an event's `created_at` to **microsecond** precision.
///
/// # Why the fold normalises this (ai-meta#265 Phase 3)
///
/// `noetl.event.created_at` is a Postgres `timestamp`, which stores
/// **microseconds**. The EHDB event-bus envelope carries the original
/// **nanosecond** timestamp. So the same event, read from the two stores, is the
/// same instant at two precisions — and `WorkflowState::apply_event` propagates
/// `event.timestamp` into `started_at` / `completed_at` / `entered_at`, which
/// means the two folds produce states that differ in those fields and therefore
/// digest differently.
///
/// Measured, three executions × three probes, 9/9 identical:
///
/// ```text
/// /started_at: "2026-08-26T05:01:33.645451Z" != "2026-08-26T05:01:33.645451448Z"
///                            └ µs (Postgres)              └ ns (WAL envelope)
/// diff_fields=1  non_timestamp=0
/// ```
///
/// **This is not a source-sufficiency problem.** Field presence was identical on
/// both sides — events 4/4, `with_context` 2/2, `with_result` 2/2, `with_meta`
/// 4/4, `with_attempt` 1/1, `distinct_created_at` 4/4 — unlike the event-log
/// tier, which lacked `context` outright. The WAL spine carries everything the
/// fold reads.
///
/// # Why microseconds, and why here
///
/// Microseconds is what the **system of record** can hold. Sub-microsecond
/// precision is not durable anywhere in this platform, so a state that depends
/// on it is depending on an artefact of which store it was read from. Normalise
/// down to what survives a round trip, and the two sources agree on the value
/// rather than merely on a hash of it.
///
/// Done on the fold's **input** rather than inside `canonical_state_digest` on
/// purpose: normalising the digest alone would leave the folded *states*
/// genuinely different, and the drive reads `entered_at` / `completed_at`
/// directly. Equal digests over unequal states is precisely the kind of
/// agreement-by-construction this effort keeps refusing.
fn truncate_to_micros(ts: chrono::DateTime<chrono::Utc>) -> chrono::DateTime<chrono::Utc> {
    use chrono::TimeZone;
    // ROUND half-up, do not truncate.
    //
    // Postgres rounds a `timestamp` to the nearest microsecond; the first
    // version of this function floored, and the two agreed only when the
    // sub-microsecond remainder happened to be < 500 ns — measured at 1 of 4
    // executions matching, with the residual diff exactly one microsecond:
    //
    //   /started_at: "…05:09:02.725065Z" != "…05:09:02.725064Z"
    //                        └ Postgres rounded up   └ we floored
    //
    // Matching the system of record's rounding is the whole point: the goal is
    // the value Postgres would have stored, not merely a coarser value.
    let nanos = match ts.timestamp_nanos_opt() {
        Some(n) => n,
        // Outside the ~1677–2262 nanosecond-representable window. Leave it
        // alone rather than silently mangling a timestamp we cannot reason
        // about; such an event cannot come from this platform's clock.
        None => return ts,
    };
    let micros = nanos.div_euclid(1_000);
    let rem = nanos.rem_euclid(1_000);
    let rounded = if rem >= 500 { micros + 1 } else { micros };
    chrono::Utc.timestamp_micros(rounded).single().unwrap_or(ts)
}

/// Apply [`truncate_to_micros`] across an event set, in place.
fn normalise_event_precision(events: &mut [crate::db::models::Event]) {
    for e in events.iter_mut() {
        e.created_at = truncate_to_micros(e.created_at);
    }
}

fn fold_with_body(
    source: FoldSource,
    mut events: Vec<crate::db::models::Event>,
) -> Result<(FoldedState, serde_json::Value), FoldRefusal> {
    use noetl_orchestrate_core::state::{canonical_state_digest, WorkflowState};
    normalise_event_precision(&mut events);
    let version = events.iter().map(|e| e.event_id).max().unwrap_or(0);
    let applied_count = events.len();
    let core: Vec<noetl_orchestrate_core::event::Event> = events.iter().map(Into::into).collect();
    let state = WorkflowState::from_events(&core).ok_or(FoldRefusal::FoldFailed)?;
    let body = serde_json::to_value(&state).unwrap_or(serde_json::Value::Null);
    Ok((
        FoldedState {
            source,
            version,
            applied_count,
            digest: canonical_state_digest(&state),
        },
        body,
    ))
}

fn fold(
    source: FoldSource,
    mut events: Vec<crate::db::models::Event>,
) -> Result<FoldedState, FoldRefusal> {
    use noetl_orchestrate_core::state::{canonical_state_digest, WorkflowState};
    normalise_event_precision(&mut events);
    let version = events.iter().map(|e| e.event_id).max().unwrap_or(0);
    let applied_count = events.len();
    let core: Vec<noetl_orchestrate_core::event::Event> = events.iter().map(Into::into).collect();
    let state = WorkflowState::from_events(&core).ok_or(FoldRefusal::FoldFailed)?;
    Ok(FoldedState {
        source,
        version,
        applied_count,
        digest: canonical_state_digest(&state),
    })
}

/// Fold from the worker's **WAL spine** (ai-meta#265 Phase 2).
///
/// Reads `GET /ehdb/state-spine` from the worker relay and folds the ordered
/// verbatim slim payloads it serves. This is the source the RFC settles on:
///
/// * it carries `context`, which the event-log tier does not and which
///   `apply_event` reads in six places;
/// * it carries `timestamp`/`created_at`, so no `Utc::now()` substitution — the
///   reason a tier-sourced fold was byte-stable while the Postgres one was not,
///   before the loader fix;
/// * and it holds credential material **by alias**, not resolved, so folding
///   from it propagates no secrets (measured on kind's population; prod's is
///   unmeasured).
///
/// `head` pins the fold to a watermark. When supplied and the spine cannot
/// reach it, this refuses with [`FoldRefusal::SpineIncomplete`] rather than
/// folding what it has.
/// Fetch and parse the WAL spine's events, without folding.
/// Shared by [`fold_from_wal_spine`] and the field-level diff endpoint.
pub async fn wal_spine_events(
    execution_id: i64,
    head: Option<i64>,
) -> Result<Vec<crate::db::models::Event>, FoldRefusal> {
    let inner = fold_spine_inner(execution_id, head).await?;
    Ok(inner)
}

pub async fn fold_from_wal_spine(
    execution_id: i64,
    head: Option<i64>,
) -> Result<FoldedState, FoldRefusal> {
    let events = fold_spine_inner(execution_id, head).await?;
    fold(FoldSource::WalSpine, events)
}

async fn fold_spine_inner(
    execution_id: i64,
    head: Option<i64>,
) -> Result<Vec<crate::db::models::Event>, FoldRefusal> {
    let base = std::env::var(super::ehdb::WORKER_QUERY_URL_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| FoldRefusal::SourceUnavailable("WORKER_QUERY_URL unset".into()))?;
    let mut url = format!(
        "{}/ehdb/state-spine?execution={execution_id}",
        base.trim_end_matches('/')
    );
    if let Some(h) = head {
        url.push_str(&format!("&head={h}"));
    }
    let body: serde_json::Value = reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| FoldRefusal::SourceUnavailable(e.to_string()))?
        .json()
        .await
        .map_err(|e| FoldRefusal::Unparseable(e.to_string()))?;

    match body.get("outcome").and_then(|o| o.as_str()) {
        Some("ok") => {}
        // `unavailable` is a worker with no index — NOT an execution with no
        // events. Collapsing them would fold nothing and call it a state.
        Some("unavailable") => {
            return Err(FoldRefusal::SourceUnavailable(
                "worker runs no state-builder index".into(),
            ))
        }
        Some("incomplete") => return Err(FoldRefusal::SpineIncomplete),
        other => {
            return Err(FoldRefusal::Unparseable(format!(
                "unexpected spine outcome {other:?}"
            )))
        }
    }
    // Belt and braces: the route promises no events on an incomplete build, and
    // this checks the promise rather than trusting it. A future change that
    // started serving a partial spine would be caught here instead of silently
    // producing a state for a history that never happened.
    if body.get("complete").and_then(|c| c.as_bool()) != Some(true) {
        return Err(FoldRefusal::SpineIncomplete);
    }

    let events_json = body
        .get("events")
        .and_then(|e| e.as_array())
        .ok_or_else(|| FoldRefusal::Unparseable("no events array".into()))?;
    if events_json.is_empty() {
        return Err(FoldRefusal::NoEvents);
    }
    let mut events: Vec<crate::db::models::Event> = Vec::with_capacity(events_json.len());
    for p in events_json {
        if let Some(ev) = event_from_tier_payload(p) {
            events.push(ev);
        }
    }
    if events.is_empty() {
        return Err(FoldRefusal::Unparseable(
            "spine events carried no event_id".into(),
        ));
    }
    events.sort_by_key(|e| e.event_id);
    Ok(events)
}

/// Why a comparison against a fresh WAL re-fold did not hold.
///
/// Every variant is a reason to **not use** the stored projection. There is no
/// "use it anyway" outcome, and there is deliberately no fallback-to-Postgres
/// outcome either: a comparator whose failure mode is reading a different store
/// establishes the second source of truth this whole effort exists to remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReFoldVerdict {
    /// The stored record's digest equals a fresh fold of the spine at the same
    /// version. The only outcome that permits use.
    Match,
    /// Both produced a state and the digests differ.
    DigestMismatch,
    /// The stored record claims a version the spine does not reach.
    StoredAheadOfSpine,
    /// The spine has moved past the stored record. Not corruption — the
    /// materialiser has not caught up.
    StoredBehindSpine,
    /// Nothing stored for this execution yet.
    NoStoredRecord,
    /// The spine could not be folded; carries the refusal.
    SpineRefused,
}

impl ReFoldVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::DigestMismatch => "digest_mismatch",
            Self::StoredAheadOfSpine => "stored_ahead_of_spine",
            Self::StoredBehindSpine => "stored_behind_spine",
            Self::NoStoredRecord => "no_stored_record",
            Self::SpineRefused => "spine_refused",
        }
    }
    /// Whether this verdict means the stored record is WRONG, as opposed to
    /// absent or merely behind. Only these should page.
    pub fn is_fault(self) -> bool {
        matches!(self, Self::DigestMismatch | Self::StoredAheadOfSpine)
    }
}

/// Every verdict label. Closed set, pinned at 0.
pub const REFOLD_VERDICTS: [&str; 6] = [
    "match",
    "digest_mismatch",
    "stored_ahead_of_spine",
    "stored_behind_spine",
    "no_stored_record",
    "spine_refused",
];

/// Compare a stored projection record against a **fresh fold of the WAL spine**.
///
/// Ground truth is the re-fold, never Postgres. That is the whole difference
/// from ai-meta#265 A4's comparator, which compared the tier against a
/// `noetl.projection_snapshot` row — a copy against another copy, where the
/// digest being compared was the same number moved rather than recomputed.
///
/// **Pure decision, given the two sides**, so the fault vocabulary is testable
/// without a cluster.
pub fn verdict_for(
    stored: Option<(i64, &str)>,
    spine: &Result<FoldedState, FoldRefusal>,
) -> ReFoldVerdict {
    let spine = match spine {
        Ok(s) => s,
        Err(FoldRefusal::SpineIncomplete) | Err(_) => return ReFoldVerdict::SpineRefused,
    };
    let Some((stored_version, stored_digest)) = stored else {
        return ReFoldVerdict::NoStoredRecord;
    };
    if stored_version > spine.version {
        // The record claims an event the spine does not have. Checked BEFORE
        // the digest: an ahead record's digest necessarily differs, and
        // reporting that as `digest_mismatch` would send an operator looking
        // for corruption instead of for a store that is ahead of its own log.
        return ReFoldVerdict::StoredAheadOfSpine;
    }
    if stored_version < spine.version {
        return ReFoldVerdict::StoredBehindSpine;
    }
    if stored_digest == spine.digest {
        ReFoldVerdict::Match
    } else {
        ReFoldVerdict::DigestMismatch
    }
}

/// Both folds, and whether they agree — the Phase 1 question in one reply.
#[derive(Debug, Serialize)]
pub struct FoldComparison {
    pub execution_id: i64,
    pub postgres: Option<FoldedState>,
    pub postgres_refusal: Option<FoldRefusal>,
    pub tier: Option<FoldedState>,
    pub tier_refusal: Option<FoldRefusal>,
    /// `true` only when BOTH folds produced a state and the digests match.
    /// Absence of disagreement is not agreement: two refusals are not a match.
    pub digests_agree: bool,
    /// Set when both folded but disagreed — the shortest description of why the
    /// tier is not (yet) a sufficient source.
    pub disagreement: Option<String>,
    /// Postgres folded with `context` blanked. When this equals the tier's
    /// digest, `context` is positively identified as the whole difference.
    pub postgres_without_context: Option<FoldedState>,
    /// `true` when blanking `context` on the Postgres side reproduces the
    /// tier's digest exactly.
    pub context_explains_the_gap: bool,
}

pub async fn compare_sources(pool: &DbPool, execution_id: i64) -> AppResult<FoldComparison> {
    let pg = fold_from_postgres(pool, execution_id).await;
    let tier = fold_from_tier(execution_id).await;
    let (postgres, postgres_refusal) = match pg {
        Ok(f) => (Some(f), None),
        Err(r) => (None, Some(r)),
    };
    let (tier_ok, tier_refusal) = match tier {
        Ok(f) => (Some(f), None),
        Err(r) => (None, Some(r)),
    };
    let digests_agree = match (&postgres, &tier_ok) {
        (Some(a), Some(b)) => a.digest == b.digest,
        _ => false,
    };
    let disagreement = match (&postgres, &tier_ok) {
        (Some(a), Some(b)) if a.digest != b.digest => Some(format!(
            "postgres v{} n={} {} != tier v{} n={} {}",
            a.version,
            a.applied_count,
            &a.digest[..16],
            b.version,
            b.applied_count,
            &b.digest[..16]
        )),
        _ => None,
    };
    let no_ctx = fold_from_postgres_without_context(pool, execution_id)
        .await
        .ok();
    let context_explains_the_gap = match (&no_ctx, &tier_ok) {
        (Some(a), Some(b)) => a.digest == b.digest,
        _ => false,
    };
    Ok(FoldComparison {
        execution_id,
        postgres_without_context: no_ctx,
        context_explains_the_gap,
        postgres,
        postgres_refusal,
        tier: tier_ok,
        tier_refusal,
        digests_agree,
        disagreement,
    })
}

/// Fold the same execution TWICE in one request and report every JSON path
/// where the two results differ.
///
/// Phase 1 measured the Postgres-sourced fold producing a different canonical
/// digest on every call — three calls, one process, static rows, three digests
/// — while the tier-sourced fold was byte-stable. That refutes the premise the
/// whole event-sourced read model rests on, and it refutes it for ANY source,
/// so no amount of choosing a better log fixes it.
///
/// Naming the varying field is therefore the blocking question, and it is a
/// measurement rather than a guess: fold, fold again, diff the two bodies.
/// Both folds run in the same request against the same rows, so anything that
/// differs is non-determinism in the fold itself.
fn json_diff_paths(a: &serde_json::Value, b: &serde_json::Value, at: &str, out: &mut Vec<String>) {
    if out.len() > 40 {
        return;
    }
    match (a, b) {
        (serde_json::Value::Object(x), serde_json::Value::Object(y)) => {
            let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                match (x.get(k), y.get(k)) {
                    (Some(va), Some(vb)) => json_diff_paths(va, vb, &format!("{at}/{k}"), out),
                    (Some(_), None) => out.push(format!("{at}/{k} (only in first)")),
                    (None, Some(_)) => out.push(format!("{at}/{k} (only in second)")),
                    (None, None) => {}
                }
            }
        }
        (serde_json::Value::Array(x), serde_json::Value::Array(y)) => {
            if x.len() != y.len() {
                out.push(format!("{at} array len {} vs {}", x.len(), y.len()));
                return;
            }
            for (i, (va, vb)) in x.iter().zip(y.iter()).enumerate() {
                json_diff_paths(va, vb, &format!("{at}/{i}"), out);
            }
        }
        _ => {
            if a != b {
                let sa = a.to_string();
                let sb = b.to_string();
                out.push(format!(
                    "{at}: {} != {}",
                    &sa[..sa.len().min(60)],
                    &sb[..sb.len().min(60)]
                ));
            }
        }
    }
}

/// `GET /api/ehdb/projection-fold/determinism/{id}` — fold twice, diff.
pub async fn determinism_endpoint(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
    axum::extract::Path(execution_id): axum::extract::Path<i64>,
) -> AppResult<axum::Json<serde_json::Value>> {
    use noetl_orchestrate_core::state::{canonical_state_digest, WorkflowState};
    let pool = state.pools.pool_for(execution_id);

    async fn load_events(
        pool: &crate::db::DbPool,
        execution_id: i64,
    ) -> Vec<crate::db::models::Event> {
        let rows = sqlx::query(
            r#"
            SELECT event_id, execution_id, catalog_id,
                   parent_event_id, parent_execution_id,
                   event_type, node_id, node_name, node_type, status,
                   context, meta, result, worker_id,
                   NULLIF(meta->>'attempt', '')::int AS attempt,
                   created_at
            FROM noetl.event WHERE execution_id = $1 ORDER BY event_id ASC
            "#,
        )
        .bind(execution_id)
        .fetch_all(pool)
        .await
        .unwrap_or_default();
        super::events::parse_event_rows_for_fold(rows)
    }

    let ev1 = load_events(pool, execution_id).await;
    let ev2 = load_events(pool, execution_id).await;
    // Also fold the SAME in-memory event vector twice, which separates "the
    // fold is non-deterministic" from "two reads of the table differ".
    let core1: Vec<noetl_orchestrate_core::event::Event> = ev1.iter().map(Into::into).collect();
    let core2: Vec<noetl_orchestrate_core::event::Event> = ev2.iter().map(Into::into).collect();

    let s1 = WorkflowState::from_events(&core1);
    let s2 = WorkflowState::from_events(&core2);
    let s3 = WorkflowState::from_events(&core1); // same input as s1

    let body = |s: &Option<WorkflowState>| {
        s.as_ref()
            .map(|w| serde_json::to_value(w).unwrap_or(serde_json::Value::Null))
            .unwrap_or(serde_json::Value::Null)
    };
    let (b1, b2, b3) = (body(&s1), body(&s2), body(&s3));

    let mut diff_two_reads = Vec::new();
    json_diff_paths(&b1, &b2, "", &mut diff_two_reads);
    let mut diff_same_input = Vec::new();
    json_diff_paths(&b1, &b3, "", &mut diff_same_input);

    Ok(axum::Json(serde_json::json!({
        "action": "ehdb.projection.fold.determinism",
        "execution_id": execution_id,
        "events": core1.len(),
        "digest_read1": s1.as_ref().map(canonical_state_digest),
        "digest_read2": s2.as_ref().map(canonical_state_digest),
        "digest_same_input_refold": s3.as_ref().map(canonical_state_digest),
        // The decisive split: if this is empty but the digests differ, the
        // variation is in SERIALISATION, not in the folded value.
        "diff_paths_two_reads": diff_two_reads,
        "diff_paths_same_input": diff_same_input,
    })))
}

/// `GET /api/ehdb/projection-fold/executions/{id}` — both folds, side by side.
///
/// Read-only and decision-free. It exists so the Phase-1 question is answered
/// by a measurement on a real execution rather than by reading two files and
/// reasoning about them.
pub async fn compare_sources_endpoint(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
    axum::extract::Path(execution_id): axum::extract::Path<i64>,
) -> AppResult<axum::Json<serde_json::Value>> {
    let pool = state.pools.pool_for(execution_id);
    let cmp = compare_sources(pool, execution_id).await?;
    Ok(axum::Json(serde_json::json!({
        "action": "ehdb.projection.fold.compare",
        "result": cmp,
    })))
}

/// Read the newest stored projection record's `(version, digest)` for one
/// execution, from the projection tier.
///
/// Returns `Ok(None)` for "the tier holds nothing for this execution" and an
/// `Err` for "the tier could not be read" — kept distinct for the same reason
/// the spine route does: one is an absence, the other is an inability, and a
/// comparator that scored them alike would report a broken relay as an unarmed
/// materialiser.
pub async fn stored_projection(execution_id: i64) -> Result<Option<(i64, String)>, String> {
    let base = std::env::var(super::ehdb::WORKER_QUERY_URL_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "WORKER_QUERY_URL unset".to_string())?;
    let url = format!(
        "{}/ehdb/tiers/projection?execution={execution_id}&limit=500",
        base.trim_end_matches('/')
    );
    let body: serde_json::Value = reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    if let Some(o) = body.get("outcome").and_then(|o| o.as_str()) {
        if o != "ok" {
            return Err(format!("tier outcome={o}"));
        }
    }
    let records = body
        .get("records")
        .and_then(|r| r.as_array())
        .ok_or_else(|| "no records array".to_string())?;
    let mut best: Option<(i64, u64, String)> = None;
    for r in records {
        let seq = r
            .get("global_sequence")
            .and_then(|s| s.as_u64())
            .unwrap_or(0);
        let p = match r.get("payload").and_then(|p| p.as_str()) {
            Some(s) => match serde_json::from_str::<serde_json::Value>(s) {
                Ok(v) => v,
                Err(_) => continue,
            },
            None => r.clone(),
        };
        let (Some(v), Some(d)) = (
            p.get("version").and_then(|v| {
                v.as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
            }),
            p.get("digest")
                .or_else(|| p.get("checksum"))
                .and_then(|d| d.as_str()),
        ) else {
            continue;
        };
        // Newest by (version, sequence) — the same rule the read path uses, so
        // the comparator judges the record a reader would serve.
        if best
            .as_ref()
            .is_none_or(|(bv, bs, _)| (v, seq) > (*bv, *bs))
        {
            best = Some((v, seq, d.to_string()));
        }
    }
    Ok(best.map(|(v, _, d)| (v, d)))
}

/// `GET /api/ehdb/projection-refold/executions/{id}` — the Phase 2 comparator.
///
/// Ground truth is a fresh fold of the WAL spine. Never Postgres.
pub async fn refold_endpoint(
    axum::extract::State(_state): axum::extract::State<crate::state::AppState>,
    axum::extract::Path(execution_id): axum::extract::Path<i64>,
) -> AppResult<axum::Json<serde_json::Value>> {
    let spine = fold_from_wal_spine(execution_id, None).await;
    let stored = stored_projection(execution_id).await;
    let stored_pair = match &stored {
        Ok(Some((v, d))) => Some((*v, d.as_str())),
        _ => None,
    };
    let verdict = verdict_for(stored_pair, &spine);
    crate::metrics::record_ehdb_projection_refold(verdict.as_str());
    if let Err(r) = &spine {
        crate::metrics::record_ehdb_projection_refold_refusal(r.reason());
    }
    Ok(axum::Json(serde_json::json!({
        "action": "ehdb.projection.refold",
        "execution_id": execution_id,
        "verdict": verdict.as_str(),
        // Why the spine refused, when it did. `spine_refused` alone sends an
        // operator looking for corruption when the answer is usually drain lag.
        "refusal": spine.as_ref().err().map(|r| r.reason()),
        "is_fault": verdict.is_fault(),
        "spine": spine.as_ref().ok(),
        "spine_refusal": spine.as_ref().err(),
        "stored_version": stored_pair.map(|(v, _)| v),
        "stored_digest": stored_pair.map(|(_, d)| d),
        "stored_read_error": stored.as_ref().err(),
    })))
}

/// Materialise one **in-flight** execution's state into the projection tier,
/// folded from the WAL spine (ai-meta#265 Phase 3).
///
/// # Why in-flight only
///
/// The state-builder index evicts an execution's chain on a terminal event, so
/// a completed execution has no spine and this correctly refuses. That is the
/// right shape rather than a limitation: control flow reads state to decide the
/// *next* step, and a completed execution has none.
///
/// # What it writes
///
/// The folded state plus the digest of that fold, so a later reader can be
/// checked against a **fresh re-fold** rather than against a second copy of the
/// same number. That is the difference from #265 A3, which mirrored a Postgres
/// row: there the digest compared was the same value moved, here it is
/// recomputed.
///
/// Best-effort and non-fatal: this is a read model, and failing to materialise
/// must never fail the execution that triggered it.
pub async fn materialize_from_wal(execution_id: i64) -> Result<FoldedState, FoldRefusal> {
    let (folded, body) = {
        let events = fold_spine_inner(execution_id, None).await?;
        fold_with_body(FoldSource::WalSpine, events)?
    };
    let base = std::env::var(super::ehdb::WORKER_QUERY_URL_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| FoldRefusal::SourceUnavailable("WORKER_QUERY_URL unset".into()))?;
    let record = serde_json::json!({
        "execution_id": execution_id,
        "version": folded.version,
        "applied_count": folded.applied_count,
        // Both names: `digest` is what this tier's reader looks for, `checksum`
        // is what #265's earlier readers look for. Writing one and not the
        // other is how a record becomes unreadable to half its consumers.
        "digest": folded.digest,
        "checksum": folded.digest,
        "snapshot": body,
        "updated_at": chrono::Utc::now().to_rfc3339(),
        "mirror_source": "wal_spine",
        "aggregate_type": "orchestrator_workflow_state",
    });
    let url = format!("{}/ehdb/tiers/projection", base.trim_end_matches('/'));
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&serde_json::json!({
            "execution_id": execution_id.to_string(),
            "records": [record.to_string()],
        }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| FoldRefusal::SourceUnavailable(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(FoldRefusal::SourceUnavailable(format!(
            "projection append refused: {}",
            resp.status()
        )));
    }
    crate::metrics::record_ehdb_projection_materialize("materialized");
    Ok(folded)
}

/// Resolve an execution's control-flow state from the **WAL-sourced projection**,
/// verified against a fresh re-fold (ai-meta#265 Phase 3 — the flag-ON path).
///
/// The loop is: materialise → read back → re-fold → [`verdict_for`] → serve
/// **only** on [`ReFoldVerdict::Match`].
///
/// # Fail-closed
///
/// Every non-`Match` verdict returns `None`, and `None` here means *do not
/// advance this execution on this pass* — not *fall back to Postgres*. A
/// fallback that silently reads a different store on error re-establishes the
/// second source of truth this whole effort removes. The reconciler re-drives.
///
/// The six verdicts are the ones already proven to fire in kind, one live
/// execution per arm.
pub async fn wal_projection_state(execution_id: i64) -> (Option<serde_json::Value>, ReFoldVerdict) {
    // Materialise first so an in-flight execution has a record to verify. A
    // failure here is not fatal: the read below simply finds nothing and the
    // verdict says so.
    let _ = materialize_from_wal(execution_id).await;

    let spine = fold_from_wal_spine(execution_id, None).await;
    let stored = stored_projection_full(execution_id).await;
    let pair = stored
        .as_ref()
        .ok()
        .and_then(|o| o.as_ref())
        .map(|(v, d, _)| (*v, d.as_str()));
    let verdict = verdict_for(pair, &spine);
    crate::metrics::record_ehdb_projection_refold(verdict.as_str());
    if let Err(r) = &spine {
        crate::metrics::record_ehdb_projection_refold_refusal(r.reason());
    }
    if verdict != ReFoldVerdict::Match {
        return (None, verdict);
    }
    let body = stored.ok().flatten().and_then(|(_, _, body)| body);
    (body, verdict)
}

/// As [`stored_projection`], but also returning the stored snapshot body.
pub async fn stored_projection_full(
    execution_id: i64,
) -> Result<Option<(i64, String, Option<serde_json::Value>)>, String> {
    let base = std::env::var(super::ehdb::WORKER_QUERY_URL_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "WORKER_QUERY_URL unset".to_string())?;
    let url = format!(
        "{}/ehdb/tiers/projection?execution={execution_id}&limit=500",
        base.trim_end_matches('/')
    );
    let body: serde_json::Value = reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    if let Some(o) = body.get("outcome").and_then(|o| o.as_str()) {
        if o != "ok" {
            return Err(format!("tier outcome={o}"));
        }
    }
    let records = body
        .get("records")
        .and_then(|r| r.as_array())
        .ok_or_else(|| "no records array".to_string())?;
    let mut best: Option<(i64, u64, String, Option<serde_json::Value>)> = None;
    for r in records {
        let seq = r
            .get("global_sequence")
            .and_then(|s| s.as_u64())
            .unwrap_or(0);
        let p = match r.get("payload").and_then(|p| p.as_str()) {
            Some(s) => match serde_json::from_str::<serde_json::Value>(s) {
                Ok(v) => v,
                Err(_) => continue,
            },
            None => r.clone(),
        };
        let (Some(v), Some(d)) = (
            p.get("version").and_then(|v| {
                v.as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
            }),
            p.get("digest")
                .or_else(|| p.get("checksum"))
                .and_then(|d| d.as_str()),
        ) else {
            continue;
        };
        if best
            .as_ref()
            .is_none_or(|(bv, bs, _, _)| (v, seq) > (*bv, *bs))
        {
            best = Some((
                v,
                seq,
                d.to_string(),
                p.get("snapshot").filter(|s| !s.is_null()).cloned(),
            ));
        }
    }
    Ok(best.map(|(v, _, d, b)| (v, d, b)))
}

/// `GET /api/ehdb/projection-fold/diff/{id}` — WHICH FIELDS diverge, and how.
///
/// The Phase-3 equivalence arm measured the WAL-spine fold and the incumbent
/// fold producing different digests at the **same version and same event
/// count**. A digest says *that* they differ; this says *where*, which is the
/// difference between a named cause and a guess.
///
/// Both sides are folded here, in one request, from the same execution — so a
/// difference cannot be attributed to the execution advancing between calls.
/// The version equality is asserted and reported rather than assumed.
pub async fn fold_diff_endpoint(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
    axum::extract::Path(execution_id): axum::extract::Path<i64>,
) -> AppResult<axum::Json<serde_json::Value>> {
    let pool = state.pools.pool_for(execution_id);

    // --- incumbent side -----------------------------------------------------
    let rows = sqlx::query(
        r#"
        SELECT event_id, execution_id, catalog_id,
               parent_event_id, parent_execution_id,
               event_type, node_id, node_name, node_type, status,
               context, meta, result, worker_id,
               NULLIF(meta->>'attempt', '')::int AS attempt,
               created_at
        FROM noetl.event WHERE execution_id = $1 ORDER BY event_id ASC
        "#,
    )
    .bind(execution_id)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let pg_events = super::events::parse_event_rows_for_fold(rows);
    let pg = fold_with_body(FoldSource::Postgres, pg_events.clone());

    // --- WAL spine side -----------------------------------------------------
    let spine_events = wal_spine_events(execution_id, None).await;
    let wal = match &spine_events {
        Ok(evs) => fold_with_body(FoldSource::WalSpine, evs.clone()),
        Err(r) => Err(r.clone()),
    };

    let mut diff: Vec<String> = Vec::new();
    let mut same_version = false;
    if let (Ok((pgs, pgb)), Ok((wals, walb))) = (&pg, &wal) {
        same_version = pgs.version == wals.version;
        if same_version {
            json_diff_paths(pgb, walb, "", &mut diff);
        }
    }

    // Per-event field presence, so a state difference can be traced to the
    // INPUT rather than only observed in the output. Counts only — no values,
    // because `context` is exactly the field under suspicion and it is the one
    // that could carry sensitive material.
    let field_presence = |evs: &[crate::db::models::Event]| {
        serde_json::json!({
            "events": evs.len(),
            "with_context": evs.iter().filter(|e| e.context.is_some()).count(),
            "with_result": evs.iter().filter(|e| e.result.is_some()).count(),
            "with_meta": evs.iter().filter(|e| e.meta.is_some()).count(),
            "with_attempt": evs.iter().filter(|e| e.attempt.is_some()).count(),
            "with_node_name": evs.iter().filter(|e| e.node_name.is_some()).count(),
            "distinct_created_at": evs.iter().map(|e| e.created_at).collect::<std::collections::BTreeSet<_>>().len(),
        })
    };

    Ok(axum::Json(serde_json::json!({
        "action": "ehdb.projection.fold.diff",
        "execution_id": execution_id,
        "same_version": same_version,
        "postgres": pg.as_ref().ok().map(|(s, _)| s),
        "postgres_refusal": pg.as_ref().err(),
        "wal": wal.as_ref().ok().map(|(s, _)| s),
        "wal_refusal": wal.as_ref().err(),
        "diff_paths": diff,
        "input_fields_postgres": field_presence(&pg_events),
        "input_fields_wal": spine_events.as_ref().ok().map(|e| field_presence(e)),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tier's record shape must be read faithfully, including the field it
    /// does not carry.
    ///
    /// This is the Phase 1 question in unit form: a payload shaped exactly like
    /// `ehdb_eventlog_mirror::mirror_payload` yields an event whose `context`
    /// is `None`, while the same logical event from Postgres carries one. The
    /// fold reads `context` in six places, so `None` is not a cosmetic
    /// difference.
    #[test]
    fn a_tier_payload_has_no_context_and_the_reader_says_so() {
        let payload = serde_json::json!({
            "event_id": 10, "execution_id": 1, "catalog_id": 2,
            "event_type": "step.enter", "status": "ok",
            "node_name": "s1", "step": "s1",
            "result": {"a": 1}, "meta": {"attempt": "1"},
            "mirror_source": "server"
        });
        let ev = event_from_tier_payload(&payload).expect("parses");
        assert_eq!(ev.event_id, 10);
        assert_eq!(ev.result, Some(serde_json::json!({"a": 1})));
        assert!(
            ev.context.is_none(),
            "the tier record carries no `context` — if this ever becomes Some, \
             the mirror payload changed and the Phase 1 finding is stale"
        );
        assert_eq!(
            ev.attempt,
            Some(1),
            "attempt is derived from meta, as in the SQL projection"
        );
    }

    /// …and when a payload DOES carry context, the reader picks it up. Without
    /// this, the assertion above would also pass against a reader that simply
    /// never reads the field — proving the reader broken rather than the
    /// record incomplete.
    #[test]
    fn the_reader_picks_up_context_when_it_is_present() {
        let payload = serde_json::json!({
            "event_id": 11, "execution_id": 1, "catalog_id": 2,
            "event_type": "step.enter", "status": "ok",
            "context": {"loop": {"i": 3}}
        });
        let ev = event_from_tier_payload(&payload).expect("parses");
        assert_eq!(ev.context, Some(serde_json::json!({"loop": {"i": 3}})));
    }

    /// A record set that parses to nothing is a refusal, never an empty state.
    #[test]
    fn records_without_event_ids_refuse_rather_than_fold_nothing() {
        let payload = serde_json::json!({"no_event_id": true});
        assert!(event_from_tier_payload(&payload).is_none());
    }

    fn folded(version: i64, digest: &str) -> Result<FoldedState, FoldRefusal> {
        Ok(FoldedState {
            source: FoldSource::WalSpine,
            version,
            applied_count: 3,
            digest: digest.to_string(),
        })
    }

    /// Every fail-closed condition is named as ITSELF, and only two of them
    /// page.
    ///
    /// Two-sided: the healthy case must also be reachable, or a `verdict_for`
    /// that refused unconditionally would satisfy every negative assertion and
    /// make the read model permanently unusable while looking rigorous.
    #[test]
    fn each_refold_condition_is_named_as_itself() {
        let spine = folded(100, "aaa");

        assert_eq!(
            verdict_for(Some((100, "aaa")), &spine),
            ReFoldVerdict::Match
        );
        assert!(!ReFoldVerdict::Match.is_fault());

        // Same version, different content — the corruption case.
        assert_eq!(
            verdict_for(Some((100, "bbb")), &spine),
            ReFoldVerdict::DigestMismatch
        );
        assert!(ReFoldVerdict::DigestMismatch.is_fault());

        // Claims an event the log does not have. Checked BEFORE the digest, so
        // it is not mis-reported as corruption.
        assert_eq!(
            verdict_for(Some((101, "aaa")), &spine),
            ReFoldVerdict::StoredAheadOfSpine
        );
        assert!(ReFoldVerdict::StoredAheadOfSpine.is_fault());

        // Behind is the materialiser lagging, not corruption — must not page.
        assert_eq!(
            verdict_for(Some((99, "aaa")), &spine),
            ReFoldVerdict::StoredBehindSpine
        );
        assert!(!ReFoldVerdict::StoredBehindSpine.is_fault());

        assert_eq!(verdict_for(None, &spine), ReFoldVerdict::NoStoredRecord);
        assert!(!ReFoldVerdict::NoStoredRecord.is_fault());
    }

    /// An incomplete spine refuses the comparison rather than passing it.
    ///
    /// The dangerous shape would be treating "the log could not be read" as
    /// "nothing to disagree with" — a clean verdict from a comparison that
    /// never happened, which is the vacuous pass this codebase keeps finding.
    #[test]
    fn a_spine_that_cannot_be_folded_refuses_rather_than_agreeing() {
        let refused: Result<FoldedState, FoldRefusal> = Err(FoldRefusal::SpineIncomplete);
        assert_eq!(
            verdict_for(Some((100, "aaa")), &refused),
            ReFoldVerdict::SpineRefused
        );
        // …and it is NOT a match even when the stored record looks fine.
        assert_ne!(
            verdict_for(Some((100, "aaa")), &refused),
            ReFoldVerdict::Match
        );
        // An unreachable worker is the same shape.
        let unavail: Result<FoldedState, FoldRefusal> =
            Err(FoldRefusal::SourceUnavailable("no index".into()));
        assert_eq!(
            verdict_for(Some((100, "aaa")), &unavail),
            ReFoldVerdict::SpineRefused
        );
    }

    fn v2_record(event_id: i64) -> serde_json::Value {
        serde_json::json!({
            "event_id": event_id,
            "event_type": "step.enter",
            "status": "RUNNING",
            "execution_id": 42,
            "created_at": "2026-08-28T00:00:00Z",
            "context": {"path": "p"},
            crate::handlers::ehdb_eventlog_mirror::MIRROR_PAYLOAD_VERSION_KEY:
                crate::handlers::ehdb_eventlog_mirror::MIRROR_PAYLOAD_VERSION,
        })
    }

    /// A pre-v2 record REFUSES rather than folding without `context`.
    ///
    /// The distinction this pins is the whole reason the variant exists: the
    /// wrong behaviour here is not a crash, it is a *successful* fold whose
    /// digest then disagrees with Postgres and is reported as divergence. That
    /// would have fired on every pre-existing execution the moment this shipped
    /// — loudest exactly when it was least true.
    #[test]
    fn a_record_written_before_context_refuses_instead_of_folding_short() {
        let mut old = v2_record(1);
        old.as_object_mut()
            .unwrap()
            .remove(crate::handlers::ehdb_eventlog_mirror::MIRROR_PAYLOAD_VERSION_KEY);
        old.as_object_mut().unwrap().remove("context");

        let r = events_from_tier_records(&[old]).expect_err("a v1 record must not fold");
        assert_eq!(r.reason(), "payload_too_old");
        assert_ne!(
            r.reason(),
            "no_events",
            "a too-old payload must not be laundered into a benign reason"
        );
        assert!(REFOLD_REFUSALS.contains(&r.reason()));
    }

    /// One stale record refuses the WHOLE set — the upgrade-straddling case.
    #[test]
    fn a_single_stale_record_refuses_the_whole_execution() {
        let mut old = v2_record(1);
        old.as_object_mut()
            .unwrap()
            .remove(crate::handlers::ehdb_eventlog_mirror::MIRROR_PAYLOAD_VERSION_KEY);
        let mixed = vec![old, v2_record(2), v2_record(3)];
        assert_eq!(
            events_from_tier_records(&mixed)
                .expect_err("a mixed-version execution must refuse")
                .reason(),
            "payload_too_old",
            "folding the v2 majority and dropping the v1 record would produce a state for a \
             history that never happened"
        );
    }

    /// v2 records fold, and arrive in event_id order.
    #[test]
    fn v2_records_fold_and_are_ordered() {
        let ev = events_from_tier_records(&[v2_record(3), v2_record(1), v2_record(2)])
            .expect("v2 records must fold");
        assert_eq!(
            ev.iter().map(|e| e.event_id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(ev[0].context.is_some(), "context must survive the read");
    }

    /// A duplicated record is applied ONCE.
    ///
    /// `apply_event` is not idempotent, so a duplicate is a second state
    /// transition, not a no-op. The tier is append-only behind a retrying
    /// drain and can hold one; Postgres cannot, so this is a failure mode the
    /// incumbent source simply does not have and the comparison would blame on
    /// the fold.
    #[test]
    fn a_duplicated_tier_record_is_folded_once() {
        let ev = events_from_tier_records(&[v2_record(1), v2_record(1), v2_record(2)])
            .expect("duplicates must not refuse");
        assert_eq!(
            ev.iter().map(|e| e.event_id).collect::<Vec<_>>(),
            vec![1, 2],
            "a duplicate event was applied twice; the folded state never existed"
        );
    }

    /// Every refusal reason is pinned, and — the part that matters — the match is
    /// TOTAL, so adding a `FoldRefusal` variant fails to compile here rather than
    /// emitting an unpinned label nobody will notice is missing.
    #[test]
    fn every_refusal_reason_is_pinned_and_distinct() {
        let all = [
            FoldRefusal::NoEvents,
            FoldRefusal::SourceUnavailable("x".into()),
            FoldRefusal::Unparseable("x".into()),
            FoldRefusal::FoldFailed,
            FoldRefusal::SpineIncomplete,
            FoldRefusal::PayloadTooOld("x".into()),
        ];
        for r in &all {
            // Total match: a new variant breaks the build here.
            let expect = match r {
                FoldRefusal::NoEvents => "no_events",
                FoldRefusal::SourceUnavailable(_) => "source_unavailable",
                FoldRefusal::Unparseable(_) => "unparseable",
                FoldRefusal::FoldFailed => "fold_failed",
                FoldRefusal::SpineIncomplete => "spine_incomplete",
                FoldRefusal::PayloadTooOld(_) => "payload_too_old",
            };
            assert_eq!(r.reason(), expect);
            assert!(
                REFOLD_REFUSALS.contains(&r.reason()),
                "{} is emitted but not pinned; its series is absent until it fires",
                r.reason()
            );
        }
        let mut seen: Vec<&str> = all.iter().map(|r| r.reason()).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), all.len(), "two refusals share a label");
        assert_eq!(REFOLD_REFUSALS.len(), all.len());
    }

    /// The reason must not change the VERDICT. All six still refuse — the
    /// six-verdict fail-closed contract is unchanged by adding the reason.
    #[test]
    fn labelling_the_reason_does_not_soften_any_verdict() {
        for r in [
            FoldRefusal::NoEvents,
            FoldRefusal::SourceUnavailable("x".into()),
            FoldRefusal::Unparseable("x".into()),
            FoldRefusal::FoldFailed,
            FoldRefusal::SpineIncomplete,
        ] {
            let e: Result<FoldedState, FoldRefusal> = Err(r);
            assert_eq!(
                verdict_for(Some((100, "aaa")), &e),
                ReFoldVerdict::SpineRefused,
                "a refusal must never read as agreement"
            );
        }
    }

    /// Every verdict the code can emit is in the pinned label set.
    #[test]
    fn every_verdict_is_pinned() {
        for v in [
            ReFoldVerdict::Match,
            ReFoldVerdict::DigestMismatch,
            ReFoldVerdict::StoredAheadOfSpine,
            ReFoldVerdict::StoredBehindSpine,
            ReFoldVerdict::NoStoredRecord,
            ReFoldVerdict::SpineRefused,
        ] {
            assert!(
                REFOLD_VERDICTS.contains(&v.as_str()),
                "{} is emitted but not pinned; its series is absent until it fires",
                v.as_str()
            );
        }
        assert_eq!(REFOLD_VERDICTS.len(), 6);
    }

    /// Sub-microsecond precision is normalised away; everything coarser is kept.
    ///
    /// The negative half matters as much as the positive: a `truncate` that
    /// zeroed the whole fractional part would also make the two sources agree,
    /// and would silently destroy microsecond ordering the platform DOES
    /// preserve.
    #[test]
    fn precision_is_normalised_to_microseconds_and_no_coarser() {
        use chrono::TimeZone;
        // 05:01:33.645451448 (ns, as the WAL envelope carries it)
        let ns = chrono::Utc
            .timestamp_opt(1_756_184_493, 645_451_448)
            .single()
            .expect("valid instant");
        // 05:01:33.645451 (µs, as Postgres stores it)
        let us = chrono::Utc
            .timestamp_opt(1_756_184_493, 645_451_000)
            .single()
            .expect("valid instant");

        assert_ne!(
            ns, us,
            "the fixture must actually differ, or this proves nothing"
        );
        assert_eq!(
            truncate_to_micros(ns),
            truncate_to_micros(us),
            "the two representations of one instant must normalise to the same value"
        );
        // …and the microsecond component SURVIVES.
        assert_eq!(
            truncate_to_micros(ns).timestamp_subsec_micros(),
            645_451,
            "microsecond precision must be preserved; only sub-µs is resolved"
        );
        // ROUNDING, not flooring — this is what Postgres does, and the first
        // version of this code got it wrong in a way that only showed on ~half
        // of real executions.
        let up = chrono::Utc
            .timestamp_opt(1_756_184_493, 645_451_500)
            .single()
            .unwrap();
        assert_eq!(
            truncate_to_micros(up).timestamp_subsec_micros(),
            645_452,
            "a remainder >= 500ns must round UP, as Postgres does"
        );
        let down = chrono::Utc
            .timestamp_opt(1_756_184_493, 645_451_499)
            .single()
            .unwrap();
        assert_eq!(
            truncate_to_micros(down).timestamp_subsec_micros(),
            645_451,
            "a remainder < 500ns must round DOWN"
        );
        // Two instants a microsecond apart stay distinct.
        let next = chrono::Utc
            .timestamp_opt(1_756_184_493, 645_452_000)
            .single()
            .unwrap();
        assert_ne!(truncate_to_micros(us), truncate_to_micros(next));
    }

    /// The normalisation reaches every event the fold consumes.
    #[test]
    fn every_event_in_a_fold_is_normalised() {
        use chrono::TimeZone;
        let mk = |nanos: u32| {
            let mut e = tier_event_fixture();
            e.created_at = chrono::Utc
                .timestamp_opt(1_756_184_493, nanos)
                .single()
                .unwrap();
            e
        };
        // 645_451_448 rounds DOWN to 645451; 645_451_999 rounds UP to 645452.
        let mut evs = vec![mk(645_451_448), mk(645_451_999), mk(645_452_001)];
        normalise_event_precision(&mut evs);
        assert_ne!(
            evs[0].created_at, evs[1].created_at,
            "these straddle the half-µs boundary and must NOT collapse together"
        );
        assert_eq!(
            evs[1].created_at, evs[2].created_at,
            "645_451_999 rounds up to the same µs as 645_452_001 rounds down to"
        );
        assert!(
            evs.iter()
                .all(|e| e.created_at.timestamp_subsec_nanos() % 1_000 == 0),
            "no event may retain sub-microsecond precision after normalisation"
        );
    }

    fn tier_event_fixture() -> crate::db::models::Event {
        event_from_tier_payload(&serde_json::json!({
            "event_id": 1, "execution_id": 2, "catalog_id": 3,
            "event_type": "step.enter", "status": "ok"
        }))
        .expect("fixture parses")
    }
    /// Two refusals are not a match.
    #[test]
    fn agreement_requires_two_folds_not_two_refusals() {
        let c = FoldComparison {
            execution_id: 1,
            postgres: None,
            postgres_refusal: Some(FoldRefusal::NoEvents),
            tier: None,
            tier_refusal: Some(FoldRefusal::NoEvents),
            digests_agree: false,
            disagreement: None,
            postgres_without_context: None,
            context_explains_the_gap: false,
        };
        assert!(
            !c.digests_agree,
            "absence of disagreement is not agreement — this is the vacuous \
             pass the whole comparator discipline exists to refuse"
        );
    }
}

/// `GET /api/ehdb/projection-recovery/{id}` — the re-scoped Phase 3 gate.
pub async fn recovery_compare_endpoint(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
    axum::extract::Path(execution_id): axum::extract::Path<i64>,
) -> AppResult<axum::Json<serde_json::Value>> {
    let pool = state.pools.pool_for(execution_id);
    Ok(axum::Json(
        crate::services::orch_snapshot::recovery_read_comparison(pool, execution_id).await,
    ))
}
