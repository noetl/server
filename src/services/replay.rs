//! Replay engine — Phase D Round 5 of the Rust server FastAPI parity
//! port (noetl/ai-meta#49 / noetl/server#148).
//!
//! Round 1 ships the endpoint scaffold + minimal `execution`
//! projection fold.  The Python reference is
//! [`noetl/server/api/replay/service.py`](https://github.com/noetl/noetl/blob/main/noetl/server/api/replay/service.py)
//! (~1236 LoC); this Rust port lands it in disciplined rounds
//! (see the noetl/server#148 issue body for the full
//! decomposition).
//!
//! ## Round 1 surface
//!
//! - [`ReplayCutoff`] — exactly one of `as_of_event_id`,
//!   `as_of_position`, `as_of_time` is normally set.
//! - [`ReplayProjection`] — `execution` is the only projection
//!   produced this round; `stage` / `frame` / `command` /
//!   `business_object` / `loop` / `all` are scaffolded as accepted
//!   inputs but fold to the same minimal shape until later
//!   rounds extend the per-projection state.
//! - [`ReplayState`] — the deterministic fold output.  Round 1
//!   only fills `execution_id`, `tenant_id`, `organization_id`,
//!   `projection`, `event_count`, `last_event_id`,
//!   `last_event_type`, and the `execution` sub-object's
//!   `status` + `last_node_name`.
//! - [`ReplayService::replay_state`] — load events for an
//!   execution (applying the cutoff), then fold.
//!
//! ## Out of scope for Round 1
//!
//! - `stages` / `frames` / `commands` / `business_objects` /
//!   `loops` maps (Rounds 2-3).
//! - `replay_snapshot` seed + base_state (Round 5).
//! - `payload_resolver` bounded summaries (Round 6).
//! - `canonical_checksum` / `projection_checksums` (Round 4).
//! - Parity harness against Python (Round 7).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::db::{DbPool, DbPoolMap};
use crate::error::AppResult;

/// Replay cutoff.  Exactly one field is normally set on the wire;
/// the endpoint handler rejects requests with more than one.
///
/// Mirrors Python's `ReplayCutoff` dataclass at
/// `noetl/server/api/replay/types.py`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReplayCutoff {
    /// Replay through this event_id (inclusive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_of_event_id: Option<i64>,

    /// Alias for event-position cutoff (the Python surface accepts
    /// this as a synonym; Round 1 currently treats it as a soft
    /// alias for `as_of_event_id` because the Rust event store keys
    /// on event_id, not a separate position counter).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_of_position: Option<i64>,

    /// Replay through this `event_time` (inclusive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub as_of_time: Option<DateTime<Utc>>,
}

impl ReplayCutoff {
    /// True if no cutoff is set — load every event for the
    /// execution.
    pub fn is_empty(&self) -> bool {
        self.as_of_event_id.is_none()
            && self.as_of_position.is_none()
            && self.as_of_time.is_none()
    }

    /// Count the number of fields set.  The endpoint rejects
    /// requests with more than one set to match Python's
    /// `endpoint.py` behaviour.
    pub fn set_count(&self) -> usize {
        usize::from(self.as_of_event_id.is_some())
            + usize::from(self.as_of_position.is_some())
            + usize::from(self.as_of_time.is_some())
    }
}

/// Which projection(s) to fold the events into.
///
/// Round 1 only produces the `execution` projection — even when
/// `All` is requested, the other map fields are returned empty.
/// Round 2 fleshes out `Stage`/`Frame`/`Command`; Round 3 adds
/// `Loop` + `BusinessObject`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayProjection {
    Execution,
    Stage,
    Frame,
    Command,
    BusinessObject,
    Loop,
    All,
}

impl Default for ReplayProjection {
    fn default() -> Self {
        Self::All
    }
}

impl ReplayProjection {
    /// Wire-format name matching the Python surface
    /// (`projection=execution|frame|loop|business_object|all`).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Execution => "execution",
            Self::Stage => "stage",
            Self::Frame => "frame",
            Self::Command => "command",
            Self::BusinessObject => "business_object",
            Self::Loop => "loop",
            Self::All => "all",
        }
    }

    /// Parse a wire value.  Accepts the canonical Python names +
    /// the underscore alias `business_object`.  Returns `None`
    /// on unknown — the endpoint surfaces this as a 400.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "execution" => Some(Self::Execution),
            "stage" => Some(Self::Stage),
            "frame" => Some(Self::Frame),
            "command" => Some(Self::Command),
            "business_object" => Some(Self::BusinessObject),
            "loop" => Some(Self::Loop),
            "all" => Some(Self::All),
            _ => None,
        }
    }
}

/// Replay state result — the deterministic fold output.
///
/// Round 1 fills the top-level metadata + the `execution`
/// sub-object's `status` + `last_node_name`.  Maps are returned
/// empty but the keys exist so wire-shape consumers can rely on
/// the structure today.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayState {
    pub tenant_id: String,
    pub organization_id: String,
    pub execution_id: i64,
    pub projection: String,

    /// Total events folded into this state.
    pub event_count: u64,

    /// Highest event_id seen (or `None` if no events).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_event_id: Option<i64>,

    /// `event_type` of the highest-event_id event.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_event_type: Option<String>,

    /// Execution-level projection.  Round 1 fills `status` +
    /// `last_node_name`.
    pub execution: ReplayExecutionState,

    /// Stages map.  Empty in Round 1; Round 2 populates.
    #[serde(default)]
    pub stages: serde_json::Map<String, serde_json::Value>,

    /// Frames map.  Empty in Round 1; Round 2 populates.
    #[serde(default)]
    pub frames: serde_json::Map<String, serde_json::Value>,

    /// Commands map.  Empty in Round 1; Round 2 populates.
    #[serde(default)]
    pub commands: serde_json::Map<String, serde_json::Value>,

    /// Business objects map.  Empty in Round 1; Round 3
    /// populates.
    #[serde(default)]
    pub business_objects: serde_json::Map<String, serde_json::Value>,

    /// Loops map.  Empty in Round 1; Round 3 populates.
    #[serde(default)]
    pub loops: serde_json::Map<String, serde_json::Value>,
}

/// Execution-level projection.  Round 1 surfaces `status` +
/// `last_node_name`.  Future rounds may add `payload_refs`,
/// `tenant_id`/`organization_id` echoes, etc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplayExecutionState {
    /// One of `UNKNOWN | RUNNING | COMPLETED | FAILED |
    /// CANCELLED`.  Matches the Python fold's terminal-event
    /// short-circuit + the orchestrator's emit contract (the same
    /// playbook.completed / playbook.failed event types the
    /// status endpoint short-circuits on per server#147).
    pub status: String,

    /// Last `node_name` mentioned on a step-level event.  `None`
    /// when no step events have been folded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_node_name: Option<String>,
}

impl Default for ReplayExecutionState {
    fn default() -> Self {
        Self {
            status: "UNKNOWN".to_string(),
            last_node_name: None,
        }
    }
}

/// Subset of [`crate::db::models::event::Event`] columns the
/// replay fold actually needs.  Round 1 reads `event_id`,
/// `event_type`, `node_name`, `status`, `created_at`.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ReplayEventRow {
    pub event_id: i64,
    pub event_type: String,
    pub node_name: Option<String>,
    pub status: String,
    pub created_at: DateTime<Utc>,
}

/// Replay service.  Phase F R4-4b shape — owns a [`DbPoolMap`] so
/// per-execution queries route via `pools.pool_for(execution_id)`.
#[derive(Clone)]
pub struct ReplayService {
    pools: DbPoolMap,
}

impl ReplayService {
    /// Build a replay service from the shared pool map.
    pub fn new(pools: DbPoolMap) -> Self {
        Self { pools }
    }

    /// Test / example shim wrapping a single legacy pool.
    pub fn new_legacy(db: DbPool) -> Self {
        Self::new(DbPoolMap::from_single_pool(db))
    }

    #[inline]
    fn pool_for(&self, execution_id: i64) -> &DbPool {
        self.pools.pool_for(execution_id)
    }

    /// Replay an execution into a deterministic [`ReplayState`].
    ///
    /// Loads events for `execution_id` from `noetl.event` (applying
    /// the cutoff), folds them deterministically (by ascending
    /// `event_id`), and returns the projected state.
    ///
    /// Round 1 only ships the `execution` projection; other
    /// projections are accepted as inputs but contribute no
    /// additional data this round.
    pub async fn replay_state(
        &self,
        tenant_id: &str,
        organization_id: &str,
        execution_id: i64,
        cutoff: ReplayCutoff,
        projection: ReplayProjection,
        limit: i64,
    ) -> AppResult<ReplayState> {
        let events = self.load_events(execution_id, &cutoff, limit).await?;
        Ok(fold_replay_state(
            &events,
            tenant_id,
            organization_id,
            execution_id,
            projection,
        ))
    }

    /// Load ordered events for an execution, applying the cutoff.
    /// Public so future rounds (and the parity harness in Round 7)
    /// can reuse it.
    pub async fn load_events(
        &self,
        execution_id: i64,
        cutoff: &ReplayCutoff,
        limit: i64,
    ) -> AppResult<Vec<ReplayEventRow>> {
        // SQLx dynamic query construction is awkward; just build
        // the four shapes statically.  Round 4+ may collapse this
        // into a single CASE WHEN once snapshot seeds + payload
        // resolution are in.
        let limit = limit.clamp(1, 100_000);
        let rows = if let Some(event_id) = cutoff.as_of_event_id.or(cutoff.as_of_position) {
            sqlx::query_as::<_, ReplayEventRow>(
                r#"
                SELECT event_id, event_type, node_name, status, created_at
                FROM noetl.event
                WHERE execution_id = $1
                  AND event_id <= $2
                ORDER BY event_id ASC
                LIMIT $3
                "#,
            )
            .bind(execution_id)
            .bind(event_id)
            .bind(limit)
            .fetch_all(self.pool_for(execution_id))
            .await?
        } else if let Some(t) = cutoff.as_of_time {
            sqlx::query_as::<_, ReplayEventRow>(
                r#"
                SELECT event_id, event_type, node_name, status, created_at
                FROM noetl.event
                WHERE execution_id = $1
                  AND created_at <= $2
                ORDER BY event_id ASC
                LIMIT $3
                "#,
            )
            .bind(execution_id)
            .bind(t)
            .bind(limit)
            .fetch_all(self.pool_for(execution_id))
            .await?
        } else {
            sqlx::query_as::<_, ReplayEventRow>(
                r#"
                SELECT event_id, event_type, node_name, status, created_at
                FROM noetl.event
                WHERE execution_id = $1
                ORDER BY event_id ASC
                LIMIT $2
                "#,
            )
            .bind(execution_id)
            .bind(limit)
            .fetch_all(self.pool_for(execution_id))
            .await?
        };
        Ok(rows)
    }
}

/// Pure, deterministic event-fold function — mirrors Python's
/// `fold_replay_state` (Round 1 subset).  Public so future
/// rounds can extend the fold incrementally + unit-test each
/// projection without an active DB.
pub fn fold_replay_state(
    events: &[ReplayEventRow],
    tenant_id: &str,
    organization_id: &str,
    execution_id: i64,
    projection: ReplayProjection,
) -> ReplayState {
    let mut state = ReplayState {
        tenant_id: tenant_id.to_string(),
        organization_id: organization_id.to_string(),
        execution_id,
        projection: projection.as_str().to_string(),
        event_count: 0,
        last_event_id: None,
        last_event_type: None,
        execution: ReplayExecutionState::default(),
        stages: serde_json::Map::new(),
        frames: serde_json::Map::new(),
        commands: serde_json::Map::new(),
        business_objects: serde_json::Map::new(),
        loops: serde_json::Map::new(),
    };

    // Events arrive sorted ASC by event_id from `load_events`; the
    // fold is order-deterministic regardless thanks to the
    // terminal-event short-circuit + last_node_name being a "most
    // recent step.enter wins" projection.  Re-sort defensively in
    // case callers pass an unsorted slice.
    let mut ordered: Vec<&ReplayEventRow> = events.iter().collect();
    ordered.sort_by_key(|e| e.event_id);

    for event in &ordered {
        state.event_count += 1;
        state.last_event_id = Some(event.event_id);
        state.last_event_type = Some(event.event_type.clone());

        match event.event_type.as_str() {
            // Terminal events short-circuit `execution.status`.
            // Mirrors `determine_status` in services::execution
            // (the same terminal-event contract noetl/server#147
            // landed on the status endpoint).
            "playbook.completed" | "playbook_completed" => {
                state.execution.status = "COMPLETED".to_string();
            }
            "playbook.failed" | "playbook_failed" => {
                state.execution.status = "FAILED".to_string();
            }
            "playbook.cancelled" | "playbook_cancelled" => {
                state.execution.status = "CANCELLED".to_string();
            }
            // Step-level events: track the most recent node_name
            // touched.  This is the "current step" view (useful
            // for in-flight executions; for completed ones it's
            // the last step that ran).
            "step.enter" | "step_enter" | "step_started" => {
                if state.execution.status == "UNKNOWN" {
                    state.execution.status = "RUNNING".to_string();
                }
                if let Some(name) = &event.node_name {
                    state.execution.last_node_name = Some(name.clone());
                }
            }
            "step.exit" | "step_completed" | "command.completed" => {
                if let Some(name) = &event.node_name {
                    state.execution.last_node_name = Some(name.clone());
                }
            }
            _ => {
                // Other events still count toward event_count and
                // last_event_id but don't shape `execution.*`.
            }
        }
    }

    state
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(event_id: i64, event_type: &str, node_name: Option<&str>, status: &str) -> ReplayEventRow {
        ReplayEventRow {
            event_id,
            event_type: event_type.to_string(),
            node_name: node_name.map(|s| s.to_string()),
            status: status.to_string(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn fold_empty_event_log_returns_unknown_status() {
        let state = fold_replay_state(&[], "default", "default", 1, ReplayProjection::All);
        assert_eq!(state.event_count, 0);
        assert!(state.last_event_id.is_none());
        assert!(state.last_event_type.is_none());
        assert_eq!(state.execution.status, "UNKNOWN");
        assert!(state.execution.last_node_name.is_none());
        // Maps default to empty + present (wire-shape contract).
        assert!(state.stages.is_empty());
        assert!(state.frames.is_empty());
        assert!(state.commands.is_empty());
    }

    #[test]
    fn fold_step_enter_flips_status_to_running_and_tracks_node_name() {
        let events = vec![
            ev(1, "playbook_started", None, "RUNNING"),
            ev(2, "step.enter", Some("start"), "ENTERED"),
        ];
        let state = fold_replay_state(&events, "default", "default", 42, ReplayProjection::All);
        assert_eq!(state.event_count, 2);
        assert_eq!(state.last_event_id, Some(2));
        assert_eq!(state.last_event_type.as_deref(), Some("step.enter"));
        assert_eq!(state.execution.status, "RUNNING");
        assert_eq!(state.execution.last_node_name.as_deref(), Some("start"));
    }

    #[test]
    fn fold_playbook_completed_short_circuits_status() {
        let events = vec![
            ev(1, "step.enter", Some("start"), "ENTERED"),
            ev(2, "command.completed", Some("start"), "success"),
            ev(3, "playbook.completed", None, "COMPLETED"),
        ];
        let state = fold_replay_state(&events, "default", "default", 42, ReplayProjection::All);
        assert_eq!(state.execution.status, "COMPLETED");
        assert_eq!(state.event_count, 3);
        assert_eq!(state.last_event_id, Some(3));
        // last_node_name tracks the most recent step-level node;
        // `playbook.completed` carries `node_name=None`.
        assert_eq!(state.execution.last_node_name.as_deref(), Some("start"));
    }

    #[test]
    fn fold_playbook_failed_short_circuits_status() {
        let events = vec![
            ev(1, "step.enter", Some("start"), "ENTERED"),
            ev(2, "playbook.failed", None, "FAILED"),
        ];
        let state = fold_replay_state(&events, "default", "default", 42, ReplayProjection::All);
        assert_eq!(state.execution.status, "FAILED");
    }

    #[test]
    fn fold_underscore_aliases_recognised() {
        // Some producers use underscore event-type aliases
        // (Python-era); the fold accepts both shapes.
        let events = vec![
            ev(1, "step_started", Some("alpha"), "ENTERED"),
            ev(2, "playbook_completed", None, "COMPLETED"),
        ];
        let state = fold_replay_state(&events, "default", "default", 42, ReplayProjection::All);
        assert_eq!(state.execution.status, "COMPLETED");
        assert_eq!(state.execution.last_node_name.as_deref(), Some("alpha"));
    }

    #[test]
    fn fold_is_order_deterministic_when_input_unsorted() {
        // Pass events in reverse order — fold should still produce
        // the right terminal status because it re-sorts internally.
        let events = vec![
            ev(3, "playbook.completed", None, "COMPLETED"),
            ev(2, "command.completed", Some("start"), "success"),
            ev(1, "step.enter", Some("start"), "ENTERED"),
        ];
        let state = fold_replay_state(&events, "default", "default", 42, ReplayProjection::All);
        assert_eq!(state.execution.status, "COMPLETED");
        assert_eq!(state.last_event_id, Some(3));
        assert_eq!(state.last_event_type.as_deref(), Some("playbook.completed"));
    }

    #[test]
    fn projection_from_str_accepts_canonical_names() {
        assert_eq!(
            ReplayProjection::from_str("execution"),
            Some(ReplayProjection::Execution)
        );
        assert_eq!(
            ReplayProjection::from_str("business_object"),
            Some(ReplayProjection::BusinessObject)
        );
        assert_eq!(
            ReplayProjection::from_str("loop"),
            Some(ReplayProjection::Loop)
        );
        assert_eq!(
            ReplayProjection::from_str("all"),
            Some(ReplayProjection::All)
        );
        assert!(ReplayProjection::from_str("garbage").is_none());
    }

    #[test]
    fn cutoff_set_count_and_is_empty() {
        let empty = ReplayCutoff::default();
        assert!(empty.is_empty());
        assert_eq!(empty.set_count(), 0);

        let one = ReplayCutoff {
            as_of_event_id: Some(100),
            ..Default::default()
        };
        assert!(!one.is_empty());
        assert_eq!(one.set_count(), 1);

        let three = ReplayCutoff {
            as_of_event_id: Some(100),
            as_of_position: Some(200),
            as_of_time: Some(Utc::now()),
        };
        assert_eq!(three.set_count(), 3);
    }
}
