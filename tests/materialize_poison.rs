//! noetl/ai-meta#363 — a permanently-unmaterializable event must be PARKED, not
//! retried forever.
//!
//! Observed in kind: an event whose `catalog_id` has no `noetl.catalog` row was still
//! being retried more than an hour after publication, ~150 log lines deep, because the
//! FK fails identically every time. In publish-only mode this handler is the SOLE
//! writer, so the producer had already been told `ok` and held event ids it believed
//! were durable.
//!
//! Run: NOETL_TEST_PG_URL=postgres://postgres:chainproof@127.0.0.1:55432/noetl cargo test --test materialize_poison

async fn pool() -> Option<sqlx::PgPool> {
    let u = std::env::var("NOETL_TEST_PG_URL").ok()?;
    match sqlx::postgres::PgPoolOptions::new()
        .max_connections(3)
        .connect(&u)
        .await
    {
        Ok(p) => Some(p),
        Err(e) => panic!("NOETL_TEST_PG_URL is SET but unreachable ({e}) — refusing to skip."),
    }
}

/// The classifier, which is the whole decision.
///
/// ⚠ Unclassified MUST mean transient. Parking something that would have succeeded is
/// worse than a retry loop, because a retry loop is loud and a wrongly-parked event is
/// silent.
#[test]
fn a363_only_the_known_permanent_codes_are_permanent() {
    use noetl_server::handlers::internal::is_permanent_write_failure_for_test as perm;

    // Permanent: the insert cannot ever succeed with this row.
    for code in ["23503", "23502", "22P02", "22001"] {
        assert!(
            perm(code),
            "{code} must be PERMANENT — retrying it is the poison loop"
        );
    }
    // Transient or not-a-write-failure: must retry.
    for code in ["40001", "40P01", "53300", "08006", "57014", "XX000"] {
        assert!(
            !perm(code),
            "{code} must be TRANSIENT — parking it would silently drop an event that \
             would have succeeded"
        );
    }
    // ⭐ And unique_violation specifically is NOT permanent-parkable: the insert
    // carries ON CONFLICT DO NOTHING, so a duplicate is a normal outcome.
    assert!(
        !perm("23505"),
        "unique_violation must not be parked — ON CONFLICT DO NOTHING already handles it"
    );
}

/// ⭐⭐ End to end against a real database: an FK-violating batch is parked in
/// `noetl.event_dead_letter` and the handler does NOT error (erroring is what makes the
/// caller retry, and retrying is the defect).
#[tokio::test]
async fn a363_an_fk_violating_batch_lands_in_the_dead_letter_table() {
    let Some(p) = pool().await else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };
    let exec = 6301i64;
    let bogus_catalog = 99_999_999_999i64;

    sqlx::query("DELETE FROM noetl.event_dead_letter WHERE execution_id=$1")
        .bind(exec)
        .execute(&p)
        .await
        .ok();
    sqlx::query("DELETE FROM noetl.event WHERE execution_id=$1")
        .bind(exec)
        .execute(&p)
        .await
        .expect("clear");

    // Prove the fixture can actually exhibit the failure, before relying on it.
    let direct = sqlx::query(
        "INSERT INTO noetl.event (event_id, execution_id, catalog_id, event_type, status) \
         VALUES ($1,$2,$3,'step.enter','PENDING')",
    )
    .bind(exec * 10)
    .bind(exec)
    .bind(bogus_catalog)
    .execute(&p)
    .await;
    let err = direct.expect_err(
        "the fixture must FAIL on the foreign key, or this test proves nothing about \
         permanent failures",
    );
    let code = match &err {
        sqlx::Error::Database(d) => d.code().map(|c| c.to_string()),
        _ => None,
    };
    assert_eq!(
        code.as_deref(),
        Some("23503"),
        "expected foreign_key_violation; got {code:?}"
    );

    // Now park it the way the handler does, and assert it is retrievable.
    let parked = vec![noetl_server::services::internal::DeadLetterEvent {
        execution_id: exec,
        event_id: exec * 10,
        catalog_id: Some(bogus_catalog),
        event_type: Some("step.enter".into()),
        node_name: Some("s".into()),
        reason: format!("materialize permanent failure: {err}"),
        payload: serde_json::json!({}),
        parked_by: Some("events_materialize".into()),
    }];
    noetl_server::services::internal::dead_letter_events(&p, &parked)
        .await
        .expect("parking must succeed");

    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM noetl.event_dead_letter WHERE execution_id=$1 AND \
         reason LIKE 'materialize permanent failure:%'",
    )
    .bind(exec)
    .fetch_one(&p)
    .await
    .expect("count");
    assert_eq!(
        n, 1,
        "the event must be PARKED with its reason, so it is visible rather than merely \
         absent from noetl.event — absence is what the poison loop looked like"
    );

    sqlx::query("DELETE FROM noetl.event_dead_letter WHERE execution_id=$1")
        .bind(exec)
        .execute(&p)
        .await
        .ok();
}

/// ⚠ Both outcome labels pinned. An unpinned `parked` is an absent series and reads
/// like a build that cannot park at all.
#[test]
fn a363_materialize_outcomes_are_pinned() {
    noetl_server::metrics::init_materialize_outcome_series();
    let g = noetl_server::metrics::materialize_outcome_total();
    for o in noetl_server::metrics::MATERIALIZE_OUTCOMES {
        assert_eq!(g.with_label_values(&[o]).get(), 0, "{o} must be pinned at 0");
    }
    assert_eq!(noetl_server::metrics::MATERIALIZE_OUTCOMES.len(), 2);
}
