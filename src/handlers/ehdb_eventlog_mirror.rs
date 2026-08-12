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
//! So the mirror **moves** instead. `handlers::event_write::emit_events` is the
//! one chokepoint every authoritative event passes through — including the
//! worker's own, because a worker event only reaches `noetl.event` by way of
//! `POST /api/events`, which lands in `handle_event` and calls this same
//! chokepoint. Mirroring there gives, in one place:
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

use std::time::Duration;

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
        // Provenance. A record in the tier should say which mirror wrote it —
        // without this, "the server mirror was rolled but the worker was not"
        // and "both mirrored" are the same evidence after the fact.
        "mirror_source": MirrorSource::Server.as_str(),
    })
}

/// One HTTP client for the relay, shared across calls.
fn relay_client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(reqwest::Client::new)
}

/// Mirror a batch of authoritative rows into the event-log tier.
///
/// Called from the `emit_events` chokepoint with the rows that are about to
/// become authoritative — after terminal dedup and after chain stamping, so the
/// mirrored set is exactly the set that lands in `noetl.event` and no row is
/// mirrored that gets suppressed.
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
    let execution_id = rows[0].execution_id;
    let records: Vec<String> = rows
        .iter()
        .map(|r| mirror_payload(r).to_string())
        .collect();
    let count = records.len();

    let url = format!("{}/ehdb/tiers/eventlog", base.trim_end_matches('/'));
    let body = json!({ "execution_id": execution_id.to_string(), "records": records });

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
}
