//! The listing query must DECODE, not merely render correct SQL.
//!
//! ⚠ Gated on `NOETL_TEST_DATABASE_URL` and skipped when absent, so CI without a
//! database stays green. The CI-runnable guard for this defect is
//! `tests/null_selected_columns_decode.rs`; this file is the end-to-end proof
//! that the guard is guarding the right thing.
//!
//! Run locally:
//!   NOETL_TEST_DATABASE_URL=postgres://postgres:x@127.0.0.1:15443/postgres \
//!     cargo test --test catalog_list_decodes_against_postgres -- --nocapture

use noetl_server::db::queries::catalog::{list_catalog_entries, CatalogListOptions};

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("NOETL_TEST_DATABASE_URL").ok()?;
    Some(sqlx::PgPool::connect(&url).await.expect("connect"))
}

#[tokio::test]
async fn a_listing_without_bodies_decodes() {
    let Some(pool) = pool().await else {
        eprintln!("skipped: NOETL_TEST_DATABASE_URL not set");
        return;
    };

    sqlx::query("CREATE SCHEMA IF NOT EXISTS noetl")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS noetl.catalog (
            catalog_id bigint, path text, kind text, version smallint,
            content text, layout jsonb, payload jsonb, meta jsonb,
            -- ⚠ `timestamp`, NOT `timestamptz`. The query does
            -- `created_at AT TIME ZONE 'UTC'`, which converts timestamp ->
            -- timestamptz (what the Rust `DateTime<Utc>` wants) and timestamptz
            -- -> timestamp (which fails to decode). A fixture schema that
            -- guesses this wrong tests a database prod does not have — which is
            -- exactly how the defect this file exists for got through.
            created_at timestamp DEFAULT now())",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM noetl.catalog")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO noetl.catalog (catalog_id, path, kind, version, content, layout, payload, meta)
         VALUES (1, 'p/one', 'playbook', 1, 'apiVersion: v2', '{\"a\":1}'::jsonb, '{\"b\":2}'::jsonb, '{}'::jsonb)",
    )
    .execute(&pool).await.unwrap();

    // ⭐ The path that 500'd in production: bodies NOT requested, so the query
    // selects `NULL::text AS content`.
    let (rows, total) = list_catalog_entries(&pool, None, true, &CatalogListOptions::default())
        .await
        .expect(
            "a listing without bodies must DECODE — this is the call that \
                 returned `unexpected null; try decoding as an Option` on every \
                 /api/catalog/list request in prod",
        );
    assert_eq!(total, 1);
    assert_eq!(rows.len(), 1);
    assert!(
        rows[0].content.is_none(),
        "the body must be absent, not empty"
    );
    assert_eq!(rows[0].path, "p/one", "identity must survive");

    // …and asking for bodies still returns them.
    let opts = CatalogListOptions {
        include_content: true,
        ..Default::default()
    };
    let (rows, _) = list_catalog_entries(&pool, None, true, &opts)
        .await
        .expect("with bodies");
    assert_eq!(rows[0].content.as_deref(), Some("apiVersion: v2"));
}
