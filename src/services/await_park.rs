//! Park an awaiting `kind: playbook` step and free its worker slot
//! (noetl/ai-meta#559, Stage 2b of the latency-reduction plan, RFC #556).
//!
//! # Why
//!
//! A `return_result` step holds its worker slot for the child's whole lifetime:
//! the tool sits in a poll loop on the slot. On a 4-slot user pool four such
//! parents whose children need the same pool deadlock (the resource model's
//! same-pool `deadlock_note`), and a parent waiting on a slow child is a slot
//! nobody else can use.
//!
//! # What
//!
//! With `NOETL_PLAYBOOK_AWAIT_PARK` on (server) and the tool's matching flag on
//! (worker), the parent's tool asks `/api/execute` to PARK the await:
//!
//! 1. the server records `noetl.execution_await_park` (child → the parent's
//!    execution, step and command) and answers `parked: true`;
//! 2. the tool returns `pending_callback`, so the worker skips `call.done`,
//!    emits a `command.completed` marked `pending_callback`, and frees the slot —
//!    the existing `kind: container` callback path (noetl/ai-meta#186): the
//!    orchestrator holds a parked step without advancing past it;
//! 3. when the child is terminal AND the parent's parked completion has been
//!    seen, the server picks the child's result with the tool's own selection
//!    (`select_result_event` + `extract_child_payload`, ported verbatim), emits
//!    the parent step's `call.done` in the exact envelope the worker would have
//!    produced, and triggers the drive (`call.done` on a callback-parked step is
//!    a resume trigger in orchestrate-core).
//!
//! Both orders converge: a child that finishes before the parent's parked
//! completion lands is held until it does (resuming first would let the parked
//! completion re-park a step that already has its result).
//!
//! # Durability
//!
//! The row is the record. A sweep every [`SWEEP_EVERY`] resolves any row whose
//! child is terminal and whose parked completion is in `noetl.event` (a missed
//! wake, a restart), and fails any row past its deadline, so a parked parent can
//! never wait forever.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use crate::db::DbPool;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// How often the durable sweep runs.
pub const SWEEP_EVERY: Duration = Duration::from_secs(5);

/// What the parent's tool asks for in `/api/execute` (`await_park`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AwaitPark {
    pub parent_execution_id: i64,
    pub parent_step: String,
    pub parent_command_id: String,
    #[serde(default)]
    pub result_step: Option<String>,
    /// Seconds before a parked await fails (the tool's `timeout`; default 180,
    /// the `return_result` default).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// `CREATE TABLE IF NOT EXISTS`, run once at startup when the flag is on.
pub async fn ensure_schema(pool: &DbPool) {
    let ddl = r#"
        CREATE TABLE IF NOT EXISTS noetl.execution_await_park (
            child_execution_id   BIGINT PRIMARY KEY,
            parent_execution_id  BIGINT NOT NULL,
            parent_step          TEXT   NOT NULL,
            parent_command_id    TEXT   NOT NULL,
            result_step          TEXT,
            deadline_at          TIMESTAMPTZ NOT NULL,
            created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
            resolved_at          TIMESTAMPTZ,
            outcome              TEXT
        )"#;
    if let Err(e) = sqlx::query(ddl).execute(pool).await {
        warn!(error = %e, "await-park: ensure_schema failed; parking will refuse");
    }
    let idx = "CREATE INDEX IF NOT EXISTS idx_execution_await_park_open \
               ON noetl.execution_await_park (created_at) WHERE resolved_at IS NULL";
    let _ = sqlx::query(idx).execute(pool).await;
}

#[derive(Default)]
struct Mem {
    /// child → parent (open parks this process knows of).
    open: HashMap<i64, AwaitPark>,
    /// Children seen terminal whose parent's parked completion has not been.
    ready: HashSet<i64>,
    /// Parent command ids whose parked `command.completed` was ingested.
    parked_seen: HashSet<String>,
}

fn mem() -> &'static Mutex<Mem> {
    static M: OnceLock<Mutex<Mem>> = OnceLock::new();
    M.get_or_init(Default::default)
}

fn tx() -> &'static OnceLock<tokio::sync::mpsc::UnboundedSender<i64>> {
    static T: OnceLock<tokio::sync::mpsc::UnboundedSender<i64>> = OnceLock::new();
    &T
}

/// Is the parent step's tool a single `kind: playbook`? A playbook tool inside
/// a tool list runs within `task_sequence`, which does not park; refuse there.
fn step_is_single_playbook_tool(playbook: &crate::playbook::types::Playbook, step: &str) -> bool {
    let Ok(v) = serde_json::to_value(playbook) else {
        return false;
    };
    let steps = v.get("workflow").and_then(|w| w.as_array());
    let Some(def) = steps.and_then(|s| {
        s.iter()
            .find(|d| d.get("step").and_then(|x| x.as_str()) == Some(step))
    }) else {
        return false;
    };
    match def.get("tool") {
        Some(Value::Object(t)) => t.get("kind").and_then(|k| k.as_str()) == Some("playbook"),
        _ => false,
    }
}

/// Register a park. Returns `Ok(true)` when the parent may park (the tool then
/// frees its slot), `Ok(false)` when it must wait as today.
/// The entry step is dispatched at `/api/execute` without a `step.enter`. On a
/// server-side state builder (kind A, 2026-11-14) a parked `command.completed`
/// on it read as "never started" and the orchestrator RE-ISSUED the step 63
/// times, each issue spawning another child. (The off-server builder prod runs
/// resumed it correctly.) Never park the entry step; it waits on its slot as
/// today.
pub const NEVER_PARK_STEP: &str = "start";

pub async fn register(
    state: &AppState,
    child_execution_id: i64,
    park: AwaitPark,
) -> AppResult<bool> {
    if park.parent_step == NEVER_PARK_STEP {
        crate::metrics::record_await_park("refused_entry_step");
        return Ok(false);
    }
    // The parent's step must be a single `kind: playbook` tool.
    let parent_catalog = state
        .exec_descriptors
        .get(park.parent_execution_id)
        .await
        .map(|d| d.catalog_id)
        .filter(|c| *c != 0);
    let Some(catalog_id) = parent_catalog else {
        crate::metrics::record_await_park("refused_no_parent");
        return Ok(false);
    };
    let yaml: Option<String> =
        sqlx::query_scalar("SELECT content FROM noetl.catalog WHERE catalog_id = $1")
            .bind(catalog_id)
            .fetch_optional(state.pools.cluster())
            .await?;
    let ok_shape = yaml
        .and_then(|y| crate::playbook::parser::parse_playbook(&y).ok())
        .is_some_and(|pb| step_is_single_playbook_tool(&pb, &park.parent_step));
    if !ok_shape {
        crate::metrics::record_await_park("refused_step_shape");
        return Ok(false);
    }
    let timeout = park.timeout_secs.unwrap_or(180).clamp(1, 86_400) as f64;
    let pool = state.pools.pool_for(park.parent_execution_id);
    sqlx::query(
        "INSERT INTO noetl.execution_await_park \
         (child_execution_id, parent_execution_id, parent_step, parent_command_id, result_step, deadline_at) \
         VALUES ($1, $2, $3, $4, $5, now() + make_interval(secs => $6)) \
         ON CONFLICT (child_execution_id) DO NOTHING",
    )
    .bind(child_execution_id)
    .bind(park.parent_execution_id)
    .bind(&park.parent_step)
    .bind(&park.parent_command_id)
    .bind(&park.result_step)
    .bind(timeout)
    .execute(pool)
    .await?;
    mem().lock().unwrap().open.insert(child_execution_id, park);
    crate::metrics::record_await_park("parked");
    // The child may already be done (it ran while we registered).
    if let Some(t) = tx().get() {
        let _ = t.send(child_execution_id);
    }
    Ok(true)
}

/// A terminal row for `execution_id` was written (from `execution_wait`).
pub fn child_terminal(execution_id: i64) {
    let open = mem().lock().unwrap().open.contains_key(&execution_id);
    if open {
        if let Some(t) = tx().get() {
            let _ = t.send(execution_id);
        }
    }
}

/// The parent's parked `command.completed` was ingested (from `events`).
pub fn parked_completion_seen(command_id: &str) {
    let child = {
        let mut m = mem().lock().unwrap();
        m.parked_seen.insert(command_id.to_string());
        let found = m
            .open
            .iter()
            .find(|(_, p)| p.parent_command_id == command_id)
            .map(|(c, _)| *c);
        found.filter(|c| m.ready.contains(c))
    };
    if let (Some(c), Some(t)) = (child, tx().get()) {
        let _ = t.send(c);
    }
}

/// Start the resolver (wake-driven) and the sweep. Call once, flag on.
pub fn spawn(state: AppState) {
    let (sender, mut rx) = tokio::sync::mpsc::unbounded_channel::<i64>();
    if tx().set(sender).is_err() {
        return;
    }
    let s1 = state.clone();
    tokio::spawn(async move {
        while let Some(child) = rx.recv().await {
            if let Err(e) = try_resolve(&s1, child, false).await {
                warn!(child, %e, "await-park: resolve failed; the sweep retries");
            }
        }
    });
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SWEEP_EVERY).await;
            if let Err(e) = sweep(&state).await {
                warn!(%e, "await-park: sweep failed");
            }
        }
    });
}

async fn sweep(state: &AppState) -> AppResult<()> {
    let rows: Vec<(i64, i64, String, String, Option<String>, bool)> = sqlx::query_as(
        "SELECT child_execution_id, parent_execution_id, parent_step, parent_command_id, result_step, \
                (deadline_at < now()) AS expired \
         FROM noetl.execution_await_park WHERE resolved_at IS NULL ORDER BY created_at LIMIT 200",
    )
    .fetch_all(state.pools.cluster())
    .await?;
    for (child, parent, step, cmd, result_step, expired) in rows {
        mem()
            .lock()
            .unwrap()
            .open
            .entry(child)
            .or_insert(AwaitPark {
                parent_execution_id: parent,
                parent_step: step,
                parent_command_id: cmd,
                result_step,
                timeout_secs: None,
            });
        if let Err(e) = try_resolve(state, child, expired).await {
            debug!(child, %e, "await-park: sweep resolve deferred");
        }
    }
    Ok(())
}

/// Resolve one park if its child is terminal and the parent's parked completion
/// has landed (or `expired`: fail it).
async fn try_resolve(state: &AppState, child: i64, expired: bool) -> AppResult<()> {
    let Some(park) = mem().lock().unwrap().open.get(&child).cloned() else {
        return Ok(());
    };
    let service = crate::services::ExecutionService::new(
        state.pools.clone(),
        state.snowflake.clone(),
        state.clone(),
    );
    let status = match service.get_status(child).await {
        Ok(s) => Some(s),
        Err(AppError::NotFound(_)) => None,
        Err(e) => return Err(e),
    };
    let terminal = status
        .as_ref()
        .map(|s| {
            matches!(s.status.as_str(), "COMPLETED" | "FAILED" | "CANCELLED") || s.is_cancelled
        })
        .unwrap_or(false);
    if !terminal && !expired {
        return Ok(());
    }
    // The parent's parked completion must be in the log first.
    let seen = mem()
        .lock()
        .unwrap()
        .parked_seen
        .contains(&park.parent_command_id)
        || parked_completion_in_log(state, &park).await?;
    if !seen {
        if terminal {
            mem().lock().unwrap().ready.insert(child);
            crate::metrics::record_await_park("held_for_parent");
        }
        return Ok(());
    }
    // Claim the row (exactly one resolver wins).
    let outcome_label = if expired && !terminal {
        "timeout".to_string()
    } else {
        status
            .as_ref()
            .map(|s| s.status.to_ascii_lowercase())
            .unwrap_or_default()
    };
    let claimed: Option<(i64,)> = sqlx::query_as(
        "UPDATE noetl.execution_await_park SET resolved_at = now(), outcome = $2 \
         WHERE child_execution_id = $1 AND resolved_at IS NULL RETURNING child_execution_id",
    )
    .bind(child)
    .bind(&outcome_label)
    .fetch_optional(state.pools.cluster())
    .await?;
    {
        let mut m = mem().lock().unwrap();
        m.open.remove(&child);
        m.ready.remove(&child);
        m.parked_seen.remove(&park.parent_command_id);
    }
    if claimed.is_none() {
        return Ok(());
    }

    // The parent's call.done, in the envelope the worker would have built.
    let (status_label, inner) = if outcome_label == "completed" {
        let detail = serde_json::to_value(service.get(child).await?).unwrap_or_default();
        match select_result_event(&detail, park.result_step.as_deref()) {
            Some(ev) => {
                let payload = extract_child_payload(ev.get("result").unwrap_or(&Value::Null));
                (
                    "COMPLETED",
                    json!({"status": "success", "context": {
                        "child_execution_id": child.to_string(),
                        "data": payload,
                        "status": "success"
                    }}),
                )
            }
            None => (
                "FAILED",
                json!({"status": "error", "context": {
                    "child_execution_id": child.to_string(),
                    "error": format!("child playbook {child} completed but no result event found (result_step={:?})", park.result_step),
                    "status": "error"
                }}),
            ),
        }
    } else {
        let reason = if outcome_label == "timeout" {
            format!("child playbook {child} did not finish before the await timeout")
        } else {
            format!(
                "child execution {child} {}",
                outcome_label.to_ascii_uppercase()
            )
        };
        (
            "FAILED",
            json!({"status": "error", "context": {
                "child_execution_id": child.to_string(),
                "error": reason,
                "status": "error"
            }}),
        )
    };
    let parent = park.parent_execution_id;
    let catalog_id = state
        .exec_descriptors
        .get(parent)
        .await
        .map(|d| d.catalog_id)
        .unwrap_or(0);
    let pool = state.pools.pool_for(parent);
    let mk_id = || {
        state
            .snowflake
            .generate()
            .map_err(|e| AppError::Internal(format!("await-park: snowflake: {e}")))
    };
    let trigger_event_id = if status_label == "COMPLETED" {
        // The `call.done` the worker would have emitted for a successful await.
        let event_id = mk_id()?;
        let result_obj = json!({
            "status": status_label,
            "context": {"call_index": 0, "command_id": park.parent_command_id, "result": inner},
        });
        let ev = crate::handlers::event_write::EventRow::new(
            event_id, parent, catalog_id, "call.done", status_label, chrono::Utc::now(),
        )
        .with_node(&park.parent_step)
        .with_result(result_obj)
        .with_meta(json!({"node_type": "playbook", "resumed_by": "await_park"}));
        crate::handlers::event_write::emit_event(state, pool, ev).await?;
        event_id
    } else {
        // A failed await is what the worker emits when the tool errors:
        // `call.error` then `command.failed`, which fails the step (a FAILED
        // `call.done` would let the DAG advance past it — found on kind).
        let error = inner["context"]["error"].as_str().unwrap_or("child failed").to_string();
        let err_id = mk_id()?;
        let ev = crate::handlers::event_write::EventRow::new(
            err_id, parent, catalog_id, "call.error", "FAILED", chrono::Utc::now(),
        )
        .with_node(&park.parent_step)
        .with_result(json!({"status": "FAILED", "context": {
            "command_id": park.parent_command_id, "call_index": 0, "error": error,
            "child_execution_id": child.to_string()}}))
        .with_meta(json!({"node_type": "playbook", "resumed_by": "await_park"}));
        crate::handlers::event_write::emit_event(state, pool, ev).await?;
        let failed_id = mk_id()?;
        let ev = crate::handlers::event_write::EventRow::new(
            failed_id, parent, catalog_id, "command.failed", "FAILED", chrono::Utc::now(),
        )
        .with_node(&park.parent_step)
        .with_result(json!({"status": "FAILED", "context": {
            "command_id": park.parent_command_id, "error": error}}))
        .with_meta(json!({"node_type": "playbook", "resumed_by": "await_park"}));
        crate::handlers::event_write::emit_event(state, pool, ev).await?;
        failed_id
    };
    crate::metrics::record_await_park(&format!("resumed_{outcome_label}"));
    info!(parent, child, step = %park.parent_step, outcome = %outcome_label, "await-park: resumed parent");
    if let Err(e) =
        crate::handlers::events::trigger_orchestrator_inner(state, parent, trigger_event_id, false).await
    {
        warn!(parent, %e, "await-park: drive after resume failed; reconcile re-drives");
    }
    Ok(())
}

async fn parked_completion_in_log(state: &AppState, park: &AwaitPark) -> AppResult<bool> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT event_id FROM noetl.event WHERE execution_id = $1 AND event_type = 'command.completed' \
         AND node_name = $2 AND result->'context'->>'command_id' = $3 LIMIT 1",
    )
    .bind(park.parent_execution_id)
    .bind(&park.parent_step)
    .bind(&park.parent_command_id)
    .fetch_optional(state.pools.pool_for(park.parent_execution_id))
    .await?;
    Ok(row.is_some())
}

/// Ported verbatim from noetl-tools `PlaybookTool::select_result_event`, so a
/// parked await returns exactly what a waiting one would.
pub fn select_result_event<'a>(detail: &'a Value, result_step: Option<&str>) -> Option<&'a Value> {
    let events = detail.get("events").and_then(|v| v.as_array())?;
    let is_call_done =
        |ev: &Value| ev.get("event_type").and_then(|v| v.as_str()) == Some("call.done");
    let has_payload = |ev: &Value| {
        ev.get("result")
            .map(|r| !r.is_null() && extract_child_payload(r) != Value::Null)
            .unwrap_or(false)
    };
    if let Some(step) = result_step {
        if let Some(ev) = events.iter().rev().find(|ev| {
            ev.get("node_name").and_then(|v| v.as_str()) == Some(step) && is_call_done(ev)
        }) {
            return Some(ev);
        }
        if let Some(ev) = events.iter().rev().find(|ev| {
            ev.get("node_name").and_then(|v| v.as_str()) == Some(step) && has_payload(ev)
        }) {
            return Some(ev);
        }
    }
    events.iter().rev().find(|ev| {
        is_call_done(ev)
            && ev.get("node_name").and_then(|v| v.as_str()) != Some("playbook")
            && has_payload(ev)
    })
}

/// Ported verbatim from noetl-tools `PlaybookTool::extract_child_payload`.
pub fn extract_child_payload(event_result: &Value) -> Value {
    let mut current = event_result;
    for _ in 0..5 {
        let Value::Object(map) = current else {
            break;
        };
        if let Some(inner) = map.get("context").filter(|v| v.is_object()) {
            current = inner;
            continue;
        }
        if let Some(inner) = map.get("result").filter(|v| v.is_object()) {
            current = inner;
            continue;
        }
        break;
    }
    if let Value::Object(map) = current {
        if let Some(data) = map.get("data") {
            return data.clone();
        }
    }
    current.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_the_childs_data_through_the_call_done_envelope() {
        // The shape a child's `end` call.done has in `noetl.event` (kind, 2026-11-14).
        let r = json!({"status": "COMPLETED", "context": {"call_index": 0, "command_id": "c",
            "result": {"status": "success", "context": {"data": {"answer": 42}, "status": "success"}}}});
        assert_eq!(extract_child_payload(&r), json!({"answer": 42}));
    }

    #[test]
    fn the_entry_step_is_never_parked() {
        // register() refuses it before any I/O; pin the name so a rename of
        // the entry step cannot silently re-open the re-issue loop.
        assert_eq!(NEVER_PARK_STEP, "start");
    }

    #[test]
    fn only_a_single_playbook_tool_step_may_park() {
        let yaml = r#"
apiVersion: noetl.io/v2
kind: Playbook
metadata: { name: t, path: t }
workflow:
  - step: start
    tool: { kind: noop }
    next: [ { step: h1 } ]
  - step: h1
    tool: { kind: playbook, path: bench/child, return_result: true }
    next: [ { step: listed } ]
  - step: listed
    tool:
      - a: { kind: playbook, path: bench/child, return_result: true }
      - b: { kind: noop }
"#;
        let pb = crate::playbook::parser::parse_playbook(yaml).expect("parse");
        assert!(step_is_single_playbook_tool(&pb, "h1"));
        assert!(
            !step_is_single_playbook_tool(&pb, "listed"),
            "a tool list runs in task_sequence"
        );
        assert!(!step_is_single_playbook_tool(&pb, "start"));
        assert!(!step_is_single_playbook_tool(&pb, "missing"));
    }

    #[test]
    fn selects_the_named_step_then_the_latest_payload() {
        let detail = json!({"events": [
            {"event_type": "call.done", "node_name": "start", "result": {"context": {"data": {"a": 1}}}},
            {"event_type": "call.done", "node_name": "end", "result": {"context": {"data": {"b": 2}}}}
        ]});
        let named = select_result_event(&detail, Some("start")).unwrap();
        assert_eq!(named["node_name"], "start");
        let latest = select_result_event(&detail, None).unwrap();
        assert_eq!(latest["node_name"], "end");
    }
}
