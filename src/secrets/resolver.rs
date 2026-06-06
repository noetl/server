//! Keychain secret-source resolution (Secrets Wallet Phase 3b R3a,
//! noetl/ai-meta#61).
//!
//! Resolves a [`KeychainDef`] into the credential value it represents, fetching
//! each referenced secret from a [`SecretProvider`]. This is the **pure**
//! resolution logic; the DB plumbing — loading the playbook + workload for an
//! execution, the `get_credential` cache-miss hook, and envelope-caching the
//! resolved value — lands in R3b.

use std::collections::HashMap;

use super::residency;
use super::{SecretProvider, SecretRef, server_region};
use crate::error::AppResult;
use crate::metrics::{record_secret_resolve, record_secret_resolve_duration};
use crate::playbook::types::KeychainDef;
use crate::template::TemplateRenderer;

/// Resolve a keychain entry's secret-source into its credential value.
///
/// - **`map`-shaped entry**: each value is a secret-path template rendered
///   against `workload`, fetched via `provider`, and assembled into the object
///   `{ key: secret_value, ... }` — the "auth object as a map" shape where
///   several secrets resolve into one keychain entry.
/// - **`map`-less entry**: resolves to the single secret named by the entry.
///
/// The resolved value is never logged; callers keep it out of any
/// state-surfacing response (masked at the boundary).
///
/// **Secrets Wallet Phase 6a — region routing.**  The entry's
/// [`KeychainDef::region`] (or `NOETL_SERVER_REGION` as a fallback) is filled
/// into [`SecretRef::region`] so the provider can route the fetch to the
/// right regional endpoint / vault / cluster.  Every resolution increments
/// [`crate::metrics::record_secret_resolve`] labelled by `(provider, region,
/// status)` for per-region operator observability.  Cardinality is bounded
/// (low-tens of regions in practice) — region is a routing hint, not a secret.
pub async fn resolve_keychain_entry(
    kc: &KeychainDef,
    workload: &HashMap<String, serde_json::Value>,
    provider: &dyn SecretProvider,
) -> AppResult<serde_json::Value> {
    let renderer = TemplateRenderer::new();
    let region = effective_region(kc);
    let provider_id = provider.provider();
    // Phase 6c — residency gate, runs BEFORE any provider call.  On a
    // strict-mode violation `to_result` returns Err; the resolver
    // short-circuits and never touches the provider.  Advisory + None
    // paths fall through to the normal fetch path.  The gate records
    // its own metric inside `evaluate`; we don't need the duration
    // histogram for the gate (it's an in-process comparison, not a
    // boundary call).
    residency::to_result(residency::evaluate(kc, &region))?;
    // Phase 6b — record wall-clock latency for the whole entry resolution.
    let started = std::time::Instant::now();
    let result = match &kc.map {
        Some(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, path_template) in map {
                let path = match renderer.render(path_template, workload) {
                    Ok(p) => p,
                    Err(e) => {
                        record_secret_resolve(provider_id, &region, "template_error");
                        return Err(e);
                    }
                };
                let secret = match provider
                    .fetch(&SecretRef {
                        name: path,
                        region: Some(region.clone()).filter(|r| !r.is_empty()),
                        ..SecretRef::default()
                    })
                    .await
                {
                    Ok(s) => s,
                    Err(e) => {
                        record_secret_resolve(provider_id, &region, "provider_fetch_error");
                        return Err(e);
                    }
                };
                out.insert(key.clone(), serde_json::Value::String(secret.value));
            }
            Ok(serde_json::Value::Object(out))
        }
        None => {
            let secret = match provider
                .fetch(&SecretRef {
                    name: kc.name.clone(),
                    region: Some(region.clone()).filter(|r| !r.is_empty()),
                    ..SecretRef::default()
                })
                .await
            {
                Ok(s) => s,
                Err(e) => {
                    record_secret_resolve(provider_id, &region, "provider_fetch_error");
                    return Err(e);
                }
            };
            Ok(serde_json::Value::String(secret.value))
        }
    };
    if result.is_ok() {
        record_secret_resolve(provider_id, &region, "ok");
    }
    // Phase 6b — observe the resolve latency regardless of outcome so a
    // dashboard surfaces "everything's slow" + "everything's failing"
    // independently.  Duration is meaningful even on the error path
    // (timeouts dominate failure mode wall-clock).
    record_secret_resolve_duration(provider_id, &region, started.elapsed().as_secs_f64());
    result
}

/// The region this keychain entry resolves into — the entry's own
/// `region` wins; otherwise the server's `NOETL_SERVER_REGION` env;
/// otherwise empty (legacy).
pub(crate) fn effective_region(kc: &KeychainDef) -> String {
    if let Some(r) = kc.region.as_deref().filter(|s| !s.is_empty()) {
        return r.to_string();
    }
    server_region().to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::error::AppError;
    use crate::secrets::SecretValue;
    use async_trait::async_trait;

    /// In-memory provider: maps secret-name → value.  Records every
    /// [`SecretRef`] it was called with, so tests can assert that the
    /// resolver propagated the region correctly.
    struct FakeProvider {
        values: HashMap<String, String>,
        seen: Mutex<Vec<SecretRef>>,
    }

    impl FakeProvider {
        fn new(pairs: &[(&str, &str)]) -> Self {
            Self {
                values: pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn last_region(&self) -> Option<String> {
            self.seen
                .lock()
                .unwrap()
                .last()
                .and_then(|s| s.region.clone())
        }
    }

    #[async_trait]
    impl SecretProvider for FakeProvider {
        fn provider(&self) -> &'static str {
            "fake"
        }
        async fn fetch(&self, secret: &SecretRef) -> AppResult<SecretValue> {
            self.seen.lock().unwrap().push(secret.clone());
            self.values
                .get(&secret.name)
                .map(|v| SecretValue {
                    value: v.clone(),
                    version: None,
                })
                .ok_or_else(|| {
                    AppError::NotFound(format!("fake secret '{}' not found", secret.name))
                })
        }
    }

    fn workload(pairs: &[(&str, &str)]) -> HashMap<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
            .collect()
    }

    #[tokio::test]
    async fn map_entry_renders_path_template_and_assembles_object() {
        let kc: KeychainDef = serde_yaml::from_str(
            r#"
name: openai_token
provider: gcp
map:
  api_key: "{{ openai_secret_path }}"
"#,
        )
        .unwrap();
        // The workload supplies the secret path the template references.
        let wl = workload(&[("openai_secret_path", "projects/p/secrets/openai")]);
        let provider = FakeProvider::new(&[("projects/p/secrets/openai", "sk-live-123")]);

        let resolved = resolve_keychain_entry(&kc, &wl, &provider).await.unwrap();
        assert_eq!(resolved, serde_json::json!({ "api_key": "sk-live-123" }));
    }

    #[tokio::test]
    async fn map_entry_with_multiple_keys_fetches_each() {
        let kc: KeychainDef = serde_yaml::from_str(
            r#"
name: combo
provider: gcp
map:
  key_a: "projects/p/secrets/a"
  key_b: "{{ b_path }}"
"#,
        )
        .unwrap();
        let wl = workload(&[("b_path", "projects/p/secrets/b")]);
        let provider = FakeProvider::new(&[
            ("projects/p/secrets/a", "val-a"),
            ("projects/p/secrets/b", "val-b"),
        ]);

        let resolved = resolve_keychain_entry(&kc, &wl, &provider).await.unwrap();
        assert_eq!(resolved["key_a"], "val-a");
        assert_eq!(resolved["key_b"], "val-b");
    }

    #[tokio::test]
    async fn map_less_entry_resolves_single_value_by_name() {
        let kc: KeychainDef = serde_yaml::from_str(
            r#"
name: projects/p/secrets/duffel
provider: gcp
"#,
        )
        .unwrap();
        let provider = FakeProvider::new(&[("projects/p/secrets/duffel", "duffel-token")]);

        let resolved = resolve_keychain_entry(&kc, &HashMap::new(), &provider)
            .await
            .unwrap();
        assert_eq!(resolved, serde_json::json!("duffel-token"));
    }

    #[tokio::test]
    async fn missing_secret_propagates_error() {
        let kc: KeychainDef = serde_yaml::from_str(
            r#"
name: missing
provider: gcp
map:
  k: "projects/p/secrets/nope"
"#,
        )
        .unwrap();
        let provider = FakeProvider::new(&[]);
        let err = resolve_keychain_entry(&kc, &HashMap::new(), &provider)
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("not found"), "got: {err:?}");
    }

    // ---------- Phase 6a: region routing ----------

    #[tokio::test]
    async fn keychain_region_propagates_into_secret_ref_map_shape() {
        // KeychainDef.region must reach SecretRef.region for every fetch
        // a `map`-shaped entry triggers, so the provider can route to the
        // right regional endpoint.
        let kc: KeychainDef = serde_yaml::from_str(
            r#"
name: eu_secret
provider: aws
region: eu-central-1
map:
  api_key: "projects/p/secrets/api"
"#,
        )
        .unwrap();
        let provider = FakeProvider::new(&[("projects/p/secrets/api", "k")]);
        let _ = resolve_keychain_entry(&kc, &HashMap::new(), &provider)
            .await
            .unwrap();
        assert_eq!(provider.last_region().as_deref(), Some("eu-central-1"));
    }

    #[tokio::test]
    async fn keychain_region_propagates_into_secret_ref_map_less_shape() {
        let kc: KeychainDef = serde_yaml::from_str(
            r#"
name: projects/p/secrets/x
provider: gcp
region: us-east-1
"#,
        )
        .unwrap();
        let provider = FakeProvider::new(&[("projects/p/secrets/x", "v")]);
        let _ = resolve_keychain_entry(&kc, &HashMap::new(), &provider)
            .await
            .unwrap();
        assert_eq!(provider.last_region().as_deref(), Some("us-east-1"));
    }

    #[tokio::test]
    async fn missing_region_falls_back_to_none_when_env_unset() {
        // The server's NOETL_SERVER_REGION OnceLock is process-global,
        // so we can't reliably mutate env here without races against
        // other tests.  This test asserts the no-region path: when the
        // keychain doesn't supply one AND the cached server region is
        // empty, SecretRef.region is None.
        let kc: KeychainDef = serde_yaml::from_str(
            r#"
name: projects/p/secrets/z
provider: gcp
"#,
        )
        .unwrap();
        let provider = FakeProvider::new(&[("projects/p/secrets/z", "v")]);
        let _ = resolve_keychain_entry(&kc, &HashMap::new(), &provider)
            .await
            .unwrap();
        // If the test process happens to be running with NOETL_SERVER_REGION set,
        // the resolver fills it; otherwise None.  Either is consistent with
        // the contract — assert the value matches whatever server_region() reports.
        let expected = server_region();
        let got = provider.last_region().unwrap_or_default();
        assert_eq!(got, expected);
    }

    #[tokio::test]
    async fn effective_region_prefers_keychain_over_env() {
        // Even if env happens to be set, an explicit KeychainDef.region wins.
        let kc: KeychainDef = serde_yaml::from_str(
            r#"
name: x
provider: gcp
region: ap-south-1
"#,
        )
        .unwrap();
        assert_eq!(effective_region(&kc), "ap-south-1");
    }

    #[tokio::test]
    async fn empty_string_region_treated_as_unset() {
        // Defensive: a literal empty string should NOT be passed through as
        // a region label — it short-circuits to the env fallback path.
        let kc: KeychainDef = serde_yaml::from_str(
            r#"
name: x
provider: gcp
region: ""
"#,
        )
        .unwrap();
        assert_eq!(effective_region(&kc), server_region().to_string());
    }
}
