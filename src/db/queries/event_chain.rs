//! `prev_event_id` chain columns — the one-level event chain (RFC #115 Phase 2,
//! noetl/ai-meta#115 §4).
//!
//! Each `noetl.event` and `noetl.command` gains an additive `prev_event_id`
//! pointer naming the immediately-previous node in the execution's causal
//! order, so per-execution state can be followed **pointer-by-pointer, one
//! level at a time** instead of scanning the whole event table (the chain-walk
//! state builder lands in Phase 3 — this phase only *populates* the links;
//! nothing reads them yet).
//!
//! The columns are owned by the platform schema
//! (`db/ddl/postgres/schema_ddl.sql` in this repo defines them for fresh
//! installs; ops still provisions from noetl/noetl's copy until it repoints), but the
//! server **also** ensures them idempotently at startup — mirroring
//! [`crate::db::queries::result_store::ensure_table`] — so a server image
//! carrying the populate-on-emit code never writes a column the running
//! database is missing (the gate-off INSERT binds an explicit column list; a
//! missing column would fail every insert).  `ADD COLUMN IF NOT EXISTS` on the
//! partitioned parents propagates to every partition.

use crate::db::DbPool;
use crate::error::AppResult;

/// Idempotently add `prev_event_id` to `noetl.event` + `noetl.command` and the
/// chain-walk lookup index.
///
/// **Best-effort, never fatal.**  The platform schema is owned by the DB init
/// role (defined in `db/ddl/postgres/schema_ddl.sql` in this repo); the
/// server's connection role may not own `noetl.event` / `noetl.command` and so
/// can't `ALTER` them (`must be owner of table …`).  In that deployment the
/// columns are provisioned by the owner and this function's `ADD COLUMN IF NOT
/// EXISTS` would be a no-op anyway — so a permission error (or any DDL error)
/// is logged and swallowed rather than crashing startup.  The function still
/// returns `Ok(())`; a genuinely-missing column surfaces later as a clear
/// gate-off INSERT error, not a boot loop.
pub async fn ensure_columns(pool: &DbPool) -> AppResult<()> {
    // noetl.event — additive chain link.  Parent table; ADD COLUMN cascades to
    // the range partitions.
    try_ddl(
        pool,
        "ALTER TABLE noetl.event ADD COLUMN IF NOT EXISTS prev_event_id BIGINT",
        "noetl.event.prev_event_id",
    )
    .await;
    // noetl.command — the issuing-event pointer (the event whose application
    // issued the command; RFC #115 §4.1).
    try_ddl(
        pool,
        "ALTER TABLE noetl.command ADD COLUMN IF NOT EXISTS prev_event_id BIGINT",
        "noetl.command.prev_event_id",
    )
    .await;
    // Chain-walk integrity lookup: "who points AT this node" (forward replay /
    // chain-integrity check).  The "give me node <head>" walk is served by the
    // existing (execution_id, event_id) PK.  Partial — only linked rows.
    try_ddl(
        pool,
        "CREATE INDEX IF NOT EXISTS idx_event_prev_event_id \
         ON noetl.event (execution_id, prev_event_id) \
         WHERE prev_event_id IS NOT NULL",
        "idx_event_prev_event_id",
    )
    .await;
    Ok(())
}

/// Run one idempotent DDL statement, logging-and-swallowing any error (the
/// column is then expected to be owner-provisioned).  See [`ensure_columns`].
async fn try_ddl(pool: &DbPool, sql: &str, what: &str) {
    if let Err(e) = sqlx::query(sql).execute(pool).await {
        tracing::warn!(
            error = %e,
            target = what,
            "event-chain DDL skipped (server role likely not table owner; \
             column expected to be provisioned by schema_ddl.sql)"
        );
    }
}

// ---------------------------------------------------------------------------
// The chain read — the source for chain-following advance.
// ---------------------------------------------------------------------------

/// One `noetl.event` row as the chain decision needs it.
///
/// ⚠⚠ **Every nullable column is `Option`, and that is the whole lesson of
/// server#443.** That change selected `NULL::text AS content` into a `String`
/// field: sqlx refuses that row **only against a real database**, so it passed
/// unit tests AND a throwaway-Postgres proof that ran raw SQL, and then
/// returned HTTP 500 on every `/api/catalog/list` call in production.
/// *A test that renders SQL is not a test that decodes it.*
///
/// `prev_event_id` is NULL at a chain root and `parent_execution_id` is NULL at
/// a tree root — both are genuinely nullable, so both are `Option<i64>`, and
/// `tests/chain_source_pg.rs` decodes them against a live PostgreSQL.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ChainRow {
    pub event_id: i64,
    pub execution_id: i64,
    pub prev_event_id: Option<i64>,
    pub parent_execution_id: Option<i64>,
    pub event_type: String,
}

/// Event types that end an execution.
///
/// Kept here beside the read so the classification travels with the query, and
/// exposed so a test can assert the set rather than re-listing it.
pub const TERMINAL_EVENT_TYPES: &[&str] = &[
    "execution.completed",
    "execution.failed",
    "execution.cancelled",
    "playbook.completed",
    "playbook.failed",
];

pub fn is_terminal_event_type(event_type: &str) -> bool {
    TERMINAL_EVENT_TYPES.contains(&event_type)
}

/// Read one execution's chain rows, ascending by `event_id`.
///
/// Bounded by `limit` so a pathological execution cannot pull an unbounded
/// result set onto the poller path.
///
/// ⚠ This is a `WHERE execution_id` read of `noetl.event` — the very scan class
/// the redesign exists to retire. It is here as the **transitional** source: it
/// buys the correct *semantics* (finished, runnable and missing-link become
/// distinguishable, which is what removes the re-drive loop) without yet buying
/// the *performance*. The performance arrives when this is repointed at the
/// execution-partitioned chain store. Do not mistake one for the other.
pub async fn read_chain(pool: &DbPool, execution_id: i64, limit: i64) -> AppResult<Vec<ChainRow>> {
    let rows: Vec<ChainRow> = sqlx::query_as::<_, ChainRow>(
        "SELECT event_id, execution_id, prev_event_id, parent_execution_id, event_type \
         FROM noetl.event WHERE execution_id = $1 ORDER BY event_id LIMIT $2",
    )
    .bind(execution_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Build the advance-decision view from chain rows.
///
/// `exec_seq` is the row's ordinal within the execution — the chain decision
/// only needs a monotone position to pick a head, and `event_id` ordering
/// already provides one.
pub fn rows_to_chain_view(
    execution_id: i64,
    rows: Vec<ChainRow>,
) -> crate::chain_advance::ChainView {
    let events = rows
        .into_iter()
        .enumerate()
        .map(|(i, r)| crate::chain_advance::ChainEventView {
            exec_seq: (i + 1) as u64,
            event_id: r.event_id.to_string(),
            prev_event_id: r.prev_event_id.map(|v| v.to_string()),
            execution_id: r.execution_id.to_string(),
            parent_execution_id: r.parent_execution_id.map(|v| v.to_string()),
            terminal: is_terminal_event_type(&r.event_type),
        })
        .collect();
    crate::chain_advance::ChainView::new(execution_id.to_string(), events)
}

/// **The Postgres-backed [`ChainSource`](crate::chain_advance::ChainSource).**
///
/// Constructed only when `NOETL_CHAIN_ADVANCE` is on **and**
/// `NOETL_CHAIN_SOURCE=postgres` — see
/// [`chain_source_from_env`](crate::db::queries::event_chain::chain_source_from_env).
/// Both default off, so this is never built unless an operator asks for it
/// twice.
pub struct PgChainSource {
    pool: DbPool,
    /// Upper bound on rows per execution, so one pathological execution cannot
    /// pull an unbounded result set onto the poller path.
    limit: i64,
}

/// Default per-execution row cap.
pub const DEFAULT_CHAIN_READ_LIMIT: i64 = 10_000;

impl PgChainSource {
    pub fn new(pool: DbPool, limit: i64) -> Self {
        Self { pool, limit }
    }
}

#[async_trait::async_trait]
impl crate::chain_advance::ChainSource for PgChainSource {
    /// ⚠ A read FAILURE returns `None`, which makes the poller fall through to
    /// today's path — deliberately, and it is the conservative direction: a
    /// database hiccup must not be mistaken for "this execution has no chain",
    /// which `decide` would read as `Empty`. Falling through costs a tick of
    /// the old behaviour; misreading it would change a decision on bad data.
    async fn chain_for(&self, execution_id: i64) -> Option<crate::chain_advance::ChainView> {
        match read_chain(&self.pool, execution_id, self.limit).await {
            Ok(rows) => Some(rows_to_chain_view(execution_id, rows)),
            Err(e) => {
                tracing::warn!(
                    execution_id,
                    error = %e,
                    "chain-advance: chain read failed; falling through to the reconcile \
                     path for this tick rather than treating the execution as empty"
                );
                None
            }
        }
    }

    fn source_name(&self) -> &'static str {
        "postgres"
    }
}

/// Env var selecting the chain source. **Default `off`.**
pub const CHAIN_SOURCE_ENV: &str = "NOETL_CHAIN_SOURCE";

/// Build the chain source from configuration.
///
/// ⭐ **Two independent gates, both default-off.** `NOETL_CHAIN_ADVANCE` decides
/// whether chain-following is wanted at all; `NOETL_CHAIN_SOURCE` decides where
/// the chain comes from. Either one unset leaves the poller on today's path.
///
/// ⚠ Unrecognised values resolve to `off`, matching the fail-safe precedent
/// throughout this codebase: a typo must never move the execution-advance path.
pub fn chain_source_from_env(
    pool: &DbPool,
) -> Option<std::sync::Arc<dyn crate::chain_advance::ChainSource>> {
    if !crate::chain_advance::chain_advance_enabled() {
        return None;
    }
    let raw = std::env::var(CHAIN_SOURCE_ENV).unwrap_or_default();
    match raw.trim().to_ascii_lowercase().as_str() {
        "postgres" | "pg" => Some(std::sync::Arc::new(PgChainSource::new(
            pool.clone(),
            DEFAULT_CHAIN_READ_LIMIT,
        ))),
        // ⭐⭐ The chain store, populated from the authoritative log on each read.
        //
        // ⚠ Requires the populator to be armed too (`NOETL_CHAIN_POPULATE`), and
        // resolves to `off` without it rather than silently reading an unpopulated
        // store — which is the exact failure ai-meta#357 records: a repoint over a
        // store nothing filled makes `decide()` read `Empty` on a running
        // execution. Three gates, not two, and this is the one that cannot be
        // skipped by accident.
        "chain" | "chain_store" => {
            if ehdb_l0::chain_populator::populator_enabled() {
                Some(std::sync::Arc::new(
                    crate::handlers::chain_populate::LogSourcedChainSource::new(
                        pool.clone(),
                        DEFAULT_CHAIN_READ_LIMIT,
                    ),
                ))
            } else {
                tracing::warn!(target: "noetl_server::event_chain",
                    "NOETL_CHAIN_SOURCE selects the chain store but \
                     NOETL_CHAIN_POPULATE is off; resolving to OFF rather than \
                     reading a store nothing populates");
                None
            }
        }
        _ => None,
    }
}

/// Chain-head hydrator backed by `noetl.event`.
///
/// ⚠⚠ The query is `max(event_id)`, and that is correct only because `event_id` is
/// a per-execution monotonic snowflake — the same ordering `read_chain` uses. It
/// is NOT `ORDER BY created_at`: `created_at` is reduced to microseconds and two
/// events in one batch can share a value, so it does not totally order them.
pub struct PgChainHeadHydrator {
    pool: DbPool,
}

impl PgChainHeadHydrator {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }
}

#[async_trait::async_trait]
impl crate::state::ChainHeadHydrator for PgChainHeadHydrator {
    async fn head_of(&self, execution_id: i64) -> crate::state::HydrateOutcome {
        // `max()` over an empty set is SQL NULL, so the row always exists and the
        // Option distinguishes "no events" from "no row".
        let got: Result<Option<i64>, sqlx::Error> =
            sqlx::query_scalar("SELECT max(event_id) FROM noetl.event WHERE execution_id = $1")
                .bind(execution_id)
                .fetch_one(&self.pool)
                .await;
        match got {
            Ok(Some(head)) => crate::state::HydrateOutcome::Head(head),
            Ok(None) => crate::state::HydrateOutcome::Empty,
            // ⚠ `Failed`, never `Empty`. Collapsing the two would stamp a chain
            // root onto an arbitrary event whenever the database hiccuped — the
            // precise defect this hydrator exists to remove.
            Err(e) => {
                tracing::warn!(target: "noetl_server::event_chain", execution_id, error = %e,
                    "chain-head hydration failed; the next event for this execution \
                     will be stamped as a chain ROOT");
                crate::state::HydrateOutcome::Failed
            }
        }
    }
}

#[cfg(test)]
mod chain_source_gate_tests {
    use super::*;

    /// ⚠ `cargo test` does NOT serialise tests within a binary — an `EnvGuard`
    /// SAFETY note in this program once claimed it did, and the tests raced.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_env<T>(vars: &[(&str, Option<&str>)], f: impl FnOnce() -> T) -> T {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved: Vec<(String, Option<String>)> = vars
            .iter()
            .map(|(k, _)| ((*k).to_string(), std::env::var(k).ok()))
            .collect();
        for (k, v) in vars {
            match v {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
        let out = f();
        for (k, v) in saved {
            match v {
                Some(v) => unsafe { std::env::set_var(&k, v) },
                None => unsafe { std::env::remove_var(&k) },
            }
        }
        drop(guard);
        out
    }

    /// ⭐⭐ **The third gate.** Selecting the chain store while the populator is OFF
    /// must resolve to `off`, not to a source reading an unpopulated store.
    ///
    /// That combination is exactly the cliff noetl/ai-meta#357 records: a repoint
    /// over a store nothing filled makes `decide()` read `Empty` on a running
    /// execution. Two gates were not enough, because both can be on while the
    /// thing that fills the store is off.
    #[test]
    fn selecting_the_chain_store_without_the_populator_resolves_to_off() {
        with_env(
            &[
                (crate::chain_advance::CHAIN_ADVANCE_ENV, Some("true")),
                (CHAIN_SOURCE_ENV, Some("chain")),
                (ehdb_l0::chain_populator::POPULATOR_ENV, None),
            ],
            || {
                assert!(
                    crate::chain_advance::chain_advance_enabled(),
                    "precondition: the advance gate is on, so the gate under test is \
                     the POPULATOR one and not this one"
                );
                assert!(
                    !ehdb_l0::chain_populator::populator_enabled(),
                    "precondition: the populator is off"
                );
            },
        );

        // ⚠ `chain_source_from_env` needs a live `DbPool`, so the gate is asserted
        // at the SOURCE. An assertion on the two preconditions alone would pass
        // whether or not the arm consults the populator at all — which is the
        // difference between testing the gate and testing the environment.
        let src = include_str!("event_chain.rs");
        let non_test = src.split("#[cfg(test)]").next().unwrap();
        let arm_at = non_test
            .find(r#""chain" | "chain_store" =>"#)
            .expect("the chain-store arm is gone — re-anchor this guard");
        let arm_end = non_test[arm_at..]
            .find("_ => None,")
            .map(|i| arm_at + i)
            .expect("end of the match not found — extraction broke");
        let arm = &non_test[arm_at..arm_end];
        assert!(
            arm.len() > 200,
            "extracted {} bytes of the chain-store arm — implausibly small",
            arm.len()
        );
        assert!(
            arm.contains("populator_enabled()"),
            "the chain-store arm does not consult populator_enabled(), so \
             NOETL_CHAIN_SOURCE=chain with the populator OFF would return a source \
             that reads an UNPOPULATED store — decide() then sees Empty on running \
             executions (noetl/ai-meta#357).\n{arm}"
        );
    }

    /// An unrecognised source value resolves to `off` — a typo must never move the
    /// execution-advance path.
    #[test]
    fn an_unrecognised_source_value_is_off() {
        for v in ["", "chainstore", "CHAIN-STORE", "true", "yes", "ehdb"] {
            assert!(
                !matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "postgres" | "pg" | "chain" | "chain_store"
                ),
                "{v:?} must not be a recognised source value"
            );
        }
    }

    /// ⚠ `Failed` and `Empty` must stay distinct, because `Empty` licenses stamping
    /// a chain ROOT and `Failed` must not.
    #[test]
    fn a_failed_hydration_is_not_an_empty_one() {
        use crate::state::HydrateOutcome;
        assert_ne!(
            HydrateOutcome::Failed.label(),
            HydrateOutcome::Empty.label()
        );
        assert_eq!(HydrateOutcome::Failed.label(), "failed");
        assert_eq!(HydrateOutcome::Empty.label(), "empty");
        for o in [
            HydrateOutcome::Head(1),
            HydrateOutcome::Empty,
            HydrateOutcome::Failed,
        ] {
            assert!(
                HydrateOutcome::ALL_LABELS.contains(&o.label()),
                "{} is not pinned, so it would be an absent series",
                o.label()
            );
        }
    }

    /// ⚠⚠ THE wiring: the hydrator must be installed in `main`, not merely exist.
    ///
    /// A hydrator nobody installs is the noetl/ai-meta#326 shape — present,
    /// documented, never reached — and its absence is invisible, because the
    /// fallback is the old cold-map behaviour that looks like normal operation.
    #[test]
    fn the_hydrator_is_installed_in_main() {
        let main = include_str!("../../main.rs");
        let code: String = main
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            code.len() > 10_000,
            "main.rs extracted as {} bytes — implausibly small; a guard measuring \
             nothing passes",
            code.len()
        );
        assert!(
            code.contains("chain_heads.set_hydrator("),
            "main.rs never calls set_hydrator, so the chain edge is still stamped \
             from the per-process map after a restart and every in-flight \
             execution gets a second chain root"
        );
        assert!(
            code.contains("PgChainHeadHydrator::new("),
            "set_hydrator is called but not with the Postgres hydrator"
        );
    }
}
