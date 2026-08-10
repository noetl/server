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
/// canonical seed lives in `noetl/noetl`'s `schema_ddl.sql`; this idempotent
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
pub async fn list_catalog_entries(
    pool: &DbPool,
    kind: Option<&str>,
    include_archived: bool,
) -> AppResult<Vec<CatalogEntry>> {
    const COLS: &str = "catalog_id AS id, path, kind, version, content, layout, payload, meta, created_at AT TIME ZONE 'UTC' as created_at";
    // Absent column => no predicate at all, regardless of `include_archived`:
    // there is nothing to filter and referencing it would break the query.
    let archived = if include_archived {
        ""
    } else {
        archived_filter()
    };
    let entries = if let Some(k) = kind {
        let sql = format!(
            "SELECT {COLS} FROM noetl.catalog WHERE kind = $1{archived} ORDER BY created_at DESC"
        );
        sqlx::query_as::<_, CatalogEntry>(&sql)
            .bind(k)
            .fetch_all(pool)
            .await?
    } else {
        let sql = format!(
            "SELECT {COLS} FROM noetl.catalog WHERE 1 = 1{archived} ORDER BY created_at DESC"
        );
        sqlx::query_as::<_, CatalogEntry>(&sql)
            .fetch_all(pool)
            .await?
    };

    Ok(entries)
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
