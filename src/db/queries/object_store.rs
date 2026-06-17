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
