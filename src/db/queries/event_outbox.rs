//! `noetl.event` → `noetl.outbox` transactional-outbox bridge
//! (noetl/ai-meta#103 phase 2a — the CQRS write-path producer).
//!
//! The consumer half of the outbox already exists: the system pool drains it
//! via `/api/internal/outbox/{claim,mark-published,mark-failed,pending-count}`
//! (`system/outbox_publisher` → JetStream) and folds the read model via
//! `/api/internal/events/project` (`system/projector`).  What was missing is the
//! **producer** — something that puts a row in `noetl.outbox` for every
//! `noetl.event` insert, in the *same* transaction so a crash can't leave an
//! event un-mirrored.
//!
//! ## Why a trigger and not app-side enqueue
//!
//! `noetl.event` is inserted from 6+ sites across `handlers::events`,
//! `handlers::execute` (two multi-row `QueryBuilder` batch paths — the
//! high-volume cursor fan-out), `handlers::container_callback`,
//! `handlers::subscription`, and `services::execution` — each routing to a
//! different pool (`pool_for(scope)`, `pool_for(execution_id)`, `cluster()`),
//! several of them autocommitting on `&DbPool` rather than an explicit
//! transaction.  An `AFTER INSERT … FOR EACH ROW` trigger captures all of them
//! — including the batch inserts — atomically, in the inserting connection's
//! transaction, with no per-site refactor.  The schema's own comment on
//! `noetl.outbox` ("writers insert here in the same transaction") is exactly
//! what the trigger guarantees by construction.
//!
//! This is unlike the `trg_event_to_execution` trigger the schema deliberately
//! dropped (`schema_ddl.sql`): that one *maintained a derived table* and could
//! diverge from the event log.  This one is append-only and idempotent
//! (`ON CONFLICT (execution_id, event_id) DO NOTHING`), so it cannot.
//!
//! ## The kill-switch
//!
//! Mirroring to the outbox is a second synchronous write per event — the
//! accepted, *temporary* dual-write cost of a CQRS migration (it disappears at
//! the 2d cutover, when the worker publishes to JetStream and the projector
//! batch-writes `noetl.event` instead).  Until the read side is proven, the
//! default is **off**: the function is always (re)created (cheap, inert), but
//! the trigger exists only when `NOETL_EVENT_OUTBOX_ENABLED` is truthy.  So
//! landing 2a changes nothing in production until ops opts in, and toggling the
//! flag off `DROP`s the trigger — no redeploy needed to stop the dual-write.

use crate::db::DbPool;
use crate::error::AppResult;

/// Env flag gating the `noetl.event` → `noetl.outbox` trigger.  Default off:
/// the producer is inert until ops opts the cluster into the CQRS write path.
pub const OUTBOX_ENABLED_ENV: &str = "NOETL_EVENT_OUTBOX_ENABLED";

/// Read [`OUTBOX_ENABLED_ENV`] as a boolean (`true` / `1`, case-insensitive).
pub fn outbox_enabled_from_env() -> bool {
    std::env::var(OUTBOX_ENABLED_ENV)
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            v == "true" || v == "1" || v == "yes" || v == "on"
        })
        .unwrap_or(false)
}

/// Idempotent startup DDL for the event→outbox producer.  Always (re)creates the
/// fold function; creates the trigger only when `enabled`, and drops it
/// otherwise so the flag is a live kill-switch.  Mirrors the startup-ensure
/// pattern used by [`crate::db::queries::subscription_dedup::ensure_table`] —
/// no out-of-band migration required for first boot.  The canonical
/// `noetl/noetl` `schema_ddl.sql` should mirror this object in a follow-up so a
/// fresh DB seeded straight from the DDL has the function present (inert) too.
pub async fn ensure_event_outbox_trigger(pool: &DbPool, enabled: bool) -> AppResult<()> {
    // The fold: copy the just-inserted event into the outbox as a PENDING,
    // JSON-coded row keyed by (execution_id, event_id).  `subject` carries the
    // event type so the publisher can route it onto the JetStream subject tree
    // (`noetl.events.<type>`) without re-parsing the payload.
    sqlx::query(
        r#"
        CREATE OR REPLACE FUNCTION noetl.event_to_outbox() RETURNS trigger
        LANGUAGE plpgsql AS $fn$
        BEGIN
            INSERT INTO noetl.outbox (
                execution_id, event_id, subject, payload, payload_codec, status
            )
            VALUES (
                NEW.execution_id,
                NEW.event_id,
                'noetl.events.' || COALESCE(NEW.event_type, 'unknown'),
                to_jsonb(NEW),
                'json',
                'PENDING'
            )
            ON CONFLICT (execution_id, event_id) DO NOTHING;
            RETURN NEW;
        END;
        $fn$;
        "#,
    )
    .execute(pool)
    .await?;

    // The trigger is the kill-switch: drop unconditionally, recreate only when
    // enabled.  `DROP … IF EXISTS` + `CREATE` is the idempotent shape Postgres
    // gives us (there is no `CREATE TRIGGER IF NOT EXISTS`); it matches the
    // `DROP TRIGGER IF EXISTS trg_event_to_execution` line in `schema_ddl.sql`.
    sqlx::query("DROP TRIGGER IF EXISTS trg_event_to_outbox ON noetl.event")
        .execute(pool)
        .await?;

    if enabled {
        sqlx::query(
            r#"
            CREATE TRIGGER trg_event_to_outbox
                AFTER INSERT ON noetl.event
                FOR EACH ROW
                EXECUTE FUNCTION noetl.event_to_outbox()
            "#,
        )
        .execute(pool)
        .await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flag parse accepts the common truthy spellings and treats anything
    /// else (including unset) as off — the safe default for the dual-write.
    #[test]
    fn flag_parse_truthy_and_default_off() {
        // SAFETY: single-threaded test; we set and clear the var within it.
        for truthy in ["true", "TRUE", "1", "yes", "On", "  true  "] {
            std::env::set_var(OUTBOX_ENABLED_ENV, truthy);
            assert!(outbox_enabled_from_env(), "{truthy:?} should be enabled");
        }
        for falsy in ["false", "0", "no", "off", ""] {
            std::env::set_var(OUTBOX_ENABLED_ENV, falsy);
            assert!(!outbox_enabled_from_env(), "{falsy:?} should be disabled");
        }
        std::env::remove_var(OUTBOX_ENABLED_ENV);
        assert!(!outbox_enabled_from_env(), "unset should default off");
    }
}
