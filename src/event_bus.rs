//! **L1 T3 — the EHDB events bus (flag-gated).**
//!
//! Selects the transport that carries `noetl.event` payloads to their four
//! consumers, behind [`NOETL_EVENT_BUS`](EventBusMode). The events sibling of
//! [`crate::command_bus`], and deliberately the same shape: `nats` (default)
//! leaves today's path untouched, `shadow` publishes to **both** with NATS
//! authoritative, `ehdb` publishes to EHDB only.
//!
//! **What is actually at stake here.** The server runs with
//! `NOETL_EVENT_INGEST_PUBLISH_ONLY=true`, so it writes **zero** `noetl.event`
//! rows itself — every event publishes to the stream, and the worker-side
//! `noetl_materializer` draining it is the *sole writer* of the durable event
//! log. This bus therefore carries the write path of the platform's
//! append-only source of truth, not just the SPA's live updates. A dropped
//! event here is a hole in the audit log, which is why `shadow` exists and why
//! the cutover is per-consumer (noetl/ai-meta#212).
//!
//! **Publish semantics differ from the command bus in one way that matters.**
//! A command is claimed by exactly one worker, so a duplicate is cheap — the
//! second claimer is told it is already claimed. An event fans out to every
//! consumer, and a duplicate becomes a duplicate `noetl.event` row unless
//! something dedupes it. NATS gets that from the `Nats-Msg-Id` dedup window;
//! on the EHDB side the `event_id` rides in the record so the materializer's
//! `events/project` call (keyed on `event_id`) collapses it. Retry is still
//! at-least-once, because losing an event is worse than repeating one.

use std::collections::BTreeMap;

use crate::command_bus::{parse_writer_addrs, EhdbCommandPublisher};

/// Which transport carries event payloads (env `NOETL_EVENT_BUS`).
///
/// There is deliberately **no `Default`**, for the same reason
/// [`CommandBusMode`](crate::command_bus::CommandBusMode) has none: a default
/// here would have to name a transport, and the only transport that exists is
/// EHDB — so defaulting means guessing (noetl/ai-meta#243).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventBusMode {
    /// Publish to NATS only.
    ///
    /// ⚠ NATS was deleted at T5 (noetl/ai-meta#194). The variant survives so a
    /// stale `NOETL_EVENT_BUS=nats` produces a specific, actionable error rather
    /// than a generic parse failure — and because `publishes_nats()` still
    /// distinguishes `Shadow` from `Ehdb` on the write path. It is not
    /// selectable.
    Nats,
    /// Publish to the per-shard EHDB events writer only — the cutover.
    Ehdb,
    /// Publish to both: NATS authoritative, EHDB mirrored for parity comparison.
    Shadow,
}

impl EventBusMode {
    /// Parse the `NOETL_EVENT_BUS` value. Required — there is no default.
    ///
    /// The comment this replaces said a typo "must never silently stop publishing
    /// to NATS". That property inverted when T5 deleted NATS: falling back to
    /// `nats` no longer keeps a working path, it selects a transport that does not
    /// exist. The old catch-all then laundered three different situations into one
    /// outcome — **unset**, **a typo**, and **a stale `nats`** all became
    /// `EventBusMode::Nats`, a value nothing sets on purpose, with no error and a
    /// server that starts clean while writing the durable event log nowhere.
    ///
    /// Each failure mode now names itself, matching
    /// [`CommandBusMode::from_env_value`](crate::command_bus::CommandBusMode::from_env_value)
    /// so an operator who has flipped the command bus does not learn a second
    /// vocabulary (noetl/ai-meta#243).
    pub fn from_env_value(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "ehdb" => Ok(Self::Ehdb),
            "shadow" => Ok(Self::Shadow),
            "nats" => Err(
                "NOETL_EVENT_BUS=nats selects a transport that no longer exists — NATS was \
                 removed at T5 (noetl/ai-meta#212). Set NOETL_EVENT_BUS=ehdb."
                    .to_string(),
            ),
            "" => Err(
                "NOETL_EVENT_BUS is required and unset. There is no default: the only transport \
                 is EHDB, and guessing would mean starting a server that writes the durable \
                 event log nowhere while looking healthy. Set NOETL_EVENT_BUS=ehdb."
                    .to_string(),
            ),
            other => Err(format!(
                "NOETL_EVENT_BUS={other:?} is not a known transport. Valid values: ehdb, \
                 shadow. (nats was removed at T5.)"
            )),
        }
    }

    /// Whether this mode publishes to the EHDB events feed.
    pub fn publishes_ehdb(self) -> bool {
        matches!(self, Self::Ehdb | Self::Shadow)
    }

    /// Whether this mode publishes to NATS.
    pub fn publishes_nats(self) -> bool {
        matches!(self, Self::Nats | Self::Shadow)
    }
}

/// The events-feed publisher.
///
/// A thin wrapper over [`EhdbCommandPublisher`], which is not command-specific
/// in any way that matters here: it is a lazily-connected, shard-routing,
/// retry-across-a-writer-restart publisher of `EventRecord`s. Reusing it means
/// the events path inherits the #208 retry window (sized to span a writer pod
/// restart) and the #205 no-mutex-across-the-round-trip fix for free, rather
/// than re-deriving both and getting one of them subtly wrong.
pub struct EhdbEventPublisher {
    inner: EhdbCommandPublisher,
}

impl EhdbEventPublisher {
    /// A publisher routing over `shard_count` shards to the events writers at
    /// `addrs` (`host:port` strings — DNS names resolved at connect time).
    pub fn new(shard_count: u32, addrs: BTreeMap<u32, String>) -> Self {
        Self {
            inner: EhdbCommandPublisher::new(shard_count, addrs),
        }
    }

    /// Build from `NOETL_EVENT_BUS_WRITER_ADDRS` / `NOETL_EVENT_SHARD_COUNT`.
    pub fn from_env() -> Self {
        let addrs =
            parse_writer_addrs(&std::env::var("NOETL_EVENT_BUS_WRITER_ADDRS").unwrap_or_default());
        let shard_count = std::env::var("NOETL_EVENT_SHARD_COUNT")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(1);
        Self::new(shard_count, addrs)
    }

    /// Whether any writer address is configured.
    pub fn is_configured(&self) -> bool {
        self.inner.is_configured()
    }

    /// Publish one event onto the EHDB events feed.
    ///
    /// `execution_id` routes the shard, `event_id` is the record's identity (the
    /// dedup key the materializer projects on), and `payload` is the same
    /// `to_stream_json()` body published to NATS — byte-identical on purpose, so
    /// shadow parity is a straight comparison rather than a schema translation.
    ///
    /// `event_type` is carried inside the payload, which is where
    /// `ehdb_feed::event_feed_subject` reads it to derive the
    /// `events.<event_type>` routing subject — the analog of the NATS subject
    /// `noetl.events.<event_type>`.
    pub async fn publish_event(
        &self,
        execution_id: i64,
        event_id: i64,
        payload: &[u8],
    ) -> Result<u64, String> {
        self.inner.publish(execution_id, event_id, payload).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_selectable_modes_parse_case_and_space_insensitively() {
        assert_eq!(EventBusMode::from_env_value("EHDB"), Ok(EventBusMode::Ehdb));
        assert_eq!(
            EventBusMode::from_env_value(" Shadow "),
            Ok(EventBusMode::Shadow)
        );
    }

    /// The heart of noetl/ai-meta#243: three different situations used to become
    /// one value. Each must now name ITSELF, because the whole cost of the old
    /// behaviour was that the message did not say which of these had happened.
    #[test]
    fn unset_typo_and_stale_nats_are_three_distinct_errors() {
        let unset = EventBusMode::from_env_value("").unwrap_err();
        let typo = EventBusMode::from_env_value("ehbd").unwrap_err();
        let stale = EventBusMode::from_env_value("nats").unwrap_err();

        assert!(unset.contains("required and unset"), "{unset}");
        assert!(typo.contains("ehbd"), "the typo must be quoted back: {typo}");
        assert!(
            stale.contains("no longer exists"),
            "a stale nats must say the transport is gone, not just 'invalid': {stale}"
        );

        // Distinct, not merely non-empty. Three identical messages would satisfy
        // a naive "is it an error" test while reproducing the exact defect.
        assert_ne!(unset, typo);
        assert_ne!(typo, stale);
        assert_ne!(unset, stale);

        // Every one of them actionable in the same way.
        for m in [&unset, &typo, &stale] {
            assert!(m.contains("NOETL_EVENT_BUS"), "must name the variable: {m}");
        }
        for m in [&unset, &stale] {
            assert!(m.contains("ehdb"), "must name the fix: {m}");
        }
    }

    /// A typo must NOT silently select a mode — the property the old catch-all
    /// broke. Written as a sweep so a future catch-all fails here.
    #[test]
    fn no_unrecognised_value_ever_yields_a_mode() {
        for v in [
            "", " ", "ehbd", "EHDB ehdb", "natss", "true", "1", "off", "none",
            "shadow-mode", "ehdb;shadow", "\u{0}",
        ] {
            assert!(
                EventBusMode::from_env_value(v).is_err(),
                "{v:?} must not resolve to a mode"
            );
        }
        // Positive control: the sweep is capable of passing a value through, so
        // the assertions above are measuring the parser and not a broken loop.
        assert!(EventBusMode::from_env_value("ehdb").is_ok());
    }

    #[test]
    fn shadow_publishes_both_ehdb_publishes_only_ehdb() {
        assert!(EventBusMode::Shadow.publishes_ehdb() && EventBusMode::Shadow.publishes_nats());
        assert!(EventBusMode::Ehdb.publishes_ehdb() && !EventBusMode::Ehdb.publishes_nats());
        assert!(!EventBusMode::Nats.publishes_ehdb() && EventBusMode::Nats.publishes_nats());
    }

    #[test]
    fn an_unconfigured_publisher_reports_itself_unconfigured() {
        let p = EhdbEventPublisher::new(1, BTreeMap::new());
        assert!(!p.is_configured());
        let p = EhdbEventPublisher::new(
            1,
            parse_writer_addrs("0@noetl-cmdbus-writer-0.noetl.svc.cluster.local:9103"),
        );
        assert!(p.is_configured());
    }
}
