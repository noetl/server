//! Take the projection mirror off the orchestrator's write path — a bounded
//! queue with backpressure, never a drop.
//!
//! **[noetl/ai-meta#265](https://github.com/noetl/ai-meta/issues/265) G3.**
//! The projection-tier twin of `ehdb_eventlog_mirror_queue` (#155 Option 3),
//! deliberately built to the same shape so one runbook covers both.
//!
//! # What this is worth
//!
//! [`super::ehdb_projection_mirror::mirror_snapshot`] is called from inside
//! `orch_snapshot::save`, and one of `save`'s two callers is the **inline
//! orchestrator self-write** in `trigger_orchestrator_inner`. So every snapshot
//! upsert on that path pays a relay round trip plus a tier append before the
//! trigger returns. That is the same shape #155 removed from the event log,
//! where it measured **78.6 ms → 0.1 ms per call** and moved a median warm
//! planner turn from 16.9 s to 13.0 s.
//!
//! None of that work is on the critical path of anything: the mirror is an
//! auxiliary verification copy, and the authoritative row has already committed
//! by the time it runs.
//!
//! # The pairing rule, enforced here rather than documented
//!
//! **An async mirror without a comparator lag-tolerance window is worse than no
//! async mirror.** With the window at 0, a snapshot still in flight makes the
//! tier legitimately behind the incumbent, and the comparator scores that as a
//! divergence — so arming the queue would manufacture the exact alert the tier
//! exists to make trustworthy. #155 states this as "set both or neither"; here
//! it is a startup check.
//!
//! [`init`] **refuses to arm** when `NOETL_EHDB_PROJECTION_MIRROR_ASYNC` is on
//! and `NOETL_EHDB_PROJECTION_PARITY_LAG_TOLERANCE_SECS` is 0. Refusing leaves
//! the process on the inline path, which is correct and merely slower; erroring
//! out would turn a tuning mistake into an outage, and arming anyway would turn
//! it into a false page. The refusal is loud: an `error!` line, and
//! `..._async_enabled` stays **0** so the running state is readable rather than
//! inferred from the variable someone set.
//!
//! # Why "never drop" is not a preference
//!
//! A dropped snapshot is not a lost metric. It is a revision the tier will never
//! hold, and — once B1's read path is serving — a revision the tier is *behind*
//! by, forever. The ladder is the same as the event log's and every rung keeps
//! the snapshot:
//!
//! 1. **`try_send`** — room on the queue, microseconds.
//! 2. **`reserve().await` with a timeout** — the queue is full, so the writer
//!    *waits*. Real backpressure: the producer slows to the drain rate, nothing
//!    is lost, order is preserved.
//! 3. **inline delivery** — the queue stayed full past the timeout. Delivered
//!    synchronously, i.e. the pre-G3 behaviour.
//!
//! There is no rung 4.
//!
//! ⚠ `reserve()`, **not** `send(value)`. `send` moves the value into the future,
//! so a timeout cancelling that future takes the snapshot with it — a silent
//! drop on the one path whose entire purpose is not to have one. #155's first
//! version used `send` and lost 28 of 60 records, with every counter still
//! reading correctly because the records were counted at the moment they were
//! lost.
//!
//! # Ordering, and what it is worth here
//!
//! One drain task, FIFO, delivered sequentially — so per execution the tier
//! receives snapshots in the order `save` produced them. That matters less than
//! it does for the event log (a snapshot is a full read model, not an
//! increment, and the reader takes the newest by version) but it is not free
//! either: the read path's monotonicity expectation and the comparator's
//! `order` divergence kind both read the store's sequence.
//!
//! **This module deliberately does not coalesce per execution.** It could —
//! only the newest version is ever read — but dropping intermediate revisions
//! is a behaviour change to what the tier *contains*, and it would have to be
//! gated and proven separately. The queue's job is to move the work off the hot
//! path, not to change what the work is.
//!
//! # Flag
//!
//! `NOETL_EHDB_PROJECTION_MIRROR_ASYNC`, default **off**. Off, `mirror_snapshot`
//! takes the same inline path it took before and this module's only effect is
//! three gauges reading 0.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{error, info, warn};

use super::ehdb_projection_mirror::SnapshotMirror;

/// Arm the async queue. Default off.
pub const ASYNC_ENV: &str = "NOETL_EHDB_PROJECTION_MIRROR_ASYNC";

/// Queue depth in **snapshots**. Default 512.
///
/// Sized to absorb a burst, not to hide a stall: past this the producer is meant
/// to feel it (rung 2), because a queue deep enough never to push back is a
/// queue whose lag can exceed the comparator's tolerance window with nothing
/// noticing.
pub const CAPACITY_ENV: &str = "NOETL_EHDB_PROJECTION_MIRROR_QUEUE_CAPACITY";
const DEFAULT_CAPACITY: usize = 512;

/// How long the writer waits for room before falling back to inline delivery.
/// Default 5000 ms — the same bound the inline relay already has, so the worst
/// case a caller can see is unchanged.
pub const ENQUEUE_TIMEOUT_ENV: &str = "NOETL_EHDB_PROJECTION_MIRROR_ENQUEUE_TIMEOUT_MS";
const DEFAULT_ENQUEUE_TIMEOUT_MS: u64 = 5_000;

/// Most snapshots one drain pass takes off the queue before delivering.
pub const DRAIN_MAX_ENV: &str = "NOETL_EHDB_PROJECTION_MIRROR_DRAIN_MAX";
const DEFAULT_DRAIN_MAX: usize = 64;

/// How long the shutdown flush waits for the queue to empty.
pub const FLUSH_TIMEOUT_ENV: &str = "NOETL_EHDB_PROJECTION_MIRROR_FLUSH_TIMEOUT_MS";
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

/// Snapshots accepted onto the queue and not yet durable.
///
/// An atomic beside the gauge rather than read off it: the shutdown flush needs
/// to *wait* on this, and a Prometheus gauge is a publication surface, not a
/// synchronisation primitive.
static PENDING: AtomicI64 = AtomicI64::new(0);

static QUEUE: OnceLock<Option<mpsc::Sender<SnapshotMirror>>> = OnceLock::new();

/// Is the queue armed in this process?
pub fn enabled() -> bool {
    queue().is_some()
}

fn queue() -> Option<&'static mpsc::Sender<SnapshotMirror>> {
    QUEUE.get().and_then(|q| q.as_ref())
}

/// The pairing check, as a pure function so it is testable without a process.
///
/// Returns the reason to refuse, or `None` to arm. See the module note: this is
/// #155's "set both or neither" turned from prose into a startup condition.
pub fn refusal_reason(async_requested: bool, lag_tolerance_secs: u64) -> Option<&'static str> {
    if !async_requested {
        return None;
    }
    if lag_tolerance_secs == 0 {
        return Some(
            "NOETL_EHDB_PROJECTION_MIRROR_ASYNC is on but \
             NOETL_EHDB_PROJECTION_PARITY_LAG_TOLERANCE_SECS is 0",
        );
    }
    None
}

/// Arm the queue and start the drain task. Idempotent; call once at startup.
///
/// Started from `main` rather than lazily on first mirror: a lazy start would
/// make "armed" depend on traffic, so the gauge saying whether this process is
/// async would read 0 on an idle server that is in fact configured for it.
pub fn init(lag_tolerance_secs: u64) {
    let requested = env_bool(ASYNC_ENV);

    if let Some(reason) = refusal_reason(requested, lag_tolerance_secs) {
        // Loud, and NOT fatal. The process is correct on the inline path, just
        // slower; killing it would turn a tuning mistake into an outage, and
        // arming anyway would make the comparator judge a healthy tier on its
        // own liveness and page for it.
        error!(
            target: "noetl_server::ehdb_projection_mirror",
            reason,
            "REFUSING to arm the async projection mirror — an async mirror with a zero \
             tolerance window makes the comparator score in-flight snapshots as divergence. \
             Staying on the inline path. Set both or neither (noetl/ai-meta#265 G3, #155)."
        );
        crate::metrics::ehdb_projection_mirror_async_enabled().set(0);
        let _ = QUEUE.set(None);
        return;
    }

    crate::metrics::ehdb_projection_mirror_async_enabled().set(i64::from(requested));
    if !requested {
        let _ = QUEUE.set(None);
        return;
    }

    let capacity = env_usize(CAPACITY_ENV, DEFAULT_CAPACITY);
    let drain_max = env_usize(DRAIN_MAX_ENV, DEFAULT_DRAIN_MAX);
    let (tx, rx) = mpsc::channel::<SnapshotMirror>(capacity);
    if QUEUE.set(Some(tx)).is_err() {
        // Already initialised. Do NOT start a second drain task — two drains on
        // one queue deliver concurrently and lose the ordering this module is
        // supposed to guarantee.
        return;
    }
    info!(
        target: "noetl_server::ehdb_projection_mirror",
        capacity, drain_max, lag_tolerance_secs,
        enqueue_timeout_ms = env_millis(ENQUEUE_TIMEOUT_ENV, DEFAULT_ENQUEUE_TIMEOUT_MS).as_millis() as u64,
        "async projection mirror queue ARMED — the mirror is off the orchestrator write path"
    );
    tokio::spawn(drain_loop(rx, drain_max));
}

/// Hand a snapshot to the queue, or deliver it inline if the queue cannot take
/// it. Returns after the snapshot is *accepted*, not after it is durable.
pub async fn submit(m: SnapshotMirror) {
    let Some(tx) = queue() else {
        // Unreachable via `mirror_snapshot`, which checks `enabled()` first —
        // but a stale read must still deliver the snapshot rather than discard it.
        crate::metrics::record_ehdb_projection_mirror_queue("queue_closed_inline");
        super::ehdb_projection_mirror::deliver(&m).await;
        return;
    };

    let m = match tx.try_send(m) {
        Ok(()) => {
            accepted();
            publish_depth(tx);
            crate::metrics::record_ehdb_projection_mirror_queue("enqueued");
            return;
        }
        Err(mpsc::error::TrySendError::Closed(m)) => {
            crate::metrics::record_ehdb_projection_mirror_queue("queue_closed_inline");
            warn!(
                target: "noetl_server::ehdb_projection_mirror",
                execution_id = m.execution_id,
                "async projection mirror queue is closed — the drain task is gone; \
                 mirroring inline"
            );
            super::ehdb_projection_mirror::deliver(&m).await;
            return;
        }
        Err(mpsc::error::TrySendError::Full(m)) => m,
    };

    // Rung 2 — backpressure. `reserve()`, never `send(m)`: see the module note.
    let timeout = env_millis(ENQUEUE_TIMEOUT_ENV, DEFAULT_ENQUEUE_TIMEOUT_MS);
    match tokio::time::timeout(timeout, tx.reserve()).await {
        Ok(Ok(permit)) => {
            permit.send(m);
            accepted();
            publish_depth(tx);
            crate::metrics::record_ehdb_projection_mirror_queue("enqueued_after_wait");
        }
        Ok(Err(_closed)) => {
            crate::metrics::record_ehdb_projection_mirror_queue("queue_closed_inline");
            super::ehdb_projection_mirror::deliver(&m).await;
        }
        Err(_elapsed) => {
            // Rung 3 — full for the whole timeout. Deliver here, as the pre-G3
            // mirror did.
            crate::metrics::record_ehdb_projection_mirror_queue("queue_full_inline");
            warn!(
                target: "noetl_server::ehdb_projection_mirror",
                execution_id = m.execution_id,
                timeout_ms = timeout.as_millis() as u64,
                pending = PENDING.load(Ordering::Relaxed),
                "async projection mirror queue stayed full past the enqueue timeout — \
                 mirroring inline. This snapshot may land out of order relative to \
                 snapshots still queued for the same execution (noetl/ai-meta#265 G3)."
            );
            super::ehdb_projection_mirror::deliver(&m).await;
        }
    }
}

/// Publish occupancy from the PRODUCER side.
///
/// The drain also publishes it, but only when it wakes — so on a queue filling
/// faster than it drains, the drain-side write is always the stale one. The
/// number an operator reads during a backlog has to come from the side causing
/// the backlog.
fn publish_depth(tx: &mpsc::Sender<SnapshotMirror>) {
    let depth = tx.max_capacity().saturating_sub(tx.capacity());
    crate::metrics::ehdb_projection_mirror_queue_depth().set(depth as i64);
}

fn accepted() {
    let now = PENDING.fetch_add(1, Ordering::Relaxed) + 1;
    crate::metrics::ehdb_projection_mirror_pending().set(now);
}

fn settled() {
    let now = PENDING.fetch_sub(1, Ordering::Relaxed) - 1;
    crate::metrics::ehdb_projection_mirror_pending().set(now);
}

/// The single drain task. Drains immediately; the `try_recv` sweep only takes
/// what is already there, so nothing waits to accumulate.
async fn drain_loop(mut rx: mpsc::Receiver<SnapshotMirror>, drain_max: usize) {
    loop {
        let Some(first) = rx.recv().await else {
            info!(
                target: "noetl_server::ehdb_projection_mirror",
                "async projection mirror queue closed; drain task exiting"
            );
            return;
        };
        let mut pass: Vec<SnapshotMirror> = vec![first];
        while pass.len() < drain_max {
            match rx.try_recv() {
                Ok(m) => pass.push(m),
                Err(_) => break,
            }
        }
        crate::metrics::ehdb_projection_mirror_queue_depth().set(rx.len() as i64);
        for m in &pass {
            super::ehdb_projection_mirror::deliver(m).await;
            crate::metrics::record_ehdb_projection_mirror_queue("drained");
            settled();
        }
    }
}

/// Wait for the queue to empty at shutdown.
///
/// A snapshot still queued when the process exits never reaches the tier, and
/// the comparator will report it as a divergence for as long as the row lives.
/// Bounded, and what is left is counted as `shutdown_abandoned` so the cause is
/// attributable rather than a mystery an hour later.
pub async fn flush() {
    if !enabled() {
        return;
    }
    let deadline = env_millis(FLUSH_TIMEOUT_ENV, DEFAULT_FLUSH_TIMEOUT_MS);
    let started = std::time::Instant::now();
    while PENDING.load(Ordering::Relaxed) > 0 && started.elapsed() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let left = PENDING.load(Ordering::Relaxed);
    if left > 0 {
        crate::metrics::ehdb_projection_mirror_queue_total()
            .with_label_values(&["shutdown_abandoned"])
            .inc_by(left as u64);
        warn!(
            target: "noetl_server::ehdb_projection_mirror",
            abandoned = left,
            flush_ms = deadline.as_millis() as u64,
            "shutdown flush timed out with snapshots still queued — these will show as \
             projection-tier divergences"
        );
    } else {
        info!(
            target: "noetl_server::ehdb_projection_mirror",
            flush_ms = started.elapsed().as_millis() as u64,
            "async projection mirror queue flushed at shutdown"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pairing rule. This is the whole of G3's safety argument, so it is
    /// asserted rather than left to the module note.
    #[test]
    fn async_without_a_tolerance_window_is_refused() {
        assert!(
            refusal_reason(true, 0).is_some(),
            "async armed with a 0 window must be refused — it makes the comparator \
             score every in-flight snapshot as divergence"
        );
    }

    /// Both halves of the pair, and the two ways of having neither.
    ///
    /// The positive control matters as much as the refusal: without it, a
    /// `refusal_reason` that refused unconditionally would satisfy the test
    /// above and silently make the async path unreachable — inert, and
    /// indistinguishable from a flag nobody set.
    #[test]
    fn the_pair_arms_and_the_absence_of_it_is_not_a_refusal() {
        assert!(
            refusal_reason(true, 30).is_none(),
            "async + a real window must ARM; refusing here makes G3 permanently inert"
        );
        assert!(
            refusal_reason(false, 0).is_none(),
            "async off is the default and is not a misconfiguration"
        );
        assert!(
            refusal_reason(false, 30).is_none(),
            "a window with no async mirror is harmless — it only widens what the \
             comparator forgives"
        );
    }

    /// Every queue outcome the code can emit must be in the pinned set.
    ///
    /// A pinned set that omits one value reintroduces the absent-series bug on
    /// exactly that value, while the rest read 0 and look complete.
    #[test]
    fn every_queue_outcome_is_pinned() {
        for label in [
            "enqueued",
            "enqueued_after_wait",
            "queue_full_inline",
            "queue_closed_inline",
            "drained",
            "shutdown_abandoned",
        ] {
            assert!(
                crate::metrics::EHDB_PROJECTION_MIRROR_QUEUE_OUTCOMES.contains(&label),
                "{label} is emitted but not pinned; its series would be absent until it fires"
            );
        }
        assert_eq!(
            crate::metrics::EHDB_PROJECTION_MIRROR_QUEUE_OUTCOMES.len(),
            6,
            "the pinned set must be exactly the six the ladder can produce"
        );
    }

    /// The source must not contain a bare `tx.send(` on the enqueue path.
    ///
    /// Counting CODE, not naming a function: #155 lost 28 of 60 records to
    /// exactly this, and every counter still read correctly because the records
    /// were counted at the moment they were lost. A comment explaining the
    /// hazard must not satisfy the check, and the check must not be satisfied by
    /// deleting that comment either — so `//` is stripped first, with a positive
    /// control that the stripper left the real `reserve()` behind.
    #[test]
    fn the_enqueue_path_uses_reserve_not_send() {
        let whole = include_str!("ehdb_projection_mirror_queue.rs");
        let code_half = &whole[..whole
            .find("mod tests {")
            .expect("the test module must still be the tail of this file")];
        let code: String = code_half
            .lines()
            .map(|l| match l.find("//") {
                Some(i) => &l[..i],
                None => l,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            code.contains("tx.reserve()"),
            "the comment stripper ate the real enqueue; this guard proves nothing"
        );
        assert!(
            !code.contains("tx.send("),
            "the enqueue path must use `reserve()`. `send(value)` moves the snapshot into \
             the future, so a cancelled timeout takes it with it — a silent drop on the \
             one path whose purpose is not to have one."
        );
    }
}
