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
    /// Content fields, fetched only when the content comparison is armed
    /// (noetl/ai-meta#325). `None` when off, so the off path runs the same query
    /// it always ran.
    pub content: Option<serde_json::Map<String, serde_json::Value>>,
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

/// Arm the **content** comparison (noetl/ai-meta#325). Default **off**.
///
/// Off, this module behaves exactly as it did: `payload_divergence` compares the
/// three identifying fields and nothing else, and every verdict is byte-identical
/// to before. That default is not timidity — turning this on converts a large
/// share of today's `match` verdicts into `divergent`, and
/// `noetl_ehdb_crossstore_divergence_total` feeds a paging alert. Enabling the
/// flag and re-pointing that alert have to happen together; see
/// `playbooks/325-content-parity/ALERT-RETUNE.md`.
pub const CONTENT_PARITY_ENV: &str = "NOETL_EHDB_CROSSSTORE_PARITY_CONTENT";

/// Whether the content comparison is armed.
pub fn content_parity_enabled() -> bool {
    std::env::var(CONTENT_PARITY_ENV)
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// The content fields, in the order they are reported.
///
/// Chosen because the fold reads them: a difference in any of these changes the
/// rebuilt `WorkflowState`, which is what "the tier can stand in for Postgres"
/// actually has to mean.
pub const CONTENT_FIELDS: [&str; 6] = [
    "context",
    "result",
    "meta",
    "error",
    "worker_id",
    "parent_execution_id",
];

/// `null` and absent are the same thing here.
///
/// The authoritative row stores a SQL NULL; the mirrored payload may carry
/// `null`, or omit the key. Three spellings of "no value" that must not read as
/// a divergence — the same equivalence `normalise_null_json` applies on the fold
/// side.
fn json_present(v: Option<&serde_json::Value>) -> Option<&serde_json::Value> {
    match v {
        None | Some(serde_json::Value::Null) => None,
        // An empty object or array is "no value" too, and the two stores spell it
        // differently: the authoritative column is a SQL NULL while the mirrored
        // payload carries `{}`. Treating those as different reported a content
        // divergence on every event with no context — which is most of them.
        Some(serde_json::Value::Object(o)) if o.is_empty() => None,
        Some(serde_json::Value::Array(a)) if a.is_empty() => None,
        Some(other) => Some(other),
    }
}

/// Marker substituted for a result payload, whichever representation it is in.
const RESULT_PAYLOAD_MARKER: &str = "__noetl_result_payload__";

/// Collapse the two representations of a result payload to one marker.
///
/// **This is the correctness core of the content comparison.**  `build_result_object`
/// (`handlers/events.rs`) writes a result either as an inlined `context` or as a
/// `reference` pointer, chosen by `result_kind` — and the tier holds the inlined
/// form where Postgres holds the pointer, systematically, on every execution
/// measured.  They denote the **same logical result**.  Comparing them raw would
/// report a content divergence on essentially every execution and say nothing.
///
/// So any object carrying exactly one of `context` / `reference` has that key
/// replaced by [`RESULT_PAYLOAD_MARKER`]; an object carrying **both** is left
/// alone, because that is not a representation choice and a reader should see it.
/// Everything else recurses unchanged, so a genuine difference anywhere outside
/// the payload still survives.
///
/// ⚠ This asserts the two forms are **equivalent**, not that they are equal — the
/// pointer is not dereferenced. A tier holding an inlined payload that does not
/// match what the reference resolves to is invisible to this rule and needs the
/// reference resolved to catch. Recorded rather than hidden:
/// `noetl_ehdb_crossstore_result_representation_total`.
pub fn collapse_result_representation(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(o) => {
            let has_ctx = o.contains_key("context");
            let has_ref = o.contains_key("reference");
            let mut out = serde_json::Map::new();
            for (k, val) in o {
                if (has_ctx ^ has_ref) && (k == "context" || k == "reference") {
                    out.insert(
                        RESULT_PAYLOAD_MARKER.to_string(),
                        serde_json::Value::Bool(true),
                    );
                } else {
                    out.insert(k.clone(), collapse_result_representation(val));
                }
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.iter().map(collapse_result_representation).collect())
        }
        other => other.clone(),
    }
}

/// Two values that are the same number spelled differently.
///
/// `354748240001769472` and `"354748240001769472"` are the same id. Snowflakes
/// exceed 2^53, so anything that has been through a JavaScript-shaped hop may
/// carry them quoted, and the two stores do not agree on which.
pub fn numeric_spelling_agrees(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    fn as_num(v: &serde_json::Value) -> Option<String> {
        match v {
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::String(s) => {
                let t = s.trim();
                (!t.is_empty() && t.chars().all(|c| c.is_ascii_digit() || c == '-'))
                    .then(|| t.to_string())
            }
            _ => None,
        }
    }
    match (as_num(a), as_num(b)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// Whether a value is the **externalised** form of a payload.
///
/// `NOETL_PERMANENT_LOG_LEAN` replaces a large command context in `noetl.event`
/// with a pointer: `{"__context_ref__":"noetl://execution/…/__command_context__/…",
/// "__context_bytes__":1011}`. The tier holds the inlined object instead, because
/// the mirror copies the event before that substitution.
///
/// Same class as `reference` vs inlined `context` on a result — one logical value
/// in two representations — but a *different spelling*, which is why the earlier
/// collapse missed it and reported 622 false content divergences.
pub fn is_externalised_payload(v: &serde_json::Value) -> bool {
    v.get("__context_ref__")
        .map(|r| !r.is_null())
        .unwrap_or(false)
}

/// Whether two content values agree once null-spelling and result representation
/// are normalised away.
pub fn content_field_agrees(
    field: &str,
    auth: Option<&serde_json::Value>,
    tier: Option<&serde_json::Value>,
) -> bool {
    match (json_present(auth), json_present(tier)) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            if a == b {
                return true;
            }
            // Numerically equal values spelled differently are equal. A snowflake
            // id is a JSON number in one store and can arrive quoted from the
            // other; `ehdb_projection_fold` already carries the same accommodation
            // for `worker_id`, and it bit this comparator too.
            if numeric_spelling_agrees(a, b) {
                return true;
            }
            // Externalised vs inlined: one logical payload, two representations.
            //
            // ⚠ EQUIVALENCE, not equality — the `__context_ref__` pointer is never
            // dereferenced, exactly as the `reference` collapse below does not.
            // A tier whose inlined copy disagrees with what the pointer resolves
            // to is invisible to this, and closing that needs the pointer read.
            match (is_externalised_payload(a), is_externalised_payload(b)) {
                (true, false) | (false, true) => return true,
                // Both externalised: they are comparable AS pointers, and a
                // differing pointer is a real divergence.
                (true, true) => return a.get("__context_ref__") == b.get("__context_ref__"),
                (false, false) => {}
            }
            // Only `result` (and the contexts nested inside it) carries the
            // two-representation shape; applying the collapse everywhere would
            // blunt the comparison on fields that never have it.
            if field == "result" || field == "context" {
                collapse_result_representation(a) == collapse_result_representation(b)
            } else {
                false
            }
        }
        _ => false,
    }
}

/// The identifying projection parsed out of one mirrored payload.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MirroredEvent {
    global_sequence: u64,
    event_id: i64,
    event_type: Option<String>,
    step: Option<String>,
    status: Option<String>,
    /// Content fields, parsed only when the content comparison is armed
    /// (noetl/ai-meta#325). `None` when it is off, so the off path allocates nothing.
    content: Option<serde_json::Map<String, serde_json::Value>>,
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
    /// A shared `event_id` whose CONTENT differs — `context`, `result`, `meta`,
    /// `error`, `worker_id` or `parent_execution_id` (noetl/ai-meta#325).
    ///
    /// Only ever raised when `NOETL_EHDB_CROSSSTORE_PARITY_CONTENT` is armed.
    /// Separate from `Payload` because the two answer different questions:
    /// `Payload` asks "is this the same event", `Content` asks "does it still
    /// carry the same thing" — and a comparator that only ever answered the
    /// first is what let a systematically-divergent tier report `match`.
    Content,
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
            Self::Content => "content",
            Self::Unidentified => "unidentified",
        }
    }
}

/// Every [`DivergenceKind`] label value, for pinning and for the control suite.
pub const DIVERGENCE_KINDS: [DivergenceKind; 8] = [
    DivergenceKind::MissingExecution,
    DivergenceKind::Count,
    DivergenceKind::MissingEvent,
    DivergenceKind::ExtraEvent,
    DivergenceKind::Order,
    DivergenceKind::Payload,
    DivergenceKind::Content,
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
    /// Authoritative events for this execution that the verdict is **about** —
    /// i.e. every one of them, minus any held back by the lag tolerance window
    /// (`pending_authoritative`). The two sum to the execution's total.
    pub authoritative_count: usize,
    /// The mirror-expected `event_id`s the tier does not hold.
    ///
    /// ⚠ Structured, not only rendered into a `missing_event` divergence detail.
    /// The repair path needs this set exactly, and recovering it by parsing that
    /// human-readable string would make a repair — which WRITES to a
    /// `primary`-serving tier — depend on a message format nothing pins.
    ///
    /// Empty on a healthy execution, and `skip_serializing_if` keeps it off the
    /// wire there, so a clean report is byte-identical to before this existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_event_ids: Vec<i64>,
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
    /// Mirror-expected authoritative events held back by the lag tolerance
    /// window — too recent to be comparable, because an async mirror may still
    /// have them queued (noetl/ai-meta#155).
    ///
    /// Reported rather than silently subtracted. A tolerance that quietly grew
    /// to cover a whole execution would report `match` on a comparison that did
    /// not happen, and this is the number that makes that visible: if it is
    /// consistently the whole log, the window is too wide or the mirror is too
    /// slow, and both are things to act on.
    pub pending_authoritative: usize,
    /// Tier records excluded alongside them — events the mirror got to before
    /// the window expired. Excluded from **both** sides, so a fast mirror is
    /// not scored as holding extra records.
    pub pending_tier: usize,
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
    compare_cross_store_with_horizon(execution_id, authoritative, mirrored, None)
}

/// [`compare_cross_store`], with a bounded **lag tolerance**.
///
/// # Why this exists (noetl/ai-meta#155)
///
/// The mirror used to be synchronous: an event was in the tier before
/// `emit_events` returned, so comparing an execution the instant it was written
/// was sound. Taking the mirror off the hot path breaks that. Any execution
/// sampled while its newest events are still on the mirror queue has a tier copy
/// that is legitimately a few records behind, and this comparator — which is
/// what demotes a `primary`-serving tier — would call that `missing_event` and
/// demote a healthy tier.
///
/// That is [#263](https://github.com/noetl/ai-meta/issues/263) inverted: #263
/// was a tier reporting completeness it did not have; this would be a tier
/// reporting divergence it does not have. Both end with an operator unable to
/// trust the signal.
///
/// # What the tolerance does, and what it deliberately does not do
///
/// `horizon` is the largest authoritative `event_id` old enough to be
/// comparable. Everything after it is **excluded from both sides** — the
/// authoritative events are not counted missing, and any tier records the
/// mirror already delivered for them are not counted extra.
///
/// Everything at or below the horizon is compared exactly as before. A
/// genuinely lost event does not become invisible; it becomes invisible **for
/// the length of the window** and then reports as `missing_event` forever, which
/// is the same verdict at a bounded delay. The window is therefore a *latency*
/// concession, never a *coverage* one, and the two controls
/// `lag_within_window` / `lag_beyond_window` assert both halves of that on every
/// tick.
///
/// The cut is a **prefix**, taken at the first event newer than the horizon
/// rather than by filtering every event against it. `event_id` is a snowflake so
/// the two orders agree, but a prefix cut cannot be wrong in the dangerous
/// direction if they ever disagree: it can only exclude *more* than intended,
/// never leave a still-queued event inside the compared set.
///
/// `None` restores the pre-#155 behaviour exactly, and is what the default
/// configuration (`..._LAG_TOLERANCE_SECS=0`) produces.
pub fn compare_cross_store_with_horizon(
    execution_id: i64,
    authoritative: &[AuthoritativeEvent],
    mirrored: &[MirroredRecord],
    horizon: Option<i64>,
) -> CrossStoreReport {
    compare_inner(
        execution_id,
        authoritative,
        mirrored,
        horizon,
        content_parity_enabled(),
    )
}

/// [`compare_cross_store`] with the content comparison **forced on**, whatever
/// the environment says.
///
/// Exists for the control suite (noetl/ai-meta#325): the controls have to prove
/// the widened comparator discriminates *before* anyone arms it somewhere that
/// arming it pages. Reading the flag here would make those controls silently
/// vacuous on exactly the deployments that have not enabled it yet — a control
/// that cannot fail is the thing this suite exists to prevent.
pub fn compare_cross_store_with_content(
    execution_id: i64,
    authoritative: &[AuthoritativeEvent],
    mirrored: &[MirroredRecord],
) -> CrossStoreReport {
    compare_inner(execution_id, authoritative, mirrored, None, true)
}

fn compare_inner(
    execution_id: i64,
    authoritative: &[AuthoritativeEvent],
    mirrored: &[MirroredRecord],
    horizon: Option<i64>,
    content_on: bool,
) -> CrossStoreReport {
    let mut divergences: Vec<Divergence> = Vec::new();

    // Prefix cut. `cut` is the count of comparable authoritative events.
    let cut = match horizon {
        None => authoritative.len(),
        Some(h) => authoritative
            .iter()
            .position(|e| e.event_id > h)
            .unwrap_or(authoritative.len()),
    };
    // Captured BEFORE the shadowing below: the read-skew bound needs the whole
    // page's maximum id, not the comparable prefix's. When the window holds the
    // entire execution back the prefix is EMPTY, and that is precisely the case
    // the skew was observed in (`auth=0 ehdb=1` on prod) — taking the max from
    // the prefix there would yield `None` and skip the fix exactly when it is
    // needed.
    let full_auth_max = authoritative.last().map(|e| e.event_id);
    let (comparable, held_back) = authoritative.split_at(cut);
    let held_back_ids: BTreeSet<i64> = held_back.iter().map(|e| e.event_id).collect();
    let pending_authoritative = held_back.iter().filter(|e| e.mirror_expected).count();
    let authoritative = comparable;

    // --- parse the tier side -------------------------------------------------
    // `content_on` arrives as a parameter, read once by the caller: a flag
    // sampled per record could make half a verdict about one policy and half
    // about another.
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
                        content: content_on.then(|| {
                            let mut m = serde_json::Map::new();
                            for f in CONTENT_FIELDS {
                                if let Some(val) = v.get(f) {
                                    m.insert(f.to_string(), val.clone());
                                }
                            }
                            // Mirror the authoritative side's COALESCE: the value
                            // lives in two places and the stores denormalise it
                            // differently. Comparing one location only reported
                            // 757 false content divergences in a 200-execution
                            // sweep — a LOCATION difference wearing the costume of
                            // a missing value.
                            if !m.get("worker_id").map(|w| !w.is_null()).unwrap_or(false) {
                                if let Some(w) = v.get("meta").and_then(|mm| mm.get("worker_id")) {
                                    if !w.is_null() {
                                        m.insert("worker_id".to_string(), w.clone());
                                    }
                                }
                            }
                            m
                        }),
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

    // Drop tier records for events still inside the tolerance window. Doing it
    // here, before every check, is what keeps the exclusion symmetric: the
    // count, membership, ordering and payload checks all then run over one
    // consistent pair of sets rather than each needing its own filter.
    //
    // A second exclusion, and it is a **read-skew** fix rather than a tolerance
    // one. The authoritative page and the tier page are two separate reads, and
    // for an execution that is still emitting, events land between them — so
    // the tier legitimately holds an `event_id` the authoritative page never
    // saw. That reads as `extra_event` + `count` against a log that is
    // perfectly consistent.
    //
    // Bounded by the authoritative page's own maximum id, which is sound
    // because `noetl.event` is append-only and `event_id` is a snowflake: an id
    // above that maximum cannot be an event the earlier read *should* have
    // returned. It can only be one written after it.
    //
    // Applied only when a horizon is active, so the default (tolerance 0) path
    // stays byte-identical to the pre-#155 comparator. Without a horizon the
    // caller is the sampler, which only ever compares settled executions and
    // therefore cannot hit this race.
    let auth_max = full_auth_max;
    let skewed_ahead = match (horizon, auth_max) {
        (Some(_), Some(max_id)) => parsed.iter().filter(|m| m.event_id > max_id).count(),
        _ => 0,
    };
    if skewed_ahead > 0 {
        let max_id = auth_max.expect("checked above");
        parsed.retain(|m| m.event_id <= max_id);
    }

    let pending_tier = parsed
        .iter()
        .filter(|m| held_back_ids.contains(&m.event_id))
        .count()
        + skewed_ahead;
    parsed.retain(|m| !held_back_ids.contains(&m.event_id));
    let ehdb_comparable = mirrored.len() - pending_tier;

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
    if !expected.is_empty() && ehdb_comparable == 0 {
        divergences.push(Divergence {
            kind: DivergenceKind::MissingExecution,
            detail: format!(
                "authoritative log holds {} mirror-expected comparable events for execution \
                 {execution_id} (of {} comparable, {} held back by the lag tolerance); the tier \
                 holds none",
                expected.len(),
                authoritative.len(),
                pending_authoritative
            ),
        });
    }

    // --- count ---------------------------------------------------------------
    // Suppressed when the tier is wholly absent: the missing_execution verdict
    // above already says it, and two lines for one fact makes the divergence
    // rate read double.
    if expected.len() != ehdb_comparable && !(ehdb_comparable == 0 && !expected.is_empty()) {
        divergences.push(Divergence {
            kind: DivergenceKind::Count,
            detail: format!(
                "authoritative(mirror-expected)={} ehdb={} (authoritative comparable {})",
                expected.len(),
                ehdb_comparable,
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
    let missing_event_ids = missing.clone();
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
        let mut diverged = false;
        if let Some(detail) = payload_divergence(auth, m) {
            divergences.push(Divergence {
                kind: DivergenceKind::Payload,
                detail,
            });
            diverged = true;
        }
        if let Some(detail) = content_divergence(auth, m) {
            divergences.push(Divergence {
                kind: DivergenceKind::Content,
                detail,
            });
            diverged = true;
        }
        if !diverged {
            matched += 1;
        }
    }

    CrossStoreReport {
        execution_id: execution_id.to_string(),
        authoritative_count: authoritative.len(),
        missing_event_ids,
        authoritative_expected: expected.len(),
        unmirrored_by_design: authoritative.len() - expected.len(),
        ehdb_count: ehdb_comparable,
        pending_authoritative,
        pending_tier,
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
        Some(t) => fields.push(format!(
            "event_type: authoritative={:?} ehdb={t:?}",
            auth.event_type
        )),
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

/// Compare the CONTENT fields of one shared event (noetl/ai-meta#325).
///
/// Returns `None` when the content comparison is not armed on **both** sides —
/// a one-sided `Some` would mean the query and the parse disagreed about the
/// flag, and reporting every field as absent-on-one-side would be a
/// configuration bug dressed up as data loss.
///
/// This is the comparison that makes a `match` verdict mean "the tier can stand
/// in for Postgres". Without it the comparator reads three identifying fields
/// and is structurally blind to a tier whose every payload differs.
fn content_divergence(auth: &AuthoritativeEvent, mirrored: &MirroredEvent) -> Option<String> {
    let (a, b) = match (&auth.content, &mirrored.content) {
        (Some(a), Some(b)) => (a, b),
        _ => return None,
    };
    let mut fields: Vec<String> = Vec::new();
    for f in CONTENT_FIELDS {
        if !content_field_agrees(f, a.get(f), b.get(f)) {
            fields.push(format!(
                "{f}: authoritative={} ehdb={}",
                summarise(a.get(f)),
                summarise(b.get(f))
            ));
        }
    }
    if fields.is_empty() {
        None
    } else {
        Some(format!("event_id {}: {}", auth.event_id, fields.join("; ")))
    }
}

/// A short, bounded rendering of a content value for the divergence detail.
///
/// Bounded on purpose: `context` and `result` carry whole tool payloads, and a
/// divergence detail that embeds one of them turns a verdict into a log bomb.
fn summarise(v: Option<&serde_json::Value>) -> String {
    match v {
        None | Some(serde_json::Value::Null) => "<absent>".to_string(),
        Some(other) => {
            let s = other.to_string();
            if s.len() <= 160 {
                s
            } else {
                format!("{}… ({} bytes)", &s[..160], s.len())
            }
        }
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
pub const CONTROL_NAMES: [&str; 12] = [
    "identical",
    "missing_execution",
    "count",
    "missing_event",
    "extra_event",
    "order",
    "payload",
    "unidentified",
    // The lag-tolerance pair (noetl/ai-meta#155). Two controls, not one,
    // because the property being asserted is a *discrimination* and a single
    // control can only ever prove half of it.
    "lag_within_window",
    "lag_beyond_window",
    // noetl/ai-meta#325. Two controls, not one, and the NEGATIVE one is the
    // load-bearing half: a content comparison that flags the tier's inlined
    // result against Postgres's `reference` pointer would report a divergence on
    // essentially every execution and mean nothing.
    "content",
    "result_representation",
];

fn auth_event(event_id: i64, event_type: &str, step: &str, status: &str) -> AuthoritativeEvent {
    AuthoritativeEvent {
        event_id,
        content: None,
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

    // ---- the lag-tolerance pair (noetl/ai-meta#155) -----------------------
    //
    // ONE fixture — three authoritative events, the newest absent from the tier
    // — driven through the comparator TWICE with only the horizon changed.
    //
    // That is the whole point. A tolerance window is dangerous in exactly one
    // way: it can be a comparator that has been taught to ignore everything, and
    // such a comparator passes a "clean parity under tolerance" test perfectly.
    // The only check with teeth is that the *same missing event* is forgiven
    // inside the window and reported outside it, so both controls run on
    // identical inputs and their verdicts must differ.
    {
        let (auth, mut mirrored) = control_fixtures();
        mirrored.pop(); // 9_003 is "still on the mirror queue"

        // Inside the window: the horizon stops before 9_003, so its absence is
        // in-flight, not divergence.
        let inside = compare_cross_store_with_horizon(1, &auth, &mirrored, Some(9_002));
        let ok_inside = inside.holds && inside.pending_authoritative == 1;
        out.push(ControlResult {
            control: "lag_within_window".to_string(),
            expected: ok_inside,
            detail: if ok_inside {
                format!(
                    "1 in-flight event held back, {} compared clean",
                    inside.matched
                )
            } else {
                format!(
                    "in-flight event was NOT tolerated — kinds {:?}, holds={}, pending={}",
                    inside.kinds(),
                    inside.holds,
                    inside.pending_authoritative
                )
            },
        });

        // Outside it: the same absent event, now old enough. Must still demote.
        let outside = compare_cross_store_with_horizon(1, &auth, &mirrored, Some(9_003));
        let ok_outside = outside
            .kinds()
            .contains(DivergenceKind::MissingEvent.as_str());
        out.push(ControlResult {
            control: "lag_beyond_window".to_string(),
            expected: ok_outside,
            detail: if ok_outside {
                format!(
                    "still detected past the window; kinds {:?}",
                    outside.kinds()
                )
            } else {
                format!(
                    "TOLERANCE SWALLOWED A REAL DIVERGENCE — kinds {:?}, holds={}, pending={}",
                    outside.kinds(),
                    outside.holds,
                    outside.pending_authoritative
                )
            },
        });
    }

    // --- noetl/ai-meta#325: the content comparison ---------------------------
    //
    // These run whether or not `NOETL_EHDB_CROSSSTORE_PARITY_CONTENT` is armed,
    // because they drive `compare_cross_store` over fixtures that already carry
    // content. That is deliberate: the widened comparator is provably working
    // BEFORE anyone turns it on in an environment where turning it on pages.
    {
        let (mut auth, mut mirrored) = control_fixtures();
        let payload = serde_json::json!({"data": {"rows": 3}, "status": "ok"});
        for a in auth.iter_mut() {
            let mut m = serde_json::Map::new();
            m.insert("result".to_string(), payload.clone());
            a.content = Some(m);
        }
        // The tier copy agrees, except on one event where the payload differs.
        let mut tier_content = vec![payload.clone(); auth.len()];
        tier_content[1] = serde_json::json!({"data": {"rows": 4}, "status": "ok"});
        for (rec, c) in mirrored.iter_mut().zip(tier_content.iter()) {
            let mut v: serde_json::Value = serde_json::from_str(&rec.payload).unwrap();
            v.as_object_mut()
                .unwrap()
                .insert("result".to_string(), c.clone());
            rec.payload = v.to_string();
        }
        let r = compare_cross_store_with_content(1, &auth, &mirrored);
        let fired = r.kinds().contains(DivergenceKind::Content.as_str());
        out.push(ControlResult {
            control: "content".to_string(),
            expected: fired,
            detail: if fired {
                format!("detected; verdict kinds {:?}", r.kinds())
            } else {
                format!(
                    "A CONTENT DIVERGENCE WENT UNSEEN — kinds {:?}, holds={}",
                    r.kinds(),
                    r.holds
                )
            },
        });
    }
    {
        // Negative control. Postgres holds a `reference`; the tier holds the
        // inlined `context`. Same logical result, two representations — this must
        // NOT be a divergence, or the comparison is unusable on real data.
        let (mut auth, mut mirrored) = control_fixtures();
        for a in auth.iter_mut() {
            let mut m = serde_json::Map::new();
            m.insert(
                "result".to_string(),
                serde_json::json!({"status": "ok", "reference": {"logical_uri": "gs://b/k"}}),
            );
            a.content = Some(m);
        }
        for rec in mirrored.iter_mut() {
            let mut v: serde_json::Value = serde_json::from_str(&rec.payload).unwrap();
            v.as_object_mut().unwrap().insert(
                "result".to_string(),
                serde_json::json!({"status": "ok", "context": {"data": {"big": "payload"}}}),
            );
            rec.payload = v.to_string();
        }
        let r = compare_cross_store_with_content(1, &auth, &mirrored);
        let clean = !r.kinds().contains(DivergenceKind::Content.as_str());
        out.push(ControlResult {
            control: "result_representation".to_string(),
            expected: clean,
            detail: if clean {
                "inlined-vs-reference correctly treated as one logical result".to_string()
            } else {
                format!(
                    "REPRESENTATION REPORTED AS A CONTENT DIVERGENCE — kinds {:?}",
                    r.kinds()
                )
            },
        });
    }

    out
}

/// Record the control results and return whether every one behaved.
fn record_controls(results: &[ControlResult], recording: ParityRecording) -> bool {
    let mut all_ok = true;
    for r in results {
        let result = if r.expected { "expected" } else { "unexpected" };
        // noetl/ai-meta#264 — `noetl_ehdb_crossstore_control_total{result="unexpected"}`
        // is itself alert-wired, so an inspecting caller must not move it either.
        // The control still RUNS on the inspection path and its verdict still
        // shapes the HTTP status; only the counter is withheld.
        if recording.records() {
            crate::metrics::record_ehdb_crossstore_control(&r.control, result);
        }
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
    /// Every authoritative event for this execution is inside the mirror lag
    /// tolerance window, so there was nothing old enough to compare
    /// (noetl/ai-meta#155).
    ///
    /// Its own outcome, not `match`, for the reason the module header gives:
    /// every way of *not knowing* is reported as itself. Scoring an untaken
    /// comparison as agreement is precisely how a tolerance window turns into a
    /// comparator that has been taught to ignore everything.
    PendingMirror,
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
            Self::PendingMirror => "pending_mirror",
            Self::Error => "error",
        }
    }
}

/// Every [`ParityOutcome`] label value, for pinning.
pub const PARITY_OUTCOMES: [ParityOutcome; 9] = [
    ParityOutcome::Match,
    ParityOutcome::Divergent,
    ParityOutcome::AuthoritativeEmpty,
    ParityOutcome::EhdbUnconfigured,
    ParityOutcome::EhdbUnavailable,
    ParityOutcome::EhdbDisabled,
    ParityOutcome::SkippedTooLarge,
    ParityOutcome::PendingMirror,
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
        let global_sequence = r
            .get("global_sequence")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
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
    content_wanted: bool,
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

    // The content columns are selected only when the comparison is armed
    // (noetl/ai-meta#325).  Off, this is the same query and the same row shape it
    // has always been — a widened SELECT that ran unconditionally would put
    // `context`/`result` payloads on the wire for every parity tick, which is the
    // cost the flag exists to avoid paying before anyone wants it.
    let content_on = content_wanted;

    let rows = sqlx::query_as::<
        _,
        (
            i64,
            String,
            Option<String>,
            Option<String>,
            bool,
            Option<serde_json::Value>,
            Option<serde_json::Value>,
            Option<serde_json::Value>,
            Option<serde_json::Value>,
            Option<String>,
            Option<i64>,
        ),
    >(
        r#"
        SELECT
            event_id,
            event_type,
            node_name,
            status,
            ($3 OR ((meta->>'worker_id') IS NOT NULL
                    AND event_type <> 'command.claimed')) AS mirror_expected,
            CASE WHEN $4 THEN context END      AS context,
            CASE WHEN $4 THEN result  END      AS result,
            CASE WHEN $4 THEN meta    END      AS meta,
            CASE WHEN $4 THEN error   END      AS error,
            -- `worker_id` lives in TWO places and the stores denormalise it
            -- differently: the column is frequently NULL while `meta->>'worker_id'`
            -- carries the value, and the mirrored payload copies it to the
            -- top-level field. Comparing the column alone reported 757 false
            -- content divergences in one 200-execution sweep — a LOCATION
            -- difference wearing the costume of a missing value.
            CASE WHEN $4 THEN COALESCE(worker_id, meta->>'worker_id') END AS worker_id,
            CASE WHEN $4 THEN parent_execution_id END AS parent_execution_id
        FROM noetl.event
        WHERE execution_id = $1
        ORDER BY event_id ASC
        LIMIT $2
        "#,
    )
    .bind(execution_id)
    .bind(MAX_COMPARE_EVENTS as i64 + 1)
    .bind(server_mirrors)
    .bind(content_on)
    .fetch_all(state.pools.pool_for(execution_id))
    .await?;

    Ok(rows
        .into_iter()
        .map(
            |(
                event_id,
                event_type,
                node_name,
                status,
                mirror_expected,
                context,
                result,
                meta,
                error,
                worker_id,
                parent_execution_id,
            )| {
                let content = content_on.then(|| {
                    let mut m = serde_json::Map::new();
                    let mut put = |k: &str, v: Option<serde_json::Value>| {
                        if let Some(v) = v {
                            m.insert(k.to_string(), v);
                        }
                    };
                    put("context", context);
                    put("result", result);
                    put("meta", meta);
                    put("error", error);
                    put("worker_id", worker_id.map(serde_json::Value::String));
                    // A NUMBER, not a string. The tier payload carries this as a
                    // JSON number (`EventRow::to_stream_json` serialises the i64
                    // directly), so stringifying it here reported every child
                    // execution as divergent on identical values —
                    // `authoritative="354748240001769472" ehdb=354748240001769472`.
                    // Introduced by this comparator, and only visible once
                    // noetl/ai-meta#326 made the column non-NULL: before that the
                    // field was absent and the spelling never came up.
                    put(
                        "parent_execution_id",
                        parent_execution_id.map(serde_json::Value::from),
                    );
                    m
                });
                AuthoritativeEvent {
                    event_id,
                    event_type,
                    node_name,
                    status,
                    mirror_expected,
                    content,
                }
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
/// Whether a comparison **writes** the parity metrics, or only reads.
///
/// The periodic sampler and the HTTP endpoint share one comparator, and before
/// noetl/ai-meta#264 both of them recorded. So an operator investigating a
/// divergence alert by GETting the endpoint was **incrementing the counter the
/// alert fires on** — and nothing in the response said so. On 2026-08-13 a
/// diagnostic sweep of 20 executions moved
/// `noetl_ehdb_crossstore_divergence_total` from 3 to 21 with no change in
/// platform behaviour, which is enough on its own to hold the policy firing or
/// re-fire it after it had cleared.
///
/// That inverts the relationship between a signal and its investigation: the
/// natural response to the alert makes the alert worse, invisibly, and a second
/// operator reading the graph afterwards sees a spike with no cause in the
/// platform. It also makes the counter useless as evidence during exactly the
/// incident in which someone is reading it.
///
/// The split is: **the sampler is the measurement, the endpoint is the
/// inspection.**
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParityRecording {
    /// The periodic sampler. This comparison *is* the measurement, so it counts.
    Record,
    /// An HTTP caller looking at one execution. Reads only — no metric moves.
    Inspect,
}

impl ParityRecording {
    pub fn records(self) -> bool {
        matches!(self, ParityRecording::Record)
    }
}

/// Record a parity outcome only when this comparison is the measurement.
fn record_parity(recording: ParityRecording, tier: &str, outcome: &str) {
    if recording.records() {
        crate::metrics::record_ehdb_crossstore_parity(tier, outcome);
    }
}

/// Record a divergence only when this comparison is the measurement.
fn record_divergence(recording: ParityRecording, tier: &str, kind: &str) {
    if recording.records() {
        crate::metrics::record_ehdb_crossstore_divergence(tier, kind);
    }
}

pub async fn compare_execution(
    state: &AppState,
    execution_id: i64,
    recording: ParityRecording,
    content_override: Option<bool>,
) -> ComparisonResult {
    // Resolved ONCE, before the fetch, because the SQL and the comparison must
    // agree about it: a query that skipped the content columns feeding a
    // comparison that wanted them would report every field absent on one side —
    // a configuration bug wearing the costume of data loss.
    let content_on = content_override.unwrap_or_else(content_parity_enabled);
    let Some(base) = worker_query_base() else {
        record_parity(recording, TIER, ParityOutcome::EhdbUnconfigured.as_str());
        return ComparisonResult {
            outcome: ParityOutcome::EhdbUnconfigured,
            report: None,
            detail: Some(
                "NOETL_EHDB_WORKER_QUERY_URL is unset; the server cannot read the tier".to_string(),
            ),
            tier_query_source: None,
        };
    };

    // Authoritative side. Ask for one more than the cap so a full page is
    // distinguishable from a page that exactly filled it.
    let authoritative = match fetch_authoritative(state, execution_id, content_on).await {
        Ok(rows) => rows,
        Err(e) => {
            record_parity(recording, TIER, ParityOutcome::Error.as_str());
            return ComparisonResult {
                outcome: ParityOutcome::Error,
                report: None,
                detail: Some(format!("authoritative read failed: {e}")),
                tier_query_source: None,
            };
        }
    };
    if authoritative.len() > MAX_COMPARE_EVENTS {
        record_parity(recording, TIER, ParityOutcome::SkippedTooLarge.as_str());
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
        record_parity(recording, TIER, ParityOutcome::AuthoritativeEmpty.as_str());
        return ComparisonResult {
            outcome: ParityOutcome::AuthoritativeEmpty,
            report: None,
            detail: Some("no authoritative events for this execution".to_string()),
            tier_query_source: None,
        };
    }

    // Tier side.
    let (mirrored, tier_query_source) = match fetch_tier(relay_client(), &base, execution_id).await
    {
        Ok(m) => m,
        Err((outcome, detail)) => {
            record_parity(recording, TIER, outcome.as_str());
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
        record_parity(recording, TIER, ParityOutcome::SkippedTooLarge.as_str());
        return ComparisonResult {
            outcome: ParityOutcome::SkippedTooLarge,
            report: None,
            detail: Some(format!(
                "tier returned {MAX_COMPARE_EVENTS} records — the page cap; a truncated comparison is not a comparison"
            )),
            tier_query_source,
        };
    }

    // The lag tolerance. Default 0 ⇒ `None` ⇒ byte-identical behaviour to
    // before noetl/ai-meta#155. A failure to compute it is NOT silently treated
    // as "no tolerance": that would restore the false-demote hazard on exactly
    // the configuration that asked for tolerance, so it is reported as `error`.
    let tolerance = state.config.ehdb_crossstore_parity_lag_tolerance_secs;
    let horizon = match mirror_lag_horizon(state, execution_id, tolerance).await {
        Ok(h) => h,
        Err(e) => {
            record_parity(recording, TIER, ParityOutcome::Error.as_str());
            return ComparisonResult {
                outcome: ParityOutcome::Error,
                report: None,
                detail: Some(format!("mirror lag horizon read failed: {e}")),
                tier_query_source,
            };
        }
    };

    let report = compare_inner(execution_id, &authoritative, &mirrored, horizon, content_on);
    let outcome = outcome_for(&report);
    if report.pending_authoritative > 0 {
        crate::metrics::add_ehdb_crossstore_pending(TIER, report.pending_authoritative as u64);
    }
    record_parity(recording, TIER, outcome.as_str());
    crate::metrics::add_ehdb_crossstore_events_compared(TIER, report.matched as u64);
    // An untaken comparison publishes NO divergence evidence.
    //
    // `pending_mirror` means every comparable event was inside the lag window,
    // so the report's `divergences` describe a comparison that did not happen.
    // Recording them anyway moves `crossstore_divergence_total` — the counter
    // ops#257's `EhdbCrossStoreDivergence` **pages** on — for a verdict that is
    // explicitly not a divergence.
    //
    // Found on prod: probing an in-flight execution through the on-demand
    // endpoint returned `pending_mirror` and left `{kind="count"} 1` and
    // `{kind="extra_event"} 1` behind. That is noetl/ai-meta#264 exactly —
    // investigating with the endpoint inflates the counter its own alert reads,
    // invisibly.
    if outcome == ParityOutcome::Divergent {
        for kind in report.kinds() {
            record_divergence(recording, TIER, kind);
        }
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

/// The largest authoritative `event_id` for this execution that is old enough
/// to be compared, given a lag tolerance in seconds.
///
/// Computed as **`MIN(event_id) - 1` over the events newer than the cutoff**,
/// not `MAX(event_id)` over the older ones. The two differ exactly when
/// `event_id` order and `created_at` order disagree, and only the first is safe:
/// it excludes every event at or after the first recent one, so a still-queued
/// event can never end up inside the compared set. `MAX` over the old side
/// would leave it there and report it missing — the false demote this whole
/// mechanism exists to prevent.
///
/// `Ok(None)` means nothing is newer than the cutoff, so the whole execution is
/// comparable.
async fn mirror_lag_horizon(
    state: &AppState,
    execution_id: i64,
    tolerance_secs: u64,
) -> Result<Option<i64>, sqlx::Error> {
    if tolerance_secs == 0 {
        return Ok(None);
    }
    let row: (Option<i64>,) = sqlx::query_as(
        r#"
        SELECT MIN(event_id)
        FROM noetl.event
        WHERE execution_id = $1
          AND created_at > NOW() - INTERVAL '1 second' * $2
        "#,
    )
    .bind(execution_id)
    .bind(tolerance_secs as i64)
    .fetch_one(state.pools.pool_for(execution_id))
    .await?;
    // `- 1` because the horizon is inclusive: the newest COMPARABLE id.
    Ok(row.0.map(|min_recent| min_recent - 1))
}

/// Score a report.
///
/// Split out from [`compare_execution`] so the one rule that a lag-tolerance
/// window could silently break — **an untaken comparison is not agreement** —
/// is assertable without a database. With the window wide enough, `holds` is
/// trivially true on an empty comparable set, and scoring that `match` would
/// publish a healthy parity rate for a comparator that compared nothing.
fn outcome_for(report: &CrossStoreReport) -> ParityOutcome {
    if report.authoritative_count == 0 && report.pending_authoritative > 0 {
        return ParityOutcome::PendingMirror;
    }
    if report.holds {
        ParityOutcome::Match
    } else {
        ParityOutcome::Divergent
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
    /// Force the content comparison on (or off) for THIS request only
    /// (noetl/ai-meta#325).
    ///
    /// `None` follows `NOETL_EHDB_CROSSSTORE_PARITY_CONTENT`. The override exists
    /// because this route records nothing (`ParityRecording::Inspect`), so an
    /// operator can measure exactly what enabling the flag would surface WITHOUT
    /// arming the background sampler — and it is the sampler, which records, that
    /// makes the paging alert fire.
    pub content: Option<bool>,
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
    let controls_ok = record_controls(&controls, ParityRecording::Inspect);

    // noetl/ai-meta#264 — inspection, not measurement: this must not move the
    // counters its own alert reads.
    let result = compare_execution(&state, execution_id, ParityRecording::Inspect, q.content).await;

    // A comparison whose controls failed is not evidence, and the HTTP status
    // says so rather than leaving it to a field nobody reads.
    let status = if !controls_ok {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::OK
    };

    let mut body = json!({
        "action": "ehdb.parity.crossstore",
        // noetl/ai-meta#264 — say so in the payload. The old behaviour was not
        // just wrong, it was invisible: nothing told the caller a metric had
        // moved, so the spike they caused had no cause they could see.
        "metrics_recorded": false,
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
    let ok = record_controls(&controls, ParityRecording::Inspect);
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
    if !record_controls(&controls, ParityRecording::Record) {
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
        // `None` = follow the env flag. The sampler is the RECORDING path, so a
        // per-request override must never reach it — that separation is what makes
        // the on-demand override safe to measure with.
        let _ = compare_execution(state, execution_id, ParityRecording::Record, None).await;
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
pub(crate) async fn sample_candidates(
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

    /// A snowflake spelled as a number and as a string is the same id.
    ///
    /// Found on the live-path validation of noetl/ai-meta#326: once the column
    /// became non-NULL, the comparator reported
    /// `authoritative="354748240001769472" ehdb=354748240001769472` — the same
    /// value, two spellings — which would have flagged EVERY child execution as
    /// divergent forever. Absent values had hidden it.
    #[test]
    fn a_number_and_its_quoted_form_are_the_same_id() {
        let n = serde_json::json!(354748240001769472i64);
        let q = serde_json::json!("354748240001769472");
        assert!(numeric_spelling_agrees(&n, &q));
        assert!(content_field_agrees(
            "parent_execution_id",
            Some(&n),
            Some(&q)
        ));
        assert!(content_field_agrees(
            "parent_execution_id",
            Some(&q),
            Some(&n)
        ));
    }

    /// …and it must not make different ids, or non-numeric text, agree.
    #[test]
    fn the_numeric_spelling_rule_does_not_excuse_different_values() {
        let a = serde_json::json!(1i64);
        let b = serde_json::json!("2");
        assert!(!numeric_spelling_agrees(&a, &b));
        assert!(!content_field_agrees(
            "parent_execution_id",
            Some(&a),
            Some(&b)
        ));
        // Non-numeric strings are not coerced.
        assert!(!numeric_spelling_agrees(
            &serde_json::json!("w-1"),
            &serde_json::json!("w-2")
        ));
        assert!(!numeric_spelling_agrees(
            &serde_json::json!(""),
            &serde_json::json!(0i64)
        ));
    }

    /// `worker_id` is denormalised into two places and the stores populate them
    /// differently — the column is often NULL while `meta.worker_id` carries the
    /// value. Both sides must coalesce, or a LOCATION difference reads as a
    /// missing value (757 false divergences in one 200-execution sweep).
    #[test]
    fn worker_id_is_coalesced_from_meta_on_the_tier_side() {
        let (mut auth, mut mirrored) = control_fixtures();
        auth.truncate(1);
        mirrored.truncate(1);
        // Authoritative: the COALESCE already happened in SQL, so the map holds
        // the effective value.
        let mut m = serde_json::Map::new();
        m.insert("worker_id".to_string(), serde_json::json!("w-7"));
        // `meta` is copied verbatim between the stores, so it must be equal on
        // both sides or this test measures a meta difference instead.
        m.insert("meta".to_string(), serde_json::json!({"worker_id": "w-7"}));
        auth[0].content = Some(m);
        // Tier: top-level absent, value only in meta — exactly the prod shape.
        let mut v: serde_json::Value = serde_json::from_str(&mirrored[0].payload).unwrap();
        v.as_object_mut()
            .unwrap()
            .insert("meta".to_string(), serde_json::json!({"worker_id": "w-7"}));
        mirrored[0].payload = v.to_string();

        let r = compare_cross_store_with_content(1, &auth, &mirrored);
        assert!(
            !r.kinds().contains(DivergenceKind::Content.as_str()),
            "the tier's meta.worker_id must satisfy the authoritative worker_id; \
             kinds {:?}",
            r.kinds()
        );
    }

    /// …and the coalesce must not hide a genuinely different worker.
    #[test]
    fn a_differing_worker_id_still_diverges_after_the_coalesce() {
        let (mut auth, mut mirrored) = control_fixtures();
        auth.truncate(1);
        mirrored.truncate(1);
        let mut m = serde_json::Map::new();
        m.insert("worker_id".to_string(), serde_json::json!("w-7"));
        m.insert(
            "meta".to_string(),
            serde_json::json!({"worker_id": "w-DIFFERENT"}),
        );
        auth[0].content = Some(m);
        let mut v: serde_json::Value = serde_json::from_str(&mirrored[0].payload).unwrap();
        v.as_object_mut().unwrap().insert(
            "meta".to_string(),
            serde_json::json!({"worker_id": "w-DIFFERENT"}),
        );
        mirrored[0].payload = v.to_string();

        let r = compare_cross_store_with_content(1, &auth, &mirrored);
        assert!(
            r.kinds().contains(DivergenceKind::Content.as_str()),
            "two different workers is a real divergence and the coalesce must not \
             swallow it; kinds {:?}",
            r.kinds()
        );
    }

    /// The lean-log externalisation is the SAME representation class as
    /// `reference`, in a different spelling — and missing it produced 622 false
    /// content divergences in the first measured sweep.
    #[test]
    fn an_externalised_context_agrees_with_the_inlined_one() {
        let ext = serde_json::json!({
            "__context_ref__": "noetl://execution/1/result/__command_context__/2",
            "__context_bytes__": 1011
        });
        let inlined = serde_json::json!({"args": {}, "render_context": {"_index": 0}});
        assert!(
            content_field_agrees("context", Some(&ext), Some(&inlined)),
            "externalised vs inlined is one logical payload in two representations"
        );
        assert!(
            content_field_agrees("context", Some(&inlined), Some(&ext)),
            "and it must hold in both directions"
        );
    }

    /// …but two POINTERS are comparable as pointers, so a differing one is real.
    #[test]
    fn two_different_externalised_pointers_are_a_divergence() {
        let a = serde_json::json!({"__context_ref__": "noetl://execution/1/x", "__context_bytes__": 10});
        let b = serde_json::json!({"__context_ref__": "noetl://execution/1/DIFFERENT", "__context_bytes__": 10});
        assert!(
            !content_field_agrees("context", Some(&a), Some(&b)),
            "two externalised payloads pointing at different places is a real \
             divergence — the collapse must not become a blanket amnesty for \
             anything carrying the marker"
        );
        assert!(content_field_agrees("context", Some(&a), Some(&a)));
    }

    /// The rule must not leak into fields that never carry the marker.
    #[test]
    fn the_externalisation_rule_does_not_excuse_ordinary_differences() {
        assert!(
            !content_field_agrees(
                "worker_id",
                Some(&serde_json::json!("w-1")),
                Some(&serde_json::json!("w-2"))
            ),
            "a differing worker_id is a real divergence and must survive"
        );
        assert!(
            !content_field_agrees(
                "context",
                Some(&serde_json::json!({"a": 1})),
                Some(&serde_json::json!({"a": 2}))
            ),
            "two INLINED contexts that differ are a real divergence"
        );
    }

    /// The per-request override must widen the comparison WITHOUT arming the
    /// sampler — that separation is the only reason it is safe to measure with.
    #[test]
    fn the_per_request_override_never_reaches_the_recording_path() {
        let me = include_str!("ehdb_parity.rs");
        assert!(me.len() > 40_000, "guard is not measuring this file");
        let squashed: String = me.split_whitespace().collect::<Vec<_>>().join(" ");

        // The RECORDING path must pass a literal None.
        let sampler = format!("ParityRecording::Record{}, None)", "");
        assert!(
            squashed.contains(&sampler),
            "the sampler must pass None so a per-request override can never widen \
             the path that RECORDS — recording is what makes the alert fire"
        );
        // And the on-demand route must be Inspect and honour the parameter.
        let ondemand = format!("ParityRecording::Inspect{}, q.content)", "");
        assert!(
            squashed.contains(&ondemand),
            "the on-demand route must be Inspect and must honour ?content="
        );
    }

    // ---- noetl/ai-meta#325: the content comparison --------------------------

    fn content_pair(
        auth_result: serde_json::Value,
        tier_result: serde_json::Value,
    ) -> (Vec<AuthoritativeEvent>, Vec<MirroredRecord>) {
        let (mut auth, mut mirrored) = control_fixtures();
        auth.truncate(1);
        mirrored.truncate(1);
        let mut m = serde_json::Map::new();
        m.insert("result".to_string(), auth_result);
        auth[0].content = Some(m);
        let mut v: serde_json::Value = serde_json::from_str(&mirrored[0].payload).unwrap();
        v.as_object_mut()
            .unwrap()
            .insert("result".to_string(), tier_result);
        mirrored[0].payload = v.to_string();
        (auth, mirrored)
    }

    /// **The gate the widening exists for.** A tier whose payload differs must no
    /// longer be able to report `match`.
    #[test]
    fn a_content_divergence_the_old_comparator_missed_is_now_caught() {
        let (auth, mirrored) = content_pair(
            serde_json::json!({"data": {"rows": 3}}),
            serde_json::json!({"data": {"rows": 4}}),
        );
        // The OLD comparison — three identifying fields — sees nothing.
        let narrow = compare_cross_store(1, &auth, &mirrored);
        assert!(
            !narrow.kinds().contains(DivergenceKind::Content.as_str()),
            "with the flag off this must behave exactly as before"
        );
        // The widened one catches it.
        let wide = compare_cross_store_with_content(1, &auth, &mirrored);
        assert!(
            wide.kinds().contains(DivergenceKind::Content.as_str()),
            "a differing `result` must raise Content — this is the whole point of \
             noetl/ai-meta#325; without it a tier can differ in every payload and \
             still report match"
        );
        assert!(!wide.holds);
    }

    /// Positive control: genuinely-equal payloads must still pass. A comparison
    /// that flags everything is as useless as one that flags nothing.
    #[test]
    fn equal_content_still_reports_a_clean_match() {
        let same = serde_json::json!({"data": {"rows": 3}, "status": "ok"});
        let (auth, mirrored) = content_pair(same.clone(), same);
        let r = compare_cross_store_with_content(1, &auth, &mirrored);
        assert!(
            r.holds,
            "identical content must hold; kinds were {:?}",
            r.kinds()
        );
        assert_eq!(r.matched, 1);
    }

    /// ⚠ The load-bearing negative: Postgres holds a `reference`, the tier holds
    /// the inlined `context`. One logical result, two representations. Flagging
    /// it would report a divergence on essentially every real execution.
    #[test]
    fn inlined_versus_reference_is_not_a_content_divergence() {
        let (auth, mirrored) = content_pair(
            serde_json::json!({"status": "ok", "reference": {"logical_uri": "gs://b/k"}}),
            serde_json::json!({"status": "ok", "context": {"data": {"big": "payload"}}}),
        );
        let r = compare_cross_store_with_content(1, &auth, &mirrored);
        assert!(
            !r.kinds().contains(DivergenceKind::Content.as_str()),
            "reference-vs-inlined must not read as a content divergence; kinds {:?}",
            r.kinds()
        );
    }

    /// …but the collapse must not become a blanket amnesty: an object carrying
    /// BOTH keys is not a representation choice, and a difference *beside* the
    /// payload still has to survive.
    #[test]
    fn the_representation_collapse_does_not_swallow_real_differences() {
        // Same representation shape, different sibling field.
        let (auth, mirrored) = content_pair(
            serde_json::json!({"status": "ok", "reference": {"logical_uri": "gs://b/k"}}),
            serde_json::json!({"status": "FAILED", "context": {"x": 1}}),
        );
        let r = compare_cross_store_with_content(1, &auth, &mirrored);
        assert!(
            r.kinds().contains(DivergenceKind::Content.as_str()),
            "a differing sibling field must still be caught even when the payload \
             representation differs; kinds {:?}",
            r.kinds()
        );

        // An object with BOTH keys is left alone by the collapse.
        let both = serde_json::json!({"reference": {"u": 1}, "context": {"x": 1}});
        let other = serde_json::json!({"reference": {"u": 2}, "context": {"x": 1}});
        assert_ne!(
            collapse_result_representation(&both),
            collapse_result_representation(&other),
            "carrying both keys is not a representation choice; the collapse must \
             not erase a difference inside them"
        );
    }

    /// null / absent / `{}` are three spellings of "no value" and must agree.
    #[test]
    fn empty_null_and_absent_agree() {
        for (a, b) in [
            (None, Some(serde_json::Value::Null)),
            (None, Some(serde_json::json!({}))),
            (Some(serde_json::Value::Null), Some(serde_json::json!([]))),
        ] {
            assert!(
                content_field_agrees("meta", a.as_ref(), b.as_ref()),
                "{a:?} vs {b:?} must agree — the authoritative column is a SQL NULL \
                 while the mirrored payload carries {{}}"
            );
        }
        assert!(
            !content_field_agrees("meta", Some(&serde_json::json!({"a": 1})), None),
            "a real value against absent is a divergence"
        );
    }

    /// The flag must default OFF, or merging this pages on a condition that has
    /// been true all along.
    #[test]
    fn the_content_comparison_defaults_off() {
        assert!(
            !content_parity_enabled(),
            "NOETL_EHDB_CROSSSTORE_PARITY_CONTENT must default to false — enabling \
             it converts a large share of today's `match` into `divergent`, and \
             that counter feeds a paging alert"
        );
        assert!(
            crate::handlers::ehdb_parity::DIVERGENCE_KINDS
                .iter()
                .any(|k| k.as_str() == "content"),
            "the new kind must be in the pinned set or its series is absent until \
             it first fires"
        );
        assert!(CONTROL_NAMES.contains(&"content"));
        assert!(CONTROL_NAMES.contains(&"result_representation"));
    }

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

    // =======================================================================
    // Lag tolerance (noetl/ai-meta#155)
    // =======================================================================

    /// `None` must be byte-identical to the pre-#155 comparator.
    ///
    /// The default configuration produces `None`, so this is the assertion that
    /// the whole feature is genuinely inert until switched on — the property
    /// every "default off" claim in this repo has at some point turned out not
    /// to have.
    #[test]
    fn no_horizon_is_exactly_the_old_comparator() {
        let (auth, mut mirrored) = control_fixtures();
        mirrored.remove(1);
        let old = compare_cross_store(1, &auth, &mirrored);
        let new = compare_cross_store_with_horizon(1, &auth, &mirrored, None);
        assert_eq!(old, new);
        assert_eq!(new.pending_authoritative, 0);
        assert_eq!(new.pending_tier, 0);
        assert!(new.kinds().contains("missing_event"));
    }

    /// The window forgives an in-flight event and nothing else.
    ///
    /// The sharp case: TWO events are absent from the tier — one old, one still
    /// queued. A window that merely suppressed `missing_event` would pass a
    /// single-absence test and fail this one. The old id must still be named.
    #[test]
    fn the_window_forgives_only_what_is_inside_it() {
        let auth = vec![
            auth_event(9_001, "playbook.started", "start", "STARTED"),
            auth_event(9_002, "step.enter", "fetch", "RUNNING"),
            auth_event(9_003, "step.exit", "fetch", "COMPLETED"),
            auth_event(9_004, "playbook.completed", "end", "COMPLETED"),
        ];
        // The tier is missing 9_002 (a real loss) and 9_004 (still queued).
        let mirrored = vec![mirrored_of(1, &auth[0]), mirrored_of(2, &auth[2])];

        let r = compare_cross_store_with_horizon(1, &auth, &mirrored, Some(9_003));
        assert!(
            !r.holds,
            "a real loss inside the compared prefix must diverge"
        );
        let missing = r
            .divergences
            .iter()
            .find(|d| d.kind == DivergenceKind::MissingEvent)
            .expect("the old absent event must still be reported");
        assert!(
            missing.detail.contains("9002"),
            "the pre-window loss must be named: {}",
            missing.detail
        );
        assert!(
            !missing.detail.contains("9004"),
            "the in-flight event must not be named as missing: {}",
            missing.detail
        );
        assert_eq!(r.pending_authoritative, 1);
    }

    /// The exclusion is symmetric.
    ///
    /// If the mirror is FAST — the record for an in-flight event is already in
    /// the tier — that record must not be scored as an `extra_event` or as a
    /// `count` mismatch. Excluding only the authoritative side would make the
    /// window punish the mirror for being quick, which is a divergence alarm
    /// that fires more often the healthier the system is.
    #[test]
    fn a_tier_record_inside_the_window_is_not_an_extra() {
        let (auth, mirrored) = control_fixtures(); // tier holds all three
        let r = compare_cross_store_with_horizon(1, &auth, &mirrored, Some(9_002));
        assert!(r.holds, "{:?}", r.divergences);
        assert_eq!(r.pending_authoritative, 1);
        assert_eq!(
            r.pending_tier, 1,
            "the already-mirrored record must be excluded too"
        );
        assert_eq!(r.authoritative_count, 2);
        assert_eq!(r.ehdb_count, 2);
    }

    /// An execution wholly inside the window yields no verdict — and that is
    /// NOT `match`.
    #[test]
    fn nothing_comparable_is_pending_not_agreement() {
        let (auth, mirrored) = control_fixtures();
        // Horizon below every id: the entire execution is in flight.
        let r = compare_cross_store_with_horizon(1, &auth, &mirrored, Some(9_000));
        assert!(
            r.holds,
            "an empty comparison has no divergences by definition"
        );
        assert_eq!(r.authoritative_count, 0);
        assert_eq!(r.pending_authoritative, 3);
        assert_eq!(
            outcome_for(&r).as_str(),
            "pending_mirror",
            "an untaken comparison scored as `match` would publish a healthy parity rate for a \
             comparator that compared nothing"
        );
    }

    /// A tier that is wholly empty still reports `missing_execution` when the
    /// window does not cover the whole execution.
    #[test]
    fn the_window_does_not_hide_a_wholly_absent_tier() {
        let (auth, _) = control_fixtures();
        let r = compare_cross_store_with_horizon(1, &auth, &[], Some(9_002));
        assert!(
            r.kinds().contains("missing_execution"),
            "{:?}",
            r.divergences
        );
    }

    /// The exact shape observed on prod 2026-08-19, reproduced.
    ///
    /// An in-flight execution probed through the on-demand endpoint: the whole
    /// authoritative page is inside the lag window, and the tier — read a moment
    /// later — already holds a record for an event written *after* that page was
    /// fetched. Before the fix this reported `count` + `extra_event` and, worse,
    /// `compare_execution` recorded those kinds into
    /// `crossstore_divergence_total`, the counter ops#257 pages on.
    ///
    /// The two reads are not atomic and cannot be; the bound is that
    /// `noetl.event` is append-only with snowflake ids, so an id above the
    /// page's maximum provably could not have been in it.
    #[test]
    fn a_tier_record_written_after_the_authoritative_read_is_not_an_extra() {
        let auth = vec![
            auth_event(9_001, "playbook.started", "start", "STARTED"),
            auth_event(9_002, "step.enter", "fetch", "RUNNING"),
        ];
        // The tier holds both, plus 9_003 — an event that landed between the
        // authoritative read and the tier read.
        let ahead = auth_event(9_003, "step.exit", "fetch", "COMPLETED");
        let mirrored = vec![
            mirrored_of(1, &auth[0]),
            mirrored_of(2, &auth[1]),
            mirrored_of(3, &ahead),
        ];

        // Whole execution inside the window — the prod case exactly.
        let r = compare_cross_store_with_horizon(1, &auth, &mirrored, Some(9_000));
        assert!(
            r.holds,
            "read skew must not manufacture divergence: {:?}",
            r.divergences
        );
        assert_eq!(r.authoritative_count, 0);
        assert_eq!(r.pending_authoritative, 2);
        assert_eq!(r.pending_tier, 3, "all three tier records are excluded");
        assert_eq!(outcome_for(&r).as_str(), "pending_mirror");

        // And with a horizon that makes the page comparable, the skewed record
        // still must not be an extra — but the real events must still compare.
        let r2 = compare_cross_store_with_horizon(1, &auth, &mirrored, Some(9_002));
        assert!(r2.holds, "{:?}", r2.divergences);
        assert_eq!(r2.matched, 2);
        assert_eq!(outcome_for(&r2).as_str(), "match");
    }

    /// The skew bound must NOT swallow a genuine extra event.
    ///
    /// The positive control for the fix above: an id with no authoritative row
    /// that sits *below* the page maximum is a real `extra_event` and must still
    /// be reported. Without this the fix could be "ignore anything unexplained".
    #[test]
    fn the_skew_bound_still_reports_a_real_extra_event() {
        let auth = vec![
            auth_event(9_001, "playbook.started", "start", "STARTED"),
            auth_event(9_003, "playbook.completed", "end", "COMPLETED"),
        ];
        let ghost = auth_event(9_002, "step.enter", "ghost", "RUNNING");
        let mirrored = vec![
            mirrored_of(1, &auth[0]),
            mirrored_of(2, &ghost), // invented, and BELOW the page max
            mirrored_of(3, &auth[1]),
        ];
        let r = compare_cross_store_with_horizon(1, &auth, &mirrored, Some(9_999));
        assert!(
            r.kinds().contains("extra_event"),
            "a tier record below the page max with no authoritative row is a real \
             divergence and must survive the skew bound: {:?}",
            r.divergences
        );
    }

    /// An untaken comparison must publish no divergence evidence.
    ///
    /// Guards the recording site rather than the comparator: `compare_execution`
    /// records `crossstore_divergence_total` only for `Divergent`. `Match`
    /// carries no kinds anyway, so this is really about `PendingMirror` — and
    /// that is the one that reached prod.
    #[test]
    fn only_a_divergent_verdict_publishes_divergence_kinds() {
        let src = include_str!("ehdb_parity.rs");
        let at = src
            .find("for kind in report.kinds()")
            .expect("the divergence recording loop must still exist");
        let guard = src[..at]
            .rfind("if outcome == ParityOutcome::Divergent")
            .expect("the recording loop must be guarded on a Divergent verdict");
        assert!(
            at - guard < 400,
            "the Divergent guard must immediately precede the divergence recording \
             loop; if the loop moved out from under it, a `pending_mirror` verdict \
             can page ops#257 again (noetl/ai-meta#264)"
        );
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

        for (name, body) in [
            ("bare (tier service)", &bare),
            ("wrapped (run_query)", &wrapped),
        ] {
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

    // =======================================================================
    // noetl/ai-meta#264 — the endpoint must not write the counters its own
    // alert reads.
    // =======================================================================

    /// Pull one counter series out of the real registry, **scoped to one tier
    /// label**.
    ///
    /// ⚠ Scoping matters. The registry is process-global and these tests run in
    /// parallel with every other test in the binary, so a helper that summed the
    /// whole metric family would read other tests' writes as its own. That is not
    /// hypothetical: an early version summed the family, and mutating the gate
    /// made a test fail that the mutation had nothing to do with. Each test owns
    /// a private tier label, so the reads cannot collide.
    fn counter_value(name: &str, tier: &str) -> f64 {
        let text = crate::metrics::gather_text().expect("gather");
        let tier_label = format!("tier=\"{tier}\"");
        text.lines()
            .filter(|l| !l.starts_with('#'))
            .filter(|l| l.starts_with(name) && l.contains(&tier_label))
            .filter_map(|l| l.rsplit(' ').next())
            .filter_map(|v| v.parse::<f64>().ok())
            .sum()
    }

    #[test]
    fn inspecting_does_not_move_the_counter_the_alert_reads() {
        // The alert is `sum(increase(noetl_ehdb_crossstore_divergence_total[10m])) > 0`.
        // On 2026-08-13 a diagnostic sweep of 20 executions took it 3 -> 21 with
        // no change in platform behaviour.
        const T: &str = "inspect_tier_264a";
        let before = counter_value("noetl_ehdb_crossstore_divergence_total", T);
        for _ in 0..20 {
            record_divergence(ParityRecording::Inspect, T, "count_mismatch");
        }
        assert_eq!(
            counter_value("noetl_ehdb_crossstore_divergence_total", T),
            before,
            "twenty inspections moved the divergence counter — an operator investigating \
             the alert would be re-firing it, invisibly"
        );
    }

    #[test]
    fn the_sampler_still_measures() {
        // ⚠ The positive control. Without it, "inspection does not record" would
        // also pass on an implementation that never records at all — which would
        // silence the alert instead of fixing it, and look identical on a graph.
        const T: &str = "record_tier_264b";
        let before = counter_value("noetl_ehdb_crossstore_divergence_total", T);
        record_divergence(ParityRecording::Record, T, "count_mismatch");
        assert_eq!(
            counter_value("noetl_ehdb_crossstore_divergence_total", T),
            before + 1.0,
            "the sampler is the measurement; if it stops counting the alert goes blind"
        );
    }

    #[test]
    fn inspecting_does_not_move_the_parity_outcome_counter_either() {
        // `..._parity_total{outcome="pending_mirror"}` feeds a RATIO alert, so
        // endpoint writes skew it even when they are not divergences.
        const T: &str = "tier_264c";
        let before = counter_value("noetl_ehdb_crossstore_parity_total", T);
        record_parity(ParityRecording::Inspect, T, "ok");
        assert_eq!(
            counter_value("noetl_ehdb_crossstore_parity_total", T),
            before
        );
        record_parity(ParityRecording::Record, T, "ok");
        assert_eq!(
            counter_value("noetl_ehdb_crossstore_parity_total", T),
            before + 1.0,
            "positive control: the recording path must still count"
        );
    }

    /// A guard that counts CODE, not names: every metric write inside
    /// `compare_execution` must go through a gated recorder.
    ///
    /// The bug was not that one call site was wrong — it was that the comparator
    /// recorded unconditionally and two callers shared it. A future
    /// `crate::metrics::record_...` added straight into the body would rebuild
    /// exactly that, and would look perfectly ordinary in review.
    #[test]
    fn every_metric_write_in_the_comparator_is_gated() {
        let src = include_str!("ehdb_parity.rs");
        let start = src
            .find("pub async fn compare_execution(")
            .expect("comparator not found — this guard is anchored on its name");
        let body = &src[start..];
        let mut depth = 0i32;
        let mut end = body.len();
        for (i, c) in body.char_indices() {
            if c == '{' {
                depth += 1;
            } else if c == '}' {
                depth -= 1;
                if depth == 0 {
                    end = i;
                    break;
                }
            }
        }
        let body = &body[..end];
        let ungated = body.matches("crate::metrics::record_").count();
        assert_eq!(
            ungated, 0,
            "found {ungated} ungated metric write(s) inside compare_execution. Route them \
             through record_parity/record_divergence, which honour ParityRecording — \
             otherwise the HTTP endpoint writes the counters its own alert reads \
             (noetl/ai-meta#264)."
        );
        assert!(
            body.contains("record_parity(recording,"),
            "the guard must be looking at the real comparator body; it found none of the \
             gated recorders, which means the anchor moved and this test is vacuous"
        );
    }
}
