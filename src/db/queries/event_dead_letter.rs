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
        (
            CREATE_OUTSTANDING_INDEX,
            "idx_event_dead_letter_outstanding",
        ),
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

/// One parked row, **without its payload**.
///
/// ⚠⚠ `payload` is deliberately absent, and that absence is enforced by
/// `the_listing_select_never_reads_payload` below.
///
/// A parked row carries the raw event payload, and an event payload can carry
/// resolved credential values — which is why every response that surfaces execution
/// state goes through the response-boundary scrub
/// (`agents/rules/execution-model.md`). Rather than wire a reader through scrub and
/// hope the coverage is right, this listing does not read the column at all: the
/// cheapest way to be sure a credential cannot leak through a response is for the
/// response never to contain the field.
///
/// The consequence is deliberate and worth stating: this is **enough to triage and
/// not enough to replay**. `reason` plus `event_type` identifies the poison class;
/// reconstructing the event needs the payload, and that is a separate decision with
/// its own scrub design (noetl/ai-meta#422).
#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct ParkedEvent {
    pub execution_id: i64,
    pub event_id: i64,
    pub catalog_id: Option<i64>,
    pub event_type: Option<String>,
    pub node_name: Option<String>,
    pub reason: String,
    pub parked_by: Option<String>,
    pub parked_at: chrono::DateTime<chrono::Utc>,
    pub resolved_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The column list the listing reads. Named so the guard can assert on it.
///
/// Ordered to match `ParkedEvent`, because `sqlx::FromRow` binds by name but a
/// reader comparing the two should not have to.
pub const LISTING_COLUMNS: &str = concat!(
    "execution_id, event_id, catalog_id, event_type, ",
    "node_name, reason, parked_by, parked_at, resolved_at"
);

/// How many rows are parked and unresolved, regardless of any listing limit.
///
/// ⚠ This is the denominator for [`list_parked`]. A list truncated by `limit` tells
/// you what you can see, not how much there is — and the whole failure this table
/// exists for is a backlog nobody noticed. Uses
/// `idx_event_dead_letter_outstanding`, the partial index on
/// `parked_at DESC WHERE resolved_at IS NULL`, which until now nothing queried.
pub async fn outstanding_count(pool: &DbPool) -> AppResult<i64> {
    let (n,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM noetl.event_dead_letter WHERE resolved_at IS NULL")
            .fetch_one(pool)
            .await?;
    Ok(n)
}

/// List parked rows, newest first, **metadata only**.
///
/// `execution_id` narrows to one run. `include_resolved` is off by default so the
/// common question — "what is stuck right now" — uses the partial index.
pub async fn list_parked(
    pool: &DbPool,
    execution_id: Option<i64>,
    include_resolved: bool,
    limit: i64,
) -> AppResult<Vec<ParkedEvent>> {
    let sql = format!(
        "SELECT {LISTING_COLUMNS}            FROM noetl.event_dead_letter           WHERE ($1::BIGINT IS NULL OR execution_id = $1)             AND ($2::BOOL OR resolved_at IS NULL)           ORDER BY parked_at DESC           LIMIT $3"
    );
    let rows = sqlx::query_as::<_, ParkedEvent>(&sql)
        .bind(execution_id)
        .bind(include_resolved)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows)
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
    /// ⚠⚠ The listing must never read `payload`.
    ///
    /// A parked row carries the raw event payload, and an event payload can carry
    /// resolved credential values. The listing avoids the scrub question entirely by
    /// not selecting the column — so this guard is the thing standing between that
    /// decision and someone adding one convenient field.
    ///
    /// Asserted on the column constant AND on the struct, because adding the field
    /// to only one of them would still change what reaches a client.
    #[test]
    fn the_listing_select_never_reads_payload() {
        assert!(
            !super::LISTING_COLUMNS.contains("payload"),
            "LISTING_COLUMNS reads `payload`: a parked payload can carry resolved \
             credential values, and this listing has no scrub. Either drop the column \
             or take the scrub decision deliberately (noetl/ai-meta#422)."
        );

        // The denominator: assert the constant is a plausible column list before
        // asserting what is NOT in it. An empty or truncated constant would satisfy
        // the check above vacuously.
        let cols: Vec<&str> = super::LISTING_COLUMNS
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        assert_eq!(
            cols.len(),
            9,
            "expected 9 listing columns, got {}: {cols:?}",
            cols.len()
        );
        for required in [
            "execution_id",
            "event_id",
            "reason",
            "parked_at",
            "resolved_at",
        ] {
            assert!(
                cols.contains(&required),
                "{required} missing from LISTING_COLUMNS"
            );
        }

        // And the serialised struct, which is what actually reaches a client.
        let src = include_str!("event_dead_letter.rs");
        let start = src
            .find("pub struct ParkedEvent {")
            .expect("ParkedEvent not found — this guard is reading the wrong thing");
        let body = &src[start..start + src[start..].find('}').expect("struct must close")];
        assert!(
            body.contains("execution_id"),
            "ParkedEvent slice looks wrong: {body:?}"
        );
        assert!(
            !body.contains("payload"),
            "ParkedEvent carries a `payload` field; it is serialised straight to the \
             client and there is no scrub on this path"
        );
    }

    /// The outstanding count must not be filtered by the listing's limit.
    ///
    /// A list truncated by `limit` tells you what you can see, not how much there is,
    /// and the failure this table exists for is a backlog nobody noticed. The count
    /// query therefore has no LIMIT and no execution filter.
    #[test]
    fn the_outstanding_count_is_unbounded_and_unfiltered() {
        let src = include_str!("event_dead_letter.rs");
        let start = src
            .find("pub async fn outstanding_count")
            .expect("outstanding_count not found");
        let body = &src[start..start + src[start..].find("\n}").expect("fn must close")];
        assert!(body.contains("count(*)"), "not a count query: {body:?}");
        assert!(
            body.contains("resolved_at IS NULL"),
            "the count must be of OUTSTANDING rows"
        );
        assert!(
            !body.to_uppercase().contains("LIMIT"),
            "the outstanding count must not be bounded by a limit — it is the \
             denominator for a possibly-truncated listing"
        );
        assert!(
            !body.contains("execution_id ="),
            "the outstanding count must not be narrowed to one execution"
        );
    }

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
