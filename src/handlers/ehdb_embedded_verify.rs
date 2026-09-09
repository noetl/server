//! `verify` read path for the embedded EHDB engine (noetl/ai-meta#332).
//!
//! ⚠⚠ Why this exists at all. The embedded shadow compares `appended` against
//! `rows.len()` — *"did the engine accept the write set"*. It never reads
//! anything back. So `diverged=0` on the shadow says **nothing** about whether
//! the engine can answer a query, which is the entire content of the serve flip.
//! Treating write agreement as read readiness is the noetl/ai-meta#325 mistake,
//! where a comparator that deserialised five fields and compared three reported
//! `match` on executions that differed, for weeks.
//!
//! This endpoint reads one execution's events from **both** stores and compares
//! them. It serves nothing and changes nothing: Postgres remains authoritative
//! throughout, so a wrong engine answer costs a metric increment rather than a
//! wrong result to a caller.
//!
//! Every response carries the comparator's **positive controls**. A report whose
//! controls did not all pass is refused rather than returned, because a green
//! result from a comparator that cannot fire is worse than no result.

use crate::error::AppResult;
use crate::handlers::ehdb_embedded::{
    comparator_controls, compare_reads, read_embedded, ComparableEvent, ReadVerdict,
};
use crate::state::AppState;
use axum::{extract::Path, extract::State, Json};
use serde_json::json;

/// Read the authoritative event set for one execution.
async fn read_authoritative(
    state: &AppState,
    execution_id: i64,
) -> AppResult<Vec<ComparableEvent>> {
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT event_id, event_type FROM noetl.event WHERE execution_id = $1 ORDER BY event_id",
    )
    .bind(execution_id)
    .fetch_all(state.pools.pool_for(execution_id))
    .await?;
    Ok(rows
        .into_iter()
        .map(|(event_id, event_type)| ComparableEvent {
            event_id,
            event_type,
        })
        .collect())
}

/// `GET /api/ehdb/embedded/verify/{execution_id}`
pub async fn verify_execution(
    State(state): State<AppState>,
    Path(execution_id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let controls = comparator_controls();
    let controls_json: Vec<_> = controls
        .iter()
        .map(|(name, passed, detail)| json!({"control": name, "passed": passed, "detail": detail}))
        .collect();

    // ⚠ Refuse the whole report if any control failed. A comparator that cannot
    // detect a planted divergence makes every other number in this body a lie.
    if let Some((name, _, _)) = controls.iter().find(|(_, passed, _)| !passed) {
        return Ok(Json(json!({
            "action": "ehdb.embedded.verify",
            "outcome": "controls_failed",
            "failed_control": name,
            "controls": controls_json,
            "note": "report refused: the comparator did not detect a planted divergence, \
                     so a clean verdict here would be meaningless",
        })));
    }

    let eid: i64 = execution_id.parse().map_err(|_| {
        crate::error::AppError::BadRequest(format!("execution_id not an i64: {execution_id}"))
    })?;

    let Some(embedded) = read_embedded(&execution_id) else {
        return Ok(Json(json!({
            "action": "ehdb.embedded.verify",
            "outcome": "engine_unavailable",
            "controls": controls_json,
            "note": "the embedded engine is not open (flag off, or the open failed)",
        })));
    };
    let authoritative = read_authoritative(&state, eid).await?;
    let verdict = compare_reads(&embedded, &authoritative);
    crate::metrics::record_embedded_read(verdict.label());

    let detail = match &verdict {
        ReadVerdict::Agreed { events } => json!({"events_compared": events}),
        ReadVerdict::OutOfCoverage => json!({
            "note": "the engine holds nothing for this execution -- it predates the \
                     engine's current volume. Out of coverage, NOT a divergence and NOT agreement.",
            "authoritative_events": authoritative.len(),
        }),
        ReadVerdict::Diverged {
            embedded_only,
            authoritative_only,
            type_mismatches,
        } => json!({
            "embedded_only": embedded_only,
            "authoritative_only": authoritative_only,
            "type_mismatches": type_mismatches,
            "embedded_count": embedded.len(),
            "authoritative_count": authoritative.len(),
        }),
    };

    Ok(Json(json!({
        "action": "ehdb.embedded.verify",
        "execution_id": execution_id,
        "outcome": verdict.label(),
        "detail": detail,
        "controls": controls_json,
    })))
}
