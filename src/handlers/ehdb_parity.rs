//! Cross-store parity: does the EHDB event-log tier agree with `noetl.event`?
//!
//! **[ai-meta#258](https://github.com/noetl/ai-meta/issues/258).** Gates the
//! first EHDB primary tier flip ([#257](https://github.com/noetl/ai-meta/issues/257)
//! §3.4).
//!
//! # The gap this closes
//!
//! The worker's live mirror calls `compare_shadow_parity` with
//! `authoritative_sequence: None` (`worker/src/ehdb/eventlog.rs`), and with
//! `None` that comparator checks only the EHDB engine's **own** record count and
//! its **own** monotonic ordering. It compares EHDB to itself. So
//! `noetl_ehdb_eventlog_ops_total{outcome="mirrored"}` attests that records
//! landed in a log that stayed internally consistent — and carries no evidence
//! whatsoever that those records match the authoritative log.
//!
//! A tier cannot be promoted to `primary` on a self-consistency signal. This
//! module supplies the missing half: a comparator that **reads both stores** and
//! reports where they disagree.
//!
//! # Why this lives in the server
//!
//! Per [`data-access-boundary.md`][dab] the worker must not read `noetl.*` at
//! all, and the server is the only component that reads/writes `noetl.event`. So
//! the comparison can only happen here.
//!
//! The server's own control-plane guard is preserved: it does **not** open tier
//! storage. It fetches the EHDB side through the relay that already exists —
//! `GET {NOETL_EHDB_WORKER_QUERY_URL}/ehdb/tiers/eventlog?execution=…`, the same
//! hop [`super::ehdb::raw_tier_query`] uses — so the tier read still happens in
//! a data-plane process. This module adds no new access, only a comparison
//! across two reads the server was already entitled to make.
//!
//! [dab]: https://github.com/noetl/ai-meta/blob/main/agents/rules/data-access-boundary.md
//!
//! # What is compared, and what deliberately is not
//!
//! Compared, per execution:
//!
//! | axis | check |
//! | :-- | :-- |
//! | presence | the authoritative log has mirror-expected events ⇒ the tier holds records for that execution |
//! | count | `|mirror-expected authoritative events| == |tier records|` |
//! | membership | every mirror-expected `event_id` appears in the tier, and every tier record matches *some* authoritative row |
//! | ordering | the tier's records, read in `global_sequence` order, carry `event_id`s in the same relative order the authoritative log has them |
//! | payload identity | for each shared `event_id`: `event_type`, `node_name`/`step` and `status` agree |
//!
//! "Mirror-expected" is load-bearing and is defined on
//! [`AuthoritativeEvent::mirror_expected`] — the mirror hook sits on the
//! worker's emit chokepoint, so server-authored events have no tier copy by
//! construction and comparing against the whole log would report divergence on
//! every healthy execution.
//!
//! **Not** compared: the event *body* (`context` / `result`). Byte-identity
//! there is structurally impossible, not merely awkward — the server rewrites
//! the producer's `context` into a `result` envelope and runs
//! `sanitize_sensitive_data` over it before the row is written. Comparing two
//! things that are defined to differ would report 100% divergence and teach an
//! operator to ignore the metric. "Payload identity" here means the identifying
//! projection, and the doc comment on [`payload_divergence`] names each field.
//!
//! # Fail loud
//!
//! Every way of *not knowing* has its own outcome label and none of them is
//! `match`. A relay that is unconfigured, a worker that is unreachable, a tier
//! that reports itself disabled, a page that hit the row cap — each is reported
//! as itself. The one thing this module will never do is turn a failed fetch
//! into an empty record set and score it as agreement.
//!
//! # Why the controls ship in the binary
//!
//! A comparator that cannot detect divergence reports zero divergence, and so
//! does a platform that is genuinely healthy. [`run_controls`] closes that by
//! driving synthetic inputs — one clean pair, and one deliberately corrupted
//! pair per divergence kind — through the **same** [`compare_cross_store`] the
//! live path uses, on every tick and on every request.
//!
//! That makes a zero readable: `..._control_total{result="unexpected"} == 0`
//! together with `..._control_total{result="expected"} > 0` says the comparator
//! ran and discriminated. Without it, `divergence_total == 0` is equally
//! consistent with "the stores agree" and "the comparator is broken", which is
//! the exact ambiguity that made the old signal worthless.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{info, warn};

use crate::state::AppState;

/// The tier this comparator covers. A label value, not a parameter: the other
/// four tiers have no authoritative counterpart in `noetl.*` to compare against
/// (projection is folded, KV/object/vector are derived), so widening this would
/// need a different authoritative source per tier, not another argument.
pub const TIER: &str = "eventlog";

/// Row cap applied to both sides of the comparison.
///
/// Matches the worker's `MAX_QUERY_LIMIT` and the server's `MAX_EHDB_LIMIT`. An
/// execution with more events than this cannot be compared, because a truncated
/// authoritative page against a truncated tier page produces a *count* verdict
/// that is an artefact of the cap. Such executions are reported as
/// [`ParityOutcome::SkippedTooLarge`], never as agreement and never as
/// divergence.
pub const MAX_COMPARE_EVENTS: usize = 1000;

/// How long to wait on the worker relay before giving up. Shorter than the
/// tier-relay handler's 15s: this is a background verifier, and a slow tick must
/// not pile ticks up behind it.
const RELAY_TIMEOUT: Duration = Duration::from_secs(10);

// ===========================================================================
// The comparison core — pure, so the controls and the live path share it.
// ===========================================================================

/// One event as the authoritative log holds it (`noetl.event`, projected).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuthoritativeEvent {
    pub event_id: i64,
    pub event_type: String,
    pub node_name: Option<String>,
    pub status: Option<String>,
    /// Whether the tier is **expected** to hold a copy of this event.
    ///
    /// The mirror hook sits on the worker's emit chokepoint
    /// (`ControlPlaneClient::emit_event`), so only worker-emitted events are
    /// ever mirrored. The server authors the rest itself — the orchestrator
    /// writes `playbook_started`, `command.issued`, `step.enter` and
    /// `playbook.completed` without going near a worker — and those have no
    /// tier copy by construction.
    ///
    /// Measured, not assumed. One `tests/gate_fast_probe` run in kind writes 13
    /// events; the tier receives **6**. Comparing the whole log would report a
    /// count divergence plus seven missing events on every healthy execution — a
    /// comparator that is wrong 100% of the time while looking like it found
    /// something.
    ///
    /// The marker is
    /// `meta->>'worker_id' IS NOT NULL AND event_type <> 'command.claimed'`, and
    /// both halves were measured:
    ///
    /// | events | carry `worker_id` | mirrored |
    /// | --: | --: | --: |
    /// | 13 | 8 | 6 |
    ///
    /// `worker_id` alone gives 8, not 6, because **`command.claimed` is written
    /// by the server inside the claim transaction** (`handlers::events`, the
    /// `EventRow::new("command.claimed", …).with_worker_id(request.worker_id)`
    /// branch). Its `worker_id` names the worker that *claimed* the command, not
    /// one that emitted an event — it never passes through
    /// `ControlPlaneClient::emit_event`, so it is never mirrored. Taking
    /// `worker_id` at face value here would have produced two phantom
    /// `missing_event`s on every execution.
    ///
    /// The rest of the marker is exact: every non-test `ExecutorEvent`
    /// construction in the worker (`events::emitter`, `executor::command` ×2,
    /// `spool_runtime` ×2, `subscription` ×2) sets `worker_id: Some(..)`, and
    /// `normalize_event_to_row` copies it into `meta` only when the producer
    /// supplied it.
    ///
    /// If the server ever authors a second event on a worker's behalf, this
    /// marker will call it mirror-expected and the comparator will report a
    /// `missing_event` — loudly, on the first execution. That is the intended
    /// direction to be wrong in: the exception list fails toward noticing.
    pub mirror_expected: bool,
}

/// One record as the EHDB event-log tier returned it.
///
/// `payload` is the mirrored `ExecutorEvent` JSON, verbatim — the worker mirrors
/// `serde_json::to_string(&event)` of the event it just emitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirroredRecord {
    pub global_sequence: u64,
    pub payload: String,
}

/// The identifying projection parsed out of one mirrored payload.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MirroredEvent {
    global_sequence: u64,
    event_id: i64,
    event_type: Option<String>,
    step: Option<String>,
    status: Option<String>,
}

/// A way the two stores disagreed.
///
/// One variant per label value; the label set is closed and pinned at 0 in
/// [`crate::metrics::init_ehdb_crossstore_series`], so an operator reading
/// `/metrics` sees `0` for kinds that have never fired rather than nothing at
/// all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DivergenceKind {
    /// The authoritative log has events for this execution; the tier has none.
    MissingExecution,
    /// Both stores hold records, but not the same number of them.
    Count,
    /// An authoritative `event_id` that the tier does not hold.
    MissingEvent,
    /// A tier record whose `event_id` is not in the authoritative log.
    ExtraEvent,
    /// The tier holds the right events in the wrong relative order.
    Order,
    /// A shared `event_id` whose identifying fields differ between the stores.
    Payload,
    /// A tier record that cannot be matched at all: its payload does not parse,
    /// or it carries no `event_id`.
    ///
    /// Deliberately a divergence rather than a skip. Before ai-meta#258 five
    /// live emit paths sent `event_id: None`, so the server assigned the id and
    /// the mirrored copy had none — those records are *unidentifiable* against
    /// the authoritative log, and calling that "nothing to compare" would hide
    /// the very thing that makes the tier unverifiable.
    Unidentified,
}

impl DivergenceKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MissingExecution => "missing_execution",
            Self::Count => "count",
            Self::MissingEvent => "missing_event",
            Self::ExtraEvent => "extra_event",
            Self::Order => "order",
            Self::Payload => "payload",
            Self::Unidentified => "unidentified",
        }
    }
}

/// Every [`DivergenceKind`] label value, for pinning and for the control suite.
pub const DIVERGENCE_KINDS: [DivergenceKind; 7] = [
    DivergenceKind::MissingExecution,
    DivergenceKind::Count,
    DivergenceKind::MissingEvent,
    DivergenceKind::ExtraEvent,
    DivergenceKind::Order,
    DivergenceKind::Payload,
    DivergenceKind::Unidentified,
];

/// One divergence, with enough detail to act on without re-running the query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Divergence {
    pub kind: DivergenceKind,
    pub detail: String,
}

/// The verdict for one execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CrossStoreReport {
    pub execution_id: String,
    /// Every authoritative event for this execution.
    pub authoritative_count: usize,
    /// The subset the tier is expected to hold — see
    /// [`AuthoritativeEvent::mirror_expected`]. This is the number the tier's
    /// record count is compared against; reporting both makes the scoping
    /// visible instead of leaving a reader to wonder why 13 events compare
    /// against 8 records.
    pub authoritative_expected: usize,
    /// Authoritative events the tier is **not** expected to hold at all.
    ///
    /// Surfaced rather than merely subtracted, because it is the headline fact
    /// for the event-log tier's promotion: on a `tests/gate_fast_probe` run this
    /// is 7 of 13. The mirror hook only ever sees what a worker emits, so the
    /// tier structurally cannot hold the whole event log today — a clean parity
    /// verdict says the mirrored subset agrees, not that the tier could serve
    /// the log (noetl/ai-meta#257 §3.4).
    pub unmirrored_by_design: usize,
    pub ehdb_count: usize,
    /// Tier records that parsed and carried an `event_id`.
    pub identified: usize,
    /// Shared `event_id`s whose identifying fields agreed.
    pub matched: usize,
    pub divergences: Vec<Divergence>,
    /// Every check held.
    pub holds: bool,
}

impl CrossStoreReport {
    fn kinds(&self) -> BTreeSet<&'static str> {
        self.divergences.iter().map(|d| d.kind.as_str()).collect()
    }
}

/// Cap on how many ids one divergence detail enumerates before truncating.
/// The truncation is stated in the detail string — a silently shortened list
/// reads as a complete one.
const DETAIL_IDS: usize = 8;

fn id_list(ids: &[i64]) -> String {
    if ids.len() <= DETAIL_IDS {
        format!("{ids:?}")
    } else {
        format!(
            "{:?} (+{} more of {} total)",
            &ids[..DETAIL_IDS],
            ids.len() - DETAIL_IDS,
            ids.len()
        )
    }
}

/// Read an `event_id` that may be encoded as a JSON number or as a string.
///
/// Both spellings occur: the worker's `ExecutorEvent` serialises `event_id` as a
/// number, while the server's read-model DTOs stringify 64-bit ids so JavaScript
/// consumers do not lose precision. Accepting one and rejecting the other would
/// make every record from the other producer read as `Unidentified` — a
/// comparator that reports divergence because of its own parser is worse than no
/// comparator.
fn read_i64(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::Number(n) => n.as_i64(),
        serde_json::Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn read_string(v: Option<&serde_json::Value>) -> Option<String> {
    v.and_then(|x| x.as_str()).map(|s| s.to_string())
}

/// Compare the two stores for one execution.
///
/// `authoritative` must be ordered by `event_id` ascending (the order
/// `ExecutionService::ehdb_events_by_execution` returns). `mirrored` is taken in
/// the order the tier returned it, which is `global_sequence` ascending; the
/// monotonicity of that sequence is itself checked rather than assumed.
///
/// Pure and side-effect free. Metrics are recorded by the caller, so the control
/// suite can drive this exact function without polluting the live counters with
/// synthetic verdicts.
pub fn compare_cross_store(
    execution_id: i64,
    authoritative: &[AuthoritativeEvent],
    mirrored: &[MirroredRecord],
) -> CrossStoreReport {
    let mut divergences: Vec<Divergence> = Vec::new();

    // --- parse the tier side -------------------------------------------------
    let mut parsed: Vec<MirroredEvent> = Vec::with_capacity(mirrored.len());
    for rec in mirrored {
        match serde_json::from_str::<serde_json::Value>(&rec.payload) {
            Ok(v) => {
                let id = v.get("event_id").and_then(read_i64);
                match id {
                    Some(event_id) => parsed.push(MirroredEvent {
                        global_sequence: rec.global_sequence,
                        event_id,
                        event_type: read_string(v.get("event_type")),
                        step: read_string(v.get("step")),
                        status: read_string(v.get("status")),
                    }),
                    None => divergences.push(Divergence {
                        kind: DivergenceKind::Unidentified,
                        detail: format!(
                            "tier record at global_sequence {} carries no usable event_id",
                            rec.global_sequence
                        ),
                    }),
                }
            }
            Err(e) => divergences.push(Divergence {
                kind: DivergenceKind::Unidentified,
                detail: format!(
                    "tier record at global_sequence {} did not parse as JSON: {e}",
                    rec.global_sequence
                ),
            }),
        }
    }

    // The two authoritative views. `expected` scopes the count / missing checks
    // to events the tier should hold; `auth_by_id` covers the whole log so a
    // tier record matching a server-authored event is recognised rather than
    // reported as an extra.
    let expected: Vec<&AuthoritativeEvent> =
        authoritative.iter().filter(|e| e.mirror_expected).collect();
    let auth_by_id: BTreeMap<i64, &AuthoritativeEvent> =
        authoritative.iter().map(|e| (e.event_id, e)).collect();
    let mirrored_ids: BTreeSet<i64> = parsed.iter().map(|m| m.event_id).collect();

    // --- presence ------------------------------------------------------------
    if !expected.is_empty() && mirrored.is_empty() {
        divergences.push(Divergence {
            kind: DivergenceKind::MissingExecution,
            detail: format!(
                "authoritative log holds {} mirror-expected events for execution {execution_id} \
                 (of {} total); the tier holds none",
                expected.len(),
                authoritative.len()
            ),
        });
    }

    // --- count ---------------------------------------------------------------
    // Suppressed when the tier is wholly absent: the missing_execution verdict
    // above already says it, and two lines for one fact makes the divergence
    // rate read double.
    if expected.len() != mirrored.len() && !(mirrored.is_empty() && !expected.is_empty()) {
        divergences.push(Divergence {
            kind: DivergenceKind::Count,
            detail: format!(
                "authoritative(mirror-expected)={} ehdb={} (authoritative total {})",
                expected.len(),
                mirrored.len(),
                authoritative.len()
            ),
        });
    }

    // --- membership ----------------------------------------------------------
    let missing: Vec<i64> = expected
        .iter()
        .map(|e| e.event_id)
        .filter(|id| !mirrored_ids.contains(id))
        .collect();
    if !missing.is_empty() {
        divergences.push(Divergence {
            kind: DivergenceKind::MissingEvent,
            detail: format!(
                "{} mirror-expected authoritative event(s) absent from the tier: {}",
                missing.len(),
                id_list(&missing)
            ),
        });
    }

    // The membership checks together assert SET EQUALITY between the
    // mirror-expected ids and the tier's ids. `extra` therefore has two
    // sub-cases, and both are real:
    //
    //  * an id with no authoritative row at all — the tier invented a record;
    //  * an id that exists but is not mirror-expected — the tier holds a
    //    server-authored event, which means either the mirror saw something it
    //    should not have or the `worker_id` marker is not the boundary this
    //    comparator believes it is. Either way an operator needs to know, and
    //    silently accepting it would let the scoping hide a real defect.
    let expected_ids: BTreeSet<i64> = expected.iter().map(|e| e.event_id).collect();
    let unknown: Vec<i64> = mirrored_ids
        .iter()
        .copied()
        .filter(|id| !auth_by_id.contains_key(id))
        .collect();
    let unexpected: Vec<i64> = mirrored_ids
        .iter()
        .copied()
        .filter(|id| auth_by_id.contains_key(id) && !expected_ids.contains(id))
        .collect();
    if !unknown.is_empty() {
        divergences.push(Divergence {
            kind: DivergenceKind::ExtraEvent,
            detail: format!(
                "{} tier record(s) with no authoritative row: {}",
                unknown.len(),
                id_list(&unknown)
            ),
        });
    }
    if !unexpected.is_empty() {
        divergences.push(Divergence {
            kind: DivergenceKind::ExtraEvent,
            detail: format!(
                "{} tier record(s) for events the mirror should never see \
                 (no meta.worker_id on the authoritative row): {}",
                unexpected.len(),
                id_list(&unexpected)
            ),
        });
    }

    // --- ordering ------------------------------------------------------------
    //
    // Two separate properties, because they fail for different reasons:
    //
    //  (a) the tier returned its own records out of sequence order — a tier read
    //      bug, and it would silently invalidate (b);
    //  (b) the shared events sit in a different relative order in the two stores
    //      — the real ordering divergence.
    let mut prev = 0u64;
    let mut sequence_monotonic = true;
    for m in &parsed {
        if m.global_sequence <= prev && prev != 0 {
            sequence_monotonic = false;
        }
        prev = m.global_sequence;
    }
    if !sequence_monotonic {
        divergences.push(Divergence {
            kind: DivergenceKind::Order,
            detail: "tier records were not returned in ascending global_sequence order".to_string(),
        });
    }

    let shared_in_tier_order: Vec<i64> = parsed
        .iter()
        .map(|m| m.event_id)
        .filter(|id| auth_by_id.contains_key(id))
        .collect();
    // Over the whole authoritative log, not just the expected subset: the
    // relative order of the tier's records is defined by where those events sit
    // in the real log, and a server-authored event interleaved between two
    // mirrored ones does not change their relative order.
    let shared_in_auth_order: Vec<i64> = authoritative
        .iter()
        .map(|e| e.event_id)
        .filter(|id| mirrored_ids.contains(id))
        .collect();
    if shared_in_tier_order != shared_in_auth_order {
        divergences.push(Divergence {
            kind: DivergenceKind::Order,
            detail: format!(
                "shared events differ in order — tier {} vs authoritative {}",
                id_list(&shared_in_tier_order),
                id_list(&shared_in_auth_order)
            ),
        });
    }

    // --- payload identity ----------------------------------------------------
    let mut matched = 0usize;
    for m in &parsed {
        let Some(auth) = auth_by_id.get(&m.event_id) else {
            continue; // already reported as ExtraEvent
        };
        match payload_divergence(auth, m) {
            Some(detail) => divergences.push(Divergence {
                kind: DivergenceKind::Payload,
                detail,
            }),
            None => matched += 1,
        }
    }

    CrossStoreReport {
        execution_id: execution_id.to_string(),
        authoritative_count: authoritative.len(),
        authoritative_expected: expected.len(),
        unmirrored_by_design: authoritative.len() - expected.len(),
        ehdb_count: mirrored.len(),
        identified: parsed.len(),
        matched,
        holds: divergences.is_empty(),
        divergences,
    }
}

/// Compare the identifying fields of one shared event.
///
/// Three fields, each chosen because the server writes it through unmodified:
///
/// * `event_type` — copied verbatim into `noetl.event.event_type`.
/// * `step` → `node_name` — `normalize_event_to_row` assigns
///   `node_name = request.step`.
/// * `status` — the producer's value is used as-is; the server derives one only
///   when the producer omits it, and `ExecutorEvent::status` is a non-optional
///   `String`, so a worker-mirrored event always supplies it.
///
/// A field the tier does not carry at all is a divergence, not a skip: the
/// mirrored copy is supposed to be the event, and an event without an
/// `event_type` is not one.
fn payload_divergence(auth: &AuthoritativeEvent, mirrored: &MirroredEvent) -> Option<String> {
    let mut fields: Vec<String> = Vec::new();

    match mirrored.event_type.as_deref() {
        Some(t) if t == auth.event_type => {}
        Some(t) => fields.push(format!("event_type: authoritative={:?} ehdb={t:?}", auth.event_type)),
        None => fields.push(format!(
            "event_type: authoritative={:?} ehdb=<absent>",
            auth.event_type
        )),
    }

    // `node_name` is nullable in the authoritative row; compare only when it is
    // present, and treat a present-vs-absent pair as a divergence.
    match (auth.node_name.as_deref(), mirrored.step.as_deref()) {
        (Some(a), Some(b)) if a == b => {}
        (None, None) => {}
        (a, b) => fields.push(format!("node_name/step: authoritative={a:?} ehdb={b:?}")),
    }

    match (auth.status.as_deref(), mirrored.status.as_deref()) {
        (Some(a), Some(b)) if a == b => {}
        (None, None) => {}
        (a, b) => fields.push(format!("status: authoritative={a:?} ehdb={b:?}")),
    }

    if fields.is_empty() {
        None
    } else {
        Some(format!("event_id {}: {}", auth.event_id, fields.join("; ")))
    }
}

// ===========================================================================
// Controls — so a zero is readable.
// ===========================================================================

/// One control's verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ControlResult {
    /// `identical` for the negative control, else the [`DivergenceKind`] the
    /// synthetic corruption is supposed to produce.
    pub control: String,
    /// Whether the comparator did what the control demands.
    pub expected: bool,
    pub detail: String,
}

/// Every control label value, for pinning.
pub const CONTROL_NAMES: [&str; 8] = [
    "identical",
    "missing_execution",
    "count",
    "missing_event",
    "extra_event",
    "order",
    "payload",
    "unidentified",
];

fn auth_event(event_id: i64, event_type: &str, step: &str, status: &str) -> AuthoritativeEvent {
    AuthoritativeEvent {
        event_id,
        event_type: event_type.to_string(),
        node_name: Some(step.to_string()),
        status: Some(status.to_string()),
        mirror_expected: true,
    }
}

fn mirrored_of(seq: u64, auth: &AuthoritativeEvent) -> MirroredRecord {
    MirroredRecord {
        global_sequence: seq,
        payload: json!({
            "execution_id": 1,
            "event_id": auth.event_id,
            "event_type": auth.event_type,
            "step": auth.node_name,
            "status": auth.status,
            "context": {},
        })
        .to_string(),
    }
}

/// A corruption applied to the tier side of a control fixture.
type TierMutation = Box<dyn Fn(&mut Vec<MirroredRecord>)>;

/// A clean synthetic pair, and the same pair corrupted one way per kind.
fn control_fixtures() -> (Vec<AuthoritativeEvent>, Vec<MirroredRecord>) {
    let auth = vec![
        auth_event(9_001, "playbook.started", "start", "STARTED"),
        auth_event(9_002, "step.enter", "fetch", "RUNNING"),
        auth_event(9_003, "playbook.completed", "end", "COMPLETED"),
    ];
    let mirrored = auth
        .iter()
        .enumerate()
        .map(|(i, a)| mirrored_of(i as u64 + 1, a))
        .collect();
    (auth, mirrored)
}

/// Drive the control suite through [`compare_cross_store`].
///
/// The negative control asserts a clean pair reports `holds`; each positive
/// control corrupts the pair in exactly one way and asserts the comparator
/// reports **that** kind. A control that comes back `expected: false` means the
/// comparator is not measuring what it claims, and every zero it has published
/// is void.
pub fn run_controls() -> Vec<ControlResult> {
    let mut out = Vec::with_capacity(CONTROL_NAMES.len());

    // Negative control: identical stores must hold.
    {
        let (auth, mirrored) = control_fixtures();
        let r = compare_cross_store(1, &auth, &mirrored);
        out.push(ControlResult {
            control: "identical".to_string(),
            expected: r.holds,
            detail: if r.holds {
                format!("{} events compared, no divergence", r.matched)
            } else {
                format!("clean fixture reported divergence: {:?}", r.kinds())
            },
        });
    }

    // Positive controls, one per kind. Each mutation is minimal and touches only
    // the tier side, mirroring how a real corruption would present.
    let cases: Vec<(DivergenceKind, TierMutation)> = vec![
        (
            DivergenceKind::MissingExecution,
            Box::new(|m: &mut Vec<MirroredRecord>| m.clear()),
        ),
        (
            DivergenceKind::Count,
            // A duplicate keeps every id present, so `count` fires without
            // dragging `missing_event` along with it.
            Box::new(|m: &mut Vec<MirroredRecord>| {
                let mut dup = m[2].clone();
                dup.global_sequence = 4;
                m.push(dup);
            }),
        ),
        (
            DivergenceKind::MissingEvent,
            Box::new(|m: &mut Vec<MirroredRecord>| {
                m.remove(1);
            }),
        ),
        (
            DivergenceKind::ExtraEvent,
            Box::new(|m: &mut Vec<MirroredRecord>| {
                let ghost = auth_event(9_999, "step.enter", "ghost", "RUNNING");
                m.push(mirrored_of(4, &ghost));
            }),
        ),
        (
            DivergenceKind::Order,
            // Swap the payloads, not the records: the sequence numbers stay
            // ascending, so this exercises the relative-order check rather than
            // the monotonicity check.
            Box::new(|m: &mut Vec<MirroredRecord>| {
                let p0 = m[0].payload.clone();
                m[0].payload = m[1].payload.clone();
                m[1].payload = p0;
            }),
        ),
        (
            DivergenceKind::Payload,
            Box::new(|m: &mut Vec<MirroredRecord>| {
                m[1].payload = m[1].payload.replace("\"step.enter\"", "\"step.exit\"");
            }),
        ),
        (
            DivergenceKind::Unidentified,
            Box::new(|m: &mut Vec<MirroredRecord>| {
                m[1].payload = "{not json".to_string();
            }),
        ),
    ];

    for (kind, mutate) in cases {
        let (auth, mut mirrored) = control_fixtures();
        mutate(&mut mirrored);
        let r = compare_cross_store(1, &auth, &mirrored);
        let fired = r.kinds().contains(kind.as_str());
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
        crate::metrics::record_ehdb_crossstore_control(&r.control, result);
        if !r.expected {
            all_ok = false;
            warn!(
                target: "noetl_server::ehdb_parity",
                control = %r.control,
                detail = %r.detail,
                "EHDB cross-store parity CONTROL FAILED — every zero this comparator has published is void"
            );
        }
    }
    all_ok
}

// ===========================================================================
// Fetching the two sides.
// ===========================================================================

/// Why a comparison did not produce a verdict, or what verdict it produced.
///
/// Every non-comparison is its own value. Folding any of them into "no
/// divergence" is the failure this module exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParityOutcome {
    Match,
    Divergent,
    /// The sampled execution has no authoritative events (it was deleted, or the
    /// candidate query raced a purge).
    AuthoritativeEmpty,
    /// `NOETL_EHDB_WORKER_QUERY_URL` is unset — the server cannot reach a tier
    /// read at all.
    EhdbUnconfigured,
    /// The relay was reachable but the tier could not answer.
    EhdbUnavailable,
    /// The worker answered that EHDB is switched off.
    EhdbDisabled,
    /// Either side hit [`MAX_COMPARE_EVENTS`]; a truncated comparison is not a
    /// comparison.
    SkippedTooLarge,
    /// The authoritative read failed.
    Error,
}

impl ParityOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Match => "match",
            Self::Divergent => "divergent",
            Self::AuthoritativeEmpty => "authoritative_empty",
            Self::EhdbUnconfigured => "ehdb_unconfigured",
            Self::EhdbUnavailable => "ehdb_unavailable",
            Self::EhdbDisabled => "ehdb_disabled",
            Self::SkippedTooLarge => "skipped_too_large",
            Self::Error => "error",
        }
    }
}

/// Every [`ParityOutcome`] label value, for pinning.
pub const PARITY_OUTCOMES: [ParityOutcome; 8] = [
    ParityOutcome::Match,
    ParityOutcome::Divergent,
    ParityOutcome::AuthoritativeEmpty,
    ParityOutcome::EhdbUnconfigured,
    ParityOutcome::EhdbUnavailable,
    ParityOutcome::EhdbDisabled,
    ParityOutcome::SkippedTooLarge,
    ParityOutcome::Error,
];

/// The result of one execution's comparison: an outcome always, a report when
/// there was something to compare.
pub struct ComparisonResult {
    pub outcome: ParityOutcome,
    pub report: Option<CrossStoreReport>,
    pub detail: Option<String>,
    /// Which store on the far side of the relay actually answered — the worker's
    /// own pod-local log (`local`), or the writer-fronted tier service
    /// (`service`). Read out of the tier reply's `tier_query_source` field
    /// (noetl/ai-meta#257 PR 4).
    ///
    /// **A verdict without this is not attributable.** The tier store is
    /// pod-local, so with more than one worker replica a `local` read answers
    /// from whichever replica the relay's Service happened to route to — a
    /// fragment of the tier, in a body shaped exactly like the whole of it. A
    /// full-set match read that way is a fact about one pod, not about the tier,
    /// and there is no way to tell the two apart after the fact.
    ///
    /// `None` means the reply carried no such field, which is what a worker
    /// older than PR 4 answers. That is reported as itself, not defaulted to
    /// `local`: guessing here would put a confident wrong label on the exact
    /// question this field exists to answer.
    pub tier_query_source: Option<String>,
}

/// Parse the tier's `read_execution` body into records.
///
/// With a parser, not a pattern — and against **both** shapes the read chain
/// produces, because there are two and they differ:
///
/// * `NOETL_EHDB_TIER_QUERY_SOURCE=local` (the default) goes through the
///   worker's `run_query`, which wraps every answer:
///   `{action, tier, op, outcome, result: {…, records: […]}}`.
/// * `…=service` returns the tier service's reply verbatim, which is the driver
///   outcome itself: `{action, execution_id, exists, record_count, returned,
///   records: […]}` — `records` at the top level and no `outcome` field.
///
/// A parser written against only the second shape reads the first as "no
/// records", which is not a parse failure — it is a **missing_execution
/// divergence manufactured out of a wrapper**. So the records are looked for in
/// `result` first and then at the top level, and a body with neither is refused
/// rather than treated as empty.
fn parse_tier_body(body: &serde_json::Value) -> Result<Vec<MirroredRecord>, ParityOutcome> {
    // Typed refusals first — these are answers, but not data, and each maps to
    // its own outcome rather than to an empty record set.
    if let Some(outcome) = body.get("outcome").and_then(|v| v.as_str()) {
        match outcome {
            "disabled" => return Err(ParityOutcome::EhdbDisabled),
            "unavailable" | "guard_refused" | "invalid" | "rejected" | "error" => {
                return Err(ParityOutcome::EhdbUnavailable)
            }
            _ => {}
        }
    }

    let inner = body.get("result").unwrap_or(body);
    let records = inner.get("records").and_then(|v| v.as_array());

    if records.is_none() {
        // No `records` anywhere: either an error body, or a reply this build
        // does not understand. Reporting it as "zero records" would manufacture
        // a divergence out of a protocol mismatch.
        return Err(ParityOutcome::EhdbUnavailable);
    }
    let records = records.expect("checked above");

    let mut out = Vec::with_capacity(records.len());
    for r in records {
        let global_sequence = r.get("global_sequence").and_then(|v| v.as_u64()).unwrap_or(0);
        let payload = r
            .get("payload")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        out.push(MirroredRecord {
            global_sequence,
            payload,
        });
    }
    Ok(out)
}

/// Fetch the authoritative side for one execution.
///
/// A dedicated projection rather than `ExecutionService::ehdb_events_by_execution`
/// because it needs one column that view does not carry: whether the row was
/// produced by a worker, and is therefore expected to have a tier copy. See
/// [`AuthoritativeEvent::mirror_expected`] for why that scoping is the
/// difference between a usable comparator and one that reports divergence on
/// every healthy execution.
///
/// `LIMIT` is `MAX_COMPARE_EVENTS + 1` so a full page is distinguishable from a
/// page that exactly filled the cap.
async fn fetch_authoritative(
    state: &AppState,
    execution_id: i64,
) -> Result<Vec<AuthoritativeEvent>, sqlx::Error> {
    // noetl/ai-meta#258 — the scope depends on WHO mirrors.
    //
    // With the worker mirroring (the default), only worker-emitted events can
    // have a tier copy and the marker below is the boundary. With the server
    // mirroring, the mirror sits on the chokepoint that writes `noetl.event`
    // itself, so **every** authoritative event is expected in the tier and the
    // marker would under-scope the comparison — it would pass while silently
    // ignoring the seven events the whole exercise exists to capture.
    //
    // Derived from the same variable both mirrors read, so the comparator cannot
    // hold a different opinion about the boundary than the producer does.
    let server_mirrors = crate::handlers::ehdb_eventlog_mirror::server_mirrors();

    let rows = sqlx::query_as::<_, (i64, String, Option<String>, Option<String>, bool)>(
        r#"
        SELECT
            event_id,
            event_type,
            node_name,
            status,
            ($3 OR ((meta->>'worker_id') IS NOT NULL
                    AND event_type <> 'command.claimed')) AS mirror_expected
        FROM noetl.event
        WHERE execution_id = $1
        ORDER BY event_id ASC
        LIMIT $2
        "#,
    )
    .bind(execution_id)
    .bind(MAX_COMPARE_EVENTS as i64 + 1)
    .bind(server_mirrors)
    .fetch_all(state.pools.pool_for(execution_id))
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(event_id, event_type, node_name, status, mirror_expected)| AuthoritativeEvent {
                event_id,
                event_type,
                node_name,
                status,
                mirror_expected,
            },
        )
        .collect())
}

/// Fetch the EHDB side for one execution through the worker relay.
async fn fetch_tier(
    http: &reqwest::Client,
    base: &str,
    execution_id: i64,
) -> Result<(Vec<MirroredRecord>, Option<String>), (ParityOutcome, String)> {
    let url = format!("{}/ehdb/tiers/eventlog", base.trim_end_matches('/'));
    let resp = http
        .get(&url)
        .query(&[
            ("execution", execution_id.to_string()),
            ("limit", MAX_COMPARE_EVENTS.to_string()),
        ])
        .timeout(RELAY_TIMEOUT)
        .send()
        .await
        .map_err(|e| {
            (
                ParityOutcome::EhdbUnavailable,
                format!("relay to {url} failed: {e}"),
            )
        })?;

    let status = resp.status();
    let body: serde_json::Value = resp.json().await.map_err(|e| {
        (
            ParityOutcome::EhdbUnavailable,
            format!("tier reply from {url} was not JSON: {e}"),
        )
    })?;

    if status == reqwest::StatusCode::NOT_IMPLEMENTED {
        return Err((
            ParityOutcome::EhdbUnconfigured,
            "worker relay reports the tier query surface is not configured".to_string(),
        ));
    }

    // Read the label BEFORE the parse, so a body that fails to parse still says
    // which store produced it. Attribution is most useful exactly when the
    // answer is wrong.
    let source = tier_source_of(&body);

    parse_tier_body(&body)
        .map(|recs| (recs, source))
        .map_err(|o| {
            (
                o,
                format!(
                    "tier reply carried no comparable records (http {}, body {})",
                    status.as_u16(),
                    truncate(&body.to_string(), 400)
                ),
            )
        })
}

/// Which store the worker says answered — `tier_query_source` off the tier
/// reply (noetl/ai-meta#257 PR 4).
///
/// `None` is a real answer, not a default: a worker older than PR 4 sends no
/// such field, and labelling that `local` would be a guess printed with the
/// same confidence as a measurement. The comparator reports the absence.
fn tier_source_of(body: &serde_json::Value) -> Option<String> {
    body.get("tier_query_source")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        // Slice on a char boundary — a tier error string can carry UTF-8, and
        // panicking inside the diagnostic path would lose the diagnosis.
        let end = (0..=n).rev().find(|i| s.is_char_boundary(*i)).unwrap_or(0);
        format!("{}…", &s[..end])
    }
}

/// One HTTP client for the relay, shared across ticks.
///
/// A per-comparison `Client` would open a fresh connection pool each time and
/// leave the sampler re-doing TLS/TCP setup on every execution.
fn relay_client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(reqwest::Client::new)
}

/// Compare one execution across both stores, end to end.
///
/// Records the outcome + any divergence kinds to the metric surface. Returns the
/// report so the on-demand endpoint can show its working.
pub async fn compare_execution(state: &AppState, execution_id: i64) -> ComparisonResult {
    let Some(base) = worker_query_base() else {
        crate::metrics::record_ehdb_crossstore_parity(TIER, ParityOutcome::EhdbUnconfigured.as_str());
        return ComparisonResult {
            outcome: ParityOutcome::EhdbUnconfigured,
            report: None,
            detail: Some(
                "NOETL_EHDB_WORKER_QUERY_URL is unset; the server cannot read the tier"
                    .to_string(),
            ),
            tier_query_source: None,
        };
    };

    // Authoritative side. Ask for one more than the cap so a full page is
    // distinguishable from a page that exactly filled it.
    let authoritative = match fetch_authoritative(state, execution_id).await {
        Ok(rows) => rows,
        Err(e) => {
            crate::metrics::record_ehdb_crossstore_parity(TIER, ParityOutcome::Error.as_str());
            return ComparisonResult {
                outcome: ParityOutcome::Error,
                report: None,
                detail: Some(format!("authoritative read failed: {e}")),
            tier_query_source: None,
            };
        }
    };
    if authoritative.len() > MAX_COMPARE_EVENTS {
        crate::metrics::record_ehdb_crossstore_parity(TIER, ParityOutcome::SkippedTooLarge.as_str());
        return ComparisonResult {
            outcome: ParityOutcome::SkippedTooLarge,
            report: None,
            detail: Some(format!(
                "execution has more than {MAX_COMPARE_EVENTS} authoritative events; a truncated comparison is not a comparison"
            )),
            tier_query_source: None,
        };
    }
    if authoritative.is_empty() {
        crate::metrics::record_ehdb_crossstore_parity(
            TIER,
            ParityOutcome::AuthoritativeEmpty.as_str(),
        );
        return ComparisonResult {
            outcome: ParityOutcome::AuthoritativeEmpty,
            report: None,
            detail: Some("no authoritative events for this execution".to_string()),
        tier_query_source: None,
        };
    }

    // Tier side.
    let (mirrored, tier_query_source) = match fetch_tier(relay_client(), &base, execution_id).await {
        Ok(m) => m,
        Err((outcome, detail)) => {
            crate::metrics::record_ehdb_crossstore_parity(TIER, outcome.as_str());
            warn!(
                target: "noetl_server::ehdb_parity",
                execution_id,
                outcome = outcome.as_str(),
                detail = %detail,
                "EHDB cross-store parity: could not read the tier"
            );
            return ComparisonResult {
                outcome,
                report: None,
                detail: Some(detail),
                tier_query_source: None,
            };
        }
    };
    if mirrored.len() >= MAX_COMPARE_EVENTS {
        crate::metrics::record_ehdb_crossstore_parity(TIER, ParityOutcome::SkippedTooLarge.as_str());
        return ComparisonResult {
            outcome: ParityOutcome::SkippedTooLarge,
            report: None,
            detail: Some(format!(
                "tier returned {MAX_COMPARE_EVENTS} records — the page cap; a truncated comparison is not a comparison"
            )),
            tier_query_source,
        };
    }

    let report = compare_cross_store(execution_id, &authoritative, &mirrored);
    let outcome = if report.holds {
        ParityOutcome::Match
    } else {
        ParityOutcome::Divergent
    };
    crate::metrics::record_ehdb_crossstore_parity(TIER, outcome.as_str());
    crate::metrics::add_ehdb_crossstore_events_compared(TIER, report.matched as u64);
    for kind in report.kinds() {
        crate::metrics::record_ehdb_crossstore_divergence(TIER, kind);
    }
    if !report.holds {
        warn!(
            target: "noetl_server::ehdb_parity",
            execution_id,
            authoritative = report.authoritative_count,
            ehdb = report.ehdb_count,
            kinds = ?report.kinds(),
            "EHDB cross-store parity DIVERGENCE"
        );
    }

    ComparisonResult {
        outcome,
        report: Some(report),
        detail: None,
        tier_query_source,
    }
}

fn worker_query_base() -> Option<String> {
    std::env::var(super::ehdb::WORKER_QUERY_URL_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ===========================================================================
// HTTP surface.
// ===========================================================================

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ParityQuery {
    /// Skip the control suite in the response body. The controls still run for
    /// the sampler; this only trims the payload for a caller that wants the
    /// verdict alone.
    pub controls: Option<bool>,
}

fn disabled_body() -> serde_json::Value {
    json!({
        "action": "ehdb.parity.crossstore",
        "tier": TIER,
        "status": "disabled",
        "reason": "NOETL_EHDB_CROSSSTORE_PARITY_ENABLED is not set; \
                   the cross-store comparator is default-off",
        "tracks": "noetl/ai-meta#258",
    })
}

/// `GET /api/ehdb/parity/executions/{execution_id}` — compare one execution
/// across both stores, on demand.
///
/// Returns the full report, including every divergence with its detail, plus the
/// control suite so the answer carries its own proof that the comparator
/// discriminates.
pub async fn compare_execution_endpoint(
    State(state): State<AppState>,
    Path(execution_id): Path<i64>,
    Query(q): Query<ParityQuery>,
) -> impl IntoResponse {
    if !state.config.ehdb_crossstore_parity_enabled {
        return (StatusCode::NOT_IMPLEMENTED, Json(disabled_body()));
    }

    let controls = run_controls();
    let controls_ok = record_controls(&controls);

    let result = compare_execution(&state, execution_id).await;

    // A comparison whose controls failed is not evidence, and the HTTP status
    // says so rather than leaving it to a field nobody reads.
    let status = if !controls_ok {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::OK
    };

    let mut body = json!({
        "action": "ehdb.parity.crossstore",
        "tier": TIER,
        "execution_id": execution_id.to_string(),
        "outcome": result.outcome.as_str(),
        "detail": result.detail,
        "report": result.report,
        // Which store this verdict is ABOUT (noetl/ai-meta#257 PR 4). Beside the
        // verdict rather than inside `report`, because `report` comes out of the
        // pure comparator the controls also drive, and a control has no store.
        "tier_query_source": result.tier_query_source,
        "controls_ok": controls_ok,
    });
    if q.controls.unwrap_or(true) {
        body["controls"] = serde_json::to_value(&controls).unwrap_or(serde_json::Value::Null);
    }
    (status, Json(body))
}

/// `GET /api/ehdb/parity/self-test` — run the control suite alone.
///
/// Exists so an operator can answer "does this comparator work?" without needing
/// an execution to point it at.
pub async fn self_test_endpoint(State(state): State<AppState>) -> impl IntoResponse {
    if !state.config.ehdb_crossstore_parity_enabled {
        return (StatusCode::NOT_IMPLEMENTED, Json(disabled_body()));
    }
    let controls = run_controls();
    let ok = record_controls(&controls);
    let status = if ok {
        StatusCode::OK
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (
        status,
        Json(json!({
            "action": "ehdb.parity.self_test",
            "tier": TIER,
            "controls_ok": ok,
            "controls": controls,
        })),
    )
}

// ===========================================================================
// The sampler — what accumulates the soak evidence.
// ===========================================================================

/// Spawn the background cross-store parity sampler.
///
/// Default-off twice over: the task returns immediately unless
/// `NOETL_EHDB_CROSSSTORE_PARITY_ENABLED` is set, and even then it does nothing
/// unless `NOETL_EHDB_CROSSSTORE_PARITY_INTERVAL_SECS` is greater than zero. The
/// second knob exists so the on-demand endpoint can be used without putting a
/// recurring query on the database.
pub fn spawn_crossstore_parity_sampler(state: AppState) {
    tokio::spawn(async move {
        let cfg = &state.config;
        if !cfg.ehdb_crossstore_parity_enabled {
            return;
        }
        if cfg.ehdb_crossstore_parity_interval_secs == 0 {
            info!(
                target: "noetl_server::ehdb_parity",
                "EHDB cross-store parity: endpoint enabled, sampler off (interval 0)"
            );
            return;
        }
        let interval = Duration::from_secs(cfg.ehdb_crossstore_parity_interval_secs);
        info!(
            target: "noetl_server::ehdb_parity",
            interval_secs = cfg.ehdb_crossstore_parity_interval_secs,
            sample = cfg.ehdb_crossstore_parity_sample_size,
            settle_secs = cfg.ehdb_crossstore_parity_settle_secs,
            lookback_secs = cfg.ehdb_crossstore_parity_lookback_secs,
            "EHDB cross-store parity sampler: ENABLED — comparing the event-log tier against noetl.event"
        );
        loop {
            tokio::time::sleep(interval).await;
            run_sampler_tick(&state).await;
        }
    });
}

/// One sampler tick: run the controls, pick settled executions, compare each.
async fn run_sampler_tick(state: &AppState) {
    // Controls first. If the comparator cannot discriminate, the tick's verdicts
    // are worthless and the operator needs to know that before reading them.
    let controls = run_controls();
    if !record_controls(&controls) {
        return;
    }

    let cfg = &state.config;
    let candidates = match sample_candidates(
        state,
        cfg.ehdb_crossstore_parity_settle_secs as i64,
        cfg.ehdb_crossstore_parity_lookback_secs as i64,
        cfg.ehdb_crossstore_parity_sample_size as i64,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            warn!(
                target: "noetl_server::ehdb_parity",
                error = %e,
                "EHDB cross-store parity: candidate query failed"
            );
            crate::metrics::record_ehdb_crossstore_parity(TIER, ParityOutcome::Error.as_str());
            return;
        }
    };

    for execution_id in candidates {
        let _ = compare_execution(state, execution_id).await;
    }
}

/// Pick executions that are **settled**: their newest event is older than
/// `settle_secs` and newer than `lookback_secs`.
///
/// The settle window is load-bearing, not a tuning knob. The mirror is
/// best-effort and lands after the authoritative write, so an execution that is
/// still emitting has a tier copy that is legitimately a few records behind.
/// Comparing it would report a `count` divergence that is a race, not a defect —
/// and a comparator that cries wolf on healthy executions gets ignored exactly
/// when it matters.
async fn sample_candidates(
    state: &AppState,
    settle_secs: i64,
    lookback_secs: i64,
    limit: i64,
) -> Result<Vec<i64>, sqlx::Error> {
    // The lookback is a WHERE, not just a HAVING. As a HAVING alone it would
    // read correctly and scan the entire event log every tick — the filter
    // applies after the grouping. Bounding the rows first keeps the scan on the
    // `created_at` index; the HAVING then applies the settle window to the
    // grouped result.
    let rows = sqlx::query_as::<_, (i64,)>(
        r#"
        SELECT execution_id
        FROM noetl.event
        WHERE created_at > NOW() - INTERVAL '1 second' * $2
        GROUP BY execution_id
        HAVING MAX(created_at) < NOW() - INTERVAL '1 second' * $1
        ORDER BY MAX(created_at) DESC
        LIMIT $3
        "#,
    )
    .bind(settle_secs)
    .bind(lookback_secs)
    .bind(limit)
    .fetch_all(state.pools.cluster())
    .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_control_behaves() {
        // The in-binary controls are the anti-vacuity mechanism; this asserts
        // they are wired correctly at build time, so a broken control cannot
        // ship silently and then be the thing that was supposed to catch it.
        for r in run_controls() {
            assert!(r.expected, "control {} failed: {}", r.control, r.detail);
        }
    }

    #[test]
    fn control_names_match_the_control_suite() {
        // Drift guard: the pinned label set and the suite must not disagree, or
        // a control fires on a series nobody pinned (absent until it fails —
        // exactly backwards).
        let produced: BTreeSet<String> = run_controls().into_iter().map(|r| r.control).collect();
        let pinned: BTreeSet<String> = CONTROL_NAMES.iter().map(|s| s.to_string()).collect();
        assert_eq!(produced, pinned);
    }

    #[test]
    fn identical_stores_hold() {
        let (auth, mirrored) = control_fixtures();
        let r = compare_cross_store(1, &auth, &mirrored);
        assert!(r.holds, "{:?}", r.divergences);
        assert_eq!(r.matched, 3);
        assert_eq!(r.identified, 3);
    }

    /// The scoping that keeps the comparator from being wrong on every healthy
    /// execution.
    ///
    /// Modelled on a real `tests/gate_fast_probe` run in kind: 13 authoritative
    /// events, 8 of them worker-emitted and therefore mirrored, 5 authored by
    /// the server (`playbook_started`, `command.issued` ×2, `step.enter`,
    /// `playbook.completed`) with no tier copy. Without the `mirror_expected`
    /// scoping this pair reports a count divergence plus five missing events.
    #[test]
    fn server_authored_events_are_not_expected_in_the_tier() {
        let mut auth = Vec::new();
        let mut mirrored = Vec::new();
        let mut seq = 0u64;
        for (i, (event_type, from_worker)) in [
            ("playbook_started", false),
            ("command.issued", false),
            // Carries meta.worker_id but is written by the server inside the
            // claim transaction, so it is never mirrored.
            ("command.claimed", false),
            ("command.started", true),
            ("call.done", true),
            ("command.completed", true),
            ("step.enter", false),
            ("command.issued", false),
            ("command.claimed", false),
            ("command.started", true),
            ("call.done", true),
            ("command.completed", true),
            ("playbook.completed", false),
        ]
        .into_iter()
        .enumerate()
        {
            let mut e = auth_event(9_000 + i as i64, event_type, "s", "OK");
            e.mirror_expected = from_worker;
            if from_worker {
                seq += 1;
                mirrored.push(mirrored_of(seq, &e));
            }
            auth.push(e);
        }

        let r = compare_cross_store(1, &auth, &mirrored);
        assert!(
            r.holds,
            "a healthy execution must not diverge: {:?}",
            r.divergences
        );
        // The exact shape measured in kind: 13 authoritative, 6 mirrored.
        assert_eq!(r.authoritative_count, 13);
        assert_eq!(r.authoritative_expected, 6);
        assert_eq!(r.unmirrored_by_design, 7);
        assert_eq!(r.ehdb_count, 6);
        assert_eq!(r.matched, 6);
    }

    /// The other half of the same property: scoping must not blind the
    /// comparator to a mirror that actually lost an event.
    #[test]
    fn scoping_still_catches_a_dropped_worker_event() {
        let mut auth = vec![
            auth_event(1, "playbook_started", "p", "STARTED"),
            auth_event(2, "call.done", "s", "COMPLETED"),
        ];
        auth[0].mirror_expected = false;
        // Only the server-authored event reaches the tier — the worker event was
        // lost. If the scoping suppressed the check this would read as clean.
        let mirrored = vec![mirrored_of(1, &auth[0])];
        let r = compare_cross_store(1, &auth, &mirrored);
        // The counts match (1 expected, 1 record) — which is exactly why the
        // membership checks are set equality and not a tally.
        assert!(r.kinds().contains("missing_event"), "{:?}", r.divergences);
        assert!(r.kinds().contains("extra_event"), "{:?}", r.divergences);
    }

    #[test]
    fn empty_both_sides_holds() {
        let r = compare_cross_store(1, &[], &[]);
        assert!(r.holds);
    }

    #[test]
    fn missing_execution_does_not_double_count_as_count() {
        let (auth, _) = control_fixtures();
        let r = compare_cross_store(1, &auth, &[]);
        let kinds = r.kinds();
        assert!(kinds.contains("missing_execution"));
        assert!(
            !kinds.contains("count"),
            "an absent tier must report once, not twice: {kinds:?}"
        );
    }

    #[test]
    fn stringified_event_id_is_identified() {
        // The two producers spell event_id differently; neither may read as
        // unidentified.
        let auth = vec![auth_event(9_001, "step.enter", "s", "RUNNING")];
        let mirrored = vec![MirroredRecord {
            global_sequence: 1,
            payload: json!({"event_id": "9001", "event_type": "step.enter",
                            "step": "s", "status": "RUNNING"})
            .to_string(),
        }];
        let r = compare_cross_store(1, &auth, &mirrored);
        assert!(r.holds, "{:?}", r.divergences);
    }

    #[test]
    fn out_of_order_sequences_are_caught_separately() {
        let (auth, mut mirrored) = control_fixtures();
        mirrored.swap(0, 1);
        let r = compare_cross_store(1, &auth, &mirrored);
        assert!(r.kinds().contains("order"), "{:?}", r.divergences);
    }

    #[test]
    fn a_tier_refusal_is_never_an_empty_record_set() {
        // The whole point: "I could not read it" must not become "it was empty",
        // which would then compare as a missing_execution divergence — a wrong
        // answer dressed as a finding.
        for body in [
            json!({"action": "ehdb.tier.query", "tier": "eventlog", "op": "query",
                   "outcome": "disabled",
                   "result": {"status": "disabled", "reason": "NOETL_EHDB_ENABLED not set"}}),
            json!({"action": "ehdb.tier.query", "outcome": "unavailable",
                   "result": {"error": "no such log"}}),
            json!({"action": "ehdb.tier.query", "outcome": "guard_refused",
                   "result": {"status": "guard_refused"}}),
            json!({"action": "ehdb.tier.query", "error": "relay failed"}),
            json!({"unexpected": "shape"}),
        ] {
            assert!(
                parse_tier_body(&body).is_err(),
                "must refuse to read records out of {body}"
            );
        }
    }

    /// ai-meta#257 PR 4. The verdict must be attributable to a store, and the
    /// absence of a label must read as absence — not as `local`.
    #[test]
    fn the_store_that_answered_is_read_off_the_reply() {
        assert_eq!(
            tier_source_of(&json!({"records": [], "tier_query_source": "service"})).as_deref(),
            Some("service")
        );
        assert_eq!(
            tier_source_of(&json!({"result": {"records": []}, "tier_query_source": "local"}))
                .as_deref(),
            Some("local")
        );
        // A worker older than PR 4 sends no label. Reporting that as `local`
        // would put a guess where the whole point is a measurement — and `local`
        // is precisely the answer that would be wrong to assume, because it is
        // the one that is only a fragment under multiple replicas.
        assert_eq!(tier_source_of(&json!({"records": []})), None);
        assert_eq!(tier_source_of(&json!({"tier_query_source": "   "})), None);
        assert_eq!(tier_source_of(&json!({"tier_query_source": 7})), None);
    }

    /// Both real reply shapes, verbatim from the two sources.
    ///
    /// The wrapped one is what the worker's `run_query` returns and what the
    /// server sees by default; the bare one is the tier service's reply relayed
    /// through untouched. A parser that handles only the bare shape reads the
    /// wrapped one as zero records — a fabricated divergence, and one that would
    /// have looked like a genuine finding.
    ///
    /// It had no `#[test]`: a doc comment landed between this function's
    /// attribute and the function, so the attribute bound to the *next* item and
    /// this one silently never ran. `cargo build --all-targets` said so twice —
    /// `duplicated attribute` here and `never used` there — and both warnings
    /// name the symptom rather than the cause (noetl/ai-meta#263 drive-by).
    #[test]
    fn both_real_reply_shapes_parse() {
        let bare = json!({
            "action": "eventlog-read-execution",
            "execution_id": "1",
            "exists": true,
            "record_count": 1,
            "returned": 1,
            "records": [{
                "global_sequence": 7, "execution_id": "1",
                "transaction_id": "t", "byte_len": 2, "payload": "{}"
            }],
        });
        let wrapped = json!({
            "action": "ehdb.tier.query",
            "tier": "eventlog",
            "op": "read",
            "outcome": "served",
            "result": bare,
        });

        for (name, body) in [("bare (tier service)", &bare), ("wrapped (run_query)", &wrapped)] {
            let recs = parse_tier_body(body)
                .unwrap_or_else(|e| panic!("{name} reply must parse, got {e:?}"));
            assert_eq!(recs.len(), 1, "{name}");
            assert_eq!(recs[0].global_sequence, 7, "{name}");
        }
    }

    #[test]
    fn payload_identity_covers_each_field() {
        let auth = auth_event(1, "step.enter", "fetch", "RUNNING");
        for (patch, field) in [
            (json!({"event_type": "step.exit"}), "event_type"),
            (json!({"step": "other"}), "node_name/step"),
            (json!({"status": "FAILED"}), "status"),
        ] {
            let mut v: serde_json::Value =
                serde_json::from_str(&mirrored_of(1, &auth).payload).unwrap();
            for (k, val) in patch.as_object().unwrap() {
                v[k] = val.clone();
            }
            let mirrored = vec![MirroredRecord {
                global_sequence: 1,
                payload: v.to_string(),
            }];
            let r = compare_cross_store(1, std::slice::from_ref(&auth), &mirrored);
            assert!(
                r.kinds().contains("payload"),
                "{field} difference must be a payload divergence: {:?}",
                r.divergences
            );
            assert!(
                r.divergences.iter().any(|d| d.detail.contains(field)),
                "the detail must name the field that differed: {:?}",
                r.divergences
            );
        }
    }

    #[test]
    fn a_record_without_event_id_is_unidentified_not_ignored() {
        let auth = vec![auth_event(1, "step.enter", "s", "RUNNING")];
        let mirrored = vec![MirroredRecord {
            global_sequence: 1,
            payload: json!({"event_type": "step.enter", "step": "s", "status": "RUNNING"})
                .to_string(),
        }];
        let r = compare_cross_store(1, &auth, &mirrored);
        assert!(!r.holds);
        assert!(r.kinds().contains("unidentified"), "{:?}", r.divergences);
    }
}
