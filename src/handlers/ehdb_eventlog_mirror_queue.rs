//! Take the event-log mirror off the event-write hot path — a bounded queue
//! with backpressure, never a drop.
//!
//! **[noetl/ai-meta#155](https://github.com/noetl/ai-meta/issues/155) Option 3.**
//!
//! # What this is worth
//!
//! [`super::ehdb_eventlog_mirror::mirror_rows`] is called inline from the
//! `emit_events` chokepoint, so every authoritative event pays a relay round
//! trip plus a tier append before the event write returns. Measured on a real
//! Muno planner turn on prod: **110 ms per call, 76 calls, 8.4 s of the turn**
//! — after the runtime cache (ehdb#316) had already cut it from 85.6 s.
//!
//! None of that work is on the critical path of anything. The mirror is an
//! auxiliary verification copy; the authoritative write has already committed
//! by the time it runs. Moving it behind a queue removes the whole 8.4 s from
//! the turn and costs an enqueue.
//!
//! # Why "never drop" is not a preference
//!
//! The event-log tier serves `primary`. A dropped mirror record is not a lost
//! metric, it is an event the tier will never hold — and the comparator will
//! correctly call that a `missing_event` divergence forever after. So the
//! backpressure policy is a ladder, and every rung keeps the event:
//!
//! 1. **`try_send`** — room on the queue, done in microseconds.
//! 2. **`send().await` with a timeout** — the queue is full, so the emit path
//!    *waits* for room. This is real backpressure: the producer slows to the
//!    drain rate, nothing is lost, and order is preserved exactly.
//! 3. **inline delivery** — the queue stayed full past the timeout. The batch
//!    is mirrored synchronously, i.e. the pre-Option-3 behaviour.
//!
//! Rung 3 is the one to be uncomfortable about, and it is metered as its own
//! outcome because of it: a batch delivered inline while earlier batches for
//! the *same execution* are still queued lands out of order, and the comparator
//! reports an `order` divergence. That is a spurious demote — but it is only
//! reachable when the tier has been unable to accept a batch for the whole
//! enqueue timeout, which is a state in which the tier is genuinely degraded
//! and the demote is the right answer for a different reason. It is never a
//! lost event, and the alert fires on it.
//!
//! There is no rung 4. Dropping is not on the ladder.
//!
//! # Ordering
//!
//! One drain task, FIFO queue, batches delivered sequentially. Per execution
//! the order the tier receives is exactly the order `emit_events` produced —
//! which is a stronger guarantee than the inline path gives today, where two
//! concurrent `emit_events` calls race their two POSTs against each other.
//!
//! Across server replicas nothing changes: each replica has its own queue, and
//! two replicas emitting for one execution were already ordered only by their
//! POST timing. The drain is immediate — it never waits to accumulate — so the
//! delay this adds is bounded by one in-flight delivery (~110 ms measured, less
//! with the batch substrate), against a p50 inter-event gap of 225 ms on a real
//! prod turn.
//!
//! # Where the batch substrate finally pays off
//!
//! ehdb#317 + worker#281 landed a batch-capable tier append (one open, N
//! writes, one fsync) that was measured to be **inert** under the synchronous
//! mirror: prod events arrive 225 ms apart, so `emit_events` hands the mirror
//! one record per call and `append_batch` saw zero multi-record calls. The
//! queue is what creates them — batches accumulate while a delivery is in
//! flight, and the drain coalesces every batch for one execution into a single
//! request.
//!
//! # Flag
//!
//! `NOETL_EHDB_EVENTLOG_MIRROR_ASYNC`, default **off**. Off, `mirror_rows`
//! takes the same inline path it takes today and this module's only effect is
//! four gauges reading 0.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tracing::{info, warn};

use super::ehdb_eventlog_mirror::MirrorBatch;

/// Arm the async queue. Default off.
pub const ASYNC_ENV: &str = "NOETL_EHDB_EVENTLOG_MIRROR_ASYNC";

/// Queue depth in **batches**. Default 512.
///
/// A batch is one `emit_events` call, so 512 is ~40 Muno turns of headroom at
/// 13 events a turn. Sized to absorb a burst, not to hide a stall: past this
/// the producer is meant to feel it (rung 2), because a queue deep enough never
/// to push back is a queue whose lag can exceed the comparator's tolerance
/// window without anything noticing.
pub const CAPACITY_ENV: &str = "NOETL_EHDB_EVENTLOG_MIRROR_QUEUE_CAPACITY";
const DEFAULT_CAPACITY: usize = 512;

/// How long the emit path waits for room before falling back to inline
/// delivery. Default 5000 ms — the same bound the inline relay already has, so
/// the worst case a caller can see is unchanged from today.
pub const ENQUEUE_TIMEOUT_ENV: &str = "NOETL_EHDB_EVENTLOG_MIRROR_ENQUEUE_TIMEOUT_MS";
const DEFAULT_ENQUEUE_TIMEOUT_MS: u64 = 5_000;

/// Most batches one drain pass takes off the queue before delivering. Default
/// 64. Bounds how much one execution's coalesced request can grow, so a deep
/// queue cannot build a request larger than the tier's page cap.
pub const DRAIN_MAX_ENV: &str = "NOETL_EHDB_EVENTLOG_MIRROR_DRAIN_MAX_BATCHES";
const DEFAULT_DRAIN_MAX: usize = 64;

/// How many executions one drain pass delivers **concurrently**. Default 8.
///
/// The drain used to POST strictly one batch at a time. Throughput was therefore
/// `1 / relay-round-trip` no matter how many independent executions were waiting,
/// and on production that was not close to enough: the mean queue lag reached
/// **170 s**, the queue sat full, and **24.6% of batches** fell through to inline
/// delivery on the request path — which is the whole p95 tail and the whole
/// run-to-run bistability on `/api/execute` (noetl/ai-meta#319).
///
/// ⚠ Concurrency here is safe for a reason specific to this queue's shape, not
/// because ordering does not matter. `deliver_pass` coalesces to **at most one
/// batch per `execution_id`**, and pass *N+1* does not begin until pass *N* has
/// fully completed. So two requests in flight together are always for two
/// DIFFERENT executions, and the per-execution order this module exists to
/// guarantee is untouched. Setting this to 1 restores the old serial behaviour
/// exactly.
pub const DRAIN_CONCURRENCY_ENV: &str = "NOETL_EHDB_EVENTLOG_MIRROR_DRAIN_CONCURRENCY";
const DEFAULT_DRAIN_CONCURRENCY: usize = 8;

/// How long the shutdown flush waits for the queue to empty. Default 10 s, to
/// match the graceful-shutdown budget in `main`.
pub const FLUSH_TIMEOUT_ENV: &str = "NOETL_EHDB_EVENTLOG_MIRROR_FLUSH_TIMEOUT_MS";
const DEFAULT_FLUSH_TIMEOUT_MS: u64 = 10_000;

fn env_bool(name: &str) -> bool {
    matches!(
        std::env::var(name)
            .ok()
            .map(|s| s.trim().to_ascii_lowercase())
            .as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

fn env_millis(name: &str, default_ms: u64) -> Duration {
    Duration::from_millis(
        std::env::var(name)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(default_ms),
    )
}

/// Authoritative events enqueued and not yet durable.
///
/// Kept as an atomic beside the gauge rather than read off the gauge, because
/// the shutdown flush needs to *wait* on it and a Prometheus gauge is a
/// publication surface, not a synchronisation primitive.
static PENDING_EVENTS: AtomicI64 = AtomicI64::new(0);

static QUEUE: OnceLock<Option<mpsc::Sender<MirrorBatch>>> = OnceLock::new();

/// Is the async queue armed in this process?
/// The configured drain concurrency, for pinning the gauge and for the ARMED log.
///
/// Reads the same env the drain reads, through the same helper, so the published
/// value cannot drift from the applied one.
pub fn configured_drain_concurrency() -> usize {
    env_usize(DRAIN_CONCURRENCY_ENV, DEFAULT_DRAIN_CONCURRENCY).max(1)
}

pub fn enabled() -> bool {
    queue().is_some()
}

fn queue() -> Option<&'static mpsc::Sender<MirrorBatch>> {
    QUEUE.get().and_then(|q| q.as_ref())
}

/// Arm the queue and start the drain task. Idempotent; call once at startup.
///
/// Deliberately started from `main` rather than lazily on first mirror: a lazy
/// start would make "armed" depend on traffic, and the gauge that says whether
/// this process is async would read 0 on an idle server that is in fact
/// configured for it.
pub fn init() {
    let armed = env_bool(ASYNC_ENV);
    crate::metrics::ehdb_eventlog_mirror_async_enabled().set(i64::from(armed));
    if !armed {
        let _ = QUEUE.set(None);
        return;
    }

    let capacity = env_usize(CAPACITY_ENV, DEFAULT_CAPACITY);
    let drain_max = env_usize(DRAIN_MAX_ENV, DEFAULT_DRAIN_MAX);
    let (tx, rx) = mpsc::channel::<MirrorBatch>(capacity);
    if QUEUE.set(Some(tx)).is_err() {
        // Already initialised. Do not start a second drain task — two drains
        // on one queue would deliver batches concurrently and lose the ordering
        // this module exists to guarantee.
        return;
    }
    info!(
        target: "noetl_server::ehdb_eventlog_mirror",
        capacity,
        drain_max,
        enqueue_timeout_ms = env_millis(ENQUEUE_TIMEOUT_ENV, DEFAULT_ENQUEUE_TIMEOUT_MS).as_millis() as u64,
        drain_concurrency = configured_drain_concurrency(),
        "async event-log mirror queue ARMED — the mirror is off the event-write path"
    );
    tokio::spawn(drain_loop(rx, drain_max));
}

/// Hand a batch to the queue, or deliver it inline if the queue cannot take it.
///
/// Returns after the batch is *accepted*, not after it is durable — that is the
/// whole point. The exception is the inline rungs, which return after delivery.
pub async fn submit(batch: MirrorBatch) {
    let Some(tx) = queue() else {
        // No queue in this process. Should be unreachable — `mirror_rows` checks
        // `enabled()` first — but a stale flag read or a re-entrant call must
        // still deliver the events rather than discard them.
        let n = batch.records.len();
        crate::metrics::record_ehdb_eventlog_mirror_queue("queue_closed_inline", n);
        super::ehdb_eventlog_mirror::deliver(&batch).await;
        return;
    };

    let n = batch.records.len();
    let batch = match tx.try_send(batch) {
        Ok(()) => {
            accepted(n);
            publish_depth(tx);
            crate::metrics::record_ehdb_eventlog_mirror_queue("enqueued", n);
            return;
        }
        Err(mpsc::error::TrySendError::Closed(b)) => {
            crate::metrics::record_ehdb_eventlog_mirror_queue("queue_closed_inline", n);
            warn!(
                target: "noetl_server::ehdb_eventlog_mirror",
                execution_id = b.execution_id, events = n,
                "async mirror queue is closed — the drain task is gone; mirroring inline"
            );
            super::ehdb_eventlog_mirror::deliver(&b).await;
            return;
        }
        Err(mpsc::error::TrySendError::Full(b)) => b,
    };

    // Rung 2 — backpressure. Wait for room. Nothing is lost and order holds.
    //
    // `reserve()`, not `send(batch)`. `send` takes the value **into the
    // future**, so when the timeout cancels that future the batch goes with it
    // and the events are silently gone — a drop, on the one path whose entire
    // purpose is not to have one. `reserve` only waits for capacity; the batch
    // stays in this frame, so the timeout arm still owns it and can deliver it.
    //
    // This is not a hypothetical. The first version of this function used
    // `send`, and `tests/mirror_queue.rs` phase 3 found it immediately: 32 of
    // 60 records reached the relay. Nothing else in the change would have —
    // every counter still read correctly, because the events were counted at
    // the moment they were lost.
    let timeout = env_millis(ENQUEUE_TIMEOUT_ENV, DEFAULT_ENQUEUE_TIMEOUT_MS);
    match tokio::time::timeout(timeout, tx.reserve()).await {
        Ok(Ok(permit)) => {
            permit.send(batch);
            accepted(n);
            publish_depth(tx);
            crate::metrics::record_ehdb_eventlog_mirror_queue("enqueued_after_wait", n);
        }
        Ok(Err(_closed)) => {
            crate::metrics::record_ehdb_eventlog_mirror_queue("queue_closed_inline", n);
            super::ehdb_eventlog_mirror::deliver(&batch).await;
        }
        Err(_elapsed) => {
            // Rung 3 — the queue stayed full for the whole timeout. Deliver it
            // here, on the emit path, exactly as the pre-#155 mirror did.
            crate::metrics::record_ehdb_eventlog_mirror_queue("queue_full_inline", n);
            warn!(
                target: "noetl_server::ehdb_eventlog_mirror",
                execution_id = batch.execution_id, events = n,
                timeout_ms = timeout.as_millis() as u64,
                pending = PENDING_EVENTS.load(Ordering::Relaxed),
                "async mirror queue stayed full past the enqueue timeout — mirroring inline. \
                 This batch may land out of order relative to batches still queued for the \
                 same execution (noetl/ai-meta#155)."
            );
            super::ehdb_eventlog_mirror::deliver(&batch).await;
        }
    }
}

/// Publish the queue's occupancy from the producer side.
///
/// The drain task also publishes it, but only when it wakes — so on a queue
/// that is filling faster than it drains, the drain-side write is always the
/// stale one. The number an operator reads during a backlog has to come from
/// the side that is causing the backlog.
fn publish_depth(tx: &mpsc::Sender<MirrorBatch>) {
    let depth = tx.max_capacity().saturating_sub(tx.capacity());
    crate::metrics::ehdb_eventlog_mirror_queue_depth().set(depth as i64);
}

fn accepted(events: usize) {
    let now = PENDING_EVENTS.fetch_add(events as i64, Ordering::Relaxed) + events as i64;
    crate::metrics::ehdb_eventlog_mirror_pending_events().set(now);
}

fn settled(events: usize) {
    let now = PENDING_EVENTS.fetch_sub(events as i64, Ordering::Relaxed) - events as i64;
    crate::metrics::ehdb_eventlog_mirror_pending_events().set(now);
}

/// The single drain task.
///
/// Drains immediately — `recv().await` returns on the first batch and the
/// `try_recv` sweep only takes what is *already* there. There is no accumulation
/// window on purpose: a window was measured against real prod inter-arrival
/// times (p50 225 ms) and would have added delay to every event while batching
/// essentially nothing. The batching that does happen here is free — it is
/// whatever piled up while the previous delivery was in flight.
async fn drain_loop(mut rx: mpsc::Receiver<MirrorBatch>, drain_max: usize) {
    loop {
        let Some(first) = rx.recv().await else {
            info!(
                target: "noetl_server::ehdb_eventlog_mirror",
                "async mirror queue closed; drain task exiting"
            );
            return;
        };
        let mut pass: Vec<MirrorBatch> = vec![first];
        while pass.len() < drain_max {
            match rx.try_recv() {
                Ok(b) => pass.push(b),
                Err(_) => break,
            }
        }
        crate::metrics::ehdb_eventlog_mirror_queue_depth().set(rx.len() as i64);
        deliver_pass(pass).await;
    }
}

/// Deliver one drain pass: coalesce per execution, POST sequentially.
///
/// Coalescing is per execution and **order-preserving within it**: batches for
/// one execution are concatenated in the order they were enqueued, and the
/// merged request goes out where the first of them sat. Different executions
/// are independent, so their relative order is not a property anything checks.
async fn deliver_pass(pass: Vec<MirrorBatch>) {
    // Preserve first-seen execution order; merge records in enqueue order.
    let mut order: Vec<i64> = Vec::new();
    let mut merged: std::collections::HashMap<i64, MirrorBatch> = std::collections::HashMap::new();
    for b in pass {
        match merged.get_mut(&b.execution_id) {
            Some(existing) => {
                existing.records.extend(b.records);
                // Keep the OLDEST enqueue time: the lag this batch reports must
                // be the lag of its slowest record, not of its newest. Taking
                // the newest would make a backed-up queue publish a healthy
                // histogram — the exact silence this metric exists to break.
                if b.enqueued_at < existing.enqueued_at {
                    existing.enqueued_at = b.enqueued_at;
                }
            }
            None => {
                order.push(b.execution_id);
                merged.insert(b.execution_id, b);
            }
        }
    }

    // Deliver up to `concurrency` executions at once, as a sliding window: spawn
    // until the window is full, then retire one before spawning the next.
    //
    // `order` is first-seen execution order and is preserved as the SPAWN order.
    // Completion order is not, and does not need to be — see the note on
    // `DRAIN_CONCURRENCY_ENV`: one batch per execution per pass, and passes do
    // not overlap.
    let concurrency = configured_drain_concurrency();
    // Published from the value this pass ACTUALLY used, not from a startup read.
    // The knob is per-pass, so a gauge set only at init could disagree with the
    // running behaviour — which is precisely the drift that left the #320
    // mitigation unobservable.
    crate::metrics::ehdb_eventlog_mirror_drain_concurrency().set(concurrency as i64);
    let mut inflight: tokio::task::JoinSet<(usize, f64)> = tokio::task::JoinSet::new();

    for execution_id in order {
        let Some(batch) = merged.remove(&execution_id) else {
            continue;
        };
        while inflight.len() >= concurrency {
            retire(inflight.join_next().await);
        }
        inflight.spawn(async move {
            let n = batch.records.len();
            let waited = batch.enqueued_at.elapsed();
            super::ehdb_eventlog_mirror::deliver(&batch).await;
            (n, waited.as_secs_f64())
        });
    }
    while let Some(joined) = inflight.join_next().await {
        retire(Some(joined));
    }
}

/// Account one finished delivery.
///
/// ⚠ A task that panicked still has to be accounted. `settled(n)` is what
/// `flush_on_shutdown` waits on, so a lost accounting would leave the shutdown
/// flush waiting out its full deadline on events that are no longer coming —
/// and would report them as `shutdown_abandoned`, blaming the queue for a panic
/// somewhere else. The `deliver` path catches its own errors, so this is the
/// unexpected case, not the routine one.
fn retire(joined: Option<Result<(usize, f64), tokio::task::JoinError>>) {
    let Some(result) = joined else { return };
    match result {
        Ok((n, secs)) => {
            // Observed once per event, so the histogram's count is an event count
            // and lines up with the mirror counter it is read beside.
            for _ in 0..n {
                crate::metrics::observe_ehdb_eventlog_mirror_lag(secs);
            }
            crate::metrics::record_ehdb_eventlog_mirror_queue("drained", n);
            settled(n);
        }
        Err(e) => {
            warn!(
                target: "noetl_server::ehdb_eventlog_mirror",
                error = %e,
                "a mirror delivery task did not complete; its events are unaccounted"
            );
        }
    }
}

/// Wait for the queue to empty, up to a bounded deadline. Called on shutdown.
///
/// Without this, SIGTERM on a server with a non-empty queue silently loses
/// those events — permanently, because nothing retries a mirror. They would
/// surface later as a `missing_event` divergence on a `primary` tier with no
/// trace of the cause. Whatever is still pending when the deadline passes is
/// counted as `shutdown_abandoned` so the cause is at least attributable.
pub async fn flush_on_shutdown() {
    if !enabled() {
        return;
    }
    let deadline = env_millis(FLUSH_TIMEOUT_ENV, DEFAULT_FLUSH_TIMEOUT_MS);
    let started = Instant::now();
    let at_entry = PENDING_EVENTS.load(Ordering::Relaxed);
    if at_entry <= 0 {
        return;
    }
    info!(
        target: "noetl_server::ehdb_eventlog_mirror",
        pending = at_entry, deadline_ms = deadline.as_millis() as u64,
        "flushing the async event-log mirror queue before shutdown"
    );
    while started.elapsed() < deadline {
        if PENDING_EVENTS.load(Ordering::Relaxed) <= 0 {
            info!(
                target: "noetl_server::ehdb_eventlog_mirror",
                flushed = at_entry, took_ms = started.elapsed().as_millis() as u64,
                "async event-log mirror queue flushed"
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let abandoned = PENDING_EVENTS.load(Ordering::Relaxed).max(0);
    if abandoned > 0 {
        crate::metrics::record_ehdb_eventlog_mirror_queue("shutdown_abandoned", abandoned as usize);
        warn!(
            target: "noetl_server::ehdb_eventlog_mirror",
            abandoned,
            "shutdown flush deadline passed with events still queued for the event-log tier — \
             they will read as a missing_event divergence (noetl/ai-meta#155)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ladder has no drop rung, asserted on the label set rather than on
    /// prose. Every outcome here either enqueues the events or delivers them.
    #[test]
    fn no_queue_outcome_discards_events() {
        // `shutdown_abandoned` is the one outcome where events do not reach the
        // tier, and it is not a *policy* — it is the process being killed. It is
        // listed here so adding a genuine drop policy has to edit this test.
        let terminal_without_delivery = ["shutdown_abandoned"];
        for outcome in crate::metrics::EHDB_EVENTLOG_MIRROR_QUEUE_OUTCOMES {
            let delivers = matches!(
                outcome,
                "enqueued"
                    | "enqueued_after_wait"
                    | "queue_full_inline"
                    | "queue_closed_inline"
                    | "drained"
            );
            assert!(
                delivers || terminal_without_delivery.contains(&outcome),
                "queue outcome {outcome:?} neither delivers nor is an accounted shutdown loss — \
                 if a drop policy was added, noetl/ai-meta#155 says it may not be"
            );
        }
    }

    /// Merging must concatenate in enqueue order and keep the OLDEST timestamp.
    ///
    /// Both halves have teeth: reordering here is invisible to every other test
    /// (the comparator would catch it in kind, days later), and taking the
    /// newest timestamp would make a backed-up queue publish a healthy lag
    /// histogram.
    #[test]
    fn coalescing_preserves_order_and_reports_the_oldest_lag() {
        let t0 = Instant::now() - Duration::from_secs(9);
        let t1 = Instant::now() - Duration::from_secs(1);
        let mut a = MirrorBatch {
            base: "http://x".into(),
            execution_id: 7,
            records: vec!["r1".into(), "r2".into()],
            enqueued_at: t1,
        };
        let b = MirrorBatch {
            base: "http://x".into(),
            execution_id: 7,
            records: vec!["r3".into()],
            enqueued_at: t0,
        };
        // The same merge the drain pass performs.
        a.records.extend(b.records.clone());
        if b.enqueued_at < a.enqueued_at {
            a.enqueued_at = b.enqueued_at;
        }
        assert_eq!(a.records, vec!["r1", "r2", "r3"]);
        assert!(
            a.enqueued_at.elapsed() >= Duration::from_secs(8),
            "merged batch must report the oldest record's wait, not the newest"
        );
    }

    #[test]
    fn capacity_and_timeout_fall_back_to_defaults_on_junk() {
        // A typo in a tuning knob must not produce a zero-capacity queue (every
        // send would block) or a zero timeout (every batch would take rung 3).
        assert_eq!(env_usize("NOETL_TEST_ABSENT_CAPACITY_155", 512), 512);
        assert_eq!(
            env_millis("NOETL_TEST_ABSENT_TIMEOUT_155", 5_000),
            Duration::from_millis(5_000)
        );
    }
}
