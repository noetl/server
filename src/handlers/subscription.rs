//! `kind: Subscription` lifecycle handlers.
//!
//! Phase 2 of the subscription/listener RFC
//! ([noetl/ai-meta#90](https://github.com/noetl/ai-meta/issues/90)).
//!
//! A `kind: Subscription` catalog entry is a long-lived, activatable resource
//! — never dispatched as a one-shot step DAG.  Its lifecycle is a state
//! machine, **event-sourced** into `noetl.event` so an operator can replay
//! "when did this subscription activate / pause / drain / go down":
//!
//! ```text
//!  register → activate → (pause ⇄ resume) → drain → deactivate
//! ```
//!
//! Each transition writes a `subscription.<transition>` event keyed by the
//! subscription's own snowflake id (the subscription is itself a long-lived
//! "execution" whose event log is its lifecycle; per-message runs are ordinary
//! child executions with `parent_execution_id` = this id).  Current state is
//! the latest such event's status — no separate state table, so it stays
//! replayable and needs no schema migration.
//!
//! These routes are the control surface the **continuous subscription
//! runtime** (worker run-mode, Mode B) drives: it registers + activates on
//! startup, polls its state to honor pause/resume, and drains + deactivates on
//! shutdown.  KEDA reads the active set + source backlog to scale the pool.

use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// `(subscription_id, event_type, status, catalog_id, created_at)` — one
/// subscription's latest lifecycle row, for the cross-shard list query.
type ListRow = (
    i64,
    String,
    String,
    i64,
    Option<chrono::DateTime<chrono::Utc>>,
);

/// `(event_type, status, catalog_id, node_name, created_at)` — the latest
/// lifecycle event for one subscription.
type LatestRow = (
    String,
    String,
    i64,
    Option<String>,
    Option<chrono::DateTime<chrono::Utc>>,
);

// ---------------------------------------------------------------------------
// Lifecycle state machine
// ---------------------------------------------------------------------------

/// Subscription lifecycle state (the latest transition's status).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubState {
    Registered,
    Active,
    Paused,
    Draining,
    Deactivated,
}

impl SubState {
    fn as_status(&self) -> &'static str {
        match self {
            SubState::Registered => "REGISTERED",
            SubState::Active => "ACTIVE",
            SubState::Paused => "PAUSED",
            SubState::Draining => "DRAINING",
            SubState::Deactivated => "DEACTIVATED",
        }
    }

    fn from_status(s: &str) -> Option<SubState> {
        match s {
            "REGISTERED" => Some(SubState::Registered),
            "ACTIVE" => Some(SubState::Active),
            "PAUSED" => Some(SubState::Paused),
            "DRAINING" => Some(SubState::Draining),
            "DEACTIVATED" => Some(SubState::Deactivated),
            _ => None,
        }
    }
}

/// A lifecycle transition: its event type, the resulting state, and the set of
/// states it may be applied from.
struct Transition {
    event_type: &'static str,
    to: SubState,
    from: &'static [SubState],
}

fn transition(name: &str) -> Option<Transition> {
    use SubState::*;
    Some(match name {
        "activate" => Transition {
            event_type: "subscription.activated",
            to: Active,
            // Re-activate from a paused/deactivated subscription is allowed.
            from: &[Registered, Paused, Deactivated, Active],
        },
        "pause" => Transition {
            event_type: "subscription.paused",
            to: Paused,
            from: &[Active, Paused],
        },
        "resume" => Transition {
            event_type: "subscription.resumed",
            to: Active,
            from: &[Paused, Active],
        },
        "drain" => Transition {
            event_type: "subscription.draining",
            to: Draining,
            from: &[Active, Paused, Draining],
        },
        "deactivate" => Transition {
            event_type: "subscription.deactivated",
            to: Deactivated,
            from: &[Registered, Active, Paused, Draining, Deactivated],
        },
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Request / response shapes
// ---------------------------------------------------------------------------

/// `POST /api/subscriptions/register`
#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    /// Catalog path of the `kind: Subscription` entry.
    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct SubscriptionStatus {
    pub subscription_id: String,
    pub path: String,
    pub catalog_id: String,
    pub state: String,
    pub last_event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SubscriptionList {
    pub subscriptions: Vec<SubscriptionStatus>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// `POST /api/subscriptions/register` — mint a subscription id and write the
/// `subscription.registered` lifecycle event.  The path must resolve to a
/// `kind: subscription` catalog entry.
pub async fn register(
    State(state): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> AppResult<Json<SubscriptionStatus>> {
    // Resolve the catalog entry and confirm it is a subscription.
    let row: Option<(i64, String)> = sqlx::query_as(
        "SELECT catalog_id, kind FROM noetl.catalog WHERE path = $1 ORDER BY version DESC LIMIT 1",
    )
    .bind(&req.path)
    .fetch_optional(state.pools.cluster())
    .await?;

    let (catalog_id, kind) = row
        .ok_or_else(|| AppError::NotFound(format!("Subscription not found: {}", req.path)))?;
    if kind.to_lowercase() != "subscription" {
        return Err(AppError::Validation(format!(
            "Catalog entry '{}' is kind '{}', not 'subscription'",
            req.path, kind
        )));
    }

    let subscription_id = state.snowflake.generate()?;
    write_lifecycle_event(
        &state,
        subscription_id,
        catalog_id,
        &req.path,
        "subscription.registered",
        SubState::Registered,
    )
    .await?;

    tracing::info!(
        subscription_id,
        path = %req.path,
        catalog_id,
        "Subscription registered"
    );

    Ok(Json(SubscriptionStatus {
        subscription_id: subscription_id.to_string(),
        path: req.path,
        catalog_id: catalog_id.to_string(),
        state: SubState::Registered.as_status().to_string(),
        last_event_type: "subscription.registered".to_string(),
        updated_at: Some(chrono::Utc::now().to_rfc3339()),
    }))
}

/// `POST /api/subscriptions/{id}/{transition}` — apply a lifecycle transition
/// (activate / pause / resume / drain / deactivate), validated against the
/// current state, and write its event.
pub async fn lifecycle(
    State(state): State<AppState>,
    Path((subscription_id, action)): Path<(i64, String)>,
) -> AppResult<Json<SubscriptionStatus>> {
    let t = transition(&action)
        .ok_or_else(|| AppError::Validation(format!("Unknown subscription action '{}'", action)))?;

    let current = load_latest(&state, subscription_id).await?.ok_or_else(|| {
        AppError::NotFound(format!("Subscription {} not registered", subscription_id))
    })?;

    if !t.from.contains(&current.state) {
        return Err(AppError::Validation(format!(
            "Cannot '{}' a subscription in state '{}' (allowed from {:?})",
            action,
            current.state.as_status(),
            t.from.iter().map(|s| s.as_status()).collect::<Vec<_>>()
        )));
    }

    write_lifecycle_event(
        &state,
        subscription_id,
        current.catalog_id,
        &current.path,
        t.event_type,
        t.to,
    )
    .await?;

    tracing::info!(
        subscription_id,
        path = %current.path,
        action = %action,
        new_state = t.to.as_status(),
        "Subscription lifecycle transition"
    );

    Ok(Json(SubscriptionStatus {
        subscription_id: subscription_id.to_string(),
        path: current.path,
        catalog_id: current.catalog_id.to_string(),
        state: t.to.as_status().to_string(),
        last_event_type: t.event_type.to_string(),
        updated_at: Some(chrono::Utc::now().to_rfc3339()),
    }))
}

/// `GET /api/subscriptions/{id}` — current lifecycle state.
pub async fn get(
    State(state): State<AppState>,
    Path(subscription_id): Path<i64>,
) -> AppResult<Json<SubscriptionStatus>> {
    let current = load_latest(&state, subscription_id).await?.ok_or_else(|| {
        AppError::NotFound(format!("Subscription {} not registered", subscription_id))
    })?;
    Ok(Json(SubscriptionStatus {
        subscription_id: subscription_id.to_string(),
        path: current.path,
        catalog_id: current.catalog_id.to_string(),
        state: current.state.as_status().to_string(),
        last_event_type: current.last_event_type,
        updated_at: current.updated_at,
    }))
}

/// `GET /api/subscriptions` — every registered subscription with its current
/// state.  Fans out across shards (subscriptions are keyed by their snowflake
/// id, sharded like any execution).  The runtime + KEDA read this to find the
/// active set.
pub async fn list(State(state): State<AppState>) -> AppResult<Json<SubscriptionList>> {
    let mut subscriptions: Vec<SubscriptionStatus> = Vec::new();
    for (_idx, pool) in state.pools.all_shards() {
        let rows: Vec<ListRow> = sqlx::query_as(
                r#"
                SELECT DISTINCT ON (execution_id)
                       execution_id, event_type, status, catalog_id, created_at
                FROM noetl.event
                WHERE event_type LIKE 'subscription.%'
                ORDER BY execution_id, event_id DESC
                "#,
            )
            .fetch_all(pool)
            .await?;
        for (sid, event_type, status, catalog_id, created_at) in rows {
            let path = subscription_path(&state, catalog_id).await.unwrap_or_default();
            subscriptions.push(SubscriptionStatus {
                subscription_id: sid.to_string(),
                path,
                catalog_id: catalog_id.to_string(),
                state: SubState::from_status(&status)
                    .map(|s| s.as_status())
                    .unwrap_or("UNKNOWN")
                    .to_string(),
                last_event_type: event_type,
                updated_at: created_at.map(|t| t.to_rfc3339()),
            });
        }
    }
    Ok(Json(SubscriptionList { subscriptions }))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct LatestState {
    state: SubState,
    catalog_id: i64,
    path: String,
    last_event_type: String,
    updated_at: Option<String>,
}

async fn load_latest(state: &AppState, subscription_id: i64) -> AppResult<Option<LatestState>> {
    let row: Option<LatestRow> = sqlx::query_as(
            r#"
            SELECT event_type, status, catalog_id, node_name, created_at
            FROM noetl.event
            WHERE execution_id = $1 AND event_type LIKE 'subscription.%'
            ORDER BY event_id DESC
            LIMIT 1
            "#,
        )
        .bind(subscription_id)
        .fetch_optional(state.pools.pool_for(subscription_id))
        .await?;

    let Some((event_type, status, catalog_id, node_name, created_at)) = row else {
        return Ok(None);
    };
    let sub_state = SubState::from_status(&status).ok_or_else(|| {
        AppError::Internal(format!("Subscription {subscription_id} has unknown status '{status}'"))
    })?;
    let path = match node_name {
        Some(p) if !p.is_empty() => p,
        _ => subscription_path(state, catalog_id).await.unwrap_or_default(),
    };
    Ok(Some(LatestState {
        state: sub_state,
        catalog_id,
        path,
        last_event_type: event_type,
        updated_at: created_at.map(|t| t.to_rfc3339()),
    }))
}

async fn subscription_path(state: &AppState, catalog_id: i64) -> Option<String> {
    sqlx::query_scalar::<_, String>("SELECT path FROM noetl.catalog WHERE catalog_id = $1")
        .bind(catalog_id)
        .fetch_optional(state.pools.cluster())
        .await
        .ok()
        .flatten()
}

async fn write_lifecycle_event(
    state: &AppState,
    subscription_id: i64,
    catalog_id: i64,
    path: &str,
    event_type: &str,
    to: SubState,
) -> AppResult<()> {
    let event_id = state.snowflake.generate()?;
    let context = serde_json::json!({
        "subscription_id": subscription_id.to_string(),
        "path": path,
        "catalog_id": catalog_id.to_string(),
    });
    let meta = serde_json::json!({
        "emitted_at": chrono::Utc::now().to_rfc3339(),
        "emitter": "control_plane",
        "subscription_lifecycle": true,
    });
    sqlx::query(
        r#"
        INSERT INTO noetl.event (
            execution_id, catalog_id, event_id,
            event_type, node_id, node_name, node_type, status,
            context, meta, created_at
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
        "#,
    )
    .bind(subscription_id)
    .bind(catalog_id)
    .bind(event_id)
    .bind(event_type)
    .bind("subscription")
    .bind(path)
    .bind("subscription")
    .bind(to.as_status())
    .bind(&context)
    .bind(&meta)
    .bind(chrono::Utc::now())
    .execute(state.pools.pool_for(subscription_id))
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transition_table_states() {
        assert_eq!(transition("activate").unwrap().to, SubState::Active);
        assert_eq!(transition("pause").unwrap().to, SubState::Paused);
        assert_eq!(transition("resume").unwrap().to, SubState::Active);
        assert_eq!(transition("drain").unwrap().to, SubState::Draining);
        assert_eq!(transition("deactivate").unwrap().to, SubState::Deactivated);
        assert!(transition("bogus").is_none());
    }

    #[test]
    fn pause_only_from_active() {
        let t = transition("pause").unwrap();
        assert!(t.from.contains(&SubState::Active));
        assert!(!t.from.contains(&SubState::Registered));
        assert!(!t.from.contains(&SubState::Deactivated));
    }

    #[test]
    fn resume_only_from_paused() {
        let t = transition("resume").unwrap();
        assert!(t.from.contains(&SubState::Paused));
        assert!(!t.from.contains(&SubState::Registered));
    }

    #[test]
    fn status_roundtrip() {
        for s in [
            SubState::Registered,
            SubState::Active,
            SubState::Paused,
            SubState::Draining,
            SubState::Deactivated,
        ] {
            assert_eq!(SubState::from_status(s.as_status()), Some(s));
        }
        assert_eq!(SubState::from_status("NOPE"), None);
    }
}
