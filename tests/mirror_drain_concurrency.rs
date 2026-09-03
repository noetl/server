//! The drain delivers independent executions CONCURRENTLY — measured, not declared.
//!
//! **noetl/ai-meta#319.** The drain used to POST one batch at a time. Throughput
//! was `1 / relay-round-trip` regardless of how many independent executions were
//! waiting, and on production that was nowhere near enough: mean queue lag
//! **170 s**, the queue permanently full, and **24.6%** of batches falling
//! through to inline delivery *on the `/api/execute` request path* — the whole
//! p95 tail and the whole run-to-run bistability.
//!
//! # Why this test measures wall-clock
//!
//! A concurrency knob is exactly the kind of thing that is set, documented, and
//! **reached by nothing**. `DRAIN_CONCURRENCY_ENV` could be read, parsed and
//! logged while the deliveries still serialise behind a shared lock, a `&mut`,
//! or an accidental `.await` in the spawn loop — and every structural check
//! would pass. The only claim worth asserting is the one the production defect
//! is about: **N slow deliveries take about one delivery's time, not N.**
//!
//! # Its own binary, one test function
//!
//! The queue is a process-global (`OnceLock` + one drain task) configured from
//! process env, and `cargo test` does **not** serialise tests within a binary.
//! `mirror_queue.rs` settled on one function for that reason; this needs a
//! *different* queue configuration, so it needs a different process.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{extract::State, routing::post, Json, Router};
use noetl_server::handlers::ehdb_eventlog_mirror::MirrorBatch;
use noetl_server::handlers::ehdb_eventlog_mirror_queue as queue;
use serde_json::Value;

/// Each delivery costs this much at the relay. Long enough that serial and
/// concurrent are unmistakable, short enough that the test is quick.
const RELAY_DELAY: Duration = Duration::from_millis(200);
const EXECUTIONS: usize = 8;

#[derive(Clone)]
struct Relay {
    seen: Arc<AtomicUsize>,
    /// Highest number of requests in flight at the relay at any instant. This is
    /// the direct observation of concurrency, independent of any timing margin.
    peak: Arc<Mutex<usize>>,
    inflight: Arc<Mutex<usize>>,
}

async fn accept(State(r): State<Relay>, Json(_body): Json<Value>) -> &'static str {
    {
        let mut n = r.inflight.lock().unwrap();
        *n += 1;
        let mut p = r.peak.lock().unwrap();
        if *n > *p {
            *p = *n;
        }
    }
    tokio::time::sleep(RELAY_DELAY).await;
    *r.inflight.lock().unwrap() -= 1;
    r.seen.fetch_add(1, Ordering::SeqCst);
    "ok"
}

async fn drain_all(seen: &Arc<AtomicUsize>, what: &str) {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(30) {
        if seen.load(Ordering::SeqCst) >= EXECUTIONS {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!(
        "{what}: only {} of {EXECUTIONS} deliveries arrived in 30s",
        seen.load(Ordering::SeqCst)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn independent_executions_are_delivered_concurrently() {
    let seen = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(Mutex::new(0usize));
    let relay = Relay {
        seen: seen.clone(),
        peak: peak.clone(),
        inflight: Arc::new(Mutex::new(0)),
    };
    let app = Router::new()
        .route("/ehdb/tiers/eventlog", post(accept))
        .with_state(relay);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base = format!("http://{addr}");

    std::env::set_var(queue::ASYNC_ENV, "true");
    std::env::set_var(queue::CAPACITY_ENV, "64");
    std::env::set_var(queue::ENQUEUE_TIMEOUT_ENV, "5000");
    // Larger than EXECUTIONS, so one pass can pick all of them up: this test is
    // about concurrency WITHIN a pass, and a drain_max below EXECUTIONS would
    // split them across passes and serialise them for an unrelated reason.
    std::env::set_var(queue::DRAIN_MAX_ENV, "64");
    std::env::set_var(queue::DRAIN_CONCURRENCY_ENV, "8");
    queue::init();
    assert!(queue::enabled(), "the queue must arm from the flag");

    // ---- the measurement ---------------------------------------------------
    let started = Instant::now();
    for i in 0..EXECUTIONS {
        queue::submit(MirrorBatch {
            base: base.clone(),
            execution_id: 1000 + i as i64,
            records: vec![format!("{{\"n\":{i}}}")],
            enqueued_at: Instant::now(),
        })
        .await;
    }
    drain_all(&seen, "concurrent").await;
    let concurrent = started.elapsed();
    let observed_peak = *peak.lock().unwrap();

    let serial_floor = RELAY_DELAY * EXECUTIONS as u32;
    assert!(
        concurrent < serial_floor / 2,
        "{EXECUTIONS} independent executions took {concurrent:?}; serial would be \
         about {serial_floor:?}. The drain is still delivering one at a time — \
         which is the production defect, not a slow test."
    );
    assert!(
        observed_peak > 1,
        "the relay never saw more than one request in flight, so nothing was \
         actually concurrent regardless of how long the pass took"
    );

    // ---- ⚠ NEGATIVE CONTROL ------------------------------------------------
    //
    // Without this the assertions above pass on a machine fast enough to make
    // any implementation look quick, and the test would be measuring the host
    // rather than the drain. Concurrency is read per pass, so setting it to 1
    // restores the serial shape in the SAME process, against the SAME relay —
    // and the timing must flip.
    std::env::set_var(queue::DRAIN_CONCURRENCY_ENV, "1");
    seen.store(0, Ordering::SeqCst);
    *peak.lock().unwrap() = 0;

    let started = Instant::now();
    for i in 0..EXECUTIONS {
        queue::submit(MirrorBatch {
            base: base.clone(),
            execution_id: 2000 + i as i64,
            records: vec![format!("{{\"n\":{i}}}")],
            enqueued_at: Instant::now(),
        })
        .await;
    }
    drain_all(&seen, "serial").await;
    let serial = started.elapsed();

    assert!(
        serial >= serial_floor - RELAY_DELAY,
        "with concurrency 1 the same work took {serial:?}, under the {serial_floor:?} \
         a serial drain must cost. The knob is not being honoured, so the \
         concurrent measurement above proves nothing about it."
    );
    assert_eq!(
        *peak.lock().unwrap(),
        1,
        "concurrency 1 still put more than one request in flight"
    );
    assert!(
        concurrent < serial,
        "concurrent ({concurrent:?}) was not faster than serial ({serial:?})"
    );
}
