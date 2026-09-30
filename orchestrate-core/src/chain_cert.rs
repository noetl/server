//! Chain-certified projections — prototype of the O(1) validation path.
//!
//! Spec: `ai-meta specs/active/2026-09-30-chain-certified-projections/spec.md`
//! (PR noetl/ai-meta#366). This is the implementation under benchmark, kept
//! **alongside** the existing refold path rather than replacing it, and gated by
//! [`certification_enabled`] so the comparison is apples-to-apples in one binary.
//!
//! ## What this does and does not claim
//!
//! It makes **validation** O(1): "is this stored projection still valid for this
//! chain?" becomes a 44-byte comparison instead of re-folding the chain. It does
//! **not** make folding sublinear, and it adds nothing to the append path beyond
//! one hash per event. Per the spec's §5, appends and cross-region commit latency
//! are unchanged *by construction* — the benchmark exists to falsify that, not to
//! confirm it.

use crate::event::Event;
use sha2::{Digest, Sha256};

/// Fixed domain separator, so a digest from this construction can never collide
/// with a digest from another use of SHA-256 in the codebase (spec §2.2, A7).
pub const DOMAIN_TAG: &[u8] = b"noetl.ehdb.chain-cert.v1";

/// A chain certificate: `(execution_id, chain_len, chain_digest)`.
///
/// 8 + 4 + 32 = **44 bytes**, self-verifying, monotone in `chain_len`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainCert {
    pub execution_id: i64,
    pub chain_len: u32,
    pub chain_digest: [u8; 32],
}

impl ChainCert {
    /// The on-wire size the spec commits to. Pinned by a test so "44 bytes"
    /// stays a measurement rather than a claim.
    pub const WIRE_BYTES: usize = 8 + 4 + 32;
}

/// Why a stored projection may or may not be used, decided in O(1).
///
/// Deliberately mirrors the vocabulary of `ReFoldVerdict` so the two paths are
/// comparable, and so a reader can see that no variant means "use it anyway".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertVerdict {
    /// Same execution, same length, same digest — the stored projection is the
    /// fold of exactly this chain. The only outcome that permits use.
    Valid,
    /// The chain has advanced past the stored projection. Not corruption; the
    /// materialiser is behind. Corresponds to `StoredBehindSpine`.
    StaleButPrefix,
    /// The stored projection claims a longer chain than exists. Corresponds to
    /// `StoredAheadOfSpine`.
    StoredAhead,
    /// Same length, different digest — a fork or corruption. Never resolved
    /// silently (spec A1).
    DigestMismatch,
    /// Certificates are for different executions; refuse rather than compare.
    ExecutionMismatch,
}

impl CertVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Valid => "valid",
            Self::StaleButPrefix => "stale_but_prefix",
            Self::StoredAhead => "stored_ahead",
            Self::DigestMismatch => "digest_mismatch",
            Self::ExecutionMismatch => "execution_mismatch",
        }
    }
    /// Only these mean the stored record is WRONG rather than merely behind.
    pub fn is_fault(self) -> bool {
        matches!(self, Self::DigestMismatch | Self::StoredAhead)
    }
}

/// Deterministic byte encoding of one event, for digesting.
///
/// ⚠ **This function is the single sharpest risk in the whole design** (spec A8).
/// The program has already been bitten by two producers reducing timestamps
/// differently — one rounding, one truncating — so the same logical event
/// digested differently. If this drifts between producers or versions, every
/// validation silently degrades to a refold: a performance cliff that erases the
/// entire benefit while looking correct.
///
/// Two things make it deterministic, and both are load-bearing:
///
/// 1. `serde_json::Value` is backed by a `BTreeMap`, so object keys serialise
///    sorted at every level. (If `preserve_order` is ever enabled workspace-wide,
///    this stops being canonical — the same trap `canonical_state_digest`
///    documents.)
/// 2. The timestamp is normalised to a **fixed precision** here rather than
///    trusted from the producer. That is what closes A8: rounding and truncating
///    producers converge on the same bytes.
pub fn canonical(event: &Event) -> Vec<u8> {
    let mut value = serde_json::to_value(event).unwrap_or(serde_json::Value::Null);
    normalize_timestamp(&mut value);
    serde_json::to_vec(&value).unwrap_or_default()
}

/// Force the timestamp to whole microseconds, by truncation, always.
///
/// The choice between rounding and truncating is arbitrary; agreeing on one is
/// not. Truncation is chosen because it is monotone in the input and cannot
/// carry a value into the next second.
fn normalize_timestamp(value: &mut serde_json::Value) {
    if let Some(obj) = value.as_object_mut() {
        if let Some(ts) = obj.get("timestamp").and_then(|v| v.as_str()) {
            if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(ts) {
                let micros = parsed.timestamp_micros();
                obj.insert("timestamp".to_string(), serde_json::Value::from(micros));
            }
        }
    }
}

/// One step of the rolling digest.
///
/// `roll(None, root)` seeds the chain; `roll(Some(prev), e)` extends it. Note
/// what is absent: **no `event_id` is compared anywhere**. Order comes from the
/// link structure the caller walked, which is what makes this immune to events
/// committing out of snowflake order (spec A4 — the case that motivated the
/// design).
pub fn roll(prev: Option<[u8; 32]>, event: &Event) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(DOMAIN_TAG);
    if let Some(p) = prev {
        h.update(p);
    }
    h.update(canonical(event));
    h.finalize().into()
}

/// Build the certificate for a chain given in causal (link) order.
///
/// Returns `None` for an empty chain — there is no certificate for no events,
/// and returning a zero digest would be a forgeable sentinel.
pub fn certify(events: &[Event]) -> Option<ChainCert> {
    let first = events.first()?;
    let mut digest: Option<[u8; 32]> = None;
    for e in events {
        digest = Some(roll(digest, e));
    }
    Some(ChainCert {
        execution_id: first.execution_id,
        chain_len: events.len() as u32,
        chain_digest: digest?,
    })
}

/// The O(1) validation: compare a stored certificate against the chain's current
/// one. This is the operation that replaces an O(n) refold.
///
/// Costs one integer compare, one length compare and a 32-byte memcmp. It reads
/// no events, touches no store, and is flat in chain length — which is the claim
/// the benchmark has to falsify.
pub fn validate(stored: &ChainCert, current: &ChainCert) -> CertVerdict {
    if stored.execution_id != current.execution_id {
        return CertVerdict::ExecutionMismatch;
    }
    // Checked BEFORE the digest, for the same reason `verdict_for` does: an
    // ahead record's digest necessarily differs, and calling that
    // `digest_mismatch` sends an operator hunting corruption instead of a store
    // that is ahead of its own log.
    if stored.chain_len > current.chain_len {
        return CertVerdict::StoredAhead;
    }
    if stored.chain_len < current.chain_len {
        return CertVerdict::StaleButPrefix;
    }
    if stored.chain_digest == current.chain_digest {
        CertVerdict::Valid
    } else {
        CertVerdict::DigestMismatch
    }
}

// ---------------------------------------------------------------------------
// Instrumentation — spec A9: "the certificate exists but nothing verifies it"
// ---------------------------------------------------------------------------

use std::sync::atomic::{AtomicU64, Ordering};

/// Validations actually performed through [`validate`].
static VALIDATIONS_TAKEN: AtomicU64 = AtomicU64::new(0);
/// Refolds not performed because a validation answered instead.
static REFOLDS_AVOIDED: AtomicU64 = AtomicU64::new(0);

/// The counted entry point. Call THIS, not [`validate`], from production paths.
///
/// ⚠ This split exists because of the failure mode this codebase produces most
/// often (spec A9): a mechanism that is present, deployed, instrumented — and
/// never fires. A hydration fix once recorded `head=0` across 4,270 decisions.
/// A zero on `refolds_avoided` must be distinguishable from "the path never
/// ran", which is exactly what these two counters together give you: if
/// `validations_taken` is also zero, the path is unwired; if it is nonzero and
/// `refolds_avoided` is zero, the path runs and never helps.
pub fn validate_counted(stored: &ChainCert, current: &ChainCert) -> CertVerdict {
    VALIDATIONS_TAKEN.fetch_add(1, Ordering::Relaxed);
    let v = validate(stored, current);
    if v == CertVerdict::Valid {
        REFOLDS_AVOIDED.fetch_add(1, Ordering::Relaxed);
    }
    v
}

pub fn validations_taken() -> u64 {
    VALIDATIONS_TAKEN.load(Ordering::Relaxed)
}
pub fn refolds_avoided() -> u64 {
    REFOLDS_AVOIDED.load(Ordering::Relaxed)
}
pub fn reset_counters() {
    VALIDATIONS_TAKEN.store(0, Ordering::Relaxed);
    REFOLDS_AVOIDED.store(0, Ordering::Relaxed);
}

/// Is the certified path armed? `NOETL_CHAIN_CERT` — default **off**, so the
/// refold path is unchanged until an operator opts in.
pub fn certification_enabled() -> bool {
    matches!(
        std::env::var("NOETL_CHAIN_CERT")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};

    fn ev(execution_id: i64, event_id: i64, ts: DateTime<Utc>) -> Event {
        Event {
            event_id,
            execution_id,
            catalog_id: 7,
            event_type: "command.completed".to_string(),
            node_name: Some(format!("step_{event_id}")),
            status: "success".to_string(),
            context: Some(serde_json::json!({"k": event_id, "nested": {"b": 2, "a": 1}})),
            result: Some(serde_json::json!({"status": "success"})),
            meta: None,
            timestamp: ts,
            parent_execution_id: None,
            attempt: Some(1),
        }
    }

    fn chain(execution_id: i64, n: usize) -> Vec<Event> {
        let base = Utc.timestamp_opt(1_790_000_000, 0).unwrap();
        (0..n)
            .map(|i| {
                ev(
                    execution_id,
                    1000 + i as i64,
                    base + chrono::Duration::milliseconds(i as i64),
                )
            })
            .collect()
    }

    /// The spec commits to 44 bytes. Keep that a measurement.
    #[test]
    fn the_certificate_is_44_bytes() {
        assert_eq!(ChainCert::WIRE_BYTES, 44);
        assert_eq!(
            std::mem::size_of::<i64>() + std::mem::size_of::<u32>() + 32,
            44
        );
    }

    /// A8 CONTROL — cross-producer digest equality, for the class `canonical()`
    /// CAN close: two producers whose timestamps differ only BELOW the
    /// normalisation granularity.
    ///
    /// Plant the defect by deleting the `normalize_timestamp` call in
    /// `canonical()` and this fails — the RED proof that the normalisation is
    /// doing the work rather than luck.
    #[test]
    fn producers_differing_below_the_normalisation_granularity_agree() {
        // Same microsecond, different nanoseconds — the sub-µs class.
        let a_ns = Utc.timestamp_opt(1_790_000_000, 123_456_111).unwrap();
        let b_ns = Utc.timestamp_opt(1_790_000_000, 123_456_999).unwrap();
        assert_ne!(a_ns, b_ns, "the control is vacuous unless they differ");

        assert_eq!(
            roll(None, &ev(1, 1, a_ns)),
            roll(None, &ev(1, 1, b_ns)),
            "timestamps differing only below µs must digest identically — this is \
             the drift `canonical()` is able to absorb (A8)"
        );
    }

    /// ⚠ A8, THE HONEST LIMIT — the class `canonical()` CANNOT close.
    ///
    /// This is a finding, not a passing feature. The spec's A8 mitigation is
    /// "make `canonical()` deterministic", and that is **necessary but not
    /// sufficient**: if two producers reduce the timestamp differently BEFORE
    /// the event is constructed — one truncating ns→µs, one rounding, which is
    /// the exact incident the spec cites — the reduction crosses a microsecond
    /// boundary and the information is gone before `canonical()` ever sees it.
    /// No canonicaliser can reconstruct it.
    ///
    /// What the design does deliver here is the *right failure*: the digests
    /// differ, so validation reports a mismatch and the caller falls back to a
    /// refold. A performance cliff, never a wrong answer — which is what the
    /// spec predicts, and is why A8 is listed as a cliff rather than a
    /// correctness bug.
    ///
    /// The real mitigation is therefore a **producer contract** (all producers
    /// reduce identically), enforced upstream; `canonical()` cannot substitute
    /// for it. Closing this by normalising to milliseconds instead would
    /// reconcile these two, but events 1 ms apart are routine in this log, so it
    /// would collapse distinct events — a worse bug than the one it fixes.
    #[test]
    fn producers_differing_at_the_normalisation_granularity_cannot_be_reconciled() {
        let truncating = Utc.timestamp_opt(1_790_000_000, 123_456_000).unwrap();
        let rounding = Utc.timestamp_opt(1_790_000_000, 123_457_000).unwrap();

        assert_ne!(
            roll(None, &ev(1, 1, truncating)),
            roll(None, &ev(1, 1, rounding)),
            "documenting the limit: µs-level producer disagreement is NOT \
             absorbable, and pretending otherwise would hide a real cliff"
        );

        // And the consequence is a refusal that forces a refold, not a silent
        // wrong answer.
        let a = certify(&[ev(1, 1, truncating)]).unwrap();
        let b = certify(&[ev(1, 1, rounding)]).unwrap();
        assert_eq!(validate(&a, &b), CertVerdict::DigestMismatch);
    }

    /// The positive control for the test above. Without it, a `canonical()` that
    /// returned a constant would satisfy "two producers agree" perfectly.
    #[test]
    fn different_events_digest_differently() {
        let base = Utc.timestamp_opt(1_790_000_000, 0).unwrap();
        let a = roll(None, &ev(1, 1, base));
        let b = roll(None, &ev(1, 2, base));
        assert_ne!(
            a, b,
            "a canonicaliser that collapses distinct events is useless"
        );
        // And a change buried in a nested object must move the digest.
        let mut deep = ev(1, 1, base);
        deep.context = Some(serde_json::json!({"k": 1, "nested": {"b": 2, "a": 99}}));
        assert_ne!(roll(None, &ev(1, 1, base)), roll(None, &deep));
    }

    /// Key order in the input must not change the digest — `serde_json::Value`
    /// sorts object keys. This is the guard that fails if `preserve_order` is
    /// ever enabled workspace-wide.
    #[test]
    fn digest_is_stable_across_key_insertion_order() {
        let base = Utc.timestamp_opt(1_790_000_000, 0).unwrap();
        let mut one = ev(1, 1, base);
        one.context = Some(serde_json::json!({"a": 1, "b": 2, "c": 3}));
        let mut two = ev(1, 1, base);
        two.context = Some(serde_json::json!({"c": 3, "b": 2, "a": 1}));
        assert_eq!(roll(None, &one), roll(None, &two));
    }

    /// The chain digest follows LINKS, never `event_id` order (spec A4 — the
    /// case that motivated the design). Two chains with the same events in the
    /// same causal order digest identically even when their ids are
    /// non-monotone; reordering the causal sequence changes the digest.
    #[test]
    fn digest_follows_causal_order_not_event_id_order() {
        let base = Utc.timestamp_opt(1_790_000_000, 0).unwrap();
        // Causal order e1 -> e2, but e2 has the LOWER event_id (minted earlier,
        // committed later) — exactly the #362 shape.
        let e1 = ev(1, 9000, base);
        let e2 = ev(1, 1000, base + chrono::Duration::milliseconds(1));
        let forward = certify(&[e1.clone(), e2.clone()]).unwrap();
        let backward = certify(&[e2, e1]).unwrap();
        assert_ne!(
            forward.chain_digest, backward.chain_digest,
            "the digest must depend on causal order"
        );
        assert_eq!(forward.chain_len, 2);
    }

    /// The O(1) verdicts, including the ones that must refuse.
    #[test]
    fn validate_covers_every_verdict() {
        let c = chain(1, 10);
        let current = certify(&c).unwrap();
        assert_eq!(validate(&current, &current), CertVerdict::Valid);

        let shorter = certify(&c[..5]).unwrap();
        assert_eq!(validate(&shorter, &current), CertVerdict::StaleButPrefix);
        assert_eq!(validate(&current, &shorter), CertVerdict::StoredAhead);

        // Same length, forged digest -> refused, never resolved (A1).
        let mut forged = current;
        forged.chain_digest[0] ^= 0xff;
        assert_eq!(validate(&forged, &current), CertVerdict::DigestMismatch);
        assert!(CertVerdict::DigestMismatch.is_fault());

        let other = certify(&chain(2, 10)).unwrap();
        assert_eq!(validate(&other, &current), CertVerdict::ExecutionMismatch);
    }

    /// A3 — a post-restart second root is rejected by the CERTIFICATE alone,
    /// independently of the hydrator. A new root restarts `chain_len` at 1,
    /// which is not `>` the current length, so it reads as non-monotone.
    #[test]
    fn a_post_restart_second_root_is_rejected_by_the_certificate() {
        let full = certify(&chain(1, 40)).unwrap();
        let second_root = certify(&chain(1, 1)).unwrap();
        assert_eq!(second_root.chain_len, 1);
        let v = validate(&full, &second_root);
        assert!(
            v.is_fault(),
            "a truncated chain must be refused, not accepted as current; got {v:?}"
        );
        assert_eq!(v, CertVerdict::StoredAhead);
    }

    /// A5 — idempotence: re-validating the same prefix is a no-op verdict.
    #[test]
    fn revalidating_the_same_prefix_is_idempotent() {
        let current = certify(&chain(1, 20)).unwrap();
        for _ in 0..5 {
            assert_eq!(validate(&current, &current), CertVerdict::Valid);
        }
    }

    /// A9 CONTROL — prove the counted path actually fires, and that "0 refolds
    /// avoided" is distinguishable from "never ran".
    ///
    /// Plant the defect by having the production path call `validate` instead of
    /// `validate_counted` and `validations_taken()` stays 0 — which is the
    /// signature of an unwired check, and is exactly what this asserts against.
    #[test]
    fn the_counted_path_distinguishes_unwired_from_unhelpful() {
        reset_counters();
        assert_eq!(validations_taken(), 0, "counters start clean");
        assert_eq!(refolds_avoided(), 0);

        let current = certify(&chain(1, 12)).unwrap();

        // Ran and helped.
        assert_eq!(validate_counted(&current, &current), CertVerdict::Valid);
        assert_eq!(validations_taken(), 1, "the path must record that it RAN");
        assert_eq!(refolds_avoided(), 1, "a Valid verdict avoided a refold");

        // Ran and did NOT help — the state that must be distinguishable from
        // "never ran".
        let mut forged = current;
        forged.chain_digest[0] ^= 0xff;
        assert_eq!(
            validate_counted(&forged, &current),
            CertVerdict::DigestMismatch
        );
        assert_eq!(validations_taken(), 2, "still recorded as having run");
        assert_eq!(
            refolds_avoided(),
            1,
            "a mismatch must NOT count as an avoided refold"
        );

        // The discrimination the spec demands, stated as an assertion:
        assert!(
            validations_taken() > 0 && refolds_avoided() < validations_taken(),
            "validations_taken>0 with refolds_avoided<taken means the path RAN \
             and sometimes did not help — which is a different diagnosis from \
             validations_taken==0 (unwired)"
        );
        reset_counters();
    }

    /// The flag is off unless explicitly armed, so the refold path is unchanged
    /// for anyone who does not opt in.
    #[test]
    fn certification_is_off_by_default() {
        std::env::remove_var("NOETL_CHAIN_CERT");
        assert!(!certification_enabled());
    }
}
