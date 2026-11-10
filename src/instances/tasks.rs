//! **Cross-instance task handoff, pull-based.**
//!
//! Instance A asks the control plane to have instance B run a playbook. The control plane
//! queues the task in B's inbox. B **pulls** it (a long-poll it opens itself, so B needs no
//! inbound connectivity), runs it, and posts the result. A gets the result by polling the
//! task (also outbound), or by a callback when A asked for one and its origin is allowed.
//!
//! # Claims and redelivery
//!
//! A claim leases the task to B for [`DEFAULT_VISIBILITY_MS`] and hands out a fresh
//! `claim_id`. A result must quote the current `claim_id`. If B dies mid-run, the lease
//! lapses, the next claim redelivers the task with a new `claim_id`, and a late result from
//! the first attempt is answered `stale` rather than overwriting the second. That is the
//! container-callback rule: stale and duplicate completions are accepted and ignored, never a
//! 4xx a racing worker would retry.
//!
//! # Authority never widens across the hop
//!
//! - The target must be live, must say `accepts_tasks`, and must list the playbook path in
//!   its own `playbooks` globs. The server enforces the target's stated scope; the target
//!   enforces it again before it runs anything.
//! - Only the target may claim or complete a task; only the two parties may read it.
//! - The playbook travels by reference (path, optional sha256). The target runs its own copy,
//!   and refuses when the hash does not match.
//!
//! # Durability
//!
//! The board is the working set, in memory. With `NOETL_INSTANCE_STORE=postgres` every change
//! is also written through to `noetl.instance_tasks` ([`super::store`]) and the board is
//! rebuilt from that table at start, so queued and claimed tasks, their claims and finished
//! results survive a server restart. Without it the board is soft state: a restart loses
//! queued tasks and the submitter sees `unknown task`. Callback tokens are never persisted, so
//! a callback due after a restart is not sent; the submitter's poll still gets the result.

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const DEFAULT_VISIBILITY_MS: i64 = 5 * 60 * 1000;
pub const DEFAULT_DEADLINE_MS: i64 = 15 * 60 * 1000;
pub const MAX_DEADLINE_MS: i64 = 24 * 60 * 60 * 1000;
pub const MAX_QUEUED_PER_TARGET: usize = 256;
pub const MAX_TASKS: usize = 50_000;
pub const MAX_PAYLOAD_BYTES: usize = 128 * 1024;
pub const MAX_RESULT_BYTES: usize = 1024 * 1024;
/// A task is kept this long after it ends, so the submitter can still read the result.
pub const KEEP_DONE_MS: i64 = 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Queued,
    Claimed,
    Completed,
    Failed,
    Expired,
}

impl State {
    pub fn is_terminal(self) -> bool {
        matches!(self, State::Completed | State::Failed | State::Expired)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaybookRef {
    pub path: String,
    /// Hex SHA-256 of the playbook text the submitter means. Optional.
    #[serde(default)]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Task {
    pub task_id: String,
    pub from: String,
    pub to: String,
    pub playbook: PlaybookRef,
    pub payload: Value,
    pub state: State,
    pub created_ms: i64,
    pub deadline_ms: i64,
    pub attempts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claim_id: Option<String>,
    #[serde(skip)]
    pub lease_until_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_ms: Option<i64>,
    /// The submitter's callback: URL and the single-use bearer token. Never serialised.
    #[serde(skip)]
    pub reply: Option<(reqwest::Url, String)>,
    /// Bumped on every change; the durable store keeps only the newest revision.
    #[serde(skip)]
    pub rev: u64,
}

/// A task as the durable store keeps it: everything but the callback token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stored {
    pub task_id: String,
    pub from: String,
    pub to: String,
    pub playbook: PlaybookRef,
    pub payload: Value,
    pub state: State,
    pub created_ms: i64,
    pub deadline_ms: i64,
    pub attempts: u32,
    pub claim_id: Option<String>,
    pub lease_until_ms: i64,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub ended_ms: Option<i64>,
    pub rev: u64,
}

impl From<&Task> for Stored {
    fn from(t: &Task) -> Self {
        Stored {
            task_id: t.task_id.clone(),
            from: t.from.clone(),
            to: t.to.clone(),
            playbook: t.playbook.clone(),
            payload: t.payload.clone(),
            state: t.state,
            created_ms: t.created_ms,
            deadline_ms: t.deadline_ms,
            attempts: t.attempts,
            claim_id: t.claim_id.clone(),
            lease_until_ms: t.lease_until_ms,
            result: t.result.clone(),
            error: t.error.clone(),
            ended_ms: t.ended_ms,
            rev: t.rev,
        }
    }
}

impl From<Stored> for Task {
    fn from(s: Stored) -> Self {
        Task {
            task_id: s.task_id,
            from: s.from,
            to: s.to,
            playbook: s.playbook,
            payload: s.payload,
            state: s.state,
            created_ms: s.created_ms,
            deadline_ms: s.deadline_ms,
            attempts: s.attempts,
            claim_id: s.claim_id,
            lease_until_ms: s.lease_until_ms,
            result: s.result,
            error: s.error,
            ended_ms: s.ended_ms,
            reply: None,
            rev: s.rev,
        }
    }
}

/// What changed since the last [`Board::take_changes`]: rows to upsert and ids to delete.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Changes {
    pub upserts: Vec<Stored>,
    pub removed: Vec<String>,
}

impl Changes {
    pub fn is_empty(&self) -> bool {
        self.upserts.is_empty() && self.removed.is_empty()
    }
}

impl Task {
    /// What the submitter and the target may read.
    pub fn view(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or(Value::Null);
        if let Some(o) = v.as_object_mut() {
            // The claim id is the target's; the submitter has no use for it.
            o.remove("claim_id");
        }
        v
    }
}

/// What the target receives from a claim.
#[derive(Debug, Clone, Serialize)]
pub struct Claimed {
    pub task_id: String,
    pub claim_id: String,
    pub from: String,
    pub playbook: PlaybookRef,
    pub payload: Value,
    pub deadline_ms: i64,
    pub attempt: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Completion {
    /// The result was recorded. A callback is due if the task carries one.
    Recorded,
    /// The task had already ended. Accepted and ignored.
    Duplicate,
    /// A newer claim owns the task. Accepted and ignored.
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardError {
    pub status: u16,
    pub reason: String,
}

fn err(status: u16, reason: impl Into<String>) -> BoardError {
    BoardError {
        status,
        reason: reason.into(),
    }
}

pub fn new_id(prefix: &str) -> String {
    use rand::RngCore;
    let mut b = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut b);
    format!("{prefix}-{}", hex::encode(b))
}

#[derive(Default)]
pub struct Board {
    tasks: HashMap<String, Task>,
    inbox: HashMap<String, VecDeque<String>>,
    pub visibility_ms: i64,
    changed: HashSet<String>,
    removed: HashSet<String>,
}

pub struct NewTask {
    pub from: String,
    pub to: String,
    pub playbook: PlaybookRef,
    pub payload: Value,
    pub deadline_ms: Option<i64>,
    pub reply: Option<(reqwest::Url, String)>,
}

impl Board {
    pub fn new(visibility_ms: i64) -> Self {
        Board {
            visibility_ms,
            ..Default::default()
        }
    }

    pub fn submit(&mut self, t: NewTask, now: i64) -> Result<String, BoardError> {
        if t.playbook.path.is_empty()
            || t.playbook.path.len() > 512
            || t.playbook.path.contains("..")
        {
            return Err(err(
                400,
                "playbook.path must be 1..=512 characters with no `..`",
            ));
        }
        if let Some(h) = &t.playbook.sha256 {
            if h.len() != 64 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(err(400, "playbook.sha256 must be 64 hex characters"));
            }
        }
        let size = serde_json::to_vec(&t.payload)
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
        if size > MAX_PAYLOAD_BYTES {
            return Err(err(
                413,
                format!("payload is {size} bytes; the limit is {MAX_PAYLOAD_BYTES}"),
            ));
        }
        self.prune(now);
        if self.tasks.len() >= MAX_TASKS {
            return Err(err(503, "the task board is full; retry later"));
        }
        let q = self.inbox.entry(t.to.clone()).or_default();
        if q.len() >= MAX_QUEUED_PER_TARGET {
            return Err(err(
                429,
                format!("{} has {MAX_QUEUED_PER_TARGET} tasks waiting", t.to),
            ));
        }
        let deadline = t
            .deadline_ms
            .unwrap_or(DEFAULT_DEADLINE_MS)
            .clamp(1000, MAX_DEADLINE_MS);
        let id = new_id("rt");
        q.push_back(id.clone());
        self.tasks.insert(
            id.clone(),
            Task {
                task_id: id.clone(),
                from: t.from,
                to: t.to,
                playbook: t.playbook,
                payload: t.payload,
                state: State::Queued,
                created_ms: now,
                deadline_ms: now + deadline,
                attempts: 0,
                claim_id: None,
                lease_until_ms: 0,
                result: None,
                error: None,
                ended_ms: None,
                reply: t.reply,
                rev: 1,
            },
        );
        self.changed.insert(id.clone());
        Ok(id)
    }

    /// Lease up to `max` tasks to `instance`: queued ones, and claimed ones whose lease lapsed.
    pub fn claim(&mut self, instance: &str, max: usize, now: i64) -> Vec<Claimed> {
        let mut out = Vec::new();
        let Some(q) = self.inbox.get_mut(instance) else {
            return out;
        };
        for id in q.iter() {
            if out.len() >= max {
                break;
            }
            let Some(t) = self.tasks.get_mut(id) else {
                continue;
            };
            let free =
                t.state == State::Queued || (t.state == State::Claimed && t.lease_until_ms <= now);
            if !free || now >= t.deadline_ms {
                continue;
            }
            let claim = new_id("claim");
            t.state = State::Claimed;
            t.attempts += 1;
            t.claim_id = Some(claim.clone());
            t.lease_until_ms = (now + self.visibility_ms).min(t.deadline_ms);
            t.rev += 1;
            self.changed.insert(t.task_id.clone());
            out.push(Claimed {
                task_id: t.task_id.clone(),
                claim_id: claim,
                from: t.from.clone(),
                playbook: t.playbook.clone(),
                payload: t.payload.clone(),
                deadline_ms: t.deadline_ms,
                attempt: t.attempts,
            });
        }
        out
    }

    /// Lease one specific task to `instance` (push delivery). `None` if it is not claimable.
    pub fn claim_one(&mut self, instance: &str, task_id: &str, now: i64) -> Option<Claimed> {
        let t = self.tasks.get_mut(task_id)?;
        let free =
            t.state == State::Queued || (t.state == State::Claimed && t.lease_until_ms <= now);
        if t.to != instance || !free || now >= t.deadline_ms {
            return None;
        }
        let claim = new_id("claim");
        t.state = State::Claimed;
        t.attempts += 1;
        t.claim_id = Some(claim.clone());
        t.lease_until_ms = (now + self.visibility_ms).min(t.deadline_ms);
        t.rev += 1;
        self.changed.insert(t.task_id.clone());
        Some(Claimed {
            task_id: t.task_id.clone(),
            claim_id: claim,
            from: t.from.clone(),
            playbook: t.playbook.clone(),
            payload: t.payload.clone(),
            deadline_ms: t.deadline_ms,
            attempt: t.attempts,
        })
    }

    /// Give a claim back (a push that was not accepted): the task is queued again at once,
    /// instead of waiting for the lease to lapse. No-op unless `claim_id` is still current.
    pub fn release(&mut self, task_id: &str, claim_id: &str) -> bool {
        let Some(t) = self.tasks.get_mut(task_id) else {
            return false;
        };
        if t.state != State::Claimed || t.claim_id.as_deref() != Some(claim_id) {
            return false;
        }
        t.state = State::Queued;
        t.claim_id = None;
        t.lease_until_ms = 0;
        t.rev += 1;
        self.changed.insert(task_id.to_string());
        true
    }

    /// Does `instance` have anything claimable now?
    pub fn has_work(&self, instance: &str, now: i64) -> bool {
        self.inbox.get(instance).is_some_and(|q| {
            q.iter().filter_map(|id| self.tasks.get(id)).any(|t| {
                now < t.deadline_ms
                    && (t.state == State::Queued
                        || (t.state == State::Claimed && t.lease_until_ms <= now))
            })
        })
    }

    pub fn complete(
        &mut self,
        instance: &str,
        task_id: &str,
        claim_id: &str,
        outcome: Result<Value, String>,
        now: i64,
    ) -> Result<Completion, BoardError> {
        let t = self
            .tasks
            .get_mut(task_id)
            .ok_or_else(|| err(404, "unknown task"))?;
        if t.to != instance {
            return Err(err(404, "unknown task"));
        }
        if t.state.is_terminal() {
            return Ok(Completion::Duplicate);
        }
        if t.claim_id.as_deref() != Some(claim_id) {
            return Ok(Completion::Stale);
        }
        match outcome {
            Ok(result) => {
                let size = serde_json::to_vec(&result)
                    .map(|v| v.len())
                    .unwrap_or(usize::MAX);
                if size > MAX_RESULT_BYTES {
                    t.state = State::Failed;
                    t.error = Some(format!(
                        "result is {size} bytes; the limit is {MAX_RESULT_BYTES}"
                    ));
                } else {
                    t.state = State::Completed;
                    t.result = Some(result);
                }
            }
            Err(e) => {
                t.state = State::Failed;
                t.error = Some(e.chars().take(4096).collect());
            }
        }
        t.ended_ms = Some(now);
        t.rev += 1;
        self.changed.insert(task_id.to_string());
        if let Some(q) = self.inbox.get_mut(instance) {
            q.retain(|id| id != task_id);
        }
        Ok(Completion::Recorded)
    }

    /// Read a task as `instance`, which must be its submitter or its target.
    pub fn get(&self, instance: &str, task_id: &str) -> Result<&Task, BoardError> {
        match self.tasks.get(task_id) {
            Some(t) if t.from == instance || t.to == instance => Ok(t),
            _ => Err(err(404, "unknown task")),
        }
    }

    /// Mark tasks past their deadline expired. Returns them, so their callbacks can be sent.
    pub fn expire(&mut self, now: i64) -> Vec<Task> {
        let mut out = Vec::new();
        for t in self.tasks.values_mut() {
            if !t.state.is_terminal() && now >= t.deadline_ms {
                t.state = State::Expired;
                t.error = Some(format!(
                    "{} did not finish the task before its deadline",
                    t.to
                ));
                t.ended_ms = Some(now);
                t.rev += 1;
                out.push(t.clone());
            }
        }
        for t in &out {
            self.changed.insert(t.task_id.clone());
            if let Some(q) = self.inbox.get_mut(&t.to) {
                q.retain(|id| id != &t.task_id);
            }
        }
        out
    }

    /// Forget tasks that ended more than [`KEEP_DONE_MS`] ago.
    pub fn prune(&mut self, now: i64) {
        let gone: Vec<String> = self
            .tasks
            .values()
            .filter(|t| t.ended_ms.is_some_and(|e| now - e >= KEEP_DONE_MS))
            .map(|t| t.task_id.clone())
            .collect();
        for id in gone {
            self.tasks.remove(&id);
            self.changed.remove(&id);
            self.removed.insert(id);
        }
        self.inbox.retain(|_, q| !q.is_empty());
    }

    /// Everything that changed since the last call, for the durable store.
    pub fn take_changes(&mut self) -> Changes {
        let upserts = self
            .changed
            .drain()
            .filter_map(|id| self.tasks.get(&id).map(Stored::from))
            .collect();
        Changes {
            upserts,
            removed: self.removed.drain().collect(),
        }
    }

    /// Put changes the store could not write back, so the next write retries them.
    pub fn retry_changes(&mut self, c: &Changes) {
        for s in &c.upserts {
            if self.tasks.contains_key(&s.task_id) {
                self.changed.insert(s.task_id.clone());
            }
        }
        self.removed.extend(c.removed.iter().cloned());
    }

    /// Forget a task outright (a submit whose write failed). Not recorded as a change.
    pub fn discard(&mut self, task_id: &str) {
        if let Some(t) = self.tasks.remove(task_id) {
            if let Some(q) = self.inbox.get_mut(&t.to) {
                q.retain(|id| id != task_id);
            }
        }
        self.changed.remove(task_id);
    }

    /// Rebuild the working set from the store at start. Tasks already on the board win;
    /// non-terminal tasks rejoin their target's inbox in submission order.
    pub fn restore(&mut self, rows: Vec<Stored>) -> usize {
        let mut rows: Vec<Stored> = rows
            .into_iter()
            .filter(|r| !self.tasks.contains_key(&r.task_id))
            .collect();
        rows.sort_by_key(|r| (r.created_ms, r.task_id.clone()));
        let n = rows.len();
        for r in rows {
            if !r.state.is_terminal() {
                self.inbox
                    .entry(r.to.clone())
                    .or_default()
                    .push_back(r.task_id.clone());
            }
            self.tasks.insert(r.task_id.clone(), Task::from(r));
        }
        n
    }

    /// Take the callback for an ended task, so it is sent at most once.
    pub fn take_reply(&mut self, task_id: &str) -> Option<((reqwest::Url, String), Value)> {
        let t = self.tasks.get_mut(task_id)?;
        if !t.state.is_terminal() {
            return None;
        }
        let reply = t.reply.take()?;
        Some((reply, t.view()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const A: &str = "ni-0000000000000000000000000000000a";
    const B: &str = "ni-0000000000000000000000000000000b";

    fn new(board: &mut Board, now: i64) -> String {
        board
            .submit(
                NewTask {
                    from: A.into(),
                    to: B.into(),
                    playbook: PlaybookRef {
                        path: "edge/probe".into(),
                        sha256: None,
                    },
                    payload: json!({"target": "10.0.0.1"}),
                    deadline_ms: Some(60_000),
                    reply: None,
                },
                now,
            )
            .unwrap()
    }

    #[test]
    fn the_target_pulls_runs_and_completes_once() {
        let mut b = Board::new(10_000);
        let id = new(&mut b, 0);
        assert!(
            b.claim(A, 10, 1).is_empty(),
            "only the target's inbox holds it"
        );
        let c = b.claim(B, 10, 1);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].payload["target"], "10.0.0.1");
        assert!(
            b.claim(B, 10, 2).is_empty(),
            "leased, so not handed out twice"
        );
        assert_eq!(
            b.complete(B, &id, &c[0].claim_id, Ok(json!({"up": true})), 3)
                .unwrap(),
            Completion::Recorded
        );
        assert_eq!(
            b.complete(B, &id, &c[0].claim_id, Ok(json!({})), 4)
                .unwrap(),
            Completion::Duplicate
        );
        let t = b.get(A, &id).unwrap();
        assert_eq!(t.state, State::Completed);
        assert_eq!(t.result, Some(json!({"up": true})));
        assert!(t.view().get("claim_id").is_none());
    }

    #[test]
    fn a_lapsed_lease_redelivers_and_the_first_attempt_goes_stale() {
        let mut b = Board::new(10_000);
        let id = new(&mut b, 0);
        let first = b.claim(B, 1, 0).remove(0);
        assert!(!b.has_work(B, 9_999));
        assert!(b.has_work(B, 10_000));
        let second = b.claim(B, 1, 10_000).remove(0);
        assert_eq!(second.attempt, 2);
        assert_eq!(
            b.complete(B, &id, &first.claim_id, Ok(json!(1)), 10_001)
                .unwrap(),
            Completion::Stale
        );
        assert_eq!(
            b.complete(B, &id, &second.claim_id, Ok(json!(2)), 10_002)
                .unwrap(),
            Completion::Recorded
        );
        assert_eq!(b.get(B, &id).unwrap().result, Some(json!(2)));
    }

    #[test]
    fn only_the_parties_may_see_it_and_only_the_target_may_finish_it() {
        let mut b = Board::new(10_000);
        let id = new(&mut b, 0);
        let c = b.claim(B, 1, 0).remove(0);
        let stranger = "ni-0000000000000000000000000000000c";
        assert_eq!(b.get(stranger, &id).unwrap_err().status, 404);
        assert_eq!(
            b.complete(A, &id, &c.claim_id, Ok(json!(1)), 1)
                .unwrap_err()
                .status,
            404
        );
    }

    #[test]
    fn a_task_past_its_deadline_expires_and_its_callback_is_taken_once() {
        let mut b = Board::new(10_000);
        let url = reqwest::Url::parse("https://a.example/hook").unwrap();
        let id = b
            .submit(
                NewTask {
                    from: A.into(),
                    to: B.into(),
                    playbook: PlaybookRef {
                        path: "edge/probe".into(),
                        sha256: None,
                    },
                    payload: json!({}),
                    deadline_ms: Some(5_000),
                    reply: Some((url, "tok".into())),
                },
                0,
            )
            .unwrap();
        assert!(b.take_reply(&id).is_none(), "not ended yet");
        let expired = b.expire(5_000);
        assert_eq!(expired.len(), 1);
        assert!(b.claim(B, 1, 5_001).is_empty());
        let (_, view) = b.take_reply(&id).unwrap();
        assert_eq!(view["state"], "expired");
        assert!(
            b.take_reply(&id).is_none(),
            "a callback is sent at most once"
        );
        b.prune(5_000 + KEEP_DONE_MS);
        assert_eq!(b.get(A, &id).unwrap_err().status, 404);
    }

    #[test]
    fn inputs_are_bounded() {
        let mut b = Board::new(10_000);
        let bad = |path: &str, sha: Option<&str>, payload: Value| NewTask {
            from: A.into(),
            to: B.into(),
            playbook: PlaybookRef {
                path: path.into(),
                sha256: sha.map(str::to_string),
            },
            payload,
            deadline_ms: None,
            reply: None,
        };
        assert_eq!(
            b.submit(bad("../etc", None, json!({})), 0)
                .unwrap_err()
                .status,
            400
        );
        assert_eq!(
            b.submit(bad("x", Some("abc"), json!({})), 0)
                .unwrap_err()
                .status,
            400
        );
        let big = json!({"x": "y".repeat(MAX_PAYLOAD_BYTES)});
        assert_eq!(b.submit(bad("x", None, big), 0).unwrap_err().status, 413);
        for _ in 0..MAX_QUEUED_PER_TARGET {
            b.submit(bad("x", None, json!({})), 0).unwrap();
        }
        assert_eq!(
            b.submit(bad("x", None, json!({})), 0).unwrap_err().status,
            429
        );
    }
    #[test]
    fn changes_are_tracked_and_a_restore_rebuilds_the_inbox() {
        let mut b = Board::new(1000);
        let now = 1_000_000;
        let t1 = new(&mut b, now);
        let t2 = new(&mut b, now + 1);
        let c = b.take_changes();
        assert_eq!(c.upserts.len(), 2);
        assert!(b.take_changes().is_empty(), "drained");
        let claimed = b.claim(B, 1, now + 2);
        assert_eq!(claimed[0].task_id, t1);
        let c = b.take_changes();
        assert_eq!(c.upserts.len(), 1);
        assert_eq!(c.upserts[0].rev, 2);
        assert_eq!(c.upserts[0].state, State::Claimed);

        // Rebuild from what was written: the claim survives, t2 is still claimable.
        let mut rows: Vec<Stored> = b.tasks.values().map(Stored::from).collect();
        rows.reverse();
        let mut r = Board::new(1000);
        assert_eq!(r.restore(rows), 2);
        let again = r.claim(B, 4, now + 3);
        assert_eq!(again.len(), 1, "the claimed task keeps its lease");
        assert_eq!(again[0].task_id, t2);
        let done = r
            .complete(
                B,
                &t1,
                &claimed[0].claim_id,
                Ok(json!({"ok": true})),
                now + 4,
            )
            .unwrap();
        assert_eq!(
            done,
            Completion::Recorded,
            "a pre-restart claim id still completes"
        );
        assert!(
            r.take_reply(&t1).is_none(),
            "callback tokens are not restored"
        );
    }

    #[test]
    fn pruned_tasks_are_reported_removed_and_failed_writes_retry() {
        let mut b = Board::new(1000);
        let t = new(&mut b, 0);
        let c = b.claim(B, 1, 1);
        b.complete(B, &t, &c[0].claim_id, Ok(json!(1)), 2).unwrap();
        let ch = b.take_changes();
        b.retry_changes(&ch);
        assert_eq!(b.take_changes().upserts.len(), 1, "retried");
        b.prune(2 + KEEP_DONE_MS);
        let ch = b.take_changes();
        assert_eq!(ch.removed, vec![t.clone()]);
        assert!(ch.upserts.is_empty());
    }

    #[test]
    fn discard_drops_a_task_without_a_change() {
        let mut b = Board::new(1000);
        let t = new(&mut b, 0);
        b.discard(&t);
        assert!(b.take_changes().is_empty());
        assert!(b.claim(B, 1, 1).is_empty());
    }
}
