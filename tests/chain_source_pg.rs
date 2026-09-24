//! **The chain source, decode-tested against a LIVE PostgreSQL.**
//!
//! ⚠⚠ This file exists because of server#443. That change selected
//! `NULL::text AS content` into a `String` field. sqlx refuses that row **only
//! against a real database**, so it passed unit tests AND a throwaway-Postgres
//! proof that ran raw SQL — and then returned HTTP 500 on every
//! `/api/catalog/list` call in production. *A test that renders SQL is not a
//! test that decodes it.*
//!
//! So this test does not render SQL. It **decodes** it, from a real PostgreSQL,
//! into the real Rust types, including both nullable columns.
//!
//! ## Running it
//!
//! Skipped unless `NOETL_TEST_PG_URL` is set, so it never fails a build that has
//! no database. The kind fixture it was written against:
//!
//! ```text
//! kubectl --context kind-noetl port-forward -n noetl svc/chain-pg 55432:5432
//! NOETL_TEST_PG_URL=postgres://postgres:chainproof@127.0.0.1:55432/noetl
//! ```
//!
//! ⚠ `kubectl port-forward` **fails open**: a bound local port silently routes
//! to whatever cluster owns it, and a "kind" probe has run against PROD in this
//! program before. `--context` protects kubectl, **not** a client connecting to
//! localhost. So the first test asserts the fixture it expects is actually
//! there, and the harness that ran it killed the forward and confirmed the
//! probe died.

use noetl_server::chain_advance::{decide, AdvanceDecision};
use noetl_server::db::queries::event_chain::{
    is_terminal_event_type, read_chain, rows_to_chain_view, TERMINAL_EVENT_TYPES,
};

fn pg_url() -> Option<String> {
    std::env::var("NOETL_TEST_PG_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// ⚠⚠ **Skip only when the URL is UNSET. A set-but-unreachable URL must FAIL.**
///
/// The first draft returned `None` on a connection error and every test
/// silently returned — so with the port-forward killed the suite still reported
/// `ok`. That is a FALSE GREEN: "database absent" read identically to "database
/// correct", which is precisely the confusion this whole redesign is about.
/// Caught by running the negative control (kill the forward, confirm the probe
/// dies) — it did not die.
async fn pool() -> Option<sqlx::PgPool> {
    let Some(url) = pg_url() else {
        return None; // genuinely not configured: skip
    };
    match sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(&url)
        .await
    {
        Ok(p) => Some(p),
        Err(e) => panic!(
            "NOETL_TEST_PG_URL is SET but unreachable ({e}) — refusing to skip. A \
             skipped test reads identically to a passing one, and that is the \
             failure mode this suite exists to rule out."
        ),
    }
}

/// ⭐ **The identity check on the database itself.** Before believing anything
/// else, confirm we are talking to the fixture we think we are — because
/// port-forward fails open and a wrong-cluster reading looks exactly like a
/// right one.
#[tokio::test]
async fn the_database_we_reached_is_the_fixture_we_expect() {
    let Some(p) = pool().await else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };
    let counts: Vec<(i64, i64)> =
        sqlx::query_as("SELECT execution_id, count(*) FROM noetl.event GROUP BY 1 ORDER BY 1")
            .fetch_all(&p)
            .await
            .expect(
                "the fixture table must exist — if this fails, check WHICH cluster the \
                     port-forward actually reached",
            );
    assert_eq!(
        counts,
        vec![(1001, 7), (1002, 6), (1003, 3)],
        "not the expected fixture: got {counts:?}"
    );
}

/// ⭐⭐ **The decode test.** Both nullable columns come back as `Option<i64>`
/// from a real database. This is the assertion server#443 lacked.
#[tokio::test]
async fn nullable_columns_decode_as_option_from_a_real_database() {
    let Some(p) = pool().await else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };
    let rows = read_chain(&p, 1001, 1000)
        .await
        .expect("decode must succeed");
    assert_eq!(rows.len(), 7);

    // The root row has BOTH nullable columns NULL — the exact shape that
    // breaks a non-Option field, and only against a real database.
    let root = rows.iter().find(|r| r.event_id == 1).expect("root row");
    assert_eq!(root.prev_event_id, None, "a chain root's prev is NULL");
    assert_eq!(
        root.parent_execution_id, None,
        "a tree root's parent is NULL"
    );
    assert_eq!(root.event_type, "playbook.started");

    // A non-root row decodes its prev as Some.
    let second = rows.iter().find(|r| r.event_id == 2).expect("second row");
    assert_eq!(second.prev_event_id, Some(1));
    assert_eq!(second.execution_id, 1001);
}

/// Rows come back ascending, and bounded by the limit.
#[tokio::test]
async fn the_read_is_ordered_and_bounded() {
    let Some(p) = pool().await else {
        return;
    };
    let rows = read_chain(&p, 1001, 1000).await.unwrap();
    let ids: Vec<i64> = rows.iter().map(|r| r.event_id).collect();
    assert_eq!(ids, vec![1, 2, 3, 4, 5, 6, 7], "must be ascending");

    let capped = read_chain(&p, 1001, 3).await.unwrap();
    assert_eq!(capped.len(), 3, "the limit must bound the result set");
}

// ---------------------------------------------------------------------------
// ⭐⭐ The e2e property: the same decision, from a real database.
// ---------------------------------------------------------------------------

/// A COMPLETE chain from Postgres advances.
#[tokio::test]
async fn a_complete_chain_from_postgres_advances() {
    let Some(p) = pool().await else {
        return;
    };
    let rows = read_chain(&p, 1001, 1000).await.unwrap();
    let view = rows_to_chain_view(1001, rows);
    match decide(&view) {
        AdvanceDecision::Advance { from_event_id, .. } => {
            assert_eq!(from_event_id, "7", "the head is the last step.completed");
        }
        other => panic!("expected Advance from a complete chain, got {other:?}"),
    }
}

/// ⭐⭐⭐ **The proof this whole increment exists for.** A chain with a
/// DELIBERATELY TRUNCATED middle event (1002 is missing event 14) reports
/// `BlockedAtGap` **at the named key**, read from a real database.
///
/// Today this same execution is a no-op: the scan cannot build its state, so
/// the poller reports "did not advance" — indistinguishable from a finished
/// execution — and re-drives it forever, which is what the give-up cap exists
/// to bound.
#[tokio::test]
async fn a_truncated_chain_from_postgres_reports_blocked_at_the_named_key() {
    let Some(p) = pool().await else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };
    let rows = read_chain(&p, 1002, 1000).await.unwrap();
    assert_eq!(rows.len(), 6, "the fixture holds 6 of 7 events");
    assert!(
        !rows.iter().any(|r| r.event_id == 14),
        "event 14 must be absent — that is the truncation under test"
    );

    let view = rows_to_chain_view(1002, rows);
    match decide(&view) {
        AdvanceDecision::BlockedAtGap {
            missing_event_id,
            detected_at_event_id,
        } => {
            assert_eq!(missing_event_id, "14", "must NAME the missing key");
            assert_eq!(
                detected_at_event_id, "15",
                "and name where the dangling pointer was found"
            );
        }
        other => panic!(
            "a truncated chain must report BlockedAtGap at a named key, got {other:?} — \
             this is the distinction whose absence causes infinite re-driving"
        ),
    }
}

/// A chain ending in a terminal event is Terminal, not another indistinguishable
/// no-op.
#[tokio::test]
async fn a_terminal_chain_from_postgres_is_terminal() {
    let Some(p) = pool().await else {
        return;
    };
    let rows = read_chain(&p, 1003, 1000).await.unwrap();
    let view = rows_to_chain_view(1003, rows);
    match decide(&view) {
        AdvanceDecision::Terminal { at_event_id } => assert_eq!(at_event_id, "23"),
        other => panic!("expected Terminal, got {other:?}"),
    }
}

/// ⚠ Control: the three fixtures produce three DIFFERENT decisions. Today all
/// three produce the same boolean on the poller path.
#[tokio::test]
async fn the_three_fixtures_produce_three_different_decisions() {
    let Some(p) = pool().await else {
        return;
    };
    let mut labels = Vec::new();
    for exec in [1001i64, 1002, 1003] {
        let rows = read_chain(&p, exec, 1000).await.unwrap();
        labels.push(decide(&rows_to_chain_view(exec, rows)).label());
    }
    assert_eq!(
        labels,
        vec!["advance", "blocked_at_gap", "terminal"],
        "three situations must be three decisions, not one boolean"
    );
}

/// An unknown execution is Empty, not blocked — an absent probe is not a gap.
#[tokio::test]
async fn an_unknown_execution_is_empty_not_blocked() {
    let Some(p) = pool().await else {
        return;
    };
    let rows = read_chain(&p, 999_999, 1000).await.unwrap();
    assert!(rows.is_empty());
    assert_eq!(
        decide(&rows_to_chain_view(999_999, rows)),
        AdvanceDecision::Empty
    );
}

/// The terminal classification is a closed, asserted set — not prose.
#[test]
fn terminal_event_types_are_enumerated() {
    assert!(is_terminal_event_type("execution.completed"));
    assert!(is_terminal_event_type("playbook.failed"));
    assert!(!is_terminal_event_type("step.completed"));
    assert!(!is_terminal_event_type("command.issued"));
    assert_eq!(TERMINAL_EVENT_TYPES.len(), 5);
}

// ---------------------------------------------------------------------------
// The config gate: TWO independent switches, both default off.
// ---------------------------------------------------------------------------

use noetl_server::chain_advance::CHAIN_ADVANCE_ENV;
use noetl_server::db::queries::event_chain::{chain_source_from_env, CHAIN_SOURCE_ENV};

/// Serialised: these mutate two process-wide env vars.
static GATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn with_gates<T, Fut: std::future::Future<Output = T>>(
    advance: Option<&str>,
    source: Option<&str>,
    f: impl FnOnce() -> Fut,
) -> T {
    let _g = GATE_LOCK.lock().await;
    let (pa, ps) = (
        std::env::var(CHAIN_ADVANCE_ENV).ok(),
        std::env::var(CHAIN_SOURCE_ENV).ok(),
    );
    for (k, v) in [(CHAIN_ADVANCE_ENV, advance), (CHAIN_SOURCE_ENV, source)] {
        match v {
            Some(x) => unsafe { std::env::set_var(k, x) },
            None => unsafe { std::env::remove_var(k) },
        }
    }
    let out = f().await;
    for (k, v) in [(CHAIN_ADVANCE_ENV, pa), (CHAIN_SOURCE_ENV, ps)] {
        match v {
            Some(x) => unsafe { std::env::set_var(k, x) },
            None => unsafe { std::env::remove_var(k) },
        }
    }
    out
}

/// ⭐⭐ **The safety property, stated as a test.** Both gates must be on before
/// a chain source exists. Any other combination leaves the poller on today's
/// path — which is what makes merging and deploying this inert.
#[tokio::test]
async fn a_source_is_built_only_when_BOTH_gates_are_on() {
    let Some(p) = pool().await else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };

    // The full truth table.
    for (advance, source, expect_some) in [
        (None, None, false),                      // today
        (Some("true"), None, false),              // advance on, no source
        (None, Some("postgres"), false),          // source named, advance off
        (Some("false"), Some("postgres"), false), // explicitly off
        (Some("true"), Some("postgres"), true),   // ⭐ the only combination
    ] {
        let got = with_gates(advance, source, || async {
            chain_source_from_env(&p).is_some()
        })
        .await;
        assert_eq!(
            got, expect_some,
            "advance={advance:?} source={source:?} should yield some={expect_some}"
        );
    }
}

/// ⚠ A typo in the source name must not enable it.
#[tokio::test]
async fn an_unrecognised_source_resolves_to_none() {
    let Some(p) = pool().await else {
        return;
    };
    for junk in ["", " ", "postgress", "pgsql", "sql", "true", "1", "ehdb"] {
        let got = with_gates(Some("true"), Some(junk), || async {
            chain_source_from_env(&p).is_some()
        })
        .await;
        assert!(!got, "source {junk:?} must not build a chain source");
    }
    // ⚠ Control: the accepted spellings DO build one, or the test above is
    // satisfied by a function that always returns None.
    for ok in ["postgres", "pg", "  POSTGRES  "] {
        let got = with_gates(Some("true"), Some(ok), || async {
            chain_source_from_env(&p).is_some()
        })
        .await;
        assert!(got, "source {ok:?} must build a chain source");
    }
}

/// ⭐ End to end through the trait: with both gates on, the source reads the
/// truncated fixture from Postgres and the poller action is BlockedAtGap with
/// the budget untouched.
#[tokio::test]
async fn both_gates_on_yields_blocked_at_gap_with_the_budget_untouched() {
    let Some(p) = pool().await else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };
    let action = with_gates(Some("true"), Some("postgres"), || async {
        let src = chain_source_from_env(&p).expect("both gates on");
        assert_eq!(src.source_name(), "postgres");
        noetl_server::chain_advance::poller_action(src.as_ref(), 1002, 23).await
    })
    .await
    .expect("the chain path must engage");

    assert_eq!(action.decision, "blocked_at_gap");
    assert_eq!(action.waiting_on.as_deref(), Some("14"));
    assert_eq!(action.noops, 23, "the budget must be UNTOUCHED, not 24");
    assert!(!action.give_up);
}

/// And the complete fixture advances, resetting the budget.
#[tokio::test]
async fn both_gates_on_yields_advance_on_a_complete_chain() {
    let Some(p) = pool().await else {
        return;
    };
    let action = with_gates(Some("true"), Some("postgres"), || async {
        let src = chain_source_from_env(&p).unwrap();
        noetl_server::chain_advance::poller_action(src.as_ref(), 1001, 99).await
    })
    .await
    .expect("engaged");
    assert_eq!(action.decision, "advance");
    assert_eq!(action.noops, 0, "progress resets the budget");
}

/// ⚠⚠ **A read FAILURE must fall through, not read as an empty chain.**
///
/// Added because a mutant that returned `Some(empty_view)` on a read error
/// SURVIVED the battery: nothing exercised the failure path. The distinction is
/// not cosmetic — an empty view makes `decide()` return `Empty`, which is a
/// DECISION MADE ON BAD DATA. `None` falls through to the reconcile path for
/// that tick, which costs one tick of old behaviour and asserts nothing false.
///
/// Uses a real database that genuinely lacks the table, so the failure is a
/// real sqlx error rather than a stubbed one.
#[tokio::test]
async fn a_read_failure_falls_through_rather_than_reporting_an_empty_chain() {
    let Some(base) = pg_url() else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };
    // Same server, a database with no `noetl` schema at all.
    let broken_url = base
        .rsplit_once('/')
        .map(|(h, _)| format!("{h}/noetl_noschema"));
    let Some(broken_url) = broken_url else {
        panic!("could not derive the no-schema URL from {base}");
    };
    let Ok(broken) = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(&broken_url)
        .await
    else {
        panic!("the no-schema database must exist for this test to mean anything");
    };

    // Control: the read genuinely fails there.
    assert!(
        read_chain(&broken, 1001, 10).await.is_err(),
        "the fixture for this test is wrong — the read must actually FAIL"
    );

    use noetl_server::chain_advance::ChainSource;
    let src = noetl_server::db::queries::event_chain::PgChainSource::new(broken, 10);
    assert!(
        src.chain_for(1001).await.is_none(),
        "a failed read must yield None (fall through), never Some(empty) — an \
         empty view would make decide() return Empty, a decision on bad data"
    );
}

/// ⚠ Control for the test above: the *working* pool does yield `Some`. Without
/// it, "returns None on failure" is satisfied by a source that always returns
/// None.
#[tokio::test]
async fn a_working_source_does_yield_some() {
    let Some(p) = pool().await else {
        return;
    };
    use noetl_server::chain_advance::ChainSource;
    let src = noetl_server::db::queries::event_chain::PgChainSource::new(p, 100);
    let view = src
        .chain_for(1001)
        .await
        .expect("a healthy read must yield Some");
    assert_eq!(view.events.len(), 7);
}
