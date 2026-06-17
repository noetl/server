//! `noetl.plugin_module` queries — the server-side plug-in module registry
//! (noetl/ai-meta#105 Round 4).
//!
//! This is the durable backing for the worker's `PluginSource`: the system
//! worker pool's wasmtime host fetches a compiled plug-in module by
//! `(path, version)` and verifies its `digest`, then caches it by
//! `(path, version, digest)`. A catalog version bump publishes a new row, so the
//! next worker claim fetches + compiles the new module — the hot-reload path.
//!
//! Per [the data-access boundary](https://github.com/noetl/noetl/blob/main/agents/rules/data-access-boundary.md)
//! plug-in modules are NoETL-owned data: workers reach them through the server
//! API, never the table directly. The module bytes are stored as `BYTEA` (wasm
//! or, during the hybrid phase, WAT the host compiles).
//!
//! Pattern mirrors `db::queries::result_store` — startup-time `CREATE TABLE IF
//! NOT EXISTS`, no out-of-band migration file.

use sqlx::Row;

use crate::db::DbPool;
use crate::error::AppResult;

/// A stored plug-in module.
#[derive(Debug, Clone)]
pub struct PluginModuleRow {
    /// SHA-256 hex digest of `bytes` — the integrity check the worker pins in
    /// its cache key.
    pub digest: String,
    /// Media type (`application/wasm`, or `text/wat` during the hybrid phase).
    pub media_type: String,
    /// The module bytes.
    pub bytes: Vec<u8>,
}

/// Idempotent table creation. Runs once at startup so the schema lands on first
/// boot without an out-of-band migration step. Owned end-to-end by noetl-server.
pub async fn ensure_table(pool: &DbPool) -> AppResult<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS noetl.plugin_module (
            path        TEXT NOT NULL,
            version     INTEGER NOT NULL,
            digest      TEXT NOT NULL,
            media_type  TEXT NOT NULL DEFAULT 'application/wasm',
            bytes       BYTEA NOT NULL,
            created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
            PRIMARY KEY (path, version)
        )
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Register (or replace) a module at `(path, version)`. A version is normally
/// immutable — the hot-reload model bumps the version rather than mutating a
/// row — but `ON CONFLICT DO UPDATE` keeps registration idempotent and allows
/// dev-time fixups; the `digest` always reflects the current bytes.
pub async fn upsert(
    pool: &DbPool,
    path: &str,
    version: i32,
    digest: &str,
    media_type: &str,
    bytes: &[u8],
) -> AppResult<()> {
    sqlx::query(
        r#"
        INSERT INTO noetl.plugin_module (path, version, digest, media_type, bytes)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (path, version) DO UPDATE
          SET digest = EXCLUDED.digest,
              media_type = EXCLUDED.media_type,
              bytes = EXCLUDED.bytes,
              created_at = now()
        "#,
    )
    .bind(path)
    .bind(version)
    .bind(digest)
    .bind(media_type)
    .bind(bytes)
    .execute(pool)
    .await?;
    Ok(())
}

/// Fetch the module at `(path, version)`, or `None` (caller → HTTP 404).
pub async fn get(pool: &DbPool, path: &str, version: i32) -> AppResult<Option<PluginModuleRow>> {
    let rows = sqlx::query(
        r#"
        SELECT digest, media_type, bytes
        FROM noetl.plugin_module
        WHERE path = $1 AND version = $2
        LIMIT 1
        "#,
    )
    .bind(path)
    .bind(version)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().next().map(|r| PluginModuleRow {
        digest: r.get::<String, _>("digest"),
        media_type: r.get::<String, _>("media_type"),
        bytes: r.get::<Vec<u8>, _>("bytes"),
    }))
}
