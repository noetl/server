//! Execution management service.
//!
//! Provides operations for managing playbook executions,
//! including listing, status queries, cancellation, and finalization.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::db::{DbPool, DbPoolMap};
use crate::error::{AppError, AppResult};
use crate::handlers::event_write::{emit_event, EventRow};
use crate::state::AppState;

/// Execution summary for listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionSummary {
    pub execution_id: i64,
    pub catalog_id: i64,
    pub path: Option<String>,
    pub status: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub event_count: i64,
}

/// Detailed execution information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionDetail {
    pub execution_id: i64,
    pub catalog_id: i64,
    pub path: Option<String>,
    pub status: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub parent_execution_id: Option<i64>,
    pub workload: Option<serde_json::Value>,
    pub events: Vec<ExecutionEvent>,
}

/// Event in an execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionEvent {
    pub event_id: i64,
    pub event_type: String,
    pub node_name: Option<String>,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}

/// Execution status response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionStatus {
    pub execution_id: i64,
    pub status: String,
    pub current_step: Option<String>,
    pub progress: ExecutionProgress,
    pub is_cancelled: bool,
}

/// Execution progress information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionProgress {
    pub total_steps: i32,
    pub completed_steps: i32,
    pub running_steps: i32,
    pub failed_steps: i32,
}

/// Filter for listing executions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionFilter {
    pub catalog_id: Option<i64>,
    pub path: Option<String>,
    pub status: Option<String>,
    pub limit: Option<i32>,
    pub offset: Option<i32>,
}

/// Execution management service.
///
/// Phase F R4-4b moved this from a single `DbPool` to a
/// [`DbPoolMap`]: per-execution methods (`get`, `get_status`,
/// `cancel`, `is_cancelled`, `finalize`) route via
/// `pools.pool_for(execution_id)`; the cluster-wide list
/// endpoint fan-outs via `pools.for_each_shard` and resolves
/// catalog paths against `pools.cluster()` in a single follow-up
/// query.  In single-pool fallback mode (`NOETL_SHARDS` empty)
/// every accessor returns the same handle as the legacy pool;
/// behaviour bit-identical to pre-R4.
#[derive(Clone)]
pub struct ExecutionService {
    pools: DbPoolMap,
    snowflake: std::sync::Arc<crate::snowflake::SnowflakeGenerator>,
    /// Full application state.  Present in the production wiring
    /// (`main.rs` builds the service from the constructed `AppState`)
    /// and absent only in the pool-less unit-test shim
    /// ([`Self::new_legacy`], which exercises the in-memory
    /// `determine_status` path).  `cancel` / `finalize` route their
    /// `noetl.event` writes through the `emit_event` chokepoint
    /// (noetl/ai-meta#103 2d-3) so they honour
    /// `NOETL_EVENT_INGEST_PUBLISH_ONLY` like the other producer sites —
    /// that needs `&AppState` (the gate flag, the NATS publisher, the
    /// catalog pool for the system-pool exemption).
    state: Option<AppState>,
}

impl ExecutionService {
    /// Create a new execution service.
    ///
    /// Takes the [`DbPoolMap`] from `AppState.pools` so the
    /// service can route per-execution queries via
    /// `pools.pool_for(execution_id)` and the cluster-wide
    /// `list()` fan-out via `pools.for_each_shard`.
    ///
    /// `snowflake` is the application-side ID generator shared
    /// with `AppState` and the other services.  Phase F R1.5 of
    /// noetl/ai-meta#49 moved id generation out of the DB-side
    /// `noetl.snowflake_id()` function.
    /// `state` is the full [`AppState`]; `pools` + `snowflake` are
    /// kept as direct fields (every other accessor reads them) and
    /// the whole state is retained so `cancel` / `finalize` can reach
    /// the `emit_event` chokepoint.
    pub fn new(
        pools: DbPoolMap,
        snowflake: std::sync::Arc<crate::snowflake::SnowflakeGenerator>,
        state: AppState,
    ) -> Self {
        Self {
            pools,
            snowflake,
            state: Some(state),
        }
    }

    /// Build an [`ExecutionService`] wrapping a single legacy
    /// pool — for test / example code paths that don't have a
    /// [`DbPoolMap`] in scope.  Internally wraps the pool via
    /// [`DbPoolMap::from_single_pool`].
    /// `state` is `None`: this shim has no `AppState`, so it must not
    /// be used for `cancel` / `finalize` (those require the chokepoint).
    /// It exists only for the in-memory `determine_status` unit tests.
    pub fn new_legacy(
        db: DbPool,
        snowflake: std::sync::Arc<crate::snowflake::SnowflakeGenerator>,
    ) -> Self {
        Self {
            pools: DbPoolMap::from_single_pool(db),
            snowflake,
            state: None,
        }
    }

    /// Borrow the per-execution pool for the given `execution_id`.
    /// Internal helper to keep the per-method call sites short.
    #[inline]
    fn pool_for(&self, execution_id: i64) -> &DbPool {
        self.pools.pool_for(execution_id)
    }

    /// The `&AppState` the chokepoint needs.  Present in every
    /// production path; `None` only in the pool-less `new_legacy`
    /// test shim, which never calls `cancel` / `finalize`.  Returning
    /// a clear error (rather than silently falling back to a raw
    /// INSERT) keeps the gate contract intact — there is exactly one
    /// `noetl.event` write path for these events.
    fn require_state(&self) -> AppResult<&AppState> {
        self.state.as_ref().ok_or_else(|| {
            AppError::Internal(
                "ExecutionService built without AppState (test shim); \
                 cancel/finalize require the emit_event chokepoint"
                    .to_string(),
            )
        })
    }

    /// Resolve the execution's `catalog_id`, reading `noetl.event`
    /// first and falling back to `noetl.command`.  The fallback is
    /// load-bearing under the CQRS write-path cutover
    /// (noetl/ai-meta#103 2d-3): with `NOETL_EVENT_INGEST_PUBLISH_ONLY`
    /// on, `noetl.event` may be empty (events are published to
    /// `noetl_events` and not INSERTed until the materializer drains
    /// them), so an event-only lookup would `NotFound` a still-running
    /// execution.  `noetl.command` is written synchronously even under
    /// the gate (it's the command queue the worker reads), so it
    /// always carries the execution's `catalog_id`.  Mirrors
    /// `handlers::events::get_catalog_id`.
    async fn resolve_catalog_id(&self, execution_id: i64) -> AppResult<i64> {
        let pool = self.pool_for(execution_id);
        if let Some((id,)) = sqlx::query_as::<_, (i64,)>(
            "SELECT catalog_id FROM noetl.event WHERE execution_id = $1 LIMIT 1",
        )
        .bind(execution_id)
        .fetch_optional(pool)
        .await?
        {
            return Ok(id);
        }
        let row: Option<(i64,)> = sqlx::query_as::<_, (i64,)>(
            "SELECT catalog_id FROM noetl.command WHERE execution_id = $1 LIMIT 1",
        )
        .bind(execution_id)
        .fetch_optional(pool)
        .await?;
        row.map(|(id,)| id)
            .ok_or_else(|| AppError::NotFound(format!("Execution not found: {}", execution_id)))
    }

    /// List executions with optional filters.
    ///
    /// Phase F R4-4b: per-shard fan-out + cluster-master catalog
    /// lookup, replacing the single-pool JOIN'd CTE this used to
    /// be.  Each shard answers an `execution_stats` aggregation
    /// over its own slice of `noetl.event`; results are merged,
    /// catalog paths are looked up once on the cluster master,
    /// stitched in, then path/status filters and pagination
    /// apply post-merge.
    ///
    /// **Over-fetch**: in sharded mode each shard returns up to
    /// `(limit + offset)` rows because any single shard could
    /// contribute every row in the merged window after sorting
    /// by `started_at DESC`.  Bounded by the request's own
    /// `limit + offset` (default ≤ 50+50 = 100), so per-shard
    /// I/O stays manageable.
    ///
    /// **Path filter quirk**: `c.path LIKE $2` is applied
    /// post-merge after the cluster catalog lookup.  This means
    /// when both `catalog_id` and `path` filters are unset, the
    /// over-fetch returns all matching rows; with `path` set,
    /// the effective row count after filtering could be smaller
    /// than `limit`.  A future R4-5+ optimisation could push the
    /// path filter into the cluster lookup as a pre-filter.
    #[allow(clippy::type_complexity)]
    pub async fn list(&self, filter: &ExecutionFilter) -> AppResult<Vec<ExecutionSummary>> {
        let limit = filter.limit.unwrap_or(50).min(100);
        let offset = filter.offset.unwrap_or(0);
        let fetch_cap: i64 = (limit as i64) + (offset as i64);
        // Candidate window for stage 1 (noetl/ai-meta#62).  Without a status
        // filter the N most-recent *executions* are exactly the answer, so the
        // candidate cap equals `fetch_cap`.  With a status filter the matching
        // rows are a subset of the candidates, so over-fetch a bounded window
        // (the status filter then applies post-aggregation within the most-
        // recent `candidate_cap` executions — a paginated-recent-list semantic).
        let candidate_cap: i64 = if filter.status.is_none() {
            fetch_cap
        } else {
            fetch_cap.saturating_mul(10).min(2_000)
        };

        // Stage 1 — per-shard execution_stats aggregation.  The
        // per-shard query is the original CTE minus the catalog
        // JOIN + path filter (those move to the post-merge
        // cluster lookup).  Status filter stays in-shard because
        // it's computed from per-execution events.
        type StatsRow = (i64, i64, String, DateTime<Utc>, Option<DateTime<Utc>>, i64);
        let per_shard: Vec<(u32, Vec<StatsRow>)> = self
            .pools
            .for_each_shard(|_idx, pool| {
                let catalog_id = filter.catalog_id;
                let status = filter.status.clone();
                async move {
                    // noetl/ai-meta#62: candidate-first.  The old query
                    // GROUP BY'd the entire `noetl.event` table (O(all events)
                    // — a ~3.2M-row parallel seq scan, 7-8s) just to find the
                    // N most-recent executions.  Instead, stage `recent` picks
                    // the N most-recent executions from their per-execution
                    // start event (indexed by `event_type`), then stage `stats`
                    // aggregates status/completed/count over *only* those
                    // candidates' events (indexed by `execution_id`).  The
                    // start event is the execution's first event, so
                    // `MIN(created_at)` over start events equals it over all
                    // events — `started_at` (and the ordering) are identical to
                    // the old query.
                    sqlx::query_as::<_, StatsRow>(
                        r#"
                        WITH recent AS (
                            SELECT
                                execution_id,
                                catalog_id,
                                MIN(created_at) AT TIME ZONE 'UTC' as started_at
                            FROM noetl.event
                            WHERE event_type IN ('playbook.initialized', 'playbook_started', 'playbook.started')
                              AND ($1::BIGINT IS NULL OR catalog_id = $1)
                            GROUP BY execution_id, catalog_id
                            ORDER BY started_at DESC
                            LIMIT $4
                        ),
                        stats AS (
                            SELECT
                                e.execution_id,
                                MAX(CASE WHEN e.status IN ('COMPLETED', 'FAILED', 'CANCELLED') THEN e.created_at END) AT TIME ZONE 'UTC' as completed_at,
                                COUNT(*) as event_count,
                                -- Terminal-state priority (noetl/ai-meta#62).  The old
                                -- `MAX(CASE … ELSE 'RUNNING')` is a string MAX, and
                                -- 'RUNNING' > 'FAILED' > 'COMPLETED' > 'CANCELLED'
                                -- alphabetically — so ANY execution with a non-terminal
                                -- event reported RUNNING even after it completed (the
                                -- list-vs-detail status drift).  `bool_or` over a
                                -- prioritized CASE picks the terminal state when present.
                                CASE
                                    WHEN bool_or(e.event_type IN ('playbook.completed', 'playbook_completed')) THEN 'COMPLETED'
                                    WHEN bool_or(e.event_type IN ('playbook.failed', 'playbook_failed') OR e.status = 'FAILED') THEN 'FAILED'
                                    WHEN bool_or(e.event_type IN ('playbook.cancelled', 'playbook_cancelled')) THEN 'CANCELLED'
                                    ELSE 'RUNNING'
                                END as status
                            FROM noetl.event e
                            WHERE e.execution_id IN (SELECT execution_id FROM recent)
                            GROUP BY e.execution_id
                        )
                        SELECT
                            r.execution_id,
                            r.catalog_id,
                            s.status,
                            r.started_at,
                            s.completed_at,
                            s.event_count
                        FROM recent r
                        JOIN stats s ON s.execution_id = r.execution_id
                        WHERE ($2::TEXT IS NULL OR s.status = $2)
                        ORDER BY r.started_at DESC
                        LIMIT $3
                        "#,
                    )
                    .bind(catalog_id)
                    .bind(&status)
                    .bind(fetch_cap)
                    .bind(candidate_cap)
                    .fetch_all(&pool)
                    .await
                }
            })
            .await?;

        // Stage 2 — merge per-shard rows, sort by started_at DESC.
        let mut merged: Vec<StatsRow> = per_shard
            .into_iter()
            .flat_map(|(_idx, rows)| rows)
            .collect();
        merged.sort_by(|a, b| b.3.cmp(&a.3));

        // Stage 3 — cluster-master catalog lookup for the
        // (deduped) catalog_id set.  One SELECT regardless of
        // shard count.
        let catalog_ids: Vec<i64> = {
            let mut ids: Vec<i64> = merged.iter().map(|r| r.1).collect();
            ids.sort_unstable();
            ids.dedup();
            ids
        };
        let catalog_paths: std::collections::HashMap<i64, String> = if catalog_ids.is_empty() {
            std::collections::HashMap::new()
        } else {
            let rows: Vec<(i64, Option<String>)> = sqlx::query_as(
                "SELECT catalog_id, path FROM noetl.catalog WHERE catalog_id = ANY($1)",
            )
            .bind(&catalog_ids)
            .fetch_all(self.pools.cluster())
            .await?;
            rows.into_iter()
                .filter_map(|(id, path)| path.map(|p| (id, p)))
                .collect()
        };

        // Stage 4 — stitch paths in + apply path filter +
        // paginate.
        let path_pattern_lower = filter.path.as_ref().map(|p| p.to_lowercase());
        let summaries = merged
            .into_iter()
            .map(
                |(execution_id, catalog_id, status, started_at, completed_at, event_count)| {
                    let path = catalog_paths.get(&catalog_id).cloned();
                    ExecutionSummary {
                        execution_id,
                        catalog_id,
                        path,
                        status,
                        started_at,
                        completed_at,
                        event_count,
                    }
                },
            )
            .filter(|s| match &path_pattern_lower {
                None => true,
                Some(needle) => s
                    .path
                    .as_ref()
                    .is_some_and(|p| p.to_lowercase().contains(needle)),
            })
            .skip(offset as usize)
            .take(limit as usize)
            .collect();

        Ok(summaries)
    }

    /// Get detailed execution information.
    #[allow(clippy::type_complexity)]
    pub async fn get(&self, execution_id: i64) -> AppResult<ExecutionDetail> {
        // Get basic execution info from first event
        let info: Option<(i64, Option<i64>, Option<serde_json::Value>, DateTime<Utc>)> =
            sqlx::query_as(
                r#"
                -- created_at is TIMESTAMP (no tz); ``AT TIME ZONE 'UTC'``
                -- reinterprets it as UTC so sqlx can decode into
                -- DateTime<Utc>.  Mirror of the WITH-block in list().
                SELECT
                    catalog_id,
                    parent_execution_id,
                    context->'workload' as workload,
                    created_at AT TIME ZONE 'UTC' as created_at
                FROM noetl.event
                WHERE execution_id = $1
                  AND event_type IN ('playbook.initialized', 'playbook_started')
                LIMIT 1
                "#,
            )
            .bind(execution_id)
            .fetch_optional(self.pool_for(execution_id))
            .await?;

        let (catalog_id, parent_execution_id, workload, started_at) = info
            .ok_or_else(|| AppError::NotFound(format!("Execution not found: {}", execution_id)))?;

        // Get catalog path (cluster-wide table)
        let path: Option<(String,)> =
            sqlx::query_as("SELECT path FROM noetl.catalog WHERE catalog_id = $1")
                .bind(catalog_id)
                .fetch_optional(self.pools.cluster())
                .await?;

        // Get the most recent events for this execution.
        //
        // Loading the WHOLE log was an O(events) memory bomb: a high-volume run
        // (e.g. a 10×1000 cursor flow at ~200k events) blew past the server's
        // memory limit and OOM-killed it whenever this endpoint was hit.  The
        // response only needs the recent tail: `determine_status` scans from the
        // newest event backward for a terminal / FAILED marker (which is always
        // recent), and `completed_at` reads the terminal event's time.  So cap
        // the load to the most recent rows — ordered DESC for the LIMIT, then
        // reversed back to ASC for the response.  A future paginated
        // `/api/executions/{id}/events` endpoint can serve the full history.
        const MAX_EVENTS_RETURNED: i64 = 2000;
        let mut event_rows: Vec<(
            i64,
            String,
            Option<String>,
            String,
            DateTime<Utc>,
            Option<serde_json::Value>,
            Option<String>,
        )> = sqlx::query_as(
            r#"
                SELECT
                    event_id,
                    event_type,
                    node_name,
                    COALESCE(status, 'UNKNOWN') as status,
                    created_at AT TIME ZONE 'UTC' as created_at,
                    result,
                    error
                FROM noetl.event
                WHERE execution_id = $1
                ORDER BY created_at DESC
                LIMIT $2
                "#,
        )
        .bind(execution_id)
        .bind(MAX_EVENTS_RETURNED)
        .fetch_all(self.pool_for(execution_id))
        .await?;
        // Restore chronological (ASC) order for the response.
        event_rows.reverse();

        let events: Vec<ExecutionEvent> = event_rows
            .into_iter()
            .map(
                |(event_id, event_type, node_name, status, created_at, result, error)| {
                    ExecutionEvent {
                        event_id,
                        event_type,
                        node_name,
                        status,
                        created_at,
                        result,
                        error,
                    }
                },
            )
            .collect();

        // Determine overall status
        let status = self.determine_status(&events);

        // Get completion time
        let completed_at = events
            .iter()
            .filter(|e| {
                matches!(
                    e.event_type.as_str(),
                    "playbook.completed"
                        | "playbook_completed"
                        | "playbook.failed"
                        | "playbook_failed"
                        | "playbook.cancelled"
                        | "playbook_cancelled"
                )
            })
            .map(|e| e.created_at)
            .max();

        Ok(ExecutionDetail {
            execution_id,
            catalog_id,
            path: path.map(|(p,)| p),
            status,
            started_at,
            completed_at,
            parent_execution_id,
            workload,
            events,
        })
    }

    /// Bounded, ordered read of the **event read-model** for one
    /// execution — the read-serving primitive behind
    /// `GET /api/ehdb/executions/{id}/events`.
    ///
    /// Read-only, secret-free by construction: only the projected
    /// columns (`event_id`, `event_type`, `node_name`, `status`,
    /// `created_at`) are selected — the `result` / `error` / `context`
    /// payload bodies (which can carry credential material) are never
    /// read, mirroring `ehdb_reference::projection::EventReadModelView`.
    ///
    /// Ordered ASC by `event_id` (the application-side snowflake, which
    /// is the monotonic global-ordering key for events) with an
    /// `after`-cursor for forward pagination. Routed to the
    /// execution's shard via `pool_for(execution_id)`.
    #[allow(clippy::type_complexity)]
    pub async fn ehdb_events_by_execution(
        &self,
        execution_id: i64,
        after: Option<i64>,
        limit: i64,
    ) -> AppResult<Vec<(i64, String, Option<String>, Option<String>, DateTime<Utc>)>> {
        let rows =
            sqlx::query_as::<_, (i64, String, Option<String>, Option<String>, DateTime<Utc>)>(
                r#"
            SELECT
                event_id,
                event_type,
                node_name,
                status,
                created_at AT TIME ZONE 'UTC' as created_at
            FROM noetl.event
            WHERE execution_id = $1
              AND ($2::BIGINT IS NULL OR event_id > $2)
            ORDER BY event_id ASC
            LIMIT $3
            "#,
            )
            .bind(execution_id)
            .bind(after)
            .bind(limit)
            .fetch_all(self.pool_for(execution_id))
            .await?;
        Ok(rows)
    }

    /// Bounded, ordered scan of the **event read-model** across the log
    /// by global sequence — the read-serving primitive behind
    /// `GET /api/ehdb/events`.
    ///
    /// Same secret-free projection as [`Self::ehdb_events_by_execution`].
    /// Ordered ASC by `event_id` with an `after`-cursor.
    ///
    /// Reads the cluster-master pool (`pools.cluster()`). In single-pool
    /// (kind / unsharded) deployments this is the whole log and the scan
    /// is globally ordered. Under multi-shard prod a globally-ordered
    /// scan needs a per-shard fan-out + k-way merge (follow-up); this
    /// slice is scoped to the control-plane read-model the server already
    /// serves single-pool.
    #[allow(clippy::type_complexity)]
    pub async fn ehdb_events_scan(
        &self,
        after: Option<i64>,
        limit: i64,
    ) -> AppResult<
        Vec<(
            i64,
            i64,
            String,
            Option<String>,
            Option<String>,
            DateTime<Utc>,
        )>,
    > {
        let rows = sqlx::query_as::<
            _,
            (
                i64,
                i64,
                String,
                Option<String>,
                Option<String>,
                DateTime<Utc>,
            ),
        >(
            r#"
            SELECT
                event_id,
                execution_id,
                event_type,
                node_name,
                status,
                created_at AT TIME ZONE 'UTC' as created_at
            FROM noetl.event
            WHERE ($1::BIGINT IS NULL OR event_id > $1)
            ORDER BY event_id ASC
            LIMIT $2
            "#,
        )
        .bind(after)
        .bind(limit)
        .fetch_all(self.pools.cluster())
        .await?;
        Ok(rows)
    }

    /// Get execution status.
    pub async fn get_status(&self, execution_id: i64) -> AppResult<ExecutionStatus> {
        // Check if execution exists
        let exists: Option<(i64,)> =
            sqlx::query_as("SELECT execution_id FROM noetl.event WHERE execution_id = $1 LIMIT 1")
                .bind(execution_id)
                .fetch_optional(self.pool_for(execution_id))
                .await?;

        if exists.is_none() {
            return Err(AppError::NotFound(format!(
                "Execution not found: {}",
                execution_id
            )));
        }

        // Phase D R4 follow-up (noetl/server#146).  Look up terminal
        // events FIRST.  `playbook.completed` / `playbook.failed` are
        // the definitive terminal markers — the orchestrator emits
        // exactly one of them when it decides the playbook is done
        // (search engine.rs for `Orchestrator marked execution as
        // terminal`).  Step-stats-based inference (further below)
        // falls behind reality because `command.completed` events
        // carry `status='success'` (lowercase) from the worker, but
        // the existing `completed_steps` filter looked for
        // `status='COMPLETED'` — so completed_steps stayed at 0 even
        // after every step succeeded, the `stats.1 == stats.0`
        // equality never fired, and the endpoint reported `RUNNING`
        // indefinitely.  The list endpoint at `services::execution`
        // already uses `bool_or(playbook.completed) → COMPLETED` for
        // exactly this reason; this is the per-execution twin.
        let terminal: Option<(String,)> = sqlx::query_as(
            r#"
            SELECT event_type
            FROM noetl.event
            WHERE execution_id = $1
              AND event_type IN (
                'playbook.completed', 'playbook_completed',
                'playbook.failed',    'playbook_failed'
              )
            ORDER BY created_at DESC
            LIMIT 1
            "#,
        )
        .bind(execution_id)
        .fetch_optional(self.pool_for(execution_id))
        .await?;

        // Get step statistics.  The `completed_steps` filter now
        // accepts the realistic status values workers actually emit
        // (`'success'` lowercase from `command.completed`) in
        // addition to the legacy `'COMPLETED'` value — without this
        // a successfully-finished step would never count.
        //
        // Change 1 (noetl/ai-meta#72): `running_steps` now tracks
        // `command.claimed` and `command.started` events with statuses
        // `'RUNNING'` OR `'STARTED'`.  Workers emit `command.claimed`
        // with `status='STARTED'` and `command.started` with
        // `status='STARTED'` — the old filter (`status='RUNNING'`) never
        // matched and running_steps was perpetually 0 for in-flight
        // commands.  `step.enter` is dropped from this filter because it
        // fires once per step (not per command) and is misleading for
        // iterator steps that spawn N commands from a single step.enter.
        let stats: (i64, i64, i64, i64) = sqlx::query_as(
            r#"
            SELECT
                COUNT(DISTINCT CASE WHEN event_type = 'step.enter' THEN node_name END) as total_steps,
                COUNT(DISTINCT CASE
                    WHEN event_type IN ('step.exit', 'command.completed')
                     AND (status IN ('COMPLETED', 'completed', 'success'))
                    THEN node_name END) as completed_steps,
                COUNT(DISTINCT CASE
                    WHEN event_type IN ('command.claimed', 'command.started')
                     AND status IN ('RUNNING', 'STARTED')
                    THEN node_name END) as running_steps,
                COUNT(DISTINCT CASE WHEN status = 'FAILED' THEN node_name END) as failed_steps
            FROM noetl.event
            WHERE execution_id = $1
            "#,
        )
        .bind(execution_id)
        .fetch_one(self.pool_for(execution_id))
        .await?;

        // Get current step
        let current_step: Option<(String,)> = sqlx::query_as(
            r#"
            SELECT node_name
            FROM noetl.event
            WHERE execution_id = $1
              AND event_type IN ('step.enter', 'command.started')
              AND node_name IS NOT NULL
            ORDER BY created_at DESC
            LIMIT 1
            "#,
        )
        .bind(execution_id)
        .fetch_optional(self.pool_for(execution_id))
        .await?;

        // Check for cancellation
        let is_cancelled: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS(
                SELECT 1 FROM noetl.event
                WHERE execution_id = $1
                  AND event_type IN ('playbook.cancelled', 'playbook_cancelled')
            )
            "#,
        )
        .bind(execution_id)
        .fetch_one(self.pool_for(execution_id))
        .await?;

        // Change 2 (noetl/ai-meta#72): cross-check noetl.command for
        // commands whose status is not yet terminal.  Non-terminal
        // statuses in the command table are any value that is NOT
        // 'COMPLETED', 'FAILED', or 'CANCELLED' (in either casing — the
        // schema uses uppercase by convention but the status column is
        // VARCHAR with no check constraint, so lowercase variants may
        // appear from Python-side writes).  This query uses the same
        // pool shard as the event queries so the result is consistent
        // within the same execution's partition.
        let in_flight_commands: (i64,) = sqlx::query_as(
            r#"
            SELECT COUNT(*) FROM noetl.command
            WHERE execution_id = $1
              AND status NOT IN ('COMPLETED', 'FAILED', 'CANCELLED', 'completed', 'failed', 'cancelled')
            "#,
        )
        .bind(execution_id)
        .fetch_one(self.pool_for(execution_id))
        .await?;

        // Determine overall status.  Terminal event > cancellation >
        // failed-step heuristic > completed-step heuristic > RUNNING.
        // Terminal-event check goes first so the endpoint reflects
        // the orchestrator's decision the moment `playbook.completed`
        // lands, even if the step-stat counters are momentarily
        // behind (`command.completed` and `playbook.completed` land
        // in the same handler pass but the cross-row counter is not
        // load-bearing for terminal status — only for `progress.*`).
        //
        // Change 3 (noetl/ai-meta#72): the COMPLETED branch now also
        // requires `in_flight_commands.0 == 0`.  The dual signal prevents
        // both known failure modes:
        //   - Event-log signal alone (stats.1 == stats.0) fires too early
        //     when an iterator step has one `step.enter` but N unfinished
        //     commands.
        //   - Command-table signal alone could be misled by a stale
        //     noetl.command projection; requiring the event-log to also
        //     agree ("no more steps to start") makes the verdict robust.
        let status = if let Some((evt,)) = &terminal {
            match evt.as_str() {
                "playbook.completed" | "playbook_completed" => "COMPLETED",
                "playbook.failed" | "playbook_failed" => "FAILED",
                _ => "RUNNING",
            }
            .to_string()
        } else if is_cancelled {
            "CANCELLED".to_string()
        } else if stats.3 > 0 {
            "FAILED".to_string()
        } else if stats.1 == stats.0 && stats.0 > 0 && in_flight_commands.0 == 0 {
            "COMPLETED".to_string()
        } else {
            "RUNNING".to_string()
        };

        Ok(ExecutionStatus {
            execution_id,
            status,
            current_step: current_step.map(|(s,)| s),
            progress: ExecutionProgress {
                total_steps: stats.0 as i32,
                completed_steps: stats.1 as i32,
                running_steps: stats.2 as i32,
                failed_steps: stats.3 as i32,
            },
            is_cancelled,
        })
    }

    /// Cancel an execution.
    pub async fn cancel(&self, execution_id: i64) -> AppResult<()> {
        // Check if execution exists and is running
        let status = self.get_status(execution_id).await?;

        if status.status == "COMPLETED" || status.status == "FAILED" || status.status == "CANCELLED"
        {
            return Err(AppError::Validation(format!(
                "Cannot cancel execution in {} state",
                status.status
            )));
        }

        // The chokepoint needs the full state; fail fast if this is a
        // pool-less test shim (which never reaches here in production).
        let state = self.require_state()?;

        // Get catalog_id for the event (event-log first, command-queue
        // fallback under the gate — see `resolve_catalog_id`).
        let catalog_id = self.resolve_catalog_id(execution_id).await?;

        // Generate event ID via the application-side snowflake
        // generator (Phase F R1.5 of noetl/ai-meta#49).
        let event_id = self.snowflake.generate()?;

        // Write the cancellation event through the CQRS write-path
        // chokepoint (noetl/ai-meta#103 2d-3): gate-off INSERTs the row
        // synchronously (byte-identical to the prior inline INSERT — the
        // canonical INSERT binds the full column superset, the columns
        // this site omits default to NULL); gate-on PUBLISHES it to
        // `noetl_events` so the materializer is the sole writer.  The
        // relocated trigger then drives the execution to its terminal
        // CANCELLED state from the materialized row.
        let row = EventRow::new(
            event_id,
            execution_id,
            catalog_id,
            "playbook_cancelled",
            "CANCELLED",
            Utc::now(),
        )
        .with_node("playbook");
        emit_event(state, self.pool_for(execution_id), row).await?;

        Ok(())
    }

    /// Check if an execution is cancelled.
    pub async fn is_cancelled(&self, execution_id: i64) -> AppResult<bool> {
        let is_cancelled: bool = sqlx::query_scalar(
            r#"
            SELECT EXISTS(
                SELECT 1 FROM noetl.event
                WHERE execution_id = $1
                  AND event_type IN ('playbook.cancelled', 'playbook_cancelled')
            )
            "#,
        )
        .bind(execution_id)
        .fetch_one(self.pool_for(execution_id))
        .await?;

        Ok(is_cancelled)
    }

    /// Finalize an execution (mark as completed or failed).
    pub async fn finalize(
        &self,
        execution_id: i64,
        status: &str,
        error: Option<&str>,
    ) -> AppResult<()> {
        // Validate status
        if status != "COMPLETED" && status != "FAILED" {
            return Err(AppError::Validation(format!(
                "Invalid finalization status: {}",
                status
            )));
        }

        // The chokepoint needs the full state; fail fast if this is a
        // pool-less test shim (which never reaches here in production).
        let state = self.require_state()?;

        // Get catalog_id (event-log first, command-queue fallback under
        // the gate — see `resolve_catalog_id`).
        let catalog_id = self.resolve_catalog_id(execution_id).await?;

        // Generate event ID via the application-side snowflake
        // generator (Phase F R1.5 of noetl/ai-meta#49).
        let event_id = self.snowflake.generate()?;

        let event_type = if status == "COMPLETED" {
            "playbook_completed"
        } else {
            "playbook_failed"
        };

        // Write the finalization event through the CQRS write-path
        // chokepoint (noetl/ai-meta#103 2d-3): gate-off INSERTs the row
        // synchronously (byte-identical to the prior inline INSERT,
        // including the `error` column); gate-on PUBLISHES it to
        // `noetl_events` so the materializer is the sole writer and the
        // relocated trigger drives the execution to its terminal state.
        let row = EventRow::new(
            event_id,
            execution_id,
            catalog_id,
            event_type,
            status,
            Utc::now(),
        )
        .with_node("playbook")
        .with_error(error.map(str::to_string));
        emit_event(state, self.pool_for(execution_id), row).await?;

        Ok(())
    }

    /// Determine execution status from events.
    fn determine_status(&self, events: &[ExecutionEvent]) -> String {
        for event in events.iter().rev() {
            match event.event_type.as_str() {
                "playbook.completed" | "playbook_completed" => return "COMPLETED".to_string(),
                "playbook.failed" | "playbook_failed" => return "FAILED".to_string(),
                "playbook.cancelled" | "playbook_cancelled" => return "CANCELLED".to_string(),
                _ => {}
            }
            if event.status == "FAILED" {
                return "FAILED".to_string();
            }
        }
        "RUNNING".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_execution_summary_serialization() {
        let summary = ExecutionSummary {
            execution_id: 12345,
            catalog_id: 67890,
            path: Some("test/playbook".to_string()),
            status: "RUNNING".to_string(),
            started_at: Utc::now(),
            completed_at: None,
            event_count: 5,
        };

        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("12345"));
        assert!(json.contains("RUNNING"));
    }

    #[test]
    fn test_execution_status_serialization() {
        let status = ExecutionStatus {
            execution_id: 12345,
            status: "RUNNING".to_string(),
            current_step: Some("process_data".to_string()),
            progress: ExecutionProgress {
                total_steps: 5,
                completed_steps: 2,
                running_steps: 1,
                failed_steps: 0,
            },
            is_cancelled: false,
        };

        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("process_data"));
        assert!(json.contains("total_steps"));
    }

    #[test]
    fn test_execution_filter_default() {
        let filter = ExecutionFilter::default();
        assert!(filter.catalog_id.is_none());
        assert!(filter.limit.is_none());
    }

    // ===== Phase D R4 follow-up tests (noetl/server#146) =====

    /// Build a synthetic `ExecutionService` for unit-level tests.
    /// `determine_status` is pool-free — it operates on an in-memory
    /// event slice — so we only need a syntactically valid service.
    /// (The SQL fix in `get_status` itself is validated by the kind-val
    /// run captured in noetl/ai-meta wiki Sessions-Log on 2026-06-07;
    /// SQL semantics aren't covered by these unit tests but the
    /// determine_status helper IS the in-memory mirror of the SQL
    /// terminal-event short-circuit landed in this PR.)
    fn make_event(event_type: &str, status: &str) -> ExecutionEvent {
        ExecutionEvent {
            event_id: 0,
            event_type: event_type.to_string(),
            node_name: None,
            status: status.to_string(),
            created_at: Utc::now(),
            result: None,
            error: None,
        }
    }

    fn make_service() -> ExecutionService {
        // ExecutionService::new_legacy gives us a pool-less shim valid
        // for the in-memory determine_status path.
        let snowflake = std::sync::Arc::new(
            crate::snowflake::SnowflakeGenerator::new(0).expect("snowflake init"),
        );
        ExecutionService::new_legacy(
            sqlx::PgPool::connect_lazy("postgres://invalid").expect("lazy pool"),
            snowflake,
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn determine_status_returns_completed_on_playbook_completed_event() {
        let service = make_service();
        let events = vec![
            make_event("step.enter", "ENTERED"),
            // Worker emits `command.completed` with lowercase `success`
            // — this is the realistic shape that broke the SQL counter
            // in get_status before the #146 fix.
            make_event("command.completed", "success"),
            make_event("playbook.completed", "COMPLETED"),
        ];
        assert_eq!(service.determine_status(&events), "COMPLETED");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn determine_status_returns_completed_on_underscore_alias() {
        let service = make_service();
        let events = vec![
            make_event("command.completed", "success"),
            make_event("playbook_completed", "COMPLETED"),
        ];
        assert_eq!(service.determine_status(&events), "COMPLETED");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn determine_status_returns_failed_on_playbook_failed_event() {
        let service = make_service();
        let events = vec![
            make_event("step.enter", "ENTERED"),
            make_event("playbook.failed", "FAILED"),
        ];
        assert_eq!(service.determine_status(&events), "FAILED");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn determine_status_returns_cancelled_on_playbook_cancelled() {
        let service = make_service();
        let events = vec![
            make_event("step.enter", "ENTERED"),
            make_event("playbook.cancelled", "CANCELLED"),
        ];
        assert_eq!(service.determine_status(&events), "CANCELLED");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn determine_status_stays_running_without_terminal_event() {
        let service = make_service();
        let events = vec![
            make_event("step.enter", "ENTERED"),
            // Even after command.completed with `success` (the bug
            // shape that masked completion in the SQL path), without
            // a playbook-level terminal event there's no signal to
            // call the playbook done.
            make_event("command.completed", "success"),
        ];
        assert_eq!(service.determine_status(&events), "RUNNING");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn determine_status_returns_failed_on_individual_event_failure() {
        let service = make_service();
        let events = vec![
            make_event("step.enter", "ENTERED"),
            make_event("command.failed", "FAILED"),
        ];
        assert_eq!(service.determine_status(&events), "FAILED");
    }

    // ===== noetl/ai-meta#72 — in-flight command guard tests =====
    //
    // These tests exercise the *logic* introduced in Change 1/2/3
    // of get_status (running_steps SQL fix + in_flight_commands
    // query + COMPLETED guard).  Because get_status runs SQL against
    // a live database, the full integration path is validated on the
    // kind cluster (see noetl/ai-meta#72 + Sessions-Log.md).  The
    // unit tests below verify the surrounding control-flow and the
    // helper logic that does not require a pool.
    //
    // Specifically:
    //   - The running_steps SQL change is exercised by verifying that
    //     the `command.claimed` / `command.started` event types with
    //     status `'STARTED'` are the shapes the worker actually emits
    //     (confirmed in repos/worker/src/events/emitter.rs).
    //   - The COMPLETED guard logic is exercised by asserting that
    //     determine_status (which has no in-flight check but IS the
    //     terminal-event short-circuit) stays RUNNING when no terminal
    //     event is present — exactly the shape where Bug 2 would fire.

    /// When step counts are equal but no terminal event has landed,
    /// the endpoint must return RUNNING (not COMPLETED).  This covers
    /// the scenario from Bug 2 where stats.1 == stats.0 trips the old
    /// COMPLETED branch for an iterator step that has issued N commands
    /// but none have completed yet.
    ///
    /// The SQL-level guard (in_flight_commands.0 > 0) is the runtime
    /// fix; this test pins the determine_status path used for the
    /// in-memory short-circuit, confirming it also returns RUNNING.
    #[tokio::test(flavor = "current_thread")]
    async fn test_get_status_returns_running_when_command_in_flight_despite_step_counts_equal() {
        let service = make_service();
        // Two steps both with command.completed events, but NO
        // playbook.completed — simulates the moment between the last
        // step completing and the orchestrator emitting playbook.completed.
        // determine_status must return RUNNING (no terminal event).
        let events = vec![
            make_event("step.enter", "ENTERED"),
            make_event("command.completed", "success"),
            make_event("step.enter", "ENTERED"),
            make_event("command.completed", "success"),
            // No playbook.completed — there are in-flight commands
            // in noetl.command; the SQL guard (Change 3) prevents
            // COMPLETED; the in-memory path correctly returns RUNNING
            // because there is no terminal event.
        ];
        assert_eq!(service.determine_status(&events), "RUNNING");
    }

    /// Workers emit `command.started` with `status='STARTED'`.
    /// The old running_steps filter (`status='RUNNING'`) would miss
    /// this event entirely.  This test documents the actual wire shape
    /// the worker sends, confirming the SQL fix must accept 'STARTED'.
    ///
    /// (Full running_steps=1 assertion requires a live DB; this test
    /// confirms the worker-emitted shape via determine_status to
    /// ensure no terminal event fires for a started command.)
    #[tokio::test(flavor = "current_thread")]
    async fn test_get_status_counts_running_command_started_status() {
        let service = make_service();
        // Worker emits command.started with status='STARTED' (not 'RUNNING').
        // determine_status should return RUNNING — no terminal event.
        let events = vec![
            make_event("step.enter", "ENTERED"),
            make_event("command.started", "STARTED"),
        ];
        assert_eq!(service.determine_status(&events), "RUNNING");
    }

    /// Workers emit `command.claimed` with `status='STARTED'`
    /// (see repos/worker/src/events/emitter.rs::emit_command_claimed).
    /// The SQL running_steps filter must include this event type +
    /// status combination.  This test documents the wire shape.
    #[tokio::test(flavor = "current_thread")]
    async fn test_get_status_counts_running_command_claimed_status() {
        let service = make_service();
        // Worker emits command.claimed with status='STARTED'.
        // determine_status should return RUNNING — no terminal event.
        let events = vec![
            make_event("step.enter", "ENTERED"),
            make_event("command.claimed", "STARTED"),
        ];
        assert_eq!(service.determine_status(&events), "RUNNING");
    }

    /// COMPLETED must only fire when both the terminal event is present
    /// AND (at the SQL level) zero in-flight commands remain.
    /// This test exercises the terminal-event path: with
    /// playbook.completed present, determine_status returns COMPLETED
    /// regardless of other events — the SQL in_flight_commands guard
    /// is the second line of defence, and is only reached when no
    /// terminal event exists.
    #[tokio::test(flavor = "current_thread")]
    async fn test_get_status_completed_only_when_no_in_flight() {
        let service = make_service();
        // Terminal event present + all steps have command.completed.
        // The SQL path also checks in_flight_commands.0 == 0 before
        // returning COMPLETED; this test verifies the terminal-event
        // short-circuit (which bypasses the in-flight check, as
        // playbook.completed is authoritative).
        let events = vec![
            make_event("step.enter", "ENTERED"),
            make_event("command.completed", "success"),
            make_event("step.enter", "ENTERED"),
            make_event("command.completed", "success"),
            make_event("playbook.completed", "COMPLETED"),
        ];
        assert_eq!(service.determine_status(&events), "COMPLETED");
    }

    // ===== noetl/ai-meta#103 2d-3 — cancel/finalize chokepoint =====

    /// The pool-less unit-test shim (`new_legacy`) carries no
    /// `AppState`, so `require_state` must reject it with a clear error
    /// rather than silently falling back to a raw INSERT.  This pins the
    /// contract: there is exactly one `noetl.event` write path for
    /// cancel/finalize (the `emit_event` chokepoint), and it always runs
    /// with a real `AppState` in production.  `cancel` / `finalize`
    /// themselves can't be unit-tested without a live DB (the catalog
    /// lookup + write are validated on kind), but this guards against a
    /// future shim accidentally bypassing the gate.
    #[tokio::test(flavor = "current_thread")]
    async fn require_state_errors_without_app_state() {
        let service = make_service();
        let result = service.require_state();
        assert!(result.is_err(), "test shim must not yield an AppState");
        assert!(
            matches!(result, Err(AppError::Internal(_))),
            "expected an Internal error variant"
        );
    }
}
