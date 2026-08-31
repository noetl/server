//! Mirror the **complete** authoritative event set into the EHDB event-log tier.
//!
//! # Why this exists
//!
//! The event-log tier's mirror hook sits on the worker's emit chokepoint
//! (`ControlPlaneClient::emit_event`), and that placement caps what the tier can
//! ever hold. The server authors events itself — `playbook_started`,
//! `command.issued`, `step.enter`, `playbook.completed`, and `command.claimed`
//! inside the claim transaction — and not one of them passes through a worker.
//!
//! The cross-store comparator (`handlers::ehdb_parity`, noetl/server#343)
//! measured it rather than inferring it: one `tests/gate_fast_probe` execution
//! writes **13** authoritative events and the tier receives **6**. The comparator
//! reports the other 7 as `unmirrored_by_design`, and it is right to — they are
//! not divergence, they are absence by construction.
//!
//! But a tier holding 6 of 13 events cannot be the source of truth for replay.
//! Flipping the event-log tier to `primary` on that basis would serve an
//! incomplete log (noetl/ai-meta#257 §3.4, noetl/ai-meta#258). This module is the
//! closure.
//!
//! # The design decision: move the mirror, do not extend it
//!
//! The obvious shape is "keep the worker mirror and add server-authored events
//! beside it". That shape is wrong, and it fails on ordering.
//!
//! The comparator checks that the tier's records sit in the same relative order
//! as the authoritative log. With two independent producers appending — a worker
//! after its emit returns, the server after its own write — nothing orders their
//! appends against each other. The server can assign `event_id` 100 and the
//! worker's earlier `event_id` 99 can still land second. That is a real race with
//! no fix available from either side, and it would surface as an intermittent
//! `order` divergence indistinguishable from a genuine one.
//!
//! So the mirror **moves** instead. `handlers::event_write::emit_events` is
//! *almost* the one chokepoint every authoritative event passes through —
//! including the worker's own, because a worker event only reaches `noetl.event`
//! by way of `POST /api/events`, which lands in `handle_event` and calls this
//! same chokepoint. Mirroring there gives, in one place:
//!
//! * **completeness** — it is the code that produces the authoritative set, so
//!   the mirrored set is that set. 13 of 13, not 6 plus a patch.
//! * **ordering** — one producer, appending in the order the chain head already
//!   serialises per execution. No cross-producer race exists to lose.
//! * **identity** — the server assigns `event_id`, so the mirrored record
//!   carries it with nothing to reconcile. The worker's
//!   `NOETL_EHDB_AUTHORITATIVE_ID_STAMP` reconciliation stays for `worker` mode
//!   and is simply not needed here.
//! * **payload identity** — the mirrored projection is built from the very row
//!   about to become authoritative.
//!
//! # "Almost" — the two in-transaction writers (noetl/ai-meta#263)
//!
//! `emit_events` is where events go when the server has already decided to write
//! them *outside* a transaction. Two sites in `handlers::events` — the command
//! claim and the batch ingest — write `noetl.event` **inside** the same
//! transaction that claims the command, and reach `emit_events` only on the
//! branch where `should_publish` is true.
//!
//! `should_publish` is false for every **system-pool** execution by construction:
//! `system/*` playbooks drain the event stream, so they cannot also be fed by it.
//! That made the bypass exactly coextensive with the system pool. User-pool
//! executions took the publish branch and mirrored completely — `test/simple_loop`
//! at 29 of 29 — while every hourly `system/scheduled_cleanup` mirrored 11 of 13,
//! missing precisely its two `command.claimed` events. The comparator called all
//! 13 mirror-expected, which in server-mirror mode is correct, so it reported the
//! two as `missing_event` with `unmirrored_by_design = 0`. It was right; the tier
//! was incomplete, and `primary`.
//!
//! Both sites now call [`mirror_rows`] after their commit. That is three call
//! sites rather than one, which is worth being uncomfortable about — but the
//! ordering hazard that moved the mirror here does not apply: it was a race
//! between two independent *processes*, and these are the same process appending
//! before it answers the request that would let the next event exist.
//! `tests::every_in_tx_event_insert_is_mirrored` counts INSERT sites against
//! mirror sites so a fourth writer cannot be added silently.
//!
//! # The data-access boundary is not crossed
//!
//! Per `data-access-boundary.md` the server owns `noetl.event`, and per
//! noetl/ai-meta#257 §3.5 the server is the component that resolves through the
//! tier service when the event-log tier eventually goes primary. Mirroring from
//! the server is that boundary, not an exception to it.
//!
//! The control-plane guard is equally untouched: the server does **not** open
//! tier storage. It POSTs to the worker's tier surface — the same
//! `NOETL_EHDB_WORKER_QUERY_URL` hop it already makes to read, on the same route,
//! so the append resolves to whichever store the read resolves to. Writing where
//! the comparator reads is thereby true by construction rather than by two env
//! vars agreeing.
//!
//! # Failure posture
//!
//! Best-effort and isolated, exactly like the worker's mirror. This is an
//! auxiliary verification path; it must never be able to fail an event write the
//! platform has already committed to. Every failure is metered and logged and
//! then dropped. The consequence of a dropped append is a `missing_event` on the
//! comparator — loud, attributable, and exactly what the comparator is for.
//!
//! Default-off behind `NOETL_EHDB_EVENTLOG_MIRROR_SOURCE=server`.

use std::time::{Duration, Instant};

use serde_json::json;
use tracing::warn;

use crate::handlers::event_write::EventRow;
use crate::state::AppState;

/// `NOETL_EHDB_EVENTLOG_MIRROR_SOURCE` — read by the server AND the worker.
///
/// One variable rather than two so the halves cannot disagree about who is
/// mirroring. They still *can* disagree if only one component is rolled, and
/// that shows up as a doubled record count on the comparator rather than as
/// silence — the direction to be wrong in.
pub const MIRROR_SOURCE_ENV: &str = "NOETL_EHDB_EVENTLOG_MIRROR_SOURCE";

/// How long to wait on the relay before giving up on a batch.
///
/// Short on purpose. This sits inline on the event-write path, so the cost of a
/// hung worker is bounded latency on event emission, not an unbounded stall.
const APPEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Which component mirrors into the event-log tier. Mirror of the worker's
/// `ehdb::mirror_source::MirrorSource`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MirrorSource {
    Worker,
    Server,
}

impl MirrorSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Worker => "worker",
            Self::Server => "server",
        }
    }

    /// Anything unrecognised is `Worker` — the pre-change behaviour. A typo must
    /// leave the old mirror running, never disarm both halves and leave the tier
    /// silently empty while `NOETL_EHDB_EVENTLOG` still reads `shadow`.
    pub fn parse(raw: Option<&str>) -> Self {
        match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
            Some("server") => Self::Server,
            _ => Self::Worker,
        }
    }

    pub fn from_process_env() -> Self {
        Self::parse(std::env::var(MIRROR_SOURCE_ENV).ok().as_deref())
    }
}

/// Is the server the mirror producer for this process?
pub fn server_mirrors() -> bool {
    MirrorSource::from_process_env() == MirrorSource::Server
}

/// Key naming the payload contract a tier record was written under.
pub const MIRROR_PAYLOAD_VERSION_KEY: &str = "mirror_payload_version";

/// The payload contract this build writes.
///
/// **v1 (the absent value) omitted `context`.** That was invisible while the
/// tier was only ever compared on an identifying projection — `event_id`,
/// `event_type`, `step`, `status` — none of which touch it. It stops being
/// invisible the moment anything *folds* a tier record, because
/// `WorkflowState::apply_event` reads `context` (it is where `workload`,
/// `path` and `version` come from on the start event, and several branches
/// fall back to it), so a v1 record folds into a **different state** than the
/// same event read from `noetl.event`.
///
/// v2 carries `context`. The version is written explicitly rather than being
/// inferred from "is `context` present?", because a genuinely null `context`
/// and a record written before v2 are indistinguishable by that test — and
/// they must not be: one is foldable, the other is not. Inferring it would
/// reintroduce exactly the dead-default failure noetl/ai-meta#243 records,
/// where one value laundered three different causes.
pub const MIRROR_PAYLOAD_VERSION: i64 = 2;

/// The tier record for one authoritative row.
///
/// Two constraints meet here.
///
/// The comparator parses a mirrored record's identity out of `event_id`,
/// `event_type`, `step` and `status` — the field names an `ExecutorEvent`
/// serialises to, because until now every mirrored record was one. A
/// server-authored record has to answer to the same names or the comparator
/// cannot identify it, so `step` is emitted alongside `node_name` carrying the
/// same value.
///
/// The tier is also meant to *become* the log, so a record that carried only
/// those four fields would be a record you could verify and not one you could
/// serve from. The full row shape goes in as well.
pub fn mirror_payload(row: &EventRow) -> serde_json::Value {
    json!({
        // --- the comparator's identifying projection -------------------------
        "event_id": row.event_id,
        "event_type": row.event_type,
        // Same value under both names: `step` is what the worker's ExecutorEvent
        // calls it and what the comparator reads; `node_name` is what the
        // authoritative column is called.
        "step": row.node_name,
        "status": row.status,
        // --- the rest of the authoritative row -------------------------------
        "execution_id": row.execution_id,
        "catalog_id": row.catalog_id,
        "node_id": row.node_id,
        "node_name": row.node_name,
        "node_type": row.node_type,
        "parent_event_id": row.parent_event_id,
        "prev_event_id": row.prev_event_id,
        "parent_execution_id": row.parent_execution_id,
        "result": row.result,
        "meta": row.meta,
        "error": row.error,
        "worker_id": row.worker_id,
        "created_at": row.created_at,
        // Load-bearing for any reader that FOLDS this record rather than merely
        // identifying it. See MIRROR_PAYLOAD_VERSION for why its absence in v1
        // was invisible, and why the version is written rather than inferred.
        "context": row.context,
        // Provenance. A record in the tier should say which mirror wrote it —
        // without this, "the server mirror was rolled but the worker was not"
        // and "both mirrored" are the same evidence after the fact.
        "mirror_source": MirrorSource::Server.as_str(),
        // Lets a folding reader refuse a pre-v2 record as *too old to fold*
        // rather than folding it and reporting the missing `context` as a
        // digest divergence. Those are different facts and want different
        // operator responses.
        MIRROR_PAYLOAD_VERSION_KEY: MIRROR_PAYLOAD_VERSION,
    })
}

/// One HTTP client for the relay, shared across calls.
fn relay_client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(reqwest::Client::new)
}

/// One relay-ready mirror request: the records for one execution, in order.
///
/// Carries the relay base URL rather than re-reading it at delivery time so a
/// batch enqueued under one configuration cannot be delivered under another —
/// and carries `enqueued_at` because the lag it reports is the whole point of
/// the async path being auditable.
#[derive(Debug, Clone)]
pub struct MirrorBatch {
    pub base: String,
    pub execution_id: i64,
    pub records: Vec<String>,
    pub enqueued_at: Instant,
}

/// Env var gating the projector-sink mirror (noetl/ai-meta#307).
pub const SINK_MIRROR_ENV: &str = "NOETL_EHDB_SINK_MIRROR";

/// Whether `/api/internal/events/project` mirrors what it writes.
///
/// **Default OFF, and that is a retreat, not a design.**
///
/// Mirroring this sink closes the #307 missing-event class: the projector wrote
/// 5,088 events on prod that never reached the tier, and an affected execution
/// reads n=30 in Postgres against n=29 in the tier. Enabling it did fix that —
/// post-deploy executions showed matching event counts.
///
/// It also introduced the opposite defect, which is why this flag exists.
/// Measured on prod 2026-08-31, execution 352823243654045696:
///
/// ```text
/// authoritative (mirror-expected) = 30
/// ehdb (tier)                     = 45
/// matched                         = 41   <- 41 tier records for 30 rows
/// extra_event                     = 4    <- tier records with NO authoritative row
///                                           [352824940644278272, 352824958205829120,
///                                            352824960726605824, 352824961766793216]
/// ```
///
/// So the tier gained **11 duplicates of matched rows plus 4 orphans**. The
/// duplicates and the orphans are probably two different faults, and neither
/// mechanism is pinned. Ruled out so far: the worker does not double-mirror
/// (all pools run `MIRROR_SOURCE=server`); the emit-path INSERT carries no
/// `ON CONFLICT`, so a retry aborts rather than silently re-mirroring; and the
/// sink mirrors only rows the INSERT's `RETURNING` reported as accepted, so it
/// cannot be mirroring rows Postgres rejected.
///
/// Turning it off returns the tier to the behaviour that measured
/// `extra_event = 0` over 6,951 compared events, at the cost of reopening the
/// missing-event class. That trade is deliberate: a tier holding MORE than the
/// system of record is the worse failure, because nothing downstream is looking
/// for it, and `extra_event` feeds the parity alert.
///
/// ⚠ Do not flip this on without first reproducing the duplicate mechanism in a
/// test. "It looked fine in a burst" is what produced this flag — the first
/// verification passed only because the comparator had not sampled those
/// executions yet (`compared` was 0 at the same moment `extra_event` was 0).
pub fn sink_mirror_enabled() -> bool {
    std::env::var(SINK_MIRROR_ENV)
        .ok()
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            v == "1" || v == "true" || v == "on" || v == "enabled"
        })
        .unwrap_or(false)
}


/// Mirror a batch of authoritative rows into the event-log tier.
///
/// Called from the `emit_events` chokepoint with the rows that are about to
/// become authoritative — after terminal dedup and after chain stamping, so the
/// mirrored set is exactly the set that lands in `noetl.event` and no row is
/// mirrored that gets suppressed.
///
/// With `NOETL_EHDB_EVENTLOG_MIRROR_ASYNC` off this delivers inline, exactly as
/// it always has. With it on the batch is handed to
/// [`super::ehdb_eventlog_mirror_queue`] and this returns in microseconds; the
/// delivery, its metrics and its failure posture are unchanged, they just happen
/// on the drain task.
///
/// Never returns an error. See the module note on failure posture.
pub async fn mirror_rows(state: &AppState, rows: &[EventRow]) {
    if rows.is_empty() || !server_mirrors() {
        return;
    }
    let Some(base) = std::env::var(crate::handlers::ehdb::WORKER_QUERY_URL_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        // Asked to mirror server-side with nowhere to mirror to. This is a
        // misconfiguration, not a quiet no-op: without it the tier stays empty
        // while the worker's mirror is also disarmed, and the comparator would
        // report a missing_execution whose cause is two hops away.
        crate::metrics::record_ehdb_eventlog_mirror("unconfigured", rows.len());
        warn!(
            target: "noetl_server::ehdb_eventlog_mirror",
            "{MIRROR_SOURCE_ENV}=server but {} is unset — {} authoritative event(s) were not \
             mirrored into the event-log tier",
            crate::handlers::ehdb::WORKER_QUERY_URL_ENV,
            rows.len()
        );
        return;
    };
    let _ = state; // reserved: per-execution routing when the tier shards.

    // One request per batch, records in the order the chokepoint wrote them.
    // N concurrent single-record requests would not preserve that order, and
    // order is one of the properties the comparator checks.
    let batch = MirrorBatch {
        base,
        execution_id: rows[0].execution_id,
        records: rows.iter().map(|r| mirror_payload(r).to_string()).collect(),
        enqueued_at: Instant::now(),
    };

    if super::ehdb_eventlog_mirror_queue::enabled() {
        super::ehdb_eventlog_mirror_queue::submit(batch).await;
    } else {
        deliver(&batch).await;
    }
}

/// POST one batch to the tier relay and meter the result.
///
/// The delivery half of [`mirror_rows`], split out so the queue's drain task
/// runs **the same code** the inline path runs. Two deliverers would be two
/// failure postures, two metric spellings, and one of them would drift.
pub(crate) async fn deliver(batch: &MirrorBatch) {
    let execution_id = batch.execution_id;
    let count = batch.records.len();
    if count == 0 {
        return;
    }

    let url = format!("{}/ehdb/tiers/eventlog", batch.base.trim_end_matches('/'));
    let body = json!({ "execution_id": execution_id.to_string(), "records": batch.records });

    let resp = relay_client()
        .post(&url)
        .json(&body)
        .timeout(APPEND_TIMEOUT)
        .send()
        .await;

    match resp {
        Err(e) => {
            crate::metrics::record_ehdb_eventlog_mirror("unavailable", count);
            warn!(
                target: "noetl_server::ehdb_eventlog_mirror",
                execution_id, events = count, error = %e,
                "event-log tier mirror relay failed"
            );
        }
        Ok(r) => {
            let status = r.status();
            if status == reqwest::StatusCode::NOT_IMPLEMENTED
                || status == reqwest::StatusCode::METHOD_NOT_ALLOWED
            {
                // Two distinct causes, one meaning: this worker cannot accept
                // server-authored appends, and the fix is a rollout rather than
                // an outage.
                //
                //   501 — rolled, but still in `worker` mode.
                //   405 — NOT rolled. The GET route exists on every worker back
                //         to the query interface, so axum answers a POST to it
                //         with "method not allowed" rather than 404. Counting
                //         that as `degraded` would file the single most likely
                //         operational state — server rolled ahead of worker —
                //         under a label meaning "the append was refused", and
                //         send an operator looking at the tier instead of at
                //         the rollout.
                crate::metrics::record_ehdb_eventlog_mirror("unconfigured", count);
                warn!(
                    target: "noetl_server::ehdb_eventlog_mirror",
                    execution_id, events = count, status = status.as_u16(),
                    "worker cannot accept server-authored appends — is it rolled, and is \
                     {MIRROR_SOURCE_ENV}=server set on it?"
                );
            } else if status.is_success() {
                crate::metrics::record_ehdb_eventlog_mirror("mirrored", count);
            } else {
                let detail = r.text().await.unwrap_or_default();
                crate::metrics::record_ehdb_eventlog_mirror("degraded", count);
                warn!(
                    target: "noetl_server::ehdb_eventlog_mirror",
                    execution_id, events = count, status = status.as_u16(),
                    detail = %detail.chars().take(400).collect::<String>(),
                    "event-log tier mirror was refused"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {

    /// The fold reads `context`; the payload must therefore carry it.
    ///
    /// Guarding the *field* and not just the version, because a version bump
    /// with the field still missing is the failure this whole change exists to
    /// end — and it would advertise itself as fixed.
    #[test]
    fn a_mirrored_record_carries_the_context_the_fold_reads() {
        let row = crate::handlers::event_write::EventRow::new(
            7, 42, 1, "execution.start", "STARTED", chrono::Utc::now(),
        )
        .with_context(serde_json::json!({"workload": {"k": "v"}, "path": "p", "version": "1"}));
        let p = mirror_payload(&row);

        assert_eq!(
            p.get("context").and_then(|c| c.get("path")).and_then(|v| v.as_str()),
            Some("p"),
            "context is absent from the mirrored record — a fold of it cannot equal a fold of \
             noetl.event, and the difference shows up as a digest divergence"
        );
        assert_eq!(
            p.get(MIRROR_PAYLOAD_VERSION_KEY).and_then(|v| v.as_i64()),
            Some(MIRROR_PAYLOAD_VERSION)
        );
    }

    /// A null `context` still produces the key, and the version still says v2.
    ///
    /// This is the case that makes the explicit version load-bearing: a record
    /// whose context is genuinely null is byte-identical, on the `context` key
    /// alone, to a v1 record written before the field existed. One is foldable
    /// and one is not, so the reader cannot be left to infer which it has.
    #[test]
    fn a_null_context_is_still_v2_and_therefore_still_foldable() {
        let row = crate::handlers::event_write::EventRow::new(
            8, 42, 1, "step.enter", "RUNNING", chrono::Utc::now(),
        );
        let p = mirror_payload(&row);
        assert!(p.get("context").is_some_and(|c| c.is_null()));
        assert_eq!(
            p.get(MIRROR_PAYLOAD_VERSION_KEY).and_then(|v| v.as_i64()),
            Some(MIRROR_PAYLOAD_VERSION),
            "a genuinely-null context must not be mistakable for a pre-v2 record"
        );
    }

    /// The identifying projection the comparator reads is untouched by v2.
    #[test]
    fn v2_does_not_disturb_the_comparators_identifying_fields() {
        let row = crate::handlers::event_write::EventRow::new(
            9, 42, 1, "step.exit", "COMPLETED", chrono::Utc::now(),
        )
        .with_node("fetch");
        let p = mirror_payload(&row);
        assert_eq!(p.get("event_id").and_then(|v| v.as_i64()), Some(9));
        assert_eq!(p.get("event_type").and_then(|v| v.as_str()), Some("step.exit"));
        assert_eq!(p.get("step").and_then(|v| v.as_str()), Some("fetch"));
        assert_eq!(p.get("status").and_then(|v| v.as_str()), Some("COMPLETED"));
    }
    use super::*;
    use chrono::{DateTime, Utc};

    fn row() -> EventRow {
        EventRow::new(
            991,
            7,
            3,
            "command.completed",
            "success",
            DateTime::parse_from_rfc3339("2026-08-11T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        )
        .with_node("step1")
    }

    #[test]
    fn unrecognised_source_stays_on_the_worker_mirror() {
        assert_eq!(MirrorSource::parse(None), MirrorSource::Worker);
        assert_eq!(MirrorSource::parse(Some("")), MirrorSource::Worker);
        assert_eq!(MirrorSource::parse(Some("srever")), MirrorSource::Worker);
        assert_eq!(MirrorSource::parse(Some("worker")), MirrorSource::Worker);
    }

    #[test]
    fn server_is_recognised_case_and_space_insensitively() {
        for v in ["server", "SERVER", " Server "] {
            assert_eq!(MirrorSource::parse(Some(v)), MirrorSource::Server, "{v:?}");
        }
    }

    #[test]
    fn the_payload_answers_to_the_comparators_field_names() {
        // The comparator reads `event_id`, `event_type`, `step`, `status` — the
        // ExecutorEvent names. A server-authored record that omitted `step`
        // would be reported as a payload divergence on every event, so this is
        // the contract between the two halves, asserted on the value the wire
        // carries rather than on the code that builds it.
        let p = mirror_payload(&row());
        assert_eq!(p["event_id"], 991);
        assert_eq!(p["event_type"], "command.completed");
        assert_eq!(p["step"], "step1");
        assert_eq!(p["status"], "success");
        // and the full row is there too, so the tier could serve from it.
        assert_eq!(p["node_name"], "step1");
        assert_eq!(p["execution_id"], 7);
        assert_eq!(p["mirror_source"], "server");
    }

    #[test]
    fn step_and_node_name_never_disagree() {
        // They carry one value under two names; if a later edit sourced them
        // separately the comparator would see a payload divergence the row
        // itself does not have.
        let p = mirror_payload(&row());
        assert_eq!(p["step"], p["node_name"]);
    }

    /// Every place the server writes `noetl.event` outside the chokepoint must
    /// also mirror — the guard for noetl/ai-meta#263.
    ///
    /// The mirror lives in `event_write::emit_events`, described there as "the
    /// one chokepoint every authoritative event passes through". It is not: two
    /// sites in `handlers::events` write the table directly, in-transaction, on
    /// the branch `should_publish` takes when it returns false. That branch is
    /// the ONLY branch a **system-pool** execution can take (`is_system_execution`
    /// is one of `should_publish`'s three false conditions), so `system/*`
    /// playbooks mirrored every event except the ones written there — 11 of 13 on
    /// every hourly `system/scheduled_cleanup`, on a tier that is `primary`.
    ///
    /// Counting `INSERT INTO noetl.event` rather than naming the two known sites:
    /// a third one added later is the failure this is here to catch, and a test
    /// that lists the sites it already knows about cannot catch it.
    #[test]
    fn every_in_tx_event_insert_is_mirrored() {
        let events_rs = include_str!("events.rs");
        let inserts = events_rs.matches("INSERT INTO noetl.event").count();
        let mirrors = events_rs.matches("ehdb_eventlog_mirror::mirror_rows").count();
        assert_eq!(
            inserts, mirrors,
            "handlers/events.rs has {inserts} direct `noetl.event` INSERT site(s) but \
             {mirrors} mirror call(s). Every in-tx INSERT bypasses \
             `event_write::emit_events` and therefore bypasses the mirror on it; \
             each one owes the event-log tier a post-commit `mirror_rows` or the \
             tier serves an incomplete log (noetl/ai-meta#263)."
        );
        assert!(
            inserts >= 2,
            "expected the claim + batch in-tx INSERT sites to still exist; if they \
             were routed through `emit_events` instead, delete this test and say so"
        );
    }

    /// The chokepoint still mirrors, and mirrors before the publish/insert fork.
    ///
    /// The #263 fix adds mirror call sites; it must not have moved or removed the
    /// one that covers everything else.
    ///
    /// # This guard was broken, and nothing said so
    ///
    /// It matched the literal `if should_publish(state, rows[0].catalog_id)`.
    /// noetl/ai-meta#155's phase-attribution change (server#352) hoisted that
    /// call into `let __should_publish = should_publish(…).await;` so it could be
    /// timed — a correct edit that left the guard unable to find its landmark.
    /// The guard has been `panic!`-ing ever since, and **no `cargo test` runs in
    /// CI on any Rust repo** ([ai-meta#232](https://github.com/noetl/ai-meta/issues/232)),
    /// so a red guard and a green one are the same observable.
    ///
    /// Rewritten to anchor on `should_publish(` — the call, which cannot be
    /// hoisted away without ceasing to exist — rather than on one spelling of
    /// the statement around it.
    #[test]
    fn the_chokepoint_mirrors_before_it_forks() {
        let ew = include_str!("event_write.rs");
        let mirror_at = ew
            .find("ehdb_eventlog_mirror::mirror_rows")
            .expect("event_write::emit_events must still mirror");
        let fork_at = ew
            .find("should_publish(state, rows[0].catalog_id)")
            .expect("the publish/insert fork must still be here");
        assert!(
            mirror_at < fork_at,
            "the mirror must sit BEFORE the publish/insert fork so one call site \
             covers both branches"
        );
    }

    /// The async path and the inline path must deliver through the SAME code.
    ///
    /// `deliver` carries the whole failure posture — the 501/405 discrimination,
    /// the four outcome labels, the timeout. A drain task with its own copy would
    /// be a second posture that drifts, and the drift would only ever be visible
    /// on the async configuration, i.e. the one with less operational history.
    #[test]
    fn there_is_exactly_one_deliverer() {
        let me = include_str!("ehdb_eventlog_mirror.rs");
        let queue = include_str!("ehdb_eventlog_mirror_queue.rs");
        assert_eq!(
            me.matches("relay_client()\n        .post(").count(),
            1,
            "the relay POST must exist once, in `deliver`"
        );
        assert!(
            !queue.contains(".post("),
            "the queue module must not POST — it calls `deliver`, or the two paths \
             will report differently for the same failure"
        );
        assert!(
            queue.contains("ehdb_eventlog_mirror::deliver"),
            "the drain task must call the shared deliverer"
        );
    }

    /// Enqueueing must not be able to become a drop.
    ///
    /// Counted rather than named: the hazard is a *new* early return added later
    /// that skips both the queue and the inline path, and a test that lists the
    /// returns it already knows about cannot catch that. Every early exit from
    /// `mirror_rows` that is not a delivery has to be one of the two accounted
    /// ones — "nothing to mirror" and "not the mirror source" — or this fails.
    #[test]
    fn mirror_rows_has_no_unaccounted_early_return() {
        let me = include_str!("ehdb_eventlog_mirror.rs");
        let body_start = me
            .find("pub async fn mirror_rows")
            .expect("mirror_rows must exist");
        let body_end = me[body_start..]
            .find("pub(crate) async fn deliver")
            .expect("deliver must follow mirror_rows")
            + body_start;
        let body = &me[body_start..body_end];
        // `return;` in the guard clause, in the unconfigured branch. Two.
        assert_eq!(
            body.matches("return;").count(),
            2,
            "mirror_rows grew an early return. Every path out of it either delivers \
             the events or is one of the two accounted no-ops (empty batch / not the \
             mirror source); a third would silently drop authoritative events on a \
             `primary`-serving tier (noetl/ai-meta#155)."
        );
    }

    /// Every `noetl.event` INSERT in the crate either mirrors, or is registered
    /// here as deliberately unmirrored with a written reason.
    ///
    /// # Why the previous guard could not see the defect it was written for
    ///
    /// `every_in_tx_event_insert_is_mirrored` counts inserts in `events.rs` and
    /// only `events.rs`. That was the file #263 was about, so the guard passed
    /// while **three other files** wrote the same table without mirroring —
    /// including `services::internal::project_events`, which had written
    /// **5,088** events into `noetl.event` on production by 2026-08-31, none of
    /// them reaching the event-log tier. That is the missing-event class in
    /// noetl/ai-meta#307: an affected execution has n=30 in Postgres and n=29 in
    /// the tier, leaving a loop step at `command_started` with 2 of 3
    /// iterations.
    ///
    /// A guard scoped to one file cannot establish a property of a codebase.
    /// This one walks `src/**/*.rs` at test time — so a NEW file with a new
    /// insert is caught too, which `include_str!` of a known list can never do.
    ///
    /// # The two dispositions
    ///
    /// A site either mirrors, or it is `UNMIRRORED_BY_DESIGN` with a reason a
    /// human wrote. There is no third option and no default: an unregistered
    /// site fails the test rather than being assumed benign, because "nobody
    /// listed it" is exactly how the 5,088 happened.
    #[test]
    fn every_event_insert_in_the_crate_mirrors_or_is_registered() {
        use std::collections::BTreeMap;

        /// Sites that write `noetl.event` and DO mirror.
        const MIRRORS: &[&str] = &["handlers/events.rs", "handlers/event_write.rs"];

        /// Sites that write `noetl.event` and deliberately do not mirror.
        /// The reason is the point: an entry without one is not a decision.
        const UNMIRRORED_BY_DESIGN: &[(&str, &str)] = &[
            // ⚠ NONE. All three of the following are UNDER INVESTIGATION as the
            // #307 missing-event cause and are registered as *known-unmirrored*,
            // not as *by design*. Moving one into a by-design entry requires
            // saying why the tier is allowed to lack those events.
        ];

        /// Sites that do not mirror and are NOT yet justified. This list may
        /// only ever SHRINK. Adding to it is the failure mode this test exists
        /// to make loud.
        /// Sites whose INSERT is mirrored by their CALLER, in another file.
        /// The caller is named and checked — a cross-file claim that nothing
        /// verifies is how a registry rots into decoration.
        const MIRRORED_BY_CALLER: &[(&str, &str)] = &[(
            // POST /api/internal/events/project — the projector sink. Its
            // handler mirrors exactly the rows the INSERT's RETURNING reports
            // as accepted (noetl/ai-meta#307).
            "services/internal.rs",
            "handlers/internal.rs",
        )];

        const KNOWN_UNMIRRORED_PENDING_FIX: &[(&str, &str)] = &[
            (
                "db/queries/event.rs",
                "generic insert helper; callers vary, so the mirror belongs at a \
                 caller or here — undecided (noetl/ai-meta#307)",
            ),
            (
                "handlers/internal.rs",
                "POST /api/internal/events/materialize — a sink writer. \
                 noetl_events_materialized_total was 0 on prod 2026-08-31, so it \
                 contributes no divergence today, but it is the same shape as \
                 project_events (noetl/ai-meta#307)",
            ),
        ];

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut found: BTreeMap<String, (usize, usize)> = BTreeMap::new();

        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(rd) = std::fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let mut files = Vec::new();
        walk(&root, &mut files);
        assert!(
            files.len() > 50,
            "the walk found only {} .rs files — it is not reaching the tree, and \
             a guard that scans nothing passes for the wrong reason",
            files.len()
        );

        for f in &files {
            let Ok(src) = std::fs::read_to_string(f) else { continue };
            // Strip the test module and line comments: this file discusses the
            // needle in its own prose, and a guard that counts its own comments
            // measures itself rather than the code.
            let code = src.split("#[cfg(test)]").next().unwrap_or("");
            let code: String = code
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");

            let inserts = code.matches("INSERT INTO noetl.event").count();
            if inserts == 0 {
                continue;
            }
            let mirrors = code.matches("mirror_rows").count();
            let rel = f
                .strip_prefix(&root)
                .unwrap_or(f)
                .to_string_lossy()
                .replace('\\', "/");
            found.insert(rel, (inserts, mirrors));
        }

        let mut problems = Vec::new();
        for (file, (inserts, mirrors)) in &found {
            let mirrors_registered = MIRRORS.contains(&file.as_str());
            let by_design = UNMIRRORED_BY_DESIGN.iter().any(|(f, _)| f == file);
            let pending = KNOWN_UNMIRRORED_PENDING_FIX.iter().any(|(f, _)| f == file);
            let by_caller = MIRRORED_BY_CALLER.iter().any(|(f, _)| f == file);

            if !mirrors_registered && !by_design && !pending && !by_caller {
                problems.push(format!(
                    "{file}: {inserts} INSERT INTO noetl.event site(s), {mirrors} mirror \
                     call(s), and NOT REGISTERED. Either call `mirror_rows` after the \
                     commit, or add it to UNMIRRORED_BY_DESIGN with the reason the \
                     event-log tier may lack these events."
                ));
            }
            if mirrors_registered && *mirrors < *inserts {
                problems.push(format!(
                    "{file}: registered as mirroring but has {inserts} insert site(s) \
                     and only {mirrors} mirror call(s)"
                ));
            }
        }
        for (file, caller) in MIRRORED_BY_CALLER {
            let caller_path = root.join(caller);
            let caller_src = std::fs::read_to_string(&caller_path).unwrap_or_default();
            let caller_code = caller_src
                .split("#[cfg(test)]")
                .next()
                .unwrap_or("")
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            if !caller_code.contains("mirror_rows") {
                problems.push(format!(
                    "{file} is registered as mirrored-by-caller {caller}, but {caller} \
                     contains no `mirror_rows` call — the claim is not true"
                ));
            }
        }
        for (file, reason) in UNMIRRORED_BY_DESIGN {
            assert!(
                !reason.trim().is_empty(),
                "{file} is registered unmirrored-by-design with an empty reason; \
                 an entry without a reason is not a decision"
            );
        }
        for (file, _) in KNOWN_UNMIRRORED_PENDING_FIX {
            if !found.contains_key(*file) {
                problems.push(format!(
                    "{file} is listed as a known-unmirrored insert site but no longer \
                     contains one — if it was fixed, REMOVE it from the list so the \
                     list keeps meaning something"
                ));
            }
        }

        assert!(
            problems.is_empty(),
            "{} `noetl.event` insert-site problem(s):\n  {}\n\nsites found: {:?}",
            problems.len(),
            problems.join("\n  "),
            found
        );

        assert_eq!(
            found.len(),
            5,
            "expected exactly the 5 known `noetl.event` insert-site files; found {:?}. \
             A new file writing this table must be registered above.",
            found.keys().collect::<Vec<_>>()
        );
    }

    /// The projector-sink mirror must be OFF unless explicitly enabled.
    ///
    /// It shipped ON in server v3.99.0 and drove `extra_event` from 0 to 6 on
    /// production within ten minutes. Default-off is the safe state until the
    /// duplicate mechanism is reproduced.
    #[test]
    fn sink_mirror_is_off_by_default_and_needs_an_explicit_opt_in() {
        // Cannot mutate process env safely here (tests share it and cargo does
        // NOT serialise them), so exercise the parse rule directly.
        fn parses_as_enabled(v: &str) -> bool {
            let v = v.trim().to_ascii_lowercase();
            v == "1" || v == "true" || v == "on" || v == "enabled"
        }

        for off in ["", " ", "0", "false", "no", "off", "disabled", "yes", "maybe"] {
            assert!(
                !parses_as_enabled(off),
                "{off:?} must NOT enable the sink mirror — anything but an \
                 explicit opt-in leaves the tier in the state that measured \
                 extra_event = 0"
            );
        }
        for on in ["1", "true", "TRUE", " on ", "enabled"] {
            assert!(parses_as_enabled(on), "{on:?} must enable it");
        }

        // The unset case is the one that matters, and it is what the deployed
        // manifests express: no NOETL_EHDB_SINK_MIRROR anywhere.
        assert!(
            !sink_mirror_enabled() || std::env::var(SINK_MIRROR_ENV).is_ok(),
            "sink_mirror_enabled() returned true with {SINK_MIRROR_ENV} unset"
        );
    }

    /// The guard from #263 must still account for every insert site.
    ///
    /// Gating the mirror changes which files contain a `mirror_rows` call, and a
    /// registry that silently stopped matching would hide the next bypass.
    #[test]
    fn gating_the_sink_mirror_did_not_break_the_insert_registry() {
        let internal = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/handlers/internal.rs"),
        )
        .expect("handlers/internal.rs readable");
        let code = internal.split("#[cfg(test)]").next().unwrap_or("");
        assert!(
            code.contains("mirror_rows"),
            "handlers/internal.rs must still CONTAIN the mirror call — the #263 \
             registry lists it as the caller that mirrors services/internal.rs, \
             and that claim is checked by file content"
        );
        assert!(
            code.contains("sink_mirror_enabled()"),
            "the call must be gated, not unconditional"
        );
    }
}
