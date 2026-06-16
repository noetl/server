//! NATS JetStream event-log publisher — the CQRS write-path queue
//! (noetl/ai-meta#103 phase 2a).
//!
//! Where [`crate::nats::publisher`] publishes *lightweight* command
//! notifications (workers fetch the detail by id), this publishes the **full
//! event** onto a durable JetStream stream so the read side can be derived from
//! the stream alone.  The `system/projector` playbook (phase 2b) is a batch
//! consumer of this stream: it pulls N messages, folds them into the projection
//! in one transaction via `/api/internal/events/project`, and acks.
//!
//! ## Why a stream and not the DB trigger it replaces
//!
//! A trigger welds the producer to Postgres internals — invisible, vendor-
//! specific, and it doesn't travel across a storage-type change.  A JetStream
//! stream is a storage-agnostic queue (swap it for Kafka behind the same
//! publish/consume contract) and is the design-note's stated write side.  At the
//! 2d cutover the *worker* publishes here directly and the synchronous
//! `noetl.event` INSERT goes away — the stream the [`EventStreamPublisher`]
//! fills in 2a is the same stream the worker fills in 2d, so nothing downstream
//! changes.
//!
//! ## Idempotency
//!
//! Each event is published with the JetStream **message-id** set to its
//! `event_id` (`Nats-Msg-Id` header).  The stream's dedup window collapses
//! re-publishes — so the tailer ([`crate::services::event_stream`]) can re-scan
//! an overlapping window after a restart without double-delivering, and the
//! projector folds each event exactly once.

use async_nats::HeaderMap;
use async_nats::jetstream::{self, Context};
use std::sync::Arc;
use std::time::Duration;

use super::publisher::NatsError;

/// JetStream stream name for the event log.
pub const EVENT_STREAM: &str = "noetl_events";

/// Subject prefix.  Events publish to `noetl.events.<event_type>`; the stream
/// binds the wildcard `noetl.events.>` so every type lands in one stream while
/// consumers can filter by subject if they want a slice.
pub const EVENT_SUBJECT_PREFIX: &str = "noetl.events";

/// Wildcard the stream binds.
pub const EVENT_SUBJECT_WILDCARD: &str = "noetl.events.>";

/// Durable pull consumer the `system/projector` playbook drains
/// (noetl/ai-meta#103 phase 2b).  `js_consume` / `tool: subscription` require
/// the consumer to pre-exist; the server creates it alongside the stream so the
/// playbook stays a pure consumer.
pub const PROJECTOR_CONSUMER: &str = "noetl_projector";

/// Publishes full events onto the `noetl_events` JetStream stream.
#[derive(Clone)]
pub struct EventStreamPublisher {
    js: Context,
}

impl EventStreamPublisher {
    /// Build the publisher from a connected NATS client, ensuring the stream
    /// exists.  Mirrors [`crate::nats::publisher::NatsPublisher::new`].
    ///
    /// `dedup_window` sizes the stream's message-id dedup window — it must be
    /// at least as long as the tailer's re-scan lookback so an overlapping
    /// re-publish after a restart is collapsed rather than re-delivered.
    pub async fn new(
        client: Arc<async_nats::Client>,
        dedup_window: Duration,
        max_age: Duration,
    ) -> Result<Self, NatsError> {
        let js = jetstream::new((*client).clone());
        Self::ensure_stream(&js, dedup_window, max_age).await?;
        Self::ensure_projector_consumer(&js).await?;
        Ok(Self { js })
    }

    /// Ensure the durable `noetl_projector` pull consumer exists on the stream.
    /// Idempotent: `create_consumer` with a stable durable name + config is a
    /// no-op when it already exists.  Explicit-ack so the projector controls
    /// when a batch is acked (the `tool: subscription` poll acks on success).
    async fn ensure_projector_consumer(js: &Context) -> Result<(), NatsError> {
        let stream = js
            .get_stream(EVENT_STREAM)
            .await
            .map_err(|e| NatsError::JetStream(e.to_string()))?;
        stream
            .create_consumer(jetstream::consumer::pull::Config {
                durable_name: Some(PROJECTOR_CONSUMER.to_string()),
                filter_subject: EVENT_SUBJECT_WILDCARD.to_string(),
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                ..Default::default()
            })
            .await
            .map_err(|e| NatsError::JetStream(e.to_string()))?;
        tracing::debug!(
            stream = EVENT_STREAM,
            consumer = PROJECTOR_CONSUMER,
            "ensured projector pull consumer"
        );
        Ok(())
    }

    /// Ensure the `noetl_events` stream exists with the dedup window + retention
    /// we need.  If it already exists we leave its config untouched (an operator
    /// may have tuned retention); we only create on absence, same shape as the
    /// command stream.
    async fn ensure_stream(
        js: &Context,
        dedup_window: Duration,
        max_age: Duration,
    ) -> Result<(), NatsError> {
        match js.get_stream(EVENT_STREAM).await {
            Ok(_) => {
                tracing::debug!(stream = EVENT_STREAM, "Using existing event stream");
                Ok(())
            }
            Err(_) => {
                let config = jetstream::stream::Config {
                    name: EVENT_STREAM.to_string(),
                    subjects: vec![EVENT_SUBJECT_WILDCARD.to_string()],
                    // File storage: the stream is a durable write log, not a
                    // best-effort notification channel like noetl_commands.
                    storage: jetstream::stream::StorageType::File,
                    // Dedup by Nats-Msg-Id (= event_id) so the tailer's restart
                    // re-scan can't double-deliver.
                    duplicate_window: dedup_window,
                    // Retention bounds the stream; the projector folds into the
                    // durable projection tables well inside this window.  During
                    // dual-write noetl.event remains the source of truth, so the
                    // stream is allowed to age out.
                    max_age,
                    ..Default::default()
                };
                js.create_stream(config)
                    .await
                    .map_err(|e| NatsError::JetStream(e.to_string()))?;
                tracing::info!(
                    stream = EVENT_STREAM,
                    subject = EVENT_SUBJECT_WILDCARD,
                    "Created event stream (CQRS write-path queue, #103 phase 2a)"
                );
                Ok(())
            }
        }
    }

    /// Publish one event's full JSON payload, keyed by `event_id` for dedup.
    /// `event_type` selects the subject suffix.
    pub async fn publish_event(
        &self,
        event_id: i64,
        event_type: &str,
        payload: &[u8],
    ) -> Result<(), NatsError> {
        let subject = format!("{EVENT_SUBJECT_PREFIX}.{event_type}");
        let mut headers = HeaderMap::new();
        // JetStream message-dedup key.
        headers.insert("Nats-Msg-Id", event_id.to_string().as_str());

        self.js
            .publish_with_headers(subject, headers, payload.to_vec().into())
            .await
            .map_err(|e| NatsError::Publish(e.to_string()))?
            .await
            .map_err(|e| NatsError::Publish(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_constants_are_stable() {
        assert_eq!(EVENT_STREAM, "noetl_events");
        assert_eq!(EVENT_SUBJECT_WILDCARD, "noetl.events.>");
        // The wildcard must cover the prefixed subjects the publisher emits.
        assert!(EVENT_SUBJECT_WILDCARD.starts_with(EVENT_SUBJECT_PREFIX));
    }
}
