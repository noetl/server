//! The FK that turned one bad event into a cluster-wide stall.
//!
//! `noetl.event.catalog_id` is `NOT NULL REFERENCES noetl.catalog(catalog_id)`.
//! An event emitted with `catalog_id = 0` is therefore un-insertable — and
//! because the materializer posts a batch as one unit and never acks a failed
//! batch, one such event holds the head of the ordered drain for ever. On prod
//! (2026-09-27) five `playbook.failed` events emitted with a zero catalog_id on
//! 2026-09-24 were still being redelivered three days later: 120,545 project
//! errors against 24,888 successes, 271s of projection lag, a 22s playbook
//! taking 290s, and 1,283 executions abandoned by the reconcile poller.
//!
//! `handlers::events::terminal_catalog_id_tests` is the CI-runnable guard —
//! it pins "never emit `Some(0)`" without a database. THIS file is the proof
//! that the guard is guarding the right thing: that Postgres really does reject
//! the row, so refusing to emit it is not defensive noise.
//!
//! ⚠ Gated on `NOETL_TEST_DATABASE_URL` and skipped when absent, so CI without
//! a database stays green.
//!
//! Run locally:
//!   NOETL_TEST_DATABASE_URL=postgres://postgres:x@127.0.0.1:15443/postgres \
//!     cargo test --test zero_catalog_id_event_is_unacceptable -- --nocapture

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("NOETL_TEST_DATABASE_URL").ok()?;
    Some(sqlx::PgPool::connect(&url).await.expect("connect"))
}

#[tokio::test]
async fn postgres_rejects_a_zero_catalog_id_and_accepts_a_real_one() {
    let Some(pool) = pool().await else {
        eprintln!("skipped: NOETL_TEST_DATABASE_URL not set");
        return;
    };

    // A throwaway schema, so this never touches a real `noetl.*`.
    let schema = format!("fk_probe_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&pool)
        .await
        .unwrap();

    // The same shape as `noetl.catalog` / `noetl.event`, reduced to the one
    // relationship under test.
    sqlx::query(&format!(
        "CREATE TABLE {schema}.catalog (catalog_id BIGINT PRIMARY KEY)"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "CREATE TABLE {schema}.event (
             event_id   BIGINT PRIMARY KEY,
             catalog_id BIGINT NOT NULL
                        REFERENCES {schema}.catalog(catalog_id),
             event_type TEXT
         )"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let real_catalog_id: i64 = 717_028_015_384_821_801;
    sqlx::query(&format!(
        "INSERT INTO {schema}.catalog (catalog_id) VALUES ($1)"
    ))
    .bind(real_catalog_id)
    .execute(&pool)
    .await
    .unwrap();

    // There is deliberately NO `catalog_id = 0` row — exactly as on prod.
    let zero: Option<(i64,)> = sqlx::query_as(&format!(
        "SELECT catalog_id FROM {schema}.catalog WHERE catalog_id = 0"
    ))
    .fetch_optional(&pool)
    .await
    .unwrap();
    assert!(
        zero.is_none(),
        "the premise of this whole class of stall: no catalog row with id 0"
    );

    // 1. The poison. This is what `playbook.failed` looked like on prod.
    let err = sqlx::query(&format!(
        "INSERT INTO {schema}.event (event_id, catalog_id, event_type)
         VALUES (1, 0, 'playbook.failed')"
    ))
    .execute(&pool)
    .await
    .expect_err("a zero catalog_id MUST be rejected — that is why it poison-loops");
    let msg = err.to_string();
    assert!(
        msg.contains("violates foreign key constraint"),
        "the rejection must be the FK violation the materializer saw, got: {msg}"
    );

    // 2. The control: the identical row with a real catalog_id lands fine, so
    //    the rejection above is about the zero and nothing else.
    sqlx::query(&format!(
        "INSERT INTO {schema}.event (event_id, catalog_id, event_type)
         VALUES (2, $1, 'playbook.failed')"
    ))
    .bind(real_catalog_id)
    .execute(&pool)
    .await
    .expect("the same event with a resolved catalog_id must insert");

    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&pool)
        .await
        .unwrap();
}
