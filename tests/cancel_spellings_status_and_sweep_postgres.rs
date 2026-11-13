//! Every cancel spelling must read as cancelled in the execution-status lookup
//! and as terminal in the orphan-command sweep.
//!
//! The defect (same family as noetl/server#581): the cancel API writes
//! `playbook_cancelled`, but `get_execution_status` checked only
//! `playbook.cancelled`, and the orphan sweep's terminal list held only the
//! dotted spelling.  An API-cancelled execution therefore read as RUNNING, and
//! one with a dead worker's claim was swept and marked `playbook.failed` after
//! it had been cancelled.
//!
//! Skipped unless `NOETL_TEST_DATABASE_URL` is set, like
//! `claim_refused_after_cancel_postgres.rs`.  The CI-runnable guards are
//! `db::queries::event::cancelled_event_types_tests` and
//! `handlers::orphan_sweep::tests`.
//!
//! ```text
//! NOETL_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:55499/noetl_claim_test \
//!   cargo test --test cancel_spellings_status_and_sweep_postgres -- --nocapture
//! ```

use noetl_server::db::queries::event::{get_execution_status, CANCELLED_EVENT_TYPES};
use noetl_server::handlers::orphan_sweep::orphan_candidate_execution_ids;

const DDL: &str = include_str!("../db/ddl/postgres/schema_ddl.sql");
const SPELLINGS: [&str; 3] = [
    "playbook_cancelled",
    "playbook.cancelled",
    "execution.cancelled",
];

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("NOETL_TEST_DATABASE_URL").ok()?;
    let pool = sqlx::PgPool::connect(&url).await.expect("connect");
    sqlx::query("CREATE SCHEMA IF NOT EXISTS noetl")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(DDL).execute(&pool).await.expect("apply DDL");
    sqlx::query("INSERT INTO noetl.resource (name) VALUES ('Playbook') ON CONFLICT DO NOTHING")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO noetl.catalog (catalog_id, path, version, kind, content)
         VALUES (1, 'test/cancel-spellings', 1, 'Playbook', 'apiVersion: noetl.io/v2')
         ON CONFLICT DO NOTHING",
    )
    .execute(&pool)
    .await
    .unwrap();
    Some(pool)
}

/// Insert one event `age_secs` in the past.
async fn event(
    pool: &sqlx::PgPool,
    execution_id: i64,
    event_id: i64,
    event_type: &str,
    status: &str,
    worker_id: Option<&str>,
    age_secs: i64,
) {
    sqlx::query(
        "INSERT INTO noetl.event
           (execution_id, catalog_id, event_id, event_type, node_name, status, worker_id, created_at)
         VALUES ($1, 1, $2, $3, 'fetch', $4, $5, NOW() - INTERVAL '1 second' * $6)",
    )
    .bind(execution_id)
    .bind(event_id)
    .bind(event_type)
    .bind(status)
    .bind(worker_id)
    .bind(age_secs)
    .execute(pool)
    .await
    .unwrap();
}

/// A running execution whose head command was claimed 120 s ago and never
/// finished: the shape the orphan sweep looks for.
async fn seed_claimed(pool: &sqlx::PgPool, execution_id: i64) {
    event(
        pool,
        execution_id,
        execution_id,
        "playbook_started",
        "RUNNING",
        None,
        130,
    )
    .await;
    event(
        pool,
        execution_id,
        execution_id + 1,
        "command.claimed",
        "RUNNING",
        Some("dead-worker"),
        120,
    )
    .await;
}

#[tokio::test]
async fn every_cancel_spelling_is_cancelled_for_status_and_terminal_for_the_sweep() {
    let Some(pool) = pool().await else {
        eprintln!("skipped: NOETL_TEST_DATABASE_URL not set");
        return;
    };
    assert_eq!(
        CANCELLED_EVENT_TYPES, SPELLINGS,
        "keep this test's spellings in step"
    );
    let base = (chrono::Utc::now().timestamp_micros() % 1_000_000_000_000) * 1_000;

    // Control: a running execution with a stale claim reads RUNNING and is a
    // sweep candidate.  Without this the assertions below could pass on a
    // broken fixture.
    let control = base + 100;
    seed_claimed(&pool, control).await;
    assert_eq!(
        get_execution_status(&pool, control).await.unwrap(),
        "RUNNING"
    );
    let candidates = orphan_candidate_execution_ids(&pool, 60, 3_600, 10_000)
        .await
        .unwrap();
    assert!(
        candidates.contains(&control),
        "control must be a sweep candidate"
    );

    for (i, spelling) in SPELLINGS.iter().enumerate() {
        let eid = base + 200 + (i as i64) * 100;
        seed_claimed(&pool, eid).await;
        event(&pool, eid, eid + 50, spelling, "CANCELLED", None, 110).await;

        assert_eq!(
            get_execution_status(&pool, eid).await.unwrap(),
            "CANCELLED",
            "status lookup missed {spelling}"
        );
        let candidates = orphan_candidate_execution_ids(&pool, 60, 3_600, 10_000)
            .await
            .unwrap();
        assert!(
            !candidates.contains(&eid),
            "the orphan sweep would fail an execution cancelled with {spelling}"
        );
    }
}
