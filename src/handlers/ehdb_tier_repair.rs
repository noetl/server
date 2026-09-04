//! **Repair an execution's tier coverage** — re-mirror only the events the
//! comparator says are absent (noetl/ai-meta#313).
//!
//! # Why this has to exist as an endpoint
//!
//! The parity comparator reports which mirror-expected events the tier does not
//! hold, but nothing can act on that report. The mirror fires on write; there is
//! no re-drive. And a repair cannot run from outside the server:
//! `data-access-boundary.md` restricts `noetl.*` reads to the server, so the
//! authoritative rows are only reachable here.
//!
//! # Why it is safe to run now and was not before
//!
//! Re-mirroring used to mean duplicating: the tier had no idempotency key, so a
//! record delivered twice landed twice — that is noetl/ai-meta#313, which
//! observed 11 duplicates from exactly this shape. With `event_id` now carried on
//! the tier append and deduplicated at the driver, a re-delivery is acknowledged
//! at its existing position instead. **A repair is therefore idempotent by
//! construction, not by bookkeeping here.**
//!
//! # Three properties this deliberately holds
//!
//! * **Scoped.** Only the ids the comparator reported absent are re-mirrored,
//!   never the execution's whole log. A blind full re-mirror would be correct
//!   *only because* dedupe absorbs it — which is a guarantee borrowed from
//!   another component, on the write path of a `primary`-serving tier.
//! * **Non-inflating.** Both comparisons run with [`ParityRecording::Inspect`].
//!   noetl/ai-meta#264: the parity *endpoint* used to write the very counters its
//!   own paging alert read, so investigating a divergence inflated it. A repair
//!   that recorded would do the same, and would additionally make its own
//!   before/after numbers unreadable.
//! * **Honest about what it cannot fix.** An id the comparator calls absent but
//!   for which no authoritative row comes back is reported as `unrecoverable`,
//!   not quietly dropped from the count. That is the difference between "the gap
//!   closed" and "the gap stopped being measured".

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::json;

use crate::handlers::ehdb_parity::{compare_execution, ParityRecording};
use crate::handlers::event_write::EventRow;
use crate::state::AppState;

/// The ids the comparator called absent for which no authoritative row exists.
///
/// ⚠ Reported, never subtracted. An id with no authoritative row cannot be
/// repaired from here — the source is gone — and quietly dropping it from the
/// count is how "the gap closed" comes to mean "the gap stopped being measured".
pub fn unrecoverable_ids(missing: &[i64], fetched: impl Iterator<Item = i64>) -> Vec<i64> {
    let have: std::collections::HashSet<i64> = fetched.collect();
    missing
        .iter()
        .copied()
        .filter(|id| !have.contains(id))
        .collect()
}

/// `repaired` only when NOTHING is still missing.
///
/// ⚠ Keyed on the after-count, not on how many rows were delivered. A repair that
/// mirrored ten records while ten others remain absent has not repaired the
/// execution, and reporting on the delivery count would say it had.
pub fn repair_outcome(missing_after: usize) -> &'static str {
    if missing_after == 0 {
        "repaired"
    } else {
        "partial"
    }
}

/// Fetch the full rows for a specific set of event ids.
///
/// ⚠ Scoped by BOTH `execution_id` and the id list. The id list alone would let a
/// comparator bug on one execution pull rows belonging to another into that
/// execution's tier stream.
async fn fetch_rows(
    state: &AppState,
    execution_id: i64,
    ids: &[i64],
) -> Result<Vec<EventRow>, sqlx::Error> {
    use sqlx::Row;
    // ⚠ Mapped by hand rather than `query_as`: `EventRow` carries no `FromRow`
    // derive, and adding one to a type the write path also constructs would tie a
    // hot struct to this read.
    let rows = sqlx::query(
        "SELECT event_id, execution_id, catalog_id, event_type, status, \
                created_at AT TIME ZONE 'UTC' AS created_at, prev_event_id, \
                node_id, node_name, node_type, parent_event_id, parent_execution_id, \
                context, result, meta, error, worker_id \
           FROM noetl.event \
          WHERE execution_id = $1 AND event_id = ANY($2) \
          ORDER BY event_id",
    )
    .bind(execution_id)
    .bind(ids)
    .fetch_all(state.pools.pool_for(execution_id))
    .await?;

    Ok(rows
        .into_iter()
        .map(|r| EventRow {
            event_id: r.get("event_id"),
            execution_id: r.get("execution_id"),
            catalog_id: r.get("catalog_id"),
            event_type: r.get("event_type"),
            status: r.get("status"),
            created_at: r.get("created_at"),
            prev_event_id: r.get("prev_event_id"),
            node_id: r.get("node_id"),
            node_name: r.get("node_name"),
            node_type: r.get("node_type"),
            parent_event_id: r.get("parent_event_id"),
            parent_execution_id: r.get("parent_execution_id"),
            context: r.get("context"),
            result: r.get("result"),
            meta: r.get("meta"),
            error: r.get("error"),
            worker_id: r.get("worker_id"),
        })
        .collect())
}

/// `POST /api/ehdb/repair/executions/{execution_id}`
pub async fn repair_execution_endpoint(
    State(state): State<AppState>,
    Path(execution_id): Path<i64>,
) -> impl IntoResponse {
    // Before. Inspect, never Record — see the module note on #264.
    let before = compare_execution(&state, execution_id, ParityRecording::Inspect).await;
    let Some(report) = before.report.as_ref() else {
        return (
            StatusCode::OK,
            Json(json!({
                "action": "ehdb.tier.repair",
                "execution_id": execution_id.to_string(),
                "outcome": "not_comparable",
                "detail": before.detail,
            })),
        );
    };

    let missing = report.missing_event_ids.clone();
    if missing.is_empty() {
        return (
            StatusCode::OK,
            Json(json!({
                "action": "ehdb.tier.repair",
                "execution_id": execution_id.to_string(),
                "outcome": "already_complete",
                "missing_before": 0,
                "repaired": 0,
                "authoritative_expected": report.authoritative_expected,
                "ehdb_before": report.ehdb_count,
            })),
        );
    }

    let rows = match fetch_rows(&state, execution_id, &missing).await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "action": "ehdb.tier.repair",
                    "execution_id": execution_id.to_string(),
                    "outcome": "authoritative_read_failed",
                    "detail": e.to_string(),
                })),
            );
        }
    };

    // ⚠ An id the comparator called absent but that has no authoritative row is
    // UNRECOVERABLE, and is reported rather than subtracted. Silently narrowing
    // the denominator is how "the gap closed" comes to mean "the gap stopped
    // being measured".
    let unrecoverable = unrecoverable_ids(&missing, rows.iter().map(|r| r.event_id));

    // Re-mirror exactly the rows we hold. `mirror_rows` is the same chokepoint the
    // live path uses, so a repaired record is byte-identical to a first-delivery
    // one — a second mirror implementation would be a second thing to keep true.
    crate::handlers::ehdb_eventlog_mirror::mirror_rows(&state, &rows).await;

    // After. Inspect again, for the same reason.
    let after = compare_execution(&state, execution_id, ParityRecording::Inspect).await;
    let (missing_after, ehdb_after) = match after.report.as_ref() {
        Some(r) => (r.missing_event_ids.len(), r.ehdb_count),
        None => (missing.len(), report.ehdb_count),
    };

    (
        StatusCode::OK,
        Json(json!({
            "action": "ehdb.tier.repair",
            "execution_id": execution_id.to_string(),
            "outcome": repair_outcome(missing_after),
            "authoritative_expected": report.authoritative_expected,
            "ehdb_before": report.ehdb_count,
            "ehdb_after": ehdb_after,
            "missing_before": missing.len(),
            "missing_after": missing_after,
            "remirrored": rows.len(),
            "unrecoverable": unrecoverable,
            "unrecoverable_count": unrecoverable.len(),
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The handler source WITHOUT this test module.
    ///
    /// ⚠ `include_str!` on a file pulls in the tests too, so an assertion string
    /// here becomes a match in the thing being asserted about. The first version
    /// of these gates failed for exactly that reason: `ParityRecording::Record`
    /// appears below, in the sentence forbidding it.
    fn src() -> &'static str {
        const RAW: &str = include_str!("ehdb_tier_repair.rs");
        let cut = RAW
            .find("\n#[cfg(test)]")
            .expect("this file must carry exactly one test module, at the end");
        let head = &RAW[..cut];
        assert!(
            head.len() > 2000,
            "extracted an implausibly short handler body ({} bytes) — the cut is \
             wrong and every assertion below would be vacuous",
            head.len()
        );
        head
    }

    /// ⚠⚠ Both comparisons must use `Inspect`, never `Record`.
    ///
    /// noetl/ai-meta#264: the parity endpoint used to write the very counters its
    /// own paging alert read, so investigating a divergence inflated it. A repair
    /// that recorded would do the same — and would additionally corrupt its own
    /// before/after numbers, which are the only evidence the gap closed.
    #[test]
    fn both_comparisons_are_inspect_never_record() {
        // ⚠ Count the CALL form, not raw mentions: the module doc links
        // [`ParityRecording::Inspect`], and counting that made this read 3.
        assert_eq!(
            src()
                .matches("compare_execution(&state, execution_id, ParityRecording::Inspect)")
                .count(),
            2,
            "the before and after comparisons must BOTH be Inspect"
        );
        assert!(
            !src().contains("ParityRecording::Record"),
            "a repair must never record parity outcomes — it would inflate the \
             counters it exists to repair (#264)"
        );
    }

    /// ⚠ The repair is SCOPED: it mirrors only the ids the comparator reported.
    ///
    /// A blind full re-mirror would be correct *only because* the tier dedupe
    /// absorbs it — a guarantee borrowed from another component, on the write path
    /// of a `primary`-serving tier.
    #[test]
    fn the_repair_is_scoped_to_the_reported_absent_ids() {
        assert!(
            src().contains("report.missing_event_ids.clone()"),
            "the repair set must come from the comparator's structured ids"
        );
        assert!(
            src().contains("WHERE execution_id = $1 AND event_id = ANY($2)"),
            "the fetch must be bounded by BOTH the execution and the id list; the \
             id list alone would let a comparator bug on one execution pull another \
             execution's rows into this tier stream"
        );
        assert!(
            !src().contains("fetch_all_events") && !src().contains("SELECT * FROM noetl.event"),
            "no unbounded read of the execution's log"
        );
    }

    /// ⚠ It re-uses the live mirror chokepoint rather than re-implementing one.
    #[test]
    fn it_mirrors_through_the_same_chokepoint_the_live_path_uses() {
        assert!(
            src().contains("ehdb_eventlog_mirror::mirror_rows(&state, &rows)"),
            "a second mirror implementation would be a second thing to keep true, \
             and a repaired record must be byte-identical to a first delivery"
        );
    }

    #[test]
    fn unrecoverable_ids_are_reported_not_subtracted() {
        let missing = vec![1_i64, 2, 3, 4];
        let fetched = vec![1_i64, 3];
        let gone = unrecoverable_ids(&missing, fetched.into_iter());
        assert_eq!(gone, vec![2, 4], "ids with no authoritative row are named");

        assert!(
            unrecoverable_ids(&missing, missing.clone().into_iter()).is_empty(),
            "nothing is unrecoverable when every row came back"
        );
        assert_eq!(
            unrecoverable_ids(&[], std::iter::empty()),
            Vec::<i64>::new()
        );
    }

    /// ⚠ `repaired` is keyed on what is still missing, not on what was delivered.
    #[test]
    fn the_outcome_is_keyed_on_the_after_count() {
        assert_eq!(repair_outcome(0), "repaired");
        assert_eq!(
            repair_outcome(1),
            "partial",
            "one still-absent event means the execution is not repaired, however \
             many records were delivered"
        );
        assert_eq!(repair_outcome(999), "partial");
    }

    /// ⚠ Idempotence is INHERITED, and the comment saying so must stay.
    ///
    /// A re-run re-mirrors the same ids; they land as dedupes because the tier now
    /// carries `event_id`. If that property were ever removed from the tier this
    /// endpoint would start duplicating, so the dependency is stated where the
    /// repair is, not left implicit.
    #[test]
    fn idempotence_is_documented_as_inherited_from_the_tier_dedupe() {
        assert!(
            src().contains("idempotent by") && src().contains("construction, not by bookkeeping"),
            "the repair must state that its idempotence comes from the tier dedupe"
        );
    }
}
