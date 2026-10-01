//! Chain-certified projections — the certificate and the O(1) fold skip.
//!
//! Spec: ai-meta#366. A rolling SHA-256 over each execution's chain, so
//! deciding "is my folded state still valid?" is a 32-byte compare instead of
//! re-folding the chain with [`crate::state::WorkflowState::from_events`].
//!
//! # Why this is server-side
//!
//! The digest is only an O(1) win if the reader can get it WITHOUT reading the
//! chain. The server is the **producer** — every event arrives through its
//! ingest path — so it can advance the digest once per event as it writes, and
//! then compare in constant time on every subsequent fold. A reader that had to
//! read the chain to derive the digest would have done the O(n) work anyway.
//!
//! ehdb maintains the same construction inside `PartWriter::append` for the
//! storage tier (noetl/ehdb#379). Both are pinned to the same golden vector,
//! computed by a third implementation, so the two cannot silently diverge.
//!
//! # What makes this sound
//!
//! The digest is taken over the bytes the event SERIALISES to, and
//! `Event::serialize_timestamp_micros` (noetl/server#483) makes those bytes
//! deterministic across producers. Without that, two producers rendering the
//! same instant at different sub-microsecond precision derive different digests
//! — spec guardrail A8.
//!
//! # The failure mode this module is built against
//!
//! A cache that returns stale state is worse than no cache. So the skip is
//! permitted **only** on an exact `(chain_len, digest)` match, and every other
//! outcome — absent certificate, shorter chain, longer chain, same length with
//! a different digest — falls back to a real refold. `FoldDecision` enumerates
//! them so a reviewer can see there is no default-allow path, and
//! `a_mismatched_certificate_must_not_serve_cached_state` plants each one.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

/// Domain separation. Shared with `ehdb_l0::chain_cert::DOMAIN_TAG`; changing
/// it forks every chain.
pub const DOMAIN_TAG: &[u8] = b"noetl.ehdb.chain-cert.v1";

/// Events per digest advance.
///
/// Measured against the recorded corpus (chains of ~36-174 events). A chunk
/// larger than a typical chain would leave short executions with nothing sealed
/// and nothing to compare in O(1) — an inert path that still reads like a
/// feature. Guarded by `a_short_chain_still_seals_at_least_one_chunk`.
pub const CHUNK_EVENTS: u32 = 8;

/// How many consecutive skips an execution may take before a real fold is
/// forced, to re-run the precondition check.
///
/// ⚠ THIS CONSTANT IS WHAT MAKES THE MISS-DETECTOR ABLE TO FIRE AT ALL, and it
/// exists because the first version of this module could not detect the worst
/// case it was built for.
///
/// `reconcile` only learns the chain's real length from a fold. But a process
/// that has stopped seeing an execution's events has a roller that stops
/// advancing — so `decide` reads "nothing changed", skips, and therefore never
/// folds, and therefore never reconciles. The miss would go undetected forever
/// while the skip served progressively staler state. Detection required the
/// very fold the skip was removing.
///
/// Forcing a fold every `MAX_CONSECUTIVE_SKIPS` bounds that blind window. At
/// 16, with the reconciler re-driving roughly every 8s, an undetected miss is
/// caught within ~2 minutes, and at most 1 fold in 17 is kept — 94% of folds
/// still eliminated, above the spec's >=90% gate.
///
/// Raising it widens the blind window; lowering it costs folds. Do not raise it
/// without re-deriving both numbers.
pub const MAX_CONSECUTIVE_SKIPS: u32 = 16;

/// Most executions this cache will track at once.
///
/// ⚠ WITHOUT THIS THE CACHE IS A MEMORY LEAK. `forget` had no callers, so with
/// the flag on every execution the process ever saw kept a roller, a cached
/// certificate, an offset, a calibration marker and a skip counter — and the
/// server's own rebuild cache kept a whole `WorkflowState` alongside — for the
/// lifetime of the process. A server that runs for days would grow without
/// bound. (Flag off, neither populates, so this was never a risk before the
/// ramp.)
///
/// Eviction is ALWAYS SAFE here, which is what makes a simple policy correct:
/// this is a cache, and a miss costs one real fold. Oldest-first by insertion
/// is enough; no LRU bookkeeping is justified.
pub const MAX_TRACKED_EXECUTIONS: usize = 2048;

/// `(chain_len, chain_digest)` for one execution — 36 bytes, 44 with the id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainCert {
    pub chain_len: u32,
    pub chain_digest: [u8; 32],
}

impl ChainCert {
    pub const WIRE_BYTES: usize = 4 + 32;
}

/// Why a fold was or was not skipped.
///
/// Exhaustive on purpose: the only variant that permits reuse is
/// [`Self::SkipValid`], and every other reason to not-match lands on a refold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldDecision {
    /// Exact match — the cached state is still the fold of this chain.
    SkipValid,
    /// Nothing cached for this execution yet.
    RefoldNoCachedState,
    /// No certificate for this execution yet (fewer than `CHUNK_EVENTS`
    /// events, or the path never observed it).
    RefoldNoCertificate,
    /// The chain advanced past what the cached state covers.
    RefoldChainAdvanced,
    /// The cached certificate is LONGER than the current chain. Should not
    /// happen; treated as a refold rather than trusted.
    RefoldCachedAhead,
    /// Same length, different digest — the chains diverged. The dangerous one:
    /// a length-only check would have called this valid.
    RefoldDigestMismatch,
    /// This execution's roller has not yet been calibrated against a real
    /// chain length by [`CertifiedFoldCache::reconcile`].
    RefoldUncalibrated,
    /// The certificate matches, but this execution has taken
    /// [`MAX_CONSECUTIVE_SKIPS`] skips in a row — fold anyway so the
    /// precondition check runs. See that constant for why this is not optional.
    RefoldRevalidationDue,
    /// This process has been proven to have missed events (see
    /// [`CertifiedFoldCache::reconcile`]), so no roller is trustworthy. Fails
    /// closed for every execution, not only the one that revealed it.
    RefoldDivergenceDetected,
}

impl FoldDecision {
    /// Only an exact match may reuse cached state.
    pub fn skips_fold(self) -> bool {
        matches!(self, Self::SkipValid)
    }
}

/// Decide whether `cached` may be reused given the chain's `current`
/// certificate. Pure, so every branch is testable without a cache.
pub fn decide(cached: Option<ChainCert>, current: Option<ChainCert>) -> FoldDecision {
    let (Some(cached), Some(current)) = (cached, current) else {
        return match (cached.is_some(), current.is_some()) {
            (false, _) => FoldDecision::RefoldNoCachedState,
            (true, false) => FoldDecision::RefoldNoCertificate,
            _ => unreachable!("both-Some handled below"),
        };
    };
    if cached.chain_len < current.chain_len {
        FoldDecision::RefoldChainAdvanced
    } else if cached.chain_len > current.chain_len {
        FoldDecision::RefoldCachedAhead
    } else if cached.chain_digest == current.chain_digest {
        FoldDecision::SkipValid
    } else {
        FoldDecision::RefoldDigestMismatch
    }
}

/// One execution's rolling chain state.
#[derive(Debug, Clone)]
struct ChunkRoller {
    hasher: Sha256,
    /// Highest `event_id` this roller has absorbed.
    ///
    /// The precondition check compares against the chain HEAD rather than an
    /// event COUNT, because the orchestrator's rebuild has two branches — an
    /// incremental one that applies only events after a snapshot, and a full
    /// fold — and only one of them ever knows the full chain length. Both
    /// always know the head id.
    max_event_id: i64,
    len: u32,
    sealed_len: u32,
    sealed_digest: Option<[u8; 32]>,
    open: bool,
}

impl ChunkRoller {
    fn new() -> Self {
        Self {
            hasher: Sha256::new(),
            max_event_id: 0,
            len: 0,
            sealed_len: 0,
            sealed_digest: None,
            open: false,
        }
    }

    #[inline]
    fn absorb_with_id(&mut self, event_id: i64, body: &[u8]) {
        if event_id > self.max_event_id {
            self.max_event_id = event_id;
        }
        self.absorb(body);
    }

    #[inline]
    fn absorb(&mut self, body: &[u8]) {
        if !self.open {
            self.hasher = Sha256::new();
            self.hasher.update(DOMAIN_TAG);
            if let Some(p) = self.sealed_digest {
                self.hasher.update(p);
            }
            self.open = true;
        }
        self.hasher.update(body);
        self.len += 1;
        if self.len.is_multiple_of(CHUNK_EVENTS) {
            let d: [u8; 32] =
                sha2::digest::FixedOutputReset::finalize_fixed_reset(&mut self.hasher).into();
            self.sealed_digest = Some(d);
            self.sealed_len = self.len;
            self.open = false;
        }
    }

    fn certificate(&self) -> Option<ChainCert> {
        self.sealed_digest.map(|d| ChainCert {
            chain_len: self.sealed_len,
            chain_digest: d,
        })
    }
}

/// Counters, so a clean result can be told apart from an unwired one.
///
/// A zero on `skipped` alone is ambiguous: it means either "the check ran and
/// never helped" or "the check never ran". `observed` and `decided`
/// disambiguate, which is the A9 guardrail.
static OBSERVED: AtomicU64 = AtomicU64::new(0);
static DECIDED: AtomicU64 = AtomicU64::new(0);
static SKIPPED: AtomicU64 = AtomicU64::new(0);
static REFOLDED: AtomicU64 = AtomicU64::new(0);
/// Times a refold proved the roller had MISSED events — see
/// [`CertifiedFoldCache::reconcile`]. Non-zero means the skip's precondition is
/// violated and it must not be trusted. This is the divergence signal a ramp
/// rolls back on.
static DIVERGENCES: AtomicU64 = AtomicU64::new(0);

/// Snapshot: `(observed, decided, skipped, refolded, divergences)`.
pub fn counters() -> (u64, u64, u64, u64, u64) {
    (
        OBSERVED.load(Ordering::Relaxed),
        DECIDED.load(Ordering::Relaxed),
        SKIPPED.load(Ordering::Relaxed),
        REFOLDED.load(Ordering::Relaxed),
        DIVERGENCES.load(Ordering::Relaxed),
    )
}

/// Non-zero once any execution's roller is proven to have missed events.
/// A ramp watches this and rolls back on the first increment.
pub fn divergences() -> u64 {
    DIVERGENCES.load(Ordering::Relaxed)
}

/// Reset the counters. Tests only.
pub fn reset_counters() {
    OBSERVED.store(0, Ordering::Relaxed);
    DECIDED.store(0, Ordering::Relaxed);
    SKIPPED.store(0, Ordering::Relaxed);
    REFOLDED.store(0, Ordering::Relaxed);
    DIVERGENCES.store(0, Ordering::Relaxed);
}

/// Is the fold skip enabled? `NOETL_CHAIN_CERT`, default **off**.
///
/// Read per call rather than cached so the flag can be flipped on a running
/// process during a ramp without a restart.
pub fn fold_skip_enabled() -> bool {
    matches!(
        std::env::var("NOETL_CHAIN_CERT").ok().as_deref(),
        Some("1") | Some("true") | Some("on")
    )
}

/// Per-execution chain certificates, advanced at ingest.
///
/// `observe` is the producer side (once per event, as it is written);
/// `decide_for` is the reader side (once per fold, O(1)).
#[derive(Debug, Default)]
pub struct CertifiedFoldCache {
    rollers: HashMap<i64, ChunkRoller>,
    cached: HashMap<i64, ChainCert>,
    /// Events an execution already had before this roller started watching it.
    ///
    /// The flag can be turned on mid-flight, and a process restarts, so a roller
    /// routinely begins partway through a chain. Its digest is still a valid
    /// CHANGE DETECTOR from that point on, but its length is permanently short
    /// by however much it missed. Without recording that offset, the first
    /// `reconcile` would read as "this process missed events", fail closed, and
    /// the feature would disable itself the moment it was enabled.
    ///
    /// Set once per execution, on its first `reconcile`. Every later mismatch is
    /// then a REAL miss, which is what the guard is for.
    offsets: HashMap<i64, u32>,
    /// Consecutive skips per execution, reset by [`Self::record_fold`]. Bounds
    /// how long a missed event can go undetected — see
    /// [`MAX_CONSECUTIVE_SKIPS`].
    skips_since_fold: HashMap<i64, u32>,
    /// Executions whose baseline has been established by a `reconcile_head`.
    calibrated: std::collections::HashSet<i64>,
    /// Insertion order, for bounding the cache — see [`MAX_TRACKED_EXECUTIONS`].
    order: std::collections::VecDeque<i64>,
    /// Executions dropped by the last bound enforcement, for a parallel cache
    /// to mirror. Drained by [`Self::take_evicted`].
    evicted: Vec<i64>,
}

impl CertifiedFoldCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Advance `execution_id`'s chain with the bytes this event serialises to.
    ///
    /// ⚠ ONE map lookup on the hot path. `contains_key` + `get_mut` hashes the
    /// key twice per event, which measured as a real per-event cost in
    /// noetl/ehdb#379 and showed up first as a non-linear benchmark rather than
    /// as a number.
    /// Observe one event by the fields the fold keys on.
    ///
    /// ⚠ This, not `observe`, is what call sites use. There are at least four
    /// places that write `noetl.event`, and `handlers::ehdb_eventlog_mirror`
    /// records that the nominal chokepoint (`event_write::emit_events`) is NOT
    /// one — two sites in `handlers::events` write the table directly on the
    /// branch `should_publish` takes when false, which is the ONLY branch a
    /// system-pool execution can take. That already shipped as a defect
    /// (noetl/ai-meta#263: `system/*` mirrored 11 of 13 events per run).
    ///
    /// So the identity is built HERE from values every site already has, rather
    /// than each site serialising its own. Four sites each composing their own
    /// byte string is four chances to compose it differently, and a digest that
    /// differs by call site is a digest that reports change that did not happen.
    ///
    /// Covers event identity AND the fields the fold reacts to, so an in-place
    /// mutation (not just an append) also moves the digest.
    pub fn observe_event(
        &mut self,
        execution_id: i64,
        event_id: i64,
        event_type: &str,
        node_name: Option<&str>,
        status: &str,
    ) {
        let mut body = Vec::with_capacity(48 + event_type.len() + status.len());
        body.extend_from_slice(&event_id.to_be_bytes());
        body.push(0x1f);
        body.extend_from_slice(event_type.as_bytes());
        body.push(0x1f);
        body.extend_from_slice(node_name.unwrap_or("").as_bytes());
        body.push(0x1f);
        body.extend_from_slice(status.as_bytes());

        self.absorb_into(execution_id, event_id, &body);
    }

    /// The ONE place a roller is created or advanced.
    ///
    /// ⚠ Both `observe` and `observe_event` route through here. They were
    /// separate insertion paths at first, and only `observe_event` enforced the
    /// bound — so the cache still grew without limit through the other one. One
    /// path, one bound.
    fn absorb_into(&mut self, execution_id: i64, event_id: i64, body: &[u8]) {
        OBSERVED.fetch_add(1, Ordering::Relaxed);
        if let Some(r) = self.rollers.get_mut(&execution_id) {
            r.absorb_with_id(event_id, body);
        } else {
            let mut r = ChunkRoller::new();
            r.absorb_with_id(event_id, body);
            self.rollers.insert(execution_id, r);
            self.order.push_back(execution_id);
            self.evict_to_bound();
        }
    }

    /// Drop oldest-first until the cache is within [`MAX_TRACKED_EXECUTIONS`].
    ///
    /// Returns the executions evicted, so a caller holding a parallel cache
    /// (the server's rebuild cache) can drop them too rather than outliving
    /// this one.
    fn evict_to_bound(&mut self) -> Vec<i64> {
        let mut evicted = Vec::new();
        while self.rollers.len() > MAX_TRACKED_EXECUTIONS {
            match self.order.pop_front() {
                Some(old) => {
                    // Skip ids already forgotten by other means.
                    if self.rollers.contains_key(&old) {
                        self.forget_inner(old);
                        evicted.push(old);
                        self.evicted.push(old);
                    }
                }
                None => break,
            }
        }
        evicted
    }

    /// Executions evicted by the most recent bound enforcement, drained.
    pub fn take_evicted(&mut self) -> Vec<i64> {
        std::mem::take(&mut self.evicted)
    }

    /// How many executions are currently tracked.
    pub fn tracked_executions(&self) -> usize {
        self.rollers.len()
    }

    /// Highest `event_id` observed for `execution_id`.
    pub fn observed_head(&self, execution_id: i64) -> i64 {
        self.rollers
            .get(&execution_id)
            .map_or(0, |r| r.max_event_id)
    }

    pub fn observe(&mut self, execution_id: i64, body: &[u8]) {
        self.absorb_into(execution_id, 0, body);
    }

    /// This execution's current certificate, if a chunk has sealed.
    pub fn current(&self, execution_id: i64) -> Option<ChainCert> {
        self.rollers
            .get(&execution_id)
            .and_then(|r| r.certificate())
    }

    /// The certificate the cached folded state was built against.
    pub fn cached(&self, execution_id: i64) -> Option<ChainCert> {
        self.cached.get(&execution_id).copied()
    }

    /// Decide whether a fold can be skipped for `execution_id`, counting the
    /// outcome. Returns [`FoldDecision::RefoldNoCachedState`] when the flag is
    /// off, so a disabled ramp can never skip.
    pub fn decide_for(&self, execution_id: i64) -> FoldDecision {
        if !fold_skip_enabled() {
            return FoldDecision::RefoldNoCachedState;
        }
        // Once this process has demonstrably missed an event, its rollers are
        // no longer change detectors for anything. Fail closed, globally, not
        // just for the execution that revealed it.
        // Every decision below is COUNTED, including the refusals: a ramp needs
        // skipped/decided to be a true ratio, and "we refused because the
        // roller was never calibrated" is a refold that should show up as one.
        let d = if DIVERGENCES.load(Ordering::Relaxed) > 0 {
            FoldDecision::RefoldDivergenceDetected
        } else if !self.is_calibrated(execution_id) {
            // An uncalibrated roller has never been checked against a real
            // chain length, so its digest covers an unverified prefix. Refold
            // once to calibrate before any skip is licensed.
            FoldDecision::RefoldUncalibrated
        } else {
            let d = decide(self.cached(execution_id), self.current(execution_id));
            // A matching certificate is necessary but not sufficient: force a
            // fold periodically so the miss-detector gets to run.
            if d.skips_fold()
                && self
                    .skips_since_fold
                    .get(&execution_id)
                    .copied()
                    .unwrap_or(0)
                    >= MAX_CONSECUTIVE_SKIPS
            {
                FoldDecision::RefoldRevalidationDue
            } else {
                d
            }
        };
        DECIDED.fetch_add(1, Ordering::Relaxed);
        if d.skips_fold() {
            SKIPPED.fetch_add(1, Ordering::Relaxed);
        } else {
            REFOLDED.fetch_add(1, Ordering::Relaxed);
        }
        d
    }

    /// Events this roller has absorbed for `execution_id`.
    pub fn observed_len(&self, execution_id: i64) -> u32 {
        self.rollers.get(&execution_id).map_or(0, |r| r.len)
    }

    /// ⚠ THE PRECONDITION CHECK. Call after every real refold with the number
    /// of events the refold actually read.
    ///
    /// The whole design rests on this process having seen EVERY event for the
    /// execution — the roller is a change detector, not a chain identity, so a
    /// roller that missed an event silently stops advancing and the skip then
    /// serves stale state. That precondition holds only while a single process
    /// ingests the chain. Prod runs the server as a one-replica StatefulSet
    /// (`noetl-server-rust-embedded`) today, but that is a deployment fact, not
    /// an invariant the code can assume — scale it to two and the skip becomes
    /// unsound with no other symptom.
    ///
    /// So rather than document the hazard, detect it: a refold knows the real
    /// chain length, and if it disagrees with what the roller absorbed, this
    /// process missed events. That execution's state is dropped and
    /// [`divergences`] increments, which is the signal to roll a ramp back.
    ///
    /// Returns `true` when consistent.
    pub fn reconcile(&mut self, execution_id: i64, actual_len: u32) -> bool {
        let seen = self.observed_len(execution_id);
        match self.offsets.get(&execution_id).copied() {
            // Already calibrated: `seen + offset` must equal the real length.
            // Anything else means this process missed an event.
            Some(offset) => {
                if seen.saturating_add(offset) == actual_len {
                    true
                } else {
                    DIVERGENCES.fetch_add(1, Ordering::Relaxed);
                    self.forget(execution_id);
                    false
                }
            }
            // First sight of this execution: adopt whatever it already had as
            // the offset. This is a late start, not a miss.
            None => {
                let offset = actual_len.saturating_sub(seen);
                self.offsets.insert(execution_id, offset);
                // A roller that somehow saw MORE than the chain holds is not a
                // late start — that is incoherent, so fail closed.
                if seen > actual_len {
                    DIVERGENCES.fetch_add(1, Ordering::Relaxed);
                    self.forget(execution_id);
                    return false;
                }
                true
            }
        }
    }

    /// ⚠ THE PRECONDITION CHECK, by chain HEAD rather than by count.
    ///
    /// Call after every real rebuild with the head `event_id` it actually saw.
    /// Compares against the highest id this process observed:
    ///
    /// * `observed >= actual_head` — consistent. (Strictly greater is benign:
    ///   an event landed between the rebuild's read and this call, so the next
    ///   decision refolds anyway because the digest advanced.)
    /// * `observed < actual_head` — this process has not seen the newest
    ///   event(s). On the FIRST call for an execution that is a late start
    ///   (flag flipped mid-flight, process restart), so it calibrates. After
    ///   that it is a real miss: [`divergences`] increments and the skip fails
    ///   closed GLOBALLY.
    ///
    /// Head rather than count because the orchestrator's rebuild has an
    /// incremental branch that never learns the full chain length, and a
    /// guard that cannot run on the hot path is not a guard.
    pub fn reconcile_head(&mut self, execution_id: i64, actual_head: i64) -> bool {
        let observed = self.observed_head(execution_id);
        if observed >= actual_head {
            self.calibrated.insert(execution_id);
            return true;
        }
        if self.calibrated.contains(&execution_id) {
            DIVERGENCES.fetch_add(1, Ordering::Relaxed);
            self.forget(execution_id);
            return false;
        }
        // Late start: adopt and move on.
        self.calibrated.insert(execution_id);
        true
    }

    /// Has this execution's baseline been calibrated by a `reconcile` yet?
    ///
    /// Until it has, the roller's length is unanchored, so a skip could be
    /// licensed by a digest covering a prefix nobody verified. `decide_for`
    /// refuses to skip before calibration.
    pub fn is_calibrated(&self, execution_id: i64) -> bool {
        self.offsets.contains_key(&execution_id) || self.calibrated.contains(&execution_id)
    }

    /// Note that a skip was taken, for the revalidation budget. Called by the
    /// wiring immediately after it acts on a [`FoldDecision::SkipValid`].
    pub fn note_skip(&mut self, execution_id: i64) {
        *self.skips_since_fold.entry(execution_id).or_insert(0) += 1;
    }

    /// Consecutive skips taken since this execution last really folded.
    pub fn skips_since_fold(&self, execution_id: i64) -> u32 {
        self.skips_since_fold
            .get(&execution_id)
            .copied()
            .unwrap_or(0)
    }

    /// Record that a fold was performed, so the next decision can match it.
    pub fn record_fold(&mut self, execution_id: i64) {
        if let Some(c) = self.current(execution_id) {
            self.cached.insert(execution_id, c);
        }
        // A real fold just reconciled, so the blind window restarts.
        self.skips_since_fold.insert(execution_id, 0);
    }

    /// Drop an execution's state (it completed, or the cache slot was evicted).
    pub fn forget(&mut self, execution_id: i64) {
        self.forget_inner(execution_id);
    }

    fn forget_inner(&mut self, execution_id: i64) {
        self.rollers.remove(&execution_id);
        self.cached.remove(&execution_id);
        self.offsets.remove(&execution_id);
        self.skips_since_fold.remove(&execution_id);
        self.calibrated.remove(&execution_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `NOETL_CHAIN_CERT` is process-global, so tests that need it on must not
    /// run concurrently with tests that need it off.
    static FLAG: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_flag_on<T>(f: impl FnOnce() -> T) -> T {
        let _g = FLAG.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("NOETL_CHAIN_CERT", "1");
        let out = f();
        std::env::remove_var("NOETL_CHAIN_CERT");
        out
    }

    fn body(i: u32) -> Vec<u8> {
        format!("ehdb-cert-vector-{i:04}").into_bytes()
    }

    fn fill(cache: &mut CertifiedFoldCache, exec: i64, n: u32) {
        for i in 0..n {
            cache.observe(exec, &body(i));
        }
    }

    /// Observe `n` events AND calibrate, which is the state a real execution is
    /// in after its first refold. Most tests want this; the ones that care
    /// about calibration itself call `fill` and `reconcile` separately.
    fn fill_calibrated(cache: &mut CertifiedFoldCache, exec: i64, n: u32) {
        fill(cache, exec, n);
        assert!(cache.reconcile(exec, n), "fixture must calibrate cleanly");
    }

    #[test]
    fn the_digest_matches_an_independent_implementation() {
        // Golden vector from a THIRD implementation (Python/hashlib) sharing no
        // code with this one or with ehdb's. `ehdb_l0::chain_cert`'s
        // `the_digest_matches_an_independent_implementation` asserts the same
        // value, so the two are pinned to the third rather than to each other.
        assert_eq!(CHUNK_EVENTS, 8, "the golden vector assumes 8");
        let mut c = CertifiedFoldCache::new();
        fill(&mut c, 1, 24);
        let cert = c.current(1).expect("3 sealed chunks");
        assert_eq!(cert.chain_len, 24);
        assert_eq!(
            hex::encode(cert.chain_digest),
            "c7d3e1fff420f51f0458bcf4f3fe5a3cbefe65aa6879d036197352d8b70a1f44",
            "this implementation has drifted from the pinned construction"
        );
    }

    #[test]
    fn the_skip_actually_fires() {
        // A9: the whole point. A cache that never skips is indistinguishable
        // from no cache, and a clean latency result from an unwired skip is the
        // failure to rule out — so assert the skip HAPPENS, not merely that
        // nothing broke.
        with_flag_on(|| {
            reset_counters();
            let mut c = CertifiedFoldCache::new();
            fill(&mut c, 1, 16);
            // Uncalibrated: must refold even though a digest exists.
            assert_eq!(c.decide_for(1), FoldDecision::RefoldUncalibrated);
            assert!(c.reconcile(1, 16), "calibrate against the real length");
            assert_eq!(c.decide_for(1), FoldDecision::RefoldNoCachedState);
            c.record_fold(1);
            // Chain unchanged -> the skip must fire.
            assert_eq!(
                c.decide_for(1),
                FoldDecision::SkipValid,
                "an unchanged chain must skip the refold"
            );
            let (observed, decided, skipped, refolded, _div) = counters();
            assert_eq!(observed, 16, "the producer side must have run");
            assert_eq!(decided, 3, "the reader side must have run");
            assert_eq!(skipped, 1, "the skip must have FIRED at least once");
            assert_eq!(refolded, 2);
        });
    }

    #[test]
    fn a_chain_that_advanced_must_refold() {
        with_flag_on(|| {
            let mut c = CertifiedFoldCache::new();
            fill_calibrated(&mut c, 1, 16);
            c.record_fold(1);
            fill(&mut c, 1, 8); // now 24
            assert_eq!(
                c.decide_for(1),
                FoldDecision::RefoldChainAdvanced,
                "new events must force a refold"
            );
        });
    }

    #[test]
    fn a_mismatched_certificate_must_not_serve_cached_state() {
        // The dangerous case: SAME length, DIFFERENT digest. A length-only or
        // count-only check would call this valid and serve state for a chain
        // that diverged. Every non-exact outcome must land on a refold.
        let same_len_diff_digest = decide(
            Some(ChainCert {
                chain_len: 8,
                chain_digest: [1u8; 32],
            }),
            Some(ChainCert {
                chain_len: 8,
                chain_digest: [2u8; 32],
            }),
        );
        assert_eq!(same_len_diff_digest, FoldDecision::RefoldDigestMismatch);
        assert!(!same_len_diff_digest.skips_fold());

        // And the full matrix — no default-allow path anywhere.
        let c8 = ChainCert {
            chain_len: 8,
            chain_digest: [1u8; 32],
        };
        let c16 = ChainCert {
            chain_len: 16,
            chain_digest: [1u8; 32],
        };
        for (cached, current, want) in [
            (None, Some(c8), FoldDecision::RefoldNoCachedState),
            (Some(c8), None, FoldDecision::RefoldNoCertificate),
            (None, None, FoldDecision::RefoldNoCachedState),
            (Some(c8), Some(c16), FoldDecision::RefoldChainAdvanced),
            (Some(c16), Some(c8), FoldDecision::RefoldCachedAhead),
            (Some(c8), Some(c8), FoldDecision::SkipValid),
        ] {
            let got = decide(cached, current);
            assert_eq!(got, want, "decide({cached:?}, {current:?})");
            assert_eq!(
                got.skips_fold(),
                want == FoldDecision::SkipValid,
                "only an exact match may skip"
            );
        }
    }

    #[test]
    fn an_absent_certificate_must_refold_rather_than_trust_nothing() {
        // A chain shorter than one chunk has no certificate at all. That must
        // refold, not skip — "no evidence" is not "evidence of no change".
        with_flag_on(|| {
            let mut c = CertifiedFoldCache::new();
            fill(&mut c, 1, CHUNK_EVENTS - 1);
            assert!(c.current(1).is_none(), "under one chunk seals nothing");
            assert!(c.reconcile(1, CHUNK_EVENTS - 1));
            c.record_fold(1);
            assert!(
                !c.decide_for(1).skips_fold(),
                "no certificate must never permit a skip"
            );
        });
    }

    #[test]
    fn the_flag_off_can_never_skip() {
        // The ramp's safety property: with the flag off there is no path to a
        // skip, whatever the cache contains.
        let _g = FLAG.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("NOETL_CHAIN_CERT");
        let mut c = CertifiedFoldCache::new();
        fill(&mut c, 1, 16);
        c.record_fold(1);
        assert!(
            !c.decide_for(1).skips_fold(),
            "flag off must never skip a refold"
        );
    }

    #[test]
    fn a_divergent_early_chunk_changes_every_later_certificate() {
        // Chunks must be CHAINED. If chunk N+1 did not absorb chunk N's digest,
        // a chain that diverged early but converged later would certify as
        // valid and the skip would serve wrong state.
        let mut clean = CertifiedFoldCache::new();
        let mut tampered = CertifiedFoldCache::new();
        let mut seen = 0;
        for i in 0..(CHUNK_EVENTS * 3) {
            clean.observe(1, &body(i));
            tampered.observe(1, &if i == 0 { body(999) } else { body(i) });
            if let (Some(a), Some(b)) = (clean.current(1), tampered.current(1)) {
                assert_eq!(a.chain_len, b.chain_len);
                assert_ne!(
                    a.chain_digest, b.chain_digest,
                    "divergence in chunk 1 must still show at chain_len {}",
                    a.chain_len
                );
                seen += 1;
            }
        }
        assert!(seen >= 3, "expected >=3 sealed chunks, saw {seen}");
    }

    #[test]
    fn executions_do_not_share_chain_state() {
        let mut c = CertifiedFoldCache::new();
        for i in 0..CHUNK_EVENTS {
            c.observe(1, &body(i));
            c.observe(2, &body(100 + i));
        }
        let a = c.current(1).expect("exec 1 sealed");
        let b = c.current(2).expect("exec 2 sealed");
        assert_ne!(a.chain_digest, b.chain_digest);
        assert_eq!(c.tracked_executions(), 2);
    }

    #[test]
    fn interleaving_does_not_change_an_executions_digest() {
        // Two executions' events arrive interleaved in ingest order. Each
        // chain's digest must depend only on ITS OWN events, or the digest
        // becomes arrival-timing dependent.
        let mut inter = CertifiedFoldCache::new();
        for i in 0..CHUNK_EVENTS {
            inter.observe(1, &body(i));
            inter.observe(2, &body(100 + i));
        }
        let mut apart = CertifiedFoldCache::new();
        fill(&mut apart, 1, CHUNK_EVENTS);
        for i in 0..CHUNK_EVENTS {
            apart.observe(2, &body(100 + i));
        }
        assert_eq!(inter.current(1), apart.current(1));
        assert_eq!(inter.current(2), apart.current(2));
    }

    #[test]
    fn a_short_chain_still_seals_at_least_one_chunk() {
        // Guards CHUNK_EVENTS: set above typical chain length, short executions
        // would certify nothing and the path would be inert.
        let mut c = CertifiedFoldCache::new();
        fill(&mut c, 1, 36);
        assert!(
            c.current(1).is_some(),
            "a 36-event chain (the corpus mean) must seal a chunk; \
             CHUNK_EVENTS={CHUNK_EVENTS} is too large"
        );
    }

    #[test]
    fn forgetting_an_execution_clears_both_halves() {
        with_flag_on(|| {
            let mut c = CertifiedFoldCache::new();
            fill_calibrated(&mut c, 1, 16);
            c.record_fold(1);
            assert!(c.decide_for(1).skips_fold());
            c.forget(1);
            assert!(c.current(1).is_none());
            assert!(c.cached(1).is_none());
            assert!(
                !c.decide_for(1).skips_fold(),
                "a forgotten execution must refold, not skip on stale state"
            );
        });
    }

    // ---------------------------------------------------------------------
    // Skip CORRECTNESS: a skip must serve exactly what a refold would.
    // ---------------------------------------------------------------------
    // Everything above proves the skip fires and that non-matches fall back.
    // Neither proves the skip is SAFE: a cache that fires correctly and serves
    // the wrong state is worse than no cache. These two tie the decision to
    // the actual fold.

    use crate::event::Event;
    use crate::state::{canonical_state_digest, WorkflowState};
    use chrono::{TimeZone, Utc};

    fn ev(execution_id: i64, i: i64, kind: &str) -> Event {
        Event {
            event_id: 1000 + i,
            execution_id,
            catalog_id: 715_207_437_183_090_702,
            event_type: kind.to_string(),
            node_name: Some("step_a".to_string()),
            status: "success".to_string(),
            context: None,
            result: None,
            meta: None,
            timestamp: Utc.timestamp_opt(1_790_000_000 + i, 0).unwrap(),
            parent_execution_id: None,
            attempt: Some(1),
        }
    }

    /// A chain long enough to seal chunks, with event kinds the fold reacts to.
    fn chain(execution_id: i64, n: i64) -> Vec<Event> {
        (0..n)
            .map(|i| {
                let kind = match i % 4 {
                    0 => "step.enter",
                    1 => "command.issued",
                    2 => "command.completed",
                    _ => "call.done",
                };
                ev(execution_id, i, kind)
            })
            .collect()
    }

    #[test]
    fn a_skip_serves_exactly_what_a_refold_would_have_produced() {
        with_flag_on(|| {
            let events = chain(1, 24);
            let mut c = CertifiedFoldCache::new();
            for e in &events {
                c.observe(1, &serde_json::to_vec(e).expect("serialise"));
            }
            assert!(c.reconcile(1, events.len() as u32), "calibrate");

            // The fold the reader would cache.
            let folded = WorkflowState::from_events(&events).expect("fold");
            let cached_digest = canonical_state_digest(&folded);
            c.record_fold(1);

            // The skip fires...
            assert_eq!(c.decide_for(1), FoldDecision::SkipValid);

            // ...and what it licenses reusing is byte-identical to a fresh
            // refold of the same chain. If this ever diverges, the skip is
            // serving state the chain does not justify.
            let refolded = WorkflowState::from_events(&events).expect("refold");
            assert_eq!(
                cached_digest,
                canonical_state_digest(&refolded),
                "a skip must serve exactly the state a refold would produce"
            );
        });
    }

    #[test]
    fn a_same_length_divergent_chain_is_caught_before_it_can_serve_wrong_state() {
        // The attack the digest exists to stop, end to end: two chains of the
        // SAME length whose folds genuinely differ. A length-only check skips
        // and serves the wrong state; the digest must refuse.
        with_flag_on(|| {
            let a = chain(1, 24);
            let mut b = chain(1, 24);
            // Diverge in the middle, keeping the length identical.
            //
            // ⚠ The first attempt here swapped in a `step.error` and the two
            // folds came out IDENTICAL — the vacuity guard below caught it. The
            // fold keys state by step, so the divergence has to be something it
            // actually keys on; a different `node_name` is.
            b[10].node_name = Some("step_DIVERGED".to_string());
            b[10].event_type = "step.enter".to_string();

            let fold_a = WorkflowState::from_events(&a).expect("fold a");
            let fold_b = WorkflowState::from_events(&b).expect("fold b");
            assert_ne!(
                canonical_state_digest(&fold_a),
                canonical_state_digest(&fold_b),
                "fixture is vacuous unless the two folds actually differ"
            );

            let mut ca = CertifiedFoldCache::new();
            for e in &a {
                ca.observe(1, &serde_json::to_vec(e).unwrap());
            }
            ca.record_fold(1);
            let cert_a = ca.cached(1).expect("sealed");

            let mut cb = CertifiedFoldCache::new();
            for e in &b {
                cb.observe(1, &serde_json::to_vec(e).unwrap());
            }
            let cert_b = cb.current(1).expect("sealed");

            assert_eq!(
                cert_a.chain_len, cert_b.chain_len,
                "the point of this test is EQUAL lengths"
            );
            assert_eq!(
                decide(Some(cert_a), Some(cert_b)),
                FoldDecision::RefoldDigestMismatch,
                "a same-length divergent chain MUST refold, not skip"
            );
        });
    }

    #[test]
    fn a_roller_that_missed_events_is_detected_and_stops_skipping() {
        // The precondition guard. Simulate a process that missed events: the
        // roller absorbed 16 but a refold reads 24. That must be DETECTED,
        // counted, and must fail closed — otherwise the skip serves state for
        // a chain this process cannot see all of.
        with_flag_on(|| {
            reset_counters();
            let mut c = CertifiedFoldCache::new();
            fill_calibrated(&mut c, 1, 16);
            c.record_fold(1);
            assert!(
                c.decide_for(1).skips_fold(),
                "sanity: it would have skipped"
            );

            // Now 8 events arrive that this process never saw, and the next
            // refold reads 24. Already calibrated, so this is a REAL miss.
            assert!(
                !c.reconcile(1, 24),
                "a roller 8 events behind the real chain must be reported inconsistent"
            );
            assert_eq!(divergences(), 1, "the divergence must be COUNTED");

            // Fails closed globally, including for an execution that looks fine.
            fill_calibrated(&mut c, 2, 16);
            c.record_fold(2);
            assert_eq!(
                c.decide_for(2),
                FoldDecision::RefoldDivergenceDetected,
                "after any divergence no execution may skip"
            );
            reset_counters();
        });
    }

    #[test]
    fn a_consistent_roller_reconciles_without_a_divergence() {
        // Positive control: the guard must be capable of PASSING, or it is just
        // an off switch.
        with_flag_on(|| {
            reset_counters();
            let mut c = CertifiedFoldCache::new();
            fill(&mut c, 1, 24);
            assert!(c.reconcile(1, 24), "a complete roller must reconcile");
            assert_eq!(divergences(), 0);
            c.record_fold(1);
            assert!(
                c.decide_for(1).skips_fold(),
                "a reconciled roller must still be allowed to skip"
            );
            reset_counters();
        });
    }

    #[test]
    fn a_run_of_skips_is_bounded_so_the_detector_can_fire() {
        // The flaw this closes: `reconcile` only learns the real chain length
        // from a fold, so a process that stopped seeing events has a roller that
        // stops advancing, reads "nothing changed", skips forever, and never
        // folds — so the miss is never detected. Detection needed the very fold
        // the skip removed. The budget forces one.
        with_flag_on(|| {
            let mut c = CertifiedFoldCache::new();
            fill_calibrated(&mut c, 1, 16);
            c.record_fold(1);
            for i in 0..MAX_CONSECUTIVE_SKIPS {
                assert_eq!(
                    c.decide_for(1),
                    FoldDecision::SkipValid,
                    "skip {i} of the budget must still be allowed"
                );
                c.note_skip(1);
            }
            assert_eq!(
                c.decide_for(1),
                FoldDecision::RefoldRevalidationDue,
                "past the budget the certificate still matches, but a fold must be                  forced so the precondition check runs"
            );
            // A real fold restarts the window.
            c.record_fold(1);
            assert_eq!(c.skips_since_fold(1), 0);
            assert_eq!(c.decide_for(1), FoldDecision::SkipValid);
        });
    }

    #[test]
    fn the_budget_still_eliminates_most_folds() {
        // The budget costs folds, so pin what it costs: at most 1 fold per
        // (budget + 1) decisions, which must stay above the spec's >=90% gate.
        let kept = 1.0 / (MAX_CONSECUTIVE_SKIPS as f64 + 1.0);
        let eliminated = (1.0 - kept) * 100.0;
        assert!(
            eliminated >= 90.0,
            "MAX_CONSECUTIVE_SKIPS={MAX_CONSECUTIVE_SKIPS} eliminates only              {eliminated:.1}% of folds, below the >=90% gate"
        );
    }

    #[test]
    fn the_cache_is_bounded_so_it_cannot_leak() {
        // Holds the serialisation lock and resets the counters: `observe`
        // increments the process-global OBSERVED, which would otherwise break
        // the tests that assert exact counter values.
        let _g = FLAG.lock().unwrap_or_else(|e| e.into_inner());
        // `forget` had no callers, so with the flag on every execution the
        // process ever saw kept state forever — a leak in a server that runs for
        // days. Prove the bound holds under more executions than it allows.
        let mut c = CertifiedFoldCache::new();
        for exec in 0..(MAX_TRACKED_EXECUTIONS as i64 + 500) {
            c.observe(exec, &body(0));
        }
        assert!(
            c.tracked_executions() <= MAX_TRACKED_EXECUTIONS,
            "tracked {} executions, bound is {MAX_TRACKED_EXECUTIONS}",
            c.tracked_executions()
        );
        // Oldest-first: the earliest executions are the ones dropped.
        assert!(
            c.current(0).is_none(),
            "execution 0 should have been evicted first"
        );
        let newest = MAX_TRACKED_EXECUTIONS as i64 + 499;
        assert!(
            c.observed_len(newest) > 0,
            "the newest execution must still be tracked"
        );
        reset_counters();
    }

    #[test]
    fn eviction_is_reported_so_a_parallel_cache_can_follow() {
        // Holds the serialisation lock and resets the counters: `observe`
        // increments the process-global OBSERVED, which would otherwise break
        // the tests that assert exact counter values.
        let _g = FLAG.lock().unwrap_or_else(|e| e.into_inner());
        // The server keeps a rebuild cache alongside this one. If evictions are
        // not reported, bounding this cache leaves that one — the one holding a
        // whole WorkflowState per execution — growing unbounded anyway.
        let mut c = CertifiedFoldCache::new();
        for exec in 0..(MAX_TRACKED_EXECUTIONS as i64 + 10) {
            c.observe(exec, &body(0));
        }
        let evicted = c.take_evicted();
        assert!(
            !evicted.is_empty(),
            "evictions must be reported for the parallel cache to mirror"
        );
        assert!(
            evicted.contains(&0),
            "the oldest execution must appear in the eviction report"
        );
        assert!(
            c.take_evicted().is_empty(),
            "take_evicted must drain, or the caller re-drops the same ids forever"
        );
        reset_counters();
    }

    #[test]
    fn an_evicted_execution_refolds_rather_than_skipping() {
        // Eviction must be SAFE, which is what licenses the simple policy: a
        // dropped entry costs one real fold, never a wrong answer.
        with_flag_on(|| {
            let mut c = CertifiedFoldCache::new();
            fill_calibrated(&mut c, 7, 16);
            c.record_fold(7);
            assert!(c.decide_for(7).skips_fold());
            c.forget(7);
            assert!(
                !c.decide_for(7).skips_fold(),
                "an evicted execution must refold, not skip on state that is gone"
            );
        });
    }

    #[test]
    fn the_certificate_is_44_bytes_with_the_execution_id() {
        assert_eq!(ChainCert::WIRE_BYTES + 8, 44);
    }
}
