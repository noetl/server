//! Content-addressed catalog and execution pinning, Postgres side
//! (noetl/ai-meta#548, RFC `docs/rfc/distributed-playbook-catalog.md`).
//!
//! The pure rules live in `noetl_orchestrate_core::catalog`. This module runs
//! the one [`Lookup`] a resolution plans, and owns four side tables. It never
//! alters `noetl.catalog`:
//!
//! | table | holds |
//! |---|---|
//! | `noetl.catalog_blob` | one body per digest |
//! | `noetl.catalog_digest` | `catalog_id → digest` (the index the digest lookup needs) |
//! | `noetl.catalog_source` | where a digest can be fetched from (a set per digest) |
//! | `noetl.execution_catalog_pin` | what each execution resolved to, and the root's watermark |
//!
//! # Flags (strict `"true"`, read once per process)
//!
//! - `NOETL_CATALOG_CAS`: digest on register, dedup, and the
//!   `/api/catalog/{resolve,blob,sources,pin}` routes.
//! - `NOETL_CATALOG_PINNING`: execute writes a pin, and sub-playbooks resolve
//!   against it. Requires `NOETL_CATALOG_CAS`.
//! - `NOETL_CATALOG_HOT_RELOAD`: allows `@latest` inside a pinned tree for paths the
//!   root lists in `metadata.catalog.hot_reload`. Phase 3 wires the loop test.
//!
//! # ⚠ The tables may not exist, and that must never break execute
//!
//! A predicate on a missing column once returned 500 for every execute-by-path
//! in production (`queries::catalog::ensure_archived_column`). So
//! [`ensure_schema`] creates the tables best-effort, then *detects* them through
//! `information_schema`. Only [`ready`] decides whether any CAS path runs. With
//! the tables absent, every flag behaves as off and one ERROR line says why.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use noetl_orchestrate_core::catalog::{
    self as core, Entry, Lookup, Pin, PinPolicy, Reason, Resolution,
};
use serde::{Deserialize, Serialize};

use crate::db::DbPool;
use crate::error::{AppError, AppResult};

pub const CAS_ENV: &str = "NOETL_CATALOG_CAS";
pub const PINNING_ENV: &str = "NOETL_CATALOG_PINNING";
pub const HOT_RELOAD_ENV: &str = "NOETL_CATALOG_HOT_RELOAD";

/// The three flags, parsed from a lookup so tests need no process env.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CasFlags {
    pub cas: bool,
    pub pinning: bool,
    pub hot_reload: bool,
}

impl CasFlags {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let on = |k: &str| get(k).is_some_and(|v| v.trim() == "true");
        CasFlags {
            cas: on(CAS_ENV),
            pinning: on(PINNING_ENV),
            hot_reload: on(HOT_RELOAD_ENV),
        }
    }

    pub fn current() -> Self {
        static FLAGS: OnceLock<CasFlags> = OnceLock::new();
        *FLAGS.get_or_init(|| {
            let f = CasFlags::from_lookup(|k| std::env::var(k).ok());
            if f.pinning && !f.cas {
                tracing::error!(
                    "{PINNING_ENV}=true without {CAS_ENV}=true: pinning needs the digest tables and stays OFF"
                );
            }
            f
        })
    }
}

static SCHEMA_READY: AtomicBool = AtomicBool::new(false);

/// True only when `NOETL_CATALOG_CAS` is on and all four tables were found.
pub fn ready() -> bool {
    CasFlags::current().cas && SCHEMA_READY.load(Ordering::Relaxed)
}

/// True when executions should be pinned on this process.
pub fn pinning_active() -> bool {
    ready() && CasFlags::current().pinning
}

pub fn hot_reload_enabled() -> bool {
    CasFlags::current().hot_reload
}

const TABLES: [&str; 4] = [
    "catalog_blob",
    "catalog_digest",
    "catalog_source",
    "execution_catalog_pin",
];

const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS noetl.catalog_blob (
        digest TEXT PRIMARY KEY,
        content TEXT NOT NULL,
        bytes INTEGER NOT NULL,
        first_catalog_id BIGINT,
        created_at TIMESTAMPTZ NOT NULL DEFAULT now())",
    "CREATE TABLE IF NOT EXISTS noetl.catalog_digest (
        catalog_id BIGINT PRIMARY KEY,
        digest TEXT NOT NULL,
        created_at TIMESTAMPTZ NOT NULL DEFAULT now())",
    "CREATE INDEX IF NOT EXISTS catalog_digest_digest_idx ON noetl.catalog_digest (digest)",
    "CREATE TABLE IF NOT EXISTS noetl.catalog_source (
        digest TEXT NOT NULL,
        uri TEXT NOT NULL,
        origin TEXT NOT NULL,
        added_at TIMESTAMPTZ NOT NULL DEFAULT now(),
        verified_at TIMESTAMPTZ NULL,
        PRIMARY KEY (digest, uri))",
    "CREATE TABLE IF NOT EXISTS noetl.execution_catalog_pin (
        execution_id BIGINT PRIMARY KEY,
        root_execution_id BIGINT NOT NULL,
        parent_execution_id BIGINT NULL,
        catalog_id BIGINT NOT NULL,
        path TEXT NOT NULL,
        version INTEGER NOT NULL,
        digest TEXT NOT NULL,
        watermark BIGINT NOT NULL,
        pinned_at_ms BIGINT NOT NULL,
        reason TEXT NOT NULL,
        policy JSONB NOT NULL DEFAULT '{}'::jsonb,
        created_at TIMESTAMPTZ NOT NULL DEFAULT now())",
    "CREATE INDEX IF NOT EXISTS execution_catalog_pin_root_idx ON noetl.execution_catalog_pin (root_execution_id)",
];

/// Best-effort create, then detect. A no-op unless `NOETL_CATALOG_CAS=true`.
pub async fn ensure_schema(pool: &DbPool) {
    if !CasFlags::current().cas {
        return;
    }
    for stmt in DDL {
        if let Err(e) = sqlx::query(stmt).execute(pool).await {
            tracing::warn!(error = %e, "catalog CAS DDL statement failed (detection decides)");
        }
    }
    let found: Result<Vec<(String,)>, _> = sqlx::query_as(
        "SELECT table_name::text FROM information_schema.tables \
         WHERE table_schema = 'noetl' AND table_name = ANY($1)",
    )
    .bind(TABLES.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    .fetch_all(pool)
    .await;
    match found {
        Ok(rows) if rows.len() == TABLES.len() => {
            SCHEMA_READY.store(true, Ordering::Relaxed);
            tracing::info!(
                pinning = CasFlags::current().pinning,
                "catalog CAS tables present; content addressing ON"
            );
        }
        Ok(rows) => tracing::error!(
            found = ?rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            "catalog CAS tables missing; {CAS_ENV} and {PINNING_ENV} behave as OFF"
        ),
        Err(e) => {
            tracing::error!(error = %e, "catalog CAS table detection failed; flags behave as OFF")
        }
    }
}

/// Fill `catalog_digest` and `catalog_blob` for rows registered before the
/// flag was on. Bounded and best-effort; [`digest_for`] also fills lazily.
pub async fn backfill_digests(pool: &DbPool, limit: i64) -> usize {
    if !ready() {
        return 0;
    }
    let rows: Vec<(i64, Option<String>)> = match sqlx::query_as(
        "SELECT c.catalog_id, c.content FROM noetl.catalog c \
         WHERE NOT EXISTS (SELECT 1 FROM noetl.catalog_digest d WHERE d.catalog_id = c.catalog_id) \
         ORDER BY c.catalog_id LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "catalog digest backfill query failed");
            return 0;
        }
    };
    let mut done = 0;
    for (id, content) in rows {
        let Some(content) = content else { continue };
        let Ok(digest) = core::playbook_digest(&content) else {
            tracing::warn!(
                catalog_id = id,
                "catalog row has no digest (YAML with no JSON form); skipped"
            );
            continue;
        };
        if store_digest(pool, id, &digest, &content).await.is_ok() {
            done += 1;
        }
    }
    done
}

/// One catalog row with what execute and the routes need.
#[derive(Debug, Clone)]
pub struct CasRow {
    pub catalog_id: i64,
    pub path: String,
    pub kind: String,
    pub version: i16,
    pub content: Option<String>,
    pub payload: Option<serde_json::Value>,
}

const ROW_COLS: &str = "c.catalog_id, c.path, c.kind, c.version, c.content, c.payload";

type RowTuple = (
    i64,
    String,
    String,
    i16,
    Option<String>,
    Option<serde_json::Value>,
);

fn to_row(t: RowTuple) -> CasRow {
    CasRow {
        catalog_id: t.0,
        path: t.1,
        kind: t.2,
        version: t.3,
        content: t.4,
        payload: t.5,
    }
}

/// Run the planned lookup. Mirrors `core::MemoryCatalog::lookup` case for case.
pub async fn lookup(pool: &DbPool, l: &Lookup) -> AppResult<Option<CasRow>> {
    let archived_col = crate::db::queries::catalog::archived_column_present();
    let row: Option<RowTuple> = match l {
        Lookup::Exact { path, version } => {
            let v = i16::try_from(*version)
                .map_err(|_| AppError::Validation(format!("version {version} is beyond the catalog's smallint range")))?;
            sqlx::query_as(&format!("SELECT {ROW_COLS} FROM noetl.catalog c WHERE c.path = $1 AND c.version = $2"))
                .bind(path)
                .bind(v)
                .fetch_optional(pool)
                .await?
        }
        Lookup::Digest { digest, path } => {
            sqlx::query_as(&format!(
                "SELECT {ROW_COLS} FROM noetl.catalog c \
                 JOIN noetl.catalog_digest d ON d.catalog_id = c.catalog_id \
                 WHERE d.digest = $1 AND ($2::text IS NULL OR c.path = $2) \
                 ORDER BY c.version DESC, c.catalog_id DESC LIMIT 1"
            ))
            .bind(digest)
            .bind(path)
            .fetch_optional(pool)
            .await?
        }
        Lookup::Pinned { path, watermark, pinned_at_ms } => {
            // A version archived after the root started stays visible to it.
            let archived = if archived_col {
                " AND (c.archived_at IS NULL OR c.archived_at > to_timestamp($3::double precision / 1000.0))"
            } else {
                " AND $3::bigint IS NOT NULL"
            };
            sqlx::query_as(&format!(
                "SELECT {ROW_COLS} FROM noetl.catalog c \
                 WHERE c.path = $1 AND c.catalog_id <= $2{archived} \
                 ORDER BY c.version DESC LIMIT 1"
            ))
            .bind(path)
            .bind(watermark)
            .bind(*pinned_at_ms)
            .fetch_optional(pool)
            .await?
        }
        Lookup::Latest { path } => {
            let archived = if archived_col { " AND c.archived_at IS NULL" } else { "" };
            sqlx::query_as(&format!(
                "SELECT {ROW_COLS} FROM noetl.catalog c WHERE c.path = $1{archived} ORDER BY c.version DESC LIMIT 1"
            ))
            .bind(path)
            .fetch_optional(pool)
            .await?
        }
        Lookup::Source { uri, .. } => {
            return Err(AppError::Validation(format!(
                "'{uri}' is a source reference; fetching sources is Phase 2 (NOETL_CATALOG_URI_FETCH) and not on this server"
            )))
        }
    };
    Ok(row.map(to_row))
}

pub async fn row_by_id(pool: &DbPool, catalog_id: i64) -> AppResult<Option<CasRow>> {
    let r: Option<RowTuple> = sqlx::query_as(&format!(
        "SELECT {ROW_COLS} FROM noetl.catalog c WHERE c.catalog_id = $1"
    ))
    .bind(catalog_id)
    .fetch_optional(pool)
    .await?;
    Ok(r.map(to_row))
}

fn content_of(row: &CasRow) -> AppResult<String> {
    if let Some(c) = &row.content {
        return Ok(c.clone());
    }
    match &row.payload {
        Some(p) => serde_yaml::to_string(p).map_err(|e| AppError::Internal(e.to_string())),
        None => Err(AppError::Internal(format!(
            "catalog row {} has no content",
            row.catalog_id
        ))),
    }
}

async fn store_digest(
    pool: &DbPool,
    catalog_id: i64,
    digest: &str,
    content: &str,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO noetl.catalog_blob (digest, content, bytes, first_catalog_id) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (digest) DO NOTHING",
    )
    .bind(digest)
    .bind(content)
    .bind(content.len() as i32)
    .bind(catalog_id)
    .execute(pool)
    .await?;
    sqlx::query("INSERT INTO noetl.catalog_digest (catalog_id, digest) VALUES ($1, $2) ON CONFLICT (catalog_id) DO NOTHING")
        .bind(catalog_id)
        .bind(digest)
        .execute(pool)
        .await?;
    Ok(())
}

/// The digest of a row: indexed if known, otherwise computed and indexed now.
pub async fn digest_for(pool: &DbPool, row: &CasRow) -> AppResult<String> {
    let known: Option<(String,)> =
        sqlx::query_as("SELECT digest FROM noetl.catalog_digest WHERE catalog_id = $1")
            .bind(row.catalog_id)
            .fetch_optional(pool)
            .await?;
    if let Some((d,)) = known {
        return Ok(d);
    }
    let content = content_of(row)?;
    let digest =
        core::playbook_digest(&content).map_err(|e| AppError::Validation(e.to_string()))?;
    store_digest(pool, row.catalog_id, &digest, &content).await?;
    Ok(digest)
}

/// Where this catalog's own copy of a row is: `noetl://local/<path>@<version>`.
/// Phase 2 publishes `local` as this server's instance id.
pub fn local_source(path: &str, version: i16) -> String {
    format!("noetl://local/{path}@{version}")
}

pub async fn add_source(
    pool: &DbPool,
    digest: &str,
    uri: &str,
    origin: &str,
    verified: bool,
) -> AppResult<bool> {
    let r = sqlx::query(
        "INSERT INTO noetl.catalog_source (digest, uri, origin, verified_at) \
         VALUES ($1, $2, $3, CASE WHEN $4 THEN now() ELSE NULL END) \
         ON CONFLICT (digest, uri) DO UPDATE SET verified_at = COALESCE(EXCLUDED.verified_at, noetl.catalog_source.verified_at)",
    )
    .bind(digest)
    .bind(uri)
    .bind(origin)
    .bind(verified)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() > 0)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceRow {
    pub uri: String,
    pub origin: String,
    pub verified: bool,
}

pub async fn sources(pool: &DbPool, digest: &str) -> AppResult<Vec<SourceRow>> {
    let rows: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT uri, origin, verified_at IS NOT NULL FROM noetl.catalog_source WHERE digest = $1 ORDER BY added_at, uri",
    )
    .bind(digest)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(uri, origin, verified)| SourceRow {
            uri,
            origin,
            verified,
        })
        .collect())
}

pub async fn blob(pool: &DbPool, digest: &str) -> AppResult<Option<String>> {
    let r: Option<(String,)> =
        sqlx::query_as("SELECT content FROM noetl.catalog_blob WHERE digest = $1")
            .bind(digest)
            .fetch_optional(pool)
            .await?;
    Ok(r.map(|t| t.0))
}

// ---------------------------------------------------------------------------
// Register: digest + dedup
// ---------------------------------------------------------------------------

/// The latest non-archived row of `path` with its digest, for the dedup check.
pub async fn latest_with_digest(pool: &DbPool, path: &str) -> AppResult<Option<(CasRow, String)>> {
    let Some(row) = lookup(
        pool,
        &Lookup::Latest {
            path: path.to_string(),
        },
    )
    .await?
    else {
        return Ok(None);
    };
    let digest = digest_for(pool, &row).await?;
    Ok(Some((row, digest)))
}

/// Record a freshly inserted row: body, digest index, its own source, and an
/// optional declared source.
pub async fn record_registered(
    pool: &DbPool,
    catalog_id: i64,
    path: &str,
    version: i16,
    content: &str,
    digest: &str,
    declared_source: Option<&str>,
) -> AppResult<()> {
    store_digest(pool, catalog_id, digest, content).await?;
    add_source(
        pool,
        digest,
        &local_source(path, version),
        "registered",
        true,
    )
    .await?;
    if let Some(uri) = declared_source {
        add_source(pool, digest, uri, "declared", false).await?;
    }
    Ok(())
}

/// Validate a declared source: it must parse as a source URI, and a
/// `#sha256:` fragment, if present, must match the body being registered.
pub fn check_declared_source(uri: &str, digest: &str) -> AppResult<String> {
    match core::PlaybookRef::parse(uri) {
        Ok(core::PlaybookRef::Source { uri, expect, .. }) => {
            if let Some(e) = expect {
                if e != digest {
                    return Err(AppError::Validation(format!(
                        "source '{uri}' declares {e}, but the registered body is {digest}"
                    )));
                }
            }
            Ok(uri)
        }
        Ok(_) => Err(AppError::Validation(format!(
            "source '{uri}' is not a URI (gs://, s3://, https://, ...)"
        ))),
        Err(e) => Err(AppError::Validation(e.to_string())),
    }
}

// ---------------------------------------------------------------------------
// Pins
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PinRow {
    pub execution_id: i64,
    pub root_execution_id: i64,
    pub parent_execution_id: Option<i64>,
    pub catalog_id: i64,
    pub path: String,
    pub version: i32,
    pub digest: String,
    pub watermark: i64,
    pub pinned_at_ms: i64,
    pub reason: String,
    pub policy: PinPolicy,
}

impl PinRow {
    /// The part a child inherits.
    pub fn inherited(&self) -> Pin {
        Pin {
            root_execution_id: self.root_execution_id,
            watermark: self.watermark,
            pinned_at_ms: self.pinned_at_ms,
            policy: self.policy.clone(),
        }
    }

    /// The `/api/catalog/pin/{id}` body: ids as strings, like the rest of the API
    /// (snowflake ids exceed 2^53 and would be rounded by a JSON-number reader).
    pub fn api_json(&self) -> serde_json::Value {
        let mut v = self.event_json();
        if let serde_json::Value::Object(m) = &mut v {
            m.insert("execution_id".into(), self.execution_id.to_string().into());
            m.insert(
                "parent_execution_id".into(),
                self.parent_execution_id.map(|p| p.to_string()).into(),
            );
            m.insert("path".into(), self.path.clone().into());
        }
        v
    }

    /// The `catalog_pin` object written into `playbook_started`.
    pub fn event_json(&self) -> serde_json::Value {
        serde_json::json!({
            "digest": self.digest,
            "version": self.version,
            "catalog_id": self.catalog_id.to_string(),
            "root_execution_id": self.root_execution_id.to_string(),
            "watermark": self.watermark.to_string(),
            "pinned_at_ms": self.pinned_at_ms,
            "reason": self.reason,
            "hot_reload": self.policy.hot_reload,
        })
    }
}

pub async fn get_pin(pool: &DbPool, execution_id: i64) -> AppResult<Option<PinRow>> {
    type PinTuple = (
        i64,
        i64,
        Option<i64>,
        i64,
        String,
        i32,
        String,
        i64,
        i64,
        String,
        serde_json::Value,
    );
    let r: Option<PinTuple> =
        sqlx::query_as(
            "SELECT execution_id, root_execution_id, parent_execution_id, catalog_id, path, version, digest, \
             watermark, pinned_at_ms, reason, policy FROM noetl.execution_catalog_pin WHERE execution_id = $1",
        )
        .bind(execution_id)
        .fetch_optional(pool)
        .await?;
    Ok(r.map(|t| PinRow {
        execution_id: t.0,
        root_execution_id: t.1,
        parent_execution_id: t.2,
        catalog_id: t.3,
        path: t.4,
        version: t.5,
        digest: t.6,
        watermark: t.7,
        pinned_at_ms: t.8,
        reason: t.9,
        policy: serde_json::from_value(t.10).unwrap_or_default(),
    }))
}

pub async fn insert_pin(pool: &DbPool, p: &PinRow) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO noetl.execution_catalog_pin (execution_id, root_execution_id, parent_execution_id, catalog_id, \
         path, version, digest, watermark, pinned_at_ms, reason, policy) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
    )
    .bind(p.execution_id)
    .bind(p.root_execution_id)
    .bind(p.parent_execution_id)
    .bind(p.catalog_id)
    .bind(&p.path)
    .bind(p.version)
    .bind(&p.digest)
    .bind(p.watermark)
    .bind(p.pinned_at_ms)
    .bind(&p.reason)
    .bind(serde_json::to_value(&p.policy).unwrap_or_default())
    .execute(pool)
    .await?;
    Ok(())
}

/// `max(catalog_id)` and the database clock, read together: a new root's pin.
pub async fn watermark_now(pool: &DbPool) -> AppResult<(i64, i64)> {
    let (w, ms): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE((SELECT MAX(catalog_id) FROM noetl.catalog), 0)::bigint, \
         (EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::bigint",
    )
    .fetch_one(pool)
    .await?;
    Ok((w, ms))
}

/// The outcome of resolving an execute request under pinning, before the pin
/// is written (the root's policy comes from the parsed playbook).
#[derive(Debug, Clone)]
pub struct PendingPin {
    pub row: CasRow,
    pub resolution: Resolution,
    /// The parent's pin, when this is a child of a pinned execution.
    pub inherited: Option<Pin>,
    /// A root's fresh watermark and clock.
    pub fresh: Option<(i64, i64)>,
}

impl PendingPin {
    /// Build the row to insert. The root's policy is read from its playbook; a
    /// child copies its parent's and can never widen it.
    pub fn to_row(
        &self,
        execution_id: i64,
        parent_execution_id: Option<i64>,
        playbook_yaml: &str,
    ) -> PinRow {
        let (root, watermark, pinned_at_ms, policy) = match (&self.inherited, self.fresh) {
            (Some(p), _) => (
                p.root_execution_id,
                p.watermark,
                p.pinned_at_ms,
                p.policy.clone(),
            ),
            (None, Some((w, ms))) => {
                let policy = core::playbook_value(playbook_yaml)
                    .map(|v| PinPolicy::from_playbook(&v))
                    .unwrap_or_default();
                (execution_id, w.max(self.row.catalog_id), ms, policy)
            }
            (None, None) => (execution_id, self.row.catalog_id, 0, PinPolicy::default()),
        };
        PinRow {
            execution_id,
            root_execution_id: root,
            parent_execution_id,
            catalog_id: self.resolution.entry.catalog_id,
            path: self.resolution.entry.path.clone(),
            version: self.resolution.entry.version,
            digest: self.resolution.entry.digest.clone(),
            watermark,
            pinned_at_ms,
            reason: self.resolution.reason.as_str().to_string(),
            policy,
        }
    }
}

/// Resolve an execute request under pinning.
///
/// `parent_execution_id` with a pin → the child resolves inside that pin.
/// A parent without a pin (it started before the flag) → this is a new root.
pub async fn resolve_execute(
    pool: &DbPool,
    path: Option<&str>,
    catalog_id: Option<i64>,
    version: Option<&serde_json::Value>,
    digest: Option<&str>,
    parent_execution_id: Option<i64>,
) -> AppResult<PendingPin> {
    let inherited = match parent_execution_id {
        Some(p) => get_pin(pool, p).await?.map(|r| r.inherited()),
        None => None,
    };
    let fresh = if inherited.is_none() {
        Some(watermark_now(pool).await?)
    } else {
        None
    };

    let (row, reason) = if let Some(id) = catalog_id {
        let row = row_by_id(pool, id)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("Catalog entry not found: {id}")))?;
        (row, Reason::Exact)
    } else {
        let path = path.ok_or_else(|| {
            AppError::Validation("Either path or catalog_id must be provided".into())
        })?;
        let r = core::PlaybookRef::from_request(path, version, digest)
            .map_err(|e| AppError::Validation(e.to_string()))?;
        let (l, reason) = core::plan(&r, inherited.as_ref(), hot_reload_enabled())
            .map_err(|e| AppError::Validation(e.to_string()))?;
        // Discovery (Phase 2): a source reference is always fetched; a name or
        // digest missing locally is located on peers, but ONLY for a root. A
        // pinned child that misses locally fails: importing for it would hand it
        // a row its root never saw, which is the drift pinning exists to stop.
        let discovery = crate::services::catalog_peer::runtime();
        let found = if matches!(l, Lookup::Source { .. }) {
            None
        } else {
            lookup(pool, &l).await?
        };
        match (found, discovery) {
            (Some(row), _) => (row, reason),
            (None, Some(rt)) if matches!(l, Lookup::Source { .. }) || inherited.is_none() => {
                let loc = rt
                    .locate(&crate::services::catalog_peer::ref_string(&r))
                    .await?;
                let id: i64 = loc
                    .catalog_id
                    .parse()
                    .map_err(|_| AppError::Internal("bad catalog id".into()))?;
                let row = row_by_id(pool, id).await?.ok_or_else(|| {
                    AppError::NotFound(format!("located {} but row {id} is gone", loc.path))
                })?;
                let reason = if loc.found_at == "local" {
                    reason
                } else {
                    Reason::Import
                };
                (row, reason)
            }
            (None, _) => {
                return Err(AppError::NotFound(format!(
                    "Playbook not found: {}",
                    core::describe(&l)
                )))
            }
        }
    };
    let digest_now = digest_for(pool, &row).await?;
    if let Some(d) = digest {
        if d != digest_now {
            return Err(AppError::Validation(format!(
                "digest mismatch: asked {d}, catalog row has {digest_now}"
            )));
        }
    }
    let entry = Entry {
        catalog_id: row.catalog_id,
        path: row.path.clone(),
        version: row.version as i32,
        digest: digest_now,
    };
    Ok(PendingPin {
        row,
        resolution: Resolution { entry, reason },
        inherited,
        fresh,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn flags(pairs: &[(&str, &str)]) -> CasFlags {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        CasFlags::from_lookup(|k| m.get(k).cloned())
    }

    #[test]
    fn flags_are_strict_true() {
        assert_eq!(flags(&[]), CasFlags::default());
        assert!(!flags(&[(CAS_ENV, "1")]).cas);
        assert!(!flags(&[(CAS_ENV, "TRUE")]).cas);
        assert!(flags(&[(CAS_ENV, " true ")]).cas);
        let all = flags(&[
            (CAS_ENV, "true"),
            (PINNING_ENV, "true"),
            (HOT_RELOAD_ENV, "true"),
        ]);
        assert!(all.cas && all.pinning && all.hot_reload);
    }

    #[test]
    fn declared_sources_must_be_uris_and_agree_with_the_body() {
        let d = format!("sha256:{}", "a".repeat(64));
        let other = format!("sha256:{}", "b".repeat(64));
        assert_eq!(
            check_declared_source("gs://b/k.yaml", &d).unwrap(),
            "gs://b/k.yaml"
        );
        assert_eq!(
            check_declared_source(&format!("s3://b/k#{d}"), &d).unwrap(),
            "s3://b/k"
        );
        assert!(check_declared_source(&format!("s3://b/k#{other}"), &d).is_err());
        assert!(check_declared_source("a/b@3", &d).is_err());
        assert!(check_declared_source("s3://user:pw@b/k", &d).is_err());
    }

    fn pending(inherited: Option<Pin>, fresh: Option<(i64, i64)>, reason: Reason) -> PendingPin {
        let row = CasRow {
            catalog_id: 50,
            path: "p".into(),
            kind: "playbook".into(),
            version: 2,
            content: None,
            payload: None,
        };
        let entry = Entry {
            catalog_id: 50,
            path: "p".into(),
            version: 2,
            digest: "sha256:x".into(),
        };
        PendingPin {
            row,
            resolution: Resolution { entry, reason },
            inherited,
            fresh,
        }
    }

    #[test]
    fn a_root_takes_its_policy_from_its_playbook_and_a_child_copies_its_parent() {
        let yaml = "metadata:\n  name: r\n  catalog:\n    hot_reload: [\"loop/*\"]\n";
        let root = pending(None, Some((40, 1_000)), Reason::Latest).to_row(7, None, yaml);
        assert_eq!(
            (root.root_execution_id, root.watermark, root.pinned_at_ms),
            (7, 50, 1_000)
        );
        assert_eq!(root.policy.hot_reload, vec!["loop/*"]);
        // A child whose own playbook asks for more cannot widen the parent's policy.
        let child = pending(Some(root.inherited()), None, Reason::Pinned).to_row(
            8,
            Some(7),
            "metadata:\n  name: c\n  catalog:\n    hot_reload: [\"*\"]\n",
        );
        assert_eq!(
            (
                child.root_execution_id,
                child.watermark,
                child.reason.as_str()
            ),
            (7, 50, "pinned")
        );
        assert_eq!(child.policy.hot_reload, vec!["loop/*"]);
        assert_eq!(child.event_json()["root_execution_id"], "7");
    }
}
