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
//! The kind/emulator path needs no credential (fake-gcs-server is open). Real-GCS
//! auth (GKE workload identity / a bearer token) is a prod-rollout concern wired
//! in a later phase; the emulator path proves the read/write contract on kind.
//! When `NOETL_OBJECT_STORE_GCS_TOKEN` is set it is sent as a bearer token, so a
//! token-bearing deployment works without a code change.

use sha2::{Digest, Sha256};

use crate::db::{queries::object_store, queries::object_store::ObjectRow, DbPool};
use crate::error::{AppError, AppResult};

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
    /// Optional bearer token (empty for the open emulator).
    token: String,
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
                let token = std::env::var("NOETL_OBJECT_STORE_GCS_TOKEN").unwrap_or_default();
                tracing::info!(endpoint = %endpoint, bucket = %bucket, "object store backend: GCS (#104 Phase C)");
                ObjectBackend::Gcs(GcsBackend {
                    client: reqwest::Client::new(),
                    endpoint,
                    bucket,
                    token,
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
        if !self.token.is_empty() {
            req = req.bearer_auth(&self.token);
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
        if !self.token.is_empty() {
            req = req.bearer_auth(&self.token);
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

    // One sequential test: mutating process env from several parallel tests
    // races (the runner shares one process), so all `from_env` assertions live
    // in a single body that fully controls + resets env.
    #[test]
    fn from_env_backend_selection() {
        for k in [
            "NOETL_OBJECT_STORE_BACKEND",
            "NOETL_OBJECT_STORE_GCS_BUCKET",
            "NOETL_OBJECT_STORE_GCS_ENDPOINT",
        ] {
            std::env::remove_var(k);
        }

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

        for k in [
            "NOETL_OBJECT_STORE_BACKEND",
            "NOETL_OBJECT_STORE_GCS_BUCKET",
            "NOETL_OBJECT_STORE_GCS_ENDPOINT",
        ] {
            std::env::remove_var(k);
        }
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
