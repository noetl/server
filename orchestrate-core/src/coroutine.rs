//! Coroutine groups: the pure join-policy reducer (ai-meta RFC
//! `docs/rfc/coroutines.md`, stage 1).
//!
//! A **group** is the set of coroutines one owner step launches.  Each
//! coroutine is a **leaf unit** (one tool / ordered tool list, or one child
//! playbook; RFC §11.1, accepted 2026-11-12).  This module decides, from the
//! group's spec and the unit events seen so far:
//!
//! - which pending units to dispatch now (the `max_in_flight` window),
//! - which units to cancel (losers of `any` / `race` / `at_least`, everything
//!   on fail-fast, group timeout or owner cancel),
//! - the group's outcome, and when the owner step may complete.
//!
//! It is pure and deterministic: no clock, no I/O, no environment.  The server
//! (natively and inside the wasm `system/orchestrate` drive) and the CLI local
//! runtime are meant to share it, so join semantics cannot drift between
//! runtimes.
//!
//! **Stage 1 has no runtime path.**  Nothing in the orchestrator calls this
//! module yet and the DSL does not accept `spawn:`.  [`CoroutineGates`] defines
//! the `NOETL_COROUTINES` gate the later stages check; it is off by default.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// The gate later stages check before admitting `spawn:`.  Armed only by the
/// exact string `true`.
pub const COROUTINES_ENV: &str = "NOETL_COROUTINES";

/// Process gates for coroutines.  Off by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CoroutineGates {
    pub enabled: bool,
}

impl CoroutineGates {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        CoroutineGates {
            enabled: get(COROUTINES_ENV).as_deref() == Some("true"),
        }
    }

    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok())
    }
}

/// When the owner step's group is satisfied (RFC §3.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinPolicy {
    /// Every unit succeeds; the first failure fails the group (fail-fast).
    All,
    /// Every unit reaches a terminal state; units never fail the group.
    Settled,
    /// The first success wins; the group fails only if every unit failed.
    Any,
    /// The first unit to reach a terminal state decides the outcome.
    Race,
    /// `k` successes win; the group fails once `k` is unreachable.
    AtLeast(u32),
}

/// What a lost worker means for a unit (RFC §8).  The reducer only carries
/// it; the lease / re-dispatch machinery is stage 5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Restart {
    #[default]
    Never,
    Replay,
}

/// A group, as the reducer needs it.  Unit names are in declaration order;
/// a `spawn.each` group names its units by item index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupSpec {
    pub join: JoinPolicy,
    pub units: Vec<String>,
    /// Group window.  `None` means "no group limit" (the execution cap still
    /// applies through [`GroupState::start`]).
    #[serde(default)]
    pub max_in_flight: Option<u32>,
    #[serde(default)]
    pub restart: Restart,
}

/// A spec the reducer refuses.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpecError {
    #[error("a coroutine group needs at least one unit")]
    Empty,
    #[error("duplicate unit name `{0}`")]
    DuplicateUnit(String),
    #[error("max_in_flight must be at least 1")]
    ZeroWindow,
    #[error("join at_least: {k} needs k between 1 and the unit count ({units})")]
    AtLeastOutOfRange { k: u32, units: usize },
}

impl GroupSpec {
    pub fn validate(&self) -> Result<(), SpecError> {
        if self.units.is_empty() {
            return Err(SpecError::Empty);
        }
        let mut seen = std::collections::BTreeSet::new();
        for u in &self.units {
            if !seen.insert(u.as_str()) {
                return Err(SpecError::DuplicateUnit(u.clone()));
            }
        }
        if self.max_in_flight == Some(0) {
            return Err(SpecError::ZeroWindow);
        }
        if let JoinPolicy::AtLeast(k) = self.join {
            if k == 0 || k as usize > self.units.len() {
                return Err(SpecError::AtLeastOutOfRange {
                    k,
                    units: self.units.len(),
                });
            }
        }
        Ok(())
    }
}

/// One unit's lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitStatus {
    /// Not dispatched yet (held by the window).
    Pending,
    /// Dispatched; not terminal.
    Running,
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
}

impl UnitStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            UnitStatus::Succeeded | UnitStatus::Failed | UnitStatus::TimedOut | UnitStatus::Cancelled
        )
    }
}

/// Why a unit was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelReason {
    /// The policy was decided by another unit (`any`, `race`, `at_least`).
    LostRace,
    /// `all` failed fast, or `at_least` became unreachable.
    SiblingFailed,
    /// The group deadline passed.
    GroupTimeout,
    /// The owner step / execution was cancelled or failed.
    OwnerCancelled,
    /// Cancelled from outside the group (e.g. the cancel API on a child).
    External,
}

/// Something that happened to a unit, or to the group.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GroupEvent {
    Succeeded { unit: usize, result: Value },
    Failed { unit: usize, kind: String, message: String },
    /// The unit's own deadline passed.
    TimedOut { unit: usize },
    /// A unit the reducer asked to cancel (or one cancelled externally)
    /// confirms it stopped.
    Cancelled { unit: usize, reason: CancelReason },
    /// The group deadline passed.
    GroupTimedOut,
    /// The owner step or its execution was cancelled or failed.
    OwnerCancelled,
}

/// The group's outcome (RFC §3.4 `status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupStatus {
    Succeeded,
    Failed,
    TimedOut,
    Cancelled,
}

/// What the caller must do after `start` / `apply`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Effects {
    /// Units to dispatch now, in declaration order.  Each index appears in
    /// `dispatch` at most once over the group's whole life.
    pub dispatch: Vec<usize>,
    /// Running units to cancel now (each at most once).
    pub cancel: Vec<(usize, CancelReason)>,
    /// The outcome was decided by this call.
    pub decided: Option<GroupStatus>,
    /// The owner step may complete: the outcome is decided **and** no unit is
    /// still running (structured concurrency: no unit outlives its group).
    pub done: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct UnitState {
    status: UnitStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cancel_reason: Option<CancelReason>,
    /// A cancel was requested and not yet confirmed.
    #[serde(default)]
    cancel_requested: bool,
}

/// The reducer's state.  Serializable so the server can keep it in execution
/// state and re-derive it on replay.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupState {
    spec: GroupSpec,
    window: usize,
    units: Vec<UnitState>,
    outcome: Option<GroupStatus>,
    winner: Option<usize>,
}

/// An event the reducer cannot apply.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApplyError {
    #[error("unit index {0} is out of range")]
    UnknownUnit(usize),
    #[error("unit {0} has not been dispatched")]
    NotDispatched(usize),
}

impl GroupState {
    /// Validate the spec and open the first window.  `execution_cap` is the
    /// per-execution limit (`NOETL_COROUTINE_MAX_IN_FLIGHT`); the effective
    /// window is the smaller of it and the group's `max_in_flight`.
    pub fn start(spec: GroupSpec, execution_cap: u32) -> Result<(Self, Effects), SpecError> {
        spec.validate()?;
        let cap = execution_cap.max(1) as usize;
        let window = spec
            .max_in_flight
            .map(|m| (m as usize).min(cap))
            .unwrap_or(cap);
        let units = spec
            .units
            .iter()
            .map(|_| UnitState {
                status: UnitStatus::Pending,
                result: None,
                error: None,
                cancel_reason: None,
                cancel_requested: false,
            })
            .collect();
        let mut state = GroupState {
            spec,
            window,
            units,
            outcome: None,
            winner: None,
        };
        let mut fx = Effects::default();
        state.fill_window(&mut fx);
        fx.done = state.is_done();
        Ok((state, fx))
    }

    /// Apply one event.  Idempotent: an event for a unit that is already
    /// terminal changes nothing, so a replayed event log is safe.
    pub fn apply(&mut self, event: &GroupEvent) -> Result<Effects, ApplyError> {
        let mut fx = Effects::default();
        let before = self.outcome;
        match event {
            GroupEvent::GroupTimedOut => {
                self.decide(GroupStatus::TimedOut, None);
                self.cancel_all(CancelReason::GroupTimeout, &mut fx);
            }
            GroupEvent::OwnerCancelled => {
                self.decide(GroupStatus::Cancelled, None);
                self.cancel_all(CancelReason::OwnerCancelled, &mut fx);
            }
            GroupEvent::Succeeded { unit, .. }
            | GroupEvent::Failed { unit, .. }
            | GroupEvent::TimedOut { unit }
            | GroupEvent::Cancelled { unit, .. } => {
                let i = *unit;
                let u = self.units.get(i).ok_or(ApplyError::UnknownUnit(i))?;
                if u.status.is_terminal() {
                    fx.done = self.is_done();
                    return Ok(fx);
                }
                if u.status == UnitStatus::Pending {
                    return Err(ApplyError::NotDispatched(i));
                }
                self.settle(i, event);
                if self.outcome.is_none() {
                    self.evaluate(i, &mut fx);
                }
            }
        }
        if self.outcome.is_none() {
            self.fill_window(&mut fx);
        }
        if before.is_none() {
            fx.decided = self.outcome;
        }
        fx.done = self.is_done();
        Ok(fx)
    }

    pub fn outcome(&self) -> Option<GroupStatus> {
        self.outcome
    }

    pub fn status(&self, unit: usize) -> Option<UnitStatus> {
        self.units.get(unit).map(|u| u.status)
    }

    pub fn running(&self) -> usize {
        self.units
            .iter()
            .filter(|u| u.status == UnitStatus::Running)
            .count()
    }

    /// The owner step may complete.
    pub fn is_done(&self) -> bool {
        self.outcome.is_some() && self.running() == 0
    }

    /// The owner step's result (RFC §3.4).  `results` is keyed by unit name;
    /// JSON object key order is not preserved here, so `units` carries the
    /// declaration order.
    pub fn result(&self) -> Value {
        let mut results = Map::new();
        let mut errors = Vec::new();
        for (name, u) in self.spec.units.iter().zip(&self.units) {
            let mut entry = Map::new();
            entry.insert("status".into(), json!(u.status));
            if let Some(r) = &u.result {
                entry.insert("result".into(), r.clone());
            }
            if let Some(reason) = u.cancel_reason {
                entry.insert("reason".into(), json!(reason));
            }
            if let Some((kind, message)) = &u.error {
                entry.insert("error".into(), json!({ "kind": kind, "message": message }));
                errors.push(json!({ "unit": name, "kind": kind, "message": message }));
            } else if u.status == UnitStatus::TimedOut {
                errors.push(json!({ "unit": name, "kind": "timeout", "message": "unit deadline passed" }));
            }
            results.insert(name.clone(), Value::Object(entry));
        }
        json!({
            "policy": self.spec.join,
            "status": self.outcome,
            "winner": self.winner.map(|i| self.spec.units[i].clone()),
            "units": self.spec.units,
            "results": results,
            "errors": errors,
        })
    }

    // ---- internals -------------------------------------------------------

    fn settle(&mut self, i: usize, event: &GroupEvent) {
        let u = &mut self.units[i];
        match event {
            GroupEvent::Succeeded { result, .. } => {
                u.status = UnitStatus::Succeeded;
                u.result = Some(result.clone());
            }
            GroupEvent::Failed { kind, message, .. } => {
                u.status = UnitStatus::Failed;
                u.error = Some((kind.clone(), message.clone()));
            }
            GroupEvent::TimedOut { .. } => u.status = UnitStatus::TimedOut,
            GroupEvent::Cancelled { reason, .. } => {
                u.status = UnitStatus::Cancelled;
                if u.cancel_reason.is_none() {
                    u.cancel_reason = Some(*reason);
                }
            }
            GroupEvent::GroupTimedOut | GroupEvent::OwnerCancelled => unreachable!(),
        }
        u.cancel_requested = false;
    }

    /// Decide the policy after unit `i` settled (outcome still open).
    fn evaluate(&mut self, i: usize, fx: &mut Effects) {
        let st = self.units[i].status;
        let succeeded = self.count(UnitStatus::Succeeded);
        let n = self.units.len();
        let all_terminal = self.units.iter().all(|u| u.status.is_terminal());
        // A unit that failed, timed out or was cancelled from outside the group
        // counts as "did not succeed".
        let not_ok = matches!(
            st,
            UnitStatus::Failed | UnitStatus::TimedOut | UnitStatus::Cancelled
        );
        match self.spec.join {
            JoinPolicy::All => {
                if not_ok {
                    self.decide(GroupStatus::Failed, None);
                    self.cancel_all(CancelReason::SiblingFailed, fx);
                } else if succeeded == n {
                    self.decide(GroupStatus::Succeeded, None);
                }
            }
            JoinPolicy::Settled => {
                if all_terminal {
                    self.decide(GroupStatus::Succeeded, None);
                }
            }
            JoinPolicy::Any => {
                if st == UnitStatus::Succeeded {
                    self.decide(GroupStatus::Succeeded, Some(i));
                    self.cancel_all(CancelReason::LostRace, fx);
                } else if all_terminal {
                    self.decide(GroupStatus::Failed, None);
                }
            }
            JoinPolicy::Race => {
                let status = if st == UnitStatus::Succeeded {
                    GroupStatus::Succeeded
                } else if st == UnitStatus::TimedOut {
                    GroupStatus::TimedOut
                } else {
                    GroupStatus::Failed
                };
                self.decide(status, Some(i));
                self.cancel_all(CancelReason::LostRace, fx);
            }
            JoinPolicy::AtLeast(k) => {
                let k = k as usize;
                let open = self.units.iter().filter(|u| !u.status.is_terminal()).count();
                if succeeded >= k {
                    self.decide(GroupStatus::Succeeded, None);
                    self.cancel_all(CancelReason::LostRace, fx);
                } else if succeeded + open < k {
                    self.decide(GroupStatus::Failed, None);
                    self.cancel_all(CancelReason::SiblingFailed, fx);
                }
            }
        }
    }

    fn decide(&mut self, status: GroupStatus, winner: Option<usize>) {
        if self.outcome.is_none() {
            self.outcome = Some(status);
            self.winner = winner;
        }
    }

    /// Pending units become cancelled at once (they never ran); running units
    /// get a cancel request and stay running until they confirm.
    fn cancel_all(&mut self, reason: CancelReason, fx: &mut Effects) {
        for (i, u) in self.units.iter_mut().enumerate() {
            match u.status {
                UnitStatus::Pending => {
                    u.status = UnitStatus::Cancelled;
                    u.cancel_reason = Some(reason);
                }
                UnitStatus::Running if !u.cancel_requested => {
                    u.cancel_requested = true;
                    u.cancel_reason = Some(reason);
                    fx.cancel.push((i, reason));
                }
                _ => {}
            }
        }
    }

    fn fill_window(&mut self, fx: &mut Effects) {
        let mut running = self.running();
        for (i, u) in self.units.iter_mut().enumerate() {
            if running >= self.window {
                break;
            }
            if u.status == UnitStatus::Pending {
                u.status = UnitStatus::Running;
                fx.dispatch.push(i);
                running += 1;
            }
        }
    }

    fn count(&self, s: UnitStatus) -> usize {
        self.units.iter().filter(|u| u.status == s).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(join: JoinPolicy, n: usize, window: Option<u32>) -> GroupSpec {
        GroupSpec {
            join,
            units: (0..n).map(|i| format!("u{i}")).collect(),
            max_in_flight: window,
            restart: Restart::Never,
        }
    }

    fn ok(unit: usize) -> GroupEvent {
        GroupEvent::Succeeded {
            unit,
            result: json!(unit),
        }
    }

    fn fail(unit: usize) -> GroupEvent {
        GroupEvent::Failed {
            unit,
            kind: "tool_error".into(),
            message: "boom".into(),
        }
    }

    #[test]
    fn the_gate_is_off_unless_exactly_true() {
        assert!(!CoroutineGates::from_lookup(|_| None).enabled);
        for v in ["1", "TRUE", "yes", "on", " true"] {
            let v = v.to_string();
            assert!(!CoroutineGates::from_lookup(|_| Some(v.clone())).enabled, "{v:?}");
        }
        assert!(CoroutineGates::from_lookup(|k| (k == COROUTINES_ENV).then(|| "true".into())).enabled);
    }

    #[test]
    fn specs_are_validated() {
        assert_eq!(spec(JoinPolicy::All, 0, None).validate(), Err(SpecError::Empty));
        assert_eq!(spec(JoinPolicy::All, 2, Some(0)).validate(), Err(SpecError::ZeroWindow));
        assert!(matches!(
            spec(JoinPolicy::AtLeast(3), 2, None).validate(),
            Err(SpecError::AtLeastOutOfRange { .. })
        ));
        assert!(matches!(
            spec(JoinPolicy::AtLeast(0), 2, None).validate(),
            Err(SpecError::AtLeastOutOfRange { .. })
        ));
        let mut s = spec(JoinPolicy::All, 2, None);
        s.units[1] = "u0".into();
        assert_eq!(s.validate(), Err(SpecError::DuplicateUnit("u0".into())));
    }

    #[test]
    fn the_window_is_the_smaller_of_group_and_execution_cap() {
        let (_, fx) = GroupState::start(spec(JoinPolicy::All, 10, Some(3)), 16).unwrap();
        assert_eq!(fx.dispatch, vec![0, 1, 2]);
        let (_, fx) = GroupState::start(spec(JoinPolicy::All, 10, Some(8)), 2).unwrap();
        assert_eq!(fx.dispatch, vec![0, 1]);
        let (_, fx) = GroupState::start(spec(JoinPolicy::All, 3, None), 16).unwrap();
        assert_eq!(fx.dispatch, vec![0, 1, 2]);
    }

    #[test]
    fn a_completion_opens_the_next_slot_in_order() {
        let (mut g, _) = GroupState::start(spec(JoinPolicy::Settled, 4, Some(2)), 16).unwrap();
        assert_eq!(g.apply(&ok(1)).unwrap().dispatch, vec![2]);
        assert_eq!(g.apply(&fail(0)).unwrap().dispatch, vec![3]);
        g.apply(&ok(2)).unwrap();
        let fx = g.apply(&ok(3)).unwrap();
        assert_eq!(fx.decided, Some(GroupStatus::Succeeded));
        assert!(fx.done);
    }

    #[test]
    fn all_fails_fast_and_cancels_running_and_pending() {
        let (mut g, _) = GroupState::start(spec(JoinPolicy::All, 4, Some(2)), 16).unwrap();
        let fx = g.apply(&fail(0)).unwrap();
        assert_eq!(fx.decided, Some(GroupStatus::Failed));
        assert_eq!(fx.cancel, vec![(1, CancelReason::SiblingFailed)]);
        assert!(fx.dispatch.is_empty(), "nothing new after the outcome");
        assert_eq!(g.status(2), Some(UnitStatus::Cancelled), "pending never ran");
        assert!(!fx.done, "u1 is still running: the owner must wait for it");
        let fx = g
            .apply(&GroupEvent::Cancelled {
                unit: 1,
                reason: CancelReason::SiblingFailed,
            })
            .unwrap();
        assert!(fx.done);
        assert_eq!(fx.decided, None, "decided once");
        let r = g.result();
        assert_eq!(r["status"], "failed");
        assert_eq!(r["errors"][0]["unit"], "u0");
        assert_eq!(r["results"]["u1"]["reason"], "sibling_failed");
    }

    #[test]
    fn any_takes_the_first_success_and_fails_only_when_all_fail() {
        let (mut g, _) = GroupState::start(spec(JoinPolicy::Any, 3, None), 16).unwrap();
        assert_eq!(g.apply(&fail(0)).unwrap().decided, None);
        let fx = g.apply(&ok(2)).unwrap();
        assert_eq!(fx.decided, Some(GroupStatus::Succeeded));
        assert_eq!(fx.cancel, vec![(1, CancelReason::LostRace)]);
        assert_eq!(g.result()["winner"], "u2");

        let (mut g, _) = GroupState::start(spec(JoinPolicy::Any, 2, None), 16).unwrap();
        g.apply(&fail(0)).unwrap();
        assert_eq!(g.apply(&fail(1)).unwrap().decided, Some(GroupStatus::Failed));
        assert_eq!(g.result()["errors"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn race_takes_the_first_terminal_either_way() {
        let (mut g, _) = GroupState::start(spec(JoinPolicy::Race, 3, None), 16).unwrap();
        let fx = g.apply(&fail(1)).unwrap();
        assert_eq!(fx.decided, Some(GroupStatus::Failed));
        assert_eq!(g.result()["winner"], "u1");
        assert_eq!(fx.cancel.len(), 2);
    }

    #[test]
    fn at_least_wins_at_k_and_fails_when_k_is_unreachable() {
        let (mut g, _) = GroupState::start(spec(JoinPolicy::AtLeast(2), 4, None), 16).unwrap();
        g.apply(&ok(0)).unwrap();
        let fx = g.apply(&ok(3)).unwrap();
        assert_eq!(fx.decided, Some(GroupStatus::Succeeded));
        assert_eq!(fx.cancel.len(), 2);

        let (mut g, _) = GroupState::start(spec(JoinPolicy::AtLeast(3), 4, None), 16).unwrap();
        g.apply(&fail(0)).unwrap();
        let fx = g.apply(&fail(1)).unwrap();
        assert_eq!(fx.decided, Some(GroupStatus::Failed), "2 open + 0 ok < 3");
    }

    #[test]
    fn group_timeout_and_owner_cancel_cancel_everything() {
        let (mut g, _) = GroupState::start(spec(JoinPolicy::Settled, 3, Some(1)), 16).unwrap();
        let fx = g.apply(&GroupEvent::GroupTimedOut).unwrap();
        assert_eq!(fx.decided, Some(GroupStatus::TimedOut));
        assert_eq!(fx.cancel, vec![(0, CancelReason::GroupTimeout)]);
        assert_eq!(g.status(1), Some(UnitStatus::Cancelled));

        let (mut g, _) = GroupState::start(spec(JoinPolicy::All, 2, None), 16).unwrap();
        let fx = g.apply(&GroupEvent::OwnerCancelled).unwrap();
        assert_eq!(fx.decided, Some(GroupStatus::Cancelled));
        assert_eq!(fx.cancel.len(), 2);
        // A second owner cancel asks for nothing new.
        assert!(g.apply(&GroupEvent::OwnerCancelled).unwrap().cancel.is_empty());
    }

    #[test]
    fn replayed_and_bad_events() {
        let (mut g, _) = GroupState::start(spec(JoinPolicy::All, 2, Some(1)), 16).unwrap();
        assert_eq!(g.apply(&ok(1)), Err(ApplyError::NotDispatched(1)));
        assert_eq!(g.apply(&ok(9)), Err(ApplyError::UnknownUnit(9)));
        let first = g.apply(&ok(0)).unwrap();
        assert_eq!(first.dispatch, vec![1]);
        let again = g.apply(&ok(0)).unwrap();
        assert_eq!(again, Effects::default(), "a replayed event is a no-op");
    }

    #[test]
    fn state_round_trips_through_json() {
        let (mut g, _) = GroupState::start(spec(JoinPolicy::Any, 3, Some(2)), 16).unwrap();
        g.apply(&fail(0)).unwrap();
        let back: GroupState = serde_json::from_value(serde_json::to_value(&g).unwrap()).unwrap();
        assert_eq!(back, g);
    }

    // ---- exhaustive model check ----------------------------------------
    //
    // For every policy, group size 1..=4, window 1..=4, every assignment of
    // success / failure to units, and every order in which running units
    // finish, drive the reducer and check it against an independent oracle and
    // the structured-concurrency invariants.  No proptest dependency: the
    // space is small enough to enumerate.

    /// What the policy should decide, given the order units finished in
    /// (`true` = succeeded), ignoring units cancelled before finishing.
    fn oracle(join: JoinPolicy, n: usize, finished: &[(usize, bool)]) -> (GroupStatus, Option<usize>) {
        let mut ok = 0;
        let mut seen = 0;
        for &(i, s) in finished {
            seen += 1;
            if s {
                ok += 1;
            }
            match join {
                JoinPolicy::All if !s => return (GroupStatus::Failed, None),
                JoinPolicy::Any if s => return (GroupStatus::Succeeded, Some(i)),
                JoinPolicy::Race => {
                    return (
                        if s { GroupStatus::Succeeded } else { GroupStatus::Failed },
                        Some(i),
                    )
                }
                JoinPolicy::AtLeast(k) => {
                    if ok >= k as usize {
                        return (GroupStatus::Succeeded, None);
                    }
                    if ok + (n - seen) < k as usize {
                        return (GroupStatus::Failed, None);
                    }
                }
                _ => {}
            }
        }
        match join {
            JoinPolicy::All | JoinPolicy::Settled => (GroupStatus::Succeeded, None),
            JoinPolicy::Any => (GroupStatus::Failed, None),
            _ => unreachable!("race / at_least always decide before the end"),
        }
    }

    struct Run {
        g: GroupState,
        dispatched: Vec<usize>,
        cancel_requested: Vec<usize>,
        finished: Vec<(usize, bool)>,
        decided: Vec<GroupStatus>,
    }

    fn explore(run: Run, outcomes: &[bool], window: usize, join: JoinPolicy, checked: &mut usize) {
        let n = outcomes.len();
        let running: Vec<usize> = (0..n)
            .filter(|&i| run.g.status(i) == Some(UnitStatus::Running))
            .collect();
        assert!(running.len() <= window, "window exceeded");
        if running.is_empty() {
            // Terminal: check everything.
            *checked += 1;
            assert!(run.g.is_done());
            assert_eq!(run.decided.len(), 1, "decided exactly once");
            let (want, winner) = oracle(join, n, &run.finished);
            assert_eq!(run.g.outcome(), Some(want), "{join:?} {outcomes:?} {:?}", run.finished);
            if let Some(w) = winner {
                assert_eq!(run.g.result()["winner"], json!(format!("u{w}")));
            }
            let mut d = run.dispatched.clone();
            d.sort();
            d.dedup();
            assert_eq!(d.len(), run.dispatched.len(), "a unit dispatched twice");
            for i in 0..n {
                assert!(run.g.status(i).unwrap().is_terminal(), "unit {i} left running");
            }
            return;
        }
        for &i in &running {
            let mut g = run.g.clone();
            let cancelled = run.cancel_requested.contains(&i);
            // A unit asked to cancel confirms the cancel; any other unit
            // finishes with its assigned outcome.
            let ev = if cancelled {
                GroupEvent::Cancelled {
                    unit: i,
                    reason: CancelReason::LostRace,
                }
            } else if outcomes[i] {
                ok(i)
            } else {
                fail(i)
            };
            let fx = g.apply(&ev).unwrap();
            let mut next = Run {
                g,
                dispatched: run.dispatched.clone(),
                cancel_requested: run.cancel_requested.clone(),
                finished: run.finished.clone(),
                decided: run.decided.clone(),
            };
            if !cancelled {
                next.finished.push((i, outcomes[i]));
            }
            next.dispatched.extend(&fx.dispatch);
            for (c, _) in &fx.cancel {
                assert!(!next.cancel_requested.contains(c), "cancel requested twice");
                next.cancel_requested.push(*c);
            }
            if let Some(d) = fx.decided {
                next.decided.push(d);
            }
            if !next.decided.is_empty() {
                assert!(fx.dispatch.is_empty(), "dispatch after the outcome");
            }
            explore(next, outcomes, window, join, checked);
        }
    }

    #[test]
    fn exhaustive_model_check() {
        let mut checked = 0;
        for n in 1..=4usize {
            let mut policies = vec![JoinPolicy::All, JoinPolicy::Settled, JoinPolicy::Any, JoinPolicy::Race];
            policies.extend((1..=n as u32).map(JoinPolicy::AtLeast));
            for join in policies {
                for window in 1..=4usize {
                    for mask in 0..(1u32 << n) {
                        let outcomes: Vec<bool> = (0..n).map(|i| mask & (1 << i) != 0).collect();
                        let (g, fx) =
                            GroupState::start(spec(join, n, Some(window as u32)), 16).unwrap();
                        assert_eq!(fx.dispatch.len(), n.min(window));
                        let run = Run {
                            g,
                            dispatched: fx.dispatch,
                            cancel_requested: vec![],
                            finished: vec![],
                            decided: vec![],
                        };
                        explore(run, &outcomes, window, join, &mut checked);
                    }
                }
            }
        }
        assert!(checked >= 6_000, "explored only {checked} interleavings");
    }
}
