//! Push completion to waiters (noetl/ai-meta#559, Stage 2a of the
//! latency-reduction plan, RFC #556).
//!
//! A `kind: playbook` step that awaits its child (`return_result`) today sleeps
//! `poll_interval` (default 2 s) BEFORE its first `/status` poll and between
//! polls, so the parent learns of a child that finished in ~70 ms about 2 s
//! later (kind harness #557: `hop.notice` p50 1.95 s, per hop). With
//! `NOETL_EXECUTION_WAIT` the server serves
//! `GET /api/executions/{id}/wait?timeout=s`, a long poll woken the moment a
//! terminal row for that execution lands in `noetl.event` (the table `/status`
//! reads), and the tool waits on it instead of sleeping.
//!
//! Waking on the WRITE, not the emit: under publish-only ingest a terminal event
//! is published first and materialized later, and `/status` reads the
//! materialized table. Waking on the publish would hand the waiter a `/status`
//! that still says RUNNING. So the two places rows become readable notify:
//! `event_write::insert_rows` (synchronous ingest) and the materializer's
//! `events_project`.
//!
//! The table is process-local. A wake the process misses (another replica wrote
//! the row, or a writer this module does not hook) costs at most the handler's
//! 1 s re-check, never the full timeout.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::Notify;

type Table = Mutex<HashMap<i64, (Arc<Notify>, usize)>>;

fn table() -> &'static Table {
    static T: OnceLock<Table> = OnceLock::new();
    T.get_or_init(Default::default)
}

/// A live subscription; dropping it unregisters.
pub struct Waiter {
    execution_id: i64,
    notify: Arc<Notify>,
}

impl Waiter {
    /// The notifier to wait on.
    pub fn notify(&self) -> &Notify {
        &self.notify
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        let mut t = table().lock().unwrap();
        if let Some(entry) = t.get_mut(&self.execution_id) {
            entry.1 = entry.1.saturating_sub(1);
            if entry.1 == 0 {
                t.remove(&self.execution_id);
            }
        }
    }
}

/// Subscribe to wakes for `execution_id`.
pub fn subscribe(execution_id: i64) -> Waiter {
    let mut t = table().lock().unwrap();
    let entry = t
        .entry(execution_id)
        .or_insert_with(|| (Arc::new(Notify::new()), 0));
    entry.1 += 1;
    Waiter {
        execution_id,
        notify: entry.0.clone(),
    }
}

/// The event types after which an execution's `/status` can turn terminal.
pub fn is_terminal_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "playbook.completed"
            | "playbook_completed"
            | "playbook.failed"
            | "playbook_failed"
            | "playbook.cancelled"
            | "playbook_cancelled"
            | "execution.cancelled"
            | "execution_cancelled"
    )
}

/// Wake every waiter of an execution that just had a terminal row written.
/// Cheap when nobody waits (one map lookup per terminal row).
pub fn notify_written<'a>(rows: impl IntoIterator<Item = (i64, &'a str)>) {
    let t = table().lock().unwrap();
    if t.is_empty() {
        return;
    }
    for (execution_id, event_type) in rows {
        if is_terminal_event(event_type) {
            if let Some((n, _)) = t.get(&execution_id) {
                n.notify_waiters();
                crate::metrics::record_execution_wait("woken");
            }
        }
    }
}

/// Live subscriptions (tests).
pub fn waiter_count(execution_id: i64) -> usize {
    table()
        .lock()
        .unwrap()
        .get(&execution_id)
        .map_or(0, |e| e.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_terminal_write_wakes_a_waiter_and_drop_unregisters() {
        let w = subscribe(4242);
        assert_eq!(waiter_count(4242), 1);
        {
            let notified = w.notify().notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            notify_written([(4242, "command.completed")]);
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(20), notified.as_mut())
                    .await
                    .is_err(),
                "a non-terminal row does not wake"
            );
            notify_written([(4242, "playbook.completed")]);
            tokio::time::timeout(std::time::Duration::from_millis(200), notified)
                .await
                .expect("a terminal row wakes the waiter");
        }
        drop(w);
        assert_eq!(waiter_count(4242), 0);
    }

    #[test]
    fn terminal_types() {
        assert!(is_terminal_event("playbook.failed"));
        assert!(is_terminal_event("playbook_completed"));
        assert!(!is_terminal_event("call.done"));
    }
}
