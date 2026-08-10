//! Event database queries.

use chrono::{DateTime, Utc};

use crate::db::models::Event;
use crate::db::DbPool;
use crate::error::AppResult;

/// Insert a new event.
#[allow(clippy::too_many_arguments)]
pub async fn insert_event(
    pool: &DbPool,
    event_id: i64,
    execution_id: i64,
    catalog_id: i64,
    parent_event_id: Option<i64>,
    parent_execution_id: Option<i64>,
    event_type: &str,
    node_id: Option<&str>,
    node_name: Option<&str>,
    node_type: Option<&str>,
    status: &str,
    context: Option<&serde_json::Value>,
    meta: Option<&serde_json::Value>,
    result: Option<&serde_json::Value>,
    worker_id: Option<&str>,
    attempt: Option<i32>,
) -> AppResult<i64> {
    let row: (i64,) = sqlx::query_as(
        r#"
        INSERT INTO noetl.event (
            event_id, execution_id, catalog_id, parent_event_id, parent_execution_id,
            event_type, node_id, node_name, node_type, status,
            context, meta, result, worker_id, attempt, created_at
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
        RETURNING id
        "#,
    )
    .bind(event_id)
    .bind(execution_id)
    .bind(catalog_id)
    .bind(parent_event_id)
    .bind(parent_execution_id)
    .bind(event_type)
    .bind(node_id)
    .bind(node_name)
    .bind(node_type)
    .bind(status)
    .bind(context)
    .bind(meta)
    .bind(result)
    .bind(worker_id)
    .bind(attempt)
    .bind(Utc::now())
    .fetch_one(pool)
    .await?;

    Ok(row.0)
}

/// Get an event by event_id.
pub async fn get_event_by_id(pool: &DbPool, event_id: i64) -> AppResult<Option<Event>> {
    let event = sqlx::query_as::<_, Event>(
        r#"
        SELECT id, execution_id, catalog_id, event_id, parent_event_id, parent_execution_id,
               event_type, node_id, node_name, node_type, status,
               context, meta, result, worker_id, attempt, created_at
        FROM noetl.event
        WHERE event_id = $1
        "#,
    )
    .bind(event_id)
    .fetch_optional(pool)
    .await?;

    Ok(event)
}

/// Get all events for an execution.
pub async fn get_events_by_execution(
    pool: &DbPool,
    execution_id: i64,
    event_type: Option<&str>,
    limit: Option<i64>,
) -> AppResult<Vec<Event>> {
    let events = if let Some(et) = event_type {
        sqlx::query_as::<_, Event>(
            r#"
            SELECT id, execution_id, catalog_id, event_id, parent_event_id, parent_execution_id,
                   event_type, node_id, node_name, node_type, status,
                   context, meta, result, worker_id, attempt, created_at
            FROM noetl.event
            WHERE execution_id = $1 AND event_type = $2
            ORDER BY created_at ASC
            LIMIT $3
            "#,
        )
        .bind(execution_id)
        .bind(et)
        .bind(limit.unwrap_or(1000))
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as::<_, Event>(
            r#"
            SELECT id, execution_id, catalog_id, event_id, parent_event_id, parent_execution_id,
                   event_type, node_id, node_name, node_type, status,
                   context, meta, result, worker_id, attempt, created_at
            FROM noetl.event
            WHERE execution_id = $1
            ORDER BY created_at ASC
            LIMIT $2
            "#,
        )
        .bind(execution_id)
        .bind(limit.unwrap_or(1000))
        .fetch_all(pool)
        .await?
    };

    Ok(events)
}

/// Get events by execution and multiple event types.
pub async fn get_events_by_types(
    pool: &DbPool,
    execution_id: i64,
    event_types: &[&str],
) -> AppResult<Vec<Event>> {
    // Build IN clause dynamically
    let placeholders: Vec<String> = (2..=event_types.len() + 1)
        .map(|i| format!("${}", i))
        .collect();
    let in_clause = placeholders.join(", ");

    let query = format!(
        r#"
        SELECT id, execution_id, catalog_id, event_id, parent_event_id, parent_execution_id,
               event_type, node_id, node_name, node_type, status,
               context, meta, result, worker_id, attempt, created_at
        FROM noetl.event
        WHERE execution_id = $1 AND event_type IN ({})
        ORDER BY created_at ASC
        "#,
        in_clause
    );

    let mut query_builder = sqlx::query_as::<_, Event>(&query).bind(execution_id);

    for et in event_types {
        query_builder = query_builder.bind(*et);
    }

    let events = query_builder.fetch_all(pool).await?;
    Ok(events)
}

/// Get the latest event for an execution.
pub async fn get_latest_event(
    pool: &DbPool,
    execution_id: i64,
    event_type: Option<&str>,
) -> AppResult<Option<Event>> {
    let event = if let Some(et) = event_type {
        sqlx::query_as::<_, Event>(
            r#"
            SELECT id, execution_id, catalog_id, event_id, parent_event_id, parent_execution_id,
                   event_type, node_id, node_name, node_type, status,
                   context, meta, result, worker_id, attempt, created_at
            FROM noetl.event
            WHERE execution_id = $1 AND event_type = $2
            ORDER BY created_at DESC
            LIMIT 1
            "#,
        )
        .bind(execution_id)
        .bind(et)
        .fetch_optional(pool)
        .await?
    } else {
        sqlx::query_as::<_, Event>(
            r#"
            SELECT id, execution_id, catalog_id, event_id, parent_event_id, parent_execution_id,
                   event_type, node_id, node_name, node_type, status,
                   context, meta, result, worker_id, attempt, created_at
            FROM noetl.event
            WHERE execution_id = $1
            ORDER BY created_at DESC
            LIMIT 1
            "#,
        )
        .bind(execution_id)
        .fetch_optional(pool)
        .await?
    };

    Ok(event)
}

/// Get events for a specific step.
pub async fn get_events_by_step(
    pool: &DbPool,
    execution_id: i64,
    node_name: &str,
) -> AppResult<Vec<Event>> {
    let events = sqlx::query_as::<_, Event>(
        r#"
        SELECT id, execution_id, catalog_id, event_id, parent_event_id, parent_execution_id,
               event_type, node_id, node_name, node_type, status,
               context, meta, result, worker_id, attempt, created_at
        FROM noetl.event
        WHERE execution_id = $1 AND node_name = $2
        ORDER BY created_at ASC
        "#,
    )
    .bind(execution_id)
    .bind(node_name)
    .fetch_all(pool)
    .await?;

    Ok(events)
}

/// Get step result (latest action_completed or command.completed for a step).
pub async fn get_step_result(
    pool: &DbPool,
    execution_id: i64,
    node_name: &str,
) -> AppResult<Option<serde_json::Value>> {
    let row: Option<(Option<serde_json::Value>,)> = sqlx::query_as(
        r#"
        SELECT result
        FROM noetl.event
        WHERE execution_id = $1
          AND node_name = $2
          AND event_type IN ('action_completed', 'command.completed')
        ORDER BY created_at DESC
        LIMIT 1
        "#,
    )
    .bind(execution_id)
    .bind(node_name)
    .fetch_optional(pool)
    .await?;

    Ok(row.and_then(|(r,)| r))
}

/// Get all step results for an execution.
pub async fn get_all_step_results(
    pool: &DbPool,
    execution_id: i64,
) -> AppResult<Vec<(String, serde_json::Value)>> {
    let rows: Vec<(Option<String>, Option<serde_json::Value>)> = sqlx::query_as(
        r#"
        SELECT DISTINCT ON (node_name) node_name, result
        FROM noetl.event
        WHERE execution_id = $1
          AND event_type IN ('action_completed', 'command.completed')
          AND result IS NOT NULL
        ORDER BY node_name, created_at DESC
        "#,
    )
    .bind(execution_id)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(|(name, result)| name.zip(result))
        .collect())
}

/// Check if an event type exists for an execution.
pub async fn has_event_type(pool: &DbPool, execution_id: i64, event_type: &str) -> AppResult<bool> {
    let row: Option<(i32,)> = sqlx::query_as(
        r#"
        SELECT 1
        FROM noetl.event
        WHERE execution_id = $1 AND event_type = $2
        LIMIT 1
        "#,
    )
    .bind(execution_id)
    .bind(event_type)
    .fetch_optional(pool)
    .await?;

    Ok(row.is_some())
}

/// Check if workflow is initialized.
pub async fn is_workflow_initialized(pool: &DbPool, execution_id: i64) -> AppResult<bool> {
    has_event_type(pool, execution_id, "workflow.initialized").await
}

/// Check if playbook has completed.
pub async fn is_playbook_completed(pool: &DbPool, execution_id: i64) -> AppResult<bool> {
    has_event_type(pool, execution_id, "playbook.completed").await
}

/// Check if playbook has failed.
pub async fn is_playbook_failed(pool: &DbPool, execution_id: i64) -> AppResult<bool> {
    has_event_type(pool, execution_id, "playbook.failed").await
}

/// noetl/ai-meta#251 — did any step of this execution record a failure?
///
/// The failure is always in the log: the pre-dispatch and tool paths write
/// `command.completed` / `command.failed` / `call.error` with `status = 'error'`.
/// Only the read boundary discarded it.
pub async fn has_errored_step(pool: &DbPool, execution_id: i64) -> AppResult<bool> {
    let row: Option<(i32,)> = sqlx::query_as(
        r#"
        SELECT 1
        FROM noetl.event
        WHERE execution_id = $1
          AND event_type IN ('command.completed', 'command.failed', 'call.error')
          AND status = 'error'
        LIMIT 1
        "#,
    )
    .bind(execution_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.is_some())
}

/// Is the step-derived execution status enabled?
///
/// Default OFF.  Turning this on changes what `COMPLETED` means to every
/// consumer of `GET /api/executions/{id}`: an execution that finished its
/// workflow but had a failing step flips COMPLETED -> FAILED.  That is a
/// semantics change, so it must be a deliberate flip and not a side effect of
/// deploying a new image.
fn status_from_steps_enabled() -> bool {
    matches!(
        std::env::var("NOETL_EXECUTION_STATUS_FROM_STEPS")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Get execution status based on events.
pub async fn get_execution_status(pool: &DbPool, execution_id: i64) -> AppResult<String> {
    // Check for terminal states first
    if is_playbook_failed(pool, execution_id).await? {
        return Ok("FAILED".to_string());
    }
    if is_playbook_completed(pool, execution_id).await? {
        // noetl/ai-meta#251 — a workflow can reach `playbook.completed` with a
        // step that raised.  Reporting COMPLETED there loses the failure at the
        // read boundary even though the event log recorded it.
        if status_from_steps_enabled() && has_errored_step(pool, execution_id).await? {
            return Ok("FAILED".to_string());
        }
        return Ok("COMPLETED".to_string());
    }

    // Check for cancellation
    if has_event_type(pool, execution_id, "playbook.cancelled").await? {
        return Ok("CANCELLED".to_string());
    }

    // Check if started
    if has_event_type(pool, execution_id, "playbook_started").await? {
        return Ok("RUNNING".to_string());
    }

    Ok("PENDING".to_string())
}

/// Count events for an execution.
pub async fn count_events(
    pool: &DbPool,
    execution_id: i64,
    event_type: Option<&str>,
) -> AppResult<i64> {
    let count: (i64,) = if let Some(et) = event_type {
        sqlx::query_as(
            r#"
            SELECT COUNT(*)
            FROM noetl.event
            WHERE execution_id = $1 AND event_type = $2
            "#,
        )
        .bind(execution_id)
        .bind(et)
        .fetch_one(pool)
        .await?
    } else {
        sqlx::query_as(
            r#"
            SELECT COUNT(*)
            FROM noetl.event
            WHERE execution_id = $1
            "#,
        )
        .bind(execution_id)
        .fetch_one(pool)
        .await?
    };

    Ok(count.0)
}

/// Get events since a specific timestamp.
pub async fn get_events_since(
    pool: &DbPool,
    execution_id: i64,
    since: DateTime<Utc>,
) -> AppResult<Vec<Event>> {
    let events = sqlx::query_as::<_, Event>(
        r#"
        SELECT id, execution_id, catalog_id, event_id, parent_event_id, parent_execution_id,
               event_type, node_id, node_name, node_type, status,
               context, meta, result, worker_id, attempt, created_at
        FROM noetl.event
        WHERE execution_id = $1 AND created_at > $2
        ORDER BY created_at ASC
        "#,
    )
    .bind(execution_id)
    .bind(since)
    .fetch_all(pool)
    .await?;

    Ok(events)
}

/// Get playbook start event to extract metadata.
pub async fn get_playbook_start_event(
    pool: &DbPool,
    execution_id: i64,
) -> AppResult<Option<Event>> {
    get_latest_event(pool, execution_id, Some("playbook_started")).await
}

#[cfg(test)]
mod tests {
    /// noetl/ai-meta#251 — a playbook whose step failed reported COMPLETED.
    ///
    /// The failure was never lost: `command.completed` carries `status = 'error'`
    /// and the payload holds the traceback.  It was dropped at the READ boundary,
    /// where `get_execution_status` returned COMPLETED the moment a
    /// `playbook.completed` event existed, without consulting step outcomes.
    ///
    /// This asserts the CALL SITE, not a helper in isolation.  A test that only
    /// checked `has_errored_step` would keep passing with the call reverted —
    /// the weakness the first noetl/ai-meta#250 guard had.
    #[test]
    fn completed_status_consults_step_outcomes() {
        let src = include_str!("event.rs");
        let non_test = src.split_once("\n#[cfg(test)]").map_or(src, |(b, _)| b);
        assert!(
            non_test.contains("has_errored_step"),
            "get_execution_status must consult step outcomes before reporting \
             COMPLETED (noetl/ai-meta#251)"
        );
        assert!(
            non_test.contains("NOETL_EXECUTION_STATUS_FROM_STEPS"),
            "the step-derived status must stay flag-gated so the semantics change \
             is a deliberate flip, not a side effect of a deploy"
        );
        let after = non_test
            .split_once("is_playbook_completed(pool, execution_id).await?")
            .map(|(_, rest)| rest.chars().take(500).collect::<String>())
            .unwrap_or_default();
        assert!(
            after.contains("has_errored_step"),
            "the COMPLETED branch itself must consult step outcomes"
        );
    }
}
