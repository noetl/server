//! Object-store backend selector ([noetl/ai-meta#104](https://github.com/noetl/ai-meta/issues/104) Phase C).
//!
//! Phase B introduced the server-mediated object endpoint
//! (`PUT/GET /api/internal/objects/{*key}`) backed by Postgres `BYTEA`
//! (`db::queries::object_store`). Phase C wires a **GCS** backend behind the same
//! HTTP contract so the Feather/JSON result tier can live in object store, where
//! it belongs (the RFC §5.3 durable cross-node tier), and the resolve-by-URN read
//! path reads it from there.
//!
//! Selection is by env (`NOETL_OBJECT_STORE_BACKEND`):
//!
//! - **`postgres`** (default) — the Phase B behavior, byte-identical. The bytes
//!   live in `noetl.object_store`. Prod/default is unchanged.
//! - **`gcs`** — the bytes live in a GCS bucket. The server talks to GCS (or a
//!   GCS-compatible endpoint — a [fake-gcs-server] emulator on kind) over the
//!   GCS JSON API via the existing `reqwest` client. No new heavy dependency: the
//!   slim control plane stays slim (no `gcp_auth`, no `object_store` crate).
//!
//! The endpoint is **server-mediated** end to end
//! ([data-access-boundary.md](https://github.com/noetl/ai-meta/blob/main/agents/rules/data-access-boundary.md)):
//! workers never reach GCS directly — they `PUT`/`GET` through the server, which
//! holds the bucket binding and (in prod) the workload-identity credential.
//!
//! [fake-gcs-server]: https://github.com/fsouza/fake-gcs-server
//!
//! ## Auth
//!
//! The GCS backend resolves one of three auth modes at startup
//! ([noetl/ai-meta#104](https://github.com/noetl/ai-meta/issues/104)). The mode
//! is chosen by `NOETL_OBJECT_STORE_GCS_AUTH` (default `auto`):
//!
//! | Mode | When (`auto`) | Behavior |
//! | :-- | :-- | :-- |
//! | `none` | endpoint is **not** a `googleapis.com` host (the fake-gcs-server emulator on kind) | No `Authorization` header. The emulator is open; kind validation runs unchanged. |
//! | `static` | `NOETL_OBJECT_STORE_GCS_TOKEN` is set | The configured token rides as a bearer (explicit override — e.g. a short-lived token injected by a sidecar). |
//! | `adc` | real GCS (`storage.googleapis.com`) with no static token | Mints + **auto-refreshes** a short-lived OAuth token from Workload Identity / Application Default Credentials, scope `devstorage.read_write`. The prod path: the bucket enforces public-access-prevention and the server runs under a WI-bound KSA. |
//!
//! `auto` resolution order: a non-empty `NOETL_OBJECT_STORE_GCS_TOKEN` wins
//! (`static`); else a real-GCS endpoint selects `adc`; else (custom/emulator
//! endpoint) `none`. Set `NOETL_OBJECT_STORE_GCS_AUTH` to `none` / `static` /
//! `adc` to force a mode (e.g. `adc` against a non-default private-google
//! endpoint).
//!
//! The ADC token is held by [`gcp_auth`]'s `TokenProvider`, which caches the
//! token and refreshes it before expiry — the backend asks for a token per
//! request but does **not** mint per request. The provider is initialized lazily
//! on first GCS I/O so startup stays free of a credential round-trip.
//!
//! [`gcp_auth`]: https://docs.rs/gcp_auth

use std::sync::Arc;

use sha2::{Digest, Sha256};
use tokio::sync::RwLock;

use crate::db::{queries::object_store, queries::object_store::ObjectRow, DbPool};
use crate::error::{AppError, AppResult};
use crate::metrics;

/// OAuth scope for read/write access to GCS objects (the result tier `PUT`s and
/// `GET`s object bytes). Narrower than `cloud-platform` — the server only needs
/// object read/write on its one results bucket.
const GCS_RW_SCOPE: &[&str] = &["https://www.googleapis.com/auth/devstorage.read_write"];

/// The resolved object-store backend (built once at startup from env).
#[derive(Clone)]
pub enum ObjectBackend {
    /// Bytes in `noetl.object_store` (Phase B default; prod/default behavior).
    Postgres,
    /// Bytes in a GCS bucket (or GCS-compatible emulator) via the JSON API.
    Gcs(GcsBackend),
}

/// GCS (or GCS-compatible) backend configuration.
#[derive(Clone)]
pub struct GcsBackend {
    client: reqwest::Client,
    /// Base endpoint, no trailing slash (`https://storage.googleapis.com` for
    /// real GCS, `http://fake-gcs-server:4443` for the kind emulator).
    endpoint: String,
    /// Target bucket.
    bucket: String,
    /// Resolved auth mode (no-auth emulator | static token | ADC/WI).
    auth: GcsAuth,
}

/// How the GCS backend authenticates each request.
#[derive(Clone)]
enum GcsAuth {
    /// No credential — the open fake-gcs-server emulator on kind.
    None,
    /// Explicit static bearer token (`NOETL_OBJECT_STORE_GCS_TOKEN`).
    Static(String),
    /// Workload Identity / Application Default Credentials — mints + refreshes a
    /// short-lived OAuth token via [`gcp_auth`]. The prod path.
    Adc(AdcAuth),
}

/// Lazily-initialized ADC token source. The [`gcp_auth`] provider is created on
/// first use and caches + auto-refreshes the token internally, so this holds it
/// behind a shared `RwLock` (cloning the backend shares one provider + one
/// cache). Mirrors the worker/tools `GcpAuth` wrapper.
#[derive(Clone)]
struct AdcAuth {
    provider: Arc<RwLock<Option<Arc<dyn gcp_auth::TokenProvider>>>>,
}

impl AdcAuth {
    fn new() -> Self {
        Self {
            provider: Arc::new(RwLock::new(None)),
        }
    }

    /// Resolve a bearer token for the GCS read/write scope. The provider serves
    /// from its internal cache when the token is still fresh and refreshes it
    /// transparently before expiry — so this is safe (and intended) to call per
    /// request without minting per request.
    async fn token(&self) -> AppResult<String> {
        // Fast path: provider already initialized.
        {
            let guard = self.provider.read().await;
            if let Some(provider) = guard.as_ref() {
                return Self::fetch(provider).await;
            }
        }
        // Initialize once. Two callers racing here both call `gcp_auth::provider()`
        // and the last write wins; both refer to the same ADC source, so the
        // observable result is identical and the cost is one extra init at most.
        let provider = gcp_auth::provider().await.map_err(|e| {
            metrics::record_object_store_gcs_auth("adc", "error");
            AppError::Internal(format!("gcs adc provider init: {e}"))
        })?;
        {
            let mut guard = self.provider.write().await;
            *guard = Some(provider);
        }
        let guard = self.provider.read().await;
        let provider = guard
            .as_ref()
            .ok_or_else(|| AppError::Internal("gcs adc provider missing after init".to_string()))?;
        Self::fetch(provider).await
    }

    async fn fetch(provider: &Arc<dyn gcp_auth::TokenProvider>) -> AppResult<String> {
        match provider.token(GCS_RW_SCOPE).await {
            Ok(tok) => {
                metrics::record_object_store_gcs_auth("adc", "acquired");
                Ok(tok.as_str().to_string())
            }
            Err(e) => {
                metrics::record_object_store_gcs_auth("adc", "error");
                Err(AppError::Internal(format!("gcs adc token: {e}")))
            }
        }
    }
}

impl GcsAuth {
    /// Resolve the auth mode from env, given the already-resolved endpoint.
    ///
    /// `NOETL_OBJECT_STORE_GCS_AUTH` (`auto` default) selects the mode; in `auto`
    /// a non-empty static token wins, else a real-GCS endpoint → `adc`, else
    /// (custom/emulator endpoint) → `none`.
    fn from_env(endpoint: &str) -> Self {
        let token = std::env::var("NOETL_OBJECT_STORE_GCS_TOKEN").unwrap_or_default();
        let mode = std::env::var("NOETL_OBJECT_STORE_GCS_AUTH")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        match mode.as_str() {
            "none" => GcsAuth::None,
            "static" => {
                if token.trim().is_empty() {
                    tracing::warn!(
                        "NOETL_OBJECT_STORE_GCS_AUTH=static but NOETL_OBJECT_STORE_GCS_TOKEN is \
                         empty; GCS requests will be sent without a bearer token"
                    );
                    GcsAuth::None
                } else {
                    GcsAuth::Static(token)
                }
            }
            "adc" => GcsAuth::Adc(AdcAuth::new()),
            // "auto" (default) or any unrecognized value.
            _ => {
                if !token.trim().is_empty() {
                    GcsAuth::Static(token)
                } else if is_real_gcs(endpoint) {
                    GcsAuth::Adc(AdcAuth::new())
                } else {
                    GcsAuth::None
                }
            }
        }
    }

    /// Stable label for logs/tests.
    fn label(&self) -> &'static str {
        match self {
            GcsAuth::None => "none",
            GcsAuth::Static(_) => "static",
            GcsAuth::Adc(_) => "adc",
        }
    }

    /// The bearer token to attach to a request, or `None` for the no-auth path.
    async fn bearer_token(&self) -> AppResult<Option<String>> {
        match self {
            GcsAuth::None => Ok(None),
            GcsAuth::Static(t) if t.is_empty() => Ok(None),
            GcsAuth::Static(t) => Ok(Some(t.clone())),
            GcsAuth::Adc(a) => Ok(Some(a.token().await?)),
        }
    }
}

/// True when `endpoint` points at real Google Cloud Storage (any
/// `*.googleapis.com` host), i.e. the prod path that needs a credential. A custom
/// endpoint (the kind fake-gcs-server) is treated as the open emulator.
fn is_real_gcs(endpoint: &str) -> bool {
    let host = endpoint
        .split("://")
        .nth(1)
        .unwrap_or(endpoint)
        .split('/')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    host == "googleapis.com" || host.ends_with(".googleapis.com")
}

impl ObjectBackend {
    /// Build the backend from env. Defaults to `Postgres` (Phase B behavior).
    /// A `gcs` selection with no bucket falls back to `Postgres` with a WARN so a
    /// misconfiguration degrades to the durable default rather than failing
    /// object I/O outright.
    pub fn from_env() -> Self {
        let backend = std::env::var("NOETL_OBJECT_STORE_BACKEND")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        match backend.as_str() {
            "gcs" => {
                let bucket = std::env::var("NOETL_OBJECT_STORE_GCS_BUCKET")
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                if bucket.is_empty() {
                    tracing::warn!(
                        "NOETL_OBJECT_STORE_BACKEND=gcs but NOETL_OBJECT_STORE_GCS_BUCKET is \
                         unset; falling back to the Postgres object backend"
                    );
                    return ObjectBackend::Postgres;
                }
                let endpoint = std::env::var("NOETL_OBJECT_STORE_GCS_ENDPOINT")
                    .unwrap_or_else(|_| "https://storage.googleapis.com".to_string())
                    .trim_end_matches('/')
                    .to_string();
                let auth = GcsAuth::from_env(&endpoint);
                tracing::info!(
                    endpoint = %endpoint,
                    bucket = %bucket,
                    auth = %auth.label(),
                    "object store backend: GCS (#104 Phase C)"
                );
                ObjectBackend::Gcs(GcsBackend {
                    client: reqwest::Client::new(),
                    endpoint,
                    bucket,
                    auth,
                })
            }
            _ => ObjectBackend::Postgres,
        }
    }

    /// Stable label for metrics / logs.
    pub fn label(&self) -> &'static str {
        match self {
            ObjectBackend::Postgres => "postgres",
            ObjectBackend::Gcs(_) => "gcs",
        }
    }

    /// Store the object at `key`. Idempotent overwrite (content-addressed §7 key).
    pub async fn put(
        &self,
        pool: &DbPool,
        key: &str,
        digest: &str,
        media_type: &str,
        bytes: &[u8],
    ) -> AppResult<()> {
        match self {
            ObjectBackend::Postgres => object_store::put(pool, key, digest, media_type, bytes).await,
            ObjectBackend::Gcs(g) => g.put(key, media_type, bytes).await,
        }
    }

    /// Fetch the object at `key`, or `None` (caller → HTTP 404).
    pub async fn get(&self, pool: &DbPool, key: &str) -> AppResult<Option<ObjectRow>> {
        match self {
            ObjectBackend::Postgres => object_store::get(pool, key).await,
            ObjectBackend::Gcs(g) => g.get(key).await,
        }
    }

    /// List object keys under `prefix`, capped at `limit`. Backs the result-tier
    /// GC sweep ([noetl/ai-meta#104](https://github.com/noetl/ai-meta/issues/104)
    /// Phase F). The ordering is best-effort per backend (Postgres → newest
    /// first; GCS → lexicographic, the JSON-API default); the GC sweep does not
    /// depend on order, only on coverage up to `limit`.
    pub async fn list(&self, pool: &DbPool, prefix: &str, limit: usize) -> AppResult<Vec<String>> {
        match self {
            ObjectBackend::Postgres => object_store::list_keys(pool, prefix, limit as i64).await,
            ObjectBackend::Gcs(g) => g.list(prefix, limit).await,
        }
    }

    /// Delete the object at `key`. Idempotent: a missing key is `Ok(false)`, not
    /// an error. Backs the result-tier GC sweep
    /// ([noetl/ai-meta#104](https://github.com/noetl/ai-meta/issues/104) Phase F).
    pub async fn delete(&self, pool: &DbPool, key: &str) -> AppResult<bool> {
        match self {
            ObjectBackend::Postgres => object_store::delete(pool, key).await,
            ObjectBackend::Gcs(g) => g.delete(key).await,
        }
    }
}

impl GcsBackend {
    /// Upload via the GCS JSON API `uploadType=media` (the object name rides as a
    /// query param, so `reqwest` URL-encodes it). Works against real GCS and the
    /// fake-gcs-server emulator alike.
    async fn put(&self, key: &str, media_type: &str, bytes: &[u8]) -> AppResult<()> {
        let url = format!("{}/upload/storage/v1/b/{}/o", self.endpoint, self.bucket);
        let mut req = self
            .client
            .post(&url)
            .query(&[("uploadType", "media"), ("name", key)])
            .header(reqwest::header::CONTENT_TYPE, media_type)
            .body(bytes.to_vec());
        if let Some(token) = self.auth.bearer_token().await? {
            req = req.bearer_auth(token);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| AppError::Internal(format!("gcs object put {key}: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AppError::Internal(format!(
                "gcs object put {key}: HTTP {} {}",
                status.as_u16(),
                body
            )));
        }
        Ok(())
    }

    /// Download via the GCS JSON API `alt=media`. The object name is a path
    /// segment, so it is percent-encoded (slashes become `%2F`). Returns `None`
    /// on 404 (so the resolver falls back fail-safe), errors on other non-2xx.
    async fn get(&self, key: &str) -> AppResult<Option<ObjectRow>> {
        let url = format!(
            "{}/storage/v1/b/{}/o/{}",
            self.endpoint,
            self.bucket,
            percent_encode_segment(key)
        );
        let mut req = self.client.get(&url).query(&[("alt", "media")]);
        if let Some(token) = self.auth.bearer_token().await? {
            req = req.bearer_auth(token);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| AppError::Internal(format!("gcs object get {key}: {e}")))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AppError::Internal(format!(
                "gcs object get {key}: HTTP {} {}",
                status.as_u16(),
                body
            )));
        }
        let media_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| AppError::Internal(format!("gcs object get {key} body: {e}")))?
            .to_vec();
        let digest = hex::encode(Sha256::digest(&bytes));
        Ok(Some(ObjectRow {
            digest,
            media_type,
            bytes,
        }))
    }

    /// List object names under `prefix` via the GCS JSON API
    /// (`GET …/o?prefix=…&maxResults=…`), following `nextPageToken` until `limit`
    /// keys are collected or the listing is exhausted. The `prefix` rides as a
    /// query param (the client URL-encodes it). Works against real GCS and the
    /// fake-gcs-server emulator alike.
    async fn list(&self, prefix: &str, limit: usize) -> AppResult<Vec<String>> {
        let url = format!("{}/storage/v1/b/{}/o", self.endpoint, self.bucket);
        let mut keys: Vec<String> = Vec::new();
        let mut page_token: Option<String> = None;
        // Bound the page walk so a pathological listing can't loop forever.
        for _ in 0..1000 {
            let remaining = limit.saturating_sub(keys.len());
            if remaining == 0 {
                break;
            }
            let max_results = remaining.min(1000).to_string();
            let mut query: Vec<(&str, &str)> = vec![("prefix", prefix), ("maxResults", &max_results)];
            if let Some(tok) = page_token.as_deref() {
                query.push(("pageToken", tok));
            }
            let mut req = self.client.get(&url).query(&query);
            if let Some(token) = self.auth.bearer_token().await? {
                req = req.bearer_auth(token);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| AppError::Internal(format!("gcs object list {prefix}: {e}")))?;
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                return Err(AppError::Internal(format!(
                    "gcs object list {prefix}: HTTP {} {}",
                    status.as_u16(),
                    body
                )));
            }
            let page: GcsListPage = resp
                .json()
                .await
                .map_err(|e| AppError::Internal(format!("gcs object list {prefix} decode: {e}")))?;
            for item in page.items {
                keys.push(item.name);
                if keys.len() >= limit {
                    break;
                }
            }
            match page.next_page_token {
                Some(tok) if keys.len() < limit => page_token = Some(tok),
                _ => break,
            }
        }
        Ok(keys)
    }

    /// Delete the object at `key` via the GCS JSON API
    /// (`DELETE …/o/<percent-encoded-key>`). Idempotent: a 404 (already gone) is
    /// `Ok(false)`; a 2xx is `Ok(true)`; other non-2xx errors.
    async fn delete(&self, key: &str) -> AppResult<bool> {
        let url = format!(
            "{}/storage/v1/b/{}/o/{}",
            self.endpoint,
            self.bucket,
            percent_encode_segment(key)
        );
        let mut req = self.client.delete(&url);
        if let Some(token) = self.auth.bearer_token().await? {
            req = req.bearer_auth(token);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| AppError::Internal(format!("gcs object delete {key}: {e}")))?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(false);
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(AppError::Internal(format!(
                "gcs object delete {key}: HTTP {} {}",
                status.as_u16(),
                body
            )));
        }
        Ok(true)
    }
}

/// One page of a GCS JSON-API object listing (`…/o` response).
#[derive(serde::Deserialize)]
struct GcsListPage {
    #[serde(default)]
    items: Vec<GcsListItem>,
    #[serde(rename = "nextPageToken", default)]
    next_page_token: Option<String>,
}

#[derive(serde::Deserialize)]
struct GcsListItem {
    name: String,
}

/// Percent-encode one path segment per RFC 3986 (encode everything but the
/// unreserved set `A-Za-z0-9-._~`), so a slash-bearing §7 object key becomes a
/// single GCS JSON-API path segment (`/` → `%2F`, `=` → `%3D`).
fn percent_encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{b:02X}"));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENV_KEYS: &[&str] = &[
        "NOETL_OBJECT_STORE_BACKEND",
        "NOETL_OBJECT_STORE_GCS_BUCKET",
        "NOETL_OBJECT_STORE_GCS_ENDPOINT",
        "NOETL_OBJECT_STORE_GCS_TOKEN",
        "NOETL_OBJECT_STORE_GCS_AUTH",
    ];

    fn clear_env() {
        for k in ENV_KEYS {
            std::env::remove_var(k);
        }
    }

    fn gcs_auth_label() -> &'static str {
        match ObjectBackend::from_env() {
            ObjectBackend::Gcs(g) => g.auth.label(),
            _ => panic!("expected gcs backend"),
        }
    }

    // One sequential test: mutating process env from several parallel tests
    // races (the runner shares one process), so all `from_env` assertions live
    // in a single body that fully controls + resets env.
    #[test]
    fn from_env_backend_selection() {
        clear_env();

        // Default → Postgres (prod/default behavior).
        assert_eq!(ObjectBackend::from_env().label(), "postgres");

        // gcs requested but no bucket → fail-safe to Postgres.
        std::env::set_var("NOETL_OBJECT_STORE_BACKEND", "gcs");
        assert_eq!(ObjectBackend::from_env().label(), "postgres");

        // gcs + bucket → GCS, endpoint trailing slash trimmed.
        std::env::set_var("NOETL_OBJECT_STORE_GCS_BUCKET", "noetl-results");
        std::env::set_var("NOETL_OBJECT_STORE_GCS_ENDPOINT", "http://fake-gcs:4443/");
        match ObjectBackend::from_env() {
            ObjectBackend::Gcs(g) => {
                assert_eq!(g.endpoint, "http://fake-gcs:4443");
                assert_eq!(g.bucket, "noetl-results");
            }
            _ => panic!("expected gcs"),
        }

        // ── Auth-mode selection (same sequential body to avoid env races) ──
        // emulator endpoint → none, static token → static, real-GCS → adc,
        // plus the explicit override knob.
        std::env::set_var("NOETL_OBJECT_STORE_BACKEND", "gcs");
        std::env::set_var("NOETL_OBJECT_STORE_GCS_BUCKET", "noetl-results");

        // auto + custom (emulator) endpoint, no token → none (kind path).
        std::env::set_var("NOETL_OBJECT_STORE_GCS_ENDPOINT", "http://fake-gcs:4443");
        assert_eq!(gcs_auth_label(), "none");

        // auto + static token set → static (explicit override wins).
        std::env::set_var("NOETL_OBJECT_STORE_GCS_TOKEN", "ya29.test-token");
        assert_eq!(gcs_auth_label(), "static");
        std::env::remove_var("NOETL_OBJECT_STORE_GCS_TOKEN");

        // auto + real-GCS endpoint (default), no token → adc (prod path).
        std::env::remove_var("NOETL_OBJECT_STORE_GCS_ENDPOINT");
        assert_eq!(gcs_auth_label(), "adc");

        // Override: force adc against a non-default endpoint.
        std::env::set_var("NOETL_OBJECT_STORE_GCS_ENDPOINT", "http://fake-gcs:4443");
        std::env::set_var("NOETL_OBJECT_STORE_GCS_AUTH", "adc");
        assert_eq!(gcs_auth_label(), "adc");

        // Override: force none even with a token set.
        std::env::set_var("NOETL_OBJECT_STORE_GCS_TOKEN", "ya29.test-token");
        std::env::set_var("NOETL_OBJECT_STORE_GCS_AUTH", "none");
        assert_eq!(gcs_auth_label(), "none");

        // Override: static with an empty token degrades to none (with WARN).
        std::env::remove_var("NOETL_OBJECT_STORE_GCS_TOKEN");
        std::env::set_var("NOETL_OBJECT_STORE_GCS_AUTH", "static");
        assert_eq!(gcs_auth_label(), "none");

        clear_env();
    }

    #[test]
    fn is_real_gcs_detection() {
        assert!(is_real_gcs("https://storage.googleapis.com"));
        assert!(is_real_gcs("https://storage.googleapis.com/"));
        assert!(is_real_gcs("https://us-central1-storage.googleapis.com"));
        assert!(!is_real_gcs("http://fake-gcs:4443"));
        assert!(!is_real_gcs("http://fake-gcs-server:4443/storage"));
        assert!(!is_real_gcs("http://localhost:4443"));
        // A look-alike host must not be treated as real GCS.
        assert!(!is_real_gcs("https://googleapis.com.evil.example"));
    }

    // The static/no-auth bearer-token resolution (the paths kind exercises) is
    // unchanged in shape: none → no header, static → the token, empty static →
    // no header. ADC needs a live metadata server, so it is covered by the
    // selection test + verified live at prod enablement.
    #[tokio::test]
    async fn bearer_token_static_and_none_paths() {
        assert_eq!(GcsAuth::None.bearer_token().await.unwrap(), None);
        assert_eq!(
            GcsAuth::Static("ya29.tok".to_string())
                .bearer_token()
                .await
                .unwrap(),
            Some("ya29.tok".to_string())
        );
        assert_eq!(
            GcsAuth::Static(String::new()).bearer_token().await.unwrap(),
            None
        );
    }

    // The ADC source is lazy (no provider until first use) and cloning the
    // backend shares ONE provider + ONE token cache — so refresh is process-wide,
    // not per-request. Proven structurally without a network round-trip.
    #[tokio::test]
    async fn adc_provider_is_lazy_and_shared() {
        let a = AdcAuth::new();
        assert!(
            a.provider.read().await.is_none(),
            "provider must not initialize until first token request"
        );
        let b = a.clone();
        assert!(
            Arc::ptr_eq(&a.provider, &b.provider),
            "clones must share one provider/cache so refresh is shared"
        );
    }

    #[test]
    fn percent_encodes_physical_key_segment() {
        let key = "noetl/env=dev/region=local/cell=local-0/shard=s0053/results/start/0/0/1.feather";
        let enc = percent_encode_segment(key);
        assert!(enc.contains("%2F"), "slashes encoded: {enc}");
        assert!(enc.contains("%3D"), "equals encoded: {enc}");
        assert!(!enc.contains('/'), "no raw slash: {enc}");
        // unreserved chars (letters, digits, `.`, `-`) pass through verbatim
        assert!(enc.ends_with("1.feather"), "tail unreserved: {enc}");
    }
}
