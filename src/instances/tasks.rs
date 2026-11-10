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
//! # Soft state
//!
//! The board is in memory, like the registry. A server restart loses queued tasks; the
//! submitter sees `unknown task` on its next poll and may resubmit. A durable inbox on an
//! EHDB feed subject (RFC §2.3) is the follow-up.

use std::collections::{HashMap, VecDeque};

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
            },
        );
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
                out.push(t.clone());
            }
        }
        for t in &out {
            if let Some(q) = self.inbox.get_mut(&t.to) {
                q.retain(|id| id != &t.task_id);
            }
        }
        out
    }

    /// Forget tasks that ended more than [`KEEP_DONE_MS`] ago.
    pub fn prune(&mut self, now: i64) {
        self.tasks
            .retain(|_, t| t.ended_ms.is_none_or(|e| now - e < KEEP_DONE_MS));
        self.inbox.retain(|_, q| !q.is_empty());
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
}
