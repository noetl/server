//! Prometheus metrics surface for the NoETL control plane.
//!
//! Follows `agents/rules/observability.md` Principles 1 and 2:
//!
//! - Every substantive change ships a counter and/or histogram
//!   alongside the code (Principle 1).
//! - Counters / histograms / gauges scale; per-event INFO logs
//!   do not (Principle 2).
//!
//! The registry is global (`OnceLock<Registry>`) so any module
//! can record without threading a handle through `AppState`.
//! `gather_text()` renders the registry into the standard
//! Prometheus text exposition format used by `/metrics`.
//!
//! ## Per-endpoint conventions
//!
//! - **Counters** are named with a trailing `_total` suffix
//!   (Prometheus convention).
//! - **Histograms** are named with a unit suffix
//!   (`_seconds`, `_bytes`, etc.) — never raw.
//! - **Labels** are low-cardinality enums (`event_type`,
//!   `status`).  `execution_id` is NEVER a label (cardinality
//!   blows up the registry); it lives on tracing spans only
//!   per Principle 4.
//!
//! ## Round 1 surface
//!
//! - `noetl_events_ingested_total{event_type, status}` —
//!   counter; one increment per `POST /api/events` call.
//!   `event_type` is a meaningful breakdown (15+ values) so it
//!   warrants its own metric.
//! - `noetl_event_ingest_duration_seconds{event_type}` —
//!   histogram; the wall-clock time spent inside the handler.
//!
//! ## Round 2 surface (the other 5 write endpoints)
//!
//! The remaining Phase B POST endpoints each have a single
//! mode of operation (catalog/register = upsert, credentials =
//! upsert, keychain = set, etc.) so they share a generic pair:
//!
//! - `noetl_write_requests_total{endpoint, status}` — counter.
//! - `noetl_write_request_duration_seconds{endpoint}` —
//!   histogram.
//!
//! `endpoint` label values (low-cardinality enum):
//! - `catalog_register`
//! - `credentials_upsert`
//! - `keychain_set`
//! - `runtime_register`
//! - `runtime_heartbeat`
//!
//! See noetl/server#21 for the round breakdown.

use std::sync::OnceLock;

use prometheus::{HistogramOpts, HistogramVec, IntCounterVec, Opts, Registry, TextEncoder};

/// Bucket boundaries for the event-ingest histogram (seconds).
///
/// Spans the 1ms–10s range an event-ingest call could plausibly
/// take (DB write + optional engine call + result-store fallback).
/// Wider buckets at the tail capture the rare slow paths without
/// overweighting the high-percentile estimate.
const EVENT_INGEST_BUCKETS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Global registry — lazily initialised on first `registry()` call.
fn registry() -> &'static Registry {
    static REG: OnceLock<Registry> = OnceLock::new();
    REG.get_or_init(Registry::new)
}

/// Counter: `POST /api/events` calls bucketed by event type and status.
pub fn events_ingested_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_events_ingested_total",
                "Total events accepted by POST /api/events (incremented once per handler call, whether the body persisted or errored).",
            ),
            &["event_type", "status"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Histogram: wall-clock time spent inside the `POST /api/events` handler.
pub fn event_ingest_duration_seconds() -> &'static HistogramVec {
    static M: OnceLock<HistogramVec> = OnceLock::new();
    M.get_or_init(|| {
        let hist = HistogramVec::new(
            HistogramOpts::new(
                "noetl_event_ingest_duration_seconds",
                "Wall-clock time spent inside POST /api/events.",
            )
            .buckets(EVENT_INGEST_BUCKETS.to_vec()),
            &["event_type"],
        )
        .expect("static histogram spec must be valid");
        registry()
            .register(Box::new(hist.clone()))
            .expect("histogram registration must succeed");
        hist
    })
}

/// Record a single `POST /api/events` outcome.
///
/// `event_type` is the wire event_type from the request
/// (`"playbook.initialized"`, `"command.claimed"`, etc.).
/// `status` is `"ok"` on the success path, `"error"` on any
/// `Err` return.  `duration_seconds` is wall-clock time
/// inside the handler.
pub fn record_event_ingest(event_type: &str, status: &str, duration_seconds: f64) {
    events_ingested_total()
        .with_label_values(&[event_type, status])
        .inc();
    event_ingest_duration_seconds()
        .with_label_values(&[event_type])
        .observe(duration_seconds);
}

// ---------------------------------------------------------------------------
// Round 2 — generic write-endpoint surface
// ---------------------------------------------------------------------------

/// Canonical endpoint labels accepted by [`record_write_request`].
///
/// Kept as `&'static str` constants so a typo at a call site is a
/// compile error rather than a runtime drift.  Add new entries here
/// (and only here) when instrumenting future write endpoints.
pub mod endpoint {
    pub const CATALOG_REGISTER: &str = "catalog_register";
    pub const CREDENTIALS_UPSERT: &str = "credentials_upsert";
    pub const KEYCHAIN_SET: &str = "keychain_set";
    pub const RUNTIME_REGISTER: &str = "runtime_register";
    pub const RUNTIME_HEARTBEAT: &str = "runtime_heartbeat";
}

/// Counter: write-endpoint dispatches bucketed by canonical
/// endpoint name and status.  Shared across the Round-2 endpoints
/// because each has a single mode of operation; per-endpoint
/// metrics would inflate the registry without adding signal.
pub fn write_requests_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_write_requests_total",
                "Total POST requests to write endpoints other than /api/events (counted once per handler call, Ok or Err).",
            ),
            &["endpoint", "status"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Histogram: wall-clock time spent inside Round-2 write
/// endpoints, bucketed by canonical endpoint label.
pub fn write_request_duration_seconds() -> &'static HistogramVec {
    static M: OnceLock<HistogramVec> = OnceLock::new();
    M.get_or_init(|| {
        let hist = HistogramVec::new(
            HistogramOpts::new(
                "noetl_write_request_duration_seconds",
                "Wall-clock time spent inside POST write endpoints (other than /api/events).",
            )
            .buckets(EVENT_INGEST_BUCKETS.to_vec()),
            &["endpoint"],
        )
        .expect("static histogram spec must be valid");
        registry()
            .register(Box::new(hist.clone()))
            .expect("histogram registration must succeed");
        hist
    })
}

/// Record a single Round-2 write-endpoint outcome.
///
/// `endpoint` should be one of the constants under
/// [`endpoint`].  `status` is `"ok"` on the success path,
/// `"error"` on any `Err` return.  `duration_seconds` is
/// wall-clock time inside the handler.
pub fn record_write_request(endpoint: &str, status: &str, duration_seconds: f64) {
    write_requests_total()
        .with_label_values(&[endpoint, status])
        .inc();
    write_request_duration_seconds()
        .with_label_values(&[endpoint])
        .observe(duration_seconds);
}

/// Counter: sealed credential responses on `GET /api/credentials/{id}/sealed`,
/// bucketed by outcome.
///
/// Secrets Wallet Phase 5b (noetl/ai-meta#61) — pairs with the `credential.seal`
/// span in `handlers::credentials::get_sealed`.  Labels:
///
/// - `status` ∈ {`ok`, `no_pubkey`, `worker_not_found`, `seal_error`,
///   `credential_error`} — the outcome bucket.
///
/// `noetl_credentials_sealed_total{status="ok"}` is the throughput counter;
/// the other label values are failure modes worth grepping in Prometheus
/// when a worker stops being able to fetch sealed credentials.  `execution_id`
/// is NOT a label (cardinality) — it lives on the matching span.
pub fn credentials_sealed_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_credentials_sealed_total",
                "GET /api/credentials/{id}/sealed calls by outcome status.",
            ),
            &["status"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Increment [`credentials_sealed_total`] by 1 for the given outcome.
pub fn record_credential_seal(status: &str) {
    credentials_sealed_total()
        .with_label_values(&[status])
        .inc();
}

/// Secrets-Wallet Phase 6a: per-resolve counter for keychain entries
/// against external secret providers.
///
/// Labels are bounded enums:
/// - `provider`: `gcp` / `aws` / `azure` / `vault` / `k8s` (the five
///   backends behind [`crate::secrets::SecretProvider`]).
/// - `region`: the secret's home region as it was resolved.  Falls back
///   to `"-"` when neither the keychain entry nor `NOETL_SERVER_REGION`
///   supplied one (legacy path; pre-6a behaviour).
/// - `status`: `ok` on a successful fetch; otherwise a failure-mode
///   label (`provider_build_error`, `provider_fetch_error`, `template_error`).
///
/// `execution_id` is NOT a label (cardinality) — it lives on the matching
/// span per [`agents/rules/observability.md`].  Region IS a label by design:
/// the cardinality is bounded (operators don't deploy into hundreds of
/// regions in practice), and per-region breakdown is exactly what an
/// operator queries when troubleshooting a residency-related outage.
pub fn secret_resolve_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_secret_resolve_total",
                "Keychain-entry resolutions against external secret providers, by \
                 provider + region + outcome.",
            ),
            &["provider", "region", "status"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Increment [`secret_resolve_total`] by 1 for the given outcome.
///
/// `region` may be empty — pass `"-"` (the convention used here) when no
/// region was supplied, to keep the label cardinality bounded.
pub fn record_secret_resolve(provider: &str, region: &str, status: &str) {
    let region_label = if region.is_empty() { "-" } else { region };
    secret_resolve_total()
        .with_label_values(&[provider, region_label, status])
        .inc();
}

/// Secrets-Wallet Phase 6b: per-`(provider, region)` provider-build counter.
///
/// `status`:
/// - `cache_hit` — the registry returned an existing entry.
/// - `ok` — a new provider was built and inserted.
/// - `error` — `build_secret_provider_for_region` failed; the cache is
///   unchanged.
///
/// Together with [`secret_resolve_total`] this answers two operator questions:
/// "Is the cache effective?" (`cache_hit / (ok + cache_hit)` ratio) and
/// "Is a region's provider down?" (`error` per-region rate).
pub fn secret_provider_build_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_secret_provider_build_total",
                "ProviderRegistry get_or_build outcomes per (provider, region).",
            ),
            &["provider", "region", "status"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Increment [`secret_provider_build_total`] by 1.
pub fn record_secret_provider_build(provider: &str, region: &str, status: &str) {
    let region_label = if region.is_empty() { "-" } else { region };
    secret_provider_build_total()
        .with_label_values(&[provider, region_label, status])
        .inc();
}

/// Secrets-Wallet Phase 6b: histogram of secret-resolve wall-clock latency,
/// keyed by `(provider, region)`.  Bucketed to span the 5 ms – 5 s range
/// where cloud secret managers and Vault clusters actually live.
///
/// `execution_id` is NOT a label — it lives on the matching `secret.resolve`
/// span per [`agents/rules/observability.md`] Principle 4.
pub fn secret_resolve_duration_seconds() -> &'static HistogramVec {
    static M: OnceLock<HistogramVec> = OnceLock::new();
    M.get_or_init(|| {
        let h = HistogramVec::new(
            HistogramOpts::new(
                "noetl_secret_resolve_duration_seconds",
                "Wall-clock seconds spent resolving one keychain entry against \
                 its provider.",
            )
            .buckets(vec![
                0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0,
            ]),
            &["provider", "region"],
        )
        .expect("static histogram spec must be valid");
        registry()
            .register(Box::new(h.clone()))
            .expect("histogram registration must succeed");
        h
    })
}

/// Observe one resolve duration on the [`secret_resolve_duration_seconds`]
/// histogram.
pub fn record_secret_resolve_duration(provider: &str, region: &str, seconds: f64) {
    let region_label = if region.is_empty() { "-" } else { region };
    secret_resolve_duration_seconds()
        .with_label_values(&[provider, region_label])
        .observe(seconds);
}

/// Render the global registry as Prometheus text-exposition
/// format.  Used by the `GET /metrics` handler.
pub fn gather_text() -> Result<String, prometheus::Error> {
    let encoder = TextEncoder::new();
    let metric_families = registry().gather();
    encoder.encode_to_string(&metric_families)
}

#[cfg(test)]
mod tests {
    use super::*;
    // The registry is process-global, so all tests share state.
    // We assert on the rendered text after at least one observation
    // — the test order is `serial_test`-coordinated by the global
    // registry's internal locks (counters are thread-safe).

    #[test]
    fn registry_initializes_once() {
        let a = registry() as *const Registry;
        let b = registry() as *const Registry;
        assert_eq!(a, b, "registry() must return the same instance");
    }

    #[test]
    fn counter_increments_by_label_set() {
        events_ingested_total()
            .with_label_values(&["test.counter_increments", "ok"])
            .inc();
        events_ingested_total()
            .with_label_values(&["test.counter_increments", "ok"])
            .inc();
        let value = events_ingested_total()
            .with_label_values(&["test.counter_increments", "ok"])
            .get();
        assert!(value >= 2, "expected at least 2 increments, got {value}");
    }

    #[test]
    fn histogram_observes_duration() {
        event_ingest_duration_seconds()
            .with_label_values(&["test.histogram_observes"])
            .observe(0.123);
        // We can't read the histogram value directly via the public
        // API, but we can confirm the gathered output mentions it.
        let text = gather_text().expect("gather_text must succeed");
        assert!(
            text.contains("test.histogram_observes"),
            "expected histogram label in text:\n{text}"
        );
    }

    #[test]
    fn gather_text_contains_metric_names() {
        record_event_ingest("test.gather_text", "ok", 0.05);
        let text = gather_text().expect("gather_text must succeed");
        assert!(
            text.contains("noetl_events_ingested_total"),
            "expected counter name in text:\n{text}"
        );
        assert!(
            text.contains("noetl_event_ingest_duration_seconds"),
            "expected histogram name in text:\n{text}"
        );
    }

    #[test]
    fn record_event_ingest_handles_both_statuses() {
        record_event_ingest("test.both_statuses", "ok", 0.01);
        record_event_ingest("test.both_statuses", "error", 0.02);
        let text = gather_text().expect("gather_text must succeed");
        assert!(text.contains("test.both_statuses"));
        // Both label sets should be present.
        assert!(
            text.contains("status=\"ok\""),
            "expected status=ok label in text:\n{text}"
        );
        assert!(
            text.contains("status=\"error\""),
            "expected status=error label in text:\n{text}"
        );
    }

    // --- Round 2: generic write-request metrics ---

    #[test]
    fn write_request_counter_increments_by_label_set() {
        record_write_request("test.write.counter", "ok", 0.01);
        record_write_request("test.write.counter", "ok", 0.02);
        let value = write_requests_total()
            .with_label_values(&["test.write.counter", "ok"])
            .get();
        assert!(value >= 2, "expected at least 2 increments, got {value}");
    }

    #[test]
    fn write_request_metric_names_appear_in_text() {
        record_write_request("test.write.text", "ok", 0.05);
        let text = gather_text().expect("gather_text must succeed");
        assert!(
            text.contains("noetl_write_requests_total"),
            "expected counter name in text:\n{text}"
        );
        assert!(
            text.contains("noetl_write_request_duration_seconds"),
            "expected histogram name in text:\n{text}"
        );
        assert!(text.contains("endpoint=\"test.write.text\""));
    }

    #[test]
    fn endpoint_constants_are_used_consistently() {
        // Compile-time check: the constants exist and resolve.
        let names = [
            endpoint::CATALOG_REGISTER,
            endpoint::CREDENTIALS_UPSERT,
            endpoint::KEYCHAIN_SET,
            endpoint::RUNTIME_REGISTER,
            endpoint::RUNTIME_HEARTBEAT,
        ];
        // Sanity: they're all distinct and non-empty.
        assert_eq!(
            names.iter().collect::<std::collections::HashSet<_>>().len(),
            names.len()
        );
        assert!(names.iter().all(|n| !n.is_empty()));
    }
}
