//! **Chain-following execution advance** — the path that retires the
//! reconcile/re-drive loop.
//!
//! Design: `noetl/ai-meta` `docs/rfc/ehdb-execution-partitioned-event-store.md`.
//! Flag: [`CHAIN_ADVANCE_ENV`], **default off**. With the flag off the existing
//! reconcile poller is untouched.
//!
//! ## What is wrong today, precisely
//!
//! The reconcile poller (`handlers/events.rs`, `spawn_reconcile_poller`) does
//! this every tick, for every active execution:
//!
//! ```text
//! let advanced = trigger_orchestrator_inner(..).await? > 0;
//! if !advanced { budget += 1; if budget >= cap { give_up() } }
//! ```
//!
//! `advanced` is a **boolean derived from a command count**. It collapses three
//! very different situations into one:
//!
//! | situation | today | should be |
//! | :-- | :-- | :-- |
//! | the execution is finished | `advanced = false` | **terminal**, stop polling |
//! | the next step is genuinely runnable | `advanced = true` | advance |
//! | the chain is missing a link and cannot be built | `advanced = false` | **blocked at a NAMED key** |
//!
//! Rows 1 and 3 are indistinguishable from each other and from "nothing to do",
//! so the only available response is *try again*. That is the re-drive loop, and
//! the give-up cap exists solely to bound it. The cap then counts poller
//! iterations on a loop measured at ~299 s per tick, so 225 ≈ 18.7 hours.
//!
//! ⭐ **The fix is not a better cap. It is making row 3 distinguishable.** Once
//! a blocked execution names the key it is waiting for, there is nothing to
//! retry blindly and nothing to bound.
//!
//! ## Why this module is self-contained
//!
//! It takes a [`ChainView`] rather than an `ehdb_l0` type. `server` pins
//! `ehdb-l0` at a released tag, and the chain store this is designed against is
//! on an unmerged branch — so depending on it would mean re-pointing the
//! dependency at a branch and reverting that before merge. The advance decision
//! is pure logic over the four ids and needs none of that.

use std::collections::{BTreeMap, BTreeSet};

/// Env var enabling chain-following advance. **Default off.**
pub const CHAIN_ADVANCE_ENV: &str = "NOETL_CHAIN_ADVANCE";

/// Whether chain-following advance is enabled.
///
/// ⚠ Fail-safe: anything unrecognised is `false`. A typo must not move the
/// execution-advance path.
pub fn chain_advance_enabled() -> bool {
    matches!(
        std::env::var(CHAIN_ADVANCE_ENV)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// One event as the advance decision needs to see it — the four ids plus
/// whether it is a terminal event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainEventView {
    pub exec_seq: u64,
    pub event_id: String,
    pub prev_event_id: Option<String>,
    pub execution_id: String,
    pub parent_execution_id: Option<String>,
    /// Whether this event ends the execution (`execution.completed`,
    /// `execution.failed`, …). The caller classifies; this module does not
    /// parse event types.
    pub terminal: bool,
}

/// An execution's chain as the advance decision reads it.
#[derive(Debug, Clone, Default)]
pub struct ChainView {
    pub execution_id: String,
    /// Events held, in any order. Ordering is derived, not trusted.
    pub events: Vec<ChainEventView>,
}

impl ChainView {
    pub fn new(execution_id: impl Into<String>, events: Vec<ChainEventView>) -> Self {
        Self {
            execution_id: execution_id.into(),
            events,
        }
    }
}

/// What the execution should do next.
///
/// ⭐ The value of this enum over today's `bool` is entirely in
/// [`BlockedAtGap`](AdvanceDecision::BlockedAtGap): it is the case that today
/// cannot be told apart from "nothing to do", and therefore the case that
/// produces infinite re-driving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdvanceDecision {
    /// The chain is whole and its head is not terminal: run the next step from
    /// this head. **This is the case that stalls today.**
    Advance {
        from_event_id: String,
        from_exec_seq: u64,
    },
    /// The head is a terminal event. Stop polling; this is not a no-op to
    /// retry, it is a finished execution.
    Terminal { at_event_id: String },
    /// A link is missing. **Named**, so the caller waits for a specific key or
    /// reports it — it never re-drives blindly.
    BlockedAtGap {
        missing_event_id: String,
        /// The event whose `prev_event_id` could not be resolved.
        detected_at_event_id: String,
    },
    /// No events at all — the execution has not started.
    Empty,
    /// Two events claim the same predecessor. Single-writer exclusion should
    /// make this unreachable; it is an alarm, not an expected state.
    Forked {
        prev_event_id: String,
        first: String,
        second: String,
    },
}

impl AdvanceDecision {
    /// Stable label, for pinning a metric's label values.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Advance { .. } => "advance",
            Self::Terminal { .. } => "terminal",
            Self::BlockedAtGap { .. } => "blocked_at_gap",
            Self::Empty => "empty",
            Self::Forked { .. } => "forked",
        }
    }

    /// Every label, so a counter can pin all of them at 0 unconditionally.
    /// A labelled metric is absent until it fires, and absent reads the same as
    /// a broken exporter.
    pub const ALL_LABELS: [&'static str; 5] =
        ["advance", "terminal", "blocked_at_gap", "empty", "forked"];

    /// ⭐ Whether the reconcile poller should re-drive on this decision.
    ///
    /// **`false` for every variant**, and that is the whole point. Today every
    /// non-advance is a retry; here each outcome is actionable on its own:
    /// advance acts, terminal stops, a gap waits on a named key, empty has not
    /// begun, a fork is an alarm. Nothing is left needing a blind retry, so
    /// nothing needs a cap to bound it.
    pub fn requires_redrive(&self) -> bool {
        false
    }
}

/// **The chain-following advance decision.**
///
/// Cost: one pass to index, then one hop per event following `prev_event_id` —
/// `O(k)` in the execution's own length, with no dependence on how many other
/// executions exist. Contrast the scan-based path, whose cost is proportional
/// to the whole store.
pub fn decide(view: &ChainView) -> AdvanceDecision {
    if view.events.is_empty() {
        return AdvanceDecision::Empty;
    }

    // Index by event id (one probe per hop) and detect a fork while doing it.
    let mut by_id: BTreeMap<&str, &ChainEventView> = BTreeMap::new();
    let mut successor_of: BTreeMap<&str, &str> = BTreeMap::new();
    for e in &view.events {
        by_id.insert(e.event_id.as_str(), e);
    }
    for e in &view.events {
        if let Some(prev) = e.prev_event_id.as_deref() {
            if let Some(first) = successor_of.insert(prev, e.event_id.as_str()) {
                // Two events claim the same predecessor.
                let (a, b) = if first <= e.event_id.as_str() {
                    (first, e.event_id.as_str())
                } else {
                    (e.event_id.as_str(), first)
                };
                return AdvanceDecision::Forked {
                    prev_event_id: prev.to_string(),
                    first: a.to_string(),
                    second: b.to_string(),
                };
            }
        }
    }

    // The head is the event nothing succeeds. Ties broken by exec_seq so a
    // partially-replicated view still picks the newest it holds.
    let head = view
        .events
        .iter()
        .filter(|e| !successor_of.contains_key(e.event_id.as_str()))
        .max_by_key(|e| e.exec_seq)
        .expect("non-empty");

    // Walk head -> root. A missing link is named at the event that referenced
    // it, which is what makes the block actionable.
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut cursor = head;
    loop {
        if !seen.insert(cursor.event_id.as_str()) {
            // A cycle cannot occur in a single-parent chain written by one
            // writer, but a corrupt view must not spin forever.
            return AdvanceDecision::Forked {
                prev_event_id: cursor
                    .prev_event_id
                    .clone()
                    .unwrap_or_else(|| "<cycle>".into()),
                first: cursor.event_id.clone(),
                second: cursor.event_id.clone(),
            };
        }
        match cursor.prev_event_id.as_deref() {
            None => break, // reached the root: the chain is whole
            Some(prev) => match by_id.get(prev) {
                Some(p) => cursor = p,
                None => {
                    return AdvanceDecision::BlockedAtGap {
                        missing_event_id: prev.to_string(),
                        detected_at_event_id: cursor.event_id.clone(),
                    }
                }
            },
        }
    }

    if head.terminal {
        AdvanceDecision::Terminal {
            at_event_id: head.event_id.clone(),
        }
    } else {
        AdvanceDecision::Advance {
            from_event_id: head.event_id.clone(),
            from_exec_seq: head.exec_seq,
        }
    }
}

/// **A model of today's scan-based decision**, for the contrast the RFC needs.
///
/// ⚠ This is deliberately a *model*, not the real `trigger_orchestrator_inner`
/// — that needs a database, a worker pool and a playbook. What it reproduces is
/// the one property that causes the stall: the scan path can only report a
/// boolean, so an execution it cannot fully build is reported as "did not
/// advance", identically to a finished one.
///
/// `spine_buildable` is the caller's model of whether the scan could assemble
/// the state. Returning `false` is what produces the endless re-drive.
pub fn legacy_would_advance(view: &ChainView, spine_buildable: bool) -> bool {
    if !spine_buildable {
        return false; // a no-op -> re-driven forever
    }
    match view.events.iter().max_by_key(|e| e.exec_seq) {
        Some(h) => !h.terminal,
        None => false,
    }
}
