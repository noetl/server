//! noetl/ai-meta#362 — THE FORCED-RESTART PROOF.
//!
//! The invariant: **exactly one NULL-prev event per execution.** That event is the
//! genesis. A second one is the re-rooting defect, and under link-defined ordering it
//! makes the partition unbuildable — nothing can say which root is real.
//!
//! # Why this forces the restart instead of performing one
//!
//! A server restart's ONLY effect on this code path is that the in-memory
//! `ChainHeads` map starts empty. Nothing else about the process matters: the map is
//! per-process state with no disk backing. Constructing a fresh `ChainHeads`
//! reproduces the post-restart condition exactly, and does it deterministically.
//!
//! ⚠ That substitution is deliberate and it is stronger than deleting a pod, which I
//! tried first. A pod delete only exhibits the defect if an execution happens to be
//! emitting across the exact window, and noetl/ai-meta#227 records that in-flight
//! executions often STALL after a restart rather than continuing — so the condition
//! frequently never arises. That is the same "waited for the condition instead of
//! forcing it" trap that let the #472 hydrator ship and never fire: on prod it
//! recorded `head=0` across 4,270 hydration decisions.
//!
//! Run: NOETL_TEST_PG_URL=postgres://postgres:chainproof@127.0.0.1:55432/noetl cargo test --test chain_forced_restart

#![allow(non_snake_case)] // RED_/GREEN_ in test names is deliberate: they are the
                          // two halves of one control and must be greppable as such.

use std::sync::Arc;

fn url() -> Option<String> {
    std::env::var("NOETL_TEST_PG_URL").ok()
}

async fn pool() -> Option<sqlx::PgPool> {
    let u = url()?;
    match sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&u)
        .await
    {
        Ok(p) => Some(p),
        Err(e) => panic!("NOETL_TEST_PG_URL is SET but unreachable ({e}) — refusing to skip."),
    }
}

fn fresh_heads() -> noetl_server::state::ChainHeads {
    // Exactly what a process start produces: an empty map.
    noetl_server::state::ChainHeads::with_coherence(Arc::new(
        noetl_server::coherence::CoherenceKv::new(),
    ))
}

async fn seed(p: &sqlx::PgPool, exec: i64, links: &[(i64, Option<i64>)]) {
    wipe(p, exec).await;
    let cid: i64 = sqlx::query_scalar("SELECT catalog_id FROM noetl.catalog LIMIT 1")
        .fetch_one(p)
        .await
        .expect("a catalog row must exist");
    for (n, prev) in links {
        sqlx::query(
            "INSERT INTO noetl.event (event_id, execution_id, catalog_id, event_type, status, \
             prev_event_id) VALUES ($1,$2,$3,'step.enter','PENDING',$4)",
        )
        .bind(exec * 1000 + n)
        .bind(exec)
        .bind(cid)
        .bind(prev.map(|v| exec * 1000 + v))
        .execute(p)
        .await
        .expect("seed");
    }
}

async fn wipe(p: &sqlx::PgPool, exec: i64) {
    sqlx::query("DELETE FROM noetl.event WHERE execution_id = $1")
        .bind(exec)
        .execute(p)
        .await
        .expect("wipe");
}

async fn roots(p: &sqlx::PgPool, exec: i64) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM noetl.event WHERE execution_id = $1 AND prev_event_id IS NULL",
    )
    .bind(exec)
    .fetch_one(p)
    .await
    .expect("roots")
}

/// Write the next event the way the server does: stamp `prev` from the head map, then
/// insert. This is the two-line shape every linking write path shares.
async fn emit_next(
    p: &sqlx::PgPool,
    heads: &noetl_server::state::ChainHeads,
    exec: i64,
    n: i64,
) -> Option<i64> {
    let id = exec * 1000 + n;
    let prev = heads
        .link_batch(exec, &[id])
        .await
        .into_iter()
        .next()
        .flatten();
    let cid: i64 = sqlx::query_scalar("SELECT catalog_id FROM noetl.catalog LIMIT 1")
        .fetch_one(p)
        .await
        .expect("catalog");
    sqlx::query(
        "INSERT INTO noetl.event (event_id, execution_id, catalog_id, event_type, status, \
         prev_event_id) VALUES ($1,$2,$3,'step.enter','PENDING',$4)",
    )
    .bind(id)
    .bind(exec)
    .bind(cid)
    .bind(prev)
    .execute(p)
    .await
    .expect("emit");
    prev
}

/// ⭐⭐ **RED: no hydrator — a restart mid-emit creates a SECOND ROOT.**
///
/// This is the defect, reproduced on demand. It is what kind's data actually shows:
/// 2 of 63 recent executions carry a `step.enter` with a NULL prev arriving ~112s
/// after the previous event.
#[tokio::test]
async fn a362_RED_without_hydration_a_restart_creates_a_second_root() {
    let Some(p) = pool().await else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };
    let exec = 4101i64;

    // Pre-restart: a genesis plus two linked events.
    seed(&p, exec, &[(1, None), (2, Some(1)), (3, Some(2))]).await;
    assert_eq!(roots(&p, exec).await, 1, "precondition: exactly one root");

    // THE RESTART: a fresh process, empty map — and NO hydrator wired, which is the
    // pre-#472 build and also what kind runs today (v3.112.3 exposes no
    // `chain_head_hydrate` series at all).
    let heads = fresh_heads();
    let prev = emit_next(&p, &heads, exec, 4).await;

    assert_eq!(
        prev, None,
        "with no hydrator the head map misses and the event is stamped as a ROOT"
    );
    assert_eq!(
        roots(&p, exec).await,
        2,
        "⭐ THE DEFECT: a restart mid-emit turns the next event into a second false \
         root. Under link-defined ordering the partition is now unbuildable."
    );
    wipe(&p, exec).await;
}

/// ⭐⭐ **GREEN: with the link-tip hydrator the same restart keeps ONE root.**
#[tokio::test]
async fn a362_GREEN_with_hydration_a_restart_keeps_exactly_one_root() {
    let Some(p) = pool().await else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };
    let exec = 4102i64;

    seed(&p, exec, &[(1, None), (2, Some(1)), (3, Some(2))]).await;
    assert_eq!(roots(&p, exec).await, 1, "precondition: exactly one root");

    let heads = fresh_heads();
    heads.set_hydrator(Some(Arc::new(
        noetl_server::db::queries::event_chain::PgChainHeadHydrator::new(p.clone()),
    )));
    let prev = emit_next(&p, &heads, exec, 4).await;

    assert_eq!(
        prev,
        Some(exec * 1000 + 3),
        "the event must link to the real TIP (…003), recovered from the database"
    );
    assert_eq!(
        roots(&p, exec).await,
        1,
        "⭐ THE FIX: the invariant holds across a restart — still exactly one root"
    );

    // And the chain is whole: walking prev-links from the tip reaches the genesis.
    let chain: Vec<(i64, Option<i64>)> = sqlx::query_as(
        "SELECT event_id, prev_event_id FROM noetl.event WHERE execution_id = $1 \
         ORDER BY event_id",
    )
    .bind(exec)
    .fetch_all(&p)
    .await
    .expect("read");
    assert_eq!(chain.len(), 4);
    assert_eq!(
        chain.iter().filter(|(_, pv)| pv.is_none()).count(),
        1,
        "one genesis"
    );
    wipe(&p, exec).await;
}

/// ⚠ The tip, not `max(event_id)` — across a restart, on an execution where the two
/// disagree. This is the #362 case and the RED/GREEN above would both pass on a
/// `max(event_id)` hydrator without it.
#[tokio::test]
async fn a362_restart_links_to_the_tip_even_when_it_is_not_the_max_id() {
    let Some(p) = pool().await else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };
    let exec = 4103i64;
    // Chain 1 -> 9 -> 4 : the tip (…004) is NOT the highest id (…009).
    seed(&p, exec, &[(1, None), (9, Some(1)), (4, Some(9))]).await;

    let heads = fresh_heads();
    heads.set_hydrator(Some(Arc::new(
        noetl_server::db::queries::event_chain::PgChainHeadHydrator::new(p.clone()),
    )));
    let prev = emit_next(&p, &heads, exec, 7).await;
    assert_eq!(
        prev,
        Some(exec * 1000 + 4),
        "must link to the TIP …004, not to max(event_id) …009 which already has a \
         successor. Linking to …009 forks the chain while looking healthy."
    );
    assert_eq!(roots(&p, exec).await, 1);
    wipe(&p, exec).await;
}

/// ⚠ A FAILED lookup must not become a root. `Failed` and `Empty` mean opposite
/// things and collapsing them stamps a root whenever the database hiccups.
#[tokio::test]
async fn a362_a_failed_hydration_does_not_silently_root() {
    let Some(p) = pool().await else {
        eprintln!("SKIP: NOETL_TEST_PG_URL unset");
        return;
    };
    let exec = 4104i64;
    seed(&p, exec, &[(1, None), (2, Some(1))]).await;

    struct AlwaysFails;
    #[async_trait::async_trait]
    impl noetl_server::state::ChainHeadHydrator for AlwaysFails {
        async fn head_of(&self, _e: i64) -> noetl_server::state::HydrateOutcome {
            noetl_server::state::HydrateOutcome::Failed
        }
    }

    let heads = fresh_heads();
    heads.set_hydrator(Some(Arc::new(AlwaysFails)));
    let before = noetl_server::metrics::chain_head_hydrate_total()
        .with_label_values(&["failed"])
        .get();
    let _ = heads.link_batch(exec, &[exec * 1000 + 3]).await;
    let after = noetl_server::metrics::chain_head_hydrate_total()
        .with_label_values(&["failed"])
        .get();
    assert_eq!(
        after - before,
        1,
        "a failed hydration must be COUNTED, or a store built on those rows is \
         trusted silently"
    );
    wipe(&p, exec).await;
}
