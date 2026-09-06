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

/// Why one candidate was not written.
///
/// ⚠ Every refusal is reported, never silently dropped from the count. A repair
/// that reports only what it did cannot be told apart from one that had nothing
/// to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The child event carries no `parent_event_id`, so there is no second source.
    NoLineage,
    /// `parent_event_id` resolves to no row — the parent event is gone.
    LineageUnresolved,
    /// The lineage points inside the same execution: a step parent, not a
    /// spawning execution.
    LineageSameExecution,
    /// Both sources resolved and disagreed. ⚠ Never resolved by preferring one.
    SourcesDisagree,
}

impl Refusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoLineage => "no_lineage",
            Self::LineageUnresolved => "lineage_unresolved",
            Self::LineageSameExecution => "lineage_same_execution",
            Self::SourcesDisagree => "sources_disagree",
        }
    }
}

/// The decision for one candidate, given both sources.
///
/// Pure, so the gate is testable without a database or a tier.
pub fn decide(
    execution_id: i64,
    tier_parent: i64,
    lineage_parent_execution: Option<i64>,
) -> Result<i64, Refusal> {
    let lineage = lineage_parent_execution.ok_or(Refusal::NoLineage)?;
    if lineage == execution_id {
        return Err(Refusal::LineageSameExecution);
    }
    if lineage != tier_parent {
        return Err(Refusal::SourcesDisagree);
    }
    Ok(lineage)
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
    let lineage = match sqlx::query_as::<_, (i64, Option<i64>, Option<i64>)>(
        r#"
        SELECT c.event_id,
               c.parent_event_id,
               p.execution_id AS lineage_parent_execution_id
        FROM noetl.event c
        LEFT JOIN noetl.event p ON p.event_id = c.parent_event_id
        WHERE c.execution_id = $1
          AND c.event_id = ANY($2)
        "#,
    )
    .bind(execution_id)
    .bind(&ids)
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
                    "detail": format!("lineage read failed: {e}"),
                })),
            );
        }
    };
    let lineage_by_id: std::collections::HashMap<i64, (Option<i64>, Option<i64>)> = lineage
        .into_iter()
        .map(|(eid, pev, pex)| (eid, (pev, pex)))
        .collect();

    let mut decisions = Vec::new();
    let mut updated = 0usize;
    let mut refused = 0usize;
    for c in &candidates {
        let (parent_event_id, lineage_parent) = lineage_by_id
            .get(&c.event_id)
            .copied()
            .unwrap_or((None, None));
        // Distinguish "no link recorded" from "link recorded but dangling" —
        // they are different failures and collapsing them hides which.
        let decided = match decide(execution_id, c.tier_parent_execution_id, lineage_parent) {
            Err(Refusal::NoLineage) if parent_event_id.is_some() => Err(Refusal::LineageUnresolved),
            other => other,
        };
        let mut row = json!({
            "event_id": c.event_id.to_string(),
            "tier_parent_execution_id": c.tier_parent_execution_id.to_string(),
            "parent_event_id": parent_event_id.map(|v| v.to_string()),
            "lineage_parent_execution_id": lineage_parent.map(|v| v.to_string()),
        });
        match decided {
            Ok(value) => {
                row["decision"] = json!("agree");
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

    #[test]
    fn agreeing_sources_are_the_only_path_to_a_write() {
        assert_eq!(decide(10, 7, Some(7)), Ok(7));
    }

    #[test]
    fn disagreeing_sources_are_refused_not_reconciled() {
        // ⚠ The mutation this pins: "prefer the tier" would return Ok(7) here and
        // write a value the authoritative store's own lineage contradicts.
        assert_eq!(decide(10, 7, Some(8)), Err(Refusal::SourcesDisagree));
    }

    #[test]
    fn a_single_source_is_never_enough() {
        assert_eq!(decide(10, 7, None), Err(Refusal::NoLineage));
    }

    #[test]
    fn a_step_parent_inside_the_same_execution_is_not_a_parent_execution() {
        assert_eq!(decide(10, 10, Some(10)), Err(Refusal::LineageSameExecution));
    }

    /// Positive control for the test above: the same shape with a genuinely
    /// different execution DOES write, so the refusal is about the sameness and
    /// not about the fixture being unwritable for some other reason.
    #[test]
    fn positive_control_for_the_same_execution_refusal() {
        assert_eq!(decide(10, 11, Some(11)), Ok(11));
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
