//! Result-tier GC endpoint ([noetl/ai-meta#104](https://github.com/noetl/ai-meta/issues/104)
//! Phase F).
//!
//! `POST /api/internal/result-tier/gc` — sweep the Feather/JSON result tier for
//! provably-dead objects and (unless `dry_run`) reclaim them. Service-account
//! gated like the rest of `/api/internal/*`; the `system/scheduled_cleanup`
//! playbook is the caller (data-access-boundary.md — workers never touch the
//! object store directly).
//!
//! The endpoint is a **no-op unless `NOETL_RESULT_TIER_GC` is set** and defaults
//! to **dry-run** (lists dead candidates, deletes nothing). The safety invariant
//! — never delete a live-referenced object — lives in
//! [`crate::services::result_tier_gc`] and is unit-tested there.

use axum::{extract::State, Json};

use crate::db::DbPool;
use crate::error::AppResult;
use crate::handlers::internal::RequireInternalApiToken;
use crate::services::object_backend::ObjectBackend;
use crate::services::result_tier_gc::{self, GcReport, GcRequest};

/// Injected GC deps: the pool (for the `noetl.event` liveness query) plus the
/// resolved object backend (for list + delete).
#[derive(Clone)]
pub struct ResultTierDeps {
    pub pool: DbPool,
    pub backend: ObjectBackend,
}

/// `POST /api/internal/result-tier/gc` — run one sweep. An empty body sweeps the
/// `noetl/` prefix in dry-run with default limit + grace.
#[tracing::instrument(skip(deps, _token, body))]
pub async fn gc(
    State(deps): State<ResultTierDeps>,
    _token: RequireInternalApiToken,
    body: Option<Json<GcRequest>>,
) -> AppResult<Json<GcReport>> {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    // noetl/ai-meta#199 Slice B — write-behind-cache sink-state source. The gate
    // (`NOETL_RESULT_TIER_GC_SINK_GATE`) retains any object whose execution is
    // still pending-sink. The server-visible feed of pending-sink executions (a
    // worker-reported set, cross-process from the worker's in-process sink gate)
    // is a tracked follow-up; until it lands the source is empty, so the gate is
    // behavior-neutral — it can only ever retain more, never delete more.
    let pending_sink: std::collections::HashSet<i64> = std::collections::HashSet::new();
    let report = result_tier_gc::sweep(&deps.pool, &deps.backend, &req, &pending_sink).await?;

    // Observability: one counter, deltas tell the story (no_op when the gate is
    // off, scanned/deleted/skipped_live/errors otherwise).
    if !report.enabled {
        crate::metrics::record_result_tier_gc("no_op", 1);
    } else {
        crate::metrics::record_result_tier_gc("scanned", report.scanned as u64);
        crate::metrics::record_result_tier_gc("deleted", report.deleted as u64);
        crate::metrics::record_result_tier_gc("skipped_live", report.skipped_live as u64);
        crate::metrics::record_result_tier_gc("skipped_grace", report.skipped_grace as u64);
        crate::metrics::record_result_tier_gc(
            "skipped_unparseable",
            report.skipped_unparseable as u64,
        );
        crate::metrics::record_result_tier_gc("skipped_unsunk", report.skipped_unsunk as u64);
        crate::metrics::record_result_tier_gc("error", report.errors as u64);
    }

    tracing::info!(
        enabled = report.enabled,
        dry_run = report.dry_run,
        prefix = %report.prefix,
        scanned = report.scanned,
        deleted = report.deleted,
        skipped_live = report.skipped_live,
        skipped_grace = report.skipped_grace,
        skipped_unparseable = report.skipped_unparseable,
        errors = report.errors,
        "result-tier GC sweep done (#104 Phase F)"
    );
    Ok(Json(report))
}
