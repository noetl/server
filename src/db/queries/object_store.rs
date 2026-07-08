//! `noetl.object_store` queries — the durable object tier for large result
//! payloads (Arrow Feather, etc.), keyed by the §7 physical object key
//! (noetl/ai-meta#105 Round 5).
//!
//! This is the server-mediated backend for a plug-in's `noetl.object_put`
//! capability: the system worker pool writes the Feather bytes through the
//! server API at the cell/shard physical key, so placement, audit, and the
//! [data-access boundary](https://github.com/noetl/noetl/blob/main/agents/rules/data-access-boundary.md)
//! stay enforced — workers never touch the object store directly.
//!
//! This first tier stores bytes as `BYTEA` in Postgres, mirroring
//! `db::queries::plugin_module` and `result_store`. A gcs / s3 / Ceph backend
//! behind the same endpoint is a config-swappable follow-up; the HTTP contract
//! is the stable surface.

use sqlx::Row;

use crate::db::DbPool;
use crate::error::AppResult;

/// A stored object.
#[derive(Debug, Clone)]
pub struct ObjectRow {
    /// SHA-256 hex digest of the bytes.
    pub digest: String,
    /// Media type (`application/vnd.apache.arrow.feather`, …).
    pub media_type: String,
    /// The object bytes.
    pub bytes: Vec<u8>,
}

/// Idempotent table creation at startup; server-owned end to end.
pub async fn ensure_table(pool: &DbPool) -> AppResult<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS noetl.object_store (
            object_key  TEXT PRIMARY KEY,
            digest      TEXT NOT NULL,
            media_type  TEXT NOT NULL DEFAULT 'application/octet-stream',
            bytes       BYTEA NOT NULL,
            created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
        )
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Write (or overwrite) the object at `object_key`. Object writes are
/// content-addressed by the §7 key, so a re-write at the same key (a retry)
/// replaces the bytes idempotently — `digest` always reflects the current
/// bytes.
pub async fn put(
    pool: &DbPool,
    object_key: &str,
    digest: &str,
    media_type: &str,
    bytes: &[u8],
) -> AppResult<()> {
    sqlx::query(
        r#"
        INSERT INTO noetl.object_store (object_key, digest, media_type, bytes)
        VALUES ($1, $2, $3, $4)
        ON CONFLICT (object_key) DO UPDATE
          SET digest = EXCLUDED.digest,
              media_type = EXCLUDED.media_type,
              bytes = EXCLUDED.bytes,
              created_at = now()
        "#,
    )
    .bind(object_key)
    .bind(digest)
    .bind(media_type)
    .bind(bytes)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch the object at `object_key`, or `None` (caller → HTTP 404).
pub async fn get(pool: &DbPool, object_key: &str) -> AppResult<Option<ObjectRow>> {
    let rows = sqlx::query(
        r#"
        SELECT digest, media_type, bytes
        FROM noetl.object_store
        WHERE object_key = $1
        LIMIT 1
        "#,
    )
    .bind(object_key)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().next().map(|r| ObjectRow {
        digest: r.get::<String, _>("digest"),
        media_type: r.get::<String, _>("media_type"),
        bytes: r.get::<Vec<u8>, _>("bytes"),
    }))
}

/// Resolve an over-budget result's JSON-tier object by its canonical logical
/// coordinates (noetl/ai-meta#104 Phase C read path for the explicit
/// `artifact get` / `result_fetch` lazy-load surface).
///
/// The producer-staged / materialised result-tier object lives at the §7
/// physical key
/// `noetl/env=…/region=…/cell=…/shard=…/tenant=…/project=…/date=…/execution=<eid>/results/<step>/<frame>/<row>/<attempt>.json`.
/// The caller (`resolve_ref`) has the canonical logical coordinates
/// (`<eid>/<step>/<frame>/<row>/<attempt>`) from the worker-stamped
/// `reference.uri` but NOT the env/region/cell/shard/date placement prefix
/// (that needs the cell registry + the snowflake date partition). Rather than
/// reconstruct the full key — which would drag the cell-placement + arrow-decode
/// graph onto the lean control plane — anchor on the placement-independent
/// **suffix**: the logical tail is unique per `(execution, step, frame, row,
/// attempt)`, so a single suffix match resolves the object without knowing the
/// physical placement.
///
/// Scoped to the JSON tier (`.json`): a non-tabular over-budget result stores
/// its scrubbed `result.context` as JSON, byte-identical to what the legacy
/// `result_store` row held, so the returned body matches the legacy resolve
/// contract exactly. Tabular results tier as Arrow Feather and are resolved via
/// the worker's bulk-binding `resolve_by_urn` path (which owns the arrow decode
/// the control plane deliberately does not carry).
///
/// `logical_tail` is `<step>/<frame>/<row>/<attempt>`; LIKE metacharacters in it
/// are escaped so a step name with `%`/`_` cannot widen the match.
pub async fn get_result_tier_json(
    pool: &DbPool,
    execution_id: i64,
    logical_tail: &str,
) -> AppResult<Option<ObjectRow>> {
    // Escape LIKE metacharacters (\ % _) in the caller-supplied tail so the
    // match stays anchored to exactly this result's coordinates.
    let escaped_tail = logical_tail
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let pattern = format!("%/execution={execution_id}/results/{escaped_tail}.json");
    let rows = sqlx::query(
        r#"
        SELECT digest, media_type, bytes
        FROM noetl.object_store
        WHERE object_key LIKE $1 ESCAPE '\'
        ORDER BY created_at DESC
        LIMIT 1
        "#,
    )
    .bind(pattern)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().next().map(|r| ObjectRow {
        digest: r.get::<String, _>("digest"),
        media_type: r.get::<String, _>("media_type"),
        bytes: r.get::<Vec<u8>, _>("bytes"),
    }))
}

/// List object keys under `prefix` (most-recently-written first), capped at
/// `limit`. Backs the result-tier GC sweep ([noetl/ai-meta#104](https://github.com/noetl/ai-meta/issues/104)
/// Phase F) for the Postgres backend.
pub async fn list_keys(pool: &DbPool, prefix: &str, limit: i64) -> AppResult<Vec<String>> {
    let rows = sqlx::query(
        r#"
        SELECT object_key
        FROM noetl.object_store
        WHERE object_key LIKE $1 || '%'
        ORDER BY created_at DESC
        LIMIT $2
        "#,
    )
    .bind(prefix)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| r.get::<String, _>("object_key"))
        .collect())
}

/// Delete the object at `object_key`. Returns `true` if a row was removed
/// (idempotent — a missing key is `false`, not an error). Backs the result-tier
/// GC sweep ([noetl/ai-meta#104](https://github.com/noetl/ai-meta/issues/104) Phase F).
pub async fn delete(pool: &DbPool, object_key: &str) -> AppResult<bool> {
    let result = sqlx::query(
        r#"
        DELETE FROM noetl.object_store
        WHERE object_key = $1
        "#,
    )
    .bind(object_key)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}
