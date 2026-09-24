//! **Chain-following advance** — the RED→GREEN contrast against today's stall.
//!
//! ⚠ On the fixture name: the request named a "profile-v6 7-event shape". No
//! such fixture exists — `profile-v6` appears nowhere in this tree (code, YAML,
//! docs or memory). Rather than invent one under that name, the 7-event shape
//! here is constructed from what the stall demonstrably *is*: a chain whose
//! state the scan path cannot assemble, so the poller reports "did not advance"
//! and re-drives forever. If `profile-v6` is a real playbook elsewhere, point
//! this fixture at it; the property under test does not change.

use noetl_server::chain_advance::{
    chain_advance_enabled, decide, legacy_would_advance, AdvanceDecision, ChainEventView,
    ChainView, CHAIN_ADVANCE_ENV,
};

/// ⚠ **Env-var tests must serialise.** `cargo test` runs tests in a thread pool
/// and does NOT serialise them — a SAFETY note elsewhere in this program once
/// claimed it did, and the tests raced. Every test below that mutates
/// `NOETL_CHAIN_ADVANCE` takes this lock, and restores the previous value.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Run `f` with `NOETL_CHAIN_ADVANCE` set to `value` (or removed), serialised,
/// restoring whatever was there before.
fn with_flag<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var(CHAIN_ADVANCE_ENV).ok();
    match value {
        Some(v) => unsafe { std::env::set_var(CHAIN_ADVANCE_ENV, v) },
        None => unsafe { std::env::remove_var(CHAIN_ADVANCE_ENV) },
    }
    let out = f();
    match prev {
        Some(v) => unsafe { std::env::set_var(CHAIN_ADVANCE_ENV, v) },
        None => unsafe { std::env::remove_var(CHAIN_ADVANCE_ENV) },
    }
    out
}

fn ev(seq: u64, id: &str, prev: Option<&str>, terminal: bool) -> ChainEventView {
    ChainEventView {
        exec_seq: seq,
        event_id: id.to_string(),
        prev_event_id: prev.map(str::to_string),
        execution_id: "exec-stall".to_string(),
        parent_execution_id: None,
        terminal,
    }
}

/// The 7-event shape: a started execution that has run six steps and whose head
/// is a non-terminal step awaiting the next dispatch.
fn seven_event_chain() -> ChainView {
    ChainView::new(
        "exec-stall",
        vec![
            ev(1, "e1-playbook-started", None, false),
            ev(2, "e2-step-enter", Some("e1-playbook-started"), false),
            ev(3, "e3-command-issued", Some("e2-step-enter"), false),
            ev(4, "e4-step-completed", Some("e3-command-issued"), false),
            ev(5, "e5-step-enter", Some("e4-step-completed"), false),
            ev(6, "e6-command-issued", Some("e5-step-enter"), false),
            ev(7, "e7-step-completed", Some("e6-command-issued"), false),
        ],
    )
}

// ---------------------------------------------------------------------------
// The flag. Default off = today's path, untouched.
// ---------------------------------------------------------------------------

#[test]
fn the_flag_is_off_by_default_and_fails_safe() {
    with_flag(None, || {
        assert!(!chain_advance_enabled(), "default MUST be off");
    });
    for junk in ["", " ", "off", "no", "0", "enabled", "chain"] {
        with_flag(Some(junk), || {
            assert!(
                !chain_advance_enabled(),
                "{junk:?} must not move the execution-advance path"
            );
        });
    }
    with_flag(Some("true"), || assert!(chain_advance_enabled()));
}

// ---------------------------------------------------------------------------
// ⭐⭐ THE CONTRAST: the same 7 events stall today and advance by chain-following.
// ---------------------------------------------------------------------------

/// **RED** — today's decision on the stall shape. The scan cannot assemble the
/// spine, so `advanced = false`: a no-op, re-driven every tick, forever. The
/// give-up cap exists only to bound this, and counts poller iterations on a loop
/// measured at ~299 s/tick — 225 ≈ 18.7 hours.
#[test]
fn today_the_seven_event_shape_reports_no_advance_and_is_re_driven() {
    let view = seven_event_chain();
    assert!(
        !legacy_would_advance(&view, false),
        "the stall: an execution whose spine cannot be built reports 'did not \
         advance', which is indistinguishable from a finished one"
    );
}

/// **GREEN** — chain-following on the *same events* yields a concrete, actionable
/// decision: advance from the head. No retry, no budget, no cap.
#[test]
fn chain_following_advances_the_same_seven_event_shape() {
    let view = seven_event_chain();
    let d = decide(&view);
    assert_eq!(
        d,
        AdvanceDecision::Advance {
            from_event_id: "e7-step-completed".to_string(),
            from_exec_seq: 7,
        },
        "the chain is whole and its head is non-terminal: this execution can advance"
    );
    assert!(
        !d.requires_redrive(),
        "no decision requires a blind re-drive — that is what removes the cap"
    );
}

/// And it reaches a **terminal** event, which today is also just `false`.
#[test]
fn the_execution_reaches_terminal_via_chain_following() {
    let mut view = seven_event_chain();
    view.events.push(ev(
        8,
        "e8-execution-completed",
        Some("e7-step-completed"),
        true,
    ));
    let d = decide(&view);
    assert_eq!(
        d,
        AdvanceDecision::Terminal {
            at_event_id: "e8-execution-completed".to_string()
        },
        "a finished execution must be TERMINAL, not an indistinguishable no-op"
    );
    assert!(!d.requires_redrive(), "stop polling, do not retry");
}

/// ⚠ **The distinction the whole redesign turns on.** Today a finished execution
/// and a blocked one produce the *same* boolean. Here they are different
/// variants, and the blocked one names the key.
#[test]
fn a_finished_and_a_blocked_execution_are_distinguishable() {
    let mut finished = seven_event_chain();
    finished
        .events
        .push(ev(8, "e8-done", Some("e7-step-completed"), true));

    // Blocked: e4 never arrived (truncation, or not-yet-replicated).
    let mut blocked = seven_event_chain();
    blocked.events.retain(|e| e.event_id != "e4-step-completed");

    // Today: identical.
    assert_eq!(
        legacy_would_advance(&finished, true),
        legacy_would_advance(&blocked, false),
        "today both report the same boolean — which is the defect"
    );

    // Chain-following: different, and the block is named.
    let df = decide(&finished);
    let db = decide(&blocked);
    assert_ne!(df, db);
    assert_eq!(
        db,
        AdvanceDecision::BlockedAtGap {
            missing_event_id: "e4-step-completed".to_string(),
            detected_at_event_id: "e5-step-enter".to_string(),
        },
        "a gap must name BOTH the missing key and where it was detected"
    );
    assert!(matches!(df, AdvanceDecision::Terminal { .. }));
}

/// A blocked execution does not ask to be re-driven — it names what it waits
/// for. This is the assertion that makes the cap deletable.
#[test]
fn a_blocked_execution_never_requests_a_redrive() {
    let mut blocked = seven_event_chain();
    blocked.events.retain(|e| e.event_id != "e4-step-completed");
    let d = decide(&blocked);
    assert!(matches!(d, AdvanceDecision::BlockedAtGap { .. }));
    assert!(
        !d.requires_redrive(),
        "there is nothing to retry blindly, so nothing needs a cap to bound it"
    );
}

// ---------------------------------------------------------------------------
// The other decisions.
// ---------------------------------------------------------------------------

#[test]
fn an_empty_execution_is_empty_not_blocked() {
    assert_eq!(decide(&ChainView::new("e", vec![])), AdvanceDecision::Empty);
}

#[test]
fn a_single_root_event_advances() {
    let v = ChainView::new("e", vec![ev(1, "e1", None, false)]);
    assert_eq!(
        decide(&v),
        AdvanceDecision::Advance {
            from_event_id: "e1".into(),
            from_exec_seq: 1
        }
    );
}

/// Two events claiming one predecessor is a fork — the single-writer alarm.
#[test]
fn two_successors_to_one_event_is_reported_as_a_fork() {
    let v = ChainView::new(
        "e",
        vec![
            ev(1, "e1", None, false),
            ev(2, "e2a", Some("e1"), false),
            ev(3, "e2b", Some("e1"), false),
        ],
    );
    match decide(&v) {
        AdvanceDecision::Forked {
            prev_event_id,
            first,
            second,
        } => {
            assert_eq!(prev_event_id, "e1");
            assert_eq!((first.as_str(), second.as_str()), ("e2a", "e2b"));
        }
        other => panic!("expected Forked, got {other:?}"),
    }
}

/// Order of the input must not matter — the chain is derived from the pointers,
/// not from the order events happened to arrive in. This is what makes the
/// decision correct on a replica receiving out of order.
#[test]
fn the_decision_is_independent_of_input_order() {
    let ordered = seven_event_chain();
    let mut shuffled = seven_event_chain();
    shuffled.events.reverse();
    let mut interleaved = seven_event_chain();
    interleaved.events.swap(0, 4);
    interleaved.events.swap(1, 6);

    let d = decide(&ordered);
    assert_eq!(decide(&shuffled), d, "reversed input changed the decision");
    assert_eq!(
        decide(&interleaved),
        d,
        "shuffled input changed the decision"
    );
}

/// ⚠ **Positive control for the contrast tests.** If the legacy model returned
/// `false` for *everything*, the RED assertions above would pass vacuously and
/// prove nothing about the stall. It must return `true` on a healthy,
/// buildable execution.
#[test]
fn the_legacy_model_does_advance_when_the_spine_is_buildable() {
    let view = seven_event_chain();
    assert!(
        legacy_would_advance(&view, true),
        "the legacy model must be capable of returning true, or the RED tests \
         are vacuous"
    );
    // And it reports false on a finished execution — for the *right* reason.
    let mut finished = seven_event_chain();
    finished
        .events
        .push(ev(8, "e8-done", Some("e7-step-completed"), true));
    assert!(!legacy_would_advance(&finished, true));
}

/// Every decision label is pinnable, so a metric can carry all of them at 0.
#[test]
fn every_decision_label_is_enumerated() {
    let labels = AdvanceDecision::ALL_LABELS;
    assert_eq!(labels.len(), 5);
    for d in [
        AdvanceDecision::Advance {
            from_event_id: "x".into(),
            from_exec_seq: 1,
        },
        AdvanceDecision::Terminal {
            at_event_id: "x".into(),
        },
        AdvanceDecision::BlockedAtGap {
            missing_event_id: "x".into(),
            detected_at_event_id: "y".into(),
        },
        AdvanceDecision::Empty,
        AdvanceDecision::Forked {
            prev_event_id: "p".into(),
            first: "a".into(),
            second: "b".into(),
        },
    ] {
        assert!(
            labels.contains(&d.label()),
            "label {} is not in ALL_LABELS",
            d.label()
        );
    }
}

/// Prod-sized: a 5,000-event chain decides without depending on anything else.
#[test]
fn a_prod_sized_chain_decides_in_one_pass() {
    let mut events = Vec::with_capacity(5_000);
    let mut prev: Option<String> = None;
    for i in 1..=5_000u64 {
        let id = format!("ev-{i}");
        events.push(ChainEventView {
            exec_seq: i,
            event_id: id.clone(),
            prev_event_id: prev.clone(),
            execution_id: "big".into(),
            parent_execution_id: None,
            terminal: false,
        });
        prev = Some(id);
    }
    let v = ChainView::new("big", events);
    let t = std::time::Instant::now();
    let d = decide(&v);
    let took = t.elapsed();
    assert_eq!(
        d,
        AdvanceDecision::Advance {
            from_event_id: "ev-5000".into(),
            from_exec_seq: 5_000
        }
    );
    assert!(
        took < std::time::Duration::from_millis(500),
        "a 5,000-event chain took {took:?}"
    );
}

// ---------------------------------------------------------------------------
// The poller wiring: what a decision does to the no-op budget.
// ---------------------------------------------------------------------------

use noetl_server::chain_advance::{apply_chain_decision, poller_action, ChainSource, PollerAction};

/// A test source. ⚠ Deliberately a double rather than the Postgres-backed
/// source: the real one needs new SQL, and new SQL needs decode-testing against
/// a live database (server#443 — a select that passed unit tests and a
/// throwaway-Postgres proof and still 500'd on every prod call).
struct FixedSource(Option<ChainView>);

impl ChainSource for FixedSource {
    fn chain_for(&self, _execution_id: i64) -> Option<ChainView> {
        self.0.clone()
    }
    fn source_name(&self) -> &'static str {
        "fixed-test-source"
    }
}

/// ⭐⭐ **The assertion that makes the cap deletable.** A blocked execution
/// leaves the no-op budget UNCHANGED — it is a wait on a named key, not a no-op
/// to retry. Today the same situation burns budget every tick.
#[test]
fn a_blocked_execution_does_not_spend_budget() {
    let mut blocked = seven_event_chain();
    blocked.events.retain(|e| e.event_id != "e4-step-completed");
    let action = apply_chain_decision(&decide(&blocked), 17);
    assert_eq!(
        action,
        PollerAction {
            noops: 17, // ⭐ unchanged, NOT 18
            give_up: false,
            terminal: false,
            waiting_on: Some("e4-step-completed".to_string()),
            decision: "blocked_at_gap",
        },
        "a blocked execution must not spend budget, and must name what it waits on"
    );
}

/// Progress resets the budget.
#[test]
fn an_advance_resets_the_budget() {
    let action = apply_chain_decision(&decide(&seven_event_chain()), 99);
    assert_eq!(action.noops, 0, "progress resets the budget");
    assert!(!action.give_up);
    assert!(!action.terminal);
    assert_eq!(action.decision, "advance");
}

/// A terminal execution stops polling and is not a no-op.
#[test]
fn a_terminal_execution_stops_polling_without_spending_budget() {
    let mut v = seven_event_chain();
    v.events
        .push(ev(8, "e8-done", Some("e7-step-completed"), true));
    let action = apply_chain_decision(&decide(&v), 42);
    assert!(action.terminal, "must stop polling");
    assert!(!action.give_up, "finishing is not giving up");
    assert_eq!(action.noops, 0);
}

/// ⭐ **No decision ever gives up.** That is the property that lets the cap and
/// its 18.7-hour arithmetic be deleted outright rather than tuned.
#[test]
fn no_chain_decision_ever_gives_up() {
    let complete = seven_event_chain();
    let mut blocked = seven_event_chain();
    blocked.events.retain(|e| e.event_id != "e4-step-completed");
    let mut terminal = seven_event_chain();
    terminal
        .events
        .push(ev(8, "e8-done", Some("e7-step-completed"), true));
    let empty = ChainView::new("e", vec![]);
    let forked = ChainView::new(
        "e",
        vec![
            ev(1, "e1", None, false),
            ev(2, "a", Some("e1"), false),
            ev(3, "b", Some("e1"), false),
        ],
    );

    for (name, v) in [
        ("complete", complete),
        ("blocked", blocked),
        ("terminal", terminal),
        ("empty", empty),
        ("forked", forked),
    ] {
        // Start at the cap itself: even there, nothing gives up.
        let action = apply_chain_decision(&decide(&v), 225);
        assert!(
            !action.give_up,
            "{name}: chain-following must never give up — there is nothing to \
             retry blindly, so there is nothing to bound"
        );
    }
}

/// ⚠ A blocked execution at the cap still does not give up, and still does not
/// climb. This is the exact state that produced 53 permanently re-driven
/// executions.
#[test]
fn a_blocked_execution_at_the_cap_neither_gives_up_nor_climbs() {
    let mut blocked = seven_event_chain();
    blocked.events.retain(|e| e.event_id != "e4-step-completed");
    let d = decide(&blocked);
    for before in [0u32, 1, 224, 225, 10_000] {
        let a = apply_chain_decision(&d, before);
        assert_eq!(a.noops, before, "budget moved from {before}");
        assert!(!a.give_up, "gave up at {before}");
    }
}

// ---------------------------------------------------------------------------
// The flag gate on the wiring.
// ---------------------------------------------------------------------------

/// With the flag OFF, `poller_action` returns `None` — the caller runs today's
/// path. This is the additive guarantee.
#[test]
fn with_the_flag_off_the_poller_falls_through_to_todays_path() {
    with_flag(None, || {
        let src = FixedSource(Some(seven_event_chain()));
        assert!(
            poller_action(&src, 1, 5).is_none(),
            "flag off MUST fall through, even with a working source"
        );
    });
}

/// With the flag ON and a source, the chain decision is used.
#[test]
fn with_the_flag_on_and_a_source_the_chain_decision_is_used() {
    with_flag(Some("true"), || {
        let mut blocked = seven_event_chain();
        blocked.events.retain(|e| e.event_id != "e4-step-completed");
        let src = FixedSource(Some(blocked));
        let action = poller_action(&src, 1, 7).expect("the chain path must engage");
        assert_eq!(action.decision, "blocked_at_gap");
        assert_eq!(action.noops, 7, "budget untouched");
        assert_eq!(action.waiting_on.as_deref(), Some("e4-step-completed"));

        // ⚠ And with the flag on but NO chain available, it falls through rather
        // than inventing a decision from nothing.
        let empty_src = FixedSource(None);
        assert!(
            poller_action(&empty_src, 1, 7).is_none(),
            "no chain available must fall through, not fabricate a decision"
        );
    });
}

/// ⚠ **Positive control for the two flag tests.** If `poller_action` returned
/// `None` unconditionally, both "falls through" assertions would pass and the
/// wiring would be inert while looking wired — the defect shape this program
/// keeps finding.
#[test]
fn poller_action_is_capable_of_returning_some() {
    with_flag(Some("1"), || {
        let src = FixedSource(Some(seven_event_chain()));
        let got = poller_action(&src, 1, 0);
        assert!(
            got.is_some(),
            "poller_action must be capable of engaging, or the flag-off tests are vacuous"
        );
        assert_eq!(got.unwrap().decision, "advance");
    });
}
