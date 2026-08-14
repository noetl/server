//! Mirror the authoritative orchestrator read model into the EHDB projection
//! tier.
//!
//! **[ai-meta#265](https://github.com/noetl/ai-meta/issues/265) A3.** The
//! projection-tier twin of [`super::ehdb_eventlog_mirror`], and the survey that
//! preceded it changed three things about what "the projection tier mirrors"
//! means. They are recorded here because each one is a way this module could
//! have been built to pass its own tests while measuring nothing.
//!
//! # 1. `noetl.projection` is not the target — it is a dead table
//!
//! The charter names "the `projection` + `projection_snapshot` tables". Half of
//! that is wrong. `noetl.projection` has **0 rows** and **no writer anywhere in
//! the Rust control plane**; its only writer is the retired Python
//! `noetl/core/projection_store/postgres.py`. It is the same shape of thing as
//! `noetl.execution.status` (ai-meta#235): a table that reads like live data and
//! is frozen.
//!
//! A mirror and comparator pointed at it would have compared an empty table to
//! an empty tier and reported perfect parity, forever, on a tier holding
//! nothing.
//!
//! # 2. The authoritative store has exactly ONE writer, so the mirror goes inside it
//!
//! `noetl.projection_snapshot` (`aggregate_type = orchestrator_workflow_state`)
//! is written from **one** `INSERT` site: [`crate::services::orch_snapshot::save`].
//!
//! That is a stronger position than the event log ever had. `emit_events` was
//! described as "the one chokepoint every authoritative event passes through"
//! and was not — two in-transaction writers bypassed it, exactly on the system
//! pool, and the tier served 11 of 13 events while reporting no divergence
//! (ai-meta#263). The fix there was a third call site plus a test that *counts*
//! INSERT sites.
//!
//! Here the mirror sits **inside `save`**, after its upsert succeeds. A bypass
//! would have to be a second `INSERT INTO noetl.projection_snapshot`, and
//! `tests::the_snapshot_store_has_exactly_one_writer` counts them. Naming the
//! known writer would not catch a new one; counting does.
//!
//! # 3. Content parity is a checksum the incumbent already computed
//!
//! The event-log comparator explicitly cannot compare bodies: the server
//! rewrites the producer's `context` into a `result` envelope and sanitises it,
//! so byte-identity is *defined* to fail and comparing it would report 100%
//! divergence.
//!
//! The snapshot has no such rewrite, and `save` already computes
//! `sha256(snapshot)` for the row it stores. The mirror carries that digest
//! verbatim. So content parity is a string comparison against a value the
//! **incumbent** authored — nothing is re-derived here, so nothing can disagree
//! by having been derived differently. That is the strongest form of this check
//! available anywhere in the tier program, and it is available only because the
//! incumbent happened to already store one.
//!
//! # Failure posture
//!
//! Best-effort and isolated, exactly like the event-log mirror. `save` is on the
//! orchestrator's path; an auxiliary verification append must never be able to
//! fail a read model the platform has already committed. Every failure is
//! metered and logged and then dropped — the consequence is a `missing_version`
//! on the comparator, which is loud, attributable, and what the comparator is
//! for.
//!
//! Default-off behind `NOETL_EHDB_PROJECTION_MIRROR_SOURCE=server`.

use std::time::Duration;

use serde_json::json;
use tracing::warn;

/// `NOETL_EHDB_PROJECTION_MIRROR_SOURCE` — read by the server AND the worker.
///
/// **Deliberately not the event log's variable.** The two tiers cut over
/// independently: the event log is `primary` in prod today and the projection
/// tier is not, so one shared variable would make arming this mirror a change to
/// the event log's configuration. That is how a tier-2 experiment becomes a
/// tier-1 incident.
pub const MIRROR_SOURCE_ENV: &str = "NOETL_EHDB_PROJECTION_MIRROR_SOURCE";

/// The tier name, on the wire and on every metric label.
pub const TIER: &str = "projection";

/// How long to wait on the relay before giving up on one snapshot.
///
/// Short on purpose: this sits inline on the orchestrator's snapshot write, so
/// the cost of a hung worker is bounded latency on that write, not a stall.
const APPEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether the server is the projection mirror's producer in this process.
///
/// Only `server` arms it. The worker has no `worker` mode that could mean
/// anything here — it cannot read `noetl.projection_snapshot` at all
/// (`data-access-boundary.md`), so it has nothing to mirror. Anything
/// unrecognised, including a typo, is off: the failure mode of a typo must be
/// "no mirror" rather than "a mirror of something else".
pub fn server_mirrors() -> bool {
    matches!(
        std::env::var(MIRROR_SOURCE_ENV)
            .ok()
            .map(|s| s.trim().to_ascii_lowercase())
            .as_deref(),
        Some("server")
    )
}

/// The tier record for one authoritative snapshot row.
///
/// Two audiences, as on the event log.
///
/// The **comparator** reads `version`, `checksum` and `applied_count` — the
/// identifying projection of the row. Those three answer "is the tier holding
/// this execution's read model, at the right revision, with the right content".
///
/// The tier is also meant to eventually *become* the store, so the `snapshot`
/// body goes in as well. A record carrying only the digest would be one you
/// could verify and not one you could serve from, which is the difference
/// between a checksum log and a read model.
pub fn mirror_payload(
    execution_id: i64,
    version: i64,
    applied_count: i64,
    checksum: &str,
    snapshot: &serde_json::Value,
) -> serde_json::Value {
    json!({
        // --- the comparator's identifying projection -------------------------
        "execution_id": execution_id,
        // The snapshot watermark: the highest `event_id` folded in. Monotonic
        // per execution, which is what lets the comparator say "the tier is
        // BEHIND" rather than only "the tier disagrees".
        "version": version,
        // sha256 of `snapshot`, computed by `orch_snapshot::save` for the row it
        // stores. Carried, never recomputed — see the module note.
        "checksum": checksum,
        "applied_count": applied_count,
        // --- the read model itself -------------------------------------------
        "snapshot": snapshot,
        // Provenance. Without it, "the server mirror was rolled" and "something
        // else wrote this" are the same evidence after the fact.
        "mirror_source": "server",
        "aggregate_type": "orchestrator_workflow_state",
    })
}

/// One HTTP client for the relay, shared across calls.
fn relay_client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(reqwest::Client::new)
}

/// Mirror one authoritative snapshot into the projection tier.
///
/// Called from inside [`crate::services::orch_snapshot::save`], **after** its
/// upsert has succeeded — so a snapshot that failed to become authoritative is
/// never mirrored, and the tier can never be ahead of the incumbent by way of a
/// write that did not happen.
///
/// Never returns an error. See the module note on failure posture.
pub async fn mirror_snapshot(
    execution_id: i64,
    version: i64,
    applied_count: i64,
    checksum: &str,
    snapshot: &serde_json::Value,
) {
    if !server_mirrors() {
        return;
    }
    let Some(base) = std::env::var(crate::handlers::ehdb::WORKER_QUERY_URL_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        // Asked to mirror with nowhere to mirror to. A misconfiguration, not a
        // quiet no-op: without this line the tier stays empty and the comparator
        // reports a missing_execution whose cause is two hops away.
        crate::metrics::record_ehdb_projection_mirror("unconfigured");
        warn!(
            target: "noetl_server::ehdb_projection_mirror",
            execution_id,
            "{MIRROR_SOURCE_ENV}=server but {} is unset — the projection snapshot at version \
             {version} was not mirrored into the projection tier",
            crate::handlers::ehdb::WORKER_QUERY_URL_ENV,
        );
        return;
    };

    // Same route the read resolves through, for the same reason the event log's
    // append does: writing where the comparator reads is then true by
    // construction rather than by two env vars agreeing.
    let url = format!("{}/ehdb/tiers/{TIER}", base.trim_end_matches('/'));
    let record = mirror_payload(execution_id, version, applied_count, checksum, snapshot);
    let body = json!({
        "execution_id": execution_id.to_string(),
        "records": [record.to_string()],
    });

    let resp = relay_client()
        .post(&url)
        .json(&body)
        .timeout(APPEND_TIMEOUT)
        .send()
        .await;

    match resp {
        Err(e) => {
            crate::metrics::record_ehdb_projection_mirror("unavailable");
            warn!(
                target: "noetl_server::ehdb_projection_mirror",
                execution_id, version, error = %e,
                "projection tier mirror relay failed"
            );
        }
        Ok(r) => {
            let status = r.status();
            if status == reqwest::StatusCode::NOT_IMPLEMENTED
                || status == reqwest::StatusCode::METHOD_NOT_ALLOWED
            {
                // Two causes, one meaning: this worker cannot accept
                // server-authored projection appends, and the fix is a rollout
                // rather than an outage.
                //
                //   501 — rolled, but not in `server` mirror mode for this tier.
                //   405 — NOT rolled far enough: the GET route exists on older
                //         workers, so axum answers a POST with "method not
                //         allowed" rather than 404.
                //
                // Counting either as `degraded` would file the single most
                // likely operational state — server rolled ahead of worker —
                // under a label meaning "the append was refused".
                crate::metrics::record_ehdb_projection_mirror("unconfigured");
                warn!(
                    target: "noetl_server::ehdb_projection_mirror",
                    execution_id, version, status = status.as_u16(),
                    "worker cannot accept server-authored projection appends — is it rolled, \
                     and is {MIRROR_SOURCE_ENV}=server set on it?"
                );
            } else if status.is_success() {
                crate::metrics::record_ehdb_projection_mirror("mirrored");
            } else {
                let detail = r.text().await.unwrap_or_default();
                crate::metrics::record_ehdb_projection_mirror("degraded");
                warn!(
                    target: "noetl_server::ehdb_projection_mirror",
                    execution_id, version, status = status.as_u16(),
                    detail = %detail.chars().take(400).collect::<String>(),
                    "projection tier mirror was refused"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> serde_json::Value {
        json!({"execution_id": 7, "steps": {"a": "done"}})
    }

    #[test]
    fn the_payload_answers_to_the_comparators_field_names() {
        // The contract between the two halves, asserted on the value that goes
        // over the wire rather than on the code that builds it. A record missing
        // `version` would be reported as a divergence on every healthy
        // execution, which is how a comparator teaches an operator to ignore it.
        let p = mirror_payload(7, 991, 12, "abc123", &snap());
        assert_eq!(p["execution_id"], 7);
        assert_eq!(p["version"], 991);
        assert_eq!(p["checksum"], "abc123");
        assert_eq!(p["applied_count"], 12);
        // ...and the read model itself, so the tier could serve from it.
        assert_eq!(p["snapshot"], snap());
        assert_eq!(p["mirror_source"], "server");
        assert_eq!(p["aggregate_type"], "orchestrator_workflow_state");
    }

    #[test]
    fn the_checksum_is_carried_and_never_recomputed() {
        // The property the module note argues for: the digest in the record is
        // the incumbent's, byte for byte. If a later edit recomputed it here,
        // the two sides would be derived by two code paths free to disagree —
        // and the comparator would then be checking this module against itself.
        let p = mirror_payload(7, 1, 1, "not-a-real-sha-but-must-survive", &snap());
        assert_eq!(p["checksum"], "not-a-real-sha-but-must-survive");
    }

    /// Strip `//`-comments so a guard counts CODE, not text about code.
    ///
    /// This exists because the guard below caught its own first false positive:
    /// the doc comment on `orch_snapshot::save` explains where the mirror sits
    /// and, in doing so, writes the words `INSERT INTO noetl.projection_snapshot`.
    /// A naive `matches()` counted that as a second writer.
    ///
    /// The reverse direction is the one that actually matters. Left unfixed, the
    /// count could be *satisfied* by deleting a comment while adding a real
    /// writer — a guard that can be silenced by editing prose is not a guard.
    /// This is the same defect the reachability sweep found twice, where a doc
    /// comment naming a function cleared a "does anything call it" check.
    fn code_only(src: &str) -> String {
        src.lines()
            .map(|l| match l.find("//") {
                Some(i) => &l[..i],
                None => l,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The authoritative snapshot store must have exactly ONE writer, because
    /// the mirror sits inside it.
    ///
    /// This is the ai-meta#263 guard, applied before the defect rather than
    /// after. There, `emit_events` was documented as the one chokepoint every
    /// event passes through; two in-transaction writers bypassed it, on the
    /// system pool specifically, and the tier served 11 of 13 while reporting
    /// `unmirrored_by_design = 0`.
    ///
    /// **Counting, not naming.** A test that listed the writers it already knew
    /// about is exactly the test that would have passed through #263. A second
    /// `INSERT` added anywhere in the service fails this instead.
    #[test]
    fn the_snapshot_store_has_exactly_one_writer() {
        let src = code_only(include_str!("../services/orch_snapshot.rs"));
        // Positive control on the stripper: it must not have eaten the real
        // statement. Without this, a stripper bug would report 0 writers and the
        // assertion below would be satisfied by finding nothing.
        assert!(
            src.contains("INSERT INTO noetl.projection_snapshot"),
            "the comment stripper removed the real INSERT — this guard would then \
             pass by measuring an empty string"
        );
        let inserts = src.matches("INSERT INTO noetl.projection_snapshot").count();
        assert_eq!(
            inserts, 1,
            "orch_snapshot.rs has {inserts} `noetl.projection_snapshot` INSERT site(s) in \
             CODE. The projection mirror sits inside the single writer; a second one \
             bypasses it and the tier serves a stale read model while reporting no \
             divergence (the shape of noetl/ai-meta#263)."
        );
        assert!(
            src.contains("ehdb_projection_mirror::mirror_snapshot"),
            "the single writer must CALL the mirror — without this the count above is \
             satisfied by a writer that mirrors nothing"
        );
    }

    /// No other module may write the snapshot table either.
    ///
    /// The test above proves `orch_snapshot.rs` has one writer. It says nothing
    /// about `handlers/events.rs`, which is where #263's bypasses lived.
    #[test]
    fn no_handler_writes_the_snapshot_table_directly() {
        for (name, src) in [
            ("handlers/events.rs", include_str!("events.rs")),
            ("handlers/internal.rs", include_str!("internal.rs")),
        ] {
            let n = code_only(src).matches("INTO noetl.projection_snapshot").count();
            assert_eq!(
                n, 0,
                "{name} writes noetl.projection_snapshot directly ({n} site(s)) — it must \
                 go through services::orch_snapshot::save, which is where the mirror is"
            );
        }
    }

    #[test]
    fn a_typo_disarms_rather_than_arming_something_else() {
        // Asserted on the parse rule rather than the process env, because
        // `cargo test` does not serialise tests and setting the variable here
        // would race every other test in this binary.
        for raw in [None, Some(""), Some("worker"), Some("srever"), Some("SERVERX")] {
            let armed = matches!(
                raw.map(|s: &str| s.trim().to_ascii_lowercase()).as_deref(),
                Some("server")
            );
            assert!(!armed, "{raw:?} must not arm the projection mirror");
        }
        for raw in ["server", "SERVER", " Server "] {
            let armed = matches!(
                Some(raw.trim().to_ascii_lowercase()).as_deref(),
                Some("server")
            );
            assert!(armed, "{raw:?} must arm it");
        }
    }

    #[test]
    fn the_projection_mirror_does_not_share_the_event_logs_variable() {
        // Prod sets the event log's variable TODAY. If this module read it, the
        // next server rollout would arm a tier-2 mirror nobody asked for.
        assert_ne!(
            MIRROR_SOURCE_ENV,
            crate::handlers::ehdb_eventlog_mirror::MIRROR_SOURCE_ENV,
            "the two tiers must cut over independently"
        );
    }
}
