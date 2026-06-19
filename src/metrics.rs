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

use prometheus::{
    Histogram, HistogramOpts, HistogramVec, IntCounterVec, IntGauge, Opts, Registry, TextEncoder,
};

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

/// Counter: rows purged by `POST /api/internal/cleanup/purge`, bucketed by
/// the `noetl.*` table they were purged from.  Lets retention runs be
/// observed without per-delete log lines (noetl/ai-meta#96).
pub fn cleanup_rows_purged_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_cleanup_rows_purged_total",
                "Total rows deleted by the scheduled-cleanup internal endpoint, by table.",
            ),
            &["table"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record the rows purged from one table by a cleanup run.
pub fn record_cleanup_purged(table: &str, rows: u64) {
    cleanup_rows_purged_total()
        .with_label_values(&[table])
        .inc_by(rows);
}

/// Counter: worker-driven orchestrate drive events, by stage (`dispatched` —
/// the server issued the orchestrate command to the pool; `applied` — its
/// result was applied; `decode_error` — the worker's result couldn't be
/// decoded; `skipped_in_flight` — a drive was already running). The drive loop's
/// health at a glance (noetl/ai-meta#108).
pub fn orchestrate_drive_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_orchestrate_drive_total",
                "Worker-driven orchestrate drive events, by stage.",
            ),
            &["stage"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one worker-driven drive event (`dispatched` / `applied` /
/// `decode_error` / `skipped_in_flight`).
pub fn record_orchestrate_drive(stage: &str) {
    orchestrate_drive_total().with_label_values(&[stage]).inc();
}

// ── State-build mode (RFC noetl/ai-meta#115 Phase 3) ─────────────────────────

/// `noetl_state_build_total{mode, outcome}` — how the drive built `WorkflowState`
/// for a trigger. `mode` = `chain_walk` | `event_scan`. `outcome` = `ok`
/// (built via that mode) | `fallback_cold_head` / `fallback_node_missing` /
/// `fallback_non_genesis` / `fallback_empty` (chain_walk asked for, but a guard
/// sent it to the event-scan path — correctness preserved). Watching
/// `chain_walk/ok` vs the `fallback_*` outcomes shows how often the in-memory
/// chain head served the build without a scan.
pub fn state_build_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_state_build_total",
                "Orchestrator WorkflowState builds, by mode and outcome (RFC #115 Phase 3).",
            ),
            &["mode", "outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one state build (`mode` = `chain_walk`|`event_scan`; `outcome` = `ok`
/// or a `fallback_*` reason).
pub fn record_state_build(mode: &str, outcome: &str) {
    state_build_total().with_label_values(&[mode, outcome]).inc();
}

/// `noetl_state_build_event_scans_total` — incremented once each time the drive
/// path enters the **event-scan** state-construction block (the block that issues
/// `WHERE execution_id = $1 …` scans of `noetl.event`: the consistency `COUNT`,
/// the `event_id > $2` window, and the bounded `rebuild_state`). This is the
/// no-scan proof counter for RFC #115 tenet 3: with `NOETL_STATE_BUILD_MODE=chain_walk`
/// and no fallback, the drive never enters that block, so this counter's delta
/// over a run is **0** while `noetl_state_build_chain_hops` shows the PK-only walk
/// did the work.
pub fn state_build_event_scans_total() -> &'static prometheus::IntCounter {
    static M: OnceLock<prometheus::IntCounter> = OnceLock::new();
    M.get_or_init(|| {
        let counter = prometheus::IntCounter::new(
            "noetl_state_build_event_scans_total",
            "Times the drive entered the noetl.event-scanning state-build block (RFC #115 \
             tenet 3 no-scan proof; chain_walk keeps this at 0).",
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record that the drive used the event-scanning state-build path for a trigger.
pub fn record_state_build_event_scan() {
    state_build_event_scans_total().inc();
}

/// `noetl_state_build_chain_hops` — distribution of chain-walk depth (number of
/// `(execution_id, event_id)` PK lookups) per successful chain-walk build. Each
/// observation == the events collected by following `prev_event_id` head→root.
/// Non-zero observations are the positive evidence the PK-only walk is doing the
/// state construction.
pub fn state_build_chain_hops() -> &'static Histogram {
    static M: OnceLock<Histogram> = OnceLock::new();
    M.get_or_init(|| {
        let hist = Histogram::with_opts(
            HistogramOpts::new(
                "noetl_state_build_chain_hops",
                "Chain-walk depth (prev_event_id PK lookups) per state build (RFC #115 Phase 3).",
            )
            .buckets(vec![1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0]),
        )
        .expect("static histogram spec must be valid");
        registry()
            .register(Box::new(hist.clone()))
            .expect("histogram registration must succeed");
        hist
    })
}

/// Record the depth of one chain-walk build.
pub fn record_state_build_chain_hops(hops: usize) {
    state_build_chain_hops().observe(hops as f64);
}

/// `noetl_state_build_parity_total{result}` — when `NOETL_STATE_BUILD_PARITY_CHECK`
/// is on, each shadow comparison of the event-scan vs chain-walk build records
/// `match` or `mismatch`. A non-zero `mismatch` is a correctness alarm (the two
/// builders disagree for an execution) and is the parity proof's failure signal.
pub fn state_build_parity_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_state_build_parity_total",
                "Shadow event-scan vs chain-walk state-build comparisons, by result (RFC #115).",
            ),
            &["result"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one parity-check result (`match` | `mismatch`).
pub fn record_state_build_parity(result: &str) {
    state_build_parity_total().with_label_values(&[result]).inc();
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

/// Counter: events published onto the `noetl_events` JetStream stream by the
/// CQRS write-path tailer (noetl/ai-meta#103 phase 2a), by event type.  Lets the
/// producer's throughput be observed without a log line per event.
pub fn event_stream_published_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_event_stream_published_total",
                "Total events published to the noetl_events JetStream stream by the CQRS write-path tailer, by event type.",
            ),
            &["event_type"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Counter: events published through the `emit_event` chokepoint when the
/// `NOETL_EVENT_INGEST_PUBLISH_ONLY` gate is on (noetl/ai-meta#103 phase 2d-3),
/// by event type.  Distinct from the tailer's `event_stream_published_total`:
/// this is the **producer cutover** path (the synchronous INSERT replaced by a
/// publish), so a non-zero rate here means the materializer is the sole writer.
pub fn event_ingest_published_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_event_ingest_published_total",
                "Total events published to noetl_events by the emit_event chokepoint under NOETL_EVENT_INGEST_PUBLISH_ONLY (2d-3 cutover), by event type.",
            ),
            &["event_type"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one event published through the chokepoint (gate-on path).
pub fn record_event_published(event_type: &str) {
    event_ingest_published_total()
        .with_label_values(&[event_type])
        .inc();
}

/// Gauge: the tailer's current cursor (`noetl.event.id` last published).  Pair
/// with the table's `MAX(id)` to read publish lag.  Single series (no labels) —
/// one tailer per server.
pub fn event_stream_cursor() -> &'static IntGauge {
    static M: OnceLock<IntGauge> = OnceLock::new();
    M.get_or_init(|| {
        let gauge = IntGauge::new(
            "noetl_event_stream_cursor",
            "noetl.event.id last published to the noetl_events stream by the CQRS write-path tailer.",
        )
        .expect("static gauge spec must be valid");
        registry()
            .register(Box::new(gauge.clone()))
            .expect("gauge registration must succeed");
        gauge
    })
}

/// Record a batch published by the tailer: bump the per-type counter for each
/// event and advance the cursor gauge.
pub fn record_event_stream_published(event_type: &str, count: u64, cursor: i64) {
    event_stream_published_total()
        .with_label_values(&[event_type])
        .inc_by(count);
    event_stream_cursor().set(cursor);
}

/// Counter: events the tailer SKIPPED (payload over NATS max), by type.  A
/// non-zero rate means oversized events aren't reaching the stream — visible
/// rather than silently wedging the cursor (noetl/ai-meta#103).
pub fn event_stream_skipped_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_event_stream_skipped_total",
                "Events the CQRS write-path tailer skipped because the payload exceeded the NATS max, by event type.",
            ),
            &["event_type"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one event skipped by the tailer (oversized payload).
pub fn record_event_stream_skipped(event_type: &str) {
    event_stream_skipped_total()
        .with_label_values(&[event_type])
        .inc();
}

/// Counter: executions whose `projection_snapshot` was advanced by the
/// `system/projector` playbook via `/api/internal/projection/advance`
/// (noetl/ai-meta#103 phase 2b).  No labels — one global rate.
pub fn projection_advanced_total() -> &'static prometheus::IntCounter {
    static M: OnceLock<prometheus::IntCounter> = OnceLock::new();
    M.get_or_init(|| {
        let counter = prometheus::IntCounter::new(
            "noetl_projection_advanced_total",
            "Executions whose projection_snapshot was advanced by the system/projector playbook.",
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record one execution's projection advance.
pub fn record_projection_advanced(_version: i64) {
    projection_advanced_total().inc();
}

/// Counter: `noetl.event` rows materialized from the stream by the
/// `system/event_materializer` playbook via `/api/internal/events/materialize`
/// (noetl/ai-meta#103 phase 2d).  No labels — one global rate.
pub fn events_materialized_total() -> &'static prometheus::IntCounter {
    static M: OnceLock<prometheus::IntCounter> = OnceLock::new();
    M.get_or_init(|| {
        let counter = prometheus::IntCounter::new(
            "noetl_events_materialized_total",
            "noetl.event rows materialized from the noetl_events stream by the system/event_materializer playbook.",
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Record a batch of materialized event rows.
pub fn record_events_materialized(rows: u64) {
    events_materialized_total().inc_by(rows);
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

/// Secrets-Wallet Phase 6c: residency-policy gate outcomes.
///
/// Labels are bounded enums:
/// - `policy`: `none` / `advisory` / `strict` — the `KeychainDef.residency`
///   value at evaluation time.
/// - `decision`: one of `allowed_no_policy` / `allowed_same_region` /
///   `allowed_in_allowlist` / `violation_allowed` / `violation_blocked`.
///
/// `policy="strict"` + `decision="violation_blocked"` is the alert-worthy
/// combination — it means the gate refused a resolution that would have
/// crossed a residency boundary.  `policy="advisory"` +
/// `decision="violation_allowed"` is the migration-window surface for
/// finding existing cross-region flows before flipping to `strict`.
pub fn secret_residency_check_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_secret_residency_check_total",
                "Residency-policy gate outcomes per keychain-entry \
                 resolution (Secrets Wallet Phase 6c).",
            ),
            &["policy", "decision"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Increment [`secret_residency_check_total`] by 1.
pub fn record_secret_residency_check(policy: &str, decision: &str) {
    secret_residency_check_total()
        .with_label_values(&[policy, decision])
        .inc();
}

/// Secrets-Wallet Phase 6d: histogram of issuer-reported dynamic-secret
/// time-to-expiry at resolution time.
///
/// Buckets span the common cloud-token TTLs:
/// `[60, 300, 900, 3600, 14400, 43200]` seconds = 1 min / 5 min / 15 min /
/// 1 h / 4 h / 12 h.  An operator watching this dashboard sees whether
/// their fleet is hot-pathing through short-lived creds (most calls
/// landing in the 1 min – 15 min buckets) or running off long-lived ones
/// (12 h+).
///
/// No labels: the metric tells a fleet-wide story; per-credential
/// inspection lives on the matching `secret.resolve` tracing span.
pub fn secret_dynamic_ttl_seconds() -> &'static prometheus::Histogram {
    static M: OnceLock<prometheus::Histogram> = OnceLock::new();
    M.get_or_init(|| {
        let h = prometheus::Histogram::with_opts(
            HistogramOpts::new(
                "noetl_secret_dynamic_ttl_seconds",
                "Issuer-reported time-to-expiry of resolved dynamic secrets (Phase 6d).",
            )
            .buckets(vec![60.0, 300.0, 900.0, 3600.0, 14400.0, 43200.0]),
        )
        .expect("static histogram spec must be valid");
        registry()
            .register(Box::new(h.clone()))
            .expect("histogram registration must succeed");
        h
    })
}

/// Observe one issuer-reported TTL (seconds).  Caller filters to the
/// dynamic-secret case (i.e. only when `SecretValue.expires_at` was set).
pub fn record_secret_dynamic_ttl(seconds: f64) {
    secret_dynamic_ttl_seconds().observe(seconds);
}

/// Secrets-Wallet Phase 6d: counter for keychain-cache writes the
/// resolver skipped.
///
/// `reason` is a bounded enum:
/// - `already_expired` — issuer's `expires_at` already in the past or
///   within the safety margin.  Caching would store something already
///   dead.
///
/// Future 6d-follow-up reasons may include `unsupported_scope`, etc.
pub fn secret_cache_skip_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_secret_cache_skip_total",
                "Keychain-cache writes skipped by reason (Phase 6d).",
            ),
            &["reason"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Increment [`secret_cache_skip_total`] by 1.
pub fn record_secret_cache_skip(reason: &str) {
    secret_cache_skip_total().with_label_values(&[reason]).inc();
}

/// Secrets-Wallet Phase 6e: cross-region broker call outcomes.
///
/// `broker_region` is the region the request was routed to (or `"-"`
/// for diagnostics paths that don't know).  `outcome` is a bounded
/// enum:
/// - `ok` — broker sealed the response and returned it.
/// - `unreachable` — network / DNS / TLS / 5xx from the broker.
/// - `denied_by_broker` — broker rejected the request (its own region
///   gate or local policy).
/// - `wrong_region` — broker's `server_region()` didn't match the
///   requested `expected_entry_region`.
/// - `bad_pubkey` — requesting peer sent a malformed worker public key.
/// - `resolve_error` / `serialize_error` / `seal_error` — broker-side
///   pipeline errors.
///
/// `wrong_region` is the alert-worthy combination — it means a peer's
/// broker registry is out of date.
pub fn cross_region_broker_call_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_secret_broker_call_total",
                "Cross-region broker call outcomes per broker_region (Phase 6e).",
            ),
            &["broker_region", "outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Increment [`cross_region_broker_call_total`] by 1.
pub fn record_cross_region_broker_call(broker_region: &str, outcome: &str) {
    let region_label = if broker_region.is_empty() {
        "-"
    } else {
        broker_region
    };
    cross_region_broker_call_total()
        .with_label_values(&[region_label, outcome])
        .inc();
}

/// Secrets-Wallet Phase 6e: histogram of cross-region broker call
/// wall-clock latency.  Buckets span the cross-region round-trip range
/// (`[0.05, 0.1, 0.25, 0.5, 1, 2, 5]`).  Caller observes regardless of
/// outcome so a dashboard shows "broker is slow" + "broker is failing"
/// independently.
pub fn cross_region_broker_call_duration_seconds() -> &'static HistogramVec {
    static M: OnceLock<HistogramVec> = OnceLock::new();
    M.get_or_init(|| {
        let h = HistogramVec::new(
            HistogramOpts::new(
                "noetl_secret_broker_call_duration_seconds",
                "Wall-clock seconds spent in a cross-region broker call.",
            )
            .buckets(vec![0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0]),
            &["broker_region"],
        )
        .expect("static histogram spec must be valid");
        registry()
            .register(Box::new(h.clone()))
            .expect("histogram registration must succeed");
        h
    })
}

/// Observe one cross-region broker call duration.
pub fn record_cross_region_broker_call_duration(broker_region: &str, seconds: f64) {
    let region_label = if broker_region.is_empty() {
        "-"
    } else {
        broker_region
    };
    cross_region_broker_call_duration_seconds()
        .with_label_values(&[region_label])
        .observe(seconds);
}

/// Secrets-Wallet Phase 7a: wallet KEK-rotation pass outcomes.
///
/// `table` is `credential` or `keychain` (the two `noetl.*` tables that
/// hold envelope-encrypted blobs).  `status` is a bounded enum:
/// - `skipped` — record already wrapped under the current KEK version.
/// - `rewrapped` — DEK was unwrapped under the old version and re-wrapped
///   under the current.
/// - `failed_unwrap` — provider can't produce the old KEK version (key
///   compromise + delete-all rotation; operator must reseed).
/// - `failed_wrap` — provider can't issue a fresh wrap (KMS reachability).
/// - `parse_error` — stored value isn't a valid envelope (forward-only
///   contract — re-register the record).
///
/// `failed_unwrap` is the alert-worthy combination — it means the
/// rotation can't complete without operator intervention.
pub fn wallet_rotate_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_wallet_rotate_total",
                "Wallet KEK-rotation pass outcomes per table (Phase 7a).",
            ),
            &["table", "status"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Increment [`wallet_rotate_total`] by 1.
pub fn record_wallet_rotate(table: &str, status: &str) {
    wallet_rotate_total()
        .with_label_values(&[table, status])
        .inc();
}

/// Secrets-Wallet Phase 7b: secret-resolution audit-write outcomes.
///
/// Labels are bounded enums:
/// - `operation`: matches `services::secret_audit::Operation::as_str` —
///   `get_sealed` / `cross_region_broker_serve` / `resolve_keychain` /
///   `get_credential`.
/// - `outcome`: the resolver's actual outcome at audit time —
///   `ok` / `residency_violation` / `broker_unreachable` / etc.
///   (mirrors `services::secret_audit::Outcome::as_str`).
/// - `status`: what happened to the audit write itself —
///   - `written` — sink confirmed the row landed.
///   - `dropped_async` — fire-and-forget write failed (logged + dropped).
///   - `failed_strict` — `NOETL_SECRET_AUDIT_REQUIRED=true` and the
///     sink errored.  **Alert-worthy.**
pub fn secret_audit_writes_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_secret_audit_writes_total",
                "Secret-resolution audit-write outcomes (Phase 7b).",
            ),
            &["operation", "outcome", "status"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Increment [`secret_audit_writes_total`] by 1.
pub fn record_secret_audit_write(operation: &str, outcome: &str, status: &str) {
    secret_audit_writes_total()
        .with_label_values(&[operation, outcome, status])
        .inc();
}

/// Secrets-Wallet Phase 7c: token auto-renewal outcomes.
///
/// `outcome` is a bounded enum:
/// - `triggered` — refresh decision made (will spawn or collapse).
/// - `succeeded` — refresh ran and the new value landed in the cache.
/// - `failed` — refresh ran but the provider errored.  The cached
///   row is **preserved** (we don't poison the cache on a transient
///   outage; the next natural cache miss after `expires_at` re-resolves).
/// - `stampede_collapsed` — concurrent request found a refresh already
///   in flight; piggy-backed on it.
///
/// `failed` at sustained rate is alert-worthy — it means a provider
/// is unreachable AND a cached token is about to expire.
pub fn secret_refresh_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_secret_refresh_total",
                "Token auto-renewal outcomes (Phase 7c).  Aliases are NOT \
                 labeled (cardinality); per-alias detail lives on the \
                 secret.refresh tracing span.",
            ),
            &["outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Increment [`secret_refresh_total`] by 1.
pub fn record_secret_refresh(outcome: &str) {
    secret_refresh_total().with_label_values(&[outcome]).inc();
}

// ---------------------------------------------------------------------------
// Result-store metrics (noetl/ai-meta#70)
// ---------------------------------------------------------------------------

/// Counter: `PUT /api/result/{execution_id}` calls by outcome status.
///
/// `status` ∈ { `ok`, `error` }.
/// `execution_id` is NOT a label (cardinality) — it lives on the
/// `result_store.put` tracing span per
/// [`agents/rules/observability.md`] Principle 4.
pub fn result_store_put_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_result_store_put_total",
                "PUT /api/result/{execution_id} calls by outcome status.",
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

/// Histogram: wall-clock time spent inside `PUT /api/result/{eid}`.
pub fn result_store_put_duration_seconds() -> &'static HistogramVec {
    static M: OnceLock<HistogramVec> = OnceLock::new();
    M.get_or_init(|| {
        let h = HistogramVec::new(
            HistogramOpts::new(
                "noetl_result_store_put_duration_seconds",
                "Wall-clock seconds for PUT /api/result/{execution_id}.",
            )
            .buckets(EVENT_INGEST_BUCKETS.to_vec()),
            &["status"],
        )
        .expect("static histogram spec must be valid");
        registry()
            .register(Box::new(h.clone()))
            .expect("histogram registration must succeed");
        h
    })
}

/// Record one `PUT /api/result/{eid}` outcome.
///
/// `bytes` is the stored payload size (0 on error paths).
/// `status` is `"ok"` or `"error"`.
pub fn record_result_store_put(duration_seconds: f64, bytes: usize, status: &str) {
    result_store_put_total()
        .with_label_values(&[status])
        .inc();
    result_store_put_duration_seconds()
        .with_label_values(&[status])
        .observe(duration_seconds);
    // Log bytes as a span field in the handler; Prometheus metric
    // stays label-free on bytes (unbounded cardinality for a gauge).
    let _ = bytes; // consumed for future histogram extension
}

/// Counter: `GET /api/result/resolve` calls by outcome status.
///
/// `status` ∈ { `ok`, `not_found`, `bad_request`, `error` }.
pub fn result_store_resolve_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_result_store_resolve_total",
                "GET /api/result/resolve calls by outcome status.",
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

/// Histogram: wall-clock time for `GET /api/result/resolve`.
pub fn result_store_resolve_duration_seconds() -> &'static HistogramVec {
    static M: OnceLock<HistogramVec> = OnceLock::new();
    M.get_or_init(|| {
        let h = HistogramVec::new(
            HistogramOpts::new(
                "noetl_result_store_resolve_duration_seconds",
                "Wall-clock seconds for GET /api/result/resolve.",
            )
            .buckets(EVENT_INGEST_BUCKETS.to_vec()),
            &["status"],
        )
        .expect("static histogram spec must be valid");
        registry()
            .register(Box::new(h.clone()))
            .expect("histogram registration must succeed");
        h
    })
}

/// Record one `GET /api/result/resolve` outcome.
pub fn record_result_store_resolve(duration_seconds: f64, status: &str) {
    result_store_resolve_total()
        .with_label_values(&[status])
        .inc();
    result_store_resolve_duration_seconds()
        .with_label_values(&[status])
        .observe(duration_seconds);
}

/// Secrets-Wallet Phase 7c: histogram of token auto-renewal wall-clock
/// latency.  Buckets `[0.05, 0.1, 0.25, 0.5, 1, 2, 5]` — span the range
/// where auth round-trips actually live.  Observed regardless of
/// outcome so a dashboard surfaces "refresh is slow" + "refresh is
/// failing" independently.
pub fn secret_refresh_duration_seconds() -> &'static prometheus::Histogram {
    static M: OnceLock<prometheus::Histogram> = OnceLock::new();
    M.get_or_init(|| {
        let h = prometheus::Histogram::with_opts(
            HistogramOpts::new(
                "noetl_secret_refresh_duration_seconds",
                "Wall-clock seconds spent in one token auto-renewal (Phase 7c).",
            )
            .buckets(vec![0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0]),
        )
        .expect("static histogram spec must be valid");
        registry()
            .register(Box::new(h.clone()))
            .expect("histogram registration must succeed");
        h
    })
}

/// Observe one refresh duration.
pub fn record_secret_refresh_duration(seconds: f64) {
    secret_refresh_duration_seconds().observe(seconds);
}

// ---------------------------------------------------------------------------
// Subscription scale hardening (noetl/ai-meta#90 Phase 7)
// ---------------------------------------------------------------------------

/// Counter: executions created, bucketed by the `/api/execute` entry path
/// (`single` | `batch`) and the dedup outcome (`new` | `duplicate` | `error`).
/// Lets an operator see batch-dispatch uptake and how often the opt-in dedup
/// window is collapsing duplicates without grepping logs.
pub fn execute_outcomes_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_execute_outcomes_total",
                "Executions handled by /api/execute(/batch), bucketed by entry path and dedup outcome (noetl/ai-meta#90 Phase 7).",
            ),
            &["entry", "outcome"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Histogram: `POST /api/execute/batch` request sizes (number of executions in
/// one HTTP round-trip).  Answers "is the runtime actually batching, and how
/// deep" — the whole point of Phase 7's batch dispatch.
pub fn execute_batch_size() -> &'static HistogramVec {
    static M: OnceLock<HistogramVec> = OnceLock::new();
    M.get_or_init(|| {
        let hist = HistogramVec::new(
            HistogramOpts::new(
                "noetl_execute_batch_size",
                "Number of executions submitted in one POST /api/execute/batch call (noetl/ai-meta#90 Phase 7).",
            )
            .buckets(vec![1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0]),
            &[],
        )
        .expect("static histogram spec must be valid");
        registry()
            .register(Box::new(hist.clone()))
            .expect("histogram registration must succeed");
        hist
    })
}

/// Record one execution outcome.  `entry` is `"single"` or `"batch"`;
/// `outcome` is `"new"` (execution created), `"duplicate"` (dedup window
/// collapsed it), or `"error"`.
pub fn record_execute_outcome(entry: &str, outcome: &str) {
    execute_outcomes_total()
        .with_label_values(&[entry, outcome])
        .inc();
}

/// Observe one batch-dispatch request size.
pub fn record_execute_batch_size(n: usize) {
    execute_batch_size().with_label_values(&[]).observe(n as f64);
}

/// Render the global registry as Prometheus text-exposition
/// format.  Used by the `GET /metrics` handler.
pub fn gather_text() -> Result<String, prometheus::Error> {
    let encoder = TextEncoder::new();
    let metric_families = registry().gather();
    encoder.encode_to_string(&metric_families)
}

// ---------------------------------------------------------------------------
// Container Tool Callback (noetl/ai-meta#43 Round 2 — noetl/server#140)
// ---------------------------------------------------------------------------

/// Counter for in-flight container-callback emits, labelled by terminal
/// state.  Sister metric to [`container_callback_stale_total`] (stale path).
/// Together they answer "how many Job terminations did the watcher
/// observe, and what fraction matched a live execution".
pub fn container_callback_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_container_callback_total",
                "Container-tool callback receives that matched an in-flight \
                 execution and emitted a call.done event (Container Tool \
                 Callback umbrella, noetl/ai-meta#43).",
            ),
            &["state"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Increment [`container_callback_total`] by 1.
pub fn record_container_callback(state: &str) {
    container_callback_total()
        .with_label_values(&[state])
        .inc();
}

/// Counter for stale container-callback receives — Job terminations the
/// watcher observed for executions that don't exist on this server.
/// **Alert-worthy when sustained** — usually means a stale watcher
/// pointing at the wrong namespace, or a Job created out-of-band.
pub fn container_callback_stale_total() -> &'static IntCounterVec {
    static M: OnceLock<IntCounterVec> = OnceLock::new();
    M.get_or_init(|| {
        let counter = IntCounterVec::new(
            Opts::new(
                "noetl_container_callback_stale_total",
                "Container-tool callback receives that did NOT match any \
                 in-flight execution (stale — execution gc'd, watcher \
                 mis-namespaced, or Job created out-of-band).",
            ),
            &["state"],
        )
        .expect("static counter spec must be valid");
        registry()
            .register(Box::new(counter.clone()))
            .expect("counter registration must succeed");
        counter
    })
}

/// Increment [`container_callback_stale_total`] by 1.
pub fn record_container_callback_stale(state: &str) {
    container_callback_stale_total()
        .with_label_values(&[state])
        .inc();
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
