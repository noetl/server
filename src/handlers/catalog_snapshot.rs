//! Execution-start catalog snapshot (noetl/ai-meta catalog-relation step 1).
//!
//! At execution start this records, as an **immutable event tied to the
//! execution**, exactly which catalog item the execution is running: its
//! `catalog_id`, `path`, `kind`, `version`, and a `content_sha256` over the bytes
//! `parse_playbook` actually consumed.
//!
//! # Why this exists at all
//!
//! A catalog reference by `path` resolves to "the latest version", which is a
//! moving target. Register a new version of a path and every *past* execution's
//! reference now points somewhere else — so "what did this run actually execute?"
//! becomes unanswerable, and answerably-wrong rather than obviously-wrong.
//! Pinning `version` fixes that for registered versions; pinning
//! `content_sha256` fixes it outright, because a hash already written to an
//! append-only log cannot be altered by any later catalog change.
//!
//! # The elegant part
//!
//! Because the snapshot lands in the immutable append-only event log, the
//! ephemeral-version and cross-instance-drift problem is solved *by
//! construction*. The log is the ground-truth record of exactly what ran; a
//! catalog relation folded from it is only a queryable index over that record. A
//! structure that existed for one execution and was never registered is still
//! fully recorded.
//!
//! # Fail-safe
//!
//! A snapshot is a record *about* an execution, never a precondition *for* one.
//! Every failure path here is swallowed and metered. An execution must never fail
//! because its snapshot could not be written.

use serde::Serialize;
use sha2::{Digest, Sha256};

/// `NOETL_CATALOG_SNAPSHOT` — whether, and how much, to snapshot.
pub const MODE_ENV: &str = "NOETL_CATALOG_SNAPSHOT";

/// `NOETL_CATALOG_SNAPSHOT_MAX_BYTES` — the `full`-mode content cap.
pub const MAX_BYTES_ENV: &str = "NOETL_CATALOG_SNAPSHOT_MAX_BYTES";

/// Default cap: 1 MiB, chosen above the largest content measured in a live
/// catalog (267,388 bytes — `muno/playbooks/itinerary-planner` v17) so `full`
/// mode covers the whole current corpus rather than silently starting to omit.
pub const DEFAULT_MAX_BYTES: usize = 1024 * 1024;

/// The event type. Deliberately **not** a field on `playbook_started`: that
/// event's `context` is read by the fold (`state.rs:508` takes `workload`,
/// `path` and `version` from it), so adding keys there could change
/// `canonical_state_digest` for every execution — the exact digests
/// noetl/ai-meta#307's comparator compares. `WorkflowState::apply_event` ends in
/// a `_ => {}` fallback, so an unrecognised event type contributes nothing to the
/// folded state.
pub const EVENT_TYPE: &str = "execution.catalog_snapshot";

/// The snapshot record's own schema version, so a reader can tell a v1 record
/// from a future shape without inferring it from which keys happen to be present.
pub const SNAPSHOT_VERSION: i64 = 1;

/// How much of the catalog item to record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// No snapshot event. Byte-identical to before this shipped, and the default,
    /// so this lands inert.
    Off,
    /// Metadata + `content_sha256` + `content_bytes`. ~250 bytes per execution.
    /// **The pin is complete in this mode**: the hash identifies the exact bytes,
    /// and the bytes themselves are recoverable from the immutable catalog row.
    Digest,
    /// As `Digest`, plus the content, up to the cap. Buys self-sufficiency — the
    /// log alone reconstructs the item — which matters only against
    /// `delete_catalog_entries`, the one hard-delete path.
    Full,
}

/// Every mode, pinned as metric labels.
pub const MODES: [&str; 3] = ["off", "digest", "full"];

/// Every snapshot outcome, pinned as metric labels. A closed set, so each is a
/// visible 0 rather than an absent series.
pub const OUTCOMES: [&str; 5] = [
    "recorded",
    "content_omitted",
    "catalog_read_failed",
    "emit_failed",
    "disabled",
];

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Digest => "digest",
            Self::Full => "full",
        }
    }
    pub fn enabled(self) -> bool {
        !matches!(self, Self::Off)
    }
    /// Whether the content itself is carried.
    pub fn carries_content(self) -> bool {
        matches!(self, Self::Full)
    }
}

/// Resolve the configured mode.
pub fn mode() -> Mode {
    parse_mode(std::env::var(MODE_ENV).ok().as_deref())
}

/// The parse, without the environment — so the rules are testable without
/// `set_var`, which would race (`cargo test` does not serialise tests).
///
/// Unrecognised resolves to `Off`, not to a snapshotting mode: a typo must not
/// silently start writing a payload to every execution's event log. Same
/// direction as `ehdb_projection_fold::parse_recovery_source`, for the reason
/// noetl/ai-meta#243 records.
pub fn parse_mode(raw: Option<&str>) -> Mode {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("digest") => Mode::Digest,
        Some("full") => Mode::Full,
        _ => Mode::Off,
    }
}

/// The `full`-mode content cap.
pub fn max_bytes() -> usize {
    std::env::var(MAX_BYTES_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_MAX_BYTES)
}

/// Lowercase hex sha256.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// The catalog identity a snapshot pins.
#[derive(Debug, Clone)]
pub struct CatalogItem {
    pub catalog_id: i64,
    pub path: String,
    pub kind: String,
    pub version: i16,
}

/// The snapshot body, as it lands in the event's `context`.
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub snapshot_version: i64,
    pub catalog_id: String,
    pub execution_id: String,
    pub path: String,
    pub kind: String,
    pub version: i16,
    /// Pins the exact bytes. **This is the load-bearing field** — everything else
    /// is convenience.
    pub content_sha256: String,
    /// Always present, even when the content is not, so a reader can tell "small
    /// and omitted" from "large and omitted" without the bytes.
    pub content_bytes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    pub content_included: bool,
    /// Set **iff** `content_included` is false. Content is never truncated: a
    /// truncated snapshot is a lie about what ran, and it would hash to something
    /// that matches nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_omitted_reason: Option<String>,
    /// Over the **effective** workload — the playbook's `workload:` defaults
    /// overlaid with the request's `payload:` — not the request alone. That is
    /// what the execution actually started with.
    pub workload_sha256: String,
}

/// Build the snapshot body.
///
/// Pure: no clock, no environment, no IO, so every rule above is reachable by a
/// test rather than only by a running server.
pub fn build(
    item: &CatalogItem,
    execution_id: i64,
    content: &str,
    workload: &serde_json::Value,
    mode: Mode,
    max_bytes: usize,
) -> Snapshot {
    let content_bytes = content.len();
    let (content_out, included, reason) = if !mode.carries_content() {
        (None, false, Some(format!("mode={}", mode.as_str())))
    } else if content_bytes > max_bytes {
        (
            None,
            false,
            Some(format!("content {content_bytes} bytes exceeds cap {max_bytes}")),
        )
    } else {
        (Some(content.to_string()), true, None)
    };

    Snapshot {
        snapshot_version: SNAPSHOT_VERSION,
        catalog_id: item.catalog_id.to_string(),
        execution_id: execution_id.to_string(),
        path: item.path.clone(),
        kind: item.kind.clone(),
        version: item.version,
        content_sha256: sha256_hex(content.as_bytes()),
        content_bytes,
        content: content_out,
        content_included: included,
        content_omitted_reason: reason,
        // `to_string` on a serde_json::Value is key-ordered for maps (serde_json
        // preserves BTreeMap ordering by default), so this is stable across
        // processes for the same logical workload.
        workload_sha256: sha256_hex(workload.to_string().as_bytes()),
    }
}

/// Read the catalog identity and record the snapshot event.
///
/// # Fail-safe by construction
///
/// Returns `()`. Every failure — the catalog read, the id generation, the emit —
/// is swallowed and metered. A snapshot is a record *about* an execution, never a
/// precondition *for* one, and an execution that failed to start because its
/// audit record could not be written would be a strictly worse system than one
/// with no audit record.
///
/// Called **after** `playbook_started` so that event remains the execution's
/// first event and the chain root — `get_latest_event(.., "playbook_started")`
/// and the descriptor seeding both depend on that ordering.
pub async fn record(
    state: &crate::state::AppState,
    execution_id: i64,
    item: &CatalogItem,
    content: &str,
    workload: &serde_json::Value,
) {
    let mode = mode();
    if !mode.enabled() {
        crate::metrics::record_catalog_snapshot(mode.as_str(), "disabled");
        return;
    }

    let catalog_id = item.catalog_id;

    // The path/kind/version this snapshot names come from the caller's single
    // catalog read (noetl/ai-meta#319 P2), not from a re-read here.
    //
    // The re-read existed so the mode would cost exactly nothing when off — but
    // `digest` is what production runs, so in practice it was a FOURTH read of a
    // row already in memory, on every execution. The property it protected still
    // holds, and now holds more strongly: the snapshot names the path as the
    // CATALOG holds it (the `path` column, not the request's spelling), and it is
    // now provably the SAME read the execution resolved through — so the snapshot
    // cannot name a different row than the one that ran. Two reads could disagree
    // if the catalog changed between them; one cannot.
    //
    // Off-mode still costs nothing: this function returns above, and the caller
    // builds its `CatalogItem` from values it already holds.

    let snap = build(item, execution_id, content, workload, mode, max_bytes());
    let outcome = if snap.content_included || !mode.carries_content() {
        "recorded"
    } else {
        "content_omitted"
    };

    let Ok(event_id) = state.snowflake.generate() else {
        crate::metrics::record_catalog_snapshot(mode.as_str(), "emit_failed");
        return;
    };
    let context = match serde_json::to_value(&snap) {
        Ok(v) => v,
        Err(_) => {
            crate::metrics::record_catalog_snapshot(mode.as_str(), "emit_failed");
            return;
        }
    };

    let ev = crate::handlers::event_write::EventRow::new(
        event_id,
        execution_id,
        catalog_id,
        EVENT_TYPE,
        "RECORDED",
        chrono::Utc::now(),
    )
    .with_nodes("catalog_snapshot", &item.path)
    .with_node_type("catalog")
    .with_context(context)
    .with_meta(serde_json::json!({
        "emitted_at": chrono::Utc::now().to_rfc3339(),
        "emitter": "control_plane",
        "snapshot_mode": mode.as_str(),
    }));

    if crate::handlers::event_write::emit_event(
        state,
        state.pools.pool_for(execution_id),
        ev,
    )
    .await
    .is_err()
    {
        crate::metrics::record_catalog_snapshot(mode.as_str(), "emit_failed");
        tracing::warn!(
            target: "noetl_server::catalog_snapshot",
            execution_id, catalog_id,
            "catalog snapshot event could not be emitted; execution continues"
        );
        return;
    }

    crate::metrics::record_catalog_snapshot(mode.as_str(), outcome);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item() -> CatalogItem {
        CatalogItem {
            catalog_id: 7,
            path: "muno/playbooks/itinerary-planner".into(),
            kind: "playbook".into(),
            version: 17,
        }
    }

    /// The default is OFF, so this lands inert.
    #[test]
    fn snapshotting_is_off_unless_asked_for() {
        assert_eq!(parse_mode(None), Mode::Off);
        assert_eq!(parse_mode(Some("")), Mode::Off);
        assert!(!Mode::Off.enabled());
    }

    /// A typo must not start writing to every execution's event log.
    #[test]
    fn an_unrecognised_mode_does_not_start_snapshotting() {
        for junk in ["digset", "FULL!", "true", "1", "on", "yes"] {
            assert_eq!(
                parse_mode(Some(junk)),
                Mode::Off,
                "{junk} must not enable snapshot writes"
            );
        }
        assert_eq!(parse_mode(Some(" Digest ")), Mode::Digest);
        assert_eq!(parse_mode(Some("FULL")), Mode::Full);
    }

    /// ⭐ The property the whole design rests on: the snapshot pins the CONTENT,
    /// not merely the version. Two different contents at the same (path,
    /// version) must not produce the same snapshot — otherwise a mutated
    /// ephemeral structure would be indistinguishable from the registered one.
    #[test]
    fn the_snapshot_pins_content_not_just_the_version() {
        let w = serde_json::json!({"city": "Rome"});
        let a = build(&item(), 1, "steps: [a]", &w, Mode::Digest, DEFAULT_MAX_BYTES);
        let b = build(&item(), 1, "steps: [b]", &w, Mode::Digest, DEFAULT_MAX_BYTES);
        assert_eq!(a.version, b.version, "same catalog version, by construction");
        assert_ne!(
            a.content_sha256, b.content_sha256,
            "identical digests for different content would make an ephemeral \
             structure indistinguishable from the registered one"
        );
    }

    /// The same content always hashes the same — the other half of the pin.
    #[test]
    fn the_same_content_pins_identically() {
        let w = serde_json::json!({});
        let a = build(&item(), 1, "steps: [a]", &w, Mode::Digest, DEFAULT_MAX_BYTES);
        let b = build(&item(), 2, "steps: [a]", &w, Mode::Digest, DEFAULT_MAX_BYTES);
        assert_eq!(a.content_sha256, b.content_sha256);
    }

    /// The effective workload is pinned, so a request payload that overrode a
    /// playbook default is visible in the record.
    #[test]
    fn the_effective_workload_is_pinned_not_the_request_alone() {
        let a = build(
            &item(), 1, "x", &serde_json::json!({"city": "Rome"}),
            Mode::Digest, DEFAULT_MAX_BYTES,
        );
        let b = build(
            &item(), 1, "x", &serde_json::json!({"city": "Paris"}),
            Mode::Digest, DEFAULT_MAX_BYTES,
        );
        assert_ne!(
            a.workload_sha256, b.workload_sha256,
            "two executions of the same playbook with different effective \
             workloads must not look identical"
        );
    }

    /// `digest` mode omits content but the pin is still complete.
    #[test]
    fn digest_mode_omits_content_but_keeps_the_pin() {
        let s = build(&item(), 1, "steps: [a]", &serde_json::json!({}), Mode::Digest, DEFAULT_MAX_BYTES);
        assert!(s.content.is_none());
        assert!(!s.content_included);
        assert_eq!(s.content_omitted_reason.as_deref(), Some("mode=digest"));
        assert_eq!(s.content_bytes, "steps: [a]".len(), "the size is recorded even when the bytes are not");
        assert_eq!(s.content_sha256.len(), 64);
    }

    /// `full` mode carries the content verbatim.
    #[test]
    fn full_mode_carries_the_content() {
        let s = build(&item(), 1, "steps: [a]", &serde_json::json!({}), Mode::Full, DEFAULT_MAX_BYTES);
        assert_eq!(s.content.as_deref(), Some("steps: [a]"));
        assert!(s.content_included);
        assert!(s.content_omitted_reason.is_none());
    }

    /// Over the cap the content is OMITTED WITH A REASON, never truncated.
    ///
    /// Truncation would hash to something matching nothing, and would claim to
    /// be a record of what ran while being a record of a prefix of it.
    #[test]
    fn oversized_content_is_omitted_with_a_reason_never_truncated() {
        let big = "y".repeat(500);
        let s = build(&item(), 1, &big, &serde_json::json!({}), Mode::Full, 100);
        assert!(s.content.is_none(), "content must not be carried over the cap");
        assert!(!s.content_included);
        let r = s.content_omitted_reason.expect("an omission must say why");
        assert!(r.contains("500") && r.contains("100"), "reason must name both sizes: {r}");
        assert_eq!(
            s.content_sha256,
            sha256_hex(big.as_bytes()),
            "the digest must still pin the WHOLE content, not the part that fit"
        );
    }

    /// The default cap clears the largest content measured in a live catalog.
    #[test]
    fn the_default_cap_clears_the_largest_real_playbook() {
        assert!(
            DEFAULT_MAX_BYTES > 267_388,
            "the default cap must not silently start omitting the itinerary planner"
        );
    }

    #[test]
    fn every_mode_and_outcome_label_is_pinned() {
        for m in [Mode::Off, Mode::Digest, Mode::Full] {
            let expect = match m {
                Mode::Off => "off",
                Mode::Digest => "digest",
                Mode::Full => "full",
            };
            assert_eq!(m.as_str(), expect);
            assert!(MODES.contains(&m.as_str()));
        }
        assert_eq!(MODES.len(), 3);
        assert_eq!(OUTCOMES.len(), 5);
    }
}
