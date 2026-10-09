//! Reading archived event history — [noetl/ai-meta#459](https://github.com/noetl/ai-meta/issues/459) P5.
//!
//! Two routes:
//!
//! * `GET /api/archive/executions/{execution_id}` — one execution's archived events.
//! * `GET /api/archive/executions?date=YYYY-MM-DD` — the executions that STARTED that date.
//!
//! ## ⚠⚠ Scope, stated precisely
//!
//! The server's embedded EHDB store is a **shadow**: verified against `origin/main`, the
//! only readers of `engine()` are the shadow writer itself and the parity verifier, and
//! `GET /api/executions/{id}` serves events from Postgres `noetl.event` — which is
//! append-only and never purged. So pruning the embedded store does **not** make history
//! unreadable through the existing API, and these routes are not a rescue for that.
//!
//! What they are: the only way to read **EHDB-format** history once the hot copy is gone,
//! and the read surface the retention tier owes its callers. The fall-through helper
//! ([`crate::services::event_archive::read_with_fallthrough`]) is what a serving tier
//! would use if the embedded store ever becomes one.

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::error::{AppError, AppResult};
use crate::services::archive_store::ArchiveStore;
use crate::services::event_archive as ea;

/// The archive store plus its config, resolved once at startup.
#[derive(Clone)]
pub struct ArchiveReadDeps {
    pub store: Option<std::sync::Arc<dyn ArchiveStore>>,
    pub configured_bucket: Option<String>,
}

impl ArchiveReadDeps {
    fn store(&self) -> AppResult<&dyn ArchiveStore> {
        match self.store.as_deref() {
            Some(s) => Ok(s),
            // ⚠ 503 with the reason, never an empty 200. An empty success would read
            // exactly like "this execution was never archived", which is a different fact.
            // `OwnerUnavailable` is the variant this repo maps to SERVICE_UNAVAILABLE.
            None => Err(AppError::OwnerUnavailable(format!(
                "the event archive is not configured ({}); set NOETL_EHDB_ARCHIVE_BUCKET",
                match &self.configured_bucket {
                    Some(b) => format!("bucket {b} present but the store failed to open"),
                    None => "no bucket".into(),
                }
            ))),
        }
    }
}

/// `GET /api/archive/executions/{execution_id}`
pub async fn get_execution(
    State(deps): State<ArchiveReadDeps>,
    Path(execution_id): Path<i64>,
) -> AppResult<Json<serde_json::Value>> {
    let store = deps.store()?;
    // No hot source here — this route reads the archive deliberately, so a caller can ask
    // "what does the archive hold" without the answer depending on hot-store state.
    let read = ea::read_with_fallthrough(store, execution_id, || None)
        .await
        .map_err(AppError::Internal)?;

    match &read.source {
        ea::ReadSource::NotFound => Err(AppError::NotFound(format!(
            "execution {execution_id} is not in the archive"
        ))),
        source => Ok(Json(json!({
            "execution_id": execution_id,
            "source": source,
            "records": read.records,
        }))),
    }
}

#[derive(Debug, Deserialize)]
pub struct DateQuery {
    pub date: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Default page size for a by-date listing.
const DEFAULT_DATE_LIMIT: usize = 500;
/// Hard cap, so one request cannot list an unbounded day.
const MAX_DATE_LIMIT: usize = 5_000;

/// `GET /api/archive/executions?date=YYYY-MM-DD`
pub async fn list_by_date(
    State(deps): State<ArchiveReadDeps>,
    Query(q): Query<DateQuery>,
) -> AppResult<Json<serde_json::Value>> {
    // ⚠ Validated, not passed through. A malformed date would silently list a prefix that
    // matches nothing and return a clean empty — a quiet day that never existed.
    if !is_iso_date(&q.date) {
        return Err(AppError::BadRequest(format!(
            "date must be YYYY-MM-DD, got {:?}",
            q.date
        )));
    }
    let store = deps.store()?;
    let limit = q.limit.unwrap_or(DEFAULT_DATE_LIMIT).min(MAX_DATE_LIMIT);
    let listing = ea::list_executions_for_date(store, &q.date, limit)
        .await
        .map_err(AppError::Internal)?;

    Ok(Json(json!({
        "date": listing.date,
        "execution_ids": listing.execution_ids,
        // ⭐ The denominator travels with the answer: keys the prefix returned, keys that
        // were not references, and whether the cap was hit. A short list and a quiet day
        // are different facts.
        "keys_seen": listing.keys_seen,
        "unrecognised": listing.unrecognised,
        "truncated": listing.truncated,
        "limit_applied": limit,
    })))
}

/// Strict `YYYY-MM-DD`.
pub fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b[..4].iter().all(|c| c.is_ascii_digit())
        && b[5..7].iter().all(|c| c.is_ascii_digit())
        && b[8..].iter().all(|c| c.is_ascii_digit())
        && chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
}
