//! `noetl.registry` queries — the versioned model / dataset / eval / release
//! registry that the SLM MLOps stages write to
//! ([noetl/ai-meta#146](https://github.com/noetl/ai-meta/issues/146), platform
//! foundation **G3**).
//!
//! The registry is a **typed catalog index with versioning + lineage on top of
//! the [noetl/ai-meta#104](https://github.com/noetl/ai-meta/issues/104) result
//! tier**. A registry entry records *what* a thing is (kind / name / version),
//! *where its bytes live* (`artifact_uri` — an object-store key the
//! [`crate::services::object_backend::ObjectBackend`] holds), and *how it was
//! produced* (`metadata` + `lineage`). The blob bytes themselves never live in
//! this table — they go through the existing object endpoint
//! (`PUT/GET /api/internal/objects/{*key}`), so GB-scale artifacts reuse the
//! #104 substrate and this table stays a slim, queryable index.
//!
//! ## Why a dedicated table (design fork, [noetl/ai-meta#146](https://github.com/noetl/ai-meta/issues/146))
//!
//! The RFC frames the registry as a "catalog resource kind". The two viable
//! shapes are (a) extend `noetl.catalog` + its `noetl.resource(kind)` FK with
//! `model`/`dataset`/`eval`/`release` rows, or (b) a separate additive
//! `noetl.registry` table. We pick **(b)** — the lower-risk fork: it does not
//! touch the catalog read/write hot path (load-bearing for playbook registration
//! and the prod planner), it carries registry-specific columns (version,
//! artifact ref, lineage) without bending the catalog schema, and it is fully
//! additive + flag-gated (`NOETL_REGISTRY_ENABLED`, default off) so default
//! deployments see no schema or route change. The entry is still modelled as a
//! typed catalog *concept* (the `registry://` URN resolves like a result URN);
//! only its storage is separate.
//!
//! All access is **server-mediated** ([data-access-boundary.md](https://github.com/noetl/ai-meta/blob/main/agents/rules/data-access-boundary.md)):
//! the table lives in the `noetl.*` schema, so workers / playbooks reach it only
//! through the service-account-gated `/api/internal/registry/*` routes.

use sqlx::Row;

use crate::db::DbPool;
use crate::error::AppResult;

/// One registered registry entry.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RegistryRow {
    /// Application-minted snowflake id (stable across retries — observability.md
    /// Principle 3).
    pub entry_id: i64,
    pub tenant: String,
    pub project: String,
    /// `model` | `dataset` | `eval` | `release`.
    pub kind: String,
    pub name: String,
    /// Monotonic per `(tenant, project, kind, name)`, assigned at insert.
    pub version: i32,
    /// Object-store key (an `ObjectBackend` key) where the artifact bytes live,
    /// or `None` for a metadata-only entry (e.g. an eval that points only at the
    /// models it scored).
    pub artifact_uri: Option<String>,
    /// SHA-256 hex digest of the artifact bytes, if known.
    pub artifact_digest: Option<String>,
    /// Artifact size in bytes, if known.
    pub artifact_bytes: Option<i64>,
    pub media_type: Option<String>,
    /// Free-form metrics / base-model / recipe metadata.
    pub metadata: serde_json::Value,
    /// Parent registry refs this entry derives from (dataset→model→eval lineage).
    pub lineage: serde_json::Value,
    /// Free-form tags for filtering.
    pub tags: serde_json::Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Idempotent table creation. Gated by the caller on `NOETL_REGISTRY_ENABLED`
/// so a default deployment never creates the table.
pub async fn ensure_table(pool: &DbPool) -> AppResult<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS noetl.registry (
            entry_id        BIGINT PRIMARY KEY,
            tenant          TEXT NOT NULL DEFAULT 'default',
            project         TEXT NOT NULL DEFAULT 'default',
            kind            TEXT NOT NULL,
            name            TEXT NOT NULL,
            version         INTEGER NOT NULL,
            artifact_uri    TEXT,
            artifact_digest TEXT,
            artifact_bytes  BIGINT,
            media_type      TEXT,
            metadata        JSONB NOT NULL DEFAULT '{}'::jsonb,
            lineage         JSONB NOT NULL DEFAULT '[]'::jsonb,
            tags            JSONB NOT NULL DEFAULT '[]'::jsonb,
            created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
            UNIQUE (tenant, project, kind, name, version)
        )
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS idx_registry_lookup
        ON noetl.registry (tenant, project, kind, name, version DESC)
        "#,
    )
    .execute(pool)
    .await?;
    Ok(())
}

/// Insert a new entry, assigning `version = max(existing for the
/// tenant/project/kind/name) + 1` atomically in the same statement. The
/// `UNIQUE (tenant, project, kind, name, version)` constraint rejects a
/// concurrent race (two registers computing the same next version); the caller
/// retries on conflict. Returns the assigned `(version, created_at)`.
///
/// `entry_id` is minted by the caller (snowflake) before this call.
#[allow(clippy::too_many_arguments)]
pub async fn insert_next_version(
    pool: &DbPool,
    entry_id: i64,
    tenant: &str,
    project: &str,
    kind: &str,
    name: &str,
    artifact_uri: Option<&str>,
    artifact_digest: Option<&str>,
    artifact_bytes: Option<i64>,
    media_type: Option<&str>,
    metadata: &serde_json::Value,
    lineage: &serde_json::Value,
    tags: &serde_json::Value,
) -> AppResult<(i32, chrono::DateTime<chrono::Utc>)> {
    let row = sqlx::query(
        r#"
        INSERT INTO noetl.registry
            (entry_id, tenant, project, kind, name, version,
             artifact_uri, artifact_digest, artifact_bytes, media_type,
             metadata, lineage, tags)
        SELECT $1, $2, $3, $4, $5,
               COALESCE(
                 (SELECT MAX(version) FROM noetl.registry
                  WHERE tenant = $2 AND project = $3 AND kind = $4 AND name = $5),
                 0) + 1,
               $6, $7, $8, $9, $10, $11, $12
        RETURNING version, created_at
        "#,
    )
    .bind(entry_id)
    .bind(tenant)
    .bind(project)
    .bind(kind)
    .bind(name)
    .bind(artifact_uri)
    .bind(artifact_digest)
    .bind(artifact_bytes)
    .bind(media_type)
    .bind(metadata)
    .bind(lineage)
    .bind(tags)
    .fetch_one(pool)
    .await?;
    Ok((
        row.get::<i32, _>("version"),
        row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
    ))
}

/// True if the error is the `UNIQUE (tenant, project, kind, name, version)`
/// violation — the version-race signal the service retries on.
pub fn is_version_conflict(e: &crate::error::AppError) -> bool {
    let msg = e.to_string();
    msg.contains("registry") && (msg.contains("duplicate key") || msg.contains("unique"))
}

fn row_to_entry(r: &sqlx::postgres::PgRow) -> RegistryRow {
    RegistryRow {
        entry_id: r.get::<i64, _>("entry_id"),
        tenant: r.get::<String, _>("tenant"),
        project: r.get::<String, _>("project"),
        kind: r.get::<String, _>("kind"),
        name: r.get::<String, _>("name"),
        version: r.get::<i32, _>("version"),
        artifact_uri: r.get::<Option<String>, _>("artifact_uri"),
        artifact_digest: r.get::<Option<String>, _>("artifact_digest"),
        artifact_bytes: r.get::<Option<i64>, _>("artifact_bytes"),
        media_type: r.get::<Option<String>, _>("media_type"),
        metadata: r.get::<serde_json::Value, _>("metadata"),
        lineage: r.get::<serde_json::Value, _>("lineage"),
        tags: r.get::<serde_json::Value, _>("tags"),
        created_at: r.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
    }
}

const SELECT_COLS: &str = "entry_id, tenant, project, kind, name, version, \
    artifact_uri, artifact_digest, artifact_bytes, media_type, metadata, lineage, tags, created_at";

/// Fetch one specific version.
pub async fn get_version(
    pool: &DbPool,
    tenant: &str,
    project: &str,
    kind: &str,
    name: &str,
    version: i32,
) -> AppResult<Option<RegistryRow>> {
    let sql = format!(
        "SELECT {SELECT_COLS} FROM noetl.registry \
         WHERE tenant = $1 AND project = $2 AND kind = $3 AND name = $4 AND version = $5 LIMIT 1"
    );
    let rows = sqlx::query(&sql)
        .bind(tenant)
        .bind(project)
        .bind(kind)
        .bind(name)
        .bind(version)
        .fetch_all(pool)
        .await?;
    Ok(rows.first().map(row_to_entry))
}

/// Fetch the latest (highest-version) entry for a name.
pub async fn get_latest(
    pool: &DbPool,
    tenant: &str,
    project: &str,
    kind: &str,
    name: &str,
) -> AppResult<Option<RegistryRow>> {
    let sql = format!(
        "SELECT {SELECT_COLS} FROM noetl.registry \
         WHERE tenant = $1 AND project = $2 AND kind = $3 AND name = $4 \
         ORDER BY version DESC LIMIT 1"
    );
    let rows = sqlx::query(&sql)
        .bind(tenant)
        .bind(project)
        .bind(kind)
        .bind(name)
        .fetch_all(pool)
        .await?;
    Ok(rows.first().map(row_to_entry))
}

/// List entries under a tenant/project, optionally filtered by `kind` and
/// `name`, newest-first (by `created_at`), capped at `limit`.
pub async fn list(
    pool: &DbPool,
    tenant: &str,
    project: &str,
    kind: Option<&str>,
    name: Option<&str>,
    limit: i64,
) -> AppResult<Vec<RegistryRow>> {
    // Bind the optional filters as nullable params and let SQL skip them when
    // NULL — keeps one prepared statement regardless of which filters are set.
    let sql = format!(
        "SELECT {SELECT_COLS} FROM noetl.registry \
         WHERE tenant = $1 AND project = $2 \
           AND ($3::text IS NULL OR kind = $3) \
           AND ($4::text IS NULL OR name = $4) \
         ORDER BY created_at DESC LIMIT $5"
    );
    let rows = sqlx::query(&sql)
        .bind(tenant)
        .bind(project)
        .bind(kind)
        .bind(name)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(row_to_entry).collect())
}
