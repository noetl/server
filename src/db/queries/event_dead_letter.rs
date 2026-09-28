//! Provision `noetl.event_dead_letter` at startup, idempotently.
//!
//! The dead-letter table is the landing spot for events `noetl.event` will never
//! accept (see the DDL comment in `db/ddl/postgres/schema_ddl.sql`). It must exist
//! before the materializer can park anything, and the materializer refuses to ack
//! an unparked event — so a missing table degrades to "the drain stays blocked",
//! which is the safe direction but not a useful one.
//!
//! `CREATE TABLE IF NOT EXISTS` needs CREATE on the schema, not ownership of any
//! existing table, so unlike `event_chain::ensure_columns` this usually succeeds
//! on a deployment whose server role does not own `noetl.event`. Where it does
//! not, the error is logged and swallowed exactly as there: the table is then
//! expected to be owner-provisioned from `schema_ddl.sql`, and the consequence of
//! it being absent is a refused park, never a dropped event.

use crate::db::DbPool;
use crate::error::AppResult;

/// The table, verbatim from `db/ddl/postgres/schema_ddl.sql`.
///
/// Kept in sync by `dead_letter_ddl_matches_schema_ddl` below rather than by
/// hand — two copies of a CREATE TABLE is exactly the kind of thing that drifts.
pub const CREATE_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS noetl.event_dead_letter (
    execution_id    BIGINT NOT NULL,
    event_id        BIGINT NOT NULL,
    catalog_id      BIGINT,
    event_type      TEXT,
    node_name       TEXT,
    reason          TEXT NOT NULL,
    payload         JSONB NOT NULL,
    parked_by       TEXT,
    parked_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_at     TIMESTAMPTZ,
    PRIMARY KEY (execution_id, event_id)
)";

const CREATE_OUTSTANDING_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_event_dead_letter_outstanding \
    ON noetl.event_dead_letter (parked_at DESC) \
    WHERE resolved_at IS NULL";

const CREATE_EXECUTION_INDEX: &str = "\
CREATE INDEX IF NOT EXISTS idx_event_dead_letter_execution \
    ON noetl.event_dead_letter (execution_id, event_id)";

/// Ensure the dead-letter table and its indexes exist. Never fails startup.
pub async fn ensure_table(pool: &DbPool) -> AppResult<()> {
    for (sql, what) in [
        (CREATE_TABLE, "noetl.event_dead_letter"),
        (CREATE_OUTSTANDING_INDEX, "idx_event_dead_letter_outstanding"),
        (CREATE_EXECUTION_INDEX, "idx_event_dead_letter_execution"),
    ] {
        if let Err(e) = sqlx::query(sql).execute(pool).await {
            tracing::warn!(
                error = %e,
                target = what,
                "dead-letter DDL skipped (server role likely lacks CREATE on the \
                 schema; expected to be provisioned by schema_ddl.sql).  Until it \
                 exists the materializer will REFUSE to ack poison — the drain stays \
                 blocked rather than dropping an event with nowhere to land."
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::CREATE_TABLE;

    /// The startup DDL and `schema_ddl.sql` must declare the SAME table.
    ///
    /// Two copies of a CREATE TABLE drift, and the way this one would drift is
    /// the expensive way: a column the sink binds but the startup-provisioned
    /// table lacks means the park fails on a fresh deployment, so the drain stays
    /// blocked and nobody can tell why. Compared on the column-name set, so
    /// whitespace and comment differences between the two files are allowed but a
    /// missing or extra column is not.
    #[test]
    fn dead_letter_ddl_matches_schema_ddl() {
        let schema = include_str!("../../../db/ddl/postgres/schema_ddl.sql");
        let start = schema
            .find("CREATE TABLE IF NOT EXISTS noetl.event_dead_letter")
            .expect("schema_ddl.sql must declare noetl.event_dead_letter");
        let body = &schema[start..];
        let end = body.find(");").expect("the declaration must close");
        let from_schema = columns(&body[..end]);
        let from_const = columns(CREATE_TABLE);
        assert_eq!(
            from_const, from_schema,
            "the startup DDL and schema_ddl.sql declare different columns for \
             noetl.event_dead_letter — a fresh deployment would provision a table \
             the sink cannot write, and the drain would stay blocked silently"
        );
        assert!(
            from_const.contains(&"payload".to_string()),
            "the positive control: without `payload` the parked event is not \
             reconstructable and the ack would be a delete, so an empty or \
             mis-parsed column set must not pass"
        );
    }

    /// Column names in declaration order, ignoring comments and layout.
    fn columns(ddl: &str) -> Vec<String> {
        ddl.lines()
            .map(|l| l.split("--").next().unwrap_or("").trim())
            .filter(|l| {
                !l.is_empty()
                    && !l.starts_with("CREATE")
                    && !l.starts_with("PRIMARY KEY")
                    && !l.starts_with(')')
                    && !l.starts_with('(')
            })
            .filter_map(|l| l.split_whitespace().next().map(|c| c.to_string()))
            .collect()
    }
}
