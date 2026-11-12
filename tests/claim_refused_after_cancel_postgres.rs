//! A queued command must not be claimable once its execution is cancelled.
//!
//! The defect: `POST /api/executions/{id}/cancel` writes `playbook_cancelled`
//! (`ExecutionService::cancel`), but `claim_command` refused a claim only when
//! an `execution.cancelled` event existed.  A command issued before the cancel
//! stayed claimable, so a worker could still run it.
//!
//! This drives the real cancel service and the real claim handler against
//! Postgres with the server's own DDL.  Skipped unless `NOETL_TEST_DATABASE_URL`
//! is set, like `catalog_list_decodes_against_postgres.rs`; the CI-runnable
//! guard is `handlers::events::claim_cancel_check_tests`.
//!
//! ```text
//! NOETL_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:55499/noetl_claim_test \
//!   cargo test --test claim_refused_after_cancel_postgres -- --nocapture
//! ```

use axum::extract::{Json, Path, State};
use noetl_server::config::AppConfig;
use noetl_server::handlers::events::{claim_command, ClaimRequest};
use noetl_server::services::execution::ExecutionService;
use noetl_server::state::AppState;
use noetl_server::AppError;
use serde_json::json;

const DDL: &str = include_str!("../db/ddl/postgres/schema_ddl.sql");

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("NOETL_TEST_DATABASE_URL").ok()?;
    let pool = sqlx::PgPool::connect(&url).await.expect("connect");
    sqlx::query("CREATE SCHEMA IF NOT EXISTS noetl")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(DDL).execute(&pool).await.expect("apply DDL");
    // `noetl.event.catalog_id` references the catalog.
    sqlx::query("INSERT INTO noetl.resource (name) VALUES ('Playbook') ON CONFLICT DO NOTHING")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO noetl.catalog (catalog_id, path, version, kind, content)
         VALUES (1, 'test/claim-after-cancel', 1, 'Playbook', 'apiVersion: noetl.io/v2')
         ON CONFLICT DO NOTHING",
    )
    .execute(&pool)
    .await
    .unwrap();
    Some(pool)
}

/// Seed a running execution with one queued command; return the command's
/// `command.issued` event id (what a worker claims by).
async fn seed_queued_command(pool: &sqlx::PgPool, execution_id: i64) -> i64 {
    let catalog_id = 1_i64;
    let issued_event_id = execution_id + 1;
    sqlx::query(
        "INSERT INTO noetl.event (execution_id, catalog_id, event_id, event_type, node_name, status)
         VALUES ($1, $2, $3, 'playbook_started', 'playbook', 'RUNNING')",
    )
    .bind(execution_id)
    .bind(catalog_id)
    .bind(execution_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO noetl.event
           (execution_id, catalog_id, event_id, event_type, node_name, node_type, status, context, meta)
         VALUES ($1, $2, $3, 'command.issued', 'fetch', 'http', 'PENDING', '{}'::jsonb, $4)",
    )
    .bind(execution_id)
    .bind(catalog_id)
    .bind(issued_event_id)
    .bind(json!({ "command_id": format!("{execution_id}:fetch:{issued_event_id}") }))
    .execute(pool)
    .await
    .unwrap();
    issued_event_id
}

async fn claim(state: &AppState, event_id: i64) -> Result<String, AppError> {
    claim_command(
        State(state.clone()),
        Path(event_id),
        Json(ClaimRequest {
            worker_id: "regression-worker".into(),
        }),
    )
    .await
    .map(|Json(r)| r.status)
}

async fn claimed_events(pool: &sqlx::PgPool, execution_id: i64) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM noetl.event WHERE execution_id = $1 AND event_type = 'command.claimed'",
    )
    .bind(execution_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn assert_refused_as_cancelled(result: Result<String, AppError>, what: &str) {
    match result {
        Err(AppError::Conflict(msg)) => assert!(
            msg.contains("cancelled"),
            "{what}: refused, but not for cancellation: {msg}"
        ),
        other => panic!("{what}: a queued command was claimable after cancel: {other:?}"),
    }
}

#[tokio::test]
async fn a_queued_command_is_not_claimed_after_cancel() {
    let Some(pool) = pool().await else {
        eprintln!("skipped: NOETL_TEST_DATABASE_URL not set");
        return;
    };
    // Both buses are required at startup.  No writer address is configured, so
    // the EHDB publishers are absent and every event write is the synchronous
    // INSERT into `noetl.event` (gate-off), the table the claim check reads.
    // One test function in this file, so setting the process env is safe.
    std::env::set_var("NOETL_EVENT_BUS", "ehdb");
    std::env::remove_var("NOETL_EVENT_BUS_WRITER_ADDRS");
    let config = AppConfig {
        command_bus: Some("ehdb".into()),
        ..AppConfig::default()
    };
    let state = AppState::new_legacy(pool.clone(), config);
    // Distinct ids per run so reruns against the same database do not collide.
    let base = (chrono::Utc::now().timestamp_micros() % 1_000_000_000_000) * 1_000;

    // Control: with no cancel, the same seeding is claimable.  Without this the
    // refusals below could come from a broken fixture, not the cancel check.
    let eid = base + 100;
    let cmd = seed_queued_command(&pool, eid).await;
    assert_eq!(claim(&state, cmd).await.expect("control claim"), "ok");
    assert_eq!(claimed_events(&pool, eid).await, 1);

    // The real path: cancel through the service the cancel API calls, then claim.
    let eid = base + 200;
    let cmd = seed_queued_command(&pool, eid).await;
    let svc = ExecutionService::new(state.pools.clone(), state.snowflake.clone(), state.clone());
    svc.cancel(eid).await.expect("cancel");
    let written: Vec<String> = sqlx::query_scalar(
        "SELECT event_type FROM noetl.event WHERE execution_id = $1 AND status = 'CANCELLED'",
    )
    .bind(eid)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        written,
        vec!["playbook_cancelled".to_string()],
        "the cancel API's event type changed; keep CANCEL_API_EVENT_TYPE in step"
    );
    assert_refused_as_cancelled(claim(&state, cmd).await, "cancel API");
    assert_eq!(claimed_events(&pool, eid).await, 0, "no claim may be recorded");

    // The other two spellings must refuse too.
    for (i, spelling) in ["playbook.cancelled", "execution.cancelled"].iter().enumerate() {
        let eid = base + 300 + (i as i64) * 100;
        let cmd = seed_queued_command(&pool, eid).await;
        sqlx::query(
            "INSERT INTO noetl.event (execution_id, catalog_id, event_id, event_type, node_name, status)
             VALUES ($1, 1, $2, $3, 'playbook', 'CANCELLED')",
        )
        .bind(eid)
        .bind(eid + 50)
        .bind(*spelling)
        .execute(&pool)
        .await
        .unwrap();
        assert_refused_as_cancelled(claim(&state, cmd).await, spelling);
        assert_eq!(claimed_events(&pool, eid).await, 0);
    }
}
