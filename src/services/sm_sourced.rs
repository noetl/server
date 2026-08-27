//! Secret-Manager-sourced credential values (noetl/ai-meta#267, Option 2).
//!
//! # What this is
//!
//! A credential record whose **value** lives in a secret manager rather than in
//! the encrypted `data` column. The record keeps its identity, its alias and its
//! type; only the value is fetched at serve time. Playbooks are untouched —
//! `kind: credential` + `{{ keychain.<alias>.client_secret }}` resolves exactly
//! as it does for a static credential.
//!
//! # Why this shape and not a provider-backed keychain entry
//!
//! The obvious design — declare `provider: gcp` on the playbook's `keychain:`
//! block — is unreachable. Measured in kind: the template is correctly *deferred*
//! by `template_references_keychain`, handed to the worker, and the worker never
//! calls any keychain-resolve endpoint (`grep 'api/keychain'` across worker and
//! tools yields one error-message string). The render context arrives as
//! `{"workload": {}}` with no `keychain` key, so the reference renders empty and
//! the failure surfaces later as a 401 — a wrong-looking credential rather than
//! an unresolved one.
//!
//! `GET /api/credentials/{alias}` is the path that *is* wired and is what prod
//! uses today, so the fetch belongs behind it.
//!
//! # The marker
//!
//! Inside the credential's decrypted `data`:
//!
//! ```json
//! {"__secret_source__": {"provider": "gcp", "secret": "auth0_client", "project": "…"}}
//! ```
//!
//! It lives in `data` rather than a new column so there is no schema migration,
//! the existing register/update API carries it unchanged, and the marker is
//! itself protected by the envelope cipher.
//!
//! # Latency and failure, decided rather than inherited
//!
//! This sits on the worker's dispatch path, so an unbounded call here would be
//! noetl/ai-meta#297 all over again. Three bounds, all deliberate:
//!
//! - the provider's HTTP client already carries a **10s timeout**, so a hung peer
//!   cannot park the serve path;
//! - a successful fetch is cached in-process for [`SM_CACHE_TTL_SECS`], so a hot
//!   alias costs one fetch per TTL rather than one per dispatch;
//! - a fetch failure is **fail-closed**: it returns an error. It does not serve a
//!   stale value past its TTL and it does not serve an empty object — an empty
//!   object renders an empty sub-field and fails at the API call, looking like a
//!   wrong credential rather than an unreachable secret manager.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::error::{AppError, AppResult};
use crate::secrets::{build_secret_provider, SecretRef};

/// The key that marks a credential's value as secret-manager-sourced.
pub const MARKER: &str = "__secret_source__";

/// How long a successfully fetched value is reused. Short enough that a rotation
/// takes effect without a restart; long enough that a busy alias is not a fetch
/// per dispatch.
pub const SM_CACHE_TTL_SECS: u64 = 300;

/// Where a credential's value comes from, parsed out of the marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretSource {
    pub provider: String,
    pub secret: String,
    pub project: Option<String>,
    pub version: Option<String>,
}

/// Read the marker out of a decrypted credential `data` object.
///
/// Returns `None` for every ordinary credential, which is what keeps static
/// credentials on exactly their current path.
pub fn parse_marker(data: &serde_json::Value) -> Option<SecretSource> {
    let m = data.get(MARKER)?.as_object()?;
    let provider = m.get("provider")?.as_str()?.trim();
    let secret = m.get("secret")?.as_str()?.trim();
    if provider.is_empty() || secret.is_empty() {
        return None;
    }
    Some(SecretSource {
        provider: provider.to_string(),
        secret: secret.to_string(),
        project: m
            .get("project")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty()),
        version: m
            .get("version")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty()),
    })
}

struct Cached {
    value: serde_json::Value,
    at: Instant,
}

fn cache() -> &'static Mutex<HashMap<String, Cached>> {
    static C: OnceLock<Mutex<HashMap<String, Cached>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_key(src: &SecretSource) -> String {
    format!(
        "{}|{}|{}|{}",
        src.provider,
        src.project.as_deref().unwrap_or("-"),
        src.secret,
        src.version.as_deref().unwrap_or("latest")
    )
}

/// Fetch the value for a marked credential, honouring the in-process TTL.
///
/// The returned object is what the caller serves as the credential's `data`, so
/// `{{ keychain.<alias>.<field> }}` indexes it directly.
pub async fn resolve(src: &SecretSource) -> AppResult<serde_json::Value> {
    let key = cache_key(src);
    if let Ok(g) = cache().lock() {
        if let Some(c) = g.get(&key) {
            if c.at.elapsed() < Duration::from_secs(SM_CACHE_TTL_SECS) {
                crate::metrics::record_sm_sourced_credential("cache_hit");
                return Ok(c.value.clone());
            }
        }
    }

    let provider = build_secret_provider(&src.provider)?;
    let fetched = provider
        .fetch(&SecretRef {
            name: src.secret.clone(),
            project: src.project.clone(),
            version: src.version.clone(),
            region: None,
        })
        .await
        .map_err(|e| {
            crate::metrics::record_sm_sourced_credential("fetch_error");
            // Fail closed, and name the cause. Serving `{}` here would render an
            // empty sub-field and fail at the API call as a wrong credential.
            AppError::ExternalService(format!(
                "credential value is sourced from {} secret '{}' and could not be \
                 fetched: {e} (noetl/ai-meta#267)",
                src.provider, src.secret
            ))
        })?;

    let value = crate::secrets::structured_or_string(&fetched.value);
    if !value.is_object() {
        crate::metrics::record_sm_sourced_credential("not_an_object");
        return Err(AppError::ExternalService(format!(
            "credential value from {} secret '{}' is not a JSON object, so it has \
             no sub-fields to serve (noetl/ai-meta#267)",
            src.provider, src.secret
        )));
    }

    if let Ok(mut g) = cache().lock() {
        g.insert(
            key,
            Cached {
                value: value.clone(),
                at: Instant::now(),
            },
        );
    }
    crate::metrics::record_sm_sourced_credential("fetched");
    Ok(value)
}

/// Drop any cached value for this source — used by the rollback path so a
/// repointed credential takes effect without waiting out the TTL.
pub fn invalidate(src: &SecretSource) {
    if let Ok(mut g) = cache().lock() {
        g.remove(&cache_key(src));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The property that keeps every existing credential working: an ordinary
    /// `data` object carries no marker, so nothing changes for it.
    #[test]
    fn an_ordinary_credential_has_no_marker() {
        for d in [
            json!({"client_secret": "S"}),
            json!({"user": "u", "password": "p"}),
            json!({}),
            json!("a bare string"),
            json!(null),
        ] {
            assert_eq!(parse_marker(&d), None, "data {d:?}");
        }
    }

    /// The marker is read in full, including the optional fields.
    #[test]
    fn a_marked_credential_parses_its_source() {
        let d = json!({MARKER: {
            "provider": "gcp", "secret": "auth0_client",
            "project": "shastaratech-noetl-prod", "version": "7"
        }});
        let s = parse_marker(&d).expect("marker");
        assert_eq!(s.provider, "gcp");
        assert_eq!(s.secret, "auth0_client");
        assert_eq!(s.project.as_deref(), Some("shastaratech-noetl-prod"));
        assert_eq!(s.version.as_deref(), Some("7"));
    }

    /// `project` and `version` are optional — the provider falls back to its
    /// own defaults.
    #[test]
    fn project_and_version_are_optional() {
        let s = parse_marker(&json!({MARKER: {"provider": "gcp", "secret": "s"}}))
            .expect("marker");
        assert_eq!(s.project, None);
        assert_eq!(s.version, None);
    }

    /// ⚠ A half-written marker must NOT be treated as SM-sourced. Falling back to
    /// the static path is the safe direction: the credential keeps working, and
    /// the operator sees the old value rather than an error.
    #[test]
    fn a_malformed_marker_falls_back_to_static_not_to_an_error() {
        for d in [
            json!({MARKER: {"provider": "gcp"}}),              // no secret
            json!({MARKER: {"secret": "s"}}),                  // no provider
            json!({MARKER: {"provider": "", "secret": "s"}}),  // empty provider
            json!({MARKER: {"provider": "gcp", "secret": ""}}),// empty secret
            json!({MARKER: "not an object"}),
            json!({MARKER: null}),
        ] {
            assert_eq!(parse_marker(&d), None, "data {d:?}");
        }
    }

    /// The cache key must separate secrets that differ in any dimension —
    /// otherwise a project or version change would serve the wrong value.
    #[test]
    fn the_cache_key_separates_every_dimension() {
        let base = SecretSource {
            provider: "gcp".into(),
            secret: "s".into(),
            project: Some("p".into()),
            version: None,
        };
        let mut other_project = base.clone();
        other_project.project = Some("q".into());
        let mut other_version = base.clone();
        other_version.version = Some("2".into());
        let mut other_secret = base.clone();
        other_secret.secret = "t".into();

        let k = cache_key(&base);
        for v in [&other_project, &other_version, &other_secret] {
            assert_ne!(k, cache_key(v), "key collision with {v:?}");
        }
    }

    /// The TTL must be bounded and non-zero: zero would fetch on every dispatch
    /// (the #297 hot-path hazard), and an unbounded one would never see a
    /// rotation.
    ///
    /// Asserted through a runtime value rather than the constant directly —
    /// comparing a `const` to a literal is folded at compile time, and clippy is
    /// right that such an assertion can never fail. Routing it through
    /// `Duration` makes it a real check.
    #[test]
    fn the_cache_ttl_is_bounded_and_non_zero() {
        let ttl = Duration::from_secs(SM_CACHE_TTL_SECS);
        assert!(!ttl.is_zero(), "a zero TTL fetches on every dispatch");
        assert!(
            ttl <= Duration::from_secs(3600),
            "an hour-plus TTL would not see a rotation"
        );
    }
}
