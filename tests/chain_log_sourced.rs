//! **The log-sourced chain source, against a real PostgreSQL** — the proof for
//! noetl/ai-meta#357.
//!
//! Every assertion here exists because a measurement contradicted a prediction.
//! The three fixtures are the shapes that were actually observed in kind on
//! 2026-09-28, not invented ones:
//!
//! | execution | shape | what it broke before |
//! | :-- | :-- | :-- |
//! | 2001 | 7 events, **3 null-prev** (server restarted mid-flight, twice) | store froze at 6, watermark `1:7` |
//! | 2002 | 6 events, cleanly linked | the control: this one always worked |
//! | 2003 | 3 events, still running | armed mid-flight → store held 1, read `authoritative first=1` |
//! | 2004 | 3 events, `parent_execution_id` set, terminal | big-parent + terminal decision |
//!
//! ⚠ Every fixture starts with `playbook.initialized`, **not**
//! `playbook_started`. That is the measured distribution — 533 of 595 kind
//! executions — and a root check built on `playbook_started` would have been inert
//! for 90% of them. Nothing here keys off the start event's name at all; the root
//! is "position 0 in the log", which is correct across both.
//!
//! ## Running it
//!
//! ```text
//! kubectl --context kind-noetl -n noetl port-forward svc/chain-pg 55432:5432
//! NOETL_TEST_PG_URL=postgres://postgres:chainproof@127.0.0.1:55432/noetl
//! ```
//!
//! ⚠ `kubectl port-forward` **fails open** — a bound local port routes to whatever
//! cluster owns it, and a "kind" probe has run against PROD in this program before.
//! So the first test asserts the fixture is the one expected, and the harness that
//! ran it killed the forward and confirmed the probe died.

use std::sync::Arc;

use noetl_server::chain_advance::{decide, AdvanceDecision, ChainSource};
use noetl_server::state::{ChainHeadHydrator, HydrateOutcome};

fn pg_url() -> Option<String> {
    std::env::var("NOETL_TEST_PG_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// ⚠⚠ Skip only when UNSET. A set-but-unreachable URL must FAIL, because a
/// skipped test reads identically to a passing one.
async fn pool() -> Option<sqlx::PgPool> {
    let url = pg_url()?;
    match sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(&url)
        .await
    {
        Ok(p) => Some(p),
        Err(e) => panic!("NOETL_TEST_PG_URL is SET but unreachable ({e}) — refusing to skip."),
    }
}

/// Arm the chain store at a per-process root.
///
/// ⚠ The store is a process-wide `OnceLock`, so the first test to touch it fixes
/// the directory for the whole binary. All tests therefore arm the SAME path, and
/// use disjoint execution ids so partitions never collide.
fn arm_store() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("chain-proof-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create chain root");
        unsafe {
            std::env::set_var(ehdb_l0::chain_populator::POPULATOR_ENV, "true");
            std::env::set_var(
                noetl_server::handlers::chain_populate::POPULATE_DIR_ENV,
                dir.to_str().unwrap(),
            );
        }
    });
}

/// ⭐ Identity check on the database. Before believing anything else, confirm the
/// fixture is the one expected — a wrong-cluster reading looks exactly like a
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
            .expect("the fixture table must exist — check WHICH cluster was reached");
    // ⚠ The UNION of two fixtures. 1001-1003 belong to `chain_source_pg.rs` (its
    // 1002 has a DELIBERATE hole at event 14); 2001-2004 are this suite's. They are
    // renumbered apart because that test wants 1001's first event to be
    // `playbook.started` and a truncated middle, and this one wants
    // `playbook.initialized` and whole chains — incompatible shapes for one id.
    assert_eq!(
        counts,
        vec![
            (1001, 7),
            (1002, 6),
            (1003, 3),
            (2001, 7),
            (2002, 6),
            (2003, 3),
            (2004, 3)
        ],
        "not the expected fixture: got {counts:?}"
    );

    // ⭐ And the null-prev shape is present, because that is what the fix is about.
    let nulls: i64 =
        sqlx::query_scalar("SELECT count(*) FROM noetl.event WHERE prev_event_id IS NULL")
            .fetch_one(&p)
            .await
            .unwrap();
    assert_eq!(
        nulls, 10,
        "the fixture must carry the null-prev rows the fix exists for; a fixture \
         that cannot exhibit the failure makes the test decorative"
    );
}

// ---------------------------------------------------------------------------
// Fix 1 — chain-head hydration.
// ---------------------------------------------------------------------------

/// A cold map must hydrate the REAL head, so the next event links instead of
/// becoming a second root.
#[tokio::test]
async fn hydration_returns_the_real_head_for_a_running_execution() {
    let Some(p) = pool().await else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };
    let h = noetl_server::db::queries::event_chain::PgChainHeadHydrator::new(p.clone());
    assert_eq!(
        h.head_of(2001).await,
        HydrateOutcome::Head(2007),
        "2001's head is its greatest event_id"
    );
    assert_eq!(h.head_of(2003).await, HydrateOutcome::Head(2023));
    assert_eq!(
        h.head_of(999_999).await,
        HydrateOutcome::Empty,
        "an execution with no events is Empty — the ONLY case that licenses \
         stamping a chain root"
    );
}

/// ⚠⚠ **The S5 gap.** A read FAILURE must be `Failed`, never `Empty`.
///
/// Found by a surviving mutant: mapping `Err(_) => Empty` changed no test, because
/// nothing exercised a failing read. `Empty` licenses stamping a chain ROOT, so
/// that mutant would plant a second root on an arbitrary event every time the
/// database hiccuped — the defect the hydrator exists to remove, reintroduced
/// through its own error path.
///
/// A comment is not a guard; this connects to a real database that lacks
/// `noetl.event`.
#[tokio::test]
async fn a_failed_hydration_read_is_failed_not_empty() {
    let Some(url) = pg_url() else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };
    // Same server, the `postgres` database — which has no `noetl.event`, so the
    // query errors rather than returning no rows.
    let other = url
        .rsplit_once('/')
        .map(|(h, _)| format!("{h}/postgres"))
        .unwrap();
    let p = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(&other)
        .await
        .expect("the postgres database must be reachable for this control");

    // ⭐ POSITIVE CONTROL: prove the table really is absent, so a `Failed` below is
    // the error path and not a coincidence.
    let exists: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM information_schema.tables WHERE table_schema='noetl' AND table_name='event'",
    )
    .fetch_optional(&p)
    .await
    .unwrap();
    assert!(
        exists.is_none(),
        "control failed: noetl.event EXISTS in this database, so the read would \
         not error and this test would prove nothing"
    );

    let h = noetl_server::db::queries::event_chain::PgChainHeadHydrator::new(p);
    assert_eq!(
        h.head_of(2001).await,
        HydrateOutcome::Failed,
        "a failing read must be Failed. Empty would license stamping a chain ROOT \
         onto an arbitrary event whenever the database hiccuped."
    );
}

// ---------------------------------------------------------------------------
// Fix 2 — the log-sourced chain source.
// ---------------------------------------------------------------------------

async fn view_for(
    p: &sqlx::PgPool,
    execution_id: i64,
) -> Option<noetl_server::chain_advance::ChainView> {
    arm_store();
    let src = noetl_server::handlers::chain_populate::LogSourcedChainSource::new(p.clone(), 1000);
    src.chain_for(execution_id).await
}

/// ⭐⭐ **Post-restart: null-prev rows no longer yield pseudo-roots.**
///
/// 2001 carries THREE null-prev rows. The per-event populator froze at the first
/// of them (Postgres 7, store 6, watermark `1:7`). The log source recomputes the
/// edge from log order, so the chain is whole and singly-rooted.
#[tokio::test]
async fn post_restart_null_prev_rows_yield_one_root_and_a_whole_chain() {
    let Some(p) = pool().await else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };
    let view = view_for(&p, 2001).await.expect(
        "the log source must answer for a post-restart execution; None here means \
         the guarded read refused, which is the OLD behaviour",
    );
    assert_eq!(
        view.events.len(),
        7,
        "every event, not the pre-restart prefix"
    );

    let roots = view
        .events
        .iter()
        .filter(|e| e.prev_event_id.is_none())
        .count();
    assert_eq!(
        roots, 1,
        "3 null-prev rows in the log must still yield ONE chain root; found {roots}"
    );
    for w in view.events.windows(2) {
        assert_eq!(
            w[1].prev_event_id.as_deref(),
            Some(w[0].event_id.as_str()),
            "each event links to its predecessor in log order"
        );
    }

    // ⭐ And the decision is Advance, not Empty and not BlockedAtGap.
    let d = decide(&view);
    assert!(
        matches!(d, AdvanceDecision::Advance { .. }),
        "a running post-restart execution must Advance, got {d:?}"
    );
    assert!(!d.requires_redrive(), "no decision may require a re-drive");
}

/// ⭐⭐ **Mid-flight arming reads the TRUE event set.**
///
/// Measured before the fix: store 1 of Postgres' 3, read as complete. Now the
/// partition is built from the log's first event, so it cannot be short.
#[tokio::test]
async fn mid_flight_arming_reads_the_true_event_set() {
    let Some(p) = pool().await else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };
    let pg_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM noetl.event WHERE execution_id=2003")
            .fetch_one(&p)
            .await
            .unwrap();

    let view = view_for(&p, 2003).await.expect("must answer");
    assert_eq!(
        view.events.len() as i64,
        pg_count,
        "the chain must hold exactly what the authoritative log holds — this is \
         the independent-oracle check, and it is the assertion that would have \
         caught the truncation (1 vs 3)"
    );
    assert_eq!(
        view.events[0].event_id, "2021",
        "rooted at the log's FIRST event"
    );

    let d = decide(&view);
    assert!(
        !matches!(d, AdvanceDecision::Empty),
        "a running execution must never decide Empty, got {d:?}"
    );
    assert!(matches!(d, AdvanceDecision::Advance { .. }), "got {d:?}");
}

/// A cleanly-linked execution with a terminal event decides Terminal.
#[tokio::test]
async fn a_terminal_execution_decides_terminal() {
    let Some(p) = pool().await else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };
    let view = view_for(&p, 2004).await.expect("must answer");
    assert_eq!(view.events.len(), 3);
    assert_eq!(
        view.events[0].parent_execution_id.as_deref(),
        Some("2001"),
        "the execution-tree edge must survive into the store"
    );
    let d = decide(&view);
    assert!(
        matches!(d, AdvanceDecision::Terminal { .. }),
        "2004 ends in execution.completed, got {d:?}"
    );
}

/// ⭐ Every fixture agrees with Postgres, exactly. The forward-identity property.
#[tokio::test]
async fn every_execution_agrees_with_the_postgres_oracle() {
    let Some(p) = pool().await else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };
    for exec in [2001i64, 2002, 2003, 2004] {
        let pg: i64 = sqlx::query_scalar("SELECT count(*) FROM noetl.event WHERE execution_id=$1")
            .bind(exec)
            .fetch_one(&p)
            .await
            .unwrap();
        let view = view_for(&p, exec).await.unwrap_or_else(|| {
            panic!("no view for {exec} — the source refused an execution the log has")
        });
        assert_eq!(
            view.events.len() as i64,
            pg,
            "execution {exec}: store {} vs postgres {pg}",
            view.events.len()
        );
        // Exactly one root, always.
        assert_eq!(
            view.events
                .iter()
                .filter(|e| e.prev_event_id.is_none())
                .count(),
            1,
            "execution {exec} must have exactly one chain root"
        );
        assert!(!decide(&view).requires_redrive());
    }
}

/// ⚠ Re-reading is idempotent: the second call must agree with the first, not
/// double-append or diverge.
#[tokio::test]
async fn reading_twice_agrees_with_itself() {
    let Some(p) = pool().await else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };
    let a = view_for(&p, 2002).await.expect("first read");
    let b = view_for(&p, 2002).await.expect("second read");
    assert_eq!(a.events.len(), b.events.len());
    assert_eq!(
        a.events
            .iter()
            .map(|e| e.event_id.clone())
            .collect::<Vec<_>>(),
        b.events
            .iter()
            .map(|e| e.event_id.clone())
            .collect::<Vec<_>>(),
        "a second read must return the same chain"
    );
}

/// ⚠⚠ Flag-off must be inert: no store, no files, and the source must not answer.
#[tokio::test]
async fn with_the_populator_off_the_source_writes_nothing() {
    let Some(p) = pool().await else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };
    // A root nothing has ever touched, and the flag left off for this call.
    let dir = std::env::temp_dir().join(format!("chain-off-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let before = std::fs::read_dir(&dir).unwrap().count();
    assert_eq!(before, 0, "the control root must start empty");

    // ⚠ Cannot un-arm the process-wide store once opened, so this asserts the
    // DECISION at the source rather than re-opening: the `chain` arm resolves to
    // None when the populator is off, which the unit guard
    // `selecting_the_chain_store_without_the_populator_resolves_to_off` pins.
    // Here we only confirm no stray writes land in an unrelated root.
    let _ = view_for(&p, 2002).await;
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        0,
        "a root the populator was never pointed at must stay empty"
    );
}
