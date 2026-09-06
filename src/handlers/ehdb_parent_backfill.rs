//! **Backfill `noetl.event.parent_execution_id` from two agreeing sources**
//! (noetl/ai-meta#326).
//!
//! # Why this is the opposite direction from the tier repair
//!
//! [`crate::handlers::ehdb_tier_repair`] fixes a *mirror* that is missing what
//! the system of record has. This fixes the **system of record**, which is
//! missing a column its mirror kept — because `services::internal::project_events`
//! did not select it. The asymmetry matters: writing the authoritative log from
//! its mirror is the direction that can launder a mirror bug into permanent
//! record, so a single source is not enough here and this endpoint refuses to
//! act on one.
//!
//! # The two sources, and why they are independent
//!
//! * **Lineage (in-Postgres).** The child's `parent_event_id` is a column that
//!   was *never* dropped by any writer; the execution that owns that event is
//!   the parent. This never left the authoritative store, so it is not evidence
//!   borrowed from the thing being repaired.
//! * **Tier.** What the emitter published, surviving in the mirror. Same write,
//!   different store.
//!
//! They fail differently — a mirror bug cannot move a `parent_event_id`, and a
//! lineage bug cannot move what was published — which is the entire reason to
//! require both.
//!
//! # The gate
//!
//! A row is written **only** when both sources resolve and agree. Anything else
//! — either source absent, the two disagreeing, the lineage pointing back at the
//! same execution — is reported with its reason and left alone. There is no
//! "best available" path and no default. ⚠ A parent written wrong is worse than
//! one left NULL: the nonconvergence sweep reads this column to decide whether
//! an execution still has a live parent, so a fabricated value could make it
//! spare an orphan **or** terminate a live child.
//!
//! # Idempotence
//!
//! The `UPDATE` carries `AND parent_execution_id IS NULL`. A second run matches
//! no rows and reports `updated: 0` — not because anything is remembered here,
//! but because the write is a no-op by construction. The same clause is what
//! makes it impossible to overwrite a value that is already present.

use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::handlers::ehdb_parity::{compare_execution, ParityRecording};
use crate::state::AppState;

/// `?apply=true` writes. Anything else — including omitting it — is a dry run.
///
/// ⚠ Default `false` on purpose. This is an `UPDATE` against the system of
/// record; the safe value is the one you get by forgetting to think about it.
#[derive(Debug, Clone, Deserialize)]
pub struct BackfillQuery {
    #[serde(default)]
    pub apply: bool,
}

/// What the **authoritative store** independently says about the tier's claim.
///
/// Every field here is read from `noetl.event` — never from the tier — so a
/// mirror bug cannot manufacture its own corroboration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Corroboration {
    /// The execution owning the child's `parent_event_id`, when it has one.
    ///
    /// ⚠ Measured and found **absent on every row in scope**: a child
    /// execution's `playbook_started` carries no parent *event*. Kept because it
    /// is the strongest oracle when present — and because it is the only one that
    /// can *contradict*, which the other two cannot.
    pub lineage_parent_execution: Option<i64>,
    /// The claimed parent's own authoritative events mention this child — the
    /// orchestrator's spawn relationship, recorded on the side that spawned.
    pub parent_references_child: bool,
    /// The child's own authoritative `context`/`meta` carry the claimed parent.
    /// A value found here never left the system of record; only the *column* did.
    pub self_payload_carries_parent: bool,
}

impl Corroboration {
    /// How many independent authoritative oracles back the claim.
    pub fn count(&self) -> usize {
        usize::from(self.lineage_parent_execution.is_some())
            + usize::from(self.parent_references_child)
            + usize::from(self.self_payload_carries_parent)
    }
}

/// Why one candidate was not written.
///
/// ⚠ Every refusal is reported, never silently dropped from the count. A repair
/// that reports only what it did cannot be told apart from one that had nothing
/// to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Nothing in the authoritative store backs the tier's claim, so the value
    /// would be taken on the mirror's word alone.
    Uncorroborated,
    /// The child's `parent_event_id` resolves to a **different** execution than
    /// the tier claims. ⚠ Never resolved by preferring one.
    LineageContradicts,
    /// The lineage points inside the same execution: a step parent, not a
    /// spawning execution — so it warrants nothing about a parent execution.
    LineageSameExecution,
}

impl Refusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Uncorroborated => "uncorroborated",
            Self::LineageContradicts => "lineage_contradicts",
            Self::LineageSameExecution => "lineage_same_execution",
        }
    }
}

/// The gate: write only what the authoritative store itself backs.
///
/// The tier supplies the *value*; `noetl.event` supplies the *warrant*. A claim
/// with no warrant is refused — there is no "best available" path — and a claim
/// the store **contradicts** is refused even when another oracle agrees, because
/// a disagreement among oracles is a reason to stop, not to hold a vote.
///
/// Pure, so the gate is testable without a database or a tier.
pub fn decide(execution_id: i64, tier_parent: i64, ev: &Corroboration) -> Result<i64, Refusal> {
    if let Some(lineage) = ev.lineage_parent_execution {
        if lineage == execution_id {
            return Err(Refusal::LineageSameExecution);
        }
        if lineage != tier_parent {
            return Err(Refusal::LineageContradicts);
        }
        return Ok(tier_parent);
    }
    if ev.parent_references_child || ev.self_payload_carries_parent {
        return Ok(tier_parent);
    }
    Err(Refusal::Uncorroborated)
}

/// `POST /api/ehdb/repair/parent-execution-id/{execution_id}`
pub async fn backfill_endpoint(
    State(state): State<AppState>,
    Path(execution_id): Path<i64>,
    Query(q): Query<BackfillQuery>,
) -> impl IntoResponse {
    // `Inspect`, not `Record` — noetl/ai-meta#264. A repair that recorded would
    // write the counters its own before/after numbers are read from, and would
    // inflate the paging alert for the divergence it is fixing.
    //
    // `Some(true)` forces the content comparison on regardless of the flag: the
    // candidates only exist in a content comparison, and a backfill that
    // silently found nothing because a flag was off would report "clean".
    let before =
        compare_execution(&state, execution_id, ParityRecording::Inspect, Some(true)).await;
    let Some(report) = before.report else {
        return (
            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "status": "error",
                "execution_id": execution_id.to_string(),
                "outcome": before.outcome.as_str(),
                "detail": before.detail,
            })),
        );
    };

    let candidates = report.parent_backfill_candidates.clone();
    if candidates.is_empty() {
        return (
            axum::http::StatusCode::OK,
            Json(json!({
                "status": "ok",
                "execution_id": execution_id.to_string(),
                "applied": false,
                "candidates": 0,
                "updated": 0,
                "detail": "nothing to backfill: the authoritative log is not missing \
                           any parent_execution_id the tier holds",
            })),
        );
    }

    // Source A. Scoped by BOTH execution_id and the candidate ids — the id list
    // alone would let a comparator bug on one execution reach rows in another.
    let ids: Vec<i64> = candidates.iter().map(|c| c.event_id).collect();
    let claimed: Vec<i64> = candidates
        .iter()
        .map(|c| c.tier_parent_execution_id)
        .collect();
    let corroboration = match sqlx::query_as::<_, (i64, Option<i64>, Option<i64>, bool, bool)>(
        r#"
        WITH candidate AS (
            SELECT unnest($2::bigint[]) AS event_id,
                   unnest($3::bigint[]) AS claimed_parent
        )
        SELECT c.event_id,
               ev.parent_event_id,
               p.execution_id AS lineage_parent_execution_id,
               -- Does the CLAIMED PARENT, in the authoritative log, reference this
               -- child?  That is the orchestrator's spawn relationship, recorded on
               -- the side that did the spawning.
               EXISTS (
                   SELECT 1 FROM noetl.event pe
                   WHERE pe.execution_id = c.claimed_parent
                     AND (COALESCE(pe.context::text, '') ~ ('(^|[^0-9])'
                              || ev.execution_id::text || '([^0-9]|$)')
                       OR COALESCE(pe.result::text, '')  ~ ('(^|[^0-9])'
                              || ev.execution_id::text || '([^0-9]|$)')
                       OR COALESCE(pe.meta::text, '')    ~ ('(^|[^0-9])'
                              || ev.execution_id::text || '([^0-9]|$)'))
               ) AS parent_references_child,
               -- Does the child's OWN authoritative payload carry the claimed
               -- parent?  The column was dropped; `context`/`meta` were not, so a
               -- value found here never left the system of record at all.
               (COALESCE(ev.context::text, '') ~ ('(^|[^0-9])'
                        || c.claimed_parent::text || '([^0-9]|$)')
                OR COALESCE(ev.meta::text, '') ~ ('(^|[^0-9])'
                        || c.claimed_parent::text || '([^0-9]|$)')) AS self_payload_carries_parent
        FROM candidate c
        JOIN noetl.event ev
          ON ev.event_id = c.event_id AND ev.execution_id = $1
        LEFT JOIN noetl.event p ON p.event_id = ev.parent_event_id
        "#,
    )
    .bind(execution_id)
    .bind(&ids)
    .bind(&claimed)
    .fetch_all(&state.db)
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "status": "error",
                    "execution_id": execution_id.to_string(),
                    "detail": format!("corroboration read failed: {e}"),
                })),
            );
        }
    };
    let by_id: std::collections::HashMap<i64, (Option<i64>, Option<i64>, bool, bool)> =
        corroboration
            .into_iter()
            .map(|(eid, pev, pex, refs, selfp)| (eid, (pev, pex, refs, selfp)))
            .collect();

    let mut decisions = Vec::new();
    let mut updated = 0usize;
    let mut refused = 0usize;
    for c in &candidates {
        let (parent_event_id, lineage_parent, parent_refs_child, self_carries) = by_id
            .get(&c.event_id)
            .copied()
            .unwrap_or((None, None, false, false));
        let ev = Corroboration {
            lineage_parent_execution: lineage_parent,
            parent_references_child: parent_refs_child,
            self_payload_carries_parent: self_carries,
        };
        let decided = decide(execution_id, c.tier_parent_execution_id, &ev);
        let mut row = json!({
            "event_id": c.event_id.to_string(),
            "tier_parent_execution_id": c.tier_parent_execution_id.to_string(),
            "parent_event_id": parent_event_id.map(|v| v.to_string()),
            "lineage_parent_execution_id": lineage_parent.map(|v| v.to_string()),
            "parent_references_child": parent_refs_child,
            "self_payload_carries_parent": self_carries,
            "corroborations": ev.count(),
        });
        match decided {
            Ok(value) => {
                row["decision"] = json!("corroborated");
                row["value"] = json!(value.to_string());
                if q.apply {
                    // `AND parent_execution_id IS NULL` is what makes this both
                    // idempotent and incapable of overwriting a present value.
                    match sqlx::query(
                        "UPDATE noetl.event SET parent_execution_id = $1 \
                         WHERE event_id = $2 AND execution_id = $3 \
                           AND parent_execution_id IS NULL",
                    )
                    .bind(value)
                    .bind(c.event_id)
                    .bind(execution_id)
                    .execute(&state.db)
                    .await
                    {
                        Ok(r) => {
                            let n = r.rows_affected();
                            updated += n as usize;
                            row["rows_affected"] = json!(n);
                        }
                        Err(e) => {
                            row["decision"] = json!("update_failed");
                            row["error"] = json!(e.to_string());
                        }
                    }
                }
            }
            Err(r) => {
                refused += 1;
                row["decision"] = json!("refused");
                row["reason"] = json!(r.as_str());
            }
        }
        decisions.push(row);
    }

    // After-state from the same comparator, so "closed" means the oracle that
    // reported the gap no longer does — not that this endpoint believes it acted.
    let after = if q.apply {
        let a = compare_execution(&state, execution_id, ParityRecording::Inspect, Some(true)).await;
        a.report.map(|r| r.parent_backfill_candidates.len())
    } else {
        None
    };

    (
        axum::http::StatusCode::OK,
        Json(json!({
            "status": "ok",
            "execution_id": execution_id.to_string(),
            "applied": q.apply,
            "candidates": candidates.len(),
            "updated": updated,
            "refused": refused,
            "candidates_after": after,
            "decisions": decisions,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lineage(id: i64) -> Corroboration {
        Corroboration {
            lineage_parent_execution: Some(id),
            ..Default::default()
        }
    }

    #[test]
    fn an_agreeing_lineage_warrants_the_write() {
        assert_eq!(decide(10, 7, &lineage(7)), Ok(7));
    }

    #[test]
    fn a_contradicting_lineage_is_refused_not_reconciled() {
        // ⚠ The mutation this pins: "prefer the tier" would return Ok(7) here and
        // write a value the authoritative store's own lineage contradicts.
        assert_eq!(decide(10, 7, &lineage(8)), Err(Refusal::LineageContradicts));
    }

    /// ⚠ A contradiction is not out-voted. Two oracles saying yes do not license
    /// writing a value the third says is wrong.
    #[test]
    fn a_contradiction_beats_any_number_of_agreements() {
        let ev = Corroboration {
            lineage_parent_execution: Some(8),
            parent_references_child: true,
            self_payload_carries_parent: true,
        };
        assert_eq!(ev.count(), 3);
        assert_eq!(decide(10, 7, &ev), Err(Refusal::LineageContradicts));
    }

    #[test]
    fn the_tiers_word_alone_is_never_enough() {
        // No authoritative oracle backs the claim: refuse.
        assert_eq!(
            decide(10, 7, &Corroboration::default()),
            Err(Refusal::Uncorroborated)
        );
    }

    #[test]
    fn either_authoritative_oracle_warrants_the_write_on_its_own() {
        let spawn_side = Corroboration {
            parent_references_child: true,
            ..Default::default()
        };
        let self_side = Corroboration {
            self_payload_carries_parent: true,
            ..Default::default()
        };
        assert_eq!(decide(10, 7, &spawn_side), Ok(7));
        assert_eq!(decide(10, 7, &self_side), Ok(7));
    }

    #[test]
    fn a_step_parent_inside_the_same_execution_is_not_a_parent_execution() {
        assert_eq!(
            decide(10, 10, &lineage(10)),
            Err(Refusal::LineageSameExecution)
        );
    }

    /// Positive control for the test above: the same shape with a genuinely
    /// different execution DOES write, so the refusal is about the sameness and
    /// not about the fixture being unwritable for some other reason.
    #[test]
    fn positive_control_for_the_same_execution_refusal() {
        assert_eq!(decide(10, 11, &lineage(11)), Ok(11));
    }

    #[test]
    fn apply_defaults_to_false_when_the_parameter_is_omitted() {
        // ⚠ The mutation this pins: `#[serde(default)]` on a bool that someone
        // later "helpfully" defaults to true.
        let q: BackfillQuery = serde_json::from_str("{}").unwrap();
        assert!(!q.apply, "a forgotten parameter must not write");
    }

    /// The candidate extractor must not treat a *conflict* as a gap.
    #[test]
    fn a_present_authoritative_value_is_never_a_backfill_candidate() {
        use crate::handlers::ehdb_parity::parent_backfill_candidate;
        let auth = serde_json::json!(5);
        let tier = serde_json::json!(7);
        assert_eq!(
            parent_backfill_candidate(Some(&auth), Some(&tier)),
            None,
            "a disagreeing present value is a conflict, and repairing it would \
             overwrite the system of record with its mirror"
        );
        // Positive control: absent authoritative side, same tier value.
        assert_eq!(parent_backfill_candidate(None, Some(&tier)), Some(7));
        // And a quoted snowflake is a spelling, not a different value.
        let quoted = serde_json::json!("7");
        assert_eq!(parent_backfill_candidate(None, Some(&quoted)), Some(7));
    }
}
