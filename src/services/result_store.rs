//! Result-store service (Result Store MVP,
//! [`noetl/ai-meta#70`](https://github.com/noetl/ai-meta/issues/70)).
//!
//! Wraps `db::queries::result_store` with the business logic that
//! the PUT handler needs: snowflake id generation, byte count +
//! SHA-256 computation, URI construction, and the resolve path.
//!
//! Pattern mirrors `services::secret_audit` (mock-sink tests,
//! struct-level Clone).

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::db::queries::result_store::{self as queries, ResultStoreRow};
use crate::db::DbPool;
use crate::error::{AppError, AppResult};
use crate::snowflake::SnowflakeGenerator;

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// PUT /api/result/{execution_id} request body.
///
/// Wire shape matches the worker's `put_result` call site:
/// `repos/worker/src/client/control_plane.rs:557–594`.
/// Fields beyond `{name, data, scope, source_step}` (e.g. `store`,
/// `ttl`, `correlation`, `compress`) are accepted but silently
/// ignored in the MVP — they will be honoured as tier/GC support is
/// added.
#[derive(Debug, Deserialize)]
pub struct PutResultBody {
    /// Logical name for the result (usually the step name).
    pub name: String,
    /// Arbitrary JSON value to store.
    pub data: serde_json::Value,
    /// Lifecycle scope.  The worker always sends `"execution"`;
    /// any short string is accepted.
    #[serde(default = "default_scope")]
    pub scope: String,
    /// Step that produced this result (optional).
    pub source_step: Option<String>,
    // Accepted but not yet implemented:
    #[serde(default)]
    pub store: Option<String>,
    #[serde(default)]
    pub ttl: Option<String>,
    #[serde(default)]
    pub correlation: Option<serde_json::Value>,
    #[serde(default)]
    pub compress: bool,
}

fn default_scope() -> String {
    "execution".to_string()
}

/// PUT response — mirrors the Python `ResultPutResponse` and the
/// worker's `ResultPutResponse` struct in
/// `repos/worker/src/client/control_plane.rs:34–50`.
#[derive(Debug, Serialize)]
pub struct ResultPutResponse {
    /// `noetl://execution/<eid>/result/<name>/<id>` URI.
    pub r#ref: String,
    /// Storage tier used (`"db"` for the MVP single-tier path).
    pub store: String,
    /// Lifecycle scope echoed from the request.
    pub scope: String,
    /// Serialised size in bytes.
    pub bytes: u64,
    /// SHA-256 hex digest of the serialised JSON.
    pub sha256: Option<String>,
    /// Expiry — always `null` in the MVP.
    pub expires_at: Option<String>,
}

/// Parsed components of a `noetl://execution/<eid>/result/<name>/<id>` URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoetlRef {
    pub execution_id: i64,
    pub name: String,
    pub result_id: i64,
}

// ---------------------------------------------------------------------------
// URI parser
// ---------------------------------------------------------------------------

/// Parse a `noetl://execution/<eid>/result/<name>/<id>` URI into
/// typed parts.
///
/// Returns an error on any structural mismatch so the handler can
/// respond 400 rather than propagating a panic.
///
/// ```
/// # use noetl_server::services::result_store::parse_noetl_ref;
/// let r = parse_noetl_ref("noetl://execution/123/result/my_step/456").unwrap();
/// assert_eq!(r.execution_id, 123);
/// assert_eq!(r.name, "my_step");
/// assert_eq!(r.result_id, 456);
/// ```
pub fn parse_noetl_ref(s: &str) -> Result<NoetlRef, String> {
    // Expected: noetl://execution/<eid>/result/<name>/<id>
    let path = s
        .strip_prefix("noetl://")
        .ok_or_else(|| format!("URI must start with 'noetl://', got: {s:?}"))?;

    let parts: Vec<&str> = path.split('/').collect();
    // parts: ["execution", "<eid>", "result", "<name>", "<id>"]
    if parts.len() < 5 {
        return Err(format!(
            "URI must have 5 path segments after 'noetl://', got {}: {s:?}",
            parts.len()
        ));
    }
    if parts[0] != "execution" {
        return Err(format!(
            "First path segment must be 'execution', got {:?} in {s:?}",
            parts[0]
        ));
    }
    if parts[2] != "result" {
        return Err(format!(
            "Third path segment must be 'result', got {:?} in {s:?}",
            parts[2]
        ));
    }

    let execution_id = parts[1]
        .parse::<i64>()
        .map_err(|_| format!("execution_id segment {:?} is not an i64 in {s:?}", parts[1]))?;

    // name may contain slashes in future — for now the worker emits
    // simple step names; join everything between index 3 and the last
    // segment to be forward-compatible.
    if parts.len() < 5 {
        return Err(format!("URI missing result_id segment: {s:?}"));
    }
    let result_id_str = parts[parts.len() - 1];
    let result_id = result_id_str
        .parse::<i64>()
        .map_err(|_| format!("result_id segment {:?} is not an i64 in {s:?}", result_id_str))?;

    // Name is everything between index 3 and the last segment.
    let name = parts[3..parts.len() - 1].join("/");
    if name.is_empty() {
        return Err(format!("name segment is empty in URI {s:?}"));
    }

    Ok(NoetlRef {
        execution_id,
        name,
        result_id,
    })
}

// ---------------------------------------------------------------------------
// Canonical-or-legacy result reference (noetl/ai-meta#104 Phase A)
// ---------------------------------------------------------------------------

/// A `noetl://` result reference in either shape the platform now produces.
///
/// The worker stamps the **canonical** logical Resource Locator
/// (`reference.uri`) additively on over-budget references (noetl/ai-meta#104
/// R02b), while the server still mints + resolves the **legacy** execution ref
/// (`reference.ref`).  Phase A teaches the server to *accept* both via the
/// shared [`noetl_tools::locator`] implementation so later phases can migrate
/// consumption without a flag-day rename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultRef {
    /// Legacy `noetl://execution/<eid>/result/<name>/<id>` — what the server
    /// mints today and what the resolve path keys off (`result_id` is the
    /// `noetl.result_store` row id).
    Legacy(NoetlRef),
    /// Canonical `noetl://<tenant>/<project>/results/<eid>/<step>/<frame>/<row>/<attempt>`
    /// — the stable, location-independent logical name from
    /// [`noetl_tools::locator::ResourceLocator`].
    Canonical(noetl_tools::locator::ResourceLocator),
}

impl ResultRef {
    /// The shape label recorded on `noetl_result_uri_accept_total{outcome}`.
    pub fn shape(&self) -> &'static str {
        match self {
            ResultRef::Legacy(_) => "legacy",
            ResultRef::Canonical(_) => "canonical",
        }
    }
}

/// Parse a `noetl://` result reference, **accepting both** the legacy
/// execution-scoped shape and the canonical logical Resource Locator
/// (noetl/ai-meta#104 Phase A).
///
/// Routes on the first path segment: `execution/...` is the legacy shape
/// (detected via [`noetl_tools::locator::is_legacy_execution_ref`]) and keeps
/// the server's own richer legacy parse (which extracts the numeric
/// `result_id` the resolve path needs — the locator's legacy parse is a
/// structural check only).  Anything else is parsed as the canonical
/// `<tenant>/<project>/<kind>/<logical_path>` locator.
///
/// Returns a human-readable error string on a structural mismatch so the
/// caller can log + count without panicking — Phase A never fails an event on
/// a bad reference.
pub fn parse_result_ref(uri: &str) -> Result<ResultRef, String> {
    use noetl_tools::locator::{is_legacy_execution_ref, ResourceLocator};

    if is_legacy_execution_ref(uri) {
        return parse_noetl_ref(uri).map(ResultRef::Legacy);
    }
    ResourceLocator::parse(uri)
        .map(ResultRef::Canonical)
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

/// Result-store service — wraps the query layer with id generation
/// and hashing.
#[derive(Clone)]
pub struct ResultStoreService {
    pool: DbPool,
    snowflake: Arc<SnowflakeGenerator>,
}

impl ResultStoreService {
    pub fn new(pool: DbPool, snowflake: Arc<SnowflakeGenerator>) -> Self {
        Self { pool, snowflake }
    }

    /// Store one result and return the minted `ResultPutResponse`.
    ///
    /// Steps:
    /// 1. Serialise `data` → JSON bytes; measure + SHA-256.
    /// 2. Mint a snowflake `result_id`.
    /// 3. Build the `noetl://` URI.
    /// 4. INSERT into `noetl.result_store`.
    /// 5. Return the response.
    pub async fn put(
        &self,
        execution_id: i64,
        body: &PutResultBody,
    ) -> AppResult<ResultPutResponse> {
        // Serialise to measure bytes + compute hash.
        let serialised = serde_json::to_vec(&body.data)
            .map_err(|e| AppError::Internal(format!("result_store.put: serialise: {e}")))?;
        let bytes = serialised.len() as i64;
        let sha256_hex = hex::encode(Sha256::digest(&serialised));

        // Mint a fresh snowflake id.
        let result_id = self
            .snowflake
            .generate()
            .map_err(|e| AppError::Internal(format!("result_store.put: snowflake: {e}")))?;

        // Build the noetl:// URI.
        let noetl_ref = format!(
            "noetl://execution/{}/result/{}/{}",
            execution_id, body.name, result_id
        );

        let row = ResultStoreRow {
            result_id,
            execution_id,
            name: body.name.clone(),
            scope: body.scope.clone(),
            source_step: body.source_step.clone(),
            data: body.data.clone(),
            bytes,
            sha256: sha256_hex.clone(),
            media_type: "application/json".to_string(),
            created_at: Utc::now(),
            expires_at: None,
        };

        queries::insert(&self.pool, &row).await?;

        Ok(ResultPutResponse {
            r#ref: noetl_ref,
            store: "db".to_string(),
            scope: body.scope.clone(),
            bytes: bytes as u64,
            sha256: Some(sha256_hex),
            expires_at: None,
        })
    }

    /// Resolve a parsed `NoetlRef` back to the stored `data` JSON.
    ///
    /// Returns the raw `data` JSONB value the caller stored.  The
    /// tools layer (`result_fetch`) expects the response body IS the
    /// data, not a wrapper.
    ///
    /// Returns `None` when no matching row exists (caller maps to 404).
    pub async fn resolve(&self, noetl_ref: &NoetlRef) -> AppResult<Option<serde_json::Value>> {
        let row = queries::get_by_ref(
            &self.pool,
            noetl_ref.execution_id,
            &noetl_ref.name,
            noetl_ref.result_id,
        )
        .await?;
        Ok(row.map(|r| r.data))
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_noetl_ref ---

    #[test]
    fn parses_standard_worker_emit() {
        let r = parse_noetl_ref("noetl://execution/7654321/result/my_step/1234567890")
            .expect("valid URI must parse");
        assert_eq!(r.execution_id, 7654321);
        assert_eq!(r.name, "my_step");
        assert_eq!(r.result_id, 1234567890);
    }

    #[test]
    fn parses_step_name_with_slash() {
        // Forward-compat: name segment may have slashes (future).
        let r = parse_noetl_ref("noetl://execution/1/result/a/b/999")
            .expect("slash in name must parse");
        assert_eq!(r.execution_id, 1);
        assert_eq!(r.name, "a/b");
        assert_eq!(r.result_id, 999);
    }

    #[test]
    fn rejects_wrong_scheme() {
        assert!(parse_noetl_ref("http://execution/1/result/step/2").is_err());
    }

    #[test]
    fn rejects_too_few_segments() {
        assert!(parse_noetl_ref("noetl://execution/1/result").is_err());
    }

    #[test]
    fn rejects_non_numeric_execution_id() {
        assert!(parse_noetl_ref("noetl://execution/abc/result/step/1").is_err());
    }

    #[test]
    fn rejects_non_numeric_result_id() {
        assert!(parse_noetl_ref("noetl://execution/1/result/step/xyz").is_err());
    }

    #[test]
    fn rejects_wrong_first_segment() {
        assert!(parse_noetl_ref("noetl://workflow/1/result/step/2").is_err());
    }

    #[test]
    fn rejects_wrong_third_segment() {
        assert!(parse_noetl_ref("noetl://execution/1/artifact/step/2").is_err());
    }

    // --- parse_result_ref: accepts BOTH shapes (noetl/ai-meta#104 Phase A) ---

    #[test]
    fn result_ref_accepts_legacy_shape() {
        let r = parse_result_ref("noetl://execution/123/result/my_step/456")
            .expect("legacy ref must parse");
        assert_eq!(r.shape(), "legacy");
        match r {
            ResultRef::Legacy(l) => {
                assert_eq!(l.execution_id, 123);
                assert_eq!(l.name, "my_step");
                assert_eq!(l.result_id, 456);
            }
            other => panic!("expected Legacy, got {other:?}"),
        }
    }

    #[test]
    fn result_ref_accepts_canonical_logical_uri() {
        // The exact shape the worker stamps as `reference.uri` (frame 2, row 4
        // cursor body result) — see repos/worker/src/executor/command.rs.
        let uri = "noetl://default/default/results/325/load_next_facility/2/4/1";
        let r = parse_result_ref(uri).expect("canonical URI must parse");
        assert_eq!(r.shape(), "canonical");
        match r {
            ResultRef::Canonical(loc) => {
                assert_eq!(loc.tenant, "default");
                assert_eq!(loc.project, "default");
                assert_eq!(loc.kind, "results");
                assert_eq!(loc.logical_path, "325/load_next_facility/2/4/1");
            }
            other => panic!("expected Canonical, got {other:?}"),
        }
    }

    #[test]
    fn result_ref_canonical_with_tenant_project() {
        let r = parse_result_ref("noetl://t_acme/p_gen/results/1/start/0/0/1")
            .expect("tenant/project canonical URI must parse");
        assert_eq!(r.shape(), "canonical");
    }

    #[test]
    fn result_ref_rejects_malformed() {
        // Wrong scheme, too-few segments, and empty segments are all errors —
        // the caller logs + counts `malformed` and leaves the event untouched.
        assert!(parse_result_ref("https://t/p/results/1/s/0/0/1").is_err());
        assert!(parse_result_ref("noetl://t/p/results").is_err());
        assert!(parse_result_ref("noetl://t//results/x").is_err());
        // A legacy-looking ref with a non-numeric id still routes to the legacy
        // parser and fails there (not silently accepted as canonical).
        assert!(parse_result_ref("noetl://execution/abc/result/step/1").is_err());
    }

    // --- URI construction round-trip (no DB) ---

    #[test]
    fn uri_format_round_trips_through_parser() {
        let eid: i64 = 9876543210;
        let name = "output_select";
        let result_id: i64 = 1122334455;
        let uri = format!("noetl://execution/{eid}/result/{name}/{result_id}");
        let parsed = parse_noetl_ref(&uri).unwrap();
        assert_eq!(parsed.execution_id, eid);
        assert_eq!(parsed.name, name);
        assert_eq!(parsed.result_id, result_id);
    }

    // --- sha256 + bytes helpers (no DB) ---

    #[test]
    fn serialise_and_hash_are_deterministic() {
        let data = serde_json::json!({"rows": [1, 2, 3], "columns": ["a"]});
        let bytes = serde_json::to_vec(&data).unwrap();
        let hash = hex::encode(Sha256::digest(&bytes));
        // Second call produces same output.
        let bytes2 = serde_json::to_vec(&data).unwrap();
        let hash2 = hex::encode(Sha256::digest(&bytes2));
        assert_eq!(hash, hash2);
        assert!(!hash.is_empty());
        assert_eq!(hash.len(), 64); // hex of 32-byte SHA-256
    }
}
