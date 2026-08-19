//! The async event-log mirror queue, end to end against a real HTTP relay.
//!
//! **[noetl/ai-meta#155](https://github.com/noetl/ai-meta/issues/155) Option 3.**
//!
//! The queue's whole safety claim is negative — *no event is lost, none is
//! reordered, and a full queue pushes back instead of dropping* — and negative
//! claims are exactly the ones a unit test on a pure function cannot make. So
//! this drives the real `submit` → drain → `deliver` → HTTP path against a
//! listener that records what actually arrived, and checks the bytes.
//!
//! # Why this is one test function
//!
//! The queue is a process-global (`OnceLock` + one drain task) configured from
//! process env, and `cargo test` does **not** serialise tests within a binary.
//! Two test functions here would race the env and the single `init()`, and the
//! failure would be intermittent and blamed on the queue. One function, three
//! phases, sharing one armed queue — the same shape ehdb#316's replay-count
//! guard settled on for the same reason.
//!
//! # The phases
//!
//! 1. **Order and completeness under load** — 300 batches across 4 executions,
//!    every record carrying its ordinal. Every record must arrive exactly once,
//!    and per execution the ordinals must be strictly increasing.
//! 2. **Backpressure** — the queue is deliberately tiny (8) and the relay
//!    deliberately slow, so a burst must exceed it. `enqueued_after_wait` must
//!    fire: the producer waited rather than the queue dropping.
//! 3. **The inline fallback** — with the enqueue timeout cut to ~0 the third
//!    rung is forced. `queue_full_inline` must fire **and the records must
//!    still arrive**, because the fallback is a delivery, not a discard.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{extract::State, routing::post, Json, Router};
use noetl_server::handlers::ehdb_eventlog_mirror::MirrorBatch;
use noetl_server::handlers::ehdb_eventlog_mirror_queue as queue;
use serde_json::Value;

type Received = Arc<Mutex<Vec<(i64, Vec<String>)>>>;

#[derive(Clone)]
struct Relay {
    received: Received,
    delay: Arc<Mutex<Duration>>,
}

async fn accept(State(relay): State<Relay>, Json(body): Json<Value>) -> &'static str {
    let delay = *relay.delay.lock().unwrap();
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    let execution_id: i64 = body
        .get("execution_id")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .expect("relay body must carry a stringified execution_id");
    let records: Vec<String> = body
        .get("records")
        .and_then(|v| v.as_array())
        .expect("relay body must carry records")
        .iter()
        .map(|r| r.as_str().unwrap_or_default().to_string())
        .collect();
    relay.received.lock().unwrap().push((execution_id, records));
    "ok"
}

fn queue_counter(outcome: &str) -> u64 {
    noetl_server::metrics::ehdb_eventlog_mirror_queue_total()
        .with_label_values(&[outcome])
        .get()
}

/// Wait for the queue to reach zero pending, or fail loudly.
///
/// Polling the gauge rather than sleeping a fixed time: a fixed sleep that is
/// too short turns "the queue is slow" into "the queue lost an event", which is
/// the one wrong conclusion this file exists to prevent.
async fn settle(what: &str) {
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(30) {
        if noetl_server::metrics::ehdb_eventlog_mirror_pending_events().get() <= 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "{what}: queue never drained — {} events still pending after 30s",
        noetl_server::metrics::ehdb_eventlog_mirror_pending_events().get()
    );
}

fn batch(base: &str, execution_id: i64, records: Vec<String>) -> MirrorBatch {
    MirrorBatch {
        base: base.to_string(),
        execution_id,
        records,
        enqueued_at: Instant::now(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_queue_never_loses_reorders_or_drops() {
    let received: Received = Arc::new(Mutex::new(Vec::new()));
    let delay = Arc::new(Mutex::new(Duration::ZERO));
    let relay = Relay {
        received: received.clone(),
        delay: delay.clone(),
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

    // A deliberately tiny queue: phase 2 has to be able to fill it, and a
    // capacity that a burst cannot exhaust would make the backpressure
    // assertion vacuous.
    std::env::set_var(queue::ASYNC_ENV, "true");
    std::env::set_var(queue::CAPACITY_ENV, "8");
    std::env::set_var(queue::ENQUEUE_TIMEOUT_ENV, "5000");
    queue::init();
    assert!(queue::enabled(), "the queue must arm from the flag");
    assert_eq!(
        noetl_server::metrics::ehdb_eventlog_mirror_async_enabled().get(),
        1,
        "the armed gauge must publish 1 — 'off' and 'never started' must not read alike"
    );

    // ---- phase 1: order and completeness -----------------------------------
    const EXECUTIONS: [i64; 4] = [11, 22, 33, 44];
    const BATCHES: usize = 300;
    let mut sent: HashMap<i64, Vec<usize>> = HashMap::new();
    for i in 0..BATCHES {
        let execution_id = EXECUTIONS[i % EXECUTIONS.len()];
        // 1..=3 records, so batches are not uniform and a merge that dropped a
        // tail would show up as a gap rather than as a shorter list everywhere.
        let n = (i % 3) + 1;
        let mut records = Vec::with_capacity(n);
        for k in 0..n {
            let ordinal = sent.entry(execution_id).or_default().len() + k;
            records.push(format!("{execution_id}:{ordinal}"));
        }
        let ordinals = sent.entry(execution_id).or_default();
        let start = ordinals.len();
        ordinals.extend(start..start + n);
        queue::submit(batch(&base, execution_id, records)).await;
    }
    settle("phase 1").await;

    let got = received.lock().unwrap().clone();
    let mut per_execution: HashMap<i64, Vec<String>> = HashMap::new();
    for (execution_id, records) in &got {
        per_execution
            .entry(*execution_id)
            .or_default()
            .extend(records.clone());
    }
    for execution_id in EXECUTIONS {
        let want: Vec<String> = sent[&execution_id]
            .iter()
            .map(|o| format!("{execution_id}:{o}"))
            .collect();
        let have = per_execution
            .get(&execution_id)
            .unwrap_or_else(|| panic!("execution {execution_id} received nothing at all"));
        assert_eq!(
            have, &want,
            "execution {execution_id}: the tier must receive every record exactly once, in \
             emit order. Any difference here is a lost, duplicated or reordered authoritative \
             event on a primary-serving tier (noetl/ai-meta#155)."
        );
    }
    let total_records: usize = sent.values().map(|v| v.len()).sum();
    assert_eq!(
        got.iter().map(|(_, r)| r.len()).sum::<usize>(),
        total_records,
        "no record may be delivered twice"
    );

    // Coalescing must actually have happened — otherwise phase 1 proved the
    // ordering of a queue that was never under any pressure, and the batch
    // substrate (ehdb#317) still has nothing to do.
    let multi_record_requests = got.iter().filter(|(_, r)| r.len() > 1).count();
    assert!(
        multi_record_requests > 0,
        "the drain never coalesced: {} requests, all single-record. The queue is supposed to \
         produce multi-record batches for `append_batch` — if it cannot, Option 2's substrate \
         stays inert (noetl/ai-meta#155).",
        got.len()
    );
    assert!(
        got.len() < BATCHES,
        "coalescing must reduce request count below the batch count; got {} requests for \
         {BATCHES} batches",
        got.len()
    );

    // ---- phase 2: backpressure, not drop -----------------------------------
    *delay.lock().unwrap() = Duration::from_millis(15);
    received.lock().unwrap().clear();
    let waited_before = queue_counter("enqueued_after_wait");
    let full_inline_before = queue_counter("queue_full_inline");
    for i in 0..120 {
        queue::submit(batch(&base, 99, vec![format!("burst:{i}")])).await;
    }
    settle("phase 2").await;
    assert!(
        queue_counter("enqueued_after_wait") > waited_before,
        "a burst of 120 into a queue of 8 behind a 15ms relay must have made the producer \
         WAIT. If it never waited, the queue is not bounded and the 'never drop' guarantee \
         rests on memory growth instead of on backpressure."
    );
    assert_eq!(
        queue_counter("queue_full_inline"),
        full_inline_before,
        "with a 5s enqueue timeout the third rung must not be reached — waiting is the \
         correct behaviour here, and taking the inline path would risk reordering for no reason"
    );
    let burst: Vec<String> = received
        .lock()
        .unwrap()
        .iter()
        .flat_map(|(_, r)| r.clone())
        .collect();
    assert_eq!(
        burst,
        (0..120).map(|i| format!("burst:{i}")).collect::<Vec<_>>(),
        "every burst record must arrive exactly once and in order"
    );

    // ---- phase 3: the inline fallback delivers ------------------------------
    //
    // Rung 3 is the only path that can deliver out of order, so it must at
    // least be proven to deliver AT ALL. A fallback that silently dropped would
    // pass every assertion above.
    received.lock().unwrap().clear();
    std::env::set_var(queue::ENQUEUE_TIMEOUT_ENV, "0");
    let full_inline_before = queue_counter("queue_full_inline");
    for i in 0..60 {
        queue::submit(batch(&base, 77, vec![format!("inline:{i}")])).await;
    }
    settle("phase 3").await;
    assert!(
        queue_counter("queue_full_inline") > full_inline_before,
        "a zero enqueue timeout against a full queue must reach the inline fallback"
    );
    let delivered: Vec<String> = received
        .lock()
        .unwrap()
        .iter()
        .flat_map(|(_, r)| r.clone())
        .collect();
    assert_eq!(
        delivered.len(),
        60,
        "the inline fallback must DELIVER, not discard — 'never drop' is the property the \
         whole ladder exists for (noetl/ai-meta#155). Got {} of 60.",
        delivered.len()
    );
    let mut sorted = delivered.clone();
    sorted.sort_by_key(|s| s.rsplit(':').next().unwrap().parse::<usize>().unwrap());
    assert_eq!(
        sorted,
        (0..60).map(|i| format!("inline:{i}")).collect::<Vec<_>>(),
        "every record must be present exactly once, even if the fallback reordered them"
    );

    // The lag histogram must have observations, or the parity window is being
    // checked against nothing.
    let text = noetl_server::metrics::gather_text().expect("gather");
    let count_line = text
        .lines()
        .find(|l| l.starts_with("noetl_ehdb_eventlog_mirror_lag_seconds_count"))
        .expect("the lag histogram must render");
    let observed: f64 = count_line.rsplit(' ').next().unwrap().parse().unwrap();
    assert!(
        observed > 0.0,
        "the lag histogram must carry observations — it is the evidence the comparator's \
         tolerance window is respected rather than assumed"
    );
}
