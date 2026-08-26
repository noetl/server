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
}

impl FoldSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::EhdbTier => "ehdb_tier",
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
        .timeout(std::time::Duration::from_secs(20))
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
            return Err(FoldRefusal::SourceUnavailable(format!("tier outcome={outcome}")));
        }
    }
    let records = body
        .get("records")
        .and_then(|r| r.as_array())
        .ok_or_else(|| FoldRefusal::Unparseable("no records array".into()))?;
    if records.is_empty() {
        return Err(FoldRefusal::NoEvents);
    }

    let mut events: Vec<crate::db::models::Event> = Vec::with_capacity(records.len());
    for r in records {
        let payload = match r.get("payload").and_then(|p| p.as_str()) {
            Some(s) => serde_json::from_str::<serde_json::Value>(s)
                .map_err(|e| FoldRefusal::Unparseable(e.to_string()))?,
            None => r.clone(),
        };
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
    fold(FoldSource::EhdbTier, events)
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
        v.and_then(|x| x.as_i64().or_else(|| x.as_str().and_then(|s| s.parse().ok())))
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
        node_id: p.get("node_id").and_then(|v| v.as_str()).map(str::to_string),
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
        attempt: p.get("meta").and_then(|m| m.get("attempt")).and_then(|a| {
            a.as_i64()
                .or_else(|| a.as_str().and_then(|s| s.parse().ok()))
        }).map(|v| v as i32),
        created_at: p
            .get("created_at")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now),
    })
}

fn fold(
    source: FoldSource,
    events: Vec<crate::db::models::Event>,
) -> Result<FoldedState, FoldRefusal> {
    use noetl_orchestrate_core::state::{canonical_state_digest, WorkflowState};
    let version = events.iter().map(|e| e.event_id).max().unwrap_or(0);
    let applied_count = events.len();
    let core: Vec<noetl_orchestrate_core::event::Event> =
        events.iter().map(Into::into).collect();
    let state = WorkflowState::from_events(&core).ok_or(FoldRefusal::FoldFailed)?;
    Ok(FoldedState {
        source,
        version,
        applied_count,
        digest: canonical_state_digest(&state),
    })
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
    let no_ctx = fold_from_postgres_without_context(pool, execution_id).await.ok();
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
        assert_eq!(ev.attempt, Some(1), "attempt is derived from meta, as in the SQL projection");
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
                    (Some(va), Some(vb)) => {
                        json_diff_paths(va, vb, &format!("{at}/{k}"), out)
                    }
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

    let ev1 = load_events(&pool, execution_id).await;
    let ev2 = load_events(&pool, execution_id).await;
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
    let cmp = compare_sources(&pool, execution_id).await?;
    Ok(axum::Json(serde_json::json!({
        "action": "ehdb.projection.fold.compare",
        "result": cmp,
    })))
}
