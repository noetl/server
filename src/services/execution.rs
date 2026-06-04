//! Execution management service.
//!
//! Provides operations for managing playbook executions,
//! including listing, status queries, cancellation, and finalization.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::db::{DbPool, DbPoolMap};
use crate::error::{AppError, AppResult};

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
    pub fn new(
        pools: DbPoolMap,
        snowflake: std::sync::Arc<crate::snowflake::SnowflakeGenerator>,
    ) -> Self {
        Self { pools, snowflake }
    }

    /// Build an [`ExecutionService`] wrapping a single legacy
    /// pool — for test / example code paths that don't have a
    /// [`DbPoolMap`] in scope.  Internally wraps the pool via
    /// [`DbPoolMap::from_single_pool`].
    pub fn new_legacy(
        db: DbPool,
        snowflake: std::sync::Arc<crate::snowflake::SnowflakeGenerator>,
    ) -> Self {
        Self::new(DbPoolMap::from_single_pool(db), snowflake)
    }

    /// Borrow the per-execution pool for the given `execution_id`.
    /// Internal helper to keep the per-method call sites short.
    #[inline]
    fn pool_for(&self, execution_id: i64) -> &DbPool {
        self.pools.pool_for(execution_id)
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
                    sqlx::query_as::<_, StatsRow>(
                        r#"
                        WITH execution_stats AS (
                            SELECT
                                execution_id,
                                catalog_id,
                                MIN(created_at) AT TIME ZONE 'UTC' as started_at,
                                MAX(CASE WHEN status IN ('COMPLETED', 'FAILED', 'CANCELLED') THEN created_at END) AT TIME ZONE 'UTC' as completed_at,
                                COUNT(*) as event_count,
                                MAX(CASE
                                    WHEN event_type IN ('playbook.completed', 'playbook_completed') THEN 'COMPLETED'
                                    WHEN event_type IN ('playbook.failed', 'playbook_failed') THEN 'FAILED'
                                    WHEN event_type IN ('playbook.cancelled', 'playbook_cancelled') THEN 'CANCELLED'
                                    WHEN status = 'FAILED' THEN 'FAILED'
                                    ELSE 'RUNNING'
                                END) as status
                            FROM noetl.event
                            WHERE ($1::BIGINT IS NULL OR catalog_id = $1)
                            GROUP BY execution_id, catalog_id
                        )
                        SELECT
                            execution_id,
                            catalog_id,
                            status,
                            started_at,
                            completed_at,
                            event_count
                        FROM execution_stats
                        WHERE ($2::TEXT IS NULL OR status = $2)
                        ORDER BY started_at DESC
                        LIMIT $3
                        "#,
                    )
                    .bind(catalog_id)
                    .bind(&status)
                    .bind(fetch_cap)
                    .fetch_all(&pool)
                    .await
                }
            })
            .await?;

        // Stage 2 — merge per-shard rows, sort by started_at DESC.
        let mut merged: Vec<StatsRow> =
            per_shard.into_iter().flat_map(|(_idx, rows)| rows).collect();
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
        let path_pattern_lower = filter
            .path
            .as_ref()
            .map(|p| p.to_lowercase());
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

        // Get all events for this execution
        let event_rows: Vec<(
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
                ORDER BY created_at ASC
                "#,
        )
        .bind(execution_id)
        .fetch_all(self.pool_for(execution_id))
        .await?;

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

        // Get step statistics
        let stats: (i64, i64, i64, i64) = sqlx::query_as(
            r#"
            SELECT
                COUNT(DISTINCT CASE WHEN event_type = 'step.enter' THEN node_name END) as total_steps,
                COUNT(DISTINCT CASE WHEN event_type IN ('step.exit', 'command.completed') AND status = 'COMPLETED' THEN node_name END) as completed_steps,
                COUNT(DISTINCT CASE WHEN event_type IN ('step.enter', 'command.started') AND status = 'RUNNING' THEN node_name END) as running_steps,
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

        // Determine overall status
        let status = if is_cancelled {
            "CANCELLED".to_string()
        } else if stats.3 > 0 {
            "FAILED".to_string()
        } else if stats.1 == stats.0 && stats.0 > 0 {
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

        // Get catalog_id for the event
        let catalog_id: Option<(i64,)> =
            sqlx::query_as("SELECT catalog_id FROM noetl.event WHERE execution_id = $1 LIMIT 1")
                .bind(execution_id)
                .fetch_optional(self.pool_for(execution_id))
                .await?;

        let catalog_id = catalog_id
            .ok_or_else(|| AppError::NotFound(format!("Execution not found: {}", execution_id)))?
            .0;

        // Generate event ID via the application-side snowflake
        // generator (Phase F R1.5 of noetl/ai-meta#49).
        let event_id: (i64,) = (self.snowflake.generate()?,);

        // Insert cancellation event
        sqlx::query(
            r#"
            INSERT INTO noetl.event (
                event_id, execution_id, catalog_id, event_type,
                node_id, node_name, status, created_at
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            "#,
        )
        .bind(event_id.0)
        .bind(execution_id)
        .bind(catalog_id)
        .bind("playbook_cancelled")
        .bind("playbook")
        .bind("playbook")
        .bind("CANCELLED")
        .bind(Utc::now())
        .execute(self.pool_for(execution_id))
        .await?;

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

        // Get catalog_id
        let catalog_id: Option<(i64,)> =
            sqlx::query_as("SELECT catalog_id FROM noetl.event WHERE execution_id = $1 LIMIT 1")
                .bind(execution_id)
                .fetch_optional(self.pool_for(execution_id))
                .await?;

        let catalog_id = catalog_id
            .ok_or_else(|| AppError::NotFound(format!("Execution not found: {}", execution_id)))?
            .0;

        // Generate event ID via the application-side snowflake
        // generator (Phase F R1.5 of noetl/ai-meta#49).
        let event_id: (i64,) = (self.snowflake.generate()?,);

        let event_type = if status == "COMPLETED" {
            "playbook_completed"
        } else {
            "playbook_failed"
        };

        // Insert finalization event
        sqlx::query(
            r#"
            INSERT INTO noetl.event (
                event_id, execution_id, catalog_id, event_type,
                node_id, node_name, status, error, created_at
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            "#,
        )
        .bind(event_id.0)
        .bind(execution_id)
        .bind(catalog_id)
        .bind(event_type)
        .bind("playbook")
        .bind("playbook")
        .bind(status)
        .bind(error)
        .bind(Utc::now())
        .execute(self.pool_for(execution_id))
        .await?;

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
}
