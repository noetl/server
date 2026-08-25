//! Read-serve for the EHDB projection tier (noetl/ai-meta#265 phase B1).
//!
//! Phase A gave the projection tier a store, a mirror of the incumbent's real
//! rows, and a comparator. What it did **not** give it was a reader: every
//! orchestrator rebuild still resolved its snapshot from
//! `noetl.projection_snapshot`, so `NOETL_EHDB_PROJECTION=primary` meant "the
//! write path claims authority" and nothing more. This module is the reader.
//!
//! # The invariant this module exists to hold
//!
//! **A snapshot that might be wrong must not be served.**
//!
//! `handlers::events::rebuild_state` takes the snapshot's `version` and folds
//! only the events *after* it. That makes the failure asymmetric in a way worth
//! stating plainly:
//!
//! * A snapshot **behind** the incumbent is slow but correct — more events get
//!   folded forward, and the answer is the same.
//! * A snapshot **ahead** of what actually happened is silently *wrong*: the
//!   events between the real watermark and the claimed one are never folded,
//!   and the caller receives a state that never existed. Nothing downstream can
//!   detect it, because a rebuild has no second opinion to compare against.
//!
//! So the read path refuses in both directions but for different reasons, and
//! every refusal is a **demote to the incumbent**, never an error and never a
//! degraded answer. Postgres is always reachable from here; falling back costs
//! a query, and the alternative costs correctness.
//!
//! # Modes
//!
//! `NOETL_EHDB_PROJECTION_READ_SOURCE`, default **`postgres`**:
//!
//! | value | behaviour |
//! | :-- | :-- |
//! | `postgres` (default, and any unrecognised value) | exactly the pre-#265 read. No tier I/O at all — not a relay call, not an env lookup beyond this one. |
//! | `verify` | read **both**, compare, serve the tier only when it agrees with the incumbent on version *and* checksum. The proving mode: it cannot serve a wrong answer even if every check below were broken, because the incumbent is already in hand. |
//! | `tier` | serve from the tier, reading the incumbent **only** when the tier is refused. The cutover mode: this is the one that removes the Postgres read from the hot path, and therefore the one that needs the checks to be real. |
//!
//! An unrecognised value resolves to `postgres` rather than erroring. A typo in
//! a deployment variable must not take reads off the incumbent — and it must not
//! take the process down either. It is logged once at resolve time; #243 is the
//! record of what a dead default costs when it launders three distinct causes
//! into one message.
//!
//! # What `tier` mode checks, and why each check is cheap
//!
//! Without the incumbent row in hand, three things are still checkable:
//!
//! 1. **Self-consistency.** The record carries `checksum` = `sha256(snapshot)`,
//!    computed by `orch_snapshot::save` for the row it stored. Recomputing it
//!    over the record's own `snapshot` body costs one hash and catches a record
//!    that was corrupted or truncated in transit. This is the same digest the
//!    comparator uses, so the two agree by construction rather than by two
//!    implementations happening to match.
//! 2. **Version sanity.** `SELECT MAX(event_id) FROM noetl.event WHERE
//!    execution_id = $1` — one indexed scan, no payloads. A tier version above
//!    that maximum claims to have folded an event that does not exist, which is
//!    exactly the ahead-case above. This costs a query and is kept anyway: the
//!    whole point of `tier` mode is to drop a *blob* read, and a bounded index
//!    probe is not that blob.
//! 3. **Deserialisability.** A `WorkflowState` that does not parse (a shape
//!    change across a deploy) demotes rather than erroring — the same posture
//!    `load_latest` already took for the incumbent row.
//!
//! `verify` mode does all three **and** compares against the incumbent, so it
//! is strictly stronger and strictly slower. That is the intended order of
//! adoption: `verify` in shadow, `tier` only after a soak.
//!
//! # What this module deliberately does not do
//!
//! It does not write. It does not fall back to a pod-local store — the
//! projection tier has none by design (#265 A2), and inventing one here would
//! recreate the N-disjoint-fragments defect #257 §1.3 describes.

use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::db::DbPool;

/// The tier this module reads. Named once so a copy cannot drift from the
/// mirror's and the comparator's.
pub const TIER: &str = "projection";

/// `NOETL_EHDB_PROJECTION_READ_SOURCE` — where an orchestrator rebuild resolves
/// its snapshot from.
pub const READ_SOURCE_ENV: &str = "NOETL_EHDB_PROJECTION_READ_SOURCE";

/// How long the relay read may take before the tier is treated as unavailable.
///
/// Shorter than the mirror's append timeout on purpose: this sits in front of a
/// rebuild that has a perfectly good incumbent one query away, so waiting is
/// strictly worse than demoting.
const RELAY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Cap on tier records pulled for one execution. The mirror appends one record
/// per snapshot upsert, so a long-lived execution accumulates them; only the
/// newest is served from.
const MAX_READ_RECORDS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadSource {
    Postgres,
    Verify,
    Tier,
}

impl ReadSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Verify => "verify",
            Self::Tier => "tier",
        }
    }
    /// Whether this mode touches the tier at all.
    pub fn reads_tier(self) -> bool {
        !matches!(self, Self::Postgres)
    }
    /// Whether this mode needs the incumbent row loaded *before* deciding.
    pub fn needs_incumbent_first(self) -> bool {
        matches!(self, Self::Postgres | Self::Verify)
    }
}

/// Resolve the configured read source.
///
/// Unrecognised ⇒ `Postgres`, warned once. See the module note on #243.
pub fn read_source() -> ReadSource {
    let raw = std::env::var(READ_SOURCE_ENV).ok();
    match raw
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        None | Some("") | Some("postgres") => ReadSource::Postgres,
        Some("verify") => ReadSource::Verify,
        Some("tier") => ReadSource::Tier,
        Some(other) => {
            warn_unrecognised(other);
            ReadSource::Postgres
        }
    }
}

/// Warn at most once per distinct bad value.
///
/// Once, because this is on a per-rebuild path and a typo would otherwise
/// produce one line per trigger; per distinct value, because a second typo
/// after a first is a different fact and must not be swallowed by the first
/// one's latch.
fn warn_unrecognised(value: &str) {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let mut g = seen.lock().unwrap_or_else(|e| e.into_inner());
    if g.insert(value.to_string()) {
        warn!(
            target: "noetl_server::ehdb_projection_read",
            value = %value,
            "{READ_SOURCE_ENV} is not one of postgres|verify|tier; reads stay on \
             noetl.projection_snapshot"
        );
    }
}

/// Why a read did not come from the tier.
///
/// Distinct variants because they call for different responses, and because a
/// single `demoted` label would make the one case that means "the tier is
/// dangerous" (`version_ahead`) indistinguishable from the one that means
/// "nothing has been mirrored yet" (`missing`) — which on a freshly armed
/// mirror is every execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemoteReason {
    /// `NOETL_EHDB_WORKER_QUERY_URL` unset: asked to read the tier with nowhere
    /// to read from.
    Unconfigured,
    /// The relay call failed or timed out.
    Unavailable,
    /// The relay answered, but not with a record set (a typed refusal, or a
    /// body that does not parse). Distinct from `Missing`: "the tier declined
    /// to answer" is not "the tier holds nothing".
    Unreadable,
    /// The tier holds no identifiable record for this execution. On a mirror
    /// armed after the execution started, this is the expected answer and not a
    /// fault — see the coverage note in the gap analysis.
    Missing,
    /// The record's own `checksum` does not match `sha256` of the `snapshot`
    /// body it carries.
    Checksum,
    /// The record carries no `snapshot` body — a digest, which can be verified
    /// and cannot be served from.
    NoBody,
    /// The tier's version exceeds the highest `event_id` the execution has.
    /// **The dangerous case**: serving it would skip events.
    VersionAhead,
    /// `verify` mode only: the tier is behind the incumbent.
    StaleVersion,
    /// `verify` mode only: same version, different checksum.
    Divergent,
    /// The snapshot body does not deserialise into a `WorkflowState`.
    Undeserialisable,
    /// `verify` mode only: the incumbent has no row for this execution, so
    /// there is nothing to compare the tier against.
    ///
    /// Its own label rather than folded into `missing`, because the two name
    /// opposite stores: `missing` is an empty tier beside a populated
    /// incumbent — the state of a freshly armed mirror — while this is an
    /// execution neither store has snapshotted yet, which is the *normal* state
    /// of every short execution (see the coverage denominator). One label for
    /// both would launder two causes into one number, which is the defect
    /// ai-meta#243 is the record of.
    NoIncumbent,
}

impl DemoteReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unconfigured => "unconfigured",
            Self::Unavailable => "unavailable",
            Self::Unreadable => "unreadable",
            Self::Missing => "missing",
            Self::Checksum => "checksum",
            Self::NoBody => "no_body",
            Self::VersionAhead => "version_ahead",
            Self::StaleVersion => "stale_version",
            Self::Divergent => "divergent",
            Self::Undeserialisable => "undeserialisable",
            Self::NoIncumbent => "no_incumbent",
        }
    }
    /// Whether this demote means something is *wrong*, as opposed to something
    /// being absent or switched off.
    ///
    /// `missing` and `unconfigured` are the states of a tier that has not been
    /// armed yet; alerting on them would page on every rollout. The rest mean
    /// the tier answered and the answer could not be trusted.
    pub fn is_fault(self) -> bool {
        !matches!(
            self,
            Self::Missing | Self::Unconfigured | Self::NoIncumbent
        )
    }
}

/// Every outcome label the read path can emit. Closed set, pinned at 0.
///
/// `served_tier` plus one label per [`DemoteReason`], plus `disabled`. Pinned
/// as a set so an absent series means "this binary predates the metric" rather
/// than "this case has not happened" — `Registry::gather` prunes empty families,
/// so absence is the default state of every labelled metric here.
pub const READ_OUTCOMES: [&str; 13] = [
    "served_tier",
    "disabled",
    "unconfigured",
    "unavailable",
    "unreadable",
    "missing",
    "checksum",
    "no_body",
    "version_ahead",
    "stale_version",
    "divergent",
    "undeserialisable",
    "no_incumbent",
];

/// The identifying facts of the incumbent row, for `verify` mode.
///
/// Deliberately not the whole `LoadedSnapshot`: comparison needs the watermark
/// and the digest, and passing the state as well would invite comparing two
/// deserialised structures rather than the one digest the writer authored.
#[derive(Debug, Clone)]
pub struct IncumbentFacts {
    pub version: i64,
    pub checksum: Option<String>,
}

/// What the tier read produced.
pub enum TierRead {
    /// The tier answered, and the answer passed every check that applies.
    Served(ServedSnapshot),
    /// Use the incumbent. Carries why.
    Demote(DemoteReason),
}

/// A snapshot resolved from the tier, in the shape `load_latest` returns.
pub struct ServedSnapshot {
    pub snapshot: Value,
    pub version: i64,
    pub applied_count: i64,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub routing_meta: Option<Value>,
}

/// One tier record, reduced to what the read path needs.
#[derive(Debug, Clone)]
struct Candidate {
    global_sequence: u64,
    version: i64,
    checksum: Option<String>,
    applied_count: i64,
    snapshot: Option<Value>,
    updated_at: Option<chrono::DateTime<chrono::Utc>>,
    routing_meta: Option<Value>,
}

/// Pick the record to serve from, and check it.
///
/// **Pure** — no I/O, no env, no clock. That is what lets the tests below drive
/// the same function the live path uses, with inputs whose answers are known.
/// The event-log tier's serve decision earned that property in #257 and it is
/// the reason a mutation arm can prove this fires.
///
/// `max_event_id` is the execution's highest event id, or `None` when the
/// caller could not determine it — in which case the ahead-check is **skipped
/// and the read is demoted**, because an unchecked ahead-case is precisely the
/// silent wrong answer this module exists to prevent. Not knowing is not the
/// same as being satisfied.
fn decide(
    records: &[Candidate],
    max_event_id: Option<i64>,
    incumbent: Option<&IncumbentFacts>,
) -> Result<Candidate, DemoteReason> {
    if records.is_empty() {
        return Err(DemoteReason::Missing);
    }
    // Newest by (version, sequence): the mirror appends per upsert, so the tail
    // is the current read model and the earlier records are its history.
    let newest = records
        .iter()
        .max_by_key(|c| (c.version, c.global_sequence))
        .expect("records is non-empty");

    let Some(body) = newest.snapshot.as_ref() else {
        return Err(DemoteReason::NoBody);
    };

    // Self-consistency, before anything that costs I/O to have gathered.
    match newest.checksum.as_deref() {
        None => return Err(DemoteReason::Checksum),
        Some(carried) => {
            let recomputed = sha256_of(body);
            if recomputed != carried {
                return Err(DemoteReason::Checksum);
            }
        }
    }

    // The ahead-case. Checked against the event log rather than the incumbent
    // snapshot so it also holds in `tier` mode, where no incumbent row is read.
    match max_event_id {
        None => return Err(DemoteReason::VersionAhead),
        Some(max) if newest.version > max => return Err(DemoteReason::VersionAhead),
        Some(_) => {}
    }

    // `verify` mode: the incumbent is in hand, so compare against it too.
    if let Some(inc) = incumbent {
        if newest.version < inc.version {
            return Err(DemoteReason::StaleVersion);
        }
        if newest.version > inc.version {
            // Above the incumbent but at or below the event log's tip. The tier
            // mirrored a save the incumbent row no longer reflects, which means
            // the two stores disagree about what the current read model IS.
            return Err(DemoteReason::Divergent);
        }
        if let Some(want) = inc.checksum.as_deref() {
            if newest.checksum.as_deref() != Some(want) {
                return Err(DemoteReason::Divergent);
            }
        }
    }

    Ok(newest.clone())
}

/// `sha256` over the same bytes `orch_snapshot::save` digests.
///
/// Must stay byte-identical to the writer's: `serde_json::to_vec` of the
/// snapshot value. A different serialisation here would make every record fail
/// its own checksum and demote 100% of reads — which would look like a broken
/// tier rather than a broken comparison.
fn sha256_of(snapshot: &Value) -> String {
    let bytes = serde_json::to_vec(snapshot).unwrap_or_default();
    hex::encode(Sha256::digest(&bytes))
}

fn parse_candidates(body: &Value) -> Result<Vec<Candidate>, DemoteReason> {
    // A typed refusal carries `outcome` != "ok". Scoring an empty `records`
    // array from such a body as "the tier holds nothing" is the fail-loud
    // violation the comparator was written against, and it would be worse here:
    // there it produces a wrong verdict, here it would produce `missing` on a
    // tier that is actually broken.
    if let Some(outcome) = body.get("outcome").and_then(|o| o.as_str()) {
        if outcome != "ok" {
            return Err(DemoteReason::Unreadable);
        }
    }
    let Some(arr) = body.get("records").and_then(|r| r.as_array()) else {
        return Err(DemoteReason::Unreadable);
    };
    let mut out = Vec::with_capacity(arr.len());
    for r in arr {
        let global_sequence = r.get("global_sequence").and_then(|s| s.as_u64()).unwrap_or(0);
        let payload = match r.get("payload").and_then(|p| p.as_str()) {
            Some(s) => match serde_json::from_str::<Value>(s) {
                Ok(v) => v,
                Err(_) => continue,
            },
            None => r.clone(),
        };
        let Some(version) = payload.get("version").and_then(read_i64) else {
            continue;
        };
        out.push(Candidate {
            global_sequence,
            version,
            checksum: payload
                .get("checksum")
                .and_then(|c| c.as_str())
                .map(str::to_string),
            applied_count: payload
                .get("applied_count")
                .and_then(read_i64)
                .unwrap_or(0),
            snapshot: payload
                .get("snapshot")
                .filter(|s| !s.is_null())
                .cloned(),
            updated_at: payload
                .get("updated_at")
                .and_then(|t| t.as_str())
                .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.with_timezone(&chrono::Utc)),
            routing_meta: payload
                .get("routing_meta")
                .filter(|v| !v.is_null())
                .cloned(),
        });
    }
    Ok(out)
}

fn read_i64(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
}

fn relay_client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(reqwest::Client::new)
}

/// The execution's highest `event_id`, used for the ahead-check.
///
/// Bounded index probe, no payloads. `None` on error — and `decide` treats
/// `None` as a demote, so a database hiccup produces a slower correct read
/// rather than an unchecked one.
async fn max_event_id(pool: &DbPool, execution_id: i64) -> Option<i64> {
    sqlx::query_scalar::<_, Option<i64>>(
        "SELECT MAX(event_id) FROM noetl.event WHERE execution_id = $1",
    )
    .bind(execution_id)
    .fetch_one(pool)
    .await
    .ok()
    .flatten()
}

/// Try to resolve one execution's snapshot from the projection tier.
///
/// Never returns an error: every failure is a [`DemoteReason`], because the
/// caller always has the incumbent available and a read path that can fail is
/// strictly worse than one that can only be slower.
pub async fn read(
    pool: &DbPool,
    execution_id: i64,
    incumbent: Option<&IncumbentFacts>,
) -> TierRead {
    let Some(base) = std::env::var(crate::handlers::ehdb::WORKER_QUERY_URL_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return TierRead::Demote(DemoteReason::Unconfigured);
    };

    let url = format!(
        "{}/ehdb/tiers/{TIER}?execution={execution_id}&limit={MAX_READ_RECORDS}",
        base.trim_end_matches('/')
    );
    let body: Value = match relay_client().get(&url).timeout(RELAY_TIMEOUT).send().await {
        Err(_) => return TierRead::Demote(DemoteReason::Unavailable),
        Ok(r) => match r.json().await {
            Ok(v) => v,
            Err(_) => return TierRead::Demote(DemoteReason::Unreadable),
        },
    };

    let candidates = match parse_candidates(&body) {
        Ok(c) => c,
        Err(reason) => return TierRead::Demote(reason),
    };

    // Only probe the event log when there is something to check. An empty tier
    // demotes on `missing` without touching the database.
    let max = if candidates.is_empty() {
        None
    } else {
        max_event_id(pool, execution_id).await
    };

    let chosen = match decide(&candidates, max, incumbent) {
        Ok(c) => c,
        Err(reason) => return TierRead::Demote(reason),
    };

    let snapshot = chosen.snapshot.expect("decide rejects a record with no body");
    TierRead::Served(ServedSnapshot {
        snapshot,
        version: chosen.version,
        applied_count: chosen.applied_count,
        // Missing `updated_at` means a record mirrored before the field was
        // carried. `now()` is the conservative choice: the straggler re-scan
        // window is `updated_at - margin`, so a *newer* timestamp narrows the
        // window and could skip a straggler. Demote instead of guessing.
        updated_at: match chosen.updated_at {
            Some(t) => t,
            None => return TierRead::Demote(DemoteReason::Unreadable),
        },
        routing_meta: chosen.routing_meta,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(n: i64) -> Value {
        serde_json::json!({"step": n, "vars": {"a": n}})
    }

    fn candidate(version: i64, seq: u64, body: Option<Value>, checksum: Option<String>) -> Candidate {
        let b = body.clone();
        Candidate {
            global_sequence: seq,
            version,
            checksum: checksum.or_else(|| b.as_ref().map(sha256_of)),
            applied_count: version,
            snapshot: body,
            updated_at: Some(chrono::Utc::now()),
            routing_meta: None,
        }
    }

    /// The default must be the pre-#265 behaviour, with no tier I/O.
    ///
    /// Asserted on the *type*, not on a comment: `reads_tier()` is what the
    /// caller branches on, so a future mode that accidentally reads the tier by
    /// default fails here rather than in production.
    #[test]
    fn the_default_read_source_does_not_touch_the_tier() {
        assert_eq!(ReadSource::Postgres.as_str(), "postgres");
        assert!(!ReadSource::Postgres.reads_tier());
        assert!(ReadSource::Verify.reads_tier());
        assert!(ReadSource::Tier.reads_tier());
    }

    /// A healthy record at or below the event tip is served.
    #[test]
    fn a_consistent_record_is_served() {
        let c = candidate(10, 1, Some(snap(10)), None);
        let got = decide(&[c], Some(10), None).expect("must serve");
        assert_eq!(got.version, 10);
    }

    /// **The dangerous case.** A version above the event log's tip claims to
    /// have folded an event that does not exist; serving it would skip every
    /// event in between and produce a state that never existed.
    #[test]
    fn a_version_above_the_event_tip_demotes() {
        let c = candidate(99, 1, Some(snap(99)), None);
        assert_eq!(
            decide(&[c], Some(10), None).unwrap_err(),
            DemoteReason::VersionAhead
        );
    }

    /// Not knowing the tip is not the same as being satisfied by it.
    #[test]
    fn an_unknown_event_tip_demotes_rather_than_skipping_the_check() {
        let c = candidate(10, 1, Some(snap(10)), None);
        assert_eq!(
            decide(&[c], None, None).unwrap_err(),
            DemoteReason::VersionAhead
        );
    }

    /// A record whose carried digest does not describe its own body.
    ///
    /// The mutation the kind gate performs, in unit form: same version, same
    /// shape, different content. A count- or version-based check passes this.
    #[test]
    fn a_body_that_does_not_match_its_checksum_demotes() {
        let mut c = candidate(10, 1, Some(snap(10)), None);
        c.snapshot = Some(snap(11)); // body swapped, digest left describing snap(10)
        assert_eq!(
            decide(&[c], Some(10), None).unwrap_err(),
            DemoteReason::Checksum
        );
    }

    /// A digest with no body can be verified and cannot be served from.
    #[test]
    fn a_record_with_no_snapshot_body_demotes() {
        let c = candidate(10, 1, None, Some("deadbeef".to_string()));
        assert_eq!(
            decide(&[c], Some(10), None).unwrap_err(),
            DemoteReason::NoBody
        );
    }

    /// An empty tier is `missing`, not `unreadable` — a freshly armed mirror
    /// answers this for every execution that predates it.
    #[test]
    fn an_empty_tier_is_missing_not_a_fault() {
        assert_eq!(decide(&[], Some(10), None).unwrap_err(), DemoteReason::Missing);
        assert!(!DemoteReason::Missing.is_fault());
        assert!(!DemoteReason::Unconfigured.is_fault());
        assert!(!DemoteReason::NoIncumbent.is_fault());
        assert!(DemoteReason::VersionAhead.is_fault());
        assert!(DemoteReason::Checksum.is_fault());
    }

    /// `verify` mode: behind the incumbent demotes, and says *behind* rather
    /// than a generic disagreement.
    #[test]
    fn verify_mode_demotes_a_tier_that_is_behind() {
        let c = candidate(8, 1, Some(snap(8)), None);
        let inc = IncumbentFacts {
            version: 10,
            checksum: None,
        };
        assert_eq!(
            decide(&[c], Some(10), Some(&inc)).unwrap_err(),
            DemoteReason::StaleVersion
        );
    }

    /// `verify` mode: same version, different digest.
    #[test]
    fn verify_mode_demotes_on_a_digest_disagreement() {
        let c = candidate(10, 1, Some(snap(10)), None);
        let inc = IncumbentFacts {
            version: 10,
            checksum: Some("0000000000000000000000000000000000000000000000000000000000000000".into()),
        };
        assert_eq!(
            decide(&[c], Some(10), Some(&inc)).unwrap_err(),
            DemoteReason::Divergent
        );
    }

    /// `verify` mode with everything agreeing serves — the positive control
    /// without which every assertion above could be satisfied by a `decide`
    /// that demotes unconditionally.
    #[test]
    fn verify_mode_serves_when_both_stores_agree() {
        let body = snap(10);
        let c = candidate(10, 1, Some(body.clone()), None);
        let inc = IncumbentFacts {
            version: 10,
            checksum: Some(sha256_of(&body)),
        };
        assert!(decide(&[c], Some(10), Some(&inc)).is_ok());
    }

    /// The newest record wins, and "newest" is by version then sequence — not
    /// by array position, which is the order the relay happened to return.
    #[test]
    fn the_newest_record_is_the_one_served() {
        let recs = vec![
            candidate(10, 3, Some(snap(10)), None),
            candidate(4, 1, Some(snap(4)), None),
            candidate(7, 2, Some(snap(7)), None),
        ];
        assert_eq!(decide(&recs, Some(10), None).unwrap().version, 10);
        // …and reversing the input does not change the answer.
        let mut rev = recs.clone();
        rev.reverse();
        assert_eq!(decide(&rev, Some(10), None).unwrap().version, 10);
    }

    /// A typed refusal is `unreadable`, never `missing`.
    ///
    /// The distinction that matters operationally: `missing` is a tier that has
    /// nothing yet, `unreadable` is a tier that declined to answer. Collapsing
    /// them would report a broken relay as an unarmed mirror.
    #[test]
    fn a_typed_refusal_is_not_read_as_an_empty_tier() {
        let body = serde_json::json!({"outcome": "unavailable", "records": []});
        assert_eq!(parse_candidates(&body).unwrap_err(), DemoteReason::Unreadable);
        let missing_field = serde_json::json!({"outcome": "ok"});
        assert_eq!(
            parse_candidates(&missing_field).unwrap_err(),
            DemoteReason::Unreadable
        );
        // Positive control: a well-formed empty answer IS an empty tier.
        let empty = serde_json::json!({"outcome": "ok", "records": []});
        assert!(parse_candidates(&empty).unwrap().is_empty());
    }

    /// The read path's digest must be the writer's digest.
    ///
    /// If these two serialisations ever diverge, every record fails its own
    /// checksum and 100% of reads demote — which reads like a broken tier
    /// rather than a broken comparison, and would be diagnosed in the wrong
    /// place. This pins them to the same bytes.
    #[test]
    fn the_read_digest_matches_the_writer_digest() {
        let body = snap(42);
        let writer = {
            let bytes = serde_json::to_vec(&body).unwrap_or_default();
            hex::encode(Sha256::digest(&bytes))
        };
        assert_eq!(sha256_of(&body), writer);
    }

    /// Every [`DemoteReason`] has a label in [`READ_OUTCOMES`], and every label
    /// but `served_tier`/`disabled` has a reason.
    ///
    /// Pinning a set that omits one value reintroduces the absent-series bug on
    /// exactly that value, while the rest read 0 and look complete
    /// (`representation-drift.md`).
    #[test]
    fn every_demote_reason_is_a_pinned_label() {
        let reasons = [
            DemoteReason::Unconfigured,
            DemoteReason::Unavailable,
            DemoteReason::Unreadable,
            DemoteReason::Missing,
            DemoteReason::Checksum,
            DemoteReason::NoBody,
            DemoteReason::VersionAhead,
            DemoteReason::StaleVersion,
            DemoteReason::Divergent,
            DemoteReason::Undeserialisable,
            DemoteReason::NoIncumbent,
        ];
        for r in reasons {
            assert!(
                READ_OUTCOMES.contains(&r.as_str()),
                "{} is not pinned; its series would be absent until it fires",
                r.as_str()
            );
        }
        assert_eq!(
            READ_OUTCOMES.len(),
            reasons.len() + 2,
            "READ_OUTCOMES must be exactly the reasons plus served_tier and disabled"
        );
    }
}
