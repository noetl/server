//! The D3 serve decision: when may a stored projection snapshot be used?
//! (noetl/ai-meta#332, building on #265.)
//!
//! # The asymmetry this module is built around
//!
//! The two failure directions are **not** symmetric, and the whole design follows
//! from that:
//!
//! * A snapshot **behind** the spine is *slow but correct*. `rebuild_state` folds
//!   every event after the snapshot's version, so a stale snapshot costs extra
//!   folding and yields the same answer.
//! * A snapshot **ahead** of the spine is *silently wrong*. The events between the
//!   real watermark and the claimed one are never folded, the caller receives a
//!   state that never existed, and nothing downstream can detect it — a rebuild has
//!   no second opinion.
//!
//! So behind is servable and ahead must never be. Measured on prod 2026-09-10:
//! **11 of 11** read attempts refused with `stored_behind_spine` and `served_tier`
//! stayed 0 — the tier was being refused for the one condition that is safe.
//!
//! # Why a grant type rather than an `if`
//!
//! "Ahead must never serve" written as a branch is a branch someone can reorder,
//! invert, or add an early-return in front of. [`ServeGrant`] carries a private
//! field and has **no public constructor**, so the only way to obtain one is
//! [`ServeGrant::evaluate`] — which cannot produce one for an ahead snapshot.
//! A caller that wanted to serve an ahead snapshot could not express it: there is
//! no value to pass. That is the difference between *unlikely* and *impossible*.

use serde::Serialize;

/// Why a stored snapshot may not be served.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum RefuseReason {
    /// ⚠ The stored record claims a version the spine has not reached. Serving
    /// this would skip the events in between — the silent-wrong case.
    StoredAhead,
    /// Stored and freshly-folded digests disagree at the same version.
    DigestMismatch,
    /// Nothing stored for this execution.
    NoStoredRecord,
    /// The spine itself could not be folded.
    SpineRefused,
}

impl RefuseReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StoredAhead => "stored_ahead",
            Self::DigestMismatch => "digest_mismatch",
            Self::NoStoredRecord => "no_stored_record",
            Self::SpineRefused => "spine_refused",
        }
    }
}

/// **Permission to serve a stored snapshot.**
///
/// The private field is the point: there is no public constructor and no
/// `Default`, so this value cannot be created outside [`Self::evaluate`]. An
/// ahead snapshot therefore has no representable grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServeGrant {
    /// Fold every event with id greater than this before using the state.
    forward_fold_from: i64,
    exact: bool,
}

impl ServeGrant {
    /// The version the caller must fold forward from. Always `<=` the spine
    /// version by construction.
    pub fn forward_fold_from(self) -> i64 {
        self.forward_fold_from
    }
    /// True when the snapshot was already at the spine's version (no catch-up
    /// folding needed). Distinguishes `served_tier` from `stale_within_window`.
    pub fn is_exact(self) -> bool {
        self.exact
    }
    pub fn outcome_label(self) -> &'static str {
        if self.exact {
            "served_tier"
        } else {
            "stale_within_window"
        }
    }

    /// The **only** way to obtain a grant.
    ///
    /// ⚠ The `stored_version > spine_version` check is first and returns before
    /// any other consideration, so no later condition can reach past it.
    pub fn evaluate(
        digests_agree_at_stored_version: bool,
        stored_version: Option<i64>,
        spine_version: i64,
    ) -> Result<ServeGrant, RefuseReason> {
        let Some(stored) = stored_version else {
            return Err(RefuseReason::NoStoredRecord);
        };
        // FAIL CLOSED, FIRST. Nothing below may run for an ahead snapshot.
        if stored > spine_version {
            return Err(RefuseReason::StoredAhead);
        }
        if !digests_agree_at_stored_version {
            return Err(RefuseReason::DigestMismatch);
        }
        Ok(ServeGrant {
            forward_fold_from: stored,
            exact: stored == spine_version,
        })
    }
}

/// **Enforced positive + negative controls for the serve decision.**
///
/// ⚠ Every case is driven through the real [`ServeGrant::evaluate`]. A report or
/// rollout that quotes a serve rate without these having fired is quoting a
/// number from a decision function nobody proved can refuse.
pub fn serve_controls() -> Vec<(&'static str, bool, &'static str)> {
    vec![
    // POSITIVE: the safe cases must serve.
        (
        "exact_serves",
        matches!(ServeGrant::evaluate(true, Some(100), 100), Ok(g) if g.is_exact()
            && g.forward_fold_from() == 100),
        "a snapshot at the spine version must serve, exactly",
        ),
        (
        "behind_serves_with_forward_fold",
        matches!(ServeGrant::evaluate(true, Some(80), 100), Ok(g) if !g.is_exact()
            && g.forward_fold_from() == 80),
        "a BEHIND snapshot must serve and report where to fold from -- it is slow but correct",
        ),

    // NEGATIVE: the unsafe cases must refuse. These are the load-bearing ones.
        (
        "ahead_refuses",
        ServeGrant::evaluate(true, Some(101), 100) == Err(RefuseReason::StoredAhead),
        "an AHEAD snapshot must REFUSE -- serving it skips events and is silently wrong",
        ),
        (
        "ahead_refuses_even_when_digests_agree",
        ServeGrant::evaluate(true, Some(500), 100) == Err(RefuseReason::StoredAhead),
        "ahead must refuse even with agreeing digests: the digest is computed at the \
         stored version and says nothing about the events the spine has not reached",
        ),
        (
        "digest_mismatch_refuses",
        ServeGrant::evaluate(false, Some(80), 100) == Err(RefuseReason::DigestMismatch),
        "a behind snapshot whose digest disagrees must refuse -- behind is servable, wrong is not",
        ),
    // ⚠ ORDERING control. The ahead check must run BEFORE the digest check, so
    // that no later condition can ever be reached for an ahead snapshot. Today
    // reordering them still refuses (both arms refuse), so the difference is only
    // the reported reason -- which is exactly why it needs asserting: the day
    // someone adds a serve path to the digest branch, the ordering is what stops
    // an ahead snapshot reaching it. Caught by asserting the reason on the
    // ahead+disagree combination, the one input where the two orders differ.
        (
        "ahead_is_checked_before_digest",
        ServeGrant::evaluate(false, Some(101), 100) == Err(RefuseReason::StoredAhead),
        "ahead+digest-disagree must report StoredAhead, proving ahead is evaluated \
         first and nothing downstream can be reached for an ahead snapshot",
        ),

        (
        "absent_refuses",
        ServeGrant::evaluate(true, None, 100) == Err(RefuseReason::NoStoredRecord),
        "no stored record must refuse, not serve an empty state",
        ),

    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_serve_control_fires() {
        let cs = serve_controls();
        assert!(cs.len() >= 7, "expected >=7 controls, found {}", cs.len());
        for (name, passed, detail) in &cs {
            assert!(passed, "control {name} did NOT fire: {detail}");
        }
    }

    /// ⚠ The structural claim, asserted as a property rather than a case: across a
    /// wide sweep, a grant is NEVER produced when stored > spine.
    #[test]
    fn a_grant_is_never_issued_for_an_ahead_snapshot() {
        let mut ahead = 0;
        for spine in [0i64, 1, 7, 100, 10_000, i64::MAX / 2] {
            for delta in [1i64, 2, 9, 1000, i64::MAX / 4] {
                let stored = spine.saturating_add(delta);
                ahead += 1;
                assert_eq!(
                    ServeGrant::evaluate(true, Some(stored), spine),
                    Err(RefuseReason::StoredAhead),
                    "stored={stored} spine={spine} produced a grant"
                );
            }
        }
        assert!(ahead >= 25, "sweep too small to mean anything: {ahead}");
    }

    /// A behind snapshot always folds forward from its own version, never from
    /// the spine's — folding from the spine would skip the very events the
    /// forward fold exists to replay.
    #[test]
    fn forward_fold_starts_at_the_snapshot_not_the_spine() {
        let g = ServeGrant::evaluate(true, Some(42), 99).expect("behind must serve");
        assert_eq!(g.forward_fold_from(), 42);
        assert!(!g.is_exact());
        assert_eq!(g.outcome_label(), "stale_within_window");
    }

    #[test]
    fn exact_and_stale_are_labelled_apart() {
        let e = ServeGrant::evaluate(true, Some(7), 7).unwrap();
        let s = ServeGrant::evaluate(true, Some(6), 7).unwrap();
        assert_eq!(e.outcome_label(), "served_tier");
        assert_eq!(s.outcome_label(), "stale_within_window");
        assert_ne!(e.outcome_label(), s.outcome_label());
    }
}
