//! Cross-store parity: does the EHDB projection tier agree with
//! `noetl.projection_snapshot`?
//!
//! **[ai-meta#265](https://github.com/noetl/ai-meta/issues/265) A4.** The
//! projection-tier twin of [`super::ehdb_parity`], and the evidence any flip of
//! this tier has to be gated on.
//!
//! # The gap this closes
//!
//! `worker/src/ehdb/projection.rs` already runs a parity check, and it is not
//! this one. It folds a window of events into the EHDB engine and compares the
//! result against `fold_window_authoritative` — **a second fold the worker
//! computed itself**. That is EHDB compared to a re-implementation of EHDB's own
//! logic, which is precisely the insufficiency
//! [#258](https://github.com/noetl/ai-meta/issues/258) ruled out for the event
//! log. It never reads `noetl.projection_snapshot`, and per
//! [`data-access-boundary.md`][dab] the worker cannot.
//!
//! So the projection tier's shadow signal today attests that the engine folds
//! consistently. It carries no evidence that what the tier holds matches the
//! read model the platform actually serves. This module supplies that half.
//!
//! [dab]: https://github.com/noetl/ai-meta/blob/main/agents/rules/data-access-boundary.md
//!
//! # Why this lives in the server
//!
//! Same reason as the event log's: the server is the only component entitled to
//! read `noetl.*`. Its control-plane guard is preserved — it does not open tier
//! storage, it fetches the EHDB side through the relay that already exists
//! (`GET {NOETL_EHDB_WORKER_QUERY_URL}/ehdb/tiers/projection?execution=…`). No
//! new access, only a comparison across two reads the server was already making.
//!
//! # What is compared
//!
//! The incumbent row is an **upsert**: one row per execution, carrying the
//! latest `version`. The tier is **append-only**: one record per upsert, so it
//! accumulates every revision. The comparison is between "the incumbent's
//! current row" and "the tier's newest record", and the asymmetry is what makes
//! `stale_version` a distinct and useful verdict.
//!
//! | axis | check |
//! | :-- | :-- |
//! | presence | an authoritative row exists ⇒ the tier holds ≥1 record for that execution |
//! | currency | the tier's highest `version` equals the authoritative `version` |
//! | content | at that version, `checksum` and `applied_count` agree |
//! | monotonicity | the tier's records, in `global_sequence` order, carry non-decreasing `version`s |
//! | identifiability | every tier record parses and names a `version` |
//!
//! **Content parity is a digest the incumbent authored.** `orch_snapshot::save`
//! computes `sha256(snapshot)` for the row it stores and the mirror carries it
//! verbatim, so this compares two copies of one value rather than two
//! derivations of one input. The event-log comparator cannot do this — the
//! server rewrites and sanitises event bodies, so byte-identity there is
//! *defined* to fail.
//!
//! # The backlog, excluded visibly
//!
//! Measured on the kind cluster: **3,320 of 3,344** snapshot rows belong to
//! executions with no surviving events. `projection_snapshot` is an upsert with
//! no GC while `noetl.event` is trimmed, so the table accumulates far beyond the
//! live log.
//!
//! A presence check over the whole table would therefore report ~3,300 missing
//! rows the day the mirror arms, and a real divergence would be one line in that
//! flood. The sampler windows its candidates by `updated_at` instead, and every
//! report carries [`ComparisonResult::snapshot_age_seconds`] so a reader can
//! tell "the mirror missed this" from "this predates the mirror" without
//! guessing. Scoping something away silently would make the comparator's own
//! coverage unmeasurable, which is the same defect one level up.
//!
//! # Why the controls ship in the binary
//!
//! A comparator that cannot detect divergence reports zero divergence, and so
//! does a healthy platform. [`run_controls`] drives synthetic inputs — one clean
//! pair, and one deliberately corrupted pair per divergence kind — through the
//! **same** [`compare_cross_store`] the live path uses.
//!
//! That makes a zero readable:
//! `..._control_total{control=…,result="unexpected"} == 0` together with
//! `result="expected" > 0` says the comparator ran and discriminated.

use std::time::Duration;

use axum::{
    extract::{Path, Query, State},
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::warn;

use crate::state::AppState;

/// The tier name, on every metric label and on the relay path.
pub const TIER: &str = "projection";

/// The aggregate the orchestrator read model is stored under.
const AGGREGATE_TYPE: &str = "orchestrator_workflow_state";

/// Largest number of tier records one comparison will read back.
///
/// The tier accumulates one record per snapshot upsert, and §1.5 of the design
/// note measured that as roughly one per orchestrator trigger — so a long
/// execution has many. Bounded like every other EHDB read; a comparison that hit
/// the cap says so rather than scoring a truncated set.
pub const MAX_COMPARE_RECORDS: usize = 500;

const RELAY_TIMEOUT: Duration = Duration::from_secs(10);

/// The incumbent's current row for one execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuthoritativeSnapshot {
    pub execution_id: i64,
    /// Highest `event_id` folded in — the snapshot watermark.
    pub version: i64,
    /// `sha256(snapshot)`, computed by `orch_snapshot::save`.
    pub checksum: String,
    /// `meta.applied_count` — events folded in.
    pub applied_count: i64,
    /// Seconds since the row was last written. Reported, never used to score:
    /// see the module note on the backlog.
    pub age_seconds: i64,
}

/// One record read back from the tier, before parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirroredRecord {
    pub global_sequence: u64,
    pub payload: String,
}

/// A parsed tier record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirroredSnapshot {
    pub global_sequence: u64,
    pub version: i64,
    pub checksum: Option<String>,
    pub applied_count: Option<i64>,
    /// Whether the record carried the read model itself, not just its digest. A
    /// tier of digests can be verified and cannot be served from, and the
    /// difference must be visible before anyone promotes it.
    pub has_snapshot_body: bool,
}

/// Every way the two stores can disagree.
///
/// Distinct variants because they call for different operator responses:
/// `stale_version` is a mirror falling behind (usually a relay problem),
/// `checksum` is the tier holding different *content* at the same revision
/// (a correctness problem), and `ahead_version` should be impossible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DivergenceKind {
    /// The incumbent has a row; the tier holds nothing for this execution.
    MissingExecution,
    /// The tier holds records, but its newest `version` is BELOW the
    /// incumbent's — the mirror is behind.
    StaleVersion,
    /// The tier's newest `version` is ABOVE the incumbent's. Should be
    /// unreachable: the mirror runs *after* the upsert commits, so the tier can
    /// only be ahead if the incumbent moved backwards.
    AheadVersion,
    /// Same version, different `checksum`. The tier holds different content for
    /// the revision it claims.
    Checksum,
    /// Same version, different `applied_count`.
    AppliedCount,
    /// The tier's versions do not ascend with `global_sequence` — a record was
    /// lost, duplicated, or written out of order.
    Order,
    /// A tier record that does not parse, or carries no `version`. It cannot be
    /// compared, which is not the same as agreeing.
    Unidentified,
    /// A record whose `version` is the incumbent's but carries only a digest.
    /// Not a content divergence — a *serveability* one.
    MissingBody,
}

impl DivergenceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::MissingExecution => "missing_execution",
            Self::StaleVersion => "stale_version",
            Self::AheadVersion => "ahead_version",
            Self::Checksum => "checksum",
            Self::AppliedCount => "applied_count",
            Self::Order => "order",
            Self::Unidentified => "unidentified",
            Self::MissingBody => "missing_body",
        }
    }
}

/// Every kind, for pinning. Order is the report's order.
pub const DIVERGENCE_KINDS: [DivergenceKind; 8] = [
    DivergenceKind::MissingExecution,
    DivergenceKind::StaleVersion,
    DivergenceKind::AheadVersion,
    DivergenceKind::Checksum,
    DivergenceKind::AppliedCount,
    DivergenceKind::Order,
    DivergenceKind::Unidentified,
    DivergenceKind::MissingBody,
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Divergence {
    pub kind: DivergenceKind,
    pub detail: String,
}

/// The verdict for one execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CrossStoreReport {
    pub execution_id: i64,
    /// The incumbent's current version.
    pub authoritative_version: i64,
    /// The tier's newest version, when it holds any parseable record.
    pub tier_version: Option<i64>,
    /// Tier records read back.
    pub tier_records: usize,
    /// Whether every axis agreed.
    pub holds: bool,
    pub divergences: Vec<Divergence>,
}

impl CrossStoreReport {
    pub fn kinds(&self) -> Vec<&'static str> {
        let mut k: Vec<&'static str> = self.divergences.iter().map(|d| d.kind.as_str()).collect();
        k.sort_unstable();
        k.dedup();
        k
    }
}

fn read_i64(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.parse::<i64>().ok(),
        _ => None,
    }
}

/// Parse one tier record. `None` when it carries no usable `version`.
fn parse_record(rec: &MirroredRecord) -> Option<MirroredSnapshot> {
    let v: serde_json::Value = serde_json::from_str(&rec.payload).ok()?;
    let version = v.get("version").and_then(read_i64)?;
    Some(MirroredSnapshot {
        global_sequence: rec.global_sequence,
        version,
        checksum: v
            .get("checksum")
            .and_then(|c| c.as_str())
            .map(str::to_string),
        applied_count: v.get("applied_count").and_then(read_i64),
        has_snapshot_body: v
            .get("snapshot")
            .is_some_and(|s| !s.is_null() && (s.is_object() || s.is_array())),
    })
}

/// Compare one execution's incumbent row against the tier's records.
///
/// Pure — no I/O, no env, no clock. That is what lets [`run_controls`] drive the
/// **same** function the live path uses with inputs whose answers are known.
pub fn compare_cross_store(
    auth: &AuthoritativeSnapshot,
    records: &[MirroredRecord],
) -> CrossStoreReport {
    let mut divergences: Vec<Divergence> = Vec::new();

    // ---- identifiability -------------------------------------------------
    let mut parsed: Vec<MirroredSnapshot> = Vec::with_capacity(records.len());
    let mut unparsed = 0usize;
    for r in records {
        match parse_record(r) {
            Some(p) => parsed.push(p),
            None => unparsed += 1,
        }
    }
    if unparsed > 0 {
        divergences.push(Divergence {
            kind: DivergenceKind::Unidentified,
            detail: format!(
                "{unparsed} of {} tier record(s) do not parse or carry no `version`; they \
                 cannot be compared, which is not the same as agreeing",
                records.len()
            ),
        });
    }

    // ---- presence --------------------------------------------------------
    if parsed.is_empty() {
        divergences.push(Divergence {
            kind: DivergenceKind::MissingExecution,
            detail: format!(
                "noetl.projection_snapshot holds execution {} at version {} but the \
                 projection tier holds no identifiable record for it",
                auth.execution_id, auth.version
            ),
        });
        return CrossStoreReport {
            execution_id: auth.execution_id,
            authoritative_version: auth.version,
            tier_version: None,
            tier_records: records.len(),
            holds: false,
            divergences,
        };
    }

    // ---- monotonicity ----------------------------------------------------
    // Read in the order the store assigned, versions must not go backwards.
    // Checked BEFORE currency: a rewind explains a stale tip, and reporting only
    // the tip would send an operator looking at the relay instead of the store.
    let mut by_seq = parsed.clone();
    by_seq.sort_by_key(|p| p.global_sequence);
    for w in by_seq.windows(2) {
        if w[1].version < w[0].version {
            divergences.push(Divergence {
                kind: DivergenceKind::Order,
                detail: format!(
                    "tier versions descend: sequence {} carries version {} after sequence {} \
                     carried {}",
                    w[1].global_sequence, w[1].version, w[0].global_sequence, w[0].version
                ),
            });
            break;
        }
    }

    // ---- currency --------------------------------------------------------
    let newest = by_seq
        .iter()
        .max_by_key(|p| (p.version, p.global_sequence))
        .expect("parsed is non-empty");
    let tier_version = newest.version;
    if tier_version < auth.version {
        divergences.push(Divergence {
            kind: DivergenceKind::StaleVersion,
            detail: format!(
                "tier's newest version {tier_version} is behind the incumbent's {} \
                 (execution {})",
                auth.version, auth.execution_id
            ),
        });
    } else if tier_version > auth.version {
        divergences.push(Divergence {
            kind: DivergenceKind::AheadVersion,
            detail: format!(
                "tier's newest version {tier_version} is AHEAD of the incumbent's {}. The \
                 mirror runs after the upsert commits, so this should be unreachable — \
                 either the incumbent moved backwards or something other than the mirror \
                 wrote the tier (execution {})",
                auth.version, auth.execution_id
            ),
        });
    }

    // ---- content, at the incumbent's version ------------------------------
    // Scoped to the matching revision on purpose: comparing the incumbent's
    // checksum against a record for a DIFFERENT version would report a content
    // divergence for what is only a stale mirror, and the operator would go
    // looking for corruption that is not there.
    if let Some(at_version) = by_seq.iter().find(|p| p.version == auth.version) {
        match at_version.checksum.as_deref() {
            Some(c) if c != auth.checksum => divergences.push(Divergence {
                kind: DivergenceKind::Checksum,
                detail: format!(
                    "at version {}: tier checksum {} != incumbent {} — the tier holds \
                     different content for the revision it claims (execution {})",
                    auth.version,
                    truncate(c, 16),
                    truncate(&auth.checksum, 16),
                    auth.execution_id
                ),
            }),
            Some(_) => {}
            None => divergences.push(Divergence {
                kind: DivergenceKind::Unidentified,
                detail: format!(
                    "the tier record at version {} carries no checksum, so content parity \
                     cannot be evaluated (execution {})",
                    auth.version, auth.execution_id
                ),
            }),
        }
        if let Some(ac) = at_version.applied_count {
            if ac != auth.applied_count {
                divergences.push(Divergence {
                    kind: DivergenceKind::AppliedCount,
                    detail: format!(
                        "at version {}: tier applied_count {ac} != incumbent {} (execution {})",
                        auth.version, auth.applied_count, auth.execution_id
                    ),
                });
            }
        }
        if !at_version.has_snapshot_body {
            divergences.push(Divergence {
                kind: DivergenceKind::MissingBody,
                detail: format!(
                    "the tier record at version {} carries no `snapshot` body — this tier \
                     could be verified but not served from (execution {})",
                    auth.version, auth.execution_id
                ),
            });
        }
    }

    CrossStoreReport {
        execution_id: auth.execution_id,
        authoritative_version: auth.version,
        tier_version: Some(tier_version),
        tier_records: records.len(),
        holds: divergences.is_empty(),
        divergences,
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

// ===========================================================================
// Controls — so a zero is readable.
// ===========================================================================

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ControlResult {
    pub control: String,
    pub expected: bool,
    pub detail: String,
}

/// Every control label value, for pinning.
pub const CONTROL_NAMES: [&str; 9] = [
    "identical",
    "missing_execution",
    "stale_version",
    "ahead_version",
    "checksum",
    "applied_count",
    "order",
    "unidentified",
    "missing_body",
];

fn control_auth() -> AuthoritativeSnapshot {
    AuthoritativeSnapshot {
        execution_id: 1,
        version: 9_003,
        checksum: "c0ffee00c0ffee00".to_string(),
        applied_count: 12,
        age_seconds: 0,
    }
}

fn control_record(seq: u64, version: i64, checksum: &str, applied: i64) -> MirroredRecord {
    MirroredRecord {
        global_sequence: seq,
        payload: json!({
            "execution_id": 1,
            "version": version,
            "checksum": checksum,
            "applied_count": applied,
            "snapshot": {"steps": {"a": "done"}},
            "mirror_source": "server",
        })
        .to_string(),
    }
}

/// A clean synthetic pair: three revisions, the newest matching the incumbent.
fn control_fixtures() -> (AuthoritativeSnapshot, Vec<MirroredRecord>) {
    let auth = control_auth();
    let records = vec![
        control_record(1, 9_001, "aaaa", 4),
        control_record(2, 9_002, "bbbb", 8),
        control_record(3, auth.version, &auth.checksum, auth.applied_count),
    ];
    (auth, records)
}

type TierMutation = Box<dyn Fn(&mut Vec<MirroredRecord>)>;

/// Drive the control suite through [`compare_cross_store`].
///
/// The negative control asserts a clean pair reports `holds`; each positive
/// control corrupts the tier side in exactly one way and asserts the comparator
/// reports **that** kind. A control coming back `expected: false` means every
/// zero this comparator has published is void.
pub fn run_controls() -> Vec<ControlResult> {
    let mut out = Vec::with_capacity(CONTROL_NAMES.len());

    {
        let (auth, records) = control_fixtures();
        let r = compare_cross_store(&auth, &records);
        out.push(ControlResult {
            control: "identical".to_string(),
            expected: r.holds,
            detail: if r.holds {
                format!("{} tier record(s) compared, no divergence", r.tier_records)
            } else {
                format!("clean fixture reported divergence: {:?}", r.kinds())
            },
        });
    }

    let cases: Vec<(DivergenceKind, TierMutation)> = vec![
        (
            DivergenceKind::MissingExecution,
            Box::new(|m: &mut Vec<MirroredRecord>| m.clear()),
        ),
        (
            DivergenceKind::StaleVersion,
            // Drop the newest revision: the mirror fell behind.
            Box::new(|m: &mut Vec<MirroredRecord>| {
                m.pop();
            }),
        ),
        (
            DivergenceKind::AheadVersion,
            Box::new(|m: &mut Vec<MirroredRecord>| {
                m.push(control_record(4, 9_900, "dddd", 99));
            }),
        ),
        (
            DivergenceKind::Checksum,
            // Same version, different content. The corruption that matters most:
            // every count agrees and the tier holds the wrong bytes.
            Box::new(|m: &mut Vec<MirroredRecord>| {
                let last = m.len() - 1;
                m[last] = control_record(3, 9_003, "deadbeefdeadbeef", 12);
            }),
        ),
        (
            DivergenceKind::AppliedCount,
            Box::new(|m: &mut Vec<MirroredRecord>| {
                let last = m.len() - 1;
                m[last] = control_record(3, 9_003, "c0ffee00c0ffee00", 999);
            }),
        ),
        (
            DivergenceKind::Order,
            // Versions descend with sequence: a record landed out of order.
            Box::new(|m: &mut Vec<MirroredRecord>| {
                let p0 = m[0].payload.clone();
                m[0].payload = m[1].payload.clone();
                m[1].payload = p0;
            }),
        ),
        (
            DivergenceKind::Unidentified,
            Box::new(|m: &mut Vec<MirroredRecord>| {
                m[1].payload = "{not json".to_string();
            }),
        ),
        (
            DivergenceKind::MissingBody,
            // A digest-only record: verifiable, not serveable.
            Box::new(|m: &mut Vec<MirroredRecord>| {
                let last = m.len() - 1;
                m[last].payload = json!({
                    "execution_id": 1,
                    "version": 9_003,
                    "checksum": "c0ffee00c0ffee00",
                    "applied_count": 12,
                })
                .to_string();
            }),
        ),
    ];

    for (kind, mutate) in cases {
        let (auth, mut records) = control_fixtures();
        mutate(&mut records);
        let r = compare_cross_store(&auth, &records);
        let fired = r.kinds().contains(&kind.as_str());
        out.push(ControlResult {
            control: kind.as_str().to_string(),
            expected: fired,
            detail: if fired {
                format!("detected; verdict kinds {:?}", r.kinds())
            } else {
                format!(
                    "NOT DETECTED — corrupted fixture reported kinds {:?}, holds={}",
                    r.kinds(),
                    r.holds
                )
            },
        });
    }

    out
}

/// Record the control results and return whether every one behaved.
fn record_controls(results: &[ControlResult]) -> bool {
    let mut all_ok = true;
    for r in results {
        let result = if r.expected { "expected" } else { "unexpected" };
        crate::metrics::record_ehdb_projection_control(&r.control, result);
        if !r.expected {
            all_ok = false;
            warn!(
                target: "noetl_server::ehdb_projection_parity",
                control = %r.control,
                detail = %r.detail,
                "EHDB projection parity CONTROL FAILED — every zero this comparator has \
                 published is void"
            );
        }
    }
    all_ok
}

// ===========================================================================
// Outcomes and fetching.
// ===========================================================================

/// Every way a comparison can end, including every way of *not knowing*.
///
/// None of the not-knowing outcomes is `match`. A relay that is unconfigured, a
/// worker that is unreachable, a tier that reports itself disabled — each is
/// reported as itself. The one thing this module will never do is turn a failed
/// fetch into an empty record set and score it as agreement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParityOutcome {
    /// Both stores agree.
    Match,
    /// They disagree; the report names how.
    Divergent,
    /// The incumbent has no row for this execution yet. Not a divergence — the
    /// orchestrator writes the snapshot on its first trigger, so an execution
    /// can legitimately have events and no snapshot.
    NoAuthoritative,
    /// `NOETL_EHDB_WORKER_QUERY_URL` is unset.
    RelayUnconfigured,
    /// The relay hop failed.
    WorkerUnreachable,
    /// The worker answered, and said the tier is not serving this read.
    TierUnavailable,
    /// The reply did not parse as a tier body.
    TierUnreadable,
    /// The read hit [`MAX_COMPARE_RECORDS`], so the record set is truncated and
    /// any verdict over it would be a verdict about a page.
    Truncated,
    /// The comparator is switched off.
    Disabled,
}

impl ParityOutcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Divergent => "divergent",
            Self::NoAuthoritative => "no_authoritative",
            Self::RelayUnconfigured => "relay_unconfigured",
            Self::WorkerUnreachable => "worker_unreachable",
            Self::TierUnavailable => "tier_unavailable",
            Self::TierUnreadable => "tier_unreadable",
            Self::Truncated => "truncated",
            Self::Disabled => "disabled",
        }
    }
}

pub const PARITY_OUTCOMES: [ParityOutcome; 9] = [
    ParityOutcome::Match,
    ParityOutcome::Divergent,
    ParityOutcome::NoAuthoritative,
    ParityOutcome::RelayUnconfigured,
    ParityOutcome::WorkerUnreachable,
    ParityOutcome::TierUnavailable,
    ParityOutcome::TierUnreadable,
    ParityOutcome::Truncated,
    ParityOutcome::Disabled,
];

/// The full result of one comparison, including the outcomes that carry no
/// report.
#[derive(Debug, Clone, Serialize)]
pub struct ComparisonResult {
    pub execution_id: i64,
    pub outcome: &'static str,
    pub report: Option<CrossStoreReport>,
    pub detail: Option<String>,
    /// Seconds since the incumbent row was last written.
    ///
    /// Reported on every comparison and never used to score one. It is how a
    /// reader tells "the mirror missed this" from "this predates the mirror"
    /// without the comparator having to guess when the mirror armed — see the
    /// module note on the backlog.
    pub snapshot_age_seconds: Option<i64>,
    /// Which store answered the tier read, as the worker reported it.
    pub tier_source: Option<String>,
}

/// `NOETL_EHDB_PROJECTION_PARITY_ENABLED` — the comparator's own switch.
pub const PARITY_ENABLED_ENV: &str = "NOETL_EHDB_PROJECTION_PARITY_ENABLED";

pub fn parity_enabled() -> bool {
    matches!(
        std::env::var(PARITY_ENABLED_ENV)
            .ok()
            .map(|s| s.trim().to_ascii_lowercase())
            .as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

fn relay_client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(reqwest::Client::new)
}

fn worker_query_base() -> Option<String> {
    std::env::var(crate::handlers::ehdb::WORKER_QUERY_URL_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Read the incumbent's current row.
async fn fetch_authoritative(
    state: &AppState,
    execution_id: i64,
) -> Result<Option<AuthoritativeSnapshot>, String> {
    let pool = state.pools.pool_for(execution_id);
    let row = sqlx::query(
        r#"
        SELECT version,
               checksum,
               COALESCE((meta->>'applied_count')::bigint, 0) AS applied_count,
               EXTRACT(EPOCH FROM (now() - updated_at))::bigint AS age_seconds
        FROM noetl.projection_snapshot
        WHERE aggregate_type = $1 AND aggregate_id = $2
          AND tenant_id = 'default' AND organization_id = 'default'
        "#,
    )
    .bind(AGGREGATE_TYPE)
    .bind(execution_id.to_string())
    .fetch_optional(pool)
    .await
    .map_err(|e| format!("projection_snapshot query: {e}"))?;

    use sqlx::Row;
    Ok(row.map(|r| AuthoritativeSnapshot {
        execution_id,
        version: r.try_get("version").unwrap_or(0),
        checksum: r.try_get::<String, _>("checksum").unwrap_or_default(),
        applied_count: r.try_get("applied_count").unwrap_or(0),
        age_seconds: r.try_get("age_seconds").unwrap_or(0),
    }))
}

/// Parse the worker's tier body into records.
fn parse_tier_body(body: &serde_json::Value) -> Result<Vec<MirroredRecord>, ParityOutcome> {
    // The worker's typed refusals carry `outcome`, and every one of them means
    // "this is not a record set". Scoring an empty `records` array from such a
    // body as "the tier holds nothing" is the fail-loud violation this whole
    // module is written against.
    if let Some(outcome) = body.get("outcome").and_then(|o| o.as_str()) {
        if outcome != "ok" {
            return Err(ParityOutcome::TierUnavailable);
        }
    }
    let Some(arr) = body.get("records").and_then(|r| r.as_array()) else {
        return Err(ParityOutcome::TierUnreadable);
    };
    Ok(arr
        .iter()
        .map(|r| MirroredRecord {
            global_sequence: r.get("global_sequence").and_then(|s| s.as_u64()).unwrap_or(0),
            payload: r
                .get("payload")
                .and_then(|p| p.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| r.to_string()),
        })
        .collect())
}

fn tier_source_of(body: &serde_json::Value) -> Option<String> {
    body.get("tier_query_source")
        .and_then(|s| s.as_str())
        .map(str::to_string)
}

/// Compare one execution across both stores.
pub async fn compare_execution(state: &AppState, execution_id: i64) -> ComparisonResult {
    let done = |outcome: ParityOutcome, detail: Option<String>, age: Option<i64>| ComparisonResult {
        execution_id,
        outcome: outcome.as_str(),
        report: None,
        detail,
        snapshot_age_seconds: age,
        tier_source: None,
    };

    if !parity_enabled() {
        return done(ParityOutcome::Disabled, None, None);
    }

    // Controls run on every request, not only on the sampler's tick: an operator
    // reading this endpoint during a cutover gets the discrimination evidence in
    // the same breath as the verdict.
    let controls = run_controls();
    let controls_ok = record_controls(&controls);

    let auth = match fetch_authoritative(state, execution_id).await {
        Err(e) => {
            crate::metrics::record_ehdb_crossstore_parity(TIER, ParityOutcome::TierUnreadable.as_str());
            return done(ParityOutcome::TierUnreadable, Some(e), None);
        }
        Ok(None) => {
            crate::metrics::record_ehdb_crossstore_parity(
                TIER,
                ParityOutcome::NoAuthoritative.as_str(),
            );
            return done(
                ParityOutcome::NoAuthoritative,
                Some(format!(
                    "noetl.projection_snapshot has no {AGGREGATE_TYPE} row for execution \
                     {execution_id} — the orchestrator writes it on its first trigger, so \
                     this is not a divergence"
                )),
                None,
            );
        }
        Ok(Some(a)) => a,
    };
    let age = Some(auth.age_seconds);

    let Some(base) = worker_query_base() else {
        crate::metrics::record_ehdb_crossstore_parity(
            TIER,
            ParityOutcome::RelayUnconfigured.as_str(),
        );
        return done(
            ParityOutcome::RelayUnconfigured,
            Some(format!(
                "{} is unset; the server cannot read tier storage directly \
                 (control-plane guard)",
                crate::handlers::ehdb::WORKER_QUERY_URL_ENV
            )),
            age,
        );
    };

    let url = format!(
        "{}/ehdb/tiers/{TIER}?execution={execution_id}&limit={MAX_COMPARE_RECORDS}",
        base.trim_end_matches('/')
    );
    let resp = relay_client().get(&url).timeout(RELAY_TIMEOUT).send().await;
    let body: serde_json::Value = match resp {
        Err(e) => {
            crate::metrics::record_ehdb_crossstore_parity(
                TIER,
                ParityOutcome::WorkerUnreachable.as_str(),
            );
            return done(ParityOutcome::WorkerUnreachable, Some(e.to_string()), age);
        }
        Ok(r) => match r.json().await {
            Ok(v) => v,
            Err(e) => {
                crate::metrics::record_ehdb_crossstore_parity(
                    TIER,
                    ParityOutcome::TierUnreadable.as_str(),
                );
                return done(ParityOutcome::TierUnreadable, Some(e.to_string()), age);
            }
        },
    };
    let tier_source = tier_source_of(&body);

    let records = match parse_tier_body(&body) {
        Ok(r) => r,
        Err(outcome) => {
            crate::metrics::record_ehdb_crossstore_parity(TIER, outcome.as_str());
            return ComparisonResult {
                execution_id,
                outcome: outcome.as_str(),
                report: None,
                detail: Some(truncate(&body.to_string(), 400)),
                snapshot_age_seconds: age,
                tier_source,
            };
        }
    };

    if records.len() >= MAX_COMPARE_RECORDS {
        // A verdict over a truncated page is a verdict about the page.
        crate::metrics::record_ehdb_crossstore_parity(TIER, ParityOutcome::Truncated.as_str());
        return ComparisonResult {
            execution_id,
            outcome: ParityOutcome::Truncated.as_str(),
            report: None,
            detail: Some(format!(
                "tier returned {} record(s), at or above the {MAX_COMPARE_RECORDS} cap",
                records.len()
            )),
            snapshot_age_seconds: age,
            tier_source,
        };
    }

    let report = compare_cross_store(&auth, &records);
    let outcome = if report.holds {
        ParityOutcome::Match
    } else {
        ParityOutcome::Divergent
    };
    crate::metrics::record_ehdb_crossstore_parity(TIER, outcome.as_str());
    for kind in report.kinds() {
        crate::metrics::record_ehdb_crossstore_divergence(TIER, kind);
    }
    if report.holds {
        crate::metrics::add_ehdb_crossstore_events_compared(TIER, 1);
    }
    if !report.holds {
        warn!(
            target: "noetl_server::ehdb_projection_parity",
            execution_id,
            authoritative_version = report.authoritative_version,
            tier_version = ?report.tier_version,
            kinds = ?report.kinds(),
            controls_ok,
            "EHDB projection tier diverged from noetl.projection_snapshot"
        );
    }

    ComparisonResult {
        execution_id,
        outcome: outcome.as_str(),
        report: Some(report),
        detail: None,
        snapshot_age_seconds: age,
        tier_source,
    }
}

// ===========================================================================
// Endpoints.
// ===========================================================================

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ParityQuery {}

/// `GET /api/ehdb/projection-parity/executions/{execution_id}`
pub async fn compare_execution_endpoint(
    State(state): State<AppState>,
    Path(execution_id): Path<i64>,
    Query(_q): Query<ParityQuery>,
) -> impl IntoResponse {
    let r = compare_execution(&state, execution_id).await;
    Json(json!({
        "action": "ehdb.projection.parity",
        "tier": TIER,
        "result": r,
    }))
}

/// `GET /api/ehdb/projection-parity/self-test` — run the controls and say
/// whether the comparator discriminates.
///
/// Exists because "no divergence" is the reading an operator most wants and the
/// least self-evidencing. This endpoint answers the prior question: can this
/// comparator tell the difference at all?
pub async fn self_test_endpoint(State(_state): State<AppState>) -> impl IntoResponse {
    let controls = run_controls();
    let all_ok = record_controls(&controls);
    Json(json!({
        "action": "ehdb.projection.parity.self_test",
        "tier": TIER,
        "controls_ok": all_ok,
        "controls": controls,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_pair_holds() {
        let (auth, records) = control_fixtures();
        let r = compare_cross_store(&auth, &records);
        assert!(r.holds, "clean fixture diverged: {:?}", r.divergences);
        assert_eq!(r.tier_version, Some(auth.version));
        assert_eq!(r.tier_records, 3);
    }

    #[test]
    fn every_control_discriminates() {
        // The anti-vacuity check, run as a test as well as in the binary: if a
        // control cannot fire, every zero this comparator publishes is void and
        // the failure should be visible at build time too.
        let results = run_controls();
        // Positive control on the control suite itself: it must cover every kind
        // AND the clean case, or "all controls passed" is a statement about a
        // shorter list than it appears to be.
        assert_eq!(
            results.len(),
            CONTROL_NAMES.len(),
            "the control suite must cover every pinned control name"
        );
        for r in &results {
            assert!(r.expected, "control {} failed: {}", r.control, r.detail);
        }
    }

    #[test]
    fn control_names_match_the_suite_and_the_kinds() {
        // Three lists that must not drift: the pinned label set, the kinds the
        // comparator can emit, and what the suite actually drives.
        let mut names: Vec<String> = CONTROL_NAMES.iter().map(|s| s.to_string()).collect();
        names.sort();
        let mut driven: Vec<String> = run_controls().into_iter().map(|r| r.control).collect();
        driven.sort();
        assert_eq!(names, driven);
        for k in DIVERGENCE_KINDS {
            assert!(
                CONTROL_NAMES.contains(&k.as_str()),
                "divergence kind {} has no control — it would be a kind nothing \
                 proves the comparator can detect",
                k.as_str()
            );
        }
    }

    #[test]
    fn a_stale_mirror_is_not_reported_as_corruption() {
        // The distinction that decides where an operator looks. Dropping the
        // newest revision must report `stale_version` and NOT `checksum`:
        // comparing the incumbent's digest against an older revision's would
        // send them hunting corruption that does not exist.
        let (auth, mut records) = control_fixtures();
        records.pop();
        let r = compare_cross_store(&auth, &records);
        assert!(!r.holds);
        assert_eq!(r.kinds(), vec!["stale_version"]);
        assert_eq!(r.tier_version, Some(9_002));
    }

    #[test]
    fn same_version_different_content_is_a_checksum_divergence() {
        // The corruption that matters most: every count agrees, the tier holds
        // the wrong bytes. A comparator that only counted records would pass it.
        let (auth, mut records) = control_fixtures();
        let last = records.len() - 1;
        records[last] = control_record(3, auth.version, "deadbeefdeadbeef", auth.applied_count);
        let r = compare_cross_store(&auth, &records);
        assert!(!r.holds);
        assert!(r.kinds().contains(&"checksum"));
        assert!(
            !r.kinds().contains(&"stale_version"),
            "the version matches; only the content diverges"
        );
    }

    #[test]
    fn an_unparseable_record_is_never_scored_as_agreement() {
        let (auth, mut records) = control_fixtures();
        records[0].payload = "{not json".to_string();
        let r = compare_cross_store(&auth, &records);
        assert!(!r.holds);
        assert!(r.kinds().contains(&"unidentified"));
    }

    #[test]
    fn an_empty_tier_is_missing_not_matching() {
        // The single most important negative: no records must never be "nothing
        // to disagree about".
        let auth = control_auth();
        let r = compare_cross_store(&auth, &[]);
        assert!(!r.holds);
        assert_eq!(r.kinds(), vec!["missing_execution"]);
        assert_eq!(r.tier_version, None);
    }

    #[test]
    fn a_digest_only_record_is_verifiable_but_not_serveable() {
        let (auth, mut records) = control_fixtures();
        let last = records.len() - 1;
        records[last].payload = json!({
            "version": auth.version,
            "checksum": auth.checksum,
            "applied_count": auth.applied_count,
        })
        .to_string();
        let r = compare_cross_store(&auth, &records);
        assert!(!r.holds);
        assert_eq!(r.kinds(), vec!["missing_body"]);
    }

    #[test]
    fn a_worker_refusal_is_never_parsed_as_an_empty_record_set() {
        // Fail loud. The worker's refusal bodies have no `records`, and one that
        // did would still carry a non-ok `outcome`. Either way this must not
        // become "the tier holds nothing", which the comparator would then score
        // as `missing_execution` — a true-looking verdict about a read that
        // never happened.
        let refusal = json!({
            "action": "ehdb.tier.query",
            "outcome": "unavailable",
            "error": "the projection tier is served only by the tier service",
        });
        assert_eq!(
            parse_tier_body(&refusal).unwrap_err(),
            ParityOutcome::TierUnavailable
        );
        let garbage = json!({"hello": "world"});
        assert_eq!(
            parse_tier_body(&garbage).unwrap_err(),
            ParityOutcome::TierUnreadable
        );
        // Positive control: a real body still parses, or the two assertions
        // above are satisfied by a parser that rejects everything.
        let good = json!({
            "outcome": "ok",
            "record_count": 1,
            "records": [{"global_sequence": 1, "payload": "{\"version\":5}"}],
        });
        let recs = parse_tier_body(&good).expect("a well-formed body must parse");
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].global_sequence, 1);
    }

    #[test]
    fn not_knowing_is_never_labelled_match() {
        for o in PARITY_OUTCOMES {
            if o == ParityOutcome::Match {
                continue;
            }
            assert_ne!(
                o.as_str(),
                ParityOutcome::Match.as_str(),
                "every way of not knowing must have its own label"
            );
        }
        // ...and the labels are distinct, or two of them would be one series.
        let mut labels: Vec<&str> = PARITY_OUTCOMES.iter().map(|o| o.as_str()).collect();
        labels.sort_unstable();
        let n = labels.len();
        labels.dedup();
        assert_eq!(labels.len(), n, "outcome labels must be distinct");
    }
}
