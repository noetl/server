//! The pure event type the drive core reads.
//!
//! `evaluate`/`state` consume the event log, but the server's `db::Event` is
//! `sqlx::FromRow` — native-only — so it can't compile into the wasm core.  This
//! is the db-free read-shape `evaluate` actually needs (noetl/ai-meta#109,
//! design: `orchestrate_core_event_abi.md`).
//!
//! **Named to converge.**  The field set deliberately mirrors the CQRS
//! materializer's `EventEnvelope`, under the canonical name `event::Event`, so
//! when the JetStream WAL record becomes the system's one true event
//! (noetl/ai-meta#104) this is the seed to *promote*, not a fourth shape to
//! reconcile.  The server converts its `db::Event` into this at the
//! `trigger_orchestrator` boundary via a `From` impl.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One event in an execution's log, as the drive core reads it.  A pure subset
/// of the server's `db::Event` — no DB serial `id`, `catalog_id`, `parent_event_id`,
/// `node_id`, `node_type`, or `worker_id` (the drive never reads those).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Application-side snowflake id — the drive's ordering key.
    pub event_id: i64,
    pub execution_id: i64,
    /// Catalog (playbook) id — the drive seeds WorkflowState with it.  Defaulted
    /// so a wire event that omits it still deserializes.
    #[serde(default)]
    pub catalog_id: i64,
    pub event_type: String,
    /// The step name (`node_name` in the DB / envelope).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_name: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
    /// Event-sourced timestamp — the drive reads this, never `Utc::now()`.  The
    /// DB column is `created_at`; the WAL/envelope name is `timestamp`, so accept
    /// both on the wire to converge cleanly with #104.
    #[serde(alias = "created_at", serialize_with = "serialize_timestamp_micros")]
    pub timestamp: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_execution_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<i32>,
}

/// Serialize `timestamp` truncated to whole **microseconds**.
///
/// This is the "normalise at construction" half of chain-certified projections
/// (noetl/ai-meta#366): a digest taken over the bytes the WAL stored is only
/// reproducible if those bytes are themselves deterministic. Two producers that
/// render the same logical instant with different sub-microsecond precision
/// otherwise store different bytes and therefore derive different digests —
/// the A8 failure the spec's guardrail is about.
///
/// ⚠ This is a precision reduction, and it is safe for one specific reason:
/// **Postgres already stores this column at microsecond precision**, so every
/// sub-microsecond digit is discarded the moment the event is persisted. The
/// WAL was keeping digits the database does not. Normalising here makes the two
/// agree rather than losing anything that survived a round trip — see
/// `truncation_matches_what_the_database_keeps`.
///
/// ⚠ The FORMAT is unchanged: still RFC3339, just never more than 6 fractional
/// digits. A reader on either side of this change parses both.
///
/// ⚠ Deliberately NOT feature-gated, unlike the digest itself. If the stored
/// bytes changed with the certificate flag, flipping the flag would fork the
/// chain — so the bytes must be flag-independent and the digest must be the
/// only thing the flag controls.
fn serialize_timestamp_micros<S>(ts: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use chrono::TimeZone;
    let truncated = Utc
        .timestamp_micros(ts.timestamp_micros())
        .single()
        .unwrap_or(*ts);
    truncated.serialize(serializer)
}

#[cfg(test)]
mod timestamp_normalisation_tests {
    use super::*;
    use chrono::TimeZone;

    fn ev(ts: DateTime<Utc>) -> Event {
        Event {
            event_id: 1,
            execution_id: 2,
            catalog_id: 3,
            event_type: "step.enter".into(),
            node_name: None,
            status: "success".into(),
            context: None,
            result: None,
            meta: None,
            timestamp: ts,
            parent_execution_id: None,
            attempt: None,
        }
    }

    #[test]
    fn producers_differing_below_a_microsecond_serialise_identically() {
        // THE property the stored-bytes digest depends on. Without it two
        // producers store different bytes for the same logical instant and
        // their certificates disagree (spec guardrail A8).
        let a = ev(Utc.timestamp_opt(1_790_000_000, 123_456_111).unwrap());
        let b = ev(Utc.timestamp_opt(1_790_000_000, 123_456_999).unwrap());
        assert_eq!(
            serde_json::to_vec(&a).unwrap(),
            serde_json::to_vec(&b).unwrap(),
            "timestamps differing only below a microsecond must store identical bytes"
        );
    }

    #[test]
    fn truncation_matches_what_the_database_keeps() {
        // The safety argument, asserted rather than claimed: Postgres stores
        // this column at microsecond precision, so truncating here discards
        // only digits that never survived a round trip anyway.
        let ns = Utc.timestamp_opt(1_790_000_000, 123_456_789).unwrap();
        let db_precision = Utc.timestamp_micros(ns.timestamp_micros()).single().unwrap();
        let serialised = serde_json::to_value(ev(ns)).unwrap();
        let round_tripped: Event = serde_json::from_value(serialised).unwrap();
        assert_eq!(
            round_tripped.timestamp, db_precision,
            "what we serialise must equal what the database would keep"
        );
    }

    #[test]
    fn a_whole_microsecond_timestamp_is_untouched() {
        // No silent drift for the common case: anything already at microsecond
        // precision must serialise exactly as before this change.
        let ts = Utc.timestamp_opt(1_790_000_000, 123_456_000).unwrap();
        let e = ev(ts);
        let v = serde_json::to_value(&e).unwrap();
        let back: Event = serde_json::from_value(v).unwrap();
        assert_eq!(back.timestamp, ts);
    }

    #[test]
    fn the_format_stays_rfc3339_so_old_readers_still_parse() {
        // The change must not fork the wire format — only the precision.
        let e = ev(Utc.timestamp_opt(1_790_000_000, 123_456_789).unwrap());
        let v = serde_json::to_value(&e).unwrap();
        let ts = v.get("timestamp").and_then(|t| t.as_str()).expect("a string");
        assert!(
            chrono::DateTime::parse_from_rfc3339(ts).is_ok(),
            "timestamp must stay RFC3339-parseable, got {ts}"
        );
        assert!(
            !ts.contains("123456789"),
            "sub-microsecond digits must be gone, got {ts}"
        );
    }

    #[test]
    fn deserialisation_still_accepts_sub_microsecond_input() {
        // Records written before this change carry nanosecond digits. They must
        // keep loading.
        let json = r#"{"event_id":1,"execution_id":2,"catalog_id":3,
            "event_type":"step.enter","status":"success",
            "timestamp":"2026-09-21T19:33:20.123456789Z"}"#;
        let e: Event = serde_json::from_str(json).expect("legacy record must parse");
        assert_eq!(e.timestamp.timestamp_micros(), 1_790_019_200_123_456);
    }
}
