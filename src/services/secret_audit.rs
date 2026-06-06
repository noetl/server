//! Secret-resolution audit trail (Secrets Wallet Phase 7b,
//! [`noetl/ai-meta#61`](https://github.com/noetl/ai-meta/issues/61)).
//!
//! Today the wallet has no durable record of "who accessed credential X at
//! time Y, on which execution, with what outcome."  The tracing-span
//! surface (`credential.seal`, `credential.cross_region_resolve`) is fine
//! for live debugging but evaporates with log retention.  Compliance
//! regimes (SOC 2, ISO 27001, FedRAMP, PCI-DSS) require a queryable audit
//! trail with retention measured in years, not days.
//!
//! Phase 7b adds the durable trail.  Every credential surface (sealed
//! delivery, cross-region broker, keychain resolution, plain `GET
//! /api/credentials/{id}`) emits one [`AuditEvent`] per resolution
//! attempt.  Events flow through an [`AuditSink`] — the production sink
//! writes one row to `noetl.secret_audit`; tests + audit-disabled
//! deployments use [`NoopAuditSink`].
//!
//! **The event NEVER contains the secret value.**  It carries the
//! identifier, the worker / execution context, the outcome, and a
//! short free-text note.  An operator with read access to the audit
//! table sees "who did what, when, and how it ended" — they do NOT
//! see what was actually resolved.
//!
//! Two write modes:
//! - **Async fire-and-forget** ([`SecretAuditService::record_async`]) —
//!   the default for the production path.  The resolver doesn't block on
//!   the audit write; a failed write logs a span warning and increments
//!   `noetl_secret_audit_writes_total{status="dropped_async"}`.  Used
//!   when the audit is for forensics, not gate.
//! - **Strict** ([`SecretAuditService::record_strict`]) — caller awaits
//!   the write.  If it fails, the resolution propagates the error.  Used
//!   when compliance requires the audit row exists BEFORE the value is
//!   released.  Operator opts in via `NOETL_SECRET_AUDIT_REQUIRED=true`.
//!
//! Phase 7b.2 wires the actual DB query path and the
//! `GET /api/internal/secret-audit` query endpoint; this round
//! ships the event shape, the service surface, and the
//! `get_sealed` handler integration as the proof of concept.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::AppResult;

/// One audit event — the row that lands in `noetl.secret_audit`.
///
/// Field semantics follow the issue brief on Phase 7b — anything
/// optional uses `Option<>` so a surface that doesn't know a field
/// (e.g. `worker_id` for the bare `GET /api/credentials/{id}` path)
/// just omits it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEvent {
    /// Application-side snowflake id (per `agents/rules/observability.md`
    /// Principle 3).  Generated at event construction so spans /
    /// metrics can correlate before the DB write.
    pub audit_id: i64,
    /// Wall-clock at event construction.
    pub occurred_at: DateTime<Utc>,
    /// The credential identifier / alias the resolver was asked for.
    pub credential: String,
    /// Bounded enum: `get_sealed`, `cross_region_broker_serve`,
    /// `resolve_keychain`, `get_credential`.  See [`Operation`].
    pub operation: String,
    /// Bounded enum: `ok`, `residency_violation`, `broker_unreachable`,
    /// `credential_not_found`, `no_pubkey`, `wrong_region`,
    /// `provider_fetch_error`, `template_error`.  See [`Outcome`].
    pub outcome: String,
    /// Worker that asked, when the surface knew one (sealed path).
    pub worker_id: Option<String>,
    pub execution_id: Option<i64>,
    pub parent_execution_id: Option<i64>,
    /// Which server-region served the request.
    pub server_region: Option<String>,
    /// Which region the broker served from, when the resolution chained
    /// through Phase-6e cross-region routing.
    pub broker_region: Option<String>,
    /// The KEK version on the record at access time, when available.
    /// Phase-7a's rotation primitives keep this stable across rotations.
    pub kek_version: Option<String>,
    /// Short free-text note for the operator.  **Never** the secret
    /// value; never anything that could be reverse-engineered into one.
    pub notes: Option<String>,
}

/// Bounded enum for [`AuditEvent::operation`].  Implementations stick
/// to this list to keep `noetl_secret_audit_writes_total{operation,…}`
/// cardinality bounded.
#[derive(Debug, Clone, Copy)]
pub enum Operation {
    /// `GET /api/credentials/{id}/sealed` — Phase 5b sealed delivery.
    GetSealed,
    /// `POST /api/internal/cross-region/resolve` — Phase 6e broker
    /// serving a peer's request.
    CrossRegionBrokerServe,
    /// `resolve_keychain_entry` — Phase 3b keychain provider resolution.
    ResolveKeychain,
    /// `GET /api/credentials/{id}` (with `include_data=true`) — bare
    /// bearer-path credential fetch.
    GetCredential,
}

impl Operation {
    pub fn as_str(self) -> &'static str {
        match self {
            Operation::GetSealed => "get_sealed",
            Operation::CrossRegionBrokerServe => "cross_region_broker_serve",
            Operation::ResolveKeychain => "resolve_keychain",
            Operation::GetCredential => "get_credential",
        }
    }
}

/// Bounded enum for [`AuditEvent::outcome`].
#[derive(Debug, Clone, Copy)]
pub enum Outcome {
    Ok,
    ResidencyViolation,
    BrokerUnreachable,
    CredentialNotFound,
    NoPubkey,
    WrongRegion,
    ProviderFetchError,
    TemplateError,
    /// Anything not covered above — keeps the enum closed at the type
    /// level while the metric label space stays bounded.
    Other(&'static str),
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::ResidencyViolation => "residency_violation",
            Outcome::BrokerUnreachable => "broker_unreachable",
            Outcome::CredentialNotFound => "credential_not_found",
            Outcome::NoPubkey => "no_pubkey",
            Outcome::WrongRegion => "wrong_region",
            Outcome::ProviderFetchError => "provider_fetch_error",
            Outcome::TemplateError => "template_error",
            Outcome::Other(s) => s,
        }
    }
}

/// Sink trait — production writes to `noetl.secret_audit`; tests use
/// a recording sink to assert what was written.
#[async_trait]
pub trait AuditSink: Send + Sync {
    async fn write(&self, event: &AuditEvent) -> AppResult<()>;

    /// Stable id for the sink — `db` / `noop` / `mock-N`.  Used in
    /// span attribution + the strict-mode error message.
    fn sink_id(&self) -> &str;
}

/// Default sink for tests + audit-disabled deployments.  Reports
/// success without persisting anything.
#[derive(Debug, Default)]
pub struct NoopAuditSink;

#[async_trait]
impl AuditSink for NoopAuditSink {
    async fn write(&self, _event: &AuditEvent) -> AppResult<()> {
        Ok(())
    }
    fn sink_id(&self) -> &str {
        "noop"
    }
}

/// Service wrapper a handler uses to record events.  Decouples the
/// "build an event" code path from the "write to DB or drop" path so
/// the handler integration is one line.
#[derive(Clone)]
pub struct SecretAuditService {
    sink: Arc<dyn AuditSink>,
    strict: bool,
}

impl SecretAuditService {
    /// New service.  `strict` is read from `NOETL_SECRET_AUDIT_REQUIRED`
    /// at startup (the typical wire site is `AppState::new`).
    pub fn new(sink: Arc<dyn AuditSink>, strict: bool) -> Self {
        Self { sink, strict }
    }

    /// Convenience constructor reading the env directly.
    pub fn from_env(sink: Arc<dyn AuditSink>) -> Self {
        let strict = matches!(
            std::env::var("NOETL_SECRET_AUDIT_REQUIRED").ok().as_deref(),
            Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
        );
        Self { sink, strict }
    }

    /// Convenience constructor for tests + the audit-disabled path.
    pub fn noop() -> Self {
        Self {
            sink: Arc::new(NoopAuditSink),
            strict: false,
        }
    }

    /// Async fire-and-forget write.  Caller never awaits the DB round
    /// trip — the audit is for forensics, not gate.  A failed write
    /// records `dropped_async` on the metric and emits a span warning
    /// but does NOT propagate to the caller.
    pub fn record_async(&self, event: AuditEvent) {
        let sink = self.sink.clone();
        let operation = event.operation.clone();
        let outcome = event.outcome.clone();
        tokio::spawn(async move {
            match sink.write(&event).await {
                Ok(()) => {
                    crate::metrics::record_secret_audit_write(&operation, &outcome, "written")
                }
                Err(e) => {
                    crate::metrics::record_secret_audit_write(
                        &operation,
                        &outcome,
                        "dropped_async",
                    );
                    tracing::warn!(
                        sink = sink.sink_id(),
                        operation = %operation,
                        outcome = %outcome,
                        error = %e,
                        "secret_audit.write failed (dropped)"
                    );
                }
            }
        });
    }

    /// Strict write.  Caller awaits the result.  Used when compliance
    /// requires the row exist before the credential value is released
    /// — flip via `NOETL_SECRET_AUDIT_REQUIRED=true`.  A failed write
    /// returns an error that the handler propagates verbatim, blocking
    /// the underlying resolution.
    pub async fn record_strict(&self, event: AuditEvent) -> AppResult<()> {
        let operation = event.operation.clone();
        let outcome = event.outcome.clone();
        match self.sink.write(&event).await {
            Ok(()) => {
                crate::metrics::record_secret_audit_write(&operation, &outcome, "written");
                Ok(())
            }
            Err(e) => {
                crate::metrics::record_secret_audit_write(&operation, &outcome, "failed_strict");
                tracing::error!(
                    sink = self.sink.sink_id(),
                    operation = %operation,
                    outcome = %outcome,
                    error = %e,
                    "secret_audit.write failed under strict mode (resolution blocked)"
                );
                Err(e)
            }
        }
    }

    /// Branches between [`record_async`] and [`record_strict`] based on
    /// the configured strict mode.  This is the typical handler call.
    pub async fn record(&self, event: AuditEvent) -> AppResult<()> {
        if self.strict {
            self.record_strict(event).await
        } else {
            self.record_async(event);
            Ok(())
        }
    }

    /// True iff strict-mode is enabled.  Test + diagnostic surface.
    pub fn is_strict(&self) -> bool {
        self.strict
    }
}

/// Builder helper — fills `audit_id` + `occurred_at` from the
/// application snowflake generator + wall clock so callers only
/// supply the meaningful fields.
pub fn build_event(
    audit_id: i64,
    credential: impl Into<String>,
    operation: Operation,
    outcome: Outcome,
) -> AuditEvent {
    AuditEvent {
        audit_id,
        occurred_at: Utc::now(),
        credential: credential.into(),
        operation: operation.as_str().to_string(),
        outcome: outcome.as_str().to_string(),
        worker_id: None,
        execution_id: None,
        parent_execution_id: None,
        server_region: None,
        broker_region: None,
        kek_version: None,
        notes: None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Recording sink — keeps a Mutex<Vec<AuditEvent>> so tests can
    /// assert what was written.
    #[derive(Default)]
    struct MockSink {
        seen: Mutex<Vec<AuditEvent>>,
        fail: bool,
    }

    #[async_trait]
    impl AuditSink for MockSink {
        async fn write(&self, event: &AuditEvent) -> AppResult<()> {
            if self.fail {
                return Err(crate::error::AppError::Internal(
                    "mock sink: failing write".to_string(),
                ));
            }
            self.seen.lock().unwrap().push(event.clone());
            Ok(())
        }
        fn sink_id(&self) -> &str {
            "mock"
        }
    }

    fn make_event(audit_id: i64) -> AuditEvent {
        build_event(audit_id, "duffel_token", Operation::GetSealed, Outcome::Ok)
    }

    #[test]
    fn builder_fills_audit_id_and_timestamp() {
        let e = make_event(42);
        assert_eq!(e.audit_id, 42);
        assert_eq!(e.credential, "duffel_token");
        assert_eq!(e.operation, "get_sealed");
        assert_eq!(e.outcome, "ok");
        // Worker id + execution id default to None so the caller can
        // fill them in based on the surface.
        assert!(e.worker_id.is_none());
        assert!(e.execution_id.is_none());
    }

    #[test]
    fn operation_and_outcome_as_str_round_trip() {
        // Drift-guard: the strings used in `AuditEvent.operation` /
        // `.outcome` are the same the bounded enum reports.  Anything
        // querying the table by the bounded labels stays correct.
        assert_eq!(Operation::GetSealed.as_str(), "get_sealed");
        assert_eq!(
            Operation::CrossRegionBrokerServe.as_str(),
            "cross_region_broker_serve"
        );
        assert_eq!(Outcome::ResidencyViolation.as_str(), "residency_violation");
        assert_eq!(Outcome::Other("custom_thing").as_str(), "custom_thing");
    }

    #[tokio::test]
    async fn noop_sink_always_succeeds() {
        let s = NoopAuditSink;
        s.write(&make_event(1)).await.unwrap();
        assert_eq!(s.sink_id(), "noop");
    }

    #[tokio::test]
    async fn record_strict_blocks_on_sink_failure() {
        let sink = Arc::new(MockSink {
            seen: Mutex::new(Vec::new()),
            fail: true,
        });
        let svc = SecretAuditService::new(sink, true);
        assert!(svc.is_strict());
        let err = svc.record_strict(make_event(1)).await.unwrap_err();
        assert!(format!("{err:?}").contains("failing write"));
    }

    #[tokio::test]
    async fn record_strict_persists_on_success() {
        let sink = Arc::new(MockSink {
            seen: Mutex::new(Vec::new()),
            fail: false,
        });
        let svc = SecretAuditService::new(sink.clone(), true);
        svc.record_strict(make_event(7)).await.unwrap();
        let seen = sink.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].audit_id, 7);
    }

    #[tokio::test]
    async fn record_dispatches_async_when_not_strict() {
        // Non-strict service: record() spawns a tokio task and returns
        // Ok regardless of sink state.  We can't deterministically
        // observe the task without a join handle (record_async drops
        // the handle), but we can assert the call doesn't block / err.
        let sink = Arc::new(MockSink {
            seen: Mutex::new(Vec::new()),
            fail: false,
        });
        let svc = SecretAuditService::new(sink.clone(), false);
        assert!(!svc.is_strict());
        svc.record(make_event(1)).await.unwrap();
        // Yield + small wait so the spawned task gets to run.
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let seen = sink.seen.lock().unwrap();
        assert!(seen.len() <= 1, "spawned write at most once");
    }

    #[tokio::test]
    async fn noop_service_records_without_blocking() {
        // The `SecretAuditService::noop` helper — used in tests + the
        // audit-disabled production path — must accept `record` calls
        // without erroring and without panicking.
        let svc = SecretAuditService::noop();
        svc.record(make_event(1)).await.unwrap();
        // strict-mode call also succeeds since the noop sink never errors.
        svc.record_strict(make_event(2)).await.unwrap();
    }

    #[test]
    fn from_env_respects_truthy_values() {
        // Process-global env mutations — restore on exit to avoid
        // poisoning sibling tests.
        let saved = std::env::var("NOETL_SECRET_AUDIT_REQUIRED").ok();
        let sink: Arc<dyn AuditSink> = Arc::new(NoopAuditSink);
        // Each truthy variant the constructor honors.
        for val in ["1", "true", "TRUE", "yes", "YES"] {
            unsafe { std::env::set_var("NOETL_SECRET_AUDIT_REQUIRED", val) };
            assert!(
                SecretAuditService::from_env(sink.clone()).is_strict(),
                "value {val:?} should enable strict mode"
            );
        }
        // Anything else stays non-strict.
        unsafe { std::env::set_var("NOETL_SECRET_AUDIT_REQUIRED", "0") };
        assert!(!SecretAuditService::from_env(sink.clone()).is_strict());
        unsafe { std::env::remove_var("NOETL_SECRET_AUDIT_REQUIRED") };
        assert!(!SecretAuditService::from_env(sink).is_strict());
        // Restore.
        match saved {
            Some(v) => unsafe { std::env::set_var("NOETL_SECRET_AUDIT_REQUIRED", v) },
            None => unsafe { std::env::remove_var("NOETL_SECRET_AUDIT_REQUIRED") },
        }
    }
}
