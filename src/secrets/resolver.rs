//! Keychain secret-source resolution (Secrets Wallet Phase 3b R3a,
//! noetl/ai-meta#61).
//!
//! Resolves a [`KeychainDef`] into the credential value it represents, fetching
//! each referenced secret from a [`SecretProvider`]. This is the **pure**
//! resolution logic; the DB plumbing — loading the playbook + workload for an
//! execution, the `get_credential` cache-miss hook, and envelope-caching the
//! resolved value — lands in R3b.

use std::collections::HashMap;

use super::{SecretProvider, SecretRef};
use crate::error::AppResult;
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
pub async fn resolve_keychain_entry(
    kc: &KeychainDef,
    workload: &HashMap<String, serde_json::Value>,
    provider: &dyn SecretProvider,
) -> AppResult<serde_json::Value> {
    let renderer = TemplateRenderer::new();
    match &kc.map {
        Some(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, path_template) in map {
                let path = renderer.render(path_template, workload)?;
                let secret = provider
                    .fetch(&SecretRef {
                        name: path,
                        project: None,
                        version: None,
                    })
                    .await?;
                out.insert(key.clone(), serde_json::Value::String(secret.value));
            }
            Ok(serde_json::Value::Object(out))
        }
        None => {
            let secret = provider
                .fetch(&SecretRef {
                    name: kc.name.clone(),
                    project: None,
                    version: None,
                })
                .await?;
            Ok(serde_json::Value::String(secret.value))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::AppError;
    use crate::secrets::SecretValue;
    use async_trait::async_trait;

    /// In-memory provider: maps secret-name → value.
    struct FakeProvider {
        values: HashMap<String, String>,
    }

    impl FakeProvider {
        fn new(pairs: &[(&str, &str)]) -> Self {
            Self {
                values: pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            }
        }
    }

    #[async_trait]
    impl SecretProvider for FakeProvider {
        fn provider(&self) -> &'static str {
            "fake"
        }
        async fn fetch(&self, secret: &SecretRef) -> AppResult<SecretValue> {
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
}
