//! Cross-instance execution: "run it on B" and sub-playbook import
//! (noetl/ai-meta#548 follow-up, RFC `docs/rfc/distributed-playbook-catalog.md` §12).
//!
//! A playbook on instance A names a playbook that instance B holds, as
//! `noetl://<B>/<path>[@v][#sha256:…]` or under its import name
//! `peer/<B>/<path>`. There are two modes, chosen per step (`remote:` on a
//! `kind: playbook` tool) or by `NOETL_CATALOG_REMOTE_MODE`:
//!
//! - **import** (the default): B's body is verified against its digest,
//!   imported under `peer/<B>/<path>` (Phase 2's `locate`), and run on A's
//!   workers. Children of an imported playbook resolve on its home instance:
//!   a bare `x/y` called from `peer/<B>/…` means `peer/<B>/x/y`.
//! - **delegate**: A asks B, with a signed `catalog.execute` envelope, to run
//!   that exact digest on B's own workers. A hands its worker a call id whose
//!   status and detail A proxies from B (`catalog.execution`), so the
//!   `kind: playbook` tool's `return_result` works unchanged.
//!
//! # The invariant: one root, one answer per remote reference
//!
//! Phase 1's pin bounds local lookups by the root's `catalog_id` watermark. A
//! remote reference has no place under that watermark: B's catalog has its own
//! ids, and an import lands *after* the root started. So a pinned tree records
//! what each remote reference resolved to the first time
//! (`noetl.execution_remote_pin`, keyed by root and reference) and every later
//! call in the same tree gets that digest, in both modes, even if B registers
//! a new version in between.
//!
//! On B, every delegated execution that comes from the same A root shares one
//! pin (`noetl.catalog_remote_origin`), so B's own sub-playbooks are held at
//! one watermark across all of A's calls, as if they were one tree on B.
//!
//! # Flags (strict `"true"`; all need `NOETL_CATALOG_CAS` + `NOETL_CATALOG_PINNING`)
//!
//! - `NOETL_CATALOG_REMOTE_CALLS`: the caller side. Remote pins, home-instance
//!   resolution for imports, `remote: delegate`, and the
//!   `/api/catalog/remote/calls/{id}` routes. Delegation also needs
//!   `NOETL_CATALOG_DISCOVERY` (the peer key and the registry).
//! - `NOETL_CATALOG_REMOTE_MODE`: `import` (default) or `delegate`, for a
//!   remote reference whose step does not say.
//! - `NOETL_CATALOG_REMOTE_SERVE`: the serving side, the signed
//!   `/api/catalog/peer/execute` and `/api/catalog/peer/execution` routes.
//! - `NOETL_CATALOG_REMOTE_SERVE_ALLOW`: comma-separated path globs B will run
//!   for a peer. **Fail closed**: with none, nothing is run.
//!
//! With every flag off nothing here runs and `/api/execute` behaves exactly as
//! before: a `remote:` field is ignored unless it asks for `delegate`, which is
//! refused by name.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use noetl_orchestrate_core::catalog::{self as core, Pin, PinPolicy, PlaybookRef, VersionSel};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::db::DbPool;
use crate::error::{AppError, AppResult};
use crate::instances::identity::SignedEnvelope;
use crate::services::catalog_cas as cas;
use crate::services::catalog_peer::{self as peer, PeerRuntime};
use crate::services::ExecutionService;
use crate::state::AppState;

pub const CALLS_ENV: &str = noetl_orchestrate_core::dsl::CATALOG_REMOTE_CALLS_ENV;
pub const MODE_ENV: &str = "NOETL_CATALOG_REMOTE_MODE";
pub const SERVE_ENV: &str = noetl_orchestrate_core::dsl::CATALOG_REMOTE_SERVE_ENV;
pub const SERVE_ALLOW_ENV: &str = noetl_orchestrate_core::dsl::CATALOG_REMOTE_SERVE_ALLOW_ENV;

pub const PURPOSE_EXECUTE: &str = "catalog.execute";
pub const PURPOSE_EXECUTION: &str = "catalog.execution";

/// How a remote reference is run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Import,
    Delegate,
}

impl Mode {
    pub fn parse(s: &str) -> Result<Mode, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "import" => Ok(Mode::Import),
            "delegate" => Ok(Mode::Delegate),
            other => Err(format!(
                "remote: '{other}' is not a mode (use 'import' or 'delegate')"
            )),
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Import => "import",
            Mode::Delegate => "delegate",
        }
    }
}

/// Parsed flags. Pure, so tests need no environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFlags {
    pub calls: bool,
    pub default_mode: Mode,
    pub serve: bool,
    pub serve_allow: Vec<String>,
}

impl Default for RemoteFlags {
    fn default() -> Self {
        RemoteFlags {
            calls: false,
            default_mode: Mode::Import,
            serve: false,
            serve_allow: Vec::new(),
        }
    }
}

impl RemoteFlags {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let on = |k: &str| get(k).is_some_and(|v| v.trim() == "true");
        let default_mode = match get(MODE_ENV).filter(|s| !s.trim().is_empty()) {
            None => Mode::Import,
            Some(s) => Mode::parse(&s).unwrap_or_else(|e| {
                tracing::error!("{MODE_ENV}: {e}; using import");
                Mode::Import
            }),
        };
        RemoteFlags {
            calls: on(CALLS_ENV),
            default_mode,
            serve: on(SERVE_ENV),
            serve_allow: get(SERVE_ALLOW_ENV)
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
        }
    }

    pub fn current() -> &'static RemoteFlags {
        static F: OnceLock<RemoteFlags> = OnceLock::new();
        F.get_or_init(|| RemoteFlags::from_lookup(|k| std::env::var(k).ok()))
    }

    /// Fail closed: a path is served to a peer only with a matching glob.
    pub fn serves(&self, path: &str) -> bool {
        self.serve && self.serve_allow.iter().any(|g| core::glob_match(g, path))
    }
}

// ---------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------

static SCHEMA_READY: AtomicBool = AtomicBool::new(false);

/// The remote tables exist and the caller or serving flag is on.
pub fn ready() -> bool {
    SCHEMA_READY.load(Ordering::Relaxed) && cas::pinning_active()
}

/// The caller side is active (remote pins, home-instance resolution, delegate).
pub fn calls_active() -> bool {
    RemoteFlags::current().calls && ready()
}

const TABLES: [&str; 4] = [
    "execution_remote_pin",
    "catalog_remote_call",
    "catalog_remote_origin",
    "catalog_remote_exec",
];

const DDL: &[&str] = &[
    // Caller side: what a remote reference resolved to, once per root.
    "CREATE TABLE IF NOT EXISTS noetl.execution_remote_pin (
        root_execution_id BIGINT NOT NULL,
        reference TEXT NOT NULL,
        instance_id TEXT NOT NULL,
        path TEXT NOT NULL,
        version INTEGER NOT NULL,
        digest TEXT NOT NULL,
        mode TEXT NOT NULL,
        created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
        PRIMARY KEY (root_execution_id, reference))",
    // Caller side: one row per delegated call; its id is what A's worker polls.
    "CREATE TABLE IF NOT EXISTS noetl.catalog_remote_call (
        call_id BIGINT PRIMARY KEY,
        root_execution_id BIGINT NOT NULL,
        parent_execution_id BIGINT NULL,
        instance_id TEXT NOT NULL,
        reference TEXT NOT NULL,
        path TEXT NOT NULL,
        version INTEGER NOT NULL,
        digest TEXT NOT NULL,
        remote_execution_id BIGINT NOT NULL,
        created_at TIMESTAMPTZ NOT NULL DEFAULT now())",
    "CREATE INDEX IF NOT EXISTS catalog_remote_call_root_idx ON noetl.catalog_remote_call (root_execution_id)",
    // Serving side: one pin per (calling instance, calling root).
    "CREATE TABLE IF NOT EXISTS noetl.catalog_remote_origin (
        origin_instance TEXT NOT NULL,
        origin_root BIGINT NOT NULL,
        local_root BIGINT NOT NULL,
        watermark BIGINT NOT NULL,
        pinned_at_ms BIGINT NOT NULL,
        created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
        PRIMARY KEY (origin_instance, origin_root))",
    // Serving side: which executions a peer started, so only it may read them.
    "CREATE TABLE IF NOT EXISTS noetl.catalog_remote_exec (
        execution_id BIGINT PRIMARY KEY,
        origin_instance TEXT NOT NULL,
        origin_root BIGINT NOT NULL,
        origin_execution_id BIGINT NULL,
        origin_call_id BIGINT NULL,
        created_at TIMESTAMPTZ NOT NULL DEFAULT now())",
];

/// Best-effort create, then detect. A no-op unless a remote flag is on and
/// the CAS tables are ready.
pub async fn ensure_schema(pool: &DbPool) {
    let f = RemoteFlags::current();
    if !(f.calls || f.serve) {
        return;
    }
    if !cas::pinning_active() {
        tracing::error!(
            "{CALLS_ENV}/{SERVE_ENV} need NOETL_CATALOG_CAS and NOETL_CATALOG_PINNING (with their tables); remote execution stays OFF"
        );
        return;
    }
    for stmt in DDL {
        if let Err(e) = sqlx::query(stmt).execute(pool).await {
            tracing::warn!(error = %e, "catalog remote DDL statement failed (detection decides)");
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
                calls = f.calls,
                serve = f.serve,
                default_mode = f.default_mode.as_str(),
                serve_allow = f.serve_allow.len(),
                "catalog remote execution ON"
            );
        }
        Ok(rows) => tracing::error!(
            found = ?rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            "catalog remote tables missing; remote execution stays OFF"
        ),
        Err(e) => tracing::error!(error = %e, "catalog remote table detection failed; OFF"),
    }
}

// ---------------------------------------------------------------------------
// References
// ---------------------------------------------------------------------------

/// A reference to a playbook another instance holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTarget {
    pub instance: String,
    /// The reference as the owner sees it (a name or a digest, never a URI).
    pub inner: PlaybookRef,
    /// A digest the caller pinned explicitly (`#sha256:` or `@sha256:`).
    pub expect: Option<String>,
}

impl RemoteTarget {
    /// The canonical key a root's remote pin is stored under.
    pub fn key(&self) -> String {
        format!(
            "noetl://{}/{}",
            self.instance,
            peer::ref_string(&self.inner)
        )
    }

    /// Where an import of it lives on this instance.
    pub fn import_path(&self) -> Option<String> {
        match &self.inner {
            PlaybookRef::Name { path, .. } => Some(core::import_path(&self.instance, path)),
            _ => None,
        }
    }
}

/// `peer/<instance>/<path>` → `(instance, path)`. `peer/source/…` (direct
/// fetches) has no instance to ask, so it is not a remote target.
pub fn split_peer_path(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("peer/")?;
    let (instance, inner) = rest.split_once('/')?;
    (!instance.is_empty() && instance != "source" && !inner.is_empty()).then_some((instance, inner))
}

/// Read a request's `path` (+ `version`/`digest` fields) as a remote target,
/// or `None` when it names a local playbook.
pub fn remote_target(
    path: &str,
    version: Option<&Value>,
    digest: Option<&str>,
) -> Result<Option<RemoteTarget>, String> {
    if path.starts_with("noetl://") {
        let r = PlaybookRef::parse(path).map_err(|e| e.to_string())?;
        let PlaybookRef::Source { uri, expect, .. } = r else {
            return Err(format!("'{path}' did not parse as a noetl:// source"));
        };
        let (instance, inner) = core::split_noetl_uri(&uri).map_err(|e| e.to_string())?;
        let expect = expect
            .or_else(|| digest.map(str::to_string))
            .or(match &inner {
                PlaybookRef::Digest { digest } => Some(digest.clone()),
                PlaybookRef::Name {
                    version: VersionSel::Digest(d),
                    ..
                } => Some(d.clone()),
                _ => None,
            });
        return Ok(Some(RemoteTarget {
            instance,
            inner,
            expect,
        }));
    }
    let Some((instance, inner_path)) = split_peer_path(path) else {
        return Ok(None);
    };
    let inner =
        PlaybookRef::from_request(inner_path, version, digest).map_err(|e| e.to_string())?;
    let expect = match &inner {
        PlaybookRef::Name {
            version: VersionSel::Digest(d),
            ..
        } => Some(d.clone()),
        PlaybookRef::Digest { digest } => Some(digest.clone()),
        _ => digest.map(str::to_string),
    };
    Ok(Some(RemoteTarget {
        instance: instance.to_string(),
        inner,
        expect,
    }))
}

/// Home-instance resolution: a bare name called from an imported playbook
/// (`peer/<B>/…`) means B's playbook of that name.
pub fn home_rewrite(parent_path: &str, child_path: &str) -> Option<String> {
    let (instance, _) = split_peer_path(parent_path)?;
    if child_path.starts_with("peer/") || child_path.contains("://") || core::is_digest(child_path)
    {
        return None;
    }
    Some(core::import_path(instance, child_path))
}

// ---------------------------------------------------------------------------
// Remote pins (the caller side)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePin {
    pub instance_id: String,
    pub path: String,
    pub version: i32,
    pub digest: String,
    pub mode: String,
}

pub async fn pin_get(pool: &DbPool, root: i64, reference: &str) -> AppResult<Option<RemotePin>> {
    let r: Option<(String, String, i32, String, String)> = sqlx::query_as(
        "SELECT instance_id, path, version, digest, mode FROM noetl.execution_remote_pin \
         WHERE root_execution_id = $1 AND reference = $2",
    )
    .bind(root)
    .bind(reference)
    .fetch_optional(pool)
    .await?;
    Ok(r.map(|t| RemotePin {
        instance_id: t.0,
        path: t.1,
        version: t.2,
        digest: t.3,
        mode: t.4,
    }))
}

/// Record a root's answer for a reference. First writer wins; the winner is
/// returned, so two racing calls in one tree still agree.
pub async fn pin_put(
    pool: &DbPool,
    root: i64,
    reference: &str,
    p: &RemotePin,
) -> AppResult<RemotePin> {
    sqlx::query(
        "INSERT INTO noetl.execution_remote_pin (root_execution_id, reference, instance_id, path, version, digest, mode) \
         VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT (root_execution_id, reference) DO NOTHING",
    )
    .bind(root)
    .bind(reference)
    .bind(&p.instance_id)
    .bind(&p.path)
    .bind(p.version)
    .bind(&p.digest)
    .bind(&p.mode)
    .execute(pool)
    .await?;
    pin_get(pool, root, reference)
        .await?
        .ok_or_else(|| AppError::Internal("remote pin vanished after insert".into()))
}

/// What a pinned tree keys a remote answer by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinKey {
    /// A playbook another instance holds.
    Peer(RemoteTarget),
    /// A direct source (`gs://`, `s3://`, `db://`, `ehdb://`, `https://`, …).
    Source { uri: String, expect: Option<String> },
}

impl PinKey {
    pub fn key(&self) -> String {
        match self {
            PinKey::Peer(t) => t.key(),
            PinKey::Source { uri, .. } => uri.clone(),
        }
    }
    fn expect(&self) -> Option<&str> {
        match self {
            PinKey::Peer(t) => t.expect.as_deref(),
            PinKey::Source { expect, .. } => expect.as_deref(),
        }
    }
    fn instance(&self) -> &str {
        match self {
            PinKey::Peer(t) => &t.instance,
            PinKey::Source { .. } => "source",
        }
    }
    /// The URI `locate` is asked for, carrying any digest the caller pinned.
    fn locate_uri(&self) -> String {
        match self.expect() {
            Some(d) => format!("{}#{d}", self.key()),
            None => self.key(),
        }
    }
}

/// Resolve a remote reference in import mode inside a pinned tree: the root's
/// earlier answer if it has one, otherwise locate + import now and record it.
pub async fn resolve_import(
    pool: &DbPool,
    rt: Option<&PeerRuntime>,
    root: i64,
    target: &PinKey,
    cached: Option<cas::CasRow>,
) -> AppResult<cas::CasRow> {
    let key = target.key();
    let pinned = match pin_get(pool, root, &key).await? {
        Some(p) => p,
        None => {
            let located = match rt {
                Some(rt) => rt.locate(&target.locate_uri()).await.map(|l| (l.path, l.version, l.digest)),
                None => Err(AppError::NotFound(format!(
                    "{key}: not pinned in this tree yet, and NOETL_CATALOG_DISCOVERY is not running to locate it"
                ))),
            };
            let (path, version, digest) = match (located, cached) {
                (Ok(l), _) => l,
                // A wrong digest or a bad reference is the caller's error, never
                // papered over by a cached copy.
                (Err(e @ AppError::Validation(_)), _) => return Err(e),
                (Err(e), Some(row)) => {
                    tracing::warn!(%key, root, error = %e, path = %row.path, "owner unreachable; pinning this tree to the local import");
                    let d = cas::digest_for(pool, &row).await?;
                    (row.path.clone(), row.version as i32, d)
                }
                (Err(e), None) => return Err(e),
            };
            let p = RemotePin {
                instance_id: target.instance().to_string(),
                path,
                version,
                digest,
                mode: Mode::Import.as_str().into(),
            };
            let won = pin_put(pool, root, &key, &p).await?;
            if won.digest != p.digest {
                tracing::info!(%key, root, "remote pin race: another call in this tree pinned first");
            }
            crate::metrics::record_instance("catalog_remote_pinned");
            tracing::info!(%key, root, digest = %won.digest, path = %won.path, "catalog remote pin (import)");
            won
        }
    };
    if let Some(e) = target.expect() {
        if e != pinned.digest {
            return Err(AppError::Validation(format!(
                "{key}: this tree pinned {}, asked for {e}",
                pinned.digest
            )));
        }
    }
    let l = core::Lookup::Digest {
        digest: pinned.digest.clone(),
        path: Some(pinned.path.clone()),
    };
    cas::lookup(pool, &l).await?.ok_or_else(|| {
        AppError::NotFound(format!(
            "{key}: pinned to {} at {} but that import is gone",
            pinned.digest, pinned.path
        ))
    })
}

// ---------------------------------------------------------------------------
// Delegate (the caller side)
// ---------------------------------------------------------------------------

/// What `/api/execute` answers for a delegated call, beside `execution_id`
/// (which is the call id A's worker polls).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Delegated {
    pub mode: String,
    pub instance_id: String,
    pub remote_execution_id: String,
    pub path: String,
    pub version: i32,
    pub digest: String,
    /// Where the `kind: playbook` tool polls: `{detail_path}` and `{detail_path}/status`.
    pub detail_path: String,
}

/// The fields of an execute request this module reads.
pub struct DelegateAsk<'a> {
    pub path: &'a str,
    pub version: Option<&'a Value>,
    pub digest: Option<&'a str>,
    pub remote: Option<&'a str>,
    pub payload: Value,
    pub parent_execution_id: Option<i64>,
}

/// Decide the mode for a request. `None` → not a delegation; carry on locally.
pub fn wants_delegate(ask: &DelegateAsk<'_>) -> AppResult<Option<RemoteTarget>> {
    let f = RemoteFlags::current();
    let explicit = match ask.remote {
        Some(m) if !m.trim().is_empty() => Some(Mode::parse(m).map_err(AppError::Validation)?),
        _ => None,
    };
    if !f.calls {
        // Off: behave exactly as before, except refuse a delegation by name
        // instead of silently running the playbook somewhere else.
        if explicit == Some(Mode::Delegate) {
            return Err(AppError::Forbidden(format!(
                "remote: delegate needs {CALLS_ENV}=true on this server"
            )));
        }
        return Ok(None);
    }
    let target = remote_target(ask.path, ask.version, ask.digest).map_err(AppError::Validation)?;
    let mode = explicit.unwrap_or(f.default_mode);
    match (mode, target) {
        (Mode::Import, _) => Ok(None),
        (Mode::Delegate, Some(t)) => Ok(Some(t)),
        (Mode::Delegate, None) if explicit.is_some() => Err(AppError::Validation(format!(
            "remote: delegate needs a noetl://<instance>/<path> or peer/<instance>/<path> reference, not '{}'",
            ask.path
        ))),
        (Mode::Delegate, None) => Ok(None),
    }
}

/// Run `target` on its owner. Returns the call id and what the owner started.
pub async fn delegate(
    state: &AppState,
    target: &RemoteTarget,
    ask: DelegateAsk<'_>,
) -> AppResult<(i64, Delegated)> {
    if !calls_active() {
        return Err(AppError::Forbidden(format!(
            "remote: delegate needs {CALLS_ENV}, NOETL_CATALOG_CAS and NOETL_CATALOG_PINNING with their tables"
        )));
    }
    let rt = peer::runtime().ok_or_else(|| {
        AppError::Forbidden(
            "remote: delegate needs NOETL_CATALOG_DISCOVERY (the peer key and registry)".into(),
        )
    })?;
    if target.instance == rt.key.id() || target.instance == "local" {
        return Err(AppError::Validation(format!(
            "{}: names this instance; call it without remote: delegate",
            target.key()
        )));
    }
    let pool = state.pools.cluster();
    let call_id = state.snowflake.generate()?;
    let root = match ask.parent_execution_id {
        Some(p) => cas::get_pin(pool, p)
            .await?
            .map(|r| r.root_execution_id)
            .unwrap_or(p),
        None => call_id,
    };
    let key = target.key();
    let (endpoint, audience) = rt.peer_endpoint(&target.instance)?;

    // The answer this tree already holds for the reference, or B's answer now.
    let pinned = match pin_get(pool, root, &key).await? {
        Some(p) => p,
        None => {
            let a = rt
                .ask(&endpoint, &audience, &peer::ref_string(&target.inner))
                .await?;
            if a.instance_id != target.instance {
                return Err(AppError::ExternalService(format!(
                    "{endpoint} answered as {}, not {}",
                    a.instance_id, target.instance
                )));
            }
            let digest = core::verify_fetched(&a.content, Some(&a.digest))
                .map_err(|e| AppError::ExternalService(e.to_string()))?;
            let p = RemotePin {
                instance_id: a.instance_id,
                path: a.path,
                version: a.version,
                digest,
                mode: Mode::Delegate.as_str().into(),
            };
            crate::metrics::record_instance("catalog_remote_pinned");
            pin_put(pool, root, &key, &p).await?
        }
    };
    if let Some(e) = &target.expect {
        if e != &pinned.digest {
            return Err(AppError::Validation(format!(
                "{key}: this tree pinned {}, asked for {e}",
                pinned.digest
            )));
        }
    }

    let (status, v) = rt
        .signed(
            &endpoint,
            "/api/catalog/peer/execute",
            PURPOSE_EXECUTE,
            &audience,
            json!({
                "path": pinned.path,
                "digest": pinned.digest,
                "payload": ask.payload,
                "origin_root": root.to_string(),
                "origin_execution_id": ask.parent_execution_id.map(|p| p.to_string()),
                "call_id": call_id.to_string(),
            }),
        )
        .await
        .map_err(AppError::ExternalService)?;
    if !(200..300).contains(&status) {
        crate::metrics::record_instance("catalog_remote_refused");
        let msg = v["error"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| v.to_string());
        return Err(match status {
            403 | 401 => {
                AppError::Forbidden(format!("{}: {endpoint} refused: {msg}", target.instance))
            }
            404 => AppError::NotFound(format!("{}: {msg}", target.instance)),
            _ => AppError::ExternalService(format!("{endpoint}: HTTP {status}: {msg}")),
        });
    }
    let remote_execution_id: i64 = v["execution_id"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| v["execution_id"].as_i64())
        .ok_or_else(|| AppError::ExternalService(format!("{endpoint}: no execution_id in {v}")))?;

    sqlx::query(
        "INSERT INTO noetl.catalog_remote_call (call_id, root_execution_id, parent_execution_id, instance_id, \
         reference, path, version, digest, remote_execution_id) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
    )
    .bind(call_id)
    .bind(root)
    .bind(ask.parent_execution_id)
    .bind(&target.instance)
    .bind(&key)
    .bind(&pinned.path)
    .bind(pinned.version)
    .bind(&pinned.digest)
    .bind(remote_execution_id)
    .execute(pool)
    .await?;
    crate::metrics::record_instance("catalog_remote_delegated");
    tracing::info!(
        call_id,
        root,
        instance = %target.instance,
        path = %pinned.path,
        digest = %pinned.digest,
        remote_execution_id,
        "catalog remote call delegated"
    );
    Ok((
        call_id,
        Delegated {
            mode: Mode::Delegate.as_str().into(),
            instance_id: target.instance.clone(),
            remote_execution_id: remote_execution_id.to_string(),
            path: pinned.path,
            version: pinned.version,
            digest: pinned.digest,
            detail_path: format!("/api/catalog/remote/calls/{call_id}"),
        },
    ))
}

#[derive(Debug, Clone)]
struct CallRow {
    instance_id: String,
    path: String,
    version: i32,
    digest: String,
    remote_execution_id: i64,
}

async fn call_row(pool: &DbPool, call_id: i64) -> AppResult<CallRow> {
    let r: Option<(String, String, i32, String, i64)> = sqlx::query_as(
        "SELECT instance_id, path, version, digest, remote_execution_id FROM noetl.catalog_remote_call WHERE call_id = $1",
    )
    .bind(call_id)
    .fetch_optional(pool)
    .await?;
    r.map(|t| CallRow {
        instance_id: t.0,
        path: t.1,
        version: t.2,
        digest: t.3,
        remote_execution_id: t.4,
    })
    .ok_or_else(|| AppError::NotFound(format!("remote call {call_id}")))
}

/// Ask the owner for one delegated execution's status and detail.
async fn fetch_remote(pool: &DbPool, call_id: i64) -> AppResult<(CallRow, Value)> {
    let rt =
        peer::runtime().ok_or_else(|| AppError::NotFound("catalog discovery is off".into()))?;
    let row = call_row(pool, call_id).await?;
    let (endpoint, audience) = rt.peer_endpoint(&row.instance_id)?;
    let (status, v) = rt
        .signed(
            &endpoint,
            "/api/catalog/peer/execution",
            PURPOSE_EXECUTION,
            &audience,
            json!({ "execution_id": row.remote_execution_id.to_string() }),
        )
        .await
        .map_err(AppError::ExternalService)?;
    if !(200..300).contains(&status) {
        return Err(AppError::ExternalService(format!(
            "{endpoint}: HTTP {status}: {v}"
        )));
    }
    Ok((row, v))
}

fn remote_note(call_id: i64, row: &CallRow) -> Value {
    json!({
        "call_id": call_id.to_string(),
        "instance_id": row.instance_id,
        "execution_id": row.remote_execution_id.to_string(),
        "path": row.path,
        "version": row.version,
        "digest": row.digest,
    })
}

async fn call_detail(
    State(s): State<RemoteState>,
    Path(call_id): Path<i64>,
) -> AppResult<Json<Value>> {
    let (row, v) = fetch_remote(s.app.pools.cluster(), call_id).await?;
    let mut d = v.get("detail").cloned().unwrap_or(Value::Null);
    if let Value::Object(m) = &mut d {
        m.insert("remote".into(), remote_note(call_id, &row));
    }
    Ok(Json(d))
}

async fn call_status(
    State(s): State<RemoteState>,
    Path(call_id): Path<i64>,
) -> AppResult<Json<Value>> {
    let (row, v) = fetch_remote(s.app.pools.cluster(), call_id).await?;
    let mut st = v.get("status").cloned().unwrap_or(Value::Null);
    if let Value::Object(m) = &mut st {
        m.insert("remote".into(), remote_note(call_id, &row));
    }
    Ok(Json(st))
}

// ---------------------------------------------------------------------------
// Serve (the owner side)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecuteAsk {
    path: String,
    digest: String,
    #[serde(default)]
    payload: Value,
    origin_root: String,
    #[serde(default)]
    origin_execution_id: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecutionAsk {
    execution_id: String,
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

#[allow(clippy::result_large_err)]
fn admit(
    rt: &PeerRuntime,
    env: &SignedEnvelope,
    purpose: &str,
) -> Result<crate::instances::gate::Admitted, Response> {
    rt.instances
        .gate
        .admit(env, purpose, crate::instances::now_ms())
        .map_err(|r| {
            crate::metrics::record_instance("catalog_remote_refused");
            err(
                StatusCode::from_u16(r.status).unwrap_or(StatusCode::FORBIDDEN),
                r.reason,
            )
        })
}

/// The one pin every delegated execution from `(origin, origin_root)` shares.
pub async fn origin_pin(
    pool: &DbPool,
    local_root: i64,
    origin: &str,
    origin_root: i64,
) -> AppResult<Pin> {
    let get = || async {
        let r: Option<(i64, i64, i64)> = sqlx::query_as(
            "SELECT local_root, watermark, pinned_at_ms FROM noetl.catalog_remote_origin \
             WHERE origin_instance = $1 AND origin_root = $2",
        )
        .bind(origin)
        .bind(origin_root)
        .fetch_optional(pool)
        .await?;
        AppResult::Ok(r)
    };
    if get().await?.is_none() {
        let (w, ms) = cas::watermark_now(pool).await?;
        sqlx::query(
            "INSERT INTO noetl.catalog_remote_origin (origin_instance, origin_root, local_root, watermark, pinned_at_ms) \
             VALUES ($1,$2,$3,$4,$5) ON CONFLICT (origin_instance, origin_root) DO NOTHING",
        )
        .bind(origin)
        .bind(origin_root)
        .bind(local_root)
        .bind(w)
        .bind(ms)
        .execute(pool)
        .await?;
    }
    let (root, watermark, pinned_at_ms) = get()
        .await?
        .ok_or_else(|| AppError::Internal("remote origin pin vanished after insert".into()))?;
    Ok(Pin {
        root_execution_id: root,
        watermark,
        pinned_at_ms,
        // A peer never widens this instance's pin: no hot reload.
        policy: PinPolicy::default(),
    })
}

async fn serve_execute(State(s): State<RemoteState>, Json(env): Json<SignedEnvelope>) -> Response {
    let Some(rt) = peer::runtime() else {
        return err(StatusCode::NOT_FOUND, "catalog discovery is off");
    };
    let admitted = match admit(rt, &env, PURPOSE_EXECUTE) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let ask: ExecuteAsk = match serde_json::from_value(admitted.body) {
        Ok(a) => a,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("body: {e}")),
    };
    let f = RemoteFlags::current();
    if !ready() || !f.serve {
        return err(
            StatusCode::FORBIDDEN,
            format!("{SERVE_ENV} is off on this instance"),
        );
    }
    if ask.path.starts_with("peer/") || !f.serves(&ask.path) {
        crate::metrics::record_instance("catalog_remote_refused");
        return err(
            StatusCode::FORBIDDEN,
            format!(
                "{}: not served to peers (no {SERVE_ALLOW_ENV} glob matches)",
                ask.path
            ),
        );
    }
    if !core::is_digest(&ask.digest) {
        return err(StatusCode::BAD_REQUEST, "digest must be sha256:<64 hex>");
    }
    let Ok(origin_root) = ask.origin_root.parse::<i64>() else {
        return err(StatusCode::BAD_REQUEST, "origin_root is not an id");
    };
    let parse_opt = |s: &Option<String>| s.as_deref().and_then(|v| v.parse::<i64>().ok());
    let pool = s.app.pools.cluster();
    let local_root = match s.app.snowflake.generate() {
        Ok(id) => id,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let pin = match origin_pin(pool, local_root, &admitted.instance_id, origin_root).await {
        Ok(p) => p,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let payload = match ask.payload {
        Value::Object(m) => m.into_iter().collect(),
        Value::Null => Default::default(),
        _ => return err(StatusCode::BAD_REQUEST, "payload must be an object"),
    };
    let request = crate::handlers::execute::ExecuteRequest {
        path: Some(ask.path.clone()),
        catalog_id: None,
        version: None,
        digest: Some(ask.digest.clone()),
        payload,
        parent_execution_id: None,
        execution_pool: None,
        trace: None,
        dedup: None,
        remote: None,
        pin_override: Some(pin.clone()),
        await_park: None,
    };
    let out = match crate::handlers::execute::execute_one(&s.app, request, "remote").await {
        Ok(o) => o,
        Err(AppError::NotFound(m)) => return err(StatusCode::NOT_FOUND, m),
        Err(AppError::Validation(m)) => return err(StatusCode::BAD_REQUEST, m),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    if let Err(e) = sqlx::query(
        "INSERT INTO noetl.catalog_remote_exec (execution_id, origin_instance, origin_root, origin_execution_id, origin_call_id) \
         VALUES ($1,$2,$3,$4,$5) ON CONFLICT (execution_id) DO NOTHING",
    )
    .bind(out.execution_id)
    .bind(&admitted.instance_id)
    .bind(origin_root)
    .bind(parse_opt(&ask.origin_execution_id))
    .bind(parse_opt(&ask.call_id))
    .execute(pool)
    .await
    {
        // Without this row the caller can never read its result; say so now.
        return err(StatusCode::INTERNAL_SERVER_ERROR, format!("started {} but could not record it: {e}", out.execution_id));
    }
    crate::metrics::record_instance("catalog_remote_served");
    tracing::info!(
        peer = %admitted.instance_id,
        origin_root,
        execution_id = out.execution_id,
        path = %ask.path,
        digest = %ask.digest,
        "catalog remote execute served"
    );
    Json(json!({
        "execution_id": out.execution_id.to_string(),
        "path": ask.path,
        "digest": ask.digest,
        "root_execution_id": pin.root_execution_id.to_string(),
    }))
    .into_response()
}

async fn serve_execution(
    State(s): State<RemoteState>,
    Json(env): Json<SignedEnvelope>,
) -> Response {
    let Some(rt) = peer::runtime() else {
        return err(StatusCode::NOT_FOUND, "catalog discovery is off");
    };
    let admitted = match admit(rt, &env, PURPOSE_EXECUTION) {
        Ok(a) => a,
        Err(r) => return r,
    };
    if !ready() || !RemoteFlags::current().serve {
        return err(
            StatusCode::FORBIDDEN,
            format!("{SERVE_ENV} is off on this instance"),
        );
    }
    let ask: ExecutionAsk = match serde_json::from_value(admitted.body) {
        Ok(a) => a,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("body: {e}")),
    };
    let Ok(id) = ask.execution_id.parse::<i64>() else {
        return err(StatusCode::BAD_REQUEST, "execution_id is not an id");
    };
    // Only the instance that started it may read it.
    let owner: Result<Option<(String,)>, _> = sqlx::query_as(
        "SELECT origin_instance FROM noetl.catalog_remote_exec WHERE execution_id = $1",
    )
    .bind(id)
    .fetch_optional(s.app.pools.cluster())
    .await;
    match owner {
        Ok(Some((o,))) if o == admitted.instance_id => {}
        Ok(_) => {
            return err(
                StatusCode::NOT_FOUND,
                format!("execution {id} was not started by {}", admitted.instance_id),
            )
        }
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
    let status = match s.executions.get_status(id).await {
        Ok(st) => serde_json::to_value(st).unwrap_or(Value::Null),
        Err(AppError::NotFound(m)) => return err(StatusCode::NOT_FOUND, m),
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let detail = match s.executions.get(id).await {
        Ok(d) => serde_json::to_value(d).unwrap_or(Value::Null),
        Err(e) => json!({ "error": e.to_string() }),
    };
    Json(json!({ "status": status, "detail": detail })).into_response()
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct RemoteState {
    pub app: AppState,
    pub executions: ExecutionService,
}

/// The routes to mount, or `None` with both sides off. Call after
/// [`ensure_schema`] and `catalog_peer::init`.
pub fn routes(app: AppState, executions: ExecutionService) -> Option<Router> {
    let f = RemoteFlags::current();
    if !ready() || !(f.calls || f.serve) {
        return None;
    }
    if peer::runtime().is_none() {
        tracing::warn!(
            "catalog remote execution: NOETL_CATALOG_DISCOVERY is not running, so delegation and serving are OFF (import-mode remote pins still apply)"
        );
        return None;
    }
    let mut r = Router::new();
    if f.calls {
        r = r
            .route("/api/catalog/remote/calls/{call_id}", get(call_detail))
            .route(
                "/api/catalog/remote/calls/{call_id}/status",
                get(call_status),
            );
    }
    if f.serve {
        if f.serve_allow.is_empty() {
            tracing::warn!(
                "{SERVE_ENV}=true with no {SERVE_ALLOW_ENV}: every peer execute is refused"
            );
        }
        r = r
            .route("/api/catalog/peer/execute", post(serve_execute))
            .route("/api/catalog/peer/execution", post(serve_execution));
    }
    Some(r.with_state(RemoteState { app, executions }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn flags(pairs: &[(&str, &str)]) -> RemoteFlags {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        RemoteFlags::from_lookup(|k| m.get(k).cloned())
    }

    const D: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn flags_default_off_strict_and_fail_closed() {
        assert_eq!(flags(&[]), RemoteFlags::default());
        assert!(!flags(&[(CALLS_ENV, "1")]).calls);
        assert!(flags(&[(CALLS_ENV, " true ")]).calls);
        assert_eq!(
            flags(&[(MODE_ENV, "delegate")]).default_mode,
            Mode::Delegate
        );
        assert_eq!(flags(&[(MODE_ENV, "bogus")]).default_mode, Mode::Import);
        let serve = flags(&[(SERVE_ENV, "true")]);
        assert!(!serve.serves("shared/x"), "no allowlist → nothing served");
        let serve = flags(&[(SERVE_ENV, "true"), (SERVE_ALLOW_ENV, "shared/*, xc/*")]);
        assert!(serve.serves("xc/target"));
        assert!(!serve.serves("secret/x"));
        let off = flags(&[(SERVE_ALLOW_ENV, "*")]);
        assert!(
            !off.serves("xc/target"),
            "allowlist without the flag serves nothing"
        );
    }

    #[test]
    fn noetl_uris_and_peer_paths_are_remote_targets() {
        let t = remote_target("noetl://ni-b/xc/target", None, None)
            .unwrap()
            .unwrap();
        assert_eq!(t.instance, "ni-b");
        assert_eq!(t.key(), "noetl://ni-b/xc/target");
        assert_eq!(t.import_path().as_deref(), Some("peer/ni-b/xc/target"));
        assert_eq!(t.expect, None);

        let t = remote_target(&format!("noetl://ni-b/xc/target@2#{D}"), None, None)
            .unwrap()
            .unwrap();
        assert_eq!(t.key(), "noetl://ni-b/xc/target@2");
        assert_eq!(t.expect.as_deref(), Some(D));

        let t = remote_target("peer/ni-b/xc/target", Some(&json!(3)), None)
            .unwrap()
            .unwrap();
        assert_eq!(t.key(), "noetl://ni-b/xc/target@3");

        let t = remote_target("peer/ni-b/xc/target", None, Some(D))
            .unwrap()
            .unwrap();
        assert_eq!(t.expect.as_deref(), Some(D));

        assert_eq!(remote_target("xc/target", None, None).unwrap(), None);
        assert_eq!(remote_target("peer/source/xc/x", None, None).unwrap(), None);
        assert!(remote_target("noetl://ni-b", None, None).is_err());
    }

    #[test]
    fn children_of_an_import_resolve_on_its_home_instance() {
        assert_eq!(
            home_rewrite("peer/ni-b/xc/target", "xc/leaf").as_deref(),
            Some("peer/ni-b/xc/leaf")
        );
        assert_eq!(home_rewrite("xc/target", "xc/leaf"), None, "not an import");
        assert_eq!(
            home_rewrite("peer/source/xc/x", "xc/leaf"),
            None,
            "a fetched source has no home"
        );
        assert_eq!(
            home_rewrite("peer/ni-b/xc/target", "peer/ni-c/y"),
            None,
            "already qualified"
        );
        assert_eq!(home_rewrite("peer/ni-b/xc/target", "noetl://ni-c/y"), None);
        assert_eq!(home_rewrite("peer/ni-b/xc/target", D), None);
    }

    #[test]
    fn modes_parse_and_refuse_typos() {
        assert_eq!(Mode::parse("Delegate").unwrap(), Mode::Delegate);
        assert_eq!(Mode::parse("import").unwrap(), Mode::Import);
        assert!(Mode::parse("remote").is_err());
    }
}
