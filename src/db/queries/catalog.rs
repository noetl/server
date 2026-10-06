//! Catalog database queries.

use crate::db::models::CatalogEntry;
use crate::db::DbPool;
use crate::error::AppResult;

/// Ensure the catalog kinds the Rust server owns exist in
/// `noetl.resource` (the kind lookup that `noetl.catalog.kind` FK-references).
///
/// `noetl.catalog.kind` references `noetl.resource(name)`, so a catalog
/// register of a `kind: Subscription` (noetl/ai-meta#90 Phase 2) fails with a
/// foreign-key violation unless `subscription` is a seeded resource type.  The
/// the seed lives in `db/ddl/postgres/schema_ddl.sql` in this repo; this idempotent
/// startup upsert is the safety net so a `kind: Subscription` registers on any
/// cluster the Rust server boots against, without an out-of-band migration.
/// Only kinds the server explicitly knows about are seeded — the FK still
/// rejects an unknown/typo'd kind.
pub async fn ensure_builtin_kinds(pool: &DbPool) -> AppResult<()> {
    sqlx::query(
        r#"
        INSERT INTO noetl.resource (name, meta) VALUES
            ('subscription', '{"description":"Source-driven subscription/listener workload (noetl/ai-meta#90)","executable":true,"catalog":true}'::jsonb)
        ON CONFLICT (name) DO UPDATE
        SET meta = COALESCE(noetl.resource.meta, '{}'::jsonb) || EXCLUDED.meta
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Get the next version number for a path.
///
/// `noetl.catalog.version` is Postgres `smallint`; using `i16` here
/// matches the column type and avoids the sqlx decode mismatch that
/// surfaced during noetl/ai-meta#49 Phase A ui_schema validation.
pub async fn get_next_version(pool: &DbPool, path: &str) -> AppResult<i16> {
    // `smallint + integer-literal` returns `INT4` in Postgres, so we cast
    // the entire expression back to `smallint` to match the `i16` binding
    // the sqlx decoder expects.  Without the outer cast sqlx errors with
    // `Rust type 'i16' (as SQL type 'INT2') is not compatible with SQL
    // type 'INT4'`.
    let result: Option<(i16,)> = sqlx::query_as(
        r#"
        SELECT (COALESCE(MAX(version), 0)::smallint + 1)::smallint
        FROM noetl.catalog
        WHERE path = $1
        "#,
    )
    .bind(path)
    .fetch_optional(pool)
    .await?;

    Ok(result.map(|(v,)| v).unwrap_or(1))
}

/// Insert a new catalog entry.
#[allow(clippy::too_many_arguments)]
pub async fn insert_catalog_entry(
    pool: &DbPool,
    path: &str,
    kind: &str,
    version: i16,
    content: &str,
    layout: Option<&serde_json::Value>,
    payload: Option<&serde_json::Value>,
    meta: Option<&serde_json::Value>,
) -> AppResult<i64> {
    // `noetl.catalog` has no `id` column — the PK is `catalog_id`.
    // Older code returned `id`, which would 500 with `column "id" does
    // not exist` at runtime.  Same alias-vs-column drift as the
    // v2.1.5 catalog `list` fix and the comment on get_catalog_by_id.
    let result: (i64,) = sqlx::query_as(
        r#"
        INSERT INTO noetl.catalog (path, kind, version, content, layout, payload, meta)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING catalog_id
        "#,
    )
    .bind(path)
    .bind(kind)
    .bind(version)
    .bind(content)
    .bind(layout)
    .bind(payload)
    .bind(meta)
    .fetch_one(pool)
    .await?;

    Ok(result.0)
}

/// Get a catalog entry by ID.
///
/// Filters on `catalog_id` (the real PK column).  Older versions
/// wrote `WHERE id = $1`, which would fail at runtime because the
/// table has no `id` column — only `catalog_id` aliased as `id` in
/// the SELECT list.  Same alias-vs-column drift as the v2.1.5
/// catalog `list` fix.
pub async fn get_catalog_by_id(pool: &DbPool, id: i64) -> AppResult<Option<CatalogEntry>> {
    let entry = sqlx::query_as::<_, CatalogEntry>(
        r#"
        SELECT catalog_id AS id, path, kind, version, content, layout, payload, meta, created_at AT TIME ZONE 'UTC' as created_at
        FROM noetl.catalog
        WHERE catalog_id = $1
        "#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(entry)
}

/// Get a catalog entry by path and version.
pub async fn get_catalog_by_path_version(
    pool: &DbPool,
    path: &str,
    version: i16,
) -> AppResult<Option<CatalogEntry>> {
    let sql = format!(
        "SELECT catalog_id AS id, path, kind, version, content, layout, payload, meta, created_at AT TIME ZONE 'UTC' as created_at FROM noetl.catalog WHERE path = $1 AND version = $2{archived}",
        archived = archived_filter()
    );
    let entry = sqlx::query_as::<_, CatalogEntry>(&sql)
        .bind(path)
        .bind(version)
        .fetch_optional(pool)
        .await?;

    Ok(entry)
}

/// Get the latest catalog entry by path.
pub async fn get_catalog_latest(pool: &DbPool, path: &str) -> AppResult<Option<CatalogEntry>> {
    let sql = format!(
        "SELECT catalog_id AS id, path, kind, version, content, layout, payload, meta, created_at AT TIME ZONE 'UTC' as created_at FROM noetl.catalog WHERE path = $1{archived} ORDER BY version DESC LIMIT 1",
        archived = archived_filter()
    );
    let entry = sqlx::query_as::<_, CatalogEntry>(&sql)
        .bind(path)
        .fetch_optional(pool)
        .await?;

    Ok(entry)
}

/// List all catalog entries, optionally filtered by kind.
/// `include_archived` opts in to soft-deleted rows (noetl/ai-meta#237).
///
/// The default hides them: an archived entry is retired, and a listing that
/// still showed it would defeat the point. The opt-in exists so an operator can
/// see what was retired and restore it.
/// How a catalog listing is shaped — noetl/server#436.
#[derive(Debug, Clone, Copy, Default)]
pub struct CatalogListOptions {
    /// Newest version per path only.
    pub latest_only: bool,
    /// Fetch the `content` / `layout` bodies.
    pub include_content: bool,
    /// Maximum rows; `None` means all.
    pub limit: Option<i32>,
    /// Rows to skip.
    pub offset: i32,
}

/// The body columns, or typed NULLs in their place.
///
/// ⚠ The NULLs are selected instead of the columns, not stripped afterwards, so
/// the bytes never leave Postgres. Fetching 32 MB into the server and then
/// discarding it would fix the client's memory and none of the database's,
/// the socket's, or the server's.
///
/// The casts are required: without them Postgres types a bare NULL as `text`,
/// and `layout` decodes as `jsonb`.
fn body_columns(include_content: bool) -> &'static str {
    if include_content {
        "content, layout"
    } else {
        "NULL::text AS content, NULL::jsonb AS layout"
    }
}

/// The `payload` column, or a listing projection of it.
///
/// ⚠⚠ noetl/server#436 replaced `content` and `layout` with typed NULLs and left
/// `payload` selected unconditionally. Once those two were NULL, `payload` became
/// **98.1% of the response** — measured on prod 2026-09-24, 13.28 MB of a 13.82 MB
/// listing — and **`workflow` alone is 92.6%** of that. The #436 guard asserted
/// only content+layout, so the test encoded the same omission as the code.
///
/// Every consumer of a LISTED payload reads exactly three things:
/// `metadata.description` and `metadata.name` (the console's card + search, and
/// `NoetlPrompt`), and `workflow.length` — the "Tasks" count on the catalog card.
/// Nothing renders a listed workflow's step bodies; the editor and the test lab
/// fetch a single entry through `/api/catalog/resource`.
///
/// So the listing BUILDS a reduced object rather than editing the stored one:
/// `metadata` verbatim, plus a workflow array of empty objects. The array keeps
/// its length, so `payload?.workflow?.length` — the console's "Tasks" count — is
/// unchanged for existing clients, while the step bodies and every other key
/// (`workload`, `keychain`, `workbook`, …) stay in Postgres.
///
/// ⭐ Building the object also drops `anthropic_secret_path` / `openai_secret_path`
/// from listings, which a catalog page has no reason to carry.
///
/// Asking for bodies returns the real column, and the detail path
/// (`get_catalog_by_id` / `get_catalog_by_path_version`) is untouched — it still
/// selects `content, layout, payload, meta`, so the editor and any consumer that
/// needs a real body fetches one entry through `/api/catalog/resource`.
///
/// ⚠ A NULL `payload` stays NULL: `jsonb_typeof(NULL->'workflow')` is NULL, so the
/// CASE falls through to `ELSE payload`.
fn payload_column(include_content: bool) -> &'static str {
    if include_content {
        "payload"
    } else {
        "CASE WHEN payload IS NULL THEN NULL ELSE jsonb_strip_nulls(jsonb_build_object( \
              'metadata', payload->'metadata', \
              'workflow', CASE WHEN jsonb_typeof(payload->'workflow') = 'array' \
                               THEN coalesce((SELECT jsonb_agg('{}'::jsonb) \
                                    FROM jsonb_array_elements(payload->'workflow')), '[]'::jsonb) \
                               ELSE NULL END)) END AS payload"
    }
}

/// The inner row source, shared by the count and the page so they cannot
/// disagree about what "matching" means.
fn matching_rows(kind: Option<&str>, archived: &str, opts: &CatalogListOptions) -> String {
    matching_rows_for(kind, archived, opts, false)
}

/// `for_count` selects the smallest column list that still defines the same row
/// set, because counting does not read any of them.
///
/// ⚠ This exists because `payload_column` is no longer a bare column
/// reference: for a listing it carries a sublink over `jsonb_array_elements`.
/// Leaving that in the count's row source would make "how many are there" pay
/// for a projection it discards — the same mistake #436's own comment warns
/// about for the bodies. `path` and `version` are kept because the
/// `latest_only` branch's `DISTINCT ON (path) … ORDER BY path, version DESC`
/// needs both in scope.
fn matching_rows_for(
    kind: Option<&str>,
    archived: &str,
    opts: &CatalogListOptions,
    for_count: bool,
) -> String {
    let body = body_columns(opts.include_content);
    // ⚠ `LOWER(kind)`, not `kind` — and the bound value is lowercased too.
    //
    // BOTH halves are required, and each alone leaves a different half of prod
    // wrong. Measured on prod 2026-10-06 across 1,527 catalog rows:
    //
    //     kind = 'Playbook'   875 rows
    //     kind = 'playbook'   650 rows
    //     kind = 'mcp'          2 rows
    //
    // `register` has lowercased unconditionally since f0301e86 (2026-06-09), but
    // that fix normalised future writes and never backfilled the past, so the
    // column holds both spellings. With a case-sensitive predicate a filter for
    // `playbook` returned 650 of 1,525 playbooks and one for `Playbook` returned
    // 875 — **neither empty**, which is the dangerous part: a partial result
    // looks exactly like a working query. Normalising only the parameter would
    // still return 650 and still look fine.
    //
    // There is no index on `kind` to lose to the function call (the table carries
    // only the PK and `UNIQUE (path, version)`), so this costs nothing today. If
    // one is ever added it must be `ON noetl.catalog (LOWER(kind))` to match.
    let kind_pred = if kind.is_some() {
        "LOWER(kind) = $1"
    } else {
        "1 = 1"
    };
    let payload = payload_column(opts.include_content);
    let cols = if for_count {
        "path, version".to_string()
    } else {
        format!("catalog_id, path, kind, version, {body}, {payload}, meta, created_at")
    };
    if opts.latest_only {
        // DISTINCT ON keeps the first row per path under ITS ordering, so the
        // version ordering has to live here and the display ordering outside.
        format!(
            "SELECT DISTINCT ON (path) {cols} FROM noetl.catalog \
             WHERE {kind_pred}{archived} ORDER BY path, version DESC"
        )
    } else {
        format!("SELECT {cols} FROM noetl.catalog WHERE {kind_pred}{archived}")
    }
}

/// List catalog entries, with the total that matched before paging.
pub async fn list_catalog_entries(
    pool: &DbPool,
    kind: Option<&str>,
    include_archived: bool,
    opts: &CatalogListOptions,
) -> AppResult<(Vec<CatalogEntry>, i64)> {
    // Absent column => no predicate at all, regardless of `include_archived`:
    // there is nothing to filter and referencing it would break the query.
    let archived = if include_archived {
        ""
    } else {
        archived_filter()
    };

    // ⚠ The count uses the row source WITHOUT bodies regardless of
    // `include_content`: counting does not read them, and selecting 32 MB to
    // discard it would double the cost of asking "how many are there".
    let count_opts = CatalogListOptions {
        include_content: false,
        ..*opts
    };
    let count_sql = format!(
        "SELECT count(*) FROM ({}) t",
        matching_rows_for(kind, archived, &count_opts, true)
    );
    // Lowercased to match the `LOWER(kind)` predicate. Callers legitimately send
    // the capitalised form — the CLI's own help text says `Playbook`, and so does
    // this module's rustdoc — so the normalisation belongs here rather than being
    // every caller's problem.
    let kind_lower = kind.map(str::to_lowercase);

    let total: (i64,) = if let Some(k) = kind_lower.as_deref() {
        sqlx::query_as(&count_sql).bind(k).fetch_one(pool).await?
    } else {
        sqlx::query_as(&count_sql).fetch_one(pool).await?
    };

    // `limit` and `offset` are i32 already validated non-negative by
    // `resolve_catalog_paging`, so they interpolate as integer literals with no
    // injection surface. They are not bound because the optional `kind` would
    // otherwise shift the parameter numbering between the two query shapes.
    let window = match opts.limit {
        Some(l) => format!(" LIMIT {l} OFFSET {}", opts.offset),
        None if opts.offset > 0 => format!(" OFFSET {}", opts.offset),
        None => String::new(),
    };
    let sql = format!(
        "SELECT catalog_id AS id, path, kind, version, content, layout, payload, meta, \
         created_at AT TIME ZONE 'UTC' as created_at FROM ({}) t ORDER BY created_at DESC{window}",
        matching_rows(kind, archived, opts)
    );
    let entries = if let Some(k) = kind_lower.as_deref() {
        sqlx::query_as::<_, CatalogEntry>(&sql)
            .bind(k)
            .fetch_all(pool)
            .await?
    } else {
        sqlx::query_as::<_, CatalogEntry>(&sql)
            .fetch_all(pool)
            .await?
    };

    Ok((entries, total.0))
}

/// Get all versions of a catalog entry by path.
pub async fn get_catalog_all_versions(pool: &DbPool, path: &str) -> AppResult<Vec<CatalogEntry>> {
    let sql = format!(
        "SELECT catalog_id AS id, path, kind, version, content, layout, payload, meta, created_at AT TIME ZONE 'UTC' as created_at FROM noetl.catalog WHERE path = $1{archived} ORDER BY version DESC",
        archived = archived_filter()
    );
    let entries = sqlx::query_as::<_, CatalogEntry>(&sql)
        .bind(path)
        .fetch_all(pool)
        .await?;

    Ok(entries)
}

/// Delete catalog entries for `path`, optionally narrowed to one `version`.
///
/// Returns the `(catalog_id, version)` of every row removed, so the caller can
/// report exactly what went — a delete that reports only a count cannot be
/// audited afterwards, and this table has no replay to reconstruct from.
///
/// `RETURNING` makes the read and the delete one statement: a
/// select-then-delete would race a concurrent `register`, which mints a new
/// version for the same path (`get_next_version`), and could report a row it
/// did not remove.
///
/// Idempotent: deleting an absent path or version removes nothing and returns
/// an empty vector rather than erroring.
pub async fn delete_catalog_entries(
    pool: &DbPool,
    path: &str,
    version: Option<i16>,
) -> AppResult<Vec<(i64, i16)>> {
    let rows: Vec<(i64, i16)> = match version {
        Some(v) => {
            sqlx::query_as(
                r#"
                DELETE FROM noetl.catalog
                WHERE path = $1 AND version = $2
                RETURNING catalog_id, version
                "#,
            )
            .bind(path)
            .bind(v)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as(
                r#"
                DELETE FROM noetl.catalog
                WHERE path = $1
                RETURNING catalog_id, version
                "#,
            )
            .bind(path)
            .fetch_all(pool)
            .await?
        }
    };
    Ok(rows)
}

/// Whether `noetl.catalog.archived_at` exists in this database.
///
/// Resolved once at startup by [`ensure_archived_column`] and read by every
/// query that would otherwise reference the column.
///
/// This exists because the column is genuinely OPTIONAL: the server cannot
/// create it (the table is owned by a different role in every deployment
/// checked), so it may or may not be there, and **every read path must behave
/// exactly as it did before soft delete when it is absent**.
///
/// Getting this wrong took production down for ~90 seconds: a predicate
/// referencing a non-existent column made `resolve_catalog` return 500 on every
/// execute-by-path.  The feature degrading is fine; resolution breaking is not.
static ARCHIVED_COLUMN_PRESENT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// `" AND archived_at IS NULL"` when the column exists, `""` when it does not.
///
/// Every query that filters archived rows must interpolate this rather than
/// hard-coding the predicate.
pub fn archived_filter() -> &'static str {
    if ARCHIVED_COLUMN_PRESENT.load(std::sync::atomic::Ordering::Relaxed) {
        " AND archived_at IS NULL"
    } else {
        ""
    }
}

/// True when soft delete is available in this database.
pub fn archived_column_present() -> bool {
    ARCHIVED_COLUMN_PRESENT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Best-effort add of the soft-delete column, then DETECT whether it is there
/// (noetl/ai-meta#237).
///
/// The detection is deliberately independent of whether the `ALTER` succeeded.
/// `ALTER TABLE ... ADD COLUMN IF NOT EXISTS` fails with "must be owner of
/// table" for a non-owner **even when the column already exists**, so the
/// statement's result says nothing about the column's presence. Only
/// `information_schema` does.
///
/// That also means an operator adding the column out of band turns the feature
/// on at the next restart, with no code change.
pub async fn ensure_archived_column(pool: &DbPool) -> AppResult<()> {
    // Best effort — a non-owner cannot ALTER, and that is expected.
    let alter = sqlx::query(
        "ALTER TABLE noetl.catalog ADD COLUMN IF NOT EXISTS archived_at TIMESTAMPTZ NULL",
    )
    .execute(pool)
    .await;

    let present: Option<(i32,)> = sqlx::query_as(
        r#"
        SELECT 1 FROM information_schema.columns
        WHERE table_schema = 'noetl' AND table_name = 'catalog'
          AND column_name = 'archived_at'
        "#,
    )
    .fetch_optional(pool)
    .await?;

    let found = present.is_some();
    ARCHIVED_COLUMN_PRESENT.store(found, std::sync::atomic::Ordering::Relaxed);

    match (alter, found) {
        (Ok(_), _) => tracing::info!("noetl.catalog.archived_at present; soft delete enabled"),
        (Err(_), true) => tracing::info!(
            "noetl.catalog.archived_at already present (added out of band); soft delete enabled"
        ),
        (Err(e), false) => tracing::warn!(
            error = %e,
            "noetl.catalog.archived_at is ABSENT and cannot be added (table owned by another \
             role). Soft delete is unavailable; every other catalog path behaves exactly as \
             before. An owner can enable it with: ALTER TABLE noetl.catalog ADD COLUMN IF NOT \
             EXISTS archived_at TIMESTAMPTZ NULL;"
        ),
    }
    Ok(())
}

/// Test-only override so the conditional behaviour can be exercised both ways.
#[cfg(test)]
pub fn set_archived_column_present_for_test(v: bool) {
    ARCHIVED_COLUMN_PRESENT.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// Mark catalog entries archived (noetl/ai-meta#237).
///
/// The soft-delete counterpart of [`delete_catalog_entries`], and the only
/// retirement available to an entry that has execution history: `noetl.event`
/// holds a foreign key onto `noetl.catalog` and the event log is append-only,
/// so those rows can never be removed to release it.
///
/// Idempotent by construction — `archived_at IS NULL` in the predicate means
/// re-archiving an already-archived entry matches nothing and reports 0.
pub async fn archive_catalog_entries(
    pool: &DbPool,
    path: &str,
    version: Option<i16>,
) -> AppResult<Vec<(i64, i16)>> {
    let rows: Vec<(i64, i16)> = match version {
        Some(v) => {
            sqlx::query_as(
                r#"
                UPDATE noetl.catalog SET archived_at = now()
                WHERE path = $1 AND version = $2 AND archived_at IS NULL
                RETURNING catalog_id, version
                "#,
            )
            .bind(path)
            .bind(v)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as(
                r#"
                UPDATE noetl.catalog SET archived_at = now()
                WHERE path = $1 AND archived_at IS NULL
                RETURNING catalog_id, version
                "#,
            )
            .bind(path)
            .fetch_all(pool)
            .await?
        }
    };
    Ok(rows)
}

/// Un-archive catalog entries — the reverse of [`archive_catalog_entries`].
///
/// This is why soft delete is the right answer for #237: retirement is a plain
/// `UPDATE`, so it is fully reversible and destroys nothing. Nothing else in
/// this table has that property.
pub async fn restore_catalog_entries(
    pool: &DbPool,
    path: &str,
    version: Option<i16>,
) -> AppResult<Vec<(i64, i16)>> {
    let rows: Vec<(i64, i16)> = match version {
        Some(v) => {
            sqlx::query_as(
                r#"
                UPDATE noetl.catalog SET archived_at = NULL
                WHERE path = $1 AND version = $2 AND archived_at IS NOT NULL
                RETURNING catalog_id, version
                "#,
            )
            .bind(path)
            .bind(v)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as(
                r#"
                UPDATE noetl.catalog SET archived_at = NULL
                WHERE path = $1 AND archived_at IS NOT NULL
                RETURNING catalog_id, version
                "#,
            )
            .bind(path)
            .fetch_all(pool)
            .await?
        }
    };
    Ok(rows)
}

#[cfg(test)]
mod catalog_listing_shape {
    use super::*;

    /// ⭐⭐ THE GENERIC GUARD #436 DID NOT HAVE.
    ///
    /// #436 asserted that `content` and `layout` were nulled — the two columns it
    /// happened to fix — so `payload` stayed selected verbatim and became **98.1%
    /// of the response**. A guard naming only the columns already fixed cannot
    /// fail on the next one.
    ///
    /// This asserts the PROPERTY instead: no heavy column appears as a bare
    /// reference in a listing's select list. Add a heavy column to the table and
    /// this fails until it is nulled or projected.
    #[test]
    fn a_listing_selects_no_heavy_column_verbatim() {
        const HEAVY: [&str; 3] = ["content", "layout", "payload"];
        let sql = matching_rows(None, "", &CatalogListOptions::default());
        let select_list = sql
            .split_once("SELECT")
            .expect("no SELECT")
            .1
            .split_once(" FROM ")
            .expect("no FROM")
            .0
            .to_string();
        // Strip the DISTINCT ON prefix so its `(path)` is not mistaken for a column.
        let select_list = select_list
            .split_once(')')
            .filter(|(head, _)| head.contains("DISTINCT ON"))
            .map(|(_, tail)| tail.to_string())
            .unwrap_or(select_list);
        for col in HEAVY {
            let bare = select_list
                .split(',')
                .map(str::trim)
                .any(|f| f == col);
            assert!(
                !bare,
                "a listing selects `{col}` verbatim; it must be a typed NULL or a \
                 projection, or its bytes leave Postgres for every row. \
                 select list: {select_list}"
            );
        }
    }

    /// ⭐ The projection must preserve exactly what listing clients read.
    ///
    /// The console's catalog card shows `payload.workflow.length` as "Tasks", so
    /// the array has to keep its length while losing its step bodies. It also
    /// reads `payload.metadata.description` / `.name`, so `metadata` must survive
    /// untouched — which is why this projects the workflow key rather than
    /// nulling the whole payload.
    #[test]
    fn the_listing_payload_keeps_workflow_length_and_metadata() {
        let listing = payload_column(false);
        assert!(
            listing.contains("jsonb_build_object")
                && listing.contains("'metadata', payload->'metadata'")
                && listing.contains("jsonb_agg('{}'::jsonb)")
                && listing.contains("jsonb_array_elements(payload->'workflow')"),
            "the listing must BUILD {{metadata, workflow-stub}}, preserving the \
             array length: {listing}"
        );
        // The body keys must not be carried by a listing at all.
        for heavy in ["'workload'", "'keychain'", "'workbook'"] {
            assert!(
                !listing.contains(heavy),
                "a listing must not carry {heavy}: {listing}"
            );
        }
        assert!(
            !listing.contains("NULL::jsonb AS payload"),
            "nulling the whole payload would break the catalog page's description, \
             name and Tasks count: {listing}"
        );
        assert_eq!(
            payload_column(true),
            "payload",
            "asking for bodies must fetch the real column"
        );
    }

    /// ⚠ Counting must not pay for a projection it discards — the same mistake
    /// #436's own comment warns about for the bodies.
    #[test]
    fn the_count_row_source_carries_no_payload_projection() {
        let opts = CatalogListOptions::default();
        let counting = matching_rows_for(None, "", &opts, true);
        assert!(
            !counting.contains("jsonb_array_elements"),
            "the count must not evaluate the payload projection: {counting}"
        );
        assert!(
            counting.contains("path") && counting.contains("version"),
            "the count still needs path+version for the latest_only DISTINCT ON: {counting}"
        );
    }

    /// ⭐ noetl/server#436. The bodies must not be SELECTed when they are not
    /// wanted — the whole point is that the bytes never leave Postgres.
    #[test]
    fn the_bodies_are_replaced_by_typed_nulls_not_fetched_and_dropped() {
        let without = body_columns(false);
        assert!(
            without.contains("NULL::text AS content") && without.contains("NULL::jsonb AS layout"),
            "content/layout must be selected as typed NULLs, got: {without}"
        );
        assert!(
            !without
                .split("AS content")
                .next()
                .unwrap()
                .ends_with("content, "),
            "the real columns must not still be selected"
        );

        let with = body_columns(true);
        assert_eq!(
            with, "content, layout",
            "asking for bodies must fetch the real columns"
        );

        // The casts are load-bearing: a bare NULL types as text and `layout`
        // decodes as jsonb, so an uncast NULL fails at the decode boundary.
        assert!(
            without.contains("::text") && without.contains("::jsonb"),
            "both NULLs must be cast, got: {without}"
        );
    }

    /// The count and the page must agree on what "matching" means, or `total`
    /// describes a different set than `entries` — which would make the honesty
    /// field itself dishonest.
    #[test]
    fn latest_only_and_the_filters_apply_to_both_the_count_and_the_page() {
        let opts = CatalogListOptions {
            latest_only: true,
            include_content: false,
            limit: Some(10),
            offset: 0,
        };
        let page = matching_rows(Some("Playbook"), " AND archived_at IS NULL", &opts);
        let count_opts = CatalogListOptions {
            include_content: false,
            ..opts
        };
        let count = matching_rows(Some("Playbook"), " AND archived_at IS NULL", &count_opts);
        assert_eq!(
            page, count,
            "the count and the page must be built from the same row source"
        );

        assert!(
            page.contains("DISTINCT ON (path)"),
            "latest_only must dedupe by path: {page}"
        );
        assert!(
            page.contains("ORDER BY path, version DESC"),
            "DISTINCT ON keeps the FIRST row under its own ordering, so the \
             newest version is only selected if that ordering is version DESC: {page}"
        );
        assert!(
            page.contains("LOWER(kind) = $1"),
            "the kind filter must reach the row source, and it must be \
             case-insensitive: {page}"
        );
        assert!(
            page.contains("AND archived_at IS NULL"),
            "the archived filter must reach the row source: {page}"
        );
    }

    /// Without `latest_only` the listing must keep every version — the historical
    /// shape. A dedupe that leaked into the default would silently drop records.
    #[test]
    fn the_default_listing_keeps_every_version() {
        let opts = CatalogListOptions::default();
        let sql = matching_rows(None, "", &opts);
        assert!(
            !sql.contains("DISTINCT"),
            "the default listing must not dedupe — that would drop records \
             without the caller asking: {sql}"
        );
        assert!(
            sql.contains("1 = 1"),
            "absent kind must leave a valid predicate: {sql}"
        );
    }

    /// The kind filter must be case-insensitive on BOTH sides, and each side
    /// alone is insufficient.
    ///
    /// Measured on prod 2026-10-06 across 1,527 catalog rows: `kind` holds
    /// `'Playbook'` 875 times and `'playbook'` 650 times, because `register` has
    /// lowercased unconditionally since f0301e86 (2026-06-09) and that fix never
    /// backfilled the rows written before it.
    ///
    /// With a case-sensitive predicate, `list {"resource_type":"playbook"}`
    /// returned **650 of 1,525** playbooks and `"Playbook"` returned **875**.
    /// ⚠ Neither returned zero. That is what made this survive: an empty result
    /// gets reported, a 43%-complete one gets believed. No test covered it — the
    /// string `resource_type` appeared in 0 of 27 files under `tests/`.
    ///
    /// Both halves are asserted here because fixing either one alone still
    /// returns a wrong-but-plausible count:
    ///   - predicate only: a caller sending `Playbook` binds `Playbook`, which
    ///     `LOWER(kind)` never equals, so the result is empty.
    ///   - parameter only: `kind = 'playbook'` still misses the 875 legacy rows.
    #[test]
    fn the_kind_filter_is_case_insensitive_on_both_the_column_and_the_parameter() {
        // Half 1 — the column side, in every shape the row source can take.
        for latest_only in [false, true] {
            for include_content in [false, true] {
                let opts = CatalogListOptions {
                    latest_only,
                    include_content,
                    ..CatalogListOptions::default()
                };
                let sql = matching_rows(Some("Playbook"), "", &opts);
                assert!(
                    sql.contains("LOWER(kind) = $1"),
                    "latest_only={latest_only} include_content={include_content}: \
                     the kind predicate must be case-insensitive, got {sql}"
                );
                assert!(
                    !sql.contains("WHERE kind = $1") && !sql.contains("AND kind = $1"),
                    "latest_only={latest_only} include_content={include_content}: \
                     a bare case-sensitive `kind = $1` must not survive: {sql}"
                );
            }
        }

        // Half 2 — the parameter side. A source-text guard, in the house style,
        // because the bind happens inside an async fn that needs a live pool.
        //
        // ⚠ Scanned over the CODE only, not the whole file. The first draft used
        // `include_str!` unsliced and counted its OWN assertion messages, which
        // name the needles verbatim — it reported 4 uses where the code has 2 and
        // failed with the fix correctly in place. A guard that measures itself is
        // the same defect as a comment counting as a caller.
        //
        // Cut at the LAST `#[cfg(test)]` via `rfind`, not the first via
        // `split(..).next()`: this file has an earlier `#[cfg(test)]` block around
        // line 503 with real code after it, so the split idiom would discard ~90
        // lines of the code being measured and still report a plausible number.
        let whole = include_str!("catalog.rs");
        let code = &whole[..whole
            .rfind("#[cfg(test)]")
            .expect("this file must contain a test module for this guard to slice")];

        // ⚠ Assert the extraction BEFORE asserting about it. A guard that reads
        // an empty or truncated region finds no violations and reports success.
        assert!(
            code.len() > 10_000,
            "the pre-test slice of catalog.rs is only {} bytes of {} — this guard \
             is measuring nothing and must not report a pass",
            code.len(),
            whole.len()
        );
        let src = code;

        assert!(
            src.contains("let kind_lower = kind.map(str::to_lowercase);"),
            "the bound kind must be lowercased to match LOWER(kind); without it \
             a caller sending `Playbook` matches nothing at all"
        );

        // Neither bind site may pass the caller's string through unnormalised.
        let raw_binds = src.matches(".bind(k)").count();
        let normalised_sources = src.matches("kind_lower.as_deref()").count();
        assert_eq!(
            normalised_sources, 2,
            "both the count and the page must take the kind from the lowercased \
             value; found {normalised_sources} uses of kind_lower.as_deref()"
        );
        assert_eq!(
            raw_binds, 2,
            "expected exactly 2 `.bind(k)` sites (count + page); found \
             {raw_binds}. If a third was added it needs the same normalisation, \
             so update this guard deliberately rather than relaxing it"
        );
        assert!(
            !src.contains("if let Some(k) = kind {"),
            "a bind site is still destructuring the RAW `kind`; it must come from \
             `kind_lower` or the parameter is case-sensitive again"
        );
    }
}
