//! Orchestrator state snapshots (noetl/ai-meta#101, block b).
//!
//! Persists the orchestrator's reconstructed [`WorkflowState`] to
//! `noetl.projection_snapshot` so the per-execution rebuild path is
//! **bounded**: instead of replaying the whole event log (which OOM'd the
//! server at scale — a 10×1000 PFT crashed the server at ~19k events), a
//! rebuild loads the latest snapshot + only the events newer than the
//! snapshot's `version` (the highest `event_id` folded into it).
//!
//! The snapshot is a generic event-sourcing aggregate row keyed by
//! `(tenant_id, organization_id, aggregate_type, aggregate_id)` — we use
//! `aggregate_type = "orchestrator_workflow_state"` and
//! `aggregate_id = execution_id`. `version` is the snapshot watermark
//! (highest applied `event_id`); `meta.applied_count` carries the number of
//! events folded in so the caller can detect stragglers after a rebuild.

use sha2::{Digest, Sha256};
use sqlx::Row;

use crate::db::DbPool;
use crate::engine::state::WorkflowState;
use crate::error::{AppError, AppResult};

const AGGREGATE_TYPE: &str = "orchestrator_workflow_state";

/// A snapshot loaded back from the store.
pub struct LoadedSnapshot {
    pub state: WorkflowState,
    /// Highest `event_id` folded into the snapshot.
    pub version: i64,
    /// Number of events folded in (for straggler detection on rebuild).
    pub applied_count: i64,
    /// Wall-clock time the snapshot was written.  The rebuild re-scans events
    /// with `created_at` newer than this minus a margin, so a straggler that
    /// landed *below* `version` *after* the snapshot was taken is still caught
    /// (re-applying overlap is safe — cursor counters are gated by the
    /// `cursor_issued`/`cursor_completed` id-sets that the snapshot carries).
    pub updated_at: chrono::DateTime<chrono::Utc>,
    /// The `playbook_started` event's meta (pool segment + trace routing),
    /// carried on the snapshot because that event predates every snapshot and
    /// so is never re-loaded in the events-since window.
    pub routing_meta: Option<serde_json::Value>,
    /// `sha256` of the stored snapshot, as the writer computed it.
    ///
    /// `Some` on the incumbent path, `None` on a tier-served read — a served
    /// snapshot has already been checked against its own digest, and carrying
    /// it forward would invite a second comparison against a value that is by
    /// then trivially equal.
    pub checksum: Option<String>,
}

/// Upsert the orchestrator state snapshot for an execution.
///
/// One row per execution (the PK collapses to `aggregate_id` once
/// `tenant_id`/`organization_id`/`aggregate_type` are fixed), so each save
/// overwrites the previous snapshot with a newer watermark.
pub async fn save(
    pool: &DbPool,
    execution_id: i64,
    version: i64,
    applied_count: i64,
    routing_meta: Option<&serde_json::Value>,
    state: &WorkflowState,
) -> AppResult<()> {
    // Taken BEFORE the upsert, so it is at or before the `now()` Postgres
    // stores. The mirror carries this rather than the stored value because the
    // stored one is not returned — and the direction matters: the rebuild's
    // straggler re-scan runs from `updated_at - margin`, so an earlier stamp
    // widens the window (safe) and a later one narrows it (can skip a
    // straggler).
    let mirrored_at = chrono::Utc::now();
    let snapshot = serde_json::to_value(state)
        .map_err(|e| AppError::Internal(format!("orch_snapshot.save: serialise: {e}")))?;
    // Digests `snapshot` — a `serde_json::Value` — and NOT `state` directly.
    // That distinction is load-bearing and was undocumented until ai-meta#265
    // Phase 0: `WorkflowState` holds its maps in `HashMap`, so digesting the
    // struct yields a PER-PROCESS value (measured: four processes, four
    // digests). `Value`'s object map is a `BTreeMap`, so this form is
    // key-sorted at every level and comparable across processes.
    //
    // It has never mattered because nothing re-derives this number — it is
    // computed once and copied, so #265's comparator compares it to itself. The
    // event-sourced read model is the thing that breaks that assumption.
    // `canonical_state_digest` names the property; the guard below pins that
    // this call site keeps it.
    let checksum = {
        let bytes = serde_json::to_vec(&snapshot).unwrap_or_default();
        hex::encode(Sha256::digest(&bytes))
    };
    let meta = serde_json::json!({
        "applied_count": applied_count,
        "routing_meta": routing_meta,
    });

    sqlx::query(
        r#"
        INSERT INTO noetl.projection_snapshot
            (aggregate_id, aggregate_type, version, snapshot, checksum, meta, updated_at)
        VALUES ($1, $2, $3, $4, $5, $6, now())
        ON CONFLICT (tenant_id, organization_id, aggregate_type, aggregate_id)
        DO UPDATE SET
            version = EXCLUDED.version,
            snapshot = EXCLUDED.snapshot,
            checksum = EXCLUDED.checksum,
            meta = EXCLUDED.meta,
            updated_at = now()
        "#,
    )
    .bind(execution_id.to_string())
    .bind(AGGREGATE_TYPE)
    .bind(version)
    .bind(&snapshot)
    .bind(&checksum)
    .bind(&meta)
    .execute(pool)
    .await
    .map_err(|e| AppError::Internal(format!("orch_snapshot.save: upsert: {e}")))?;

    // EHDB projection tier mirror (noetl/ai-meta#265 A3).
    //
    // **Inside the writer, not beside its callers.** This function is the only
    // `INSERT INTO noetl.projection_snapshot` in the service, so a mirror here
    // cannot be bypassed by a caller — which is exactly what happened to the
    // event log, where `emit_events` was documented as the one chokepoint and
    // two in-transaction writers went around it (ai-meta#263).
    // `ehdb_projection_mirror::tests::the_snapshot_store_has_exactly_one_writer`
    // counts INSERT sites so a second writer fails the build rather than
    // silently halving the tier.
    //
    // **After the upsert, and only on success.** A snapshot that failed to
    // become authoritative is never mirrored, so the tier cannot be ahead of the
    // incumbent by way of a write that did not happen.
    //
    // Best-effort: this is auxiliary verification and must never be able to fail
    // a read model the platform has already committed. Default-off behind
    // `NOETL_EHDB_PROJECTION_MIRROR_SOURCE=server`; the call is a cheap env read
    // and an immediate return when unset.
    crate::handlers::ehdb_projection_mirror::mirror_snapshot(
        execution_id,
        version,
        applied_count,
        &checksum,
        &snapshot,
        mirrored_at,
        routing_meta,
    )
    .await;

    Ok(())
}

/// Load the latest orchestrator state snapshot for an execution, if any.
///
/// Returns `None` when no snapshot exists yet (early in a run, before the
/// first save) — the caller then rebuilds from the full (still-small) log.
pub async fn load_latest(pool: &DbPool, execution_id: i64) -> AppResult<Option<LoadedSnapshot>> {
    use crate::handlers::ehdb_projection_read as tier_read;

    let source = tier_read::read_source();

    // The default path, unchanged and untouched by #265 B1: no relay call, no
    // extra query, not even a second env lookup. A tier that is switched off
    // must cost nothing, or "switched off" is not a real rollback.
    if !source.reads_tier() {
        crate::metrics::record_ehdb_projection_read("disabled");
        return load_incumbent(pool, execution_id).await;
    }

    // `verify` loads the incumbent FIRST and compares against it. That ordering
    // is what makes the mode safe by construction rather than by the checks
    // being right: the answer it can fall back to is already in hand before the
    // tier is consulted at all.
    let incumbent = if source.needs_incumbent_first() {
        Some(load_incumbent(pool, execution_id).await?)
    } else {
        None
    };
    let facts = incumbent.as_ref().and_then(|opt| {
        opt.as_ref().map(|s| tier_read::IncumbentFacts {
            version: s.version,
            checksum: s.checksum.clone(),
            updated_at: s.updated_at,
        })
    });

    // In `verify` mode with no incumbent row there is nothing to compare
    // against, and "the tier agrees with nothing" is not evidence. Serve the
    // absence — which is what the caller already handles by rebuilding in full.
    //
    // Its own label, not `missing`: that one means an empty tier beside a
    // populated incumbent, and this means neither store has a snapshot yet —
    // the normal state of every short execution.
    if source.needs_incumbent_first() && facts.is_none() {
        crate::metrics::record_ehdb_projection_read(
            tier_read::DemoteReason::NoIncumbent.as_str(),
        );
        return Ok(incumbent.unwrap_or(None));
    }

    match tier_read::read(pool, execution_id, facts.as_ref()).await {
        tier_read::TierRead::Served(served) => {
            // Deserialisation is the last check, and a failure here demotes for
            // the same reason the incumbent path treats it as absent: a shape
            // change across a deploy must cost a slower rebuild, never an error.
            match serde_json::from_value::<WorkflowState>(served.snapshot) {
                Ok(state) => {
                    crate::metrics::record_ehdb_projection_read("served_tier");
                    Ok(Some(LoadedSnapshot {
                        state,
                        version: served.version,
                        applied_count: served.applied_count,
                        updated_at: served.updated_at,
                        routing_meta: served.routing_meta,
                        checksum: None,
                    }))
                }
                Err(e) => {
                    tracing::warn!(
                        target: "noetl_server::ehdb_projection_read",
                        execution_id,
                        version = served.version,
                        %e,
                        "projection tier snapshot did not deserialise; demoting to the incumbent"
                    );
                    crate::metrics::record_ehdb_projection_read("undeserialisable");
                    demote(pool, execution_id, incumbent).await
                }
            }
        }
        tier_read::TierRead::Demote(reason) => {
            crate::metrics::record_ehdb_projection_read(reason.as_str());
            if reason.is_fault() {
                // Only faults log. `missing` is every execution that predates
                // the mirror arming, and `unconfigured` is every process that
                // has not been given a relay — logging those would bury the
                // lines that mean something under the ones that mean "not yet".
                tracing::warn!(
                    target: "noetl_server::ehdb_projection_read",
                    execution_id,
                    reason = reason.as_str(),
                    mode = source.as_str(),
                    "projection tier read demoted to noetl.projection_snapshot"
                );
            }
            demote(pool, execution_id, incumbent).await
        }
    }
}

/// Fall back to the incumbent, reusing the row already loaded when there is one.
///
/// `verify` mode has it; `tier` mode does not and pays for the query here. That
/// asymmetry is the whole cost model of the two modes and it is deliberate:
/// `tier` is fast when it serves and slower than the baseline when it does not,
/// which is the correct incentive.
async fn demote(
    pool: &DbPool,
    execution_id: i64,
    already_loaded: Option<Option<LoadedSnapshot>>,
) -> AppResult<Option<LoadedSnapshot>> {
    match already_loaded {
        Some(row) => Ok(row),
        None => load_incumbent(pool, execution_id).await,
    }
}

/// The incumbent read: `noetl.projection_snapshot`, exactly as before #265.
async fn load_incumbent(
    pool: &DbPool,
    execution_id: i64,
) -> AppResult<Option<LoadedSnapshot>> {
    let row = sqlx::query(
        r#"
        SELECT version, snapshot, meta, updated_at, checksum
        FROM noetl.projection_snapshot
        WHERE aggregate_type = $1 AND aggregate_id = $2
          AND tenant_id = 'default' AND organization_id = 'default'
        "#,
    )
    .bind(AGGREGATE_TYPE)
    .bind(execution_id.to_string())
    .fetch_optional(pool)
    .await
    .map_err(|e| AppError::Internal(format!("orch_snapshot.load_incumbent: query: {e}")))?;

    let Some(row) = row else {
        return Ok(None);
    };

    let version: i64 = row.try_get("version").unwrap_or(0);
    let updated_at: chrono::DateTime<chrono::Utc> = row
        .try_get("updated_at")
        .unwrap_or_else(|_| chrono::Utc::now());
    let snapshot: serde_json::Value = row
        .try_get("snapshot")
        .map_err(|e| AppError::Internal(format!("orch_snapshot.load_incumbent: snapshot col: {e}")))?;
    let meta: serde_json::Value = row.try_get("meta").unwrap_or(serde_json::Value::Null);
    let checksum: Option<String> = row.try_get("checksum").ok();
    let applied_count = meta
        .get("applied_count")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let routing_meta = meta.get("routing_meta").filter(|v| !v.is_null()).cloned();

    // A snapshot that fails to deserialise (e.g. a WorkflowState shape change
    // across a deploy) is treated as absent — the caller falls back to a full
    // rebuild, which is always correct, just slower.  Better than erroring the
    // whole trigger.
    let state: WorkflowState = match serde_json::from_value(snapshot) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                execution_id,
                version,
                %e,
                "orch_snapshot.load_incumbent: snapshot deserialise failed; ignoring (full rebuild)"
            );
            return Ok(None);
        }
    };

    Ok(Some(LoadedSnapshot {
        state,
        version,
        applied_count,
        updated_at,
        routing_meta,
        checksum,
    }))
}

#[cfg(test)]
mod tests {

    /// The snapshot checksum must be over the CANONICAL form.
    ///
    /// `save` digests `serde_json::to_value(state)` rather than `state`, and
    /// that is what makes the value comparable across processes. The shorter,
    /// more obvious refactor — digest the struct — produces a per-process
    /// digest that passes every existing test, because nothing re-derives it.
    ///
    /// Counting CODE, with `//` stripped, so the prose above cannot satisfy the
    /// guard and deleting the prose cannot break it. Positive control: the real
    /// `to_value` call must survive the stripper.
    #[test]
    fn the_snapshot_checksum_is_over_the_canonical_form() {
        let whole = include_str!("orch_snapshot.rs");
        let code_half = &whole[..whole
            .find("mod tests {")
            .expect("the test module must still be the tail of this file")];
        let code: String = code_half
            .lines()
            .map(|l| match l.find("//") {
                Some(i) => &l[..i],
                None => l,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            code.contains("serde_json::to_value(state)"),
            "the comment stripper ate the real call; this guard proves nothing"
        );
        assert!(
            !code.contains("to_vec(state)") && !code.contains("to_vec(&state)"),
            "the checksum must be over `to_value(state)`, never over the struct: \
             WorkflowState's HashMaps serialise in per-process iteration order, so a \
             struct digest is not comparable between the process that folded and the \
             process that re-folds (ai-meta#265 Phase 0)"
        );
    }
    /// The serving read path must have **exactly one** reader of
    /// `noetl.projection_snapshot`.
    ///
    /// The mirror's writer-side twin
    /// (`ehdb_projection_mirror::the_snapshot_store_has_exactly_one_writer`)
    /// exists so a second `INSERT` cannot silently halve the tier. This is the
    /// read-side statement of the same property, and #265 B1 is what makes it
    /// load-bearing: with a read-serve path in place, a *second* `SELECT` is a
    /// caller that resolves its snapshot from Postgres no matter what
    /// `NOETL_EHDB_PROJECTION_READ_SOURCE` says — a serve path that is inert on
    /// one route and live on another, which is the shape of ai-meta#263.
    ///
    /// The comparator reads the incumbent directly and must: its whole job is
    /// to compare the two stores, so it is excluded by construction — it is not
    /// one of the files scanned.
    #[test]
    fn the_serving_read_path_has_exactly_one_reader() {
        // Comment-stripped, so a doc comment naming the query does not count as
        // a reader — and, in the direction that matters, so the count cannot be
        // *satisfied* by deleting a comment while adding a real one. This file's
        // own prose says `FROM noetl.projection_snapshot` more than once.
        let code_only = |src: &str| -> String {
            src.lines()
                .map(|l| match l.find("//") {
                    Some(i) => &l[..i],
                    None => l,
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        // Scan the CODE half of this file only. The guard's own search literal
        // lives in the test module, so including it counts the guard as a
        // reader — which is exactly how this test failed the first time it ran,
        // and the third sighting of a check counting its own search string
        // (ai-meta#263, and #155's append-path counter).
        let whole = include_str!("orch_snapshot.rs");
        let self_code = &whole[..whole
            .find("mod tests {")
            .expect("the test module must still be the tail of this file")];
        let sources: &[(&str, &str)] = &[
            ("services/orch_snapshot.rs", self_code),
            ("handlers/events.rs", include_str!("../handlers/events.rs")),
            ("handlers/internal.rs", include_str!("../handlers/internal.rs")),
        ];
        let mut readers: Vec<&str> = Vec::new();
        for (name, src) in sources {
            let code = code_only(src);
            for _ in 0..code.matches("FROM noetl.projection_snapshot").count() {
                readers.push(name);
            }
        }
        // Positive control for the stripper: this file's real INSERT must
        // survive it, or every count above is a meaningless zero that would
        // still compare equal to an expectation of zero.
        assert!(
            code_only(self_code).contains("INSERT INTO noetl.projection_snapshot"),
            "the comment stripper ate real SQL; the reader count proves nothing"
        );
        assert_eq!(
            readers,
            vec!["services/orch_snapshot.rs"],
            "the serving read path must have exactly one reader of \
             noetl.projection_snapshot, and it must be `load_incumbent`. Found: {readers:?}. \
             A second reader resolves its snapshot from Postgres regardless of \
             NOETL_EHDB_PROJECTION_READ_SOURCE — a serve path live on one route and \
             inert on another."
        );
    }
}
