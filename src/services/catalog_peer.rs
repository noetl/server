//! Cross-cluster playbook discovery (noetl/ai-meta#548 Phase 2,
//! RFC `docs/rfc/distributed-playbook-catalog.md` §4).
//!
//! A server that takes part has an instance key (the item-9 ML-DSA-87 identity).
//! It does three things:
//!
//! 1. **Advertise.** Every `NOETL_CATALOG_ADVERT_SECS` it registers with each peer
//!    control plane in `NOETL_CATALOG_PEERS` as a `distributed` instance. The
//!    registration's `endpoint` is its own URL, and its `catalog` field is a
//!    Mixed-scheme bloom over `p:<path>`, `v:<path>@<version>`, `d:<digest>`.
//! 2. **Answer.** `POST /api/catalog/peer/resolve` takes a signed envelope
//!    (purpose `catalog.resolve`) and is admitted by the item-9 gate, so the
//!    caller must be in `NOETL_INSTANCE_TRUSTED_IDS`. It returns the resolved
//!    body and its digest. Credential rows are never answered.
//! 3. **Locate.** `POST /api/catalog/locate` (internal token) looks in three
//!    places, cheapest first:
//!    - the local catalog;
//!    - the reference's own source (`noetl://<id>/…`, `gs://`, `s3://`, `az://`,
//!      `minio+http(s)://`, `https://`);
//!    - the live distributed peers in this server's registry whose bloom says
//!      "maybe".
//!
//!    A body that verifies against its digest is imported under
//!    `peer/<instance>/<path>`, with its provenance as a source row.
//!
//! # Flags (strict `"true"` unless noted; all need `NOETL_CATALOG_CAS`)
//!
//! - `NOETL_CATALOG_DISCOVERY`: the routes, the advertiser, locate and import.
//!   It also needs `NOETL_INSTANCE_REGISTRY=true` (the gate and the peer list
//!   live there) and `NOETL_CATALOG_PEER_KEY_FILE`.
//! - `NOETL_CATALOG_PEERS`: `url[#audience],…`, the control planes to advertise to.
//! - `NOETL_CATALOG_SELF_URL`: the URL peers fetch from (default
//!   `NOETL_PUBLIC_SERVER_URL`).
//! - `NOETL_CATALOG_URI_FETCH` + `NOETL_CATALOG_URI_ALLOW` (comma-separated
//!   globs): direct source fetch. **Fail closed**: with no allowlist, nothing is
//!   fetched.
//!
//! # Credentials
//!
//! Object stores use ambient platform identity only: GKE workload identity for
//! `gs://`, and the standard `AWS_*` / `AZURE_*` environment for `s3://`, `az://`
//! and MinIO. Keychain-alias credentials for sources are TARGET (RFC §9).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use noetl_orchestrate_core::catalog::{self as core, PlaybookRef};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::db::DbPool;
use crate::error::{AppError, AppResult};
use crate::instances::identity::{InstanceKey, SignedEnvelope};
use crate::instances::registry::{Capabilities, CatalogAdvert, Query, Reach, RuntimeKind};
use crate::instances::InstanceState;
use crate::services::catalog_cas as cas;

pub const DISCOVERY_ENV: &str = noetl_orchestrate_core::dsl::CATALOG_DISCOVERY_ENV;
pub const PEERS_ENV: &str = "NOETL_CATALOG_PEERS";
pub const KEY_FILE_ENV: &str = noetl_orchestrate_core::dsl::CATALOG_PEER_KEY_FILE_ENV;
pub const SELF_URL_ENV: &str = "NOETL_CATALOG_SELF_URL";
pub const ADVERT_SECS_ENV: &str = "NOETL_CATALOG_ADVERT_SECS";
pub const URI_FETCH_ENV: &str = noetl_orchestrate_core::dsl::CATALOG_URI_FETCH_ENV;
pub const URI_ALLOW_ENV: &str = noetl_orchestrate_core::dsl::CATALOG_URI_ALLOW_ENV;

/// The label a server puts on its registration so peers sign for the right audience.
pub const AUDIENCE_LABEL: &str = "noetl.audience";
/// Upper bound on a fetched or answered playbook body.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
/// At ~1% FPR the Mixed cap holds about this many keys (RFC §4.2).
pub const ADVERT_KEY_BUDGET: usize = 27_000;
/// Peers queried per locate, freshest first.
pub const MAX_PEER_QUERIES: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub url: String,
    pub audience: String,
}

/// Parsed configuration. Pure, so tests need no environment.
#[derive(Debug, Clone, Default)]
pub struct PeerConfig {
    pub discovery: bool,
    pub peers: Vec<Peer>,
    pub key_file: Option<String>,
    pub self_url: Option<String>,
    pub advert_secs: u64,
    pub uri_fetch: bool,
    pub uri_allow: Vec<String>,
}

impl PeerConfig {
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let on = |k: &str| get(k).is_some_and(|v| v.trim() == "true");
        let list = |k: &str| -> Vec<String> {
            get(k)
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        };
        let peers = list(PEERS_ENV)
            .into_iter()
            .map(|p| match p.split_once('#') {
                Some((u, a)) if !a.is_empty() => Peer {
                    url: u.trim_end_matches('/').to_string(),
                    audience: a.to_string(),
                },
                _ => Peer {
                    url: p.trim_end_matches('/').trim_end_matches('#').to_string(),
                    audience: "noetl".to_string(),
                },
            })
            .collect();
        PeerConfig {
            discovery: on(DISCOVERY_ENV),
            peers,
            key_file: get(KEY_FILE_ENV).filter(|s| !s.trim().is_empty()),
            self_url: get(SELF_URL_ENV)
                .or_else(|| get("NOETL_PUBLIC_SERVER_URL"))
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim_end_matches('/').to_string()),
            advert_secs: get(ADVERT_SECS_ENV)
                .and_then(|s| s.trim().parse().ok())
                .filter(|s| (5..=3600).contains(s))
                .unwrap_or(60),
            uri_fetch: on(URI_FETCH_ENV),
            uri_allow: list(URI_ALLOW_ENV),
        }
    }

    /// Fail closed: a URI is fetchable only with the flag on and an allowlist glob that matches.
    pub fn uri_allowed(&self, uri: &str) -> bool {
        self.uri_fetch && self.uri_allow.iter().any(|g| core::glob_match(g, uri))
    }
}

/// Load an instance key file in the CLI's format: `{"alg","instance_id","seed"}`.
pub fn load_key(path: &str) -> Result<InstanceKey, String> {
    use base64::Engine as _;
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let doc: Value = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
    let seed = base64::engine::general_purpose::STANDARD
        .decode(doc["seed"].as_str().unwrap_or_default())
        .map_err(|e| format!("{path}: seed: {e}"))?;
    let seed: [u8; 32] = seed
        .try_into()
        .map_err(|_| format!("{path}: seed is not 32 bytes"))?;
    let key = InstanceKey::from_seed(seed);
    if let Some(id) = doc["instance_id"].as_str() {
        if id != key.id() {
            return Err(format!(
                "{path}: instance_id {id} does not match the seed ({})",
                key.id()
            ));
        }
    }
    Ok(key)
}

/// The running discovery service. One per process.
pub struct PeerRuntime {
    pub cfg: PeerConfig,
    pub key: InstanceKey,
    pub pool: DbPool,
    pub instances: InstanceState,
    pub http: reqwest::Client,
    generation: AtomicU64,
}

static RUNTIME: OnceLock<PeerRuntime> = OnceLock::new();

/// The service, when discovery is fully configured on this process.
pub fn runtime() -> Option<&'static PeerRuntime> {
    RUNTIME.get()
}

/// Start discovery if configured. Logs why not, otherwise. Returns the routes to mount.
pub fn init(pool: DbPool, instances: Option<InstanceState>) -> Option<Router> {
    let cfg = PeerConfig::from_lookup(|k| std::env::var(k).ok());
    if !cfg.discovery {
        return None;
    }
    let why_not = if !cas::ready() {
        Some("NOETL_CATALOG_CAS is off or its tables are missing")
    } else if instances.is_none() {
        Some("NOETL_INSTANCE_REGISTRY is off (the gate and the peer list live there)")
    } else if cfg.key_file.is_none() {
        Some("NOETL_CATALOG_PEER_KEY_FILE is unset")
    } else {
        None
    };
    if let Some(w) = why_not {
        tracing::error!("{DISCOVERY_ENV}=true but {w}; catalog discovery stays OFF");
        return None;
    }
    let key = match load_key(cfg.key_file.as_deref().unwrap_or_default()) {
        Ok(k) => k,
        Err(e) => {
            tracing::error!(error = %e, "catalog peer key did not load; catalog discovery stays OFF");
            return None;
        }
    };
    let rt = PeerRuntime {
        key,
        pool,
        instances: instances.expect("checked above"),
        http: reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_default(),
        generation: AtomicU64::new(crate::instances::now_ms().max(0) as u64),
        cfg,
    };
    tracing::info!(
        instance_id = rt.key.id(),
        peers = rt.cfg.peers.len(),
        uri_fetch = rt.cfg.uri_fetch,
        "catalog discovery ON"
    );
    if RUNTIME.set(rt).is_err() {
        return None;
    }
    let rt = RUNTIME.get().expect("just set");
    if !rt.cfg.peers.is_empty() {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(rt.cfg.advert_secs));
            loop {
                tick.tick().await;
                rt.advertise_once().await;
            }
        });
    }
    Some(
        Router::new()
            .route("/api/catalog/peer/resolve", post(peer_resolve))
            .route("/api/catalog/locate", post(locate_route)),
    )
}

// ---------------------------------------------------------------------------
// Advertise
// ---------------------------------------------------------------------------

/// Build the bloom over every non-credential, non-imported row. Imports are
/// left out so two peers never re-advertise each other's catalogs in a loop.
pub fn advert_from(rows: &[(String, i32, Option<String>)], generation: u64) -> CatalogAdvert {
    let families: Vec<&str> = if rows.len() * 3 <= ADVERT_KEY_BUDGET {
        vec!["p", "v", "d"]
    } else {
        vec!["p", "d"]
    };
    let mut bloom = ehdb_l0::Bloom::for_expected_in(
        (rows.len() * families.len()).max(64),
        ehdb_l0::bloom::BloomScheme::Mixed,
    );
    for (path, version, digest) in rows {
        let d = digest.clone().unwrap_or_default();
        for key in core::advert_keys(path, *version, &d) {
            let family = &key[..1];
            if families.contains(&family) && !(family == "d" && digest.is_none()) {
                bloom.insert(&key);
            }
        }
    }
    if rows.len() * families.len() > ADVERT_KEY_BUDGET {
        tracing::warn!(
            entries = rows.len(),
            "catalog advert over its key budget; false positives rise above 1%"
        );
    }
    CatalogAdvert {
        generation,
        entries: rows.len().min(u32::MAX as usize) as u32,
        families: families.into_iter().map(str::to_string).collect(),
        bloom,
    }
}

impl PeerRuntime {
    async fn advert(&self) -> AppResult<CatalogAdvert> {
        let archived = crate::db::queries::catalog::archived_filter();
        let rows: Vec<(String, i16, Option<String>)> = sqlx::query_as(&format!(
            "SELECT c.path, c.version, d.digest FROM noetl.catalog c \
             LEFT JOIN noetl.catalog_digest d ON d.catalog_id = c.catalog_id \
             WHERE lower(c.kind) <> 'credential' AND c.path NOT LIKE 'peer/%'{archived}"
        ))
        .fetch_all(&self.pool)
        .await?;
        let rows: Vec<(String, i32, Option<String>)> =
            rows.into_iter().map(|(p, v, d)| (p, v as i32, d)).collect();
        Ok(advert_from(
            &rows,
            self.generation.fetch_add(1, Ordering::Relaxed) + 1,
        ))
    }

    fn capabilities(&self, advert: CatalogAdvert) -> Capabilities {
        Capabilities {
            runtime_kind: RuntimeKind::Distributed,
            version: format!("noetl-server/{}", env!("CARGO_PKG_VERSION")),
            name: Some("noetl-server".into()),
            site: None,
            labels: [(
                AUDIENCE_LABEL.to_string(),
                self.instances.gate.audience.clone(),
            )]
            .into(),
            tool_kinds: vec![],
            playbooks: vec![],
            pools: vec![],
            reach: Reach::Inbox,
            endpoint: self.cfg.self_url.clone(),
            accepts_tasks: false,
            known_peers: vec![],
            catalog: Some(advert),
        }
    }

    /// Register with every configured peer once. Failures are logged, never fatal.
    pub async fn advertise_once(&self) {
        if self.cfg.self_url.is_none() {
            tracing::error!(
                "catalog advert skipped: no {SELF_URL_ENV} / NOETL_PUBLIC_SERVER_URL to advertise"
            );
            return;
        }
        let advert = match self.advert().await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(error = %e, "catalog advert build failed");
                return;
            }
        };
        let caps = self.capabilities(advert);
        let body = serde_json::to_value(&caps).unwrap_or_default();
        for p in &self.cfg.peers {
            match self
                .signed(
                    &p.url,
                    "/api/instances/register",
                    "register",
                    &p.audience,
                    body.clone(),
                )
                .await
            {
                Ok((s, _)) if (200..300).contains(&s) => {
                    crate::metrics::record_instance("catalog_advertised");
                    tracing::debug!(peer = %p.url, "catalog advertised");
                }
                Ok((s, v)) => {
                    tracing::warn!(peer = %p.url, status = s, reply = %v, "catalog advert refused")
                }
                Err(e) => tracing::warn!(peer = %p.url, error = %e, "catalog advert failed"),
            }
        }
    }

    async fn signed(
        &self,
        base: &str,
        route: &str,
        purpose: &str,
        audience: &str,
        body: Value,
    ) -> Result<(u16, Value), String> {
        let env: SignedEnvelope = self.key.sign(crate::instances::gate::payload(
            purpose,
            self.key.id(),
            audience,
            crate::instances::now_ms(),
            &nonce(),
            body,
        ));
        let r = self
            .http
            .post(format!("{base}{route}"))
            .json(&env)
            .send()
            .await
            .map_err(|e| format!("POST {base}{route}: {e}"))?;
        let status = r.status().as_u16();
        let bytes = r.bytes().await.map_err(|e| e.to_string())?;
        if bytes.len() > MAX_BODY_BYTES * 2 {
            return Err(format!(
                "{base}{route}: reply over {} bytes",
                MAX_BODY_BYTES * 2
            ));
        }
        Ok((
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        ))
    }
}

fn nonce() -> String {
    use rand::RngCore as _;
    let mut b = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

// ---------------------------------------------------------------------------
// Answer (the peer side)
// ---------------------------------------------------------------------------

/// What a peer answers: the body and the identity it claims.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PeerAnswer {
    pub instance_id: String,
    pub path: String,
    pub version: i32,
    pub kind: String,
    pub digest: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveAsk {
    #[serde(rename = "ref")]
    reference: String,
}

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

async fn peer_resolve(Json(env): Json<SignedEnvelope>) -> Response {
    let Some(rt) = runtime() else {
        return err(StatusCode::NOT_FOUND, "catalog discovery is off");
    };
    let admitted =
        match rt
            .instances
            .gate
            .admit(&env, "catalog.resolve", crate::instances::now_ms())
        {
            Ok(a) => a,
            Err(r) => {
                crate::metrics::record_instance("catalog_peer_refused");
                return err(
                    StatusCode::from_u16(r.status).unwrap_or(StatusCode::FORBIDDEN),
                    r.reason,
                );
            }
        };
    let ask: ResolveAsk = match serde_json::from_value(admitted.body) {
        Ok(a) => a,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("body: {e}")),
    };
    match answer_local(rt, &ask.reference).await {
        Ok(a) => {
            crate::metrics::record_instance("catalog_peer_answered");
            tracing::info!(peer = %admitted.instance_id, path = %a.path, version = a.version, "catalog peer resolve answered");
            Json(a).into_response()
        }
        Err(AppError::NotFound(m)) => err(StatusCode::NOT_FOUND, m),
        Err(e) => err(StatusCode::BAD_REQUEST, e.to_string()),
    }
}

async fn answer_local(rt: &PeerRuntime, reference: &str) -> AppResult<PeerAnswer> {
    let r = PlaybookRef::parse(reference).map_err(|e| AppError::Validation(e.to_string()))?;
    if matches!(r, PlaybookRef::Source { .. }) {
        return Err(AppError::Validation(
            "a peer resolves names and digests, not URIs".into(),
        ));
    }
    let (l, reason) =
        core::plan(&r, None, false).map_err(|e| AppError::Validation(e.to_string()))?;
    let row = cas::lookup(&rt.pool, &l)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("not held here: {}", core::describe(&l))))?;
    if row.kind.eq_ignore_ascii_case("credential") || row.path.starts_with("peer/") {
        return Err(AppError::NotFound(format!(
            "not held here: {}",
            core::describe(&l)
        )));
    }
    let digest = cas::digest_for(&rt.pool, &row).await?;
    let entry = core::Entry {
        catalog_id: row.catalog_id,
        path: row.path.clone(),
        version: row.version as i32,
        digest: digest.clone(),
    };
    core::finish(&l, reason, Some(entry)).map_err(|e| AppError::Validation(e.to_string()))?;
    let content = row
        .content
        .clone()
        .ok_or_else(|| AppError::NotFound("row has no content".into()))?;
    if content.len() > MAX_BODY_BYTES {
        return Err(AppError::Validation(format!(
            "body over {MAX_BODY_BYTES} bytes"
        )));
    }
    Ok(PeerAnswer {
        instance_id: rt.key.id().to_string(),
        path: row.path,
        version: row.version as i32,
        kind: row.kind,
        digest,
        content,
    })
}

// ---------------------------------------------------------------------------
// Locate (the asking side)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Located {
    /// `local`, `source` or `peer`.
    pub found_at: String,
    pub catalog_id: String,
    pub path: String,
    pub version: i32,
    pub digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Live distributed peers considered, and how many blooms said "maybe".
    pub peers_considered: usize,
    pub peers_maybe: usize,
    pub peers_queried: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocateBody {
    #[serde(rename = "ref")]
    reference: String,
}

async fn locate_route(
    _token: crate::handlers::internal::RequireInternalApiToken,
    Json(b): Json<LocateBody>,
) -> AppResult<Json<Located>> {
    let rt = runtime().ok_or_else(|| AppError::NotFound("catalog discovery is off".into()))?;
    Ok(Json(rt.locate(&b.reference).await?))
}

impl PeerRuntime {
    /// Local, then the reference's own source, then peers whose bloom says maybe.
    /// Anything found remotely is verified and imported before it is returned.
    pub async fn locate(&self, reference: &str) -> AppResult<Located> {
        let r = PlaybookRef::parse(reference).map_err(|e| AppError::Validation(e.to_string()))?;
        match &r {
            PlaybookRef::Source {
                scheme,
                uri,
                expect,
            } if scheme == "noetl" => {
                let (instance, inner) =
                    core::split_noetl_uri(uri).map_err(|e| AppError::Validation(e.to_string()))?;
                if instance == "local" || instance == self.key.id() {
                    return self
                        .local(&inner)
                        .await?
                        .ok_or_else(|| AppError::NotFound(format!("{uri}: not in this catalog")));
                }
                let (endpoint, audience) = self.peer_endpoint(&instance)?;
                let inner_ref = ref_string(&inner);
                let a = self.ask(&endpoint, &audience, &inner_ref).await?;
                if a.instance_id != instance {
                    return Err(AppError::ExternalService(format!(
                        "{endpoint} answered as {}, not {instance}",
                        a.instance_id
                    )));
                }
                let mut l = self.import_answer(a, expect.as_deref()).await?;
                l.peers_considered = 1;
                l.peers_queried = 1;
                Ok(l)
            }
            PlaybookRef::Source { uri, expect, .. } => {
                self.fetch_source(uri, expect.as_deref()).await
            }
            _ => {
                if let Some(l) = self.local(&r).await? {
                    return Ok(l);
                }
                self.probe(&r).await
            }
        }
    }

    async fn local(&self, r: &PlaybookRef) -> AppResult<Option<Located>> {
        let (l, reason) =
            core::plan(r, None, false).map_err(|e| AppError::Validation(e.to_string()))?;
        let Some(row) = cas::lookup(&self.pool, &l).await? else {
            return Ok(None);
        };
        let digest = cas::digest_for(&self.pool, &row).await?;
        let entry = core::Entry {
            catalog_id: row.catalog_id,
            path: row.path.clone(),
            version: row.version as i32,
            digest,
        };
        let res = core::finish(&l, reason, Some(entry))
            .map_err(|e| AppError::Validation(e.to_string()))?;
        Ok(Some(Located {
            found_at: "local".into(),
            catalog_id: res.entry.catalog_id.to_string(),
            path: res.entry.path,
            version: res.entry.version,
            digest: res.entry.digest,
            instance_id: None,
            source: None,
            peers_considered: 0,
            peers_maybe: 0,
            peers_queried: 0,
        }))
    }

    fn peer_endpoint(&self, instance: &str) -> AppResult<(String, String)> {
        let reg = self
            .instances
            .registry
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let now = crate::instances::now_ms();
        let rec = reg
            .get(instance)
            .filter(|_| reg.is_live(instance, now))
            .ok_or_else(|| {
                AppError::NotFound(format!(
                    "peer {instance} is not registered here (or its lease lapsed)"
                ))
            })?;
        let endpoint =
            rec.capabilities.endpoint.clone().ok_or_else(|| {
                AppError::NotFound(format!("peer {instance} advertises no endpoint"))
            })?;
        let audience = rec
            .capabilities
            .labels
            .get(AUDIENCE_LABEL)
            .cloned()
            .unwrap_or_else(|| "noetl".into());
        Ok((endpoint.trim_end_matches('/').to_string(), audience))
    }

    /// Live distributed peers with an advert, freshest first, with their blooms.
    fn candidates(&self) -> Vec<(String, String, String, CatalogAdvert)> {
        let reg = self
            .instances
            .registry
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let q = Query {
            runtime_kind: Some(RuntimeKind::Distributed),
            ..Default::default()
        };
        reg.query(&q, crate::instances::now_ms())
            .into_iter()
            .filter(|f| f.live)
            .filter_map(|f| {
                let c = &f.record.capabilities;
                let advert = c.catalog.clone()?;
                let endpoint = c.endpoint.clone()?;
                let audience = c
                    .labels
                    .get(AUDIENCE_LABEL)
                    .cloned()
                    .unwrap_or_else(|| "noetl".into());
                (f.record.instance_id != self.key.id()).then(|| {
                    (
                        f.record.instance_id.clone(),
                        endpoint.trim_end_matches('/').to_string(),
                        audience,
                        advert,
                    )
                })
            })
            .collect()
    }

    async fn probe(&self, r: &PlaybookRef) -> AppResult<Located> {
        let key =
            core::probe_key(r).ok_or_else(|| AppError::Validation("nothing to probe".into()))?;
        let cands = self.candidates();
        let maybe: Vec<_> = cands
            .iter()
            .filter(|(_, _, _, a)| a.may_hold(&key))
            .collect();
        let reference = ref_string(r);
        let expect = match r {
            PlaybookRef::Digest { digest } => Some(digest.clone()),
            PlaybookRef::Name {
                version: core::VersionSel::Digest(d),
                ..
            } => Some(d.clone()),
            _ => None,
        };
        let mut queried = 0;
        let mut last_err = None;
        for (id, endpoint, audience, _) in maybe.iter().take(MAX_PEER_QUERIES) {
            queried += 1;
            match self.ask(endpoint, audience, &reference).await {
                Ok(a) if &a.instance_id == id => {
                    let mut l = self.import_answer(a, expect.as_deref()).await?;
                    (l.peers_considered, l.peers_maybe, l.peers_queried) =
                        (cands.len(), maybe.len(), queried);
                    return Ok(l);
                }
                Ok(a) => {
                    last_err = Some(format!(
                        "{endpoint} answered as {}, registered as {id}",
                        a.instance_id
                    ))
                }
                Err(e) => last_err = Some(e.to_string()),
            }
        }
        crate::metrics::record_instance("catalog_locate_miss");
        Err(AppError::NotFound(format!(
            "{reference}: not local; {} live peer(s), {} bloom maybe, {queried} queried{}",
            cands.len(),
            maybe.len(),
            last_err.map(|e| format!("; last: {e}")).unwrap_or_default()
        )))
    }

    async fn ask(&self, endpoint: &str, audience: &str, reference: &str) -> AppResult<PeerAnswer> {
        let (status, v) = self
            .signed(
                endpoint,
                "/api/catalog/peer/resolve",
                "catalog.resolve",
                audience,
                json!({ "ref": reference }),
            )
            .await
            .map_err(AppError::ExternalService)?;
        if status == 404 {
            return Err(AppError::NotFound(format!(
                "{endpoint}: {}",
                v["error"].as_str().unwrap_or("not held")
            )));
        }
        if !(200..300).contains(&status) {
            return Err(AppError::ExternalService(format!(
                "{endpoint}: HTTP {status}: {v}"
            )));
        }
        serde_json::from_value(v)
            .map_err(|e| AppError::ExternalService(format!("{endpoint}: bad answer: {e}")))
    }

    /// Verify a peer's answer and import it under `peer/<id>/<path>`.
    async fn import_answer(&self, a: PeerAnswer, expect: Option<&str>) -> AppResult<Located> {
        let digest = core::verify_fetched(&a.content, Some(&a.digest))
            .map_err(|e| AppError::ExternalService(e.to_string()))?;
        if let Some(e) = expect {
            if e != digest {
                return Err(AppError::ExternalService(format!(
                    "peer body is {digest}, asked for {e}"
                )));
            }
        }
        let source = format!("noetl://{}/{}@{}", a.instance_id, a.path, a.version);
        let path = core::import_path(&a.instance_id, &a.path);
        let (catalog_id, version) =
            import(&self.pool, &path, &a.content, &digest, &source, "peer").await?;
        crate::metrics::record_instance("catalog_imported");
        Ok(Located {
            found_at: "peer".into(),
            catalog_id: catalog_id.to_string(),
            path,
            version: version as i32,
            digest,
            instance_id: Some(a.instance_id),
            source: Some(source),
            peers_considered: 0,
            peers_maybe: 0,
            peers_queried: 0,
        })
    }

    async fn fetch_source(&self, uri: &str, expect: Option<&str>) -> AppResult<Located> {
        if !self.cfg.uri_allowed(uri) {
            return Err(AppError::Forbidden(format!(
                "{uri}: direct fetch needs {URI_FETCH_ENV}=true and a matching {URI_ALLOW_ENV} glob"
            )));
        }
        let content = fetch_uri(&self.http, uri).await?;
        let digest = core::verify_fetched(&content, expect)
            .map_err(|e| AppError::ExternalService(e.to_string()))?;
        let (_, path, _) = crate::services::catalog::registration_identity(&content, "Playbook")?;
        let local = core::import_path("source", &path);
        let (catalog_id, version) =
            import(&self.pool, &local, &content, &digest, uri, "fetched").await?;
        Ok(Located {
            found_at: "source".into(),
            catalog_id: catalog_id.to_string(),
            path: local,
            version: version as i32,
            digest,
            instance_id: None,
            source: Some(uri.to_string()),
            peers_considered: 0,
            peers_maybe: 0,
            peers_queried: 0,
        })
    }
}

/// The reference string a [`PlaybookRef`] was parsed from (or an equivalent).
pub fn ref_string(r: &PlaybookRef) -> String {
    match r {
        PlaybookRef::Digest { digest } => digest.clone(),
        PlaybookRef::Name { path, version } => match version {
            core::VersionSel::Default => path.clone(),
            core::VersionSel::Latest => format!("{path}@latest"),
            core::VersionSel::Exact(v) => format!("{path}@{v}"),
            core::VersionSel::Digest(d) => format!("{path}@{d}"),
        },
        PlaybookRef::Source { uri, .. } => uri.clone(),
    }
}

/// Insert an imported body under `local_path`, unless that path's latest already
/// has this digest. Runs the same pre-insert checks as a local register.
pub async fn import(
    pool: &DbPool,
    local_path: &str,
    content: &str,
    digest: &str,
    source: &str,
    origin: &str,
) -> AppResult<(i64, i16)> {
    let (yaml, _, kind) = crate::services::catalog::registration_identity(content, "Playbook")?;
    crate::services::catalog::registration_checks(&yaml, &kind)?;
    if kind.eq_ignore_ascii_case("credential") {
        return Err(AppError::Validation(
            "credential resources are never imported".into(),
        ));
    }
    if let Some((row, d)) = cas::latest_with_digest(pool, local_path).await? {
        if d == digest {
            cas::add_source(pool, digest, source, origin, true).await?;
            return Ok((row.catalog_id, row.version));
        }
    }
    let version = crate::db::queries::catalog::get_next_version(pool, local_path).await?;
    let to_json = |k: &str| {
        yaml.get(k)
            .map(|v| serde_json::to_value(v).unwrap_or(Value::Null))
    };
    let meta = yaml
        .get("metadata")
        .and_then(|m| m.get("labels"))
        .map(|v| serde_json::to_value(v).unwrap_or(Value::Null));
    let catalog_id = crate::db::queries::catalog::insert_catalog_entry(
        pool,
        local_path,
        &kind,
        version,
        content,
        to_json("workflow").as_ref(),
        to_json("workload").as_ref(),
        meta.as_ref(),
    )
    .await?;
    cas::record_registered(pool, catalog_id, local_path, version, content, digest, None).await?;
    cas::add_source(pool, digest, source, origin, true).await?;
    tracing::info!(path = local_path, version, %digest, %source, "catalog import");
    Ok((catalog_id, version))
}

/// Fetch a playbook body from a source URI. Ambient credentials only.
pub async fn fetch_uri(http: &reqwest::Client, uri: &str) -> AppResult<String> {
    let bytes: Vec<u8> = if uri.starts_with("https://") || uri.starts_with("http://") {
        let r = http
            .get(uri)
            .send()
            .await
            .map_err(|e| AppError::ExternalService(format!("{uri}: {e}")))?;
        if !r.status().is_success() {
            return Err(AppError::ExternalService(format!(
                "{uri}: HTTP {}",
                r.status()
            )));
        }
        r.bytes()
            .await
            .map_err(|e| AppError::ExternalService(e.to_string()))?
            .to_vec()
    } else {
        let (store, path) = object_store_for(uri)?;
        let got = store
            .get(&path)
            .await
            .map_err(|e| AppError::ExternalService(format!("{uri}: {e}")))?;
        if got.meta.size as usize > MAX_BODY_BYTES {
            return Err(AppError::Validation(format!(
                "{uri}: {} bytes, over {MAX_BODY_BYTES}",
                got.meta.size
            )));
        }
        got.bytes()
            .await
            .map_err(|e| AppError::ExternalService(format!("{uri}: {e}")))?
            .to_vec()
    };
    if bytes.len() > MAX_BODY_BYTES {
        return Err(AppError::Validation(format!(
            "{uri}: body over {MAX_BODY_BYTES} bytes"
        )));
    }
    String::from_utf8(bytes).map_err(|_| AppError::Validation(format!("{uri}: body is not UTF-8")))
}

/// Build an `object_store` client and object path for `gs://`, `s3://`, `az://`
/// and `minio+http(s)://host:port/bucket/key`.
pub fn object_store_for(
    uri: &str,
) -> AppResult<(Box<dyn object_store::ObjectStore>, object_store::path::Path)> {
    use object_store::{
        aws::AmazonS3Builder, azure::MicrosoftAzureBuilder, gcp::GoogleCloudStorageBuilder,
    };
    let bad = |m: String| AppError::Validation(m);
    let (scheme, rest) = uri
        .split_once("://")
        .ok_or_else(|| bad(format!("'{uri}' is not a URI")))?;
    let (store, key): (Box<dyn object_store::ObjectStore>, &str) = match scheme {
        "minio+http" | "minio+https" => {
            let (host, rest) = rest
                .split_once('/')
                .ok_or_else(|| bad(format!("'{uri}' has no bucket")))?;
            let (bucket, key) = rest
                .split_once('/')
                .ok_or_else(|| bad(format!("'{uri}' has no key")))?;
            let proto = if scheme == "minio+http" {
                "http"
            } else {
                "https"
            };
            let s = AmazonS3Builder::from_env()
                .with_endpoint(format!("{proto}://{host}"))
                .with_allow_http(proto == "http")
                .with_virtual_hosted_style_request(false)
                .with_bucket_name(bucket)
                .with_region(std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".into()))
                .build()
                .map_err(|e| bad(format!("{uri}: {e}")))?;
            (Box::new(s), key)
        }
        "s3" | "gs" | "az" => {
            let (bucket, key) = rest
                .split_once('/')
                .ok_or_else(|| bad(format!("'{uri}' has no key")))?;
            let store: Box<dyn object_store::ObjectStore> = match scheme {
                "s3" => Box::new(
                    AmazonS3Builder::from_env()
                        .with_bucket_name(bucket)
                        .build()
                        .map_err(|e| bad(format!("{uri}: {e}")))?,
                ),
                "gs" => Box::new(
                    GoogleCloudStorageBuilder::from_env()
                        .with_bucket_name(bucket)
                        .build()
                        .map_err(|e| bad(format!("{uri}: {e}")))?,
                ),
                _ => Box::new(
                    MicrosoftAzureBuilder::from_env()
                        .with_container_name(bucket)
                        .build()
                        .map_err(|e| bad(format!("{uri}: {e}")))?,
                ),
            };
            (store, key)
        }
        "db" | "ehdb" => {
            return Err(bad(format!(
                "'{scheme}://' sources are TARGET (RFC §9), not implemented"
            )))
        }
        "file" => {
            return Err(bad(
                "file:// sources are for the CLI and daemon, never a server".into(),
            ))
        }
        other => return Err(bad(format!("'{other}://' is not an object store"))),
    };
    Ok((store, object_store::path::Path::from(key)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> PeerConfig {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        PeerConfig::from_lookup(|k| m.get(k).cloned())
    }

    #[test]
    fn peers_parse_with_and_without_an_audience() {
        let c = cfg(&[(PEERS_ENV, "http://a:8082/, https://b.example#cluster-b ,")]);
        assert_eq!(
            c.peers,
            vec![
                Peer {
                    url: "http://a:8082".into(),
                    audience: "noetl".into()
                },
                Peer {
                    url: "https://b.example".into(),
                    audience: "cluster-b".into()
                }
            ]
        );
        assert_eq!(c.advert_secs, 60);
        assert!(!c.discovery);
        assert_eq!(
            cfg(&[("NOETL_PUBLIC_SERVER_URL", "http://s:8082/")])
                .self_url
                .as_deref(),
            Some("http://s:8082")
        );
    }

    #[test]
    fn uri_fetch_fails_closed() {
        assert!(
            !cfg(&[(URI_FETCH_ENV, "true")]).uri_allowed("gs://b/k.yaml"),
            "no allowlist → nothing"
        );
        assert!(
            !cfg(&[(URI_ALLOW_ENV, "gs://b/*")]).uri_allowed("gs://b/k.yaml"),
            "no flag → nothing"
        );
        let c = cfg(&[
            (URI_FETCH_ENV, "true"),
            (URI_ALLOW_ENV, "gs://b/*,minio+http://minio:9000/pb/*"),
        ]);
        assert!(c.uri_allowed("gs://b/k.yaml"));
        assert!(c.uri_allowed("minio+http://minio:9000/pb/x.yaml"));
        assert!(!c.uri_allowed("gs://other/k.yaml"));
    }

    #[test]
    fn the_advert_answers_every_probe_form_and_mostly_misses_others() {
        let d = format!("sha256:{}", "c".repeat(64));
        let rows = vec![
            ("a/b".to_string(), 3, Some(d.clone())),
            ("x/y".to_string(), 1, None),
        ];
        let a = advert_from(&rows, 7);
        assert_eq!((a.generation, a.entries), (7, 2));
        let caps_ok = |k: String| a.may_hold(&k);
        for r in ["a/b", "a/b@3", "a/b@latest", d.as_str(), "x/y", "x/y@1"] {
            assert!(
                caps_ok(core::probe_key(&PlaybookRef::parse(r).unwrap()).unwrap()),
                "{r} must probe positive"
            );
        }
        let misses = (0..500)
            .filter(|i| a.may_hold(&format!("p:absent/{i}")))
            .count();
        assert!(
            misses < 25,
            "false-positive rate {misses}/500 is far above the 1% target"
        );
        // Serialises as the registration carries it, and passes validation.
        let mut c = crate::instances::registry::tests::caps(RuntimeKind::Distributed);
        c.endpoint = Some("http://b:8082".into());
        c.catalog = Some(a);
        c.validate().unwrap();
    }

    #[test]
    fn a_large_catalog_drops_the_version_family_and_says_so() {
        let rows: Vec<_> = (0..10_000).map(|i| (format!("p/{i}"), 1, None)).collect();
        let a = advert_from(&rows, 1);
        assert_eq!(a.families, vec!["p", "d"]);
        assert!(
            !a.may_hold("v:p/1@1"),
            "a dropped family never answers maybe"
        );
        assert!(a.may_hold("p:p/9999"));
    }

    #[test]
    fn object_store_uris_build_or_refuse_by_name() {
        assert!(object_store_for("minio+http://minio:9000/pb/a/b.yaml").is_ok());
        assert!(object_store_for("minio+http://minio:9000/pb").is_err());
        for (u, why) in [
            ("db://alias/s.t", "TARGET"),
            ("ehdb://catalog/1", "TARGET"),
            ("file:///x", "never a server"),
        ] {
            let e = object_store_for(u).err().unwrap().to_string();
            assert!(e.contains(why), "{u}: {e}");
        }
    }

    #[test]
    fn key_files_in_the_cli_format_load_and_a_wrong_id_is_refused() {
        use base64::Engine as _;
        let dir = std::env::temp_dir().join(format!("noetl-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let k = InstanceKey::from_seed([9u8; 32]);
        let seed = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
        let good = dir.join("good.json");
        std::fs::write(
            &good,
            json!({"alg":"ML-DSA-87","instance_id":k.id(),"seed":seed}).to_string(),
        )
        .unwrap();
        assert_eq!(load_key(good.to_str().unwrap()).unwrap().id(), k.id());
        let bad = dir.join("bad.json");
        std::fs::write(
            &bad,
            json!({"instance_id":"ni-00000000000000000000000000000000","seed":seed}).to_string(),
        )
        .unwrap();
        assert!(load_key(bad.to_str().unwrap()).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
