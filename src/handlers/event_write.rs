//! CQRS write-path chokepoint (noetl/ai-meta#103 phase 2d-3).
//!
//! Every **server-originated** `noetl.event` write goes through [`emit_event`] /
//! [`emit_events`].  Two modes, selected by
//! [`crate::config::AppConfig::event_ingest_publish_only`]:
//!
//! - **gate OFF (default):** the row is `INSERT`ed synchronously — byte-identical
//!   to the inline INSERTs these call sites used before (the canonical INSERT
//!   binds the full column superset; columns a site didn't set are `None` →
//!   bound `NULL`, which equals the DB default those sites relied on).
//! - **gate ON (`NOETL_EVENT_INGEST_PUBLISH_ONLY`):** the row is **published** to
//!   the `noetl_events` JetStream stream in the same `to_jsonb(noetl.event row)`
//!   shape the 2a tailer publishes (with `created_at`, `Nats-Msg-Id = event_id`),
//!   and **not** inserted.  The `system/event_materializer` playbook drains the
//!   stream and `POST /api/internal/events/project` becomes the **sole**
//!   `noetl.event` writer.  The orchestrator trigger then fires from that write
//!   endpoint (see `handlers::internal::events_project`) rather than the
//!   synchronous ingest, so the drive still advances when writes are async.
//!
//! The two **sink** writers — `handlers::internal::events_materialize` and
//! `services::internal::project_events` — are NOT routed here: they ARE the
//! materializer, the one path that writes when the gate is on.
//!
//! Gate-on requires **a usable event transport** — the EHDB events feed or NATS,
//! per `NOETL_EVENT_BUS`.  With none available the chokepoint falls back to the
//! synchronous INSERT so a misconfiguration degrades to today's behaviour rather
//! than dropping events.  See [`has_event_transport`]: this check used to be
//! NATS-only, which turned the whole publish path inert the moment NATS was
//! removed from the cluster (noetl/ai-meta#212).

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::db::DbPool;
use crate::error::AppResult;
use crate::state::AppState;

/// A full `noetl.event` row to write.  Column superset across every producer
/// site; a field left `None` is bound `NULL` (byte-identical to the inline
/// sites that omitted the column and let it default to `NULL`).  `tenant_id` /
/// `organization_id` are intentionally absent — like the inline sites, the
/// canonical INSERT does not bind them, so their `'default'` DB default fires.
#[derive(Clone, Debug)]
pub struct EventRow {
    pub event_id: i64,
    pub execution_id: i64,
    pub catalog_id: i64,
    pub event_type: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    /// One-level event-chain link (RFC #115 Phase 2, noetl/ai-meta#115 §4): the
    /// immediately-previous event in this execution's causal order.  Normally
    /// left `None` by the producer site and filled in by [`emit_events`] from
    /// the per-execution chain head ([`crate::state::ChainHeads`]) so every
    /// server-emitted row carries a link without each call site threading it.
    /// A producer that already knows the precise predecessor may set it
    /// explicitly; [`emit_events`] then respects it.  `None` after stamping
    /// means this is the execution's root event.
    pub prev_event_id: Option<i64>,
    pub node_id: Option<String>,
    pub node_name: Option<String>,
    pub node_type: Option<String>,
    pub parent_event_id: Option<i64>,
    pub parent_execution_id: Option<i64>,
    pub context: Option<Value>,
    pub result: Option<Value>,
    pub meta: Option<Value>,
    pub error: Option<String>,
    pub worker_id: Option<String>,
}

impl EventRow {
    /// Minimal constructor; chain the `with_*` setters for the optional columns.
    pub fn new(
        event_id: i64,
        execution_id: i64,
        catalog_id: i64,
        event_type: impl Into<String>,
        status: impl Into<String>,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            event_id,
            execution_id,
            catalog_id,
            event_type: event_type.into(),
            status: status.into(),
            created_at,
            prev_event_id: None,
            node_id: None,
            node_name: None,
            node_type: None,
            parent_event_id: None,
            parent_execution_id: None,
            context: None,
            result: None,
            meta: None,
            error: None,
            worker_id: None,
        }
    }

    /// Set `node_id` + `node_name` to the same value (the common case — the
    /// step name takes both columns).
    pub fn with_node(mut self, name: impl Into<String>) -> Self {
        let name = name.into();
        self.node_id = Some(name.clone());
        self.node_name = Some(name);
        self
    }
    /// Set `node_id` and `node_name` separately (e.g. `node_id="playbook"`,
    /// `node_name=<path>` for `playbook_started`).
    pub fn with_nodes(mut self, node_id: impl Into<String>, node_name: impl Into<String>) -> Self {
        self.node_id = Some(node_id.into());
        self.node_name = Some(node_name.into());
        self
    }
    pub fn with_node_type(mut self, t: impl Into<String>) -> Self {
        self.node_type = Some(t.into());
        self
    }
    pub fn with_parent_event_id(mut self, id: i64) -> Self {
        self.parent_event_id = Some(id);
        self
    }
    /// Explicitly set the chain link (RFC #115 §4).  Rarely needed — the
    /// chokepoint fills it from the per-execution head — but available when a
    /// producer knows the exact predecessor.
    pub fn with_prev_event_id(mut self, id: Option<i64>) -> Self {
        self.prev_event_id = id;
        self
    }
    pub fn with_parent_execution_id(mut self, id: Option<i64>) -> Self {
        self.parent_execution_id = id;
        self
    }
    pub fn with_context(mut self, v: Value) -> Self {
        self.context = Some(v);
        self
    }
    pub fn with_result(mut self, v: Value) -> Self {
        self.result = Some(v);
        self
    }
    pub fn with_meta(mut self, v: Value) -> Self {
        self.meta = Some(v);
        self
    }
    pub fn with_error(mut self, e: Option<String>) -> Self {
        self.error = e;
        self
    }
    pub fn with_worker_id(mut self, w: Option<String>) -> Self {
        self.worker_id = w;
        self
    }

    /// The `to_jsonb(noetl.event row)` shape the 2a tailer publishes — the
    /// `system/event_materializer` playbook maps `created_at → timestamp` and
    /// posts it to `/api/internal/events/project`.  Keep the DB column names +
    /// `created_at` (NOT `timestamp`) so the materialized row is byte-identical
    /// to the synchronous INSERT.
    fn to_stream_json(&self) -> Value {
        json!({
            "event_id": self.event_id,
            "execution_id": self.execution_id,
            "catalog_id": self.catalog_id,
            "event_type": self.event_type,
            "status": self.status,
            "created_at": self.created_at,
            "node_id": self.node_id,
            "node_name": self.node_name,
            "node_type": self.node_type,
            "parent_event_id": self.parent_event_id,
            "prev_event_id": self.prev_event_id,
            "parent_execution_id": self.parent_execution_id,
            "context": self.context,
            "result": self.result,
            "meta": self.meta,
            "error": self.error,
            "worker_id": self.worker_id,
        })
    }
}

/// Cache of `catalog_id → is a `system/*` playbook`.  `catalog_id → path` is
/// immutable, so this is populated once per catalog and read lock-free after.
static SYSTEM_CATALOG: std::sync::LazyLock<
    std::sync::RwLock<std::collections::HashMap<i64, bool>>,
> = std::sync::LazyLock::new(|| std::sync::RwLock::new(std::collections::HashMap::new()));

/// Is this execution a **system-pool playbook** (`system/*`)?  System playbooks —
/// the `system/event_materializer` + `system/projector` that DRAIN the stream —
/// must be **exempt** from the publish gate: if their own events published, they
/// could never bootstrap (the drainer would deadlock waiting for itself to
/// drain). So they always write synchronously, even under the gate.
async fn is_system_execution(state: &AppState, catalog_id: i64) -> bool {
    if let Some(v) = SYSTEM_CATALOG
        .read()
        .ok()
        .and_then(|m| m.get(&catalog_id).copied())
    {
        return v;
    }
    let path: Option<String> =
        sqlx::query_scalar("SELECT path FROM noetl.catalog WHERE catalog_id = $1")
            .bind(catalog_id)
            .fetch_optional(state.pools.cluster())
            .await
            .ok()
            .flatten();
    let is_sys = path.as_deref().map(is_system_path).unwrap_or(false);
    if let Ok(mut m) = SYSTEM_CATALOG.write() {
        m.insert(catalog_id, is_sys);
    }
    is_sys
}

/// A catalog path identifies a **system-pool** playbook iff it lives under the
/// `system/` namespace.  Pulled out of [`is_system_execution`] so the predicate
/// — the thing the off-server-drive gate in `events.rs` ultimately turns on
/// (noetl/ai-meta#121) — is unit-testable without a live catalog row.
fn is_system_path(path: &str) -> bool {
    path.starts_with("system/")
}

/// True when this execution's events should be PUBLISHED rather than INSERTed:
/// the gate is on, NATS is connected, AND the execution is not a system-pool
/// playbook (those drain the stream — see [`is_system_execution`]).  This is the
/// single decision the chokepoint and the relocated trigger both consult.
///
/// Every `false` also records **which** condition produced it
/// (`noetl_event_ingest_publish_skipped_total{reason}`).  The publish counter
/// alone cannot express this: it has no series until the first publish, so a
/// server that publishes nothing looks identical whether the gate is off, the
/// transport is missing (noetl/ai-meta#212), or it is simply carrying only
/// system traffic — and only the middle one is a fault.
pub async fn should_publish(state: &AppState, catalog_id: i64) -> bool {
    if !state.config.event_ingest_publish_only {
        crate::metrics::record_event_ingest_publish_skipped("gate_off");
        return false;
    }
    if !has_event_transport(state) {
        crate::metrics::record_event_ingest_publish_skipped("no_transport");
        return false;
    }
    if is_system_execution(state, catalog_id).await {
        crate::metrics::record_event_ingest_publish_skipped("system_execution");
        return false;
    }
    true
}

/// Is *some* event transport available to publish on?
///
/// This used to be `state.nats.is_some()`, which silently became "never" the
/// moment NATS was removed from the cluster: `should_publish` went false for
/// every event, the chokepoint fell through to `insert_rows`, and the server
/// quietly resumed writing `noetl.event` rows synchronously. Nothing errored —
/// executions still completed — but the whole CQRS publish path was inert and
/// the EHDB events feed sat at a flat cursor. Found exactly that way on the
/// prod EHDB-only cutover (noetl/ai-meta#212).
///
/// The honest gate is "does the configured bus have a usable transport",
/// evaluated per mode, so removing NATS disables the NATS path and nothing else.
fn has_event_transport(state: &AppState) -> bool {
    // EHDB is the only transport now. The NATS arm is gone with the rest of the
    // NATS code; `EventBusMode::publishes_nats()` survives only so an operator's
    // stale `NOETL_EVENT_BUS=nats` is a loud "no transport" rather than a silent
    // fall-through to synchronous inserts (noetl/ai-meta#212).
    state.event_bus_mode.publishes_ehdb() && state.ehdb_event_publisher.is_some()
}

/// Is this a terminal execution event?  Both the dotted (`playbook.completed`)
/// and underscore (`playbook_completed`) spellings exist in the codebase — the
/// cancel/finalize chokepoint emits the underscore form, the orchestrator emits
/// the dotted form — so match both.  Mirror of `ExecutionState`'s terminal
/// detection in `orchestrate-core::state` (`from_str` + `apply_event`).  Used to
/// stamp the stateless-drive terminal flag at the [`emit_events`] chokepoint.
fn is_terminal_event_type(event_type: &str) -> bool {
    matches!(
        event_type,
        "playbook.completed"
            | "playbook_completed"
            | "playbook.failed"
            | "playbook_failed"
            | "playbook.cancelled"
            | "playbook_cancelled"
    )
}

/// Lazily build (once) + return the `noetl_events` publisher.  Returns `None`
/// only if NATS is absent or the stream can't be ensured — callers then fall
/// back to the synchronous INSERT.

/// Write one `noetl.event` row through the chokepoint.
///
/// `pool` is the per-execution pool the caller would have inserted into
/// (`state.pools.pool_for(execution_id)`); it is used only on the gate-off
/// INSERT path.
pub async fn emit_event(state: &AppState, pool: &DbPool, row: EventRow) -> AppResult<()> {
    emit_events(state, pool, std::slice::from_ref(&row)).await
}

/// Write a batch of `noetl.event` rows through the chokepoint.  Gate-off does a
/// single multi-row INSERT; gate-on publishes each row (idempotent via
/// `Nats-Msg-Id = event_id`).  An empty batch is a no-op.
pub async fn emit_events(state: &AppState, pool: &DbPool, rows: &[EventRow]) -> AppResult<()> {
    if rows.is_empty() {
        return Ok(());
    }

    // One-level event chain (RFC #115 Phase 2, noetl/ai-meta#115 §4): stamp each
    // row's `prev_event_id` from the per-execution chain head before it is
    // written, so the per-execution events form a walkable singly-linked list
    // (`prev_event_id` → predecessor) without a `noetl.event` scan.  This is the
    // one server-side chokepoint every server-originated event passes through
    // (drive events, command.issued, and worker-lifecycle events via
    // `handle_event`), so stamping here covers the whole chain on both the
    // gate-off INSERT and gate-on publish paths — the materializer then persists
    // the link verbatim.  All rows in a batch share one execution (the batch is
    // built per execution), so a single linkage call covers them in order.
    let rows: Vec<EventRow> = {
        let execution_id = rows[0].execution_id;
        let terminal_in_batch = rows.iter().any(|r| is_terminal_event_type(&r.event_type));
        // Idempotent terminal (noetl/ai-meta#118): enforce exactly one terminal
        // event per execution.  A DUPLICATE finalize (a straggler/duplicate drive
        // under off-server + PUBLISH_ONLY materializer-lag on a single replica)
        // must be suppressed *before* it reaches the chain linker — otherwise it
        // arrives after the first terminal evicted the chain head, links to a
        // `None` head (`prev_event_id = NULL`), and forks the per-execution chain
        // into a second root (the off-server spine walk then can't reach it → a
        // benign `event_scan` fallback).  `mark()` returns true for the FIRST
        // terminal (write it) and false for any later one (drop it).  Multi-replica
        // execution-affinity (noetl/ai-meta#116) already serialises finalize to the
        // owner, so this is the single-replica safety net.  Terminal events are
        // emitted one-at-a-time today, so a suppressed-terminal batch drops to
        // empty (handled below); filtering rather than early-returning keeps a
        // hypothetical mixed batch correct.
        let drop_terminal = terminal_in_batch && !state.finalized_guard.mark(execution_id);
        if drop_terminal {
            crate::metrics::record_terminal_dedup("suppressed");
            tracing::debug!(
                execution_id,
                "emit: suppressed duplicate terminal event (execution already finalized; \
                 noetl/ai-meta#118)"
            );
        } else if terminal_in_batch {
            // Stateless off-server drive edge (RFC #115 Phase 4 remainder): stamp
            // the execute-time descriptor's terminal flag when the FIRST terminal
            // event passes through this chokepoint (cancel via
            // `services::execution::cancel`, finalize, the orchestrator's own
            // `playbook.completed`/`.failed`).  The stateless drive reads this flag
            // to stop re-dispatching a terminal execution WITHOUT rebuilding
            // `WorkflowState` to call `is_terminal()`.
            state.exec_descriptors.mark_terminal(execution_id).await;
        }
        // A suppressed duplicate terminal must not advance the chain head, so drop
        // it before `link_batch` consumes the batch's ids.
        let kept: Vec<&EventRow> = rows
            .iter()
            .filter(|r| !(drop_terminal && is_terminal_event_type(&r.event_type)))
            .collect();
        let ids: Vec<i64> = kept.iter().map(|r| r.event_id).collect();
        let prevs = state.chain_heads.link_batch(execution_id, &ids).await;
        kept.into_iter()
            .zip(prevs)
            .map(|(r, prev)| {
                // Respect an explicit prev a producer already set; otherwise
                // take the chain-head link.
                if r.prev_event_id.is_some() {
                    r.clone()
                } else {
                    let mut r = r.clone();
                    r.prev_event_id = prev;
                    r
                }
            })
            .collect()
    };
    // The whole batch was a suppressed duplicate terminal → nothing to write.
    if rows.is_empty() {
        return Ok(());
    }
    let rows = rows.as_slice();

    // noetl/ai-meta#258 — mirror the COMPLETE authoritative set into the EHDB
    // event-log tier.
    //
    // Placed here, and the position is the whole point. It is after terminal
    // dedup and after chain stamping, so `rows` is exactly the set that becomes
    // authoritative: a suppressed duplicate terminal is not mirrored, and a
    // mirrored record carries the same `prev_event_id` the log will carry.
    //
    // It is also *before* the publish/insert fork, so both branches are covered
    // by one call site. Mirroring inside the branches would have meant two
    // implementations of the same guarantee, and the gate-off branch is the one
    // nobody exercises in prod — the classic place for the second copy to rot.
    //
    // No-op unless `NOETL_EHDB_EVENTLOG_MIRROR_SOURCE=server`; best-effort and
    // isolated, so no failure here can affect the authoritative write below.
    crate::handlers::ehdb_eventlog_mirror::mirror_rows(state, rows).await;

    // All rows in a batch share the same execution + catalog, so one decision
    // covers the batch.
    if should_publish(state, rows[0].catalog_id).await {
        // noetl/ai-meta#212 L1 T3 — which transports are live for this batch.
        //
        // EHDB is the only transport. Resolved here (rather than assumed) so a
        // stale `NOETL_EVENT_BUS=nats` falls through to `insert_rows` loudly
        // instead of publishing into the void.
        let ehdb_live =
            state.event_bus_mode.publishes_ehdb() && state.ehdb_event_publisher.is_some();
        if ehdb_live {
            // noetl/ai-meta#156: when the tail-attach accelerator is on, keep the
            // `to_stream_json()` payloads we publish in the per-execution ring so
            // the off-server drive dispatch can carry the new tail to the worker
            // directly — letting it advance its WAL index without waiting on the
            // global-stream drain.  This is the SAME bytes we publish (the chain
            // link is already stamped above), so the attached tail is byte-identical
            // to what the worker would have drained off `noetl_events`.
            let mut tail_payloads: Vec<serde_json::Value> = Vec::new();
            for row in rows {
                let stream_json = row.to_stream_json();
                let bytes = serde_json::to_vec(&stream_json).map_err(|e| {
                    crate::error::AppError::Internal(format!("event publish encode: {e}"))
                })?;
                // noetl/ai-meta#212 L1 T3 — mirror onto the EHDB events feed.
                // The SAME bytes as NATS gets, so shadow parity is a straight
                // comparison rather than a schema translation.
                if ehdb_live {
                    publish_event_to_ehdb(
                        state,
                        rows[0].execution_id,
                        row.event_id,
                        &row.event_type,
                        &bytes,
                    )
                    .await?;
                }
                crate::metrics::record_event_published(&row.event_type);
                if state.config.offserver_attach_tail {
                    tail_payloads.push(stream_json);
                }
            }
            if state.config.offserver_attach_tail && !tail_payloads.is_empty() {
                state.chain_tails.push(
                    rows[0].execution_id,
                    &tail_payloads,
                    state.config.offserver_tail_cap,
                );
            }
            return Ok(());
        }
        // No transport available → fall through to INSERT.
    }

    insert_rows(pool, rows).await
}

/// The canonical full-column-superset INSERT.  Single multi-row statement.
async fn insert_rows(pool: &DbPool, rows: &[EventRow]) -> AppResult<()> {
    let mut qb = sqlx::QueryBuilder::new(
        "INSERT INTO noetl.event (event_id, execution_id, catalog_id, parent_event_id, \
         prev_event_id, parent_execution_id, event_type, node_id, node_name, node_type, status, \
         context, result, meta, error, worker_id, created_at) ",
    );
    qb.push_values(rows.iter(), |mut b, r| {
        b.push_bind(r.event_id)
            .push_bind(r.execution_id)
            .push_bind(r.catalog_id)
            .push_bind(r.parent_event_id)
            .push_bind(r.prev_event_id)
            .push_bind(r.parent_execution_id)
            .push_bind(&r.event_type)
            .push_bind(&r.node_id)
            .push_bind(&r.node_name)
            .push_bind(&r.node_type)
            .push_bind(&r.status)
            .push_bind(&r.context)
            .push_bind(&r.result)
            .push_bind(&r.meta)
            .push_bind(&r.error)
            .push_bind(&r.worker_id)
            .push_bind(r.created_at);
    });
    qb.build().execute(pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row() -> EventRow {
        EventRow::new(
            42,
            7,
            3,
            "command.completed",
            "success",
            DateTime::parse_from_rfc3339("2026-06-18T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        )
        .with_node("step1")
        .with_result(json!({"status": "success"}))
    }

    #[test]
    fn terminal_event_types_cover_both_spellings() {
        // RFC #115 Phase 4 remainder — the stateless drive's terminal flag is
        // stamped from this predicate.  The cancel/finalize chokepoint emits the
        // underscore form; the orchestrator emits the dotted form.  Both must be
        // recognized, and non-terminal types must not stamp.
        for t in [
            "playbook.completed",
            "playbook_completed",
            "playbook.failed",
            "playbook_failed",
            "playbook.cancelled",
            "playbook_cancelled",
        ] {
            assert!(is_terminal_event_type(t), "{t} must be terminal");
        }
        for t in [
            "command.completed",
            "command.failed",
            "step.enter",
            "playbook_started",
        ] {
            assert!(!is_terminal_event_type(t), "{t} must NOT be terminal");
        }
    }

    #[test]
    fn system_path_detection_gates_the_offserver_drive() {
        // noetl/ai-meta#121 — `should_publish` is false for `system/*` execs, so
        // the off-server WAL drive in events.rs is gated off for them and they
        // fall through to the server-built path.  `is_system_path` is the leaf
        // predicate that whole chain turns on.  System paths drain the stream and
        // INSERT their events (never published to `noetl_events`), so they must
        // be detected; user paths publish and stay on the off-server path.
        for p in [
            "system/scheduled_cleanup",
            "system/event_materializer",
            "system/projector",
        ] {
            assert!(is_system_path(p), "{p} must be detected as a system path");
        }
        for p in [
            "weather/forecast",
            "user/system_report", // `system` not at the path root
            "systems/monitor",    // prefix is `system/`, not `system`
            "",
        ] {
            assert!(
                !is_system_path(p),
                "{p} must NOT be detected as a system path"
            );
        }
    }

    #[test]
    fn default_config_is_synchronous_insert() {
        // The gate is off by default — the chokepoint must take the INSERT path.
        let cfg = crate::config::AppConfig::default();
        assert!(
            !cfg.event_ingest_publish_only,
            "NOETL_EVENT_INGEST_PUBLISH_ONLY must default to false"
        );
    }

    #[test]
    fn stream_json_uses_db_column_names_and_created_at() {
        // The published shape must mirror the tailer's `to_jsonb(row)`:
        // snake_case DB columns + `created_at` (the materializer playbook maps
        // created_at→timestamp), so the materialized row is byte-identical.
        let j = sample_row().to_stream_json();
        assert_eq!(j["event_id"], 42);
        assert_eq!(j["execution_id"], 7);
        assert_eq!(j["catalog_id"], 3);
        assert_eq!(j["event_type"], "command.completed");
        assert_eq!(j["status"], "success");
        assert_eq!(j["node_id"], "step1");
        assert_eq!(j["node_name"], "step1");
        assert_eq!(j["result"]["status"], "success");
        assert!(j.get("created_at").is_some(), "must carry created_at");
        assert!(
            j.get("timestamp").is_none(),
            "must NOT pre-map to timestamp — the materializer playbook does that"
        );
        // Absent optional columns serialize as JSON null (→ NULL on insert).
        assert!(j["node_type"].is_null());
        assert!(j["parent_event_id"].is_null());
        assert!(j["worker_id"].is_null());
        // RFC #115 §4: an unset chain link serializes as null (→ NULL / root).
        assert!(
            j.get("prev_event_id").is_some(),
            "must carry prev_event_id key"
        );
        assert!(j["prev_event_id"].is_null());
    }

    #[test]
    fn stream_json_carries_prev_event_id_when_set() {
        // The chain link must ride the published stream shape so the gate-on
        // materializer persists it verbatim (RFC #115 §4).
        let j = sample_row().with_prev_event_id(Some(41)).to_stream_json();
        assert_eq!(j["prev_event_id"], 41);
    }

    #[test]
    fn builder_sets_node_id_and_name_together() {
        let r = EventRow::new(1, 1, 1, "step.enter", "ENTERED", Utc::now()).with_node("s");
        assert_eq!(r.node_id.as_deref(), Some("s"));
        assert_eq!(r.node_name.as_deref(), Some("s"));
    }
}

/// Mirror one event onto the EHDB events feed (noetl/ai-meta#212 L1 T3).
///
/// **Failure semantics differ by mode, deliberately.**
///
/// In `shadow`, NATS is authoritative and this publish is an observation. A
/// failure is logged and counted but must **not** fail the caller's request —
/// shadow exists to de-risk the cutover, and a shadow path that can take down
/// event ingest is a bigger risk than the one it is measuring.
///
/// In `ehdb`, this is the only path the durable event log has. A failure is
/// returned so the caller sees a 500 and retries, rather than the event being
/// dropped silently. Fail-closed is the right posture once nothing is behind it.
async fn publish_event_to_ehdb(
    state: &crate::state::AppState,
    execution_id: i64,
    event_id: i64,
    event_type: &str,
    bytes: &[u8],
) -> Result<(), crate::error::AppError> {
    let Some(publisher) = state.ehdb_event_publisher.as_ref() else {
        return Ok(());
    };
    match publisher.publish_event(execution_id, event_id, bytes).await {
        Ok(_) => {
            crate::metrics::record_ehdb_event_published(event_type);
            Ok(())
        }
        Err(e) if state.event_bus_mode == crate::event_bus::EventBusMode::Shadow => {
            crate::metrics::record_ehdb_event_publish_error(event_type);
            tracing::warn!(
                execution_id,
                event_id,
                event_type,
                error = %e,
                "EHDB shadow event publish failed; NATS remains authoritative"
            );
            Ok(())
        }
        Err(e) => {
            crate::metrics::record_ehdb_event_publish_error(event_type);
            Err(crate::error::AppError::Internal(format!(
                "EHDB event publish: {e}"
            )))
        }
    }
}
