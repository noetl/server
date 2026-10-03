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
    // ⚠ Scoped to the DECLARED fixture executions. An unscoped count made this
    // guard fire when a later test added its own execution — correctly, but for an
    // irrelevant reason. The property is "the fixture is what it says", not "no
    // other row exists in the database".
    let counts: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT execution_id, count(*) FROM noetl.event \
         WHERE execution_id IN (1001,1002,1003,2001,2002,2003,2004) \
         GROUP BY 1 ORDER BY 1",
    )
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
    let nulls: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM noetl.event WHERE prev_event_id IS NULL \
         AND execution_id IN (1001,1002,1003,2001,2002,2003,2004)",
    )
    .fetch_one(&p)
    .await
    .unwrap();
    assert_eq!(
        nulls, 9,
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
    // ⭐⭐ 2001 is the fixture's POST-RESTART shape: three NULL-prev rows (2001, 2005,
    // 2006), so three roots and three tips (2004, 2005, 2007). The hydrator must now
    // SEE that and say so.
    //
    // ⚠ This assertion used to read `Head(2007)` with the justification "2001's head
    // is its greatest event_id". That was true of the old implementation and it was
    // the bug: `max(event_id)` cannot distinguish a clean chain from a forked one, so
    // it reported the defect the fixture exists to encode as perfectly healthy
    // (noetl/ai-meta#362). The CHOSEN head is unchanged, so behaviour is compatible —
    // what changed is that the fork is now countable.
    assert_eq!(
        h.head_of(2001).await,
        HydrateOutcome::HeadAmbiguous {
            chosen: 2007,
            tips: 3
        },
        "the post-restart fixture has 3 tips; the hydrator must report the fork AND \
         still return a head, because stamping NULL here would add a fourth root"
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
// The capital is deliberate and load-bearing: it is the word that makes the
// test name state its own verdict. Renaming it to snake case would make the
// name read as the opposite claim at a glance.
#[allow(non_snake_case)]
async fn a362_a_post_restart_multi_root_execution_is_REFUSED_not_recovered() {
    let Some(p) = pool().await else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };

    // ⚠⚠⚠ THIS TEST ASSERTED THE OPPOSITE, AND THE REVERSAL IS THE POINT.
    //
    // It used to require that the source ANSWER for 2001 — a post-restart execution
    // with three NULL-prev rows — on the reasoning that "the log source recomputes
    // the edge from log order, so the chain is whole and singly-rooted". That was
    // noetl/ai-meta#357's capability and it worked.
    //
    // noetl/ai-meta#362 removes it deliberately. Recomputing the edge from log order
    // means trusting `ORDER BY event_id`, and a snowflake id is minted before the
    // insert, so an event can commit into the MIDDLE of that order. Recovering a
    // forked chain by position is the same mechanism that produced false divergence
    // on prod — it was never recovery, it was a guess that happened to look right.
    //
    // ⚠ THE COST IS REAL AND MUST NOT BE GLOSSED: an execution that has ever been
    // re-rooted is now UNSERVABLE from the chain store. It falls through to Postgres,
    // which is correct and safe, but the store covers fewer executions until the
    // WRITE path stops producing forks (the hydrator fix, #362(b)). In the measured
    // corpus that is 2 of 63 recent executions plus the whole legacy era.
    //
    // So "comparator green" is not sufficient on its own: a source that refuses
    // everything also reports zero divergence. Coverage has to be read alongside it.
    let view = view_for(&p, 2001).await;
    assert!(
        view.is_none(),
        "a 3-root execution must be REFUSED. Nothing can say which root is real, so \
         answering means guessing — and the guess is what #362 fixes. Got a view with \
         {:?} events.",
        view.map(|v| v.events.len())
    );

    // ⭐ And the positive control: a cleanly linked execution IS served, so the
    // refusal above is about the fork and not about the source being broken.
    let ok = view_for(&p, 2002)
        .await
        .expect("2002 is cleanly linked and MUST be served; if this is None the \
                 source is broken rather than discriminating");
    assert_eq!(ok.events.len(), 6, "2002 has 6 events");
    let roots = ok
        .events
        .iter()
        .filter(|e| e.prev_event_id.is_none())
        .count();
    assert_eq!(roots, 1, "exactly one root, which is the invariant");
}
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
    // ⚠ 2001 is EXCLUDED and checked separately: it carries three NULL-prev rows, so
    // under link-defined ordering it is refused rather than reconstructed
    // (noetl/ai-meta#362). Leaving it in this loop would make the oracle assert the
    // old, position-based recovery.
    assert!(
        view_for(&p, 2001).await.is_none(),
        "the multi-root execution must be refused, not reconciled against Postgres"
    );
    for exec in [2002i64, 2003, 2004] {
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

// ---------------------------------------------------------------------------
// ⭐⭐ The repoint itself: the POLLER path, with the source selected by config.
// ---------------------------------------------------------------------------

/// ⚠ `cargo test` does not serialise tests, and these mutate env.
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// ⭐⭐ **The repoint proof.** With all THREE gates on, `chain_source_from_env`
/// yields the log-sourced store and `poller_action` decides from it.
///
/// This is the reachability half. Correctness is proven above by calling the
/// source directly; this asserts that a server configured the way a repoint would
/// configure it actually routes the poller's decision through that source — which
/// is a different question, and the one this program keeps finding answered wrong.
#[tokio::test]
async fn the_poller_decides_from_the_chain_store_when_configured_to() {
    let Some(p) = pool().await else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };
    let _g = ENV_LOCK.lock().await;
    arm_store();
    unsafe {
        std::env::set_var(noetl_server::chain_advance::CHAIN_ADVANCE_ENV, "true");
        std::env::set_var(
            noetl_server::db::queries::event_chain::CHAIN_SOURCE_ENV,
            "chain",
        );
    }

    let src = noetl_server::db::queries::event_chain::chain_source_from_env(&p)
        .expect("all three gates on must yield a source");
    assert_eq!(
        src.source_name(),
        "log_sourced_chain_store",
        "the configured source must be the chain store, not the Postgres scan"
    );

    // 2003 is running with 3 events. The budget must be RESET by an advance, and
    // the decision must not be Empty.
    let action = noetl_server::chain_advance::poller_action(src.as_ref(), 2003, 17)
        .await
        .expect("the chain path must engage — None means the source could not answer");
    assert_eq!(action.decision, "advance", "got {action:?}");
    assert!(!action.give_up);
    assert_eq!(
        action.noops, 0,
        "an advance resets the budget; a stale budget is how the give-up cap \
         misfires"
    );

    // 2004 is terminal.
    let t = noetl_server::chain_advance::poller_action(src.as_ref(), 2004, 5)
        .await
        .expect("terminal execution must engage the chain path");
    assert_eq!(t.decision, "terminal", "got {t:?}");

    // ⚠⚠ 2001 — three NULL-prev rows — must NOT engage the chain path at all.
    //
    // This asserted `advance` under noetl/ai-meta#357, which rebuilt the chain from
    // log order. #362 refuses a forked execution instead: falling through to the
    // legacy path is safe, and inventing an order for a chain whose root is unknowable
    // is not. The poller therefore gets `None` and uses today's path.
    assert!(
        noetl_server::chain_advance::poller_action(src.as_ref(), 2001, 9)
            .await
            .is_none(),
        "a multi-root execution must fall through, not advance off a guessed order"
    );

    unsafe {
        std::env::remove_var(noetl_server::db::queries::event_chain::CHAIN_SOURCE_ENV);
        std::env::remove_var(noetl_server::chain_advance::CHAIN_ADVANCE_ENV);
    }
}

/// ⚠⚠ And the third gate, through the real resolver: source=chain with the
/// populator OFF must yield NO source, not one reading an unpopulated store.
#[tokio::test]
async fn the_resolver_refuses_the_chain_store_without_the_populator() {
    let Some(p) = pool().await else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };
    let _g = ENV_LOCK.lock().await;
    unsafe {
        std::env::set_var(noetl_server::chain_advance::CHAIN_ADVANCE_ENV, "true");
        std::env::set_var(
            noetl_server::db::queries::event_chain::CHAIN_SOURCE_ENV,
            "chain",
        );
        std::env::remove_var(ehdb_l0::chain_populator::POPULATOR_ENV);
    }
    let got = noetl_server::db::queries::event_chain::chain_source_from_env(&p);
    let refused = got.is_none();
    // Restore before asserting, so a failure cannot leak the unset flag into
    // whichever test runs next.
    unsafe {
        std::env::set_var(ehdb_l0::chain_populator::POPULATOR_ENV, "true");
        std::env::remove_var(noetl_server::db::queries::event_chain::CHAIN_SOURCE_ENV);
        std::env::remove_var(noetl_server::chain_advance::CHAIN_ADVANCE_ENV);
    }
    assert!(
        refused,
        "source=chain with the populator OFF returned a source. It would read an \
         UNPOPULATED store, and decide() would report Empty on running \
         executions — the exact cliff noetl/ai-meta#357 records."
    );
}

// ===========================================================================
// noetl/ai-meta#360 — the BURST proof. Concurrent chain_for on one execution.
// ===========================================================================

/// ⭐⭐ **The condition that broke the prod ramp, reproduced against a real
/// database: concurrent readers racing a GROWING log.**
///
/// `chain_for` reads the event log and THEN takes the process-global store lock.
/// Under concurrent reads of one execution, a task that read first can reach the
/// lock second and find the store ahead of its snapshot. Before #360 that was
/// classed `Diverged`: 5 in 79 comparisons (6.3%) on prod, which failed the gate.
///
/// ⚠⚠ WHAT THIS TEST PROVES, AND WHAT IT DOES NOT — read before trusting a pass.
///
/// It is a concurrency **soak** on the shipped build: 800 concurrent `chain_for`
/// calls on a log a writer is appending to, asserting 0 divergence and that every
/// refusal has a named reason. On the shipped build it reports `stale_log=0`, so a
/// pass here does **not** discriminate the fix from the defect.
///
/// The mechanism was nevertheless CONFIRMED, on an intermediate build that did not
/// yet hold one lock across populate and the guarded read:
///
/// | build | stale_log | diverged | refused |
/// | :-- | --: | --: | --: |
/// | pre-fix classification (`Diverged` on a behind snapshot) | 0 | **10** | 12 |
/// | fixed | 9 | 0 | 0 |
///
/// 10 false divergences in 800 — the same mechanism and direction as the prod
/// ramp's 5 in 79. So the race is real and reachable by concurrent reads.
///
/// ⭐ The shipped build then reports `stale_log=0` *and* the pre-fix classification
/// reports `diverged=0`, which is the interesting part: making populate and the
/// guarded read ONE lock hold lengthens the critical section enough that readers
/// queue in read order, so the reordering that creates a behind snapshot largely
/// stops happening at this cadence. That is a real improvement, and it is also why
/// this test can no longer discriminate — so the discriminating evidence lives in
/// the deterministic tests instead:
///
/// - `ehdb-l0`'s `a360_a_snapshot_behind_the_store_is_stale_not_diverged` — the
///   classification, with its own RED control.
/// - `a360_a_store_ahead_of_a_log_that_does_not_catch_up_still_reports_diverged` —
///   the #358 escalation; its RED control gives `length_disagreement+1` instead of
///   `diverged+1`.
///
/// Earlier versions of THIS test were decorative, which is worth recording: the
/// first fired 84 readers at a *static* fixture, where no snapshot can be behind;
/// a later one read process-global counters as absolutes and reported a
/// neighbouring test's legitimate divergence as its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a360_burst_concurrent_readers_racing_a_growing_log() {
    let Some(p) = pool().await else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };
    arm_store();
    let exec = 2010i64;

    // A clean, growing execution of our own.
    sqlx::query("DELETE FROM noetl.event WHERE execution_id = $1")
        .bind(exec)
        .execute(&p)
        .await
        .expect("clear");
    let cid: i64 = sqlx::query_scalar("SELECT catalog_id FROM noetl.catalog LIMIT 1")
        .fetch_one(&p)
        .await
        .expect("a catalog row must exist");
    // Seed the root so readers have something from the first tick.
    sqlx::query(
        "INSERT INTO noetl.event (event_id, execution_id, catalog_id, event_type, status) \
         VALUES ($1,$2,$3,'playbook.initialized','initialized')",
    )
    .bind(20_100_000i64)
    .bind(exec)
    .bind(cid)
    .execute(&p)
    .await
    .expect("seed");

    // ⚠⚠ Counters are PROCESS-GLOBAL, so this must measure its own DELTA. Reading
    // absolutes made this test report `diverged=1` that belonged to
    // `a360_a_store_ahead_of_a_log_that_does_not_catch_up_still_reports_diverged`,
    // running concurrently in the same process — a neighbouring test's legitimate
    // divergence read as a false divergence here. Same class as the fixture
    // pollution this suite's identity check exists for.
    let stale0 = noetl_server::metrics::chain_populate_total()
        .with_label_values(&["stale_log"])
        .get();
    let diverged0 = noetl_server::metrics::chain_populate_total()
        .with_label_values(&["diverged"])
        .get();
    // ⚠ Every refusal reason, so a refusal is DIAGNOSED rather than just counted.
    // A bare `refused == 0` failed once with no way to tell a transient DB error
    // from a residual #360 symptom, which is the same "a number with no cause is
    // not evidence" problem this program keeps meeting.
    const REASONS: [&str; 6] = [
        "log_read_failed",
        "log_truncated",
        "populate_failed",
        "guarded_read_refused",
        "guarded_read_failed",
        "length_disagreement",
    ];
    let reasons0: Vec<u64> = REASONS
        .iter()
        .map(|r| {
            noetl_server::metrics::chain_populate_total()
                .with_label_values(&[r])
                .get()
        })
        .collect();

    // The WRITER: appends while the readers run. This is what creates snapshots of
    // differing length, which is what creates the race.
    let wp = p.clone();
    let writer = tokio::spawn(async move {
        for i in 1..=400i64 {
            // ⚠ LINKED. Seeding unlinked events makes every row a ROOT, so
            // `order_by_links` correctly refuses the whole execution as
            // `multiple_roots` and the burst measures a refusal path instead of the
            // race it exists for (noetl/ai-meta#362).
            let _ = sqlx::query(
                "INSERT INTO noetl.event (event_id, execution_id, catalog_id, event_type, status, \
                 prev_event_id) VALUES ($1,$2,$3,'step.enter','PENDING',$4)",
            )
            .bind(20_100_000i64 + i)
            .bind(exec)
            .bind(cid)
            .bind(20_100_000i64 + i - 1)
            .execute(&wp)
            .await;
        }
    });

    // The READERS: many concurrent chain_for on the SAME execution.
    let mut handles = Vec::new();
    for _ in 0..32 {
        let pool = p.clone();
        handles.push(tokio::spawn(async move {
            let mut answered = 0usize;
            let mut refused = 0usize;
            for _ in 0..25 {
                let src = noetl_server::handlers::chain_populate::LogSourcedChainSource::new(
                    pool.clone(),
                    1000,
                );
                match src.chain_for(exec).await {
                    Some(_) => answered += 1,
                    None => refused += 1,
                }
                tokio::task::yield_now().await;
            }
            (answered, refused)
        }));
    }

    let mut answered = 0usize;
    let mut refused = 0usize;
    for h in handles {
        let (a, r) = h.await.expect("reader must not panic");
        answered += a;
        refused += r;
    }
    writer.await.expect("writer must not panic");

    // ⚠ Clean up after ourselves. The fixture-identity guard counts the declared
    // executions, and leaving synthetic rows behind is how a suite starts failing
    // for reasons that have nothing to do with what it tests.
    sqlx::query("DELETE FROM noetl.event WHERE execution_id = $1")
        .bind(exec)
        .execute(&p)
        .await
        .expect("cleanup");

    let total = answered + refused;
    println!(
        "  burst vs GROWING log: {total} concurrent chain_for calls -> \
         answered={answered} refused={refused}"
    );
    assert_eq!(total, 800, "denominator must be 32 readers x 25 reads");

    // ⚠⚠ FIRST: did the race even happen? A burst that produced ZERO stale
    // snapshots cannot discriminate the fix from the defect — and an earlier
    // version of this test passed with the PRE-FIX classification restored for
    // exactly that reason. So count the outcome and fail if the condition never
    // arose: a test that cannot exhibit the failure is decorative.
    let stale = noetl_server::metrics::chain_populate_total()
        .with_label_values(&["stale_log"])
        .get()
        - stale0;
    let diverged = noetl_server::metrics::chain_populate_total()
        .with_label_values(&["diverged"])
        .get()
        - diverged0;
    println!("  outcomes (DELTA for this burst): stale_log={stale} diverged={diverged}");
    // ⚠ `stale == 0` is EXPECTED on the shipped build (see the doc comment): the
    // atomic lock hold suppresses the reordering that creates a behind snapshot. It
    // is reported rather than asserted in either direction — asserting `> 0` would
    // fail the suite for a condition the fix itself makes rare, and asserting `== 0`
    // would fail if scheduling ever produced one, which is legal and benign.
    if stale == 0 {
        println!(
            "  (no stale snapshot arose — expected on this build; this run is a \
             soak. The discriminating evidence is in the deterministic tests.)"
        );
    }
    assert_eq!(
        diverged, 0,
        "{diverged} genuine divergences during a concurrent burst. A snapshot behind \
         the store is staleness, not divergence (noetl/ai-meta#360)."
    );

    // And the refusals, BY REASON — because which reason it is changes the verdict.
    let deltas: Vec<(&str, u64)> = REASONS
        .iter()
        .zip(reasons0.iter())
        .map(|(r, &before)| {
            (
                *r,
                noetl_server::metrics::chain_populate_total()
                    .with_label_values(&[r])
                    .get()
                    - before,
            )
        })
        .collect();
    println!("  refusals by reason (delta): {deltas:?}");

    // The #360 symptoms specifically. `guarded_read_refused` and
    // `length_disagreement` are how the false alarm shows up DOWNSTREAM when the
    // classification is right but coverage is stale, so they must be zero.
    for (reason, d) in &deltas {
        if matches!(*reason, "guarded_read_refused" | "length_disagreement") {
            assert_eq!(
                *d, 0,
                "{d} refusals for {reason} during a concurrent burst against a growing \
                 log. That is the noetl/ai-meta#360 false alarm surfacing downstream: \
                 the partition must stay servable while a snapshot is merely behind. \
                 Full breakdown: {deltas:?}"
            );
        }
    }

    // ⚠ `log_read_failed` is a genuine transient (DB contention under 32 concurrent
    // readers plus a tight-loop writer) and is NOT this fix's business, so it is
    // tolerated in small numbers and REPORTED rather than asserted to zero — a test
    // that flakes on unrelated contention gets muted, and a muted test is worse
    // than none. It is capped so a systemic failure still fails.
    let transient: u64 = deltas
        .iter()
        .filter(|(r, _)| matches!(*r, "log_read_failed" | "populate_failed"))
        .map(|(_, d)| *d)
        .sum();
    assert!(
        transient * 100 < total as u64,
        "{transient} of {total} reads failed transiently (>1%) — that is no longer \
         contention noise. Breakdown: {deltas:?}"
    );
    assert_eq!(
        refused as u64,
        deltas.iter().map(|(_, d)| *d).sum::<u64>(),
        "every refusal must be accounted for by a named reason, or a refusal path \
         exists that records nothing. refused={refused}, reasons={deltas:?}"
    );
}

/// ⚠⚠ **The positive control — a REAL content conflict must still be caught,
/// against the real database.**
///
/// Without this, the burst test above would pass on a build where divergence
/// detection had simply been switched off.
#[tokio::test]
async fn a360_a_real_content_conflict_is_still_caught_against_postgres() {
    let Some(p) = pool().await else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };
    arm_store();
    // A dedicated execution so no other test's partition is disturbed.
    let exec = 2009i64;

    // Populate the store from a log this execution does NOT have, by driving the
    // populator directly: three synthetic events.
    {
        use ehdb_l0::chain_populator::LogEvent;
        let src = noetl_server::handlers::chain_populate::LogSourcedChainSource::new(p.clone(), 1000);
        let _ = src.chain_for(exec).await; // establishes/confirms an empty partition
        let planted = vec![
            LogEvent { event_id: "9001".into(), parent_execution_id: None, payload: "{}".into(), prev_event_id: None },
            LogEvent { event_id: "9002".into(), parent_execution_id: None, payload: "{}".into(), prev_event_id: Some("9001".into()) },
        ];
        let out = noetl_server::handlers::chain_populate::populate_from_log_for_test(exec, &planted)
            .expect("plant");
        assert!(out.is_healthy(), "planting must succeed, got {out:?}");
    }

    // Now a log with a CONFLICTING event at position 1.
    {
        use ehdb_l0::chain_populator::LogEvent;
        let conflicting = vec![
            LogEvent { event_id: "9001".into(), parent_execution_id: None, payload: "{}".into(), prev_event_id: None },
            LogEvent { event_id: "7777".into(), parent_execution_id: None, payload: "{}".into(), prev_event_id: Some("9001".into()) },
        ];
        let out = noetl_server::handlers::chain_populate::populate_from_log_for_test(exec, &conflicting)
            .expect("no error");
        assert!(
            !out.is_healthy(),
            "a CONTENT conflict must still be reported as divergence, got {out:?}. If \
             this is healthy, #360 switched the detector off instead of classifying."
        );
    }
}

/// ⭐⭐ **The #358 alarm, relocated and PROVEN to still fire.**
///
/// noetl/ai-meta#360 stopped the populator calling "store longer than my snapshot"
/// a divergence, because that misreported benign staleness — 5 false divergences in
/// 79 on prod. But the SAME shape is how a second writer looks
/// (noetl/ai-meta#358, 13.3% real divergence), and absorbing it would be far worse
/// than the bug being fixed.
///
/// So the verdict moved to `chain_for`, which can do what the populator cannot:
/// **read the log a second time.** If the log catches up, it was staleness. If it
/// still ends short while the store runs ahead, those events are not in the
/// authoritative log and a foreign writer put them there.
///
/// This test builds the foreign-writer shape — a store holding 4 events over a log
/// committing 3 — and asserts `chain_for` records **`diverged`**.
///
/// ⚠ Asserting only that `chain_for` returns `None` would NOT discriminate: without
/// the escalation it still returns `None`, via `length_disagreement`, for an
/// unrelated reason. The assertion is therefore on WHICH counter moved.
#[tokio::test]
async fn a360_a_store_ahead_of_a_log_that_does_not_catch_up_still_reports_diverged() {
    let Some(p) = pool().await else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };
    arm_store();
    let exec = 2011i64;

    sqlx::query("DELETE FROM noetl.event WHERE execution_id = $1")
        .bind(exec)
        .execute(&p)
        .await
        .expect("clear");
    let cid: i64 = sqlx::query_scalar("SELECT catalog_id FROM noetl.catalog LIMIT 1")
        .fetch_one(&p)
        .await
        .expect("a catalog row must exist");

    // The log COMMITS 3 events.
    for (n, (ty, st)) in [
        ("playbook.initialized", "initialized"),
        ("step.enter", "PENDING"),
        ("step.completed", "COMPLETED"),
    ]
    .iter()
    .enumerate()
    {
        sqlx::query(
            "INSERT INTO noetl.event (event_id, execution_id, catalog_id, event_type, status, \
             prev_event_id) VALUES ($1,$2,$3,$4,$5,$6)",
        )
        .bind(20_110_000i64 + n as i64)
        .bind(exec)
        .bind(cid)
        .bind(ty)
        .bind(st)
        // ⚠ LINKED, or every row is a root and the execution is refused as
        // `multiple_roots` before the escalation is ever reached.
        .bind(if n == 0 {
            None
        } else {
            Some(20_110_000i64 + n as i64 - 1)
        })
        .execute(&p)
        .await
        .expect("commit");
    }

    // A FOREIGN WRITER puts a 4th event in the STORE that the log does not have.
    // Driving it through `populate_from_log` with a 4-event view is the same end
    // state a second populator produces, and needs no extra test-only mutator.
    let four: Vec<ehdb_l0::chain_populator::LogEvent> = (0..4)
        .map(|n| ehdb_l0::chain_populator::LogEvent {
            event_id: (20_110_000i64 + n).to_string(),
            parent_execution_id: None,
            payload: String::new(),
            prev_event_id: if n == 0 {
                None
            } else {
                Some((20_110_000i64 + n - 1).to_string())
            },
        })
        .collect();
    noetl_server::handlers::chain_populate::populate_from_log_for_test(exec, &four)
        .expect("the foreign writer's populate must succeed");

    let before_div = noetl_server::metrics::chain_populate_total()
        .with_label_values(&["diverged"])
        .get();
    let before_len = noetl_server::metrics::chain_populate_total()
        .with_label_values(&["length_disagreement"])
        .get();

    let src = noetl_server::handlers::chain_populate::LogSourcedChainSource::new(p.clone(), 1000);
    let got = <noetl_server::handlers::chain_populate::LogSourcedChainSource as noetl_server::chain_advance::ChainSource>::chain_for(&src, exec).await;

    let d = noetl_server::metrics::chain_populate_total()
        .with_label_values(&["diverged"])
        .get()
        - before_div;
    let l = noetl_server::metrics::chain_populate_total()
        .with_label_values(&["length_disagreement"])
        .get()
        - before_len;
    eprintln!("  store=4 log=3 -> served={:?} diverged+{d} length_disagreement+{l}", got.is_some());

    assert!(
        got.is_none(),
        "a store holding events the committed log does not have must NEVER be served"
    );
    // ⚠ Clean up BEFORE asserting, so a failing assertion still leaves the fixture
    // as it found it. The fixture-identity guard counts declared executions, and it
    // caught exactly this leak on the first run of this test — which is the guard
    // doing its job, not noise.
    sqlx::query("DELETE FROM noetl.event WHERE execution_id = $1")
        .bind(exec)
        .execute(&p)
        .await
        .expect("cleanup");

    assert_eq!(
        d, 1,
        "the re-read escalation must report DIVERGED. diverged+{d}, \
         length_disagreement+{l} — if length_disagreement moved instead, the \
         escalation is not wired and #358 would be caught only by accident, with no \
         signal naming the cause."
    );
}
