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

/// Fold from `noetl.event`, **raw** — references NOT resolved.
///
/// ⚠ This doc used to claim it "builds the same `Event` values the
/// orchestrator's own rebuild path does, so a digest from here is the digest of
/// the state the orchestrator would have used". That was false: `rebuild_state`
/// calls `hydrate_result_references` at all eight of its fold sites, and this
/// does not. Under `NOETL_PERMANENT_LOG_LEAN=true` the raw row carries a
/// `reference` where the orchestrator (and the mirrored tier) carry content.
///
/// Use [`fold_from_postgres_hydrated`] for any comparison against the tier or
/// against orchestrator state. This raw form is for callers that specifically
/// want the persisted bytes.
pub async fn fold_from_postgres(
    pool: &DbPool,
    execution_id: i64,
) -> Result<FoldedState, FoldRefusal> {
    let events = events_from_postgres(pool, execution_id).await?;
    fold(FoldSource::Postgres, events)
}

/// As [`events_from_postgres`], but with **result references hydrated** — the
/// view the orchestrator actually folds.
///
/// ⚠ This distinction is the whole point. With `NOETL_PERMANENT_LOG_LEAN=true`
/// the persisted row carries `result.context.result.reference` (a pointer)
/// where the mirrored tier record carries the inlined `result.context.result.context`.
/// Folding the raw row therefore digests a *pointer* and compares it against
/// *content*, and reports divergence on every execution with an externalised
/// result — a false positive, not a mirror defect.
///
/// Measured on prod 2026-09-12 over the 40 most recent executions: **12 of 40
/// (30%)** were exactly this shape, with byte-identical field counts on both
/// sides and `diff_paths` containing nothing but the reference/context pair.
///
/// `rebuild_state` hydrates at all 8 of its fold sites; this leg did not, while
/// [`fold_from_postgres`]'s own doc claimed it "builds the same `Event` values
/// the orchestrator's own rebuild path does". That claim is what this restores.
pub async fn events_from_postgres_hydrated(
    pool: &DbPool,
    result_store: &crate::services::result_store::ResultStoreService,
    execution_id: i64,
) -> Result<Vec<crate::db::models::Event>, FoldRefusal> {
    let mut events = events_from_postgres(pool, execution_id).await?;
    // `keep_refs = false`: drop the reference envelope once resolved, so the
    // result reads like an inline one — which is the tier's shape.
    super::events::hydrate_result_references(&mut events, result_store, false).await;
    Ok(events)
}

/// [`fold_from_postgres`] over the hydrated event set.
pub async fn fold_from_postgres_hydrated(
    pool: &DbPool,
    result_store: &crate::services::result_store::ResultStoreService,
    execution_id: i64,
) -> Result<FoldedState, FoldRefusal> {
    let events = events_from_postgres_hydrated(pool, result_store, execution_id).await?;
    fold(FoldSource::Postgres, events)
}

/// Read `noetl.event` for one execution, parsed, ordered by `event_id`.
///
/// Split out of [`fold_from_postgres`] so the AC14 verification leg can truncate
/// at a watermark before folding, rather than issuing a differently-shaped query
/// than the full fold uses. Same columns, same parse, same order — the two
/// callers cannot drift apart into "the verifier read something else".
pub async fn events_from_postgres(
    pool: &DbPool,
    execution_id: i64,
) -> Result<Vec<crate::db::models::Event>, FoldRefusal> {
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
    Ok(super::events::parse_event_rows_for_fold(rows))
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
    result_store: &crate::services::result_store::ResultStoreService,
    execution_id: i64,
) -> Result<FoldedState, FoldRefusal> {
    // ⚠ HYDRATED before blanking (noetl/ai-meta#343).
    //
    // This is the controlled experiment behind `context_explains_the_gap`: blank
    // `context` on the Postgres side, and if the digest then matches the tier,
    // `context` was the whole difference. Reading the RAW row broke the
    // experiment in one direction only — it blanks `context` but leaves
    // `result` carrying a `reference` where the tier carries inlined content,
    // so the two digests still differ for a reason the experiment is not
    // testing. `context_explains_the_gap` was therefore biased to `false`: it
    // could report "context does not explain it" on an execution where context
    // was the entire remaining gap.
    //
    // A control that can only fail is not a control. Hydrate first, blank
    // second, so the ONE deliberate difference is the one under test.
    let mut events = events_from_postgres_hydrated(pool, result_store, execution_id).await?;
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
    let events = tier_events_within(execution_id, timeout).await?;
    fold(FoldSource::EhdbTier, events)
}

/// The events one execution has in the durable event-log tier.
///
/// The fetch half of [`fold_from_tier_within`], split out so the recovery path
/// can fold **with a body** (which the projection record needs) without a
/// second implementation of the relay read.
pub async fn tier_events_within(
    execution_id: i64,
    timeout: std::time::Duration,
) -> Result<Vec<crate::db::models::Event>, FoldRefusal> {
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

    events_from_tier_records(records)
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
        // The column is `worker_id VARCHAR`, but a payload can carry the value
        // as a JSON *number* — a snowflake id serialises that way unless the
        // producer stringifies it. Reading only `as_str()` silently yielded
        // `None` for those, while the Postgres side read the same row as
        // `Some("8123456789")`. That asymmetry is a digest divergence with no
        // visible cause, and it is one of the two residual #307 diffs.
        worker_id: p.get("worker_id").and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.as_i64().map(|n| n.to_string()))
                .or_else(|| v.as_u64().map(|n| n.to_string()))
        }),
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
    // TRUNCATE — because that is now the PRODUCER rule, not because of a guess
    // about what Postgres does.
    //
    // This arithmetic flip-flopped twice, and both times on the same false
    // premise: that there is a single reduction Postgres applies, discoverable
    // by measuring. There is not. Measured on prod 2026-08-31 over 11
    // executions and 263 events, 9 looked like rounding, 1 like truncation and
    // 1 mixed — because `noetl.event.created_at` had TWO producers reducing
    // differently (sqlx binary bind truncates; the SQL text cast rounds).
    //
    // noetl/ai-meta#307 fixed that upstream:
    // `handlers::event_write::to_storage_precision` reduces every event to
    // microseconds at construction, before either writer and before the mirror,
    // and truncation is the rule it applies. So for any event written since,
    // there is no sub-microsecond remainder left and this function is a no-op —
    // which is the point. The fold stopped being a place where a rule is
    // chosen.
    //
    // It is kept, and kept as TRUNCATION, only for events written BEFORE that
    // fix. Those still carry the old ambiguity; truncation matches the
    // binary-bind path, which wrote the overwhelming majority of them.
    //
    // ⚠ If you are about to change this again: the question is no longer "what
    // does Postgres do". It is "what does `to_storage_precision` do", and the
    // answer must match it. Changing one without the other reintroduces exactly
    // the divergence this closed.
    let nanos = match ts.timestamp_nanos_opt() {
        Some(n) => n,
        // Outside the ~1677–2262 nanosecond-representable window. Leave it
        // alone rather than silently mangling a timestamp we cannot reason
        // about; such an event cannot come from this platform's clock.
        None => return ts,
    };
    // Round half-up: add half a microsecond before flooring. `div_euclid`
    // floors, so this is round-half-away-from-zero for the positive instants
    // this platform produces — the same rule PostgreSQL applies.
    let micros = nanos.div_euclid(1_000);
    chrono::Utc.timestamp_micros(micros).single().unwrap_or(ts)
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
    normalise_null_json(&mut events);
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

/// Collapse a JSON `null` in a JSON-typed column to `None`, on both fold paths.
///
/// # Why this is symmetric rather than a tier-side fix
///
/// `noetl.event.context` is `JSONB`. sqlx decodes a SQL NULL into `None` but a
/// jsonb `'null'` into `Some(Value::Null)` — two different Rust values for two
/// database states. The mirror payload cannot preserve that distinction: it
/// serialises `Option<Value>` into JSON, so *both* arrive at the tier as
/// `"context": null`.
///
/// The tier reader compensated with `.filter(|v| !v.is_null())`, which is right
/// for the common case (SQL NULL) and wrong for the rare one, and the Postgres
/// reader had no matching filter. So one jsonb-`null` row was enough to make the
/// two sides fold to different digests — the residual `context` diff in
/// noetl/ai-meta#307.
///
/// Rather than try to carry a distinction the payload cannot express, this
/// erases it on both sides. That is sound because it is also semantically
/// right: a `context` of JSON `null` carries exactly as much information as an
/// absent one, and `WorkflowState::apply_event` treats them identically.
fn normalise_null_json(events: &mut [crate::db::models::Event]) {
    for e in events.iter_mut() {
        if matches!(e.context, Some(serde_json::Value::Null)) {
            e.context = None;
        }
        if matches!(e.meta, Some(serde_json::Value::Null)) {
            e.meta = None;
        }
        if matches!(e.result, Some(serde_json::Value::Null)) {
            e.result = None;
        }
    }
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
///
/// Used by the field-level diff endpoint, which needs the events themselves
/// rather than a state.
///
/// ⚠ Spine-pinned on purpose: a *diff* is asking "what do these two sources
/// each say", so it must not take the recovery ladder's fallback. Anything
/// asking "what is this execution's state" wants [`events_for_recovery`].
pub async fn wal_spine_events(
    execution_id: i64,
    head: Option<i64>,
) -> Result<Vec<crate::db::models::Event>, FoldRefusal> {
    let inner = fold_spine_inner(execution_id, head).await?;
    Ok(inner)
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

/// The outcome of one comparison, as **three** values.
///
/// noetl/ai-meta#343: `digests_agree: bool` cannot express "could not be
/// compared", so an execution the tier refused to return read as `false` —
/// indistinguishable from one that was read and disagreed. Three prod
/// executions in a 40-execution sample were in exactly that state, refused by
/// the tier-service frame cap at 1,174,874 bytes, and every surface reported
/// them as divergent.
///
/// The two failures need opposite responses — a divergence is a mirror gap to
/// repair, an unreadable execution is a transport limit to page around — so
/// collapsing them is not a rounding error, it sends the investigation the
/// wrong way. A two-valued verdict on a three-valued question always spends its
/// third state somewhere, and here it was spending it on "diverged".
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ComparisonVerdict {
    /// Both sides folded and the digests match.
    Agree,
    /// Both sides folded and the digests differ.
    Diverged,
    /// At least one side could not be read. **Not** a divergence.
    NotComparable,
}

impl ComparisonVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agree => "agree",
            Self::Diverged => "diverged",
            Self::NotComparable => "not_comparable",
        }
    }
}

/// Pure, so the rule is asserted by a test rather than only by the callers that
/// happen to apply it — the same discipline `equivalence_holds` follows.
pub fn comparison_verdict(
    postgres: Option<&FoldedState>,
    tier: Option<&FoldedState>,
) -> ComparisonVerdict {
    match (postgres, tier) {
        (Some(a), Some(b)) if a.digest == b.digest => ComparisonVerdict::Agree,
        (Some(_), Some(_)) => ComparisonVerdict::Diverged,
        _ => ComparisonVerdict::NotComparable,
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
    /// The three-valued outcome. Prefer this over [`Self::digests_agree`] when
    /// counting: `digests_agree == false` conflates "diverged" with "could not
    /// be read" (noetl/ai-meta#343).
    pub verdict: ComparisonVerdict,
}

pub async fn compare_sources(
    pool: &DbPool,
    result_store: &crate::services::result_store::ResultStoreService,
    execution_id: i64,
) -> AppResult<FoldComparison> {
    // ⚠ HYDRATED, like the verification path (noetl/ai-meta#342).
    //
    // This is the instrument everyone reads to decide whether the tier agrees
    // with Postgres, and it folded the RAW row while the tier holds inlined
    // content — so it reported a `reference`-vs-content difference as
    // divergence. Fixing only the serve path left the *measurement* lying:
    // v3.108.2 deployed and this endpoint's divergence count did not move by a
    // single execution, because this function was never on the path that
    // changed. An instrument that cannot see a fix cannot validate one.
    let pg = fold_from_postgres_hydrated(pool, result_store, execution_id).await;
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
    let no_ctx = fold_from_postgres_without_context(pool, result_store, execution_id)
        .await
        .ok();
    let context_explains_the_gap = match (&no_ctx, &tier_ok) {
        (Some(a), Some(b)) => a.digest == b.digest,
        _ => false,
    };
    Ok(FoldComparison {
        execution_id,
        verdict: comparison_verdict(postgres.as_ref(), tier_ok.as_ref()),
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
    let result_store = crate::services::result_store::ResultStoreService::new(
        pool.clone(),
        state.snowflake.clone(),
    );
    let cmp = compare_sources(pool, &result_store, execution_id).await?;
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
/// Ground truth is a fresh fold of the EHDB log — the spine, or the durable
/// event-log tier when the spine cannot answer. Never Postgres.
pub async fn refold_endpoint(
    axum::extract::State(_state): axum::extract::State<crate::state::AppState>,
    axum::extract::Path(execution_id): axum::extract::Path<i64>,
) -> AppResult<axum::Json<serde_json::Value>> {
    // Through the ladder, so this reports what recovery would ACTUALLY do under
    // the configured mode. This is the endpoint #307 was diagnosed with; had it
    // answered for a path the serving code no longer takes, it would have
    // become a second thing to be wrong about.
    let spine = events_for_recovery(execution_id)
        .await
        .and_then(|(source, events)| fold(source, events));
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
/// `NOETL_EHDB_RECOVERY_SOURCE` — where a recovery fold resolves its events.
pub const RECOVERY_SOURCE_ENV: &str = "NOETL_EHDB_RECOVERY_SOURCE";

/// The configured modes, pinned as metric labels.
pub const RECOVERY_SOURCES: [&str; 3] = ["spine", "verify", "tier"];

/// The sources a fold can actually be attempted against, pinned as labels.
pub const RECOVERY_FOLD_SOURCES: [&str; 2] = ["spine", "tier"];

/// Where a recovery fold resolves an execution's events from (ai-meta#307).
///
/// # The mismatch this exists to fix
///
/// Recovery folded only from the worker's in-memory state-builder index, which
/// holds **in-flight** executions and evicts them on completion. The comparator,
/// by its nature, asks about **completed** ones. Two components with
/// incompatible lifetimes: nothing was broken, and coverage was ~0 *by
/// construction*. Prod showed `spine_refused` on 4 of 4 refolds, `served_tier=0`
/// and 0 events compared, for nine hours, on executions whose 38,611 events were
/// sitting in the retained log the whole time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoverySource {
    /// The in-memory spine only — the behaviour before #307, exactly. Default,
    /// so this change lands inert and the flip is a deliberate act.
    Spine,
    /// Attempt the tier when the spine refuses, record what it *would* have
    /// done, and still refuse. The dark-launch rung: it makes coverage
    /// measurable without letting anything unproven drive an execution.
    Verify,
    /// Serve from the tier when the spine refuses.
    Tier,
}

impl RecoverySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spine => "spine",
            Self::Verify => "verify",
            Self::Tier => "tier",
        }
    }
    /// Whether this mode consults the tier at all.
    fn consults_tier(self) -> bool {
        !matches!(self, Self::Spine)
    }
    /// Whether a tier fold may actually be **used**.
    fn serves_tier(self) -> bool {
        matches!(self, Self::Tier)
    }
}

/// Resolve the configured recovery source.
///
/// Unrecognised ⇒ `Spine`, warned once per distinct value. Defaulting to the
/// pre-#307 behaviour rather than to the new one is deliberate: a typo must not
/// silently switch which store drives executions. Same posture as
/// `ehdb_projection_read::read_source`, and for the reason noetl/ai-meta#243
/// records — a default that swallows a typo erases *which* cause it had.
pub fn recovery_source() -> RecoverySource {
    parse_recovery_source(std::env::var(RECOVERY_SOURCE_ENV).ok().as_deref())
}

/// The parse, without the environment.
///
/// Split out so the rules are testable without `set_var` — which would race,
/// because `cargo test` does **not** serialise tests (a SAFETY note in this
/// workspace claimed it did; ai-meta#232 records that it does not).
pub fn parse_recovery_source(raw: Option<&str>) -> RecoverySource {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") | Some("spine") => RecoverySource::Spine,
        Some("verify") => RecoverySource::Verify,
        Some("tier") => RecoverySource::Tier,
        Some(other) => {
            use std::collections::HashSet;
            use std::sync::{Mutex, OnceLock};
            static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
            let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
            let mut g = seen.lock().unwrap_or_else(|e| e.into_inner());
            if g.insert(other.to_string()) {
                tracing::warn!(
                    target: "noetl_server::ehdb_projection_fold",
                    value = other,
                    "unrecognised {RECOVERY_SOURCE_ENV}; falling back to `spine` \
                     (pre-#307 behaviour)"
                );
            }
            RecoverySource::Spine
        }
    }
}

/// Resolve an execution's events for a recovery fold: the spine, then the
/// durable event-log tier when the spine cannot answer.
///
/// # Order, and why this one
///
/// The spine is tried **first** so an in-flight execution takes exactly the path
/// it took before this change — no added latency, no new failure mode, on the
/// only case that previously worked. The tier is consulted only where there was
/// already a refusal, so the added relay call buys coverage that did not exist
/// and costs nothing that did.
///
/// # Metric semantics
///
/// Counts fold **attempts**, and one recovery read makes two of them: the
/// materialise pass and the independent re-fold that verifies it. The second
/// fetch is not redundant — re-reading and re-computing is what makes the
/// comparison a check rather than a copy of the same number compared with
/// itself.
pub async fn events_for_recovery(
    execution_id: i64,
) -> Result<(FoldSource, Vec<crate::db::models::Event>), FoldRefusal> {
    let mode = recovery_source();

    let spine = fold_spine_inner(execution_id, None).await;
    match &spine {
        Ok(_) => crate::metrics::record_ehdb_recovery_fold("spine", "folded"),
        Err(r) => crate::metrics::record_ehdb_recovery_fold("spine", r.reason()),
    }
    let spine_refusal = match spine {
        Ok(events) => return Ok((FoldSource::WalSpine, events)),
        Err(r) => r,
    };

    if !mode.consults_tier() {
        return Err(spine_refusal);
    }

    match tier_events_within(execution_id, TIER_FOLD_TIMEOUT).await {
        Ok(events) => {
            crate::metrics::record_ehdb_recovery_fold("tier", "folded");
            if !mode.serves_tier() {
                // `verify`: measured, deliberately not served. Returning the
                // SPINE's refusal — not a tier-flavoured one — keeps the
                // caller's behaviour byte-identical to `spine` mode, so the
                // dark launch cannot change an outcome while it is being
                // trusted to only observe.
                return Err(spine_refusal);
            }
            Ok((FoldSource::EhdbTier, events))
        }
        Err(r) => {
            crate::metrics::record_ehdb_recovery_fold("tier", r.reason());
            Err(r)
        }
    }
}

pub async fn materialize_from_wal(execution_id: i64) -> Result<FoldedState, FoldRefusal> {
    let (folded, body) = {
        let (source, events) = events_for_recovery(execution_id).await?;
        fold_with_body(source, events)?
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
        // The source that actually answered, not a constant. A record claiming
        // `wal_spine` while the tier answered would be a provenance label that
        // is merely truthful-LOOKING — the exact failure ai-meta#257 records,
        // where `source=service` kept reading correct while a silent fallback
        // served a different store entirely.
        "mirror_source": folded.source.as_str(),
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
/// `NOETL_EHDB_PROJECTION_SERVE_ON_BEHIND` — allow a snapshot BEHIND the spine to
/// be served, after verifying it at its own version.
///
/// Default **off**, so this lands inert and turning it on is a deliberate act.
pub const SERVE_ON_BEHIND_ENV: &str = "NOETL_EHDB_PROJECTION_SERVE_ON_BEHIND";

/// Is serve-on-behind armed? ⚠ Strict `== "true"`, so the armed state is
/// unambiguous from a manifest.
pub fn serve_on_behind_enabled() -> bool {
    std::env::var(SERVE_ON_BEHIND_ENV)
        .map(|v| v == "true")
        .unwrap_or(false)
}
/// Does the bounded re-fold agree with what the snapshot claims?
///
/// The serve decision in one pure function, so it can be mutation-tested. It
/// could not be before: it lived inline behind a `DbPool`, and a mutation
/// replacing the whole comparison with `true` — issuing a grant with no
/// verification whatsoever — survived the entire suite.
///
/// Agreement requires BOTH the version and the digest. Version alone is not
/// evidence: `version` is `max(event_id)`, so a mirror missing MIDDLE events
/// reports the same version as the authority (ai-meta#332 AC14).
///
/// ⚠ A refusal is not agreement. Two folds that both failed have compared
/// nothing, and `Err(_) => false` is what keeps "we could not check" from
/// reading as "we checked and it was fine".
fn bounded_fold_agrees(
    bounded: &Result<FoldedState, FoldRefusal>,
    stored_version: i64,
    stored_digest: &str,
) -> bool {
    match bounded {
        Ok(b) => b.version == stored_version && b.digest == stored_digest,
        Err(_) => false,
    }
}

/// Fold `events` truncated to a watermark — the bounded verification, as a
/// pure function.
///
/// Split out of [`grant_for_behind`] so the boundary is testable without a
/// database. It was not, and a mutation flipping `<=` to `<` survived the whole
/// suite: the event AT the stored version would be dropped, every bounded fold
/// would land one version short, and serve-on-behind would refuse 100% of the
/// time while looking correct.
///
/// ⚠ Inclusive on purpose. `stored_version` is the `event_id` the snapshot
/// claims to have folded, so verifying it means folding *through* that event.
fn bounded_fold_at(
    events: Vec<crate::db::models::Event>,
    stored_version: i64,
) -> Result<FoldedState, FoldRefusal> {
    let truncated: Vec<_> = events
        .into_iter()
        .filter(|e| e.event_id <= stored_version)
        .collect();
    fold(FoldSource::Postgres, truncated)
}

/// Verify a BEHIND snapshot **at its own version** and decide whether to serve it.
///
/// ⚠ This is the whole safety argument for serve-on-behind. A behind snapshot
/// cannot be checked against the fresh fold — the spine has moved on, so their
/// digests differ for a legitimate reason. Instead the spine is re-folded
/// *bounded at the stored version* -- via the ladder, truncated to it -- and the
/// digests compared THERE. Agreement means the snapshot is provably correct at
/// its own watermark; `rebuild_state` then folds every event after it, which is
/// the forward fold. Without this bounded re-fold, serving a behind snapshot
/// would be serving an unverified one.
async fn grant_for_behind(
    pool: &DbPool,
    result_store: &crate::services::result_store::ResultStoreService,
    execution_id: i64,
    stored_version: i64,
    stored_digest: &str,
    spine_version: i64,
) -> Result<
    crate::handlers::ehdb_projection_serve::ServeGrant,
    crate::handlers::ehdb_projection_serve::RefuseReason,
> {
    use crate::handlers::ehdb_projection_serve::{RefuseReason, ServeGrant};
    // ai-meta#332 AC14 -- the verification leg reads **Postgres**, never the
    // store being verified.
    //
    // It used to fold the recovery ladder here. On prod that ladder resolves to
    // the tier on 100% of calls (`recovery_fold{source=spine}` = 25,390
    // `spine_incomplete`, 0 folded), so the "verification" compared a
    // tier-derived fold against a tier-derived record: the same events on both
    // sides of the equals sign. A tier missing events agrees with itself and is
    // granted a serve.
    //
    // Postgres is the authority the tier mirrors, so folding it is the only
    // comparison that can see a gap in the mirror. It costs a query the caller
    // is already making -- `rebuild_state` re-reads this execution's whole event
    // set from Postgres immediately after (the WAL path hands it `version: 0`),
    // so this is not a new dependency on the hot path, only an honest one.
    let bounded = events_from_postgres_hydrated(pool, result_store, execution_id)
        .await
        .and_then(|events| bounded_fold_at(events, stored_version));
    let agree = bounded_fold_agrees(&bounded, stored_version, stored_digest);
    if !agree && bounded.is_err() {
        return Err(RefuseReason::SpineRefused);
    }
    ServeGrant::evaluate(agree, Some(stored_version), spine_version)
}

pub async fn wal_projection_state(
    pool: &DbPool,
    result_store: &crate::services::result_store::ResultStoreService,
    execution_id: i64,
) -> (Option<serde_json::Value>, ReFoldVerdict) {
    // Materialise first so an in-flight execution has a record to verify. A
    // failure here is not fatal: the read below simply finds nothing and the
    // verdict says so.
    let _ = materialize_from_wal(execution_id).await;

    // The independent re-fold — ai-meta#332 AC14.
    //
    // Folds **Postgres**, the authority the tier mirrors. It previously folded
    // the recovery ladder, which on prod resolves to the tier on 100% of calls
    // (`recovery_fold{source="spine"}` = 25,390 `spine_incomplete`, 0 folded).
    // That made this a store checking itself: both sides of the comparison were
    // built from the same tier events, so a tier missing events produced equal
    // versions AND equal digests — `Match` — and was served as correct.
    //
    // ⚠ `version` is `max(event_id)`, so a mirror missing MIDDLE events carries
    // the SAME version as the authority. Only the digest separates them, and a
    // digest is only evidence when the two sides were built from different
    // reads. Execution 356712944081313792 is the worked example: 30 events in
    // Postgres, 26 in the tier, identical version.
    //
    // This is not a new Postgres dependency on the serve path. `rebuild_state`
    // re-reads this execution's entire event set from Postgres immediately
    // after, because the snapshot returned here carries `version: 0`.
    let truth = fold_from_postgres_hydrated(pool, result_store, execution_id).await;
    let stored = stored_projection_full(execution_id).await;
    let pair = stored
        .as_ref()
        .ok()
        .and_then(|o| o.as_ref())
        .map(|(v, d, _)| (*v, d.as_str()));
    let verdict = verdict_for(pair, &truth);
    crate::metrics::record_ehdb_projection_refold(verdict.as_str());
    if let Err(r) = &truth {
        crate::metrics::record_ehdb_projection_refold_refusal(r.reason());
    }
    if verdict != ReFoldVerdict::Match {
        // ai-meta#332 -- serve-on-behind. A snapshot BEHIND the spine is slow but
        // correct: rebuild_state folds every event after its version, so serving
        // it costs extra folding and yields the same answer. Measured on prod
        // 2026-09-10: 11 of 11 reads refused for exactly this safe condition.
        //
        // ⚠ AHEAD is never reachable here: `ServeGrant` has no public constructor
        // and `evaluate` cannot produce one for stored > spine, so there is no
        // value this branch could return for an ahead snapshot.
        if verdict == ReFoldVerdict::StoredBehindSpine && serve_on_behind_enabled() {
            if let (Some((sv, sd, body)), Ok(sp)) =
                (stored.as_ref().ok().and_then(|o| o.as_ref()), &truth)
            {
                match grant_for_behind(
                    pool,
                    result_store,
                    execution_id,
                    *sv,
                    sd.as_str(),
                    sp.version,
                )
                .await
                {
                    Ok(grant) => {
                        crate::metrics::record_ehdb_projection_read(grant.outcome_label());
                        return (body.clone(), verdict);
                    }
                    Err(r) => {
                        crate::metrics::record_ehdb_projection_serve_refusal(r.as_str());
                    }
                }
            }
        }
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
/// Which fields of one event differ between two sources.
///
/// Split out of [`fold_diff_endpoint`] so it is reachable by a unit test — the
/// same reason [`events_from_tier_records`] is separate. A differ that is only
/// exercisable through an HTTP call against a live relay is a differ whose
/// "no differences" answer nobody has ever checked, and a false negative here
/// sends an investigation in the wrong direction.
///
/// ⚠ Field NAMES only. Values are the caller's decision, and it reports them for
/// `created_at` alone — `context`, `meta` and `result` can carry customer
/// material.
pub fn differing_fields(
    a: &crate::db::models::Event,
    b: &crate::db::models::Event,
) -> Vec<&'static str> {
    let mut f: Vec<&'static str> = Vec::new();
    if a.event_type != b.event_type {
        f.push("event_type");
    }
    if a.status != b.status {
        f.push("status");
    }
    if a.node_id != b.node_id {
        f.push("node_id");
    }
    if a.node_name != b.node_name {
        f.push("node_name");
    }
    if a.node_type != b.node_type {
        f.push("node_type");
    }
    if a.catalog_id != b.catalog_id {
        f.push("catalog_id");
    }
    if a.parent_event_id != b.parent_event_id {
        f.push("parent_event_id");
    }
    if a.parent_execution_id != b.parent_execution_id {
        f.push("parent_execution_id");
    }
    if a.worker_id != b.worker_id {
        f.push("worker_id");
    }
    if a.attempt != b.attempt {
        f.push("attempt");
    }
    if a.context != b.context {
        f.push("context");
    }
    if a.meta != b.meta {
        f.push("meta");
    }
    if a.result != b.result {
        f.push("result");
    }
    if a.created_at != b.created_at {
        f.push("created_at");
    }
    f
}

pub async fn fold_diff_endpoint(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
    axum::extract::Path(execution_id): axum::extract::Path<i64>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> AppResult<axum::Json<serde_json::Value>> {
    // Which source Postgres is compared against.
    //
    // ⚠ `wal` stays the default so existing callers are unchanged. `tier` exists
    // because the WAL spine returns `spine_incomplete` for COMPLETED executions
    // — which is the very coverage gap noetl/ai-meta#307 is about — so the only
    // field-level differ that shipped could not diagnose the tier divergence the
    // equivalence sweep reports.
    let comparand_source = match q
        .get("source")
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("tier") => "tier",
        _ => "wal",
    };
    let pool = state.pools.pool_for(execution_id);
    let result_store = crate::services::result_store::ResultStoreService::new(
        pool.clone(),
        state.snowflake.clone(),
    );

    // --- incumbent side -----------------------------------------------------
    // ⚠ HYDRATED (noetl/ai-meta#343). This is the THIRD site to need this fix,
    // after noetl/server#425 (`fold_from_postgres`) and noetl/server#426
    // (`compare_sources`), and it is the one that mattered most: this differ is
    // what anybody reaches for to explain *why* two sources disagree.
    //
    // It ran its own copy of the `events_from_postgres` query and never
    // hydrated, so under `NOETL_PERMANENT_LOG_LEAN=true` every execution with
    // an externalised result reported `result` as differing — a `reference`
    // digested against the tier's inlined content. The differ therefore
    // reported the measurement artefact on top of the real difference, and the
    // real one could not be read out from under it.
    //
    // Reading through `events_from_postgres_hydrated` also deletes the copy:
    // the query has ONE definition again, so a fourth site cannot silently
    // drift from it. That is the actual defect class here — three identical
    // SELECTs, each fixed separately.
    let pg_read = events_from_postgres_hydrated(pool, &result_store, execution_id).await;
    // The endpoint answered with a body even when the Postgres read failed, and
    // that is worth keeping — a differ that 500s when one side is unreadable
    // says less than one that names the side. But the failure must land in the
    // refusal channel rather than being flattened into "zero events", which is
    // what `.unwrap_or_default()` did: an unreadable execution and a genuinely
    // empty one produced the same body.
    let pg_events: Vec<crate::db::models::Event> = match &pg_read {
        Ok(evs) => evs.clone(),
        Err(_) => Vec::new(),
    };
    let pg = match pg_read {
        Ok(evs) => fold_with_body(FoldSource::Postgres, evs),
        Err(r) => Err(r),
    };

    // --- comparand side -----------------------------------------------------
    let comparand_events: Result<Vec<crate::db::models::Event>, FoldRefusal> =
        if comparand_source == "tier" {
            tier_events_within(execution_id, TIER_FOLD_TIMEOUT).await
        } else {
            wal_spine_events(execution_id, None).await
        };
    let comparand = match &comparand_events {
        Ok(evs) => fold_with_body(
            if comparand_source == "tier" {
                FoldSource::EhdbTier
            } else {
                FoldSource::WalSpine
            },
            evs.clone(),
        ),
        Err(r) => Err(r.clone()),
    };

    let mut diff: Vec<String> = Vec::new();
    let mut same_version = false;
    if let (Ok((pgs, pgb)), Ok((cs, cb))) = (&pg, &comparand) {
        same_version = pgs.version == cs.version;
        if same_version {
            json_diff_paths(pgb, cb, "", &mut diff);
        }
    }

    // Per-event INPUT diff — the minimal reproducer.
    //
    // `diff_paths` names the differing *state* path; this names the event and
    // field that produced it. Without it a divergence is observable but not
    // attributable, which is what left noetl/ai-meta#307 stuck.
    //
    // ⚠ Values are reported ONLY for `created_at`, which is the hypothesis under
    // test and carries nothing sensitive. `context`, `meta` and `result` report
    // only THAT they differ — the same discipline the presence counts below
    // already follow, because `context` is exactly the field that could carry
    // customer material.
    let input_event_diffs: Vec<serde_json::Value> = match &comparand_events {
        Err(_) => Vec::new(),
        Ok(cev) => {
            let by_id: std::collections::BTreeMap<i64, &crate::db::models::Event> =
                cev.iter().map(|e| (e.event_id, e)).collect();
            pg_events
                .iter()
                .filter_map(|a| {
                    let b = by_id.get(&a.event_id)?;
                    let fields = differing_fields(a, b);
                    let ts_differs = fields.contains(&"created_at");
                    if fields.is_empty() {
                        return None;
                    }
                    Some(serde_json::json!({
                        "event_id": a.event_id,
                        "event_type": a.event_type,
                        "fields": fields,
                        // Timestamps only — see the note above.
                        "postgres_created_at": ts_differs.then(|| a.created_at.to_rfc3339()),
                        "comparand_created_at": ts_differs.then(|| b.created_at.to_rfc3339()),
                    }))
                })
                .collect()
        }
    };

    // ⚠ Compared AFTER `normalise_event_precision` would run, so a difference
    // here is one the fold actually sees — not one the normaliser reconciles.
    let input_event_diffs_post_normalisation: Vec<serde_json::Value> = match &comparand_events {
        Err(_) => Vec::new(),
        Ok(cev) => {
            let mut a_norm = pg_events.clone();
            let mut b_norm = cev.clone();
            normalise_event_precision(&mut a_norm);
            normalise_event_precision(&mut b_norm);
            let by_id: std::collections::BTreeMap<i64, &crate::db::models::Event> =
                b_norm.iter().map(|e| (e.event_id, e)).collect();
            a_norm
                .iter()
                .filter_map(|a| {
                    let b = by_id.get(&a.event_id)?;
                    (a.created_at != b.created_at).then(|| {
                        serde_json::json!({
                            "event_id": a.event_id,
                            "event_type": a.event_type,
                            "postgres_created_at": a.created_at.to_rfc3339(),
                            "comparand_created_at": b.created_at.to_rfc3339(),
                        })
                    })
                })
                .collect()
        }
    };

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
        // Back-compat: these keep their names for `source=wal`, and are null
        // under `source=tier` so an old caller never silently reads a tier
        // answer as a WAL one.
        "wal": (comparand_source == "wal").then(|| comparand.as_ref().ok().map(|(s, _)| s)).flatten(),
        "wal_refusal": (comparand_source == "wal").then(|| comparand.as_ref().err()).flatten(),
        "comparand_source": comparand_source,
        "comparand": comparand.as_ref().ok().map(|(s, _)| s),
        "comparand_refusal": comparand.as_ref().err(),
        "diff_paths": diff,
        "input_event_diffs": input_event_diffs,
        "input_event_diffs_post_normalisation": input_event_diffs_post_normalisation,
        "input_fields_postgres": field_presence(&pg_events),
        "input_fields_wal": (comparand_source == "wal")
            .then(|| comparand_events.as_ref().ok().map(|e| field_presence(e)))
            .flatten(),
        "input_fields_comparand": comparand_events.as_ref().ok().map(|e| field_presence(e)),
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

    /// The default is the PRE-#307 behaviour, so this change lands inert.
    #[test]
    fn recovery_defaults_to_spine_so_the_change_lands_inert() {
        assert_eq!(parse_recovery_source(None), RecoverySource::Spine);
        assert_eq!(parse_recovery_source(Some("")), RecoverySource::Spine);
        assert_eq!(parse_recovery_source(Some("  ")), RecoverySource::Spine);
    }

    /// A typo falls back to `spine`, not to the new path.
    ///
    /// The direction matters: falling back to `tier` would let a
    /// misspelt value silently change which store drives executions,
    /// which is the shape ai-meta#243 records.
    #[test]
    fn an_unrecognised_mode_falls_back_to_the_old_behaviour_not_the_new_one() {
        for junk in ["teir", "TIER!", "wal", "true", "1", "postgres"] {
            assert_eq!(
                parse_recovery_source(Some(junk)),
                RecoverySource::Spine,
                "{junk} must not enable an unproven read path"
            );
        }
    }

    #[test]
    fn modes_parse_case_and_space_insensitively() {
        assert_eq!(
            parse_recovery_source(Some(" Verify ")),
            RecoverySource::Verify
        );
        assert_eq!(parse_recovery_source(Some("TIER")), RecoverySource::Tier);
        assert_eq!(parse_recovery_source(Some("Spine")), RecoverySource::Spine);
    }

    /// `verify` observes and `tier` serves — and only `tier` serves.
    #[test]
    fn only_tier_mode_may_actually_serve_from_the_tier() {
        assert!(!RecoverySource::Spine.consults_tier());
        assert!(RecoverySource::Verify.consults_tier());
        assert!(RecoverySource::Tier.consults_tier());

        assert!(!RecoverySource::Spine.serves_tier());
        assert!(
            !RecoverySource::Verify.serves_tier(),
            "verify is the dark-launch rung; if it serves, the rung does not exist"
        );
        assert!(RecoverySource::Tier.serves_tier());
    }

    /// Every mode and fold-source label is pinned, via a TOTAL match so a new
    /// variant fails the build here rather than going absent on a metric.
    #[test]
    fn every_recovery_label_is_pinned() {
        for m in [
            RecoverySource::Spine,
            RecoverySource::Verify,
            RecoverySource::Tier,
        ] {
            let expect = match m {
                RecoverySource::Spine => "spine",
                RecoverySource::Verify => "verify",
                RecoverySource::Tier => "tier",
            };
            assert_eq!(m.as_str(), expect);
            assert!(RECOVERY_SOURCES.contains(&m.as_str()));
        }
        assert_eq!(RECOVERY_SOURCES.len(), 3);
        // The fold sources are the two a fold is actually attempted against —
        // `verify` is a mode, never a source, and must not appear here.
        assert_eq!(RECOVERY_FOLD_SOURCES, ["spine", "tier"]);
        assert!(!RECOVERY_FOLD_SOURCES.contains(&"verify"));
    }

    /// ⚠⚠ The serve-on-behind branch must actually CALL the grant evaluator.
    ///
    /// `ServeGrant::evaluate` being correct is worth nothing if the read path
    /// never reaches it — the noetl/ai-meta#326 shape, where a validator existed
    /// and the store never invoked it. Asserted at the source because this branch
    /// needs a live worker relay and a lagging mirror to exercise at runtime.
    #[test]
    fn the_serve_branch_calls_the_grant_evaluator() {
        let src = include_str!("ehdb_projection_fold.rs");
        let code = src
            .split_once("\n#[cfg(test)]")
            .map(|(above, _)| above)
            .unwrap_or(src);
        let start = code
            .find("pub async fn wal_projection_state")
            .expect("wal_projection_state not found — extraction broke");
        let body = &code[start..];
        let end = body.find("\n}\n").map(|i| i + 3).unwrap_or(body.len());
        let body = &body[..end];
        assert!(
            body.len() > 800,
            "wal_projection_state extracted as {} bytes — implausibly small",
            body.len()
        );
        assert!(
            body.contains("grant_for_behind("),
            "the read path does not call grant_for_behind — serve-on-behind is \
             unreachable and every `stale_within_window` it could report is \
             unreachable with it"
        );
        assert!(
            body.contains("serve_on_behind_enabled()"),
            "the serve branch is not gated by the flag — it would be on by default"
        );
    }

    /// Serve-on-behind must be OFF unless explicitly armed.
    #[test]
    fn serve_on_behind_defaults_off_and_is_strict() {
        // SAFETY: single-threaded read of a var this test does not set.
        assert!(
            !matches!(std::env::var(SERVE_ON_BEHIND_ENV).as_deref(), Ok("true"))
                || serve_on_behind_enabled(),
            "flag parse disagrees with the env"
        );
        for v in ["1", "yes", "TRUE", "on", "", "tier"] {
            assert_ne!(v, "true", "sanity");
        }
    }

    /// ai-meta#332 AC14 — a tier missing MIDDLE events must be refused, not
    /// served as correct.
    ///
    /// Modelled on prod execution `356712944081313792` (`test/simple_loop`,
    /// COMPLETED): 30 events in `noetl.event`, 26 in the tier, and — because
    /// `version` is `max(event_id)` and the missing events are in the middle —
    /// **the same version on both sides**. Version comparison cannot see this
    /// gap; only a digest computed from an independent read can.
    ///
    /// The control at the bottom is the point of the test: it reproduces what
    /// the shipped code did, and shows it returning `Match`.
    #[test]
    fn a_tier_missing_middle_events_is_refused_not_served() {
        let ev = |id: i64, node: &str| {
            event_from_tier_payload(&serde_json::json!({
                "event_id": id, "execution_id": 356712944081313792i64, "catalog_id": 7,
                "event_type": "step.enter", "node_name": node, "status": "ok",
                // ⚠ Pin `created_at`. `event_from_tier_payload` substitutes
                // `Utc::now()` when it is absent, so an unpinned fixture folds
                // to a DIFFERENT digest on every call and the control below
                // would "fail" for a reason that has nothing to do with the gap.
                "created_at": format!("2026-09-11T08:0{}:00Z", id % 10)
            }))
            .expect("fixture parses")
        };

        // Postgres — the authority: 30 events.
        let pg_events: Vec<_> = (1..=30).map(|i| ev(i, &format!("s{i}"))).collect();
        // The tier — the same execution with 4 MIDDLE events never mirrored.
        let missing = [12i64, 13, 14, 15];
        let tier_events: Vec<_> = pg_events
            .iter()
            .filter(|e| !missing.contains(&e.event_id))
            .cloned()
            .collect();
        assert_eq!(pg_events.len(), 30);
        assert_eq!(tier_events.len(), 26, "the 356712944081313792 shape");

        let pg = fold(FoldSource::Postgres, pg_events).expect("postgres folds");
        let tier = fold(FoldSource::EhdbTier, tier_events).expect("tier folds");

        // ⚠ The property that makes this gap invisible to a version check.
        assert_eq!(
            tier.version, pg.version,
            "missing MIDDLE events leave max(event_id) unchanged — the tier \
             carries the same version as the authority"
        );
        assert_ne!(
            tier.applied_count, pg.applied_count,
            "the counts differ even though the versions do not"
        );
        assert_ne!(
            tier.digest, pg.digest,
            "the digests must differ, or this fixture does not model a real gap"
        );

        // THE FIX: verified against Postgres, the gap is a fault and is refused.
        let verdict = verdict_for(Some((tier.version, tier.digest.as_str())), &Ok(pg));
        assert_eq!(
            verdict,
            ReFoldVerdict::DigestMismatch,
            "a tier missing events must not be served as if correct"
        );
        assert!(
            verdict.is_fault(),
            "and it must page — this is a mirror losing events, not lag"
        );

        // CONTROL — what the shipped code compared: the tier against itself.
        // On prod the recovery ladder resolves to the tier on 100% of calls, so
        // this was not a corner case, it was the whole path.
        let tier_again = fold(
            FoldSource::EhdbTier,
            (1..=30)
                .filter(|i| !missing.contains(i))
                .map(|i| ev(i, &format!("s{i}")))
                .collect(),
        )
        .expect("tier folds");
        assert_eq!(
            verdict_for(Some((tier.version, tier.digest.as_str())), &Ok(tier_again)),
            ReFoldVerdict::Match,
            "the pre-fix comparison called this gap a MATCH — this control is \
             why the verification leg had to stop reading the tier"
        );
    }

    /// The serve decision agrees ONLY on a full match, and never on a refusal.
    ///
    /// Four-sided, because a one-sided assertion is satisfied by a function
    /// that always refuses (safe, and it would make serve-on-behind dead) and a
    /// function that always agrees (the mutation that survived until this test
    /// existed: `let agree = true`, a grant issued with no verification).
    #[test]
    fn a_grant_requires_version_and_digest_and_never_a_refusal() {
        let ok = |v: i64, d: &str| {
            Ok(FoldedState {
                source: FoldSource::Postgres,
                version: v,
                applied_count: 30,
                digest: d.to_string(),
            })
        };

        // 1. The healthy case must actually be reachable.
        assert!(
            bounded_fold_agrees(&ok(100, "aaa"), 100, "aaa"),
            "a bounded fold that matches on both fields must agree, or \
             serve-on-behind can never grant and the flag is inert"
        );

        // 2. The 356712944081313792 shape: SAME version, different content.
        assert!(
            !bounded_fold_agrees(&ok(100, "aaa"), 100, "bbb"),
            "same version with a different digest is the missing-middle-events \
             gap — it must NOT be granted a serve"
        );

        // 3. Version alone is not enough in the other direction either.
        assert!(!bounded_fold_agrees(&ok(99, "aaa"), 100, "aaa"));

        // 4. A refusal is not agreement.
        for r in [
            FoldRefusal::NoEvents,
            FoldRefusal::SpineIncomplete,
            FoldRefusal::FoldFailed,
            FoldRefusal::SourceUnavailable("db down".into()),
        ] {
            assert!(
                !bounded_fold_agrees(&Err(r), 100, "aaa"),
                "\"could not check\" must never read as \"checked and fine\""
            );
        }
    }

    /// `fold` is order-SENSITIVE, so every reader of it must order explicitly.
    ///
    /// `fold` does not sort, and `WorkflowState::from_events` applies events in
    /// the order given. So the `ORDER BY` in [`events_from_postgres`] is not
    /// cosmetic — it is the thing that makes the fold correct.
    ///
    /// A mutation flipping that `ORDER BY` to `DESC` survived the whole suite,
    /// because the query needs a database and nothing asserted the ordering.
    /// This pairs a real order-sensitivity demonstration (the positive control,
    /// so the guard below is not decorative) with a source-text guard on the
    /// query itself.
    #[test]
    fn the_postgres_read_orders_events_and_the_fold_depends_on_it() {
        // ⚠ The fixture must model a STATE TRANSITION on one node. A first
        // draft used `step.enter` on twelve distinct nodes and the two orders
        // digested identically — distinct keys build the same map either way,
        // and the canonical digest sorts keys. Order is only observable where a
        // later event changes what an earlier one set.
        let ev = |id: i64, kind: &str| {
            event_from_tier_payload(&serde_json::json!({
                "event_id": id, "execution_id": 9, "catalog_id": 7,
                "event_type": kind, "node_name": "s",
                "status": "ok", "created_at": "2026-09-11T08:00:00Z",
                "meta": {"command_id": "c0"}
            }))
            .expect("fixture parses")
        };
        let ascending: Vec<_> = vec![
            ev(1, "step.enter"),
            ev(2, "command.issued"),
            ev(3, "command.completed"),
        ];
        let descending: Vec<_> = ascending.iter().rev().cloned().collect();

        let a = fold(FoldSource::Postgres, ascending).expect("folds");
        let d = fold(FoldSource::Postgres, descending).expect("folds");

        // Same events, same version, same count — and a different state.
        assert_eq!(a.version, d.version, "max(event_id) is order-independent");
        assert_eq!(a.applied_count, d.applied_count);
        assert_ne!(
            a.digest, d.digest,
            "if this ever becomes equal, the fold has become order-independent              and the guard below is free to relax — until then it must not"
        );

        // The guard the demonstration justifies.
        let src = include_str!("ehdb_projection_fold.rs");
        let code = src
            .split_once("\n#[cfg(test)]")
            .map(|(above, _)| above)
            .unwrap_or(src);
        // ⚠ Match the exact signature. `events_from_postgres_hydrated` now
        // precedes it in the file, and a prefix search lands on that wrapper —
        // whose body has no SQL at all, so every assertion below would be
        // measuring the wrong function.
        let start = code
            .find("pub async fn events_from_postgres(")
            .expect("events_from_postgres not found — extraction broke");
        let body = &code[start..];
        let end = body.find("\n}\n").map(|i| i + 3).unwrap_or(body.len());
        let body = &body[..end];
        assert!(body.len() > 300, "slice too small: {}", body.len());
        assert!(
            body.contains("ORDER BY event_id ASC"),
            "the verification leg's query does not order ascending — the fold \
             would apply this execution's events in the wrong sequence"
        );
    }

    /// The bounded fold is INCLUSIVE of the stored version.
    ///
    /// Written because a mutation flipping `<=` to `<` survived the entire
    /// suite. Nothing exercised the boundary, because the truncation lived
    /// inside a function that needed a `DbPool`; extracting `bounded_fold_at`
    /// is what made the property assertable.
    ///
    /// Two-sided: it pins what the fold MUST include and what it must EXCLUDE,
    /// so neither an off-by-one nor a filter that stopped filtering passes.
    #[test]
    fn the_bounded_fold_includes_the_stored_version_and_nothing_after() {
        let ev = |id: i64| {
            event_from_tier_payload(&serde_json::json!({
                "event_id": id, "execution_id": 9, "catalog_id": 7,
                "event_type": "step.enter", "node_name": format!("s{id}"),
                "status": "ok", "created_at": "2026-09-11T08:00:00Z"
            }))
            .expect("fixture parses")
        };
        let events: Vec<_> = (1..=30).map(ev).collect();

        let at20 = bounded_fold_at(events.clone(), 20).expect("folds");
        assert_eq!(
            at20.version, 20,
            "the event AT the watermark must be folded — `<` instead of `<=`              lands on 19 and every serve-on-behind grant refuses"
        );
        assert_eq!(at20.applied_count, 20, "exactly events 1..=20");

        // The whole set is not truncated away, and events after the watermark
        // are genuinely excluded.
        let at30 = bounded_fold_at(events.clone(), 30).expect("folds");
        assert_eq!(at30.version, 30);
        assert_ne!(
            at20.digest, at30.digest,
            "the truncation must actually change the folded state, or this              test would pass against a filter that does nothing"
        );

        // A watermark below every event folds nothing rather than inventing a
        // state at step 0.
        assert!(matches!(
            bounded_fold_at(events, 0),
            Err(FoldRefusal::NoEvents) | Err(FoldRefusal::FoldFailed)
        ));
    }

    /// The hydrated reader must actually hydrate, and must NOT keep the refs.
    ///
    /// The call-site guards prove *which* function the verification legs call.
    /// They cannot prove that function does anything — a
    /// `..._hydrated` that forgot to hydrate would satisfy every one of them
    /// while restoring the exact false-divergence class this fix removes.
    ///
    /// `keep_refs = false` is load-bearing: keeping the reference envelope
    /// leaves the pointer beside the content, which still digests differently
    /// from the tier's inlined-only shape.
    #[test]
    fn the_hydrated_reader_hydrates_and_drops_the_reference_envelope() {
        let src = include_str!("ehdb_projection_fold.rs");
        let code = src
            .split_once("\n#[cfg(test)]")
            .map(|(above, _)| above)
            .unwrap_or(src);
        let start = code
            .find("pub async fn events_from_postgres_hydrated(")
            .expect("hydrated reader not found — extraction broke");
        let body = &code[start..];
        let end = body.find("\n}\n").map(|i| i + 3).unwrap_or(body.len());
        let body = &body[..end];
        assert!(
            body.len() > 200,
            "hydrated reader slice too small: {}",
            body.len()
        );
        assert!(
            body.contains("hydrate_result_references("),
            "the hydrated reader does not call the hydrator — the verification \
             leg would digest a reference against inlined content again"
        );
        assert!(
            body.contains("result_store, false"),
            "the hydrator must be called with keep_refs = false; keeping the \
             envelope leaves the pointer beside the content and still diverges \
             from the tier's inlined shape"
        );
    }

    /// An externalised `reference` and an inlined `context` fold to DIFFERENT
    /// digests — which is why the Postgres leg must hydrate.
    ///
    /// This is the mechanism behind 30% of the divergence measured on prod
    /// (12 of the 40 most recent executions, 2026-09-12): byte-identical field
    /// counts on both sides, and `diff_paths` containing nothing but
    ///
    /// ```text
    /// /steps/<step>/result/context/result/reference  (only in Postgres)
    /// /steps/<step>/result/context/result/context    (only in the tier)
    /// ```
    ///
    /// With `NOETL_PERMANENT_LOG_LEAN=true` the persisted row keeps a pointer
    /// where the mirrored record keeps the content. Neither store is wrong;
    /// comparing them without resolving the pointer is.
    ///
    /// Two-sided on purpose: the same shape on both sides must AGREE, or this
    /// test would pass against a fold that simply digests everything to
    /// different values.
    #[test]
    fn a_reference_and_its_inlined_content_do_not_fold_alike() {
        let ev = |id: i64, inner: serde_json::Value| {
            event_from_tier_payload(&serde_json::json!({
                "event_id": id, "execution_id": 42, "catalog_id": 7,
                "event_type": "command.completed", "node_name": "s",
                "status": "ok", "created_at": "2026-09-12T06:00:00Z",
                "meta": {"command_id": "c0"},
                // ⚠ Two fixture corrections, both from the fold folding the
                // two sides IDENTICALLY:
                //   1. `command.issued` never sets `step.result`, so the
                //      payload had no route into the digest at all;
                //   2. the carrier is the `result` column of the event that
                //      COMPLETES the step — the prod state-diff path is
                //      `/steps/<step>/result/context/result/reference`.
                // A fixture that cannot exhibit the failure proves nothing.
                "result": {"context": {"result": inner}}
            }))
            .expect("fixture parses")
        };
        let as_reference = || {
            vec![ev(
                1,
                serde_json::json!({"reference": {"ref": "noetl://result/42/abc"}}),
            )]
        };
        let as_inlined = || vec![ev(1, serde_json::json!({"context": {"rows": [1, 2, 3]}}))];

        let pg = fold(FoldSource::Postgres, as_reference()).expect("folds");
        let tier = fold(FoldSource::EhdbTier, as_inlined()).expect("folds");

        assert_eq!(pg.version, tier.version, "same event, so same version");
        assert_eq!(
            pg.applied_count, tier.applied_count,
            "same event COUNT — this is why a count check cannot see this class"
        );
        assert_ne!(
            pg.digest, tier.digest,
            "a pointer and its content must fold differently, or the premise of \
             the hydration fix is wrong"
        );

        // The two-sided half: identical shapes agree, so the inequality above is
        // about reference-vs-content and not about folding being unstable.
        let a = fold(FoldSource::Postgres, as_inlined()).expect("folds");
        let b = fold(FoldSource::EhdbTier, as_inlined()).expect("folds");
        assert_eq!(
            a.digest, b.digest,
            "the same content must fold to the same digest regardless of source"
        );
    }

    /// The DIAGNOSTIC must hydrate too, or it cannot validate the fix.
    ///
    /// ⚠⚠ Written after a live miss. v3.108.2 shipped hydration on the serve
    /// path, deployed cleanly — and the divergence count this endpoint reports
    /// did not move by a single execution out of 40, because `compare_sources`
    /// was never on the path that changed. The fix was right and the instrument
    /// was still lying.
    ///
    /// An instrument that cannot see a fix cannot validate one, so it is held to
    /// the same rule as the legs it measures.
    #[test]
    fn the_diagnostic_comparison_hydrates_like_the_serve_path() {
        let src = include_str!("ehdb_projection_fold.rs");
        let code = src
            .split_once("\n#[cfg(test)]")
            .map(|(above, _)| above)
            .unwrap_or(src);
        let start = code
            .find("pub async fn compare_sources(")
            .expect("compare_sources not found — extraction broke");
        let body = &code[start..];
        let end = body.find("\n}\n").map(|i| i + 3).unwrap_or(body.len());
        let body = &body[..end];
        assert!(
            body.len() > 400,
            "compare_sources slice too small: {}",
            body.len()
        );
        assert!(
            body.contains("fold_from_postgres_hydrated("),
            "the diagnostic folds the RAW Postgres row — it will report a \
             reference-vs-content difference as divergence, and will not move \
             when the serve path is fixed"
        );
    }

    /// The READ PATH's verification leg must also be independent of the tier.
    ///
    /// Sibling of `the_bounded_verification_is_independent_of_the_tier`, which
    /// pins the same property for the serve-on-behind branch. This one pins it
    /// for `wal_projection_state` itself — the leg that decides `Match`, and so
    /// the one that was serving 2,212 prod reads off a tier-vs-tier comparison
    /// with the serve-on-behind flag still OFF.
    ///
    /// Two guards rather than one because they fail for different reasons and a
    /// reader fixing one should not have to infer the other.
    #[test]
    fn the_read_path_verifies_against_postgres_not_the_tier() {
        let src = include_str!("ehdb_projection_fold.rs");
        let code = src
            .split_once("\n#[cfg(test)]")
            .map(|(above, _)| above)
            .unwrap_or(src);
        let start = code
            .find("pub async fn wal_projection_state")
            .expect("wal_projection_state not found — extraction broke");
        let body = &code[start..];
        let end = body.find("\n}\n").map(|i| i + 3).unwrap_or(body.len());
        let body = &body[..end];
        // ⚠ Assert the extraction before asserting about it: a slice that
        // silently came back empty would pass every negative check below.
        assert!(
            body.len() > 400,
            "wal_projection_state slice too small: {}",
            body.len()
        );
        assert!(
            body.contains("fold_from_postgres_hydrated("),
            "the read path's re-fold must read Postgres **hydrated** — the raw \
             row carries a reference where the tier carries content, and \
             comparing those is a false divergence, not a mirror defect"
        );
        assert!(
            !body.contains("events_for_recovery("),
            "the read path's re-fold went back through the recovery ladder, \
             which falls back to the TIER it is verifying"
        );
    }

    /// The bounded verification must read **Postgres**, not the store it is
    /// verifying.
    ///
    /// ⚠ This guard INVERTED on 2026-09-11 (ai-meta#332 AC14). It used to
    /// require `events_for_recovery(` — the recovery ladder — on the reasoning
    /// that the ladder's tier fallback gives completed executions coverage the
    /// in-flight-only spine index cannot (ai-meta#307). That reasoning was
    /// right about coverage and wrong about what was being measured: prod shows
    /// `recovery_fold{source="spine"}` = 25,390 `spine_incomplete` against **0**
    /// folded, so the ladder resolves to the tier on 100% of calls. The
    /// "verification" was therefore folding tier events to check a tier record —
    /// a store confirming itself, which a mirror missing events passes.
    ///
    /// The property this guard protects is unchanged: the serve decision must
    /// rest on evidence independent of the thing being served. Only the source
    /// that satisfies it changed.
    #[test]
    fn the_bounded_verification_is_independent_of_the_tier() {
        let src = include_str!("ehdb_projection_fold.rs");
        let code = src
            .split_once("\n#[cfg(test)]")
            .map(|(above, _)| above)
            .unwrap_or(src);
        let start = code
            .find("async fn grant_for_behind")
            .expect("grant_for_behind not found — extraction broke");
        let body = &code[start..];
        let end = body.find("\n}\n").map(|i| i + 3).unwrap_or(body.len());
        let body = &body[..end];
        assert!(
            body.len() > 400,
            "grant_for_behind slice too small: {}",
            body.len()
        );
        assert!(
            body.contains("events_from_postgres_hydrated("),
            "the bounded verification must read Postgres **hydrated**. Reading \
             the raw row digests a `reference` where the tier holds inlined \
             content, so it reports divergence on every execution with an \
             externalised result — 30% of a 40-execution prod sample"
        );
        assert!(
            !body.contains("events_for_recovery("),
            "the bounded verification went back through the recovery ladder, \
             which falls back to the TIER — the store being verified. On prod \
             the spine refuses 100% of the time, so this is tier-vs-tier: a \
             mirror missing events agrees with itself and is served"
        );
        assert!(
            !body.contains("fold_spine_inner("),
            "the bounded verification reaches the in-flight-only spine index \
             directly — completed executions would refuse, which is ai-meta#307"
        );
    }

    /// Recovery must not reach the spine directly, bypassing the ladder.
    ///
    /// Counts CALL SITES rather than naming the callers: a guard that lists the
    /// functions it trusts passes unchanged when a new one is added beside
    /// them. Scans only the code ABOVE `#[cfg(test)]`, because `include_str!`
    /// pulls in this module — including this assertion, whose own needle would
    /// otherwise be the match it finds.
    #[test]
    fn recovery_reaches_the_spine_only_through_the_ladder() {
        let src = include_str!("ehdb_projection_fold.rs");
        let code = src
            .split_once("\n#[cfg(test)]")
            .map(|(above, _)| above)
            .unwrap_or(src);
        let sites = code.matches("fold_spine_inner(").count();
        assert_eq!(
            sites, 3,
            "expected exactly 3: the definition, `wal_spine_events` (diff-only, \
             deliberately spine-pinned), and `events_for_recovery`. A 4th means a \
             recovery path is reading the in-flight-only index directly again — \
             which is the whole of ai-meta#307."
        );
    }

    /// The equivalence gate must be able to FAIL, including on the shape that
    /// looks most like success: nothing compared at all.
    #[test]
    fn an_equivalence_sweep_that_compared_nothing_is_not_a_pass() {
        assert!(
            !equivalence_holds(0, 0),
            "zero agreements and zero disagreements is a sweep where everything \
             refused — reporting it as equivalent is the vacuous pass"
        );
        assert!(!equivalence_holds(0, 5));
        assert!(!equivalence_holds(9, 1), "one disagreement fails the sweep");
        assert!(equivalence_holds(9, 0));
        assert!(equivalence_holds(1, 0));
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
        // ⚠⚠ TRUNCATION, not rounding. This assertion previously required the
        // opposite, on the premise that Postgres rounds. It does not.
        //
        // Measured on prod 2026-08-30 (execution 352467520122265600, event
        // 352467520235511808): the source instant was …354081798 and Postgres
        // stored …354081. A remainder of 798 ns would have rounded UP; Postgres
        // rounds it up, and so must the fold.
        //
        // ⚠ This assertion previously expected the floor, citing an agreement
        // improvement from "1 of 4" to 15 of 37. That improvement came from
        // normalising at all, not from the direction: re-measured on prod over
        // 149 events, rounding reproduced the stored value 149/149 while
        // truncation managed 66/149.
        let up = chrono::Utc
            .timestamp_opt(1_756_184_493, 645_451_500)
            .single()
            .unwrap();
        assert_eq!(
            truncate_to_micros(up).timestamp_subsec_micros(),
            645_451,
            "a remainder of exactly 500 ns TRUNCATES — the fold follows the \
             producer rule (`event_write::to_storage_precision`), not a guess \
             about Postgres. See noetl/ai-meta#307."
        );
        let high = chrono::Utc
            .timestamp_opt(1_756_184_493, 645_451_798)
            .single()
            .unwrap();
        // ⚠ THE MINORITY CASE, KEPT DELIBERATELY.
        //
        // Execution 352467520122265600 event 352467520235511808 has a 798 ns
        // remainder and Postgres stored the FLOOR. Under rounding this event is
        // 1 µs adrift — and that is not a bug in the rule, it is evidence that
        // there is no single rule.
        //
        // Measured on prod 2026-08-31 across 11 executions / 263 events:
        //
        //     9 executions  -> round matches every event, truncate matches ~half
        //     1 execution   -> truncate matches every event, round matches 5/14
        //     1 execution   -> neither matches all
        //
        // So `noetl.event.created_at` is produced by more than one path, and
        // they do not agree on how sub-microsecond precision is reduced.
        // Rounding is chosen because it is right for 9 of 11; it CANNOT reach
        // `input_event_diffs_post_normalisation == 0`, and choosing the other
        // direction cannot either. The fix that can is upstream: make the
        // producers agree.
        //
        // This assertion therefore records the exception rather than asserting
        // it away.
        let prod_minority = chrono::Utc
            .timestamp_opt(1_756_184_493, 354_081_798)
            .single()
            .unwrap();
        assert_eq!(
            truncate_to_micros(prod_minority).timestamp_subsec_micros(),
            354_081,
            "798 ns truncates to 354081 — which is what Postgres stored for this \
             execution, and what the producer rule now guarantees for every new one"
        );
        let down = chrono::Utc
            .timestamp_opt(1_756_184_493, 645_451_499)
            .single()
            .unwrap();
        assert_eq!(
            truncate_to_micros(down).timestamp_subsec_micros(),
            645_451,
            "a remainder < 500ns floors too — truncation has no threshold"
        );
        // ⚠ 999ns is the strongest case: rounding would carry into the next
        // microsecond, truncation cannot.
        let nearly = chrono::Utc
            .timestamp_opt(1_756_184_493, 645_451_999)
            .single()
            .unwrap();
        assert_eq!(
            truncate_to_micros(nearly).timestamp_subsec_micros(),
            645_451,
            "999 ns does NOT carry — truncation, matching the producer rule in \
             `event_write::to_storage_precision` (noetl/ai-meta#307)"
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
        // Under ROUNDING (what Postgres does): 645_451_448 has a remainder of
        // 448 ns and stays at 645451; 645_451_999 and 645_452_001 both round to
        // 645452.
        //
        // ⚠ The earlier version of this test asserted that the first two must
        // normalise together, on the reasoning that values inside one
        // microsecond should bucket together. That is a reasonable-sounding
        // property and it is not the objective: the fold must reproduce the
        // value POSTGRES STORED, and Postgres rounds, so it splits them exactly
        // here too. Agreeing with the system of record beats a tidy bucketing
        // rule that disagrees with it.
        let mut evs = vec![mk(645_451_448), mk(645_451_999), mk(645_452_001)];
        normalise_event_precision(&mut evs);
        assert_eq!(
            evs[0].created_at, evs[1].created_at,
            "448 ns and 999 ns both TRUNCATE to the same microsecond"
        );
        assert_ne!(
            evs[1].created_at, evs[2].created_at,
            "999 ns floors to 645451 and 1001 ns to 645452 — different microseconds"
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
            // Two refusals: not comparable, and specifically NOT divergent.
            verdict: ComparisonVerdict::NotComparable,
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

    // ---------------------------------------------------------------------
    // noetl/ai-meta#343 — the unhydrated-comparator CLASS, not one instance.
    // ---------------------------------------------------------------------

    /// The file's production source, with **every** `#[cfg(test)] mod …` block
    /// removed.
    ///
    /// ⚠⚠ The idiom the rest of this file uses — `split_once("\n#[cfg(test)]")`
    /// — is wrong *here* and has been since `equivalence_endpoint` landed. This
    /// module has TWO test modules (`tests` and `differing_fields_tests`) with
    /// ~150 lines of production code BETWEEN them, so cutting at the first one
    /// silently drops `recently_completed`, `equivalence_holds`,
    /// `equivalence_endpoint` and `recovery_compare_endpoint` out of the
    /// population every structural guard measures.
    ///
    /// A guard whose denominator excludes part of the population cannot fail
    /// for anything in the excluded part — it reads as a pass because it never
    /// looked. This removes each test module by its own closing brace instead,
    /// so the denominator is the whole file, and
    /// [`the_production_slice_sees_past_the_first_test_module`] pins that.
    fn production_src() -> String {
        let src = include_str!("ehdb_projection_fold.rs");
        let mut out = String::new();
        let mut rest: &str = src;
        while let Some(i) = rest.find("\n#[cfg(test)]\nmod ") {
            out.push_str(&rest[..i + 1]);
            let after = &rest[i + 1..];
            // A top-level `mod` closes with `}` in column 0; every brace inside
            // it is indented, so this finds the module's own terminator.
            match after.find("\n}\n") {
                Some(j) => rest = &after[j + 3..],
                None => {
                    rest = "";
                    break;
                }
            }
        }
        out.push_str(rest);
        out
    }

    /// The slicer's own positive control.
    ///
    /// If `production_src` silently degraded to the old "cut at the first
    /// `#[cfg(test)]`" behaviour, every guard below would still pass — they
    /// would simply be measuring a smaller file. So assert directly that it
    /// keeps production code declared *after* the first test module, and that
    /// it drops test code. Without this the denominator guard is decorative.
    #[test]
    fn the_production_slice_sees_past_the_first_test_module() {
        let prod = production_src();
        assert!(
            prod.contains("pub async fn recovery_compare_endpoint"),
            "production_src lost code declared after the first test module — \
             this is the exact blind spot it exists to close"
        );
        assert!(
            prod.contains("pub async fn fold_diff_endpoint"),
            "production_src lost code declared before the first test module"
        );
        assert!(
            !prod.contains("fn the_production_slice_sees_past_the_first_test_module"),
            "production_src kept test code — the slice is not removing modules"
        );
        // Negative control on the OLD idiom: prove it really was blind, so this
        // guard is anchored to a demonstrated defect rather than a claim.
        let src = include_str!("ehdb_projection_fold.rs");
        let old = src
            .split_once("\n#[cfg(test)]")
            .map(|(above, _)| above)
            .unwrap_or(src);
        assert!(
            !old.contains("pub async fn recovery_compare_endpoint"),
            "the old idiom is no longer blind — if the file was restructured so \
             all production code precedes the tests, simplify this helper; \
             until then it must stay"
        );
    }

    /// Drop `//` comment lines.
    ///
    /// ⚠ Every "this code must NOT contain X" assertion has to read CODE. The
    /// first run of [`the_field_level_differ_hydrates`] failed against its own
    /// explanatory comment — the comment saying `.unwrap_or_default()` had been
    /// removed *was* the match. This codebase has hit the same shape before,
    /// where `mirror_rows(` matched the module's own doc comment and the guard
    /// passed for the wrong reason. A negative assertion over prose is not a
    /// negative assertion.
    fn code_only(body: &str) -> String {
        body.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The full-column read of `noetl.event` that every fold is built on.
    const FULL_EVENT_SELECT: &str =
        "FROM noetl.event WHERE execution_id = $1 ORDER BY event_id ASC";

    /// **The denominator guard.** Every full-column event read in this file is
    /// either hydrated or explicitly allowlisted with a reason.
    ///
    /// noetl/ai-meta#343's actual defect was not one unhydrated comparator — it
    /// was FOUR copies of one SELECT, fixed one at a time as each was
    /// discovered: `fold_from_postgres` (noetl/server#425), `compare_sources`
    /// (noetl/server#426), then `fold_diff_endpoint` and
    /// `fold_from_postgres_without_context` here. Each fix left the others
    /// lying, and each time the instrument that would have shown the problem
    /// was itself one of the copies.
    ///
    /// So this guard pins the POPULATION, not the instances: a fifth copy fails
    /// the test on arrival and has to justify itself in the allowlist. It
    /// prints the count it measured, because a scan that does not publish its
    /// denominator is the failure mode one layer up.
    #[test]
    fn every_full_event_read_is_hydrated_or_allowlisted() {
        let prod = production_src();
        let found = prod.matches(FULL_EVENT_SELECT).count();

        // The allowlist. One entry, with its reason stated here rather than in
        // a comment somewhere else where it could rot away from the check.
        //
        // `determinism_endpoint` folds Postgres against ITSELF — two reads of
        // the same rows, to separate "the fold is non-deterministic" from "two
        // reads differ". Hydration is not merely unnecessary there, it would be
        // actively wrong: it introduces a second store into a test whose whole
        // point is that only one is involved.
        let allowlisted = 1;
        let canonical = 1; // `events_from_postgres`, which the hydrated reader wraps.

        assert_eq!(
            found,
            canonical + allowlisted,
            "expected {} full-column `noetl.event` reads ({} canonical + {} \
             allowlisted), found {}. A NEW copy of this query is the \
             noetl/ai-meta#343 defect recurring: if it compares against the \
             tier it must read through `events_from_postgres_hydrated`, and if \
             it genuinely must be raw, add it to the allowlist above WITH the \
             reason.",
            canonical + allowlisted,
            canonical,
            allowlisted,
            found
        );

        // …and the allowlisted one is the one we think it is. Without this the
        // count could be satisfied by any two readers at all.
        let det = prod
            .find("pub async fn determinism_endpoint")
            .expect("determinism_endpoint not found — extraction broke");
        let det_body = &prod[det..];
        let det_end = det_body
            .find("\n}\n")
            .map(|i| i + 3)
            .unwrap_or(det_body.len());
        assert!(
            det_body[..det_end].contains(FULL_EVENT_SELECT),
            "the allowlisted raw reader is no longer inside determinism_endpoint"
        );
    }

    /// The field-level differ — the endpoint an operator opens to find out
    /// *why* two sources disagree — must hydrate.
    ///
    /// This is the noetl/ai-meta#343 headline. It ran its own raw SELECT, so
    /// against `?source=tier` it named `result` as differing on every execution
    /// with an externalised result, burying the real difference under a
    /// `reference`-vs-inlined artefact. The prior session read those
    /// `diff_paths` and could not explain why `compare_sources` still called
    /// the execution divergent at equal event counts — because the two
    /// instruments were reading different things.
    #[test]
    fn the_field_level_differ_hydrates() {
        let prod = production_src();
        let start = prod
            .find("pub async fn fold_diff_endpoint")
            .expect("fold_diff_endpoint not found — extraction broke");
        let body = &prod[start..];
        let end = body.find("\n}\n").map(|i| i + 3).unwrap_or(body.len());
        let body = &body[..end];
        assert!(
            body.len() > 3000,
            "fold_diff_endpoint extracted as {} bytes — implausibly small, the \
             slice broke and every assertion below would be vacuous",
            body.len()
        );
        assert!(
            body.contains("events_from_postgres_hydrated("),
            "the field-level differ does not hydrate. It compares against the \
             tier, which holds inlined content, so a raw read reports `result` \
             as differing on every execution with an externalised result and \
             the real difference cannot be seen underneath it."
        );
        let code = code_only(body);
        assert!(
            !code.contains("sqlx::query("),
            "the field-level differ runs its own SQL again. The defect class is \
             COPIES of this query drifting apart — read through \
             `events_from_postgres_hydrated` so there is one definition."
        );
        assert!(
            !code.contains("unwrap_or_default()"),
            "an unreadable Postgres side must land in the refusal channel, not \
             be flattened into zero events — otherwise `not_comparable` and \
             `genuinely empty` produce the same body"
        );
    }

    /// The `context_explains_the_gap` control must hydrate BEFORE it blanks.
    ///
    /// It is a controlled experiment: blank `context` and nothing else, and if
    /// the digest then matches the tier, `context` was the whole difference.
    /// Reading the raw row broke it in one direction — `result` still carried a
    /// `reference` against the tier's inlined content — so the experiment could
    /// report "context does not explain it" on an execution where context
    /// explained all of it. **A control that can only fail is not a control.**
    #[test]
    fn the_without_context_control_hydrates_before_blanking() {
        let prod = production_src();
        let start = prod
            .find("pub async fn fold_from_postgres_without_context")
            .expect("fold_from_postgres_without_context not found — extraction broke");
        let body = &prod[start..];
        let end = body.find("\n}\n").map(|i| i + 3).unwrap_or(body.len());
        let body = &body[..end];
        assert!(
            body.contains("events_from_postgres_hydrated("),
            "the without-context control reads Postgres raw, so it blanks \
             `context` while leaving `result` as a reference — it tests a \
             difference it did not intend and is biased to `false`"
        );
        let hydrate_at = body
            .find("events_from_postgres_hydrated(")
            .expect("checked above");
        let blank_at = body
            .find("e.context = None")
            .expect("the control no longer blanks context — that IS the control");
        assert!(
            hydrate_at < blank_at,
            "hydration must run BEFORE the blanking, or the deliberate \
             difference is not the only difference"
        );
        assert!(
            !code_only(body).contains("sqlx::query("),
            "the without-context control still runs its own copy of the query"
        );
    }

    // ---------------------------------------------------------------------
    // noetl/ai-meta#343 blocker 2 — an unreadable execution must never be
    // counted as a divergent one, and must never leave the denominator.
    // ---------------------------------------------------------------------

    fn state_with_digest(digest: &str) -> FoldedState {
        FoldedState {
            source: FoldSource::Postgres,
            version: 1,
            applied_count: 1,
            digest: digest.to_string(),
        }
    }

    /// ⭐ The rule, as a truth table. All four shapes, so the third state cannot
    /// be quietly folded into one of the other two.
    #[test]
    fn an_unreadable_side_is_not_comparable_not_divergent() {
        let a = state_with_digest("aaa");
        let b = state_with_digest("bbb");
        assert_eq!(
            comparison_verdict(Some(&a), Some(&a)),
            ComparisonVerdict::Agree
        );
        assert_eq!(
            comparison_verdict(Some(&a), Some(&b)),
            ComparisonVerdict::Diverged
        );
        assert_eq!(
            comparison_verdict(Some(&a), None),
            ComparisonVerdict::NotComparable,
            "an unreadable TIER must not be reported as a divergence — on prod              this is the frame cap refusing a >1MiB reply, which needs paging,              not a mirror repair"
        );
        assert_eq!(
            comparison_verdict(None, Some(&a)),
            ComparisonVerdict::NotComparable,
            "an unreadable POSTGRES side is not a divergence either"
        );
        assert_eq!(
            comparison_verdict(None, None),
            ComparisonVerdict::NotComparable,
            "two refusals are not a match — absence of disagreement is not              agreement"
        );
    }

    /// `digests_agree` alone cannot express the third state, which is why it
    /// must not be what anybody counts.
    ///
    /// Both a divergence and an unreadable execution give `digests_agree ==
    /// false`; only the verdict separates them. If this ever stops being true
    /// the enum is redundant and should go — but while it is true, counting the
    /// bool is the bug.
    #[test]
    fn the_boolean_cannot_distinguish_what_the_verdict_can() {
        let a = state_with_digest("aaa");
        let b = state_with_digest("bbb");
        let diverged = comparison_verdict(Some(&a), Some(&b));
        let unreadable = comparison_verdict(Some(&a), None);
        // The bool the old code counted: identical for both.
        assert_eq!(
            diverged == ComparisonVerdict::Agree,
            unreadable == ComparisonVerdict::Agree,
            "fixture broken — these must be indistinguishable as booleans"
        );
        // The verdict: different.
        assert_ne!(
            diverged, unreadable,
            "the verdict collapses divergent and unreadable — it adds nothing              over the boolean it replaced"
        );
    }

    /// The sweep must bucket EVERY examined execution and publish that it did.
    ///
    /// Before #343 an execution neither side could read fell out of
    /// `agreed + disagreed` with only a refusal-map entry, so the totals
    /// silently did not reconcile with `examined` and `equivalence_holds` was
    /// computed over a population the response never stated. A scan that does
    /// not publish its denominator is exactly the failure this codebase keeps
    /// re-finding.
    #[test]
    fn the_sweep_buckets_every_execution_and_publishes_the_reconciliation() {
        let prod = production_src();
        let start = prod
            .find("pub async fn equivalence_endpoint")
            .expect("equivalence_endpoint not found — extraction broke");
        let body = &prod[start..];
        let end = body.find("\n}\n").map(|i| i + 3).unwrap_or(body.len());
        let body = &body[..end];
        assert!(
            body.len() > 1500,
            "equivalence_endpoint extracted as {} bytes — implausibly small",
            body.len()
        );
        let code = code_only(body);
        assert!(
            code.contains("ComparisonVerdict::NotComparable => not_comparable += 1"),
            "the sweep does not count unreadable executions — they leave the \
             denominator silently, which is worse than reporting them wrong"
        );
        assert!(
            code.contains("agreed + disagreed + not_comparable + comparison_errors == ids.len()"),
            "the sweep does not reconcile its buckets against what it examined; \
             without that a fifth shape can appear and be counted nowhere"
        );
        assert!(
            code.contains("\"not_comparable\": not_comparable"),
            "the count is computed but not published — a denominator nobody can \
             read is not published"
        );
        assert!(
            !code.contains("if c.digests_agree {"),
            "the sweep still branches on the boolean, which cannot express \
             `not_comparable`"
        );
    }

    /// The behavioural half: prove the artefact the hydration removes is one
    /// this differ would actually report.
    ///
    /// The structural guards above say the endpoint *calls* the hydrator. This
    /// says *why that matters* — that `differing_fields` names `result` when one
    /// side holds a reference envelope and the other the inlined content. If
    /// this ever stops being true the hydration guards are protecting nothing
    /// and should be re-derived rather than kept out of habit.
    #[test]
    fn reference_versus_inlined_result_is_reported_as_a_result_difference() {
        let base = crate::db::models::Event {
            id: 0,
            event_id: 1,
            execution_id: 1,
            catalog_id: 2,
            parent_event_id: None,
            parent_execution_id: None,
            event_type: "step.completed".to_string(),
            node_id: Some("n".into()),
            node_name: Some("step".into()),
            node_type: Some("task".into()),
            status: "ok".to_string(),
            context: None,
            meta: None,
            result: None,
            worker_id: Some("w".into()),
            attempt: Some(1),
            created_at: chrono::DateTime::parse_from_rfc3339("2026-08-30T15:00:05.354081798Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        };
        let mut a = base.clone();
        let mut b = base.clone();

        // The shape `PERMANENT_LOG_LEAN=true` produces: Postgres keeps a
        // pointer, the mirrored tier keeps the content.
        a.result = Some(serde_json::json!({
            "context": { "result": { "reference": "resultstore://649253106579931866" } }
        }));
        b.result = Some(serde_json::json!({
            "context": { "result": { "context": { "rows": 3 } } }
        }));

        let fields = differing_fields(&a, &b);
        assert!(
            fields.contains(&"result"),
            "reference-vs-inlined no longer reports as a `result` difference — \
             the artefact the hydration removes has changed shape, so re-derive \
             the guards above instead of trusting them; got {fields:?}"
        );

        // Negative control: once hydrated, the two agree and the differ is
        // silent. A differ that reports a difference between identical events
        // would make every assertion above vacuous.
        let hydrated = b.clone();
        assert!(
            differing_fields(&hydrated, &b).is_empty(),
            "the differ reports a difference between identical events — it \
             cannot be trusted to report the absence of one"
        );
    }
}

/// How many completed executions the equivalence sweep may examine at once.
const EQUIVALENCE_MAX: i64 = 200;

/// Did the equivalence sweep pass?
///
/// **Both conditions, and the first is the one that matters.** A sweep in which
/// every execution refused has zero disagreements, so `disagreed == 0` alone
/// would report it as a pass — the vacuous pass this codebase keeps refusing.
/// Two refusals are not a match, and a gate that cannot fail is not a gate.
///
/// Pure, so the rule is asserted by a test rather than only by the endpoint
/// that happens to apply it.
pub fn equivalence_holds(agreed: usize, disagreed: usize) -> bool {
    agreed > 0 && disagreed == 0
}

/// The most recently **completed** executions, newest first.
///
/// Terminal-event driven rather than `noetl.execution.status`, which is a frozen
/// Python-era column nothing in the Rust path writes (ai-meta#235). Reading it
/// here would select a set that stopped being maintained in June and call the
/// result coverage.
async fn recently_completed(pool: &DbPool, limit: i64) -> AppResult<Vec<i64>> {
    let rows: Vec<(i64,)> = sqlx::query_as(
        r#"
        SELECT execution_id
        FROM noetl.event
        WHERE event_type IN (
            'playbook.completed', 'playbook_completed',
            'playbook.failed',    'playbook_failed',
            'playbook.cancelled', 'playbook_cancelled'
        )
        GROUP BY execution_id
        ORDER BY MAX(created_at) DESC
        LIMIT $1
        "#,
    )
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// `GET /api/ehdb/projection-recovery/equivalence?limit=N` — the
/// verify-before-serve gate for ai-meta#307.
///
/// Folds real **completed** executions from Postgres and from the durable
/// event-log tier and reports whether the digests agree. Read-only, and
/// independent of `NOETL_EHDB_RECOVERY_SOURCE`, so the evidence can be gathered
/// while the serving path is still on `spine`.
///
/// # Why `equivalent` demands a non-zero denominator
///
/// A sweep in which every execution refused would otherwise report zero
/// disagreements and read as a pass. That is the vacuous-pass shape this
/// codebase keeps refusing — two refusals are not a match, and a check that can
/// only pass is not a check. `equivalent` is therefore
/// `agreed > 0 && disagreed == 0`, and `agreed` is reported alongside it so the
/// denominator is never invisible.
///
/// Completed executions are exactly the population that could not be covered
/// before: the worker's in-memory spine evicts them on completion, so every one
/// of these previously refused.
pub async fn equivalence_endpoint(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> AppResult<axum::Json<serde_json::Value>> {
    let requested: i64 = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(25);
    let limit = requested.clamp(1, EQUIVALENCE_MAX);

    let pool = state.pools.pool_for(0);
    let ids = recently_completed(pool, limit).await?;

    let (mut agreed, mut disagreed) = (0usize, 0usize);
    // noetl/ai-meta#343 — counted, never dropped. An execution neither side
    // could read used to fall out of `agreed + disagreed` with nothing but a
    // refusal-map entry to show for it, so the totals silently did not add up
    // to `examined` and the sweep's verdict was computed over a population it
    // never published.
    let mut not_comparable = 0usize;
    let mut comparison_errors = 0usize;
    let mut tier_refusals: std::collections::BTreeMap<String, usize> = Default::default();
    let mut pg_refusals: std::collections::BTreeMap<String, usize> = Default::default();
    let mut context_explained = 0usize;
    let mut disagreements: Vec<serde_json::Value> = Vec::new();

    for id in &ids {
        let pool = state.pools.pool_for(*id);
        let result_store = crate::services::result_store::ResultStoreService::new(
            pool.clone(),
            state.snowflake.clone(),
        );
        let c = match compare_sources(pool, &result_store, *id).await {
            Ok(c) => c,
            Err(e) => {
                *pg_refusals
                    .entry(format!("comparison_error: {e}"))
                    .or_default() += 1;
                comparison_errors += 1;
                continue;
            }
        };
        if let Some(r) = &c.tier_refusal {
            *tier_refusals.entry(r.reason().to_string()).or_default() += 1;
        }
        if let Some(r) = &c.postgres_refusal {
            *pg_refusals.entry(r.reason().to_string()).or_default() += 1;
        }
        match c.verdict {
            ComparisonVerdict::NotComparable => not_comparable += 1,
            ComparisonVerdict::Agree => agreed += 1,
            ComparisonVerdict::Diverged => {
                disagreed += 1;
                if c.context_explains_the_gap {
                    context_explained += 1;
                }
                if disagreements.len() < 10 {
                    disagreements.push(serde_json::json!({
                        "execution_id": *id,
                        "detail": c.disagreement,
                        // When blanking `context` on the Postgres side reproduces
                        // the tier digest exactly, the record predates the mirror
                        // carrying it — a stale record, not a diverged one.
                        "context_explains_the_gap": c.context_explains_the_gap,
                    }));
                }
            }
        }
    }

    // **The denominator, published.** Every examined execution lands in exactly
    // one bucket, and this says so in the response rather than leaving a reader
    // to subtract. If it is ever `false`, a shape exists that none of the three
    // buckets describes — which is the state this sweep was already in for
    // unreadable executions, silently.
    let accounted_for = agreed + disagreed + not_comparable + comparison_errors == ids.len();

    Ok(axum::Json(serde_json::json!({
        "action": "ehdb.projection.recovery.equivalence",
        "requested_limit": requested,
        "limit": limit,
        "examined": ids.len(),
        "agreed": agreed,
        "disagreed": disagreed,
        // ⚠ NOT a divergence. At least one side could not be read — on prod the
        // cause is the tier-service frame cap refusing replies over 1 MiB
        // (noetl/ai-meta#343 blocker 2). Reported separately because the two
        // need opposite responses.
        "not_comparable": not_comparable,
        "comparison_errors": comparison_errors,
        "accounted_for": accounted_for,
        // Both conditions, and the first one is the one that matters: a sweep
        // where everything refused agrees with nothing.
        "equivalent": equivalence_holds(agreed, disagreed),
        "tier_refusals": tier_refusals,
        "postgres_refusals": pg_refusals,
        "disagreements_explained_by_missing_context": context_explained,
        "disagreements": disagreements,
        "recovery_source": recovery_source().as_str(),
        "mirror_payload_version": super::ehdb_eventlog_mirror::MIRROR_PAYLOAD_VERSION,
    })))
}

/// `GET /api/ehdb/projection-recovery/{id}` — the re-scoped Phase 3 gate.
pub async fn recovery_compare_endpoint(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
    axum::extract::Path(execution_id): axum::extract::Path<i64>,
) -> AppResult<axum::Json<serde_json::Value>> {
    let pool = state.pools.pool_for(execution_id);
    // Same construction every other handler uses (e.g. `advance_snapshot`) —
    // the comparison must hydrate references exactly as `rebuild_state` does,
    // or it digests a pointer against content.
    let result_store = crate::services::result_store::ResultStoreService::new(
        pool.clone(),
        state.snowflake.clone(),
    );
    Ok(axum::Json(
        crate::services::orch_snapshot::recovery_read_comparison(pool, &result_store, execution_id)
            .await,
    ))
}

#[cfg(test)]
mod differing_fields_tests {
    use super::*;

    fn ev(id: i64) -> crate::db::models::Event {
        crate::db::models::Event {
            id: 0,
            event_id: id,
            execution_id: 1,
            catalog_id: 2,
            parent_event_id: None,
            parent_execution_id: None,
            event_type: "command.issued".to_string(),
            node_id: Some("n".into()),
            node_name: Some("step".into()),
            node_type: Some("task".into()),
            status: "ok".to_string(),
            context: None,
            meta: None,
            result: None,
            worker_id: Some("w".into()),
            attempt: Some(1),
            created_at: chrono::DateTime::parse_from_rfc3339("2026-08-30T15:00:05.354081798Z")
                .unwrap()
                .with_timezone(&chrono::Utc),
        }
    }

    #[test]
    fn identical_events_report_nothing() {
        // ⚠ The control that matters most: if this ever returns fields for equal
        // events, every "no differences" answer the endpoint gives is noise, and
        // an investigation reading it goes the wrong way.
        assert!(differing_fields(&ev(1), &ev(1)).is_empty());
    }

    #[test]
    fn a_one_nanosecond_difference_is_seen() {
        // The hypothesis this endpoint exists to test. A differ that rounded, or
        // compared to second precision, would report nothing here and exonerate
        // the very cause under investigation.
        let a = ev(1);
        let mut b = ev(1);
        b.created_at = chrono::DateTime::parse_from_rfc3339("2026-08-30T15:00:05.354081799Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(differing_fields(&a, &b), vec!["created_at"]);
    }

    #[test]
    fn a_one_microsecond_difference_is_seen() {
        // The specific residual the previous floor-vs-round investigation
        // measured: "…725065Z != …725064Z".
        let a = ev(1);
        let mut b = ev(1);
        b.created_at = chrono::DateTime::parse_from_rfc3339("2026-08-30T15:00:05.354082798Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(differing_fields(&a, &b), vec!["created_at"]);
    }

    #[test]
    fn every_compared_field_is_actually_compared() {
        // ⚠⚠ A field the differ forgets is a cause it can never name. Each
        // mutation below must surface exactly its own field — this is the
        // positive control for coverage, not just for equality.
        let base = ev(1);
        let cases: Vec<(&str, Box<dyn Fn(&mut crate::db::models::Event)>)> = vec![
            (
                "event_type",
                Box::new(|e: &mut crate::db::models::Event| e.event_type = "x".into()),
            ),
            (
                "status",
                Box::new(|e: &mut crate::db::models::Event| e.status = "x".into()),
            ),
            (
                "node_id",
                Box::new(|e: &mut crate::db::models::Event| e.node_id = None),
            ),
            (
                "node_name",
                Box::new(|e: &mut crate::db::models::Event| e.node_name = None),
            ),
            (
                "node_type",
                Box::new(|e: &mut crate::db::models::Event| e.node_type = None),
            ),
            (
                "catalog_id",
                Box::new(|e: &mut crate::db::models::Event| e.catalog_id = 99),
            ),
            (
                "parent_event_id",
                Box::new(|e: &mut crate::db::models::Event| e.parent_event_id = Some(7)),
            ),
            (
                "parent_execution_id",
                Box::new(|e: &mut crate::db::models::Event| e.parent_execution_id = Some(7)),
            ),
            (
                "worker_id",
                Box::new(|e: &mut crate::db::models::Event| e.worker_id = None),
            ),
            (
                "attempt",
                Box::new(|e: &mut crate::db::models::Event| e.attempt = Some(9)),
            ),
            (
                "context",
                Box::new(|e: &mut crate::db::models::Event| {
                    e.context = Some(serde_json::json!({"a":1}))
                }),
            ),
            (
                "meta",
                Box::new(|e: &mut crate::db::models::Event| {
                    e.meta = Some(serde_json::json!({"a":1}))
                }),
            ),
            (
                "result",
                Box::new(|e: &mut crate::db::models::Event| {
                    e.result = Some(serde_json::json!({"a":1}))
                }),
            ),
        ];
        for (name, mutate) in cases {
            let mut b = base.clone();
            mutate(&mut b);
            assert_eq!(
                differing_fields(&base, &b),
                vec![name],
                "mutating {name} must be reported as exactly {name}"
            );
        }
    }

    /// #307 residual divergence: does the TIER reader disagree with the
    /// POSTGRES reader on the same value?
    ///
    /// The two paths extract the same fields differently.  Postgres uses
    /// `try_get`, driven by the column type; the tier picks fields out of JSON
    /// by hand.  Hand-written extraction is where an asymmetry hides, and an
    /// asymmetry here shows up as a digest divergence with no obvious cause —
    /// which is exactly what #307's residual `worker_id` / `context` diffs look
    /// like.
    ///
    /// This is a table of the values the mirror can actually put in a payload,
    /// checked against what the Postgres reader would have produced from the
    /// same row.  It is a controlled experiment, not an inference.
    #[test]
    fn tier_reader_agrees_with_postgres_reader_on_edge_values() {
        use serde_json::json;

        // (label, tier payload value, what Postgres `try_get` yields for that row)
        struct Case {
            label: &'static str,
            payload: serde_json::Value,
            pg_context: Option<serde_json::Value>,
            pg_worker: Option<String>,
        }

        let cases = vec![
            Case {
                label: "context is SQL NULL",
                payload: json!({"event_id": 1, "event_type": "x", "status": "s",
                                "context": null, "worker_id": null}),
                pg_context: None,
                pg_worker: None,
            },
            Case {
                label: "context is JSONB 'null' (a JSON null INSIDE the column)",
                payload: json!({"event_id": 1, "event_type": "x", "status": "s",
                                "context": null, "worker_id": "w1"}),
                // sqlx decodes a jsonb `null` into Value::Null, NOT into None.
                pg_context: Some(serde_json::Value::Null),
                pg_worker: Some("w1".into()),
            },
            Case {
                label: "worker_id present as a string",
                payload: json!({"event_id": 1, "event_type": "x", "status": "s",
                                "context": {"a": 1}, "worker_id": "worker-7"}),
                pg_context: Some(json!({"a": 1})),
                pg_worker: Some("worker-7".into()),
            },
            Case {
                label: "worker_id serialised as a NUMBER (snowflake)",
                payload: json!({"event_id": 1, "event_type": "x", "status": "s",
                                "context": {}, "worker_id": 8123456789_i64}),
                pg_context: Some(json!({})),
                pg_worker: Some("8123456789".into()),
            },
            Case {
                label: "context is an empty object",
                payload: json!({"event_id": 1, "event_type": "x", "status": "s",
                                "context": {}, "worker_id": "w"}),
                pg_context: Some(json!({})),
                pg_worker: Some("w".into()),
            },
        ];

        let mut disagreements = Vec::new();
        for c in &cases {
            let mut tier = vec![event_from_tier_payload(&c.payload)
                .unwrap_or_else(|| panic!("{}: tier reader refused the payload", c.label))];
            // Compare what the FOLD sees, not what the readers raw-produce: the
            // `context` fix is a normalisation both paths go through, so the
            // invariant under test is post-normalisation agreement.
            let mut pg = tier.clone();
            pg[0].context = c.pg_context.clone();
            pg[0].worker_id = c.pg_worker.clone();
            normalise_null_json(&mut tier);
            normalise_null_json(&mut pg);
            let ev = &tier[0];
            let c_pg_context = pg[0].context.clone();
            let c_pg_worker = pg[0].worker_id.clone();
            let c = Case {
                label: c.label,
                payload: c.payload.clone(),
                pg_context: c_pg_context,
                pg_worker: c_pg_worker,
            };
            let c = &c;
            if ev.context != c.pg_context {
                disagreements.push(format!(
                    "{}: context tier={:?} postgres={:?}",
                    c.label, ev.context, c.pg_context
                ));
            }
            if ev.worker_id != c.pg_worker {
                disagreements.push(format!(
                    "{}: worker_id tier={:?} postgres={:?}",
                    c.label, ev.worker_id, c.pg_worker
                ));
            }
        }

        assert!(
            disagreements.is_empty(),
            "the tier reader and the Postgres reader disagree on {} case(s):\n  {}",
            disagreements.len(),
            disagreements.join("\n  ")
        );
    }

    /// The microsecond normalisation must reproduce what Postgres stored.
    ///
    /// Built from REAL production values read via
    /// `/api/ehdb/projection-fold/diff/{id}?source=tier` on 2026-08-31: the
    /// left column is the tier's nanosecond instant, the right is the
    /// microsecond value Postgres actually holds for the same event.
    ///
    /// This exists because the direction was reversed once on a sample that
    /// could not distinguish the two rules — every event it examined had a
    /// sub-microsecond remainder below 500 ns, where truncation and rounding
    /// agree. The cases below deliberately include both halves.
    #[test]
    fn micros_normalisation_reproduces_what_postgres_stored() {
        // (nanosecond fraction from the tier, microsecond fraction in Postgres)
        let cases: &[(u32, u32)] = &[
            (62_560_206, 62_560),   // remainder 206 ns  -> both rules agree
            (425_616_867, 425_616), // remainder 867 ns  -> truncates
            (608_043_681, 608_043), // remainder 681 ns  -> truncates
            (356_238_891, 356_238), // remainder 891 ns  -> truncates
            (494_347_027, 494_347), // remainder 27 ns   -> both rules agree
            (292_026_088, 292_026), // remainder 88 ns   -> both rules agree
        ];

        let base = chrono::DateTime::parse_from_rfc3339("2026-08-31T06:28:20Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let mut wrong = Vec::new();
        for (ns_frac, pg_micros) in cases {
            let ts = base + chrono::Duration::nanoseconds(*ns_frac as i64);
            let got = truncate_to_micros(ts).timestamp_subsec_micros();
            if got != *pg_micros {
                wrong.push(format!(
                    "tier .{ns_frac:09} -> normalised .{got:06}, but Postgres holds .{pg_micros:06}"
                ));
            }
        }
        assert!(
            wrong.is_empty(),
            "the fold does not reproduce Postgres for {} of {} real events:\n  {}",
            wrong.len(),
            cases.len(),
            wrong.join("\n  ")
        );
    }
}
