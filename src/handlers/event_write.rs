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
//! Gate-on requires NATS.  If NATS is not connected the chokepoint falls back to
//! the synchronous INSERT (logged once) so a misconfiguration degrades to
//! today's behaviour rather than dropping events.

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
    if let Some(v) = SYSTEM_CATALOG.read().ok().and_then(|m| m.get(&catalog_id).copied()) {
        return v;
    }
    let path: Option<String> =
        sqlx::query_scalar("SELECT path FROM noetl.catalog WHERE catalog_id = $1")
            .bind(catalog_id)
            .fetch_optional(state.pools.cluster())
            .await
            .ok()
            .flatten();
    let is_sys = path.as_deref().map(|p| p.starts_with("system/")).unwrap_or(false);
    if let Ok(mut m) = SYSTEM_CATALOG.write() {
        m.insert(catalog_id, is_sys);
    }
    is_sys
}

/// True when this execution's events should be PUBLISHED rather than INSERTed:
/// the gate is on, NATS is connected, AND the execution is not a system-pool
/// playbook (those drain the stream — see [`is_system_execution`]).  This is the
/// single decision the chokepoint and the relocated trigger both consult.
pub async fn should_publish(state: &AppState, catalog_id: i64) -> bool {
    state.config.event_ingest_publish_only
        && state.nats.is_some()
        && !is_system_execution(state, catalog_id).await
}

/// Lazily build (once) + return the `noetl_events` publisher.  Returns `None`
/// only if NATS is absent or the stream can't be ensured — callers then fall
/// back to the synchronous INSERT.
async fn publisher(state: &AppState) -> Option<&crate::nats::EventStreamPublisher> {
    let client = state.nats.clone()?;
    state
        .event_stream_publisher
        .get_or_try_init(|| async move {
            let cfg = crate::services::event_stream::EventStreamConfig::from_env();
            crate::nats::EventStreamPublisher::new(client, cfg.dedup_window, cfg.max_age).await
        })
        .await
        .map_err(|e| {
            tracing::error!(%e, "publish-only: failed to build noetl_events publisher; falling back to synchronous INSERT");
            e
        })
        .ok()
}

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
        let ids: Vec<i64> = rows.iter().map(|r| r.event_id).collect();
        let prevs = state.chain_heads.link_batch(execution_id, &ids);
        rows.iter()
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
    let rows = rows.as_slice();

    // All rows in a batch share the same execution + catalog, so one decision
    // covers the batch.
    if should_publish(state, rows[0].catalog_id).await {
        if let Some(pubr) = publisher(state).await {
            for row in rows {
                let bytes = serde_json::to_vec(&row.to_stream_json()).map_err(|e| {
                    crate::error::AppError::Internal(format!("event publish encode: {e}"))
                })?;
                pubr.publish_event(row.event_id, &row.event_type, &bytes)
                    .await
                    .map_err(|e| {
                        crate::error::AppError::Internal(format!("event publish: {e}"))
                    })?;
                crate::metrics::record_event_published(&row.event_type);
            }
            return Ok(());
        }
        // NATS unavailable / stream unbuildable → fall through to INSERT.
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
        assert!(j.get("prev_event_id").is_some(), "must carry prev_event_id key");
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
