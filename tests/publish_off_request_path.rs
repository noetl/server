//! **The publish retry budget must not sit on the request path** (noetl/ai-meta#319 P3).
//!
//! `PUBLISH_DEADLINE` is 10s for a good reason (#208): it spans a writer pod
//! restart, measured at ~2.7s, and a dropped publish is *lost* — `command.issued`
//! is durable but nothing reaches the bus, and after T5 there is no NATS to fall
//! back to. That window is correct and is **not** shortened here.
//!
//! What was wrong is *who waits for it*. The whole 10s sat on `POST /api/execute`,
//! so one writer hiccup became a 10s user-visible stall: p50 180ms with a **p95 of
//! 17s** at concurrency 1, and a bistable collapse from 18 successes to 0 once
//! publishes began failing.
//!
//! These tests pin both halves of the fix: the caller stops waiting, **and** the
//! append still lands.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use noetl_server::command_bus::{EhdbCommandPublisher, PublishOutcome};

fn unused_addr() -> String {
    // Bind, read the port, drop the listener — nothing is listening there now.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let a = l.local_addr().unwrap();
    drop(l);
    format!("127.0.0.1:{}", a.port())
}

fn publisher_at(addr: &str) -> Arc<EhdbCommandPublisher> {
    let mut addrs = BTreeMap::new();
    addrs.insert(0u32, addr.to_string());
    Arc::new(EhdbCommandPublisher::new(1, addrs))
}

/// The request thread must be released in ~the on-path budget, not the full
/// deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_caller_is_released_within_the_on_path_budget() {
    let p = publisher_at(&unused_addr());
    let t0 = Instant::now();
    let out = p.publish_bounded(1, 1, b"{}").await;
    let waited = t0.elapsed();

    assert!(
        matches!(out, Ok(PublishOutcome::Deferred)),
        "an unreachable writer must defer, not fail the caller: {out:?}"
    );
    assert!(
        waited < Duration::from_secs(3),
        "the caller waited {waited:?} — the on-path budget is 500ms; anything near the \
         10s PUBLISH_DEADLINE means the retry is still on the request path, which is \
         exactly the 17s p95 this fixes"
    );
}

/// ⚠ THE NEGATIVE CONTROL. Without it, the test above passes trivially on any
/// implementation that returns fast — including one that never retries at all.
/// This pins that the FULL budget still exists on the unbounded path, so the
/// 10s window (#208's writer-restart cover) has not been quietly deleted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_full_deadline_still_exists_on_the_unbounded_path() {
    let p = publisher_at(&unused_addr());
    let t0 = Instant::now();
    let r = p.publish(2, 2, b"{}").await;
    let waited = t0.elapsed();

    assert!(r.is_err(), "an unreachable writer must eventually fail the unbounded call");
    assert!(
        waited >= Duration::from_secs(8),
        "the unbounded publish returned after {waited:?} — the 10s PUBLISH_DEADLINE that \
         covers a writer pod restart (noetl/ai-meta#208) must NOT have been shortened; \
         P3 moves the wait, it does not remove it"
    );
}

/// ⚠ THE CORRECTNESS GATE. A transient failure must still land the append —
/// deferring is not dropping.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_transient_failure_still_lands_the_append_exactly_once() {
    use ehdb_feed::{serve_ingest, FeedWriter};
    use ehdb_l0::substrate::DurableSubstrate;
    use ehdb_l0::{D1EventLog, L0Config, L0Engine, LocalFsSubstrate};

    // Reserve a port, leave it closed so the first attempts fail.
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    let p = publisher_at(&format!("127.0.0.1:{}", addr.port()));

    let t0 = Instant::now();
    let out = p.publish_bounded(4242, 7, b"{\"hello\":\"world\"}").await;
    assert!(
        matches!(out, Ok(PublishOutcome::Deferred)),
        "writer down at call time must defer: {out:?}"
    );
    assert!(
        t0.elapsed() < Duration::from_secs(3),
        "caller must not have waited for the writer to come back"
    );

    // Writer arrives late — the same shape as a pod restart completing.
    let dir = std::env::temp_dir().join(format!("p3-late-writer-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let store: Arc<dyn DurableSubstrate> = Arc::new(LocalFsSubstrate::new(&dir).unwrap());
    let engine =
        L0Engine::<D1EventLog>::open(L0Config::d1(&dir).with_shard_count(1), store).unwrap();
    let writer = Arc::new(FeedWriter::new(engine));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", addr.port())).await.unwrap();
    tokio::spawn(serve_ingest(listener, writer.clone()));

    // The deferred retry should find it within the remaining deadline.
    let mut found = 0usize;
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let eng = writer.engine();
        let g = eng.lock().unwrap();
        found = g.read_execution_after("4242", 0).map(|v| v.len()).unwrap_or(0);
        drop(g);
        if found > 0 {
            break;
        }
    }

    assert_eq!(
        found, 1,
        "the deferred publish must land EXACTLY ONCE once the writer returns — {found} \
         records found. 0 means deferring silently dropped the command (the loss bug \
         noetl/ai-meta#208 exists to prevent); >1 means the background retry duplicated \
         beyond the at-least-once contract within a single deferral."
    );
    let _ = std::fs::remove_dir_all(&dir);
}
