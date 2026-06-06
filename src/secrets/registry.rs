//! Per-`(provider_id, region)` cache of [`SecretProvider`] instances
//! (Secrets Wallet Phase 6b, [`noetl/ai-meta#61`](https://github.com/noetl/ai-meta/issues/61)).
//!
//! [`crate::secrets::resolver::resolve_keychain_entry`] is invoked on
//! every keychain-cache miss.  Without a registry, the cache-miss path
//! calls [`crate::secrets::build_secret_provider`] which rebuilds the
//! provider from env on every resolution — re-reading `AWS_ACCESS_KEY_ID`,
//! rebuilding the `reqwest::Client` (TLS bundle reparse on the rustls
//! path), reparsing the IMDS / token state.  All of that is per-resolve
//! overhead orthogonal to the actual fetch.
//!
//! The [`ProviderRegistry`] memoises by `(provider_id, region)` so the
//! per-region instance is built once and reused.  An optional TTL
//! (`NOETL_SECRET_PROVIDER_TTL_SECONDS`) lets operators evict on a clock
//! — useful for short-lived AWS STS / Azure IMDS creds before Phase 6d's
//! dynamic-secret refresh path lands.
//!
//! Observability per `agents/rules/observability.md` Principle 1:
//! - [`crate::metrics::record_secret_provider_build`] — counter,
//!   labelled `(provider, region, status)` where status is `cache_hit` /
//!   `ok` / `error`.
//! - [`crate::metrics::record_secret_resolve_duration`] — histogram of
//!   resolve latency, called by the resolver around the provider's
//!   `fetch` (cardinality bounded by `provider × region`).

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};

use crate::error::AppResult;
use crate::metrics::record_secret_provider_build;
use crate::secrets::{SecretProvider, build_secret_provider};

/// Registry singleton — process-global, lock-protected.
fn registry() -> &'static ProviderRegistry {
    static R: OnceLock<ProviderRegistry> = OnceLock::new();
    R.get_or_init(ProviderRegistry::from_env)
}

/// Get the cached provider for `(provider_id, region)` — or build,
/// cache, and return it.  Public entry point for callers (resolver +
/// future direct API endpoints).  Region empty / `"-"` is treated as
/// "no region" — the provider's `from_env()` default is used.
pub fn get_provider(provider_id: &str, region: &str) -> AppResult<Arc<dyn SecretProvider>> {
    registry().get_or_build(provider_id, region)
}

/// `(provider_id, region)` → cached provider.  Region is the
/// `KeychainDef.region` (or `NOETL_SERVER_REGION`) the resolver
/// already filled in — see [`crate::secrets::resolver`].
pub struct ProviderRegistry {
    inner: RwLock<HashMap<CacheKey, CacheEntry>>,
    ttl: Option<Duration>,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    provider_id: String,
    region: String,
}

struct CacheEntry {
    provider: Arc<dyn SecretProvider>,
    built_at: Instant,
}

impl ProviderRegistry {
    /// New registry with TTL from `NOETL_SECRET_PROVIDER_TTL_SECONDS`.
    /// Unset / `0` ⇒ no TTL (cache for process lifetime).
    pub fn from_env() -> Self {
        let ttl = std::env::var("NOETL_SECRET_PROVIDER_TTL_SECONDS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|n| *n > 0)
            .map(Duration::from_secs);
        Self {
            inner: RwLock::new(HashMap::new()),
            ttl,
        }
    }

    /// Test-only constructor — explicit TTL, empty cache.
    #[cfg(test)]
    pub fn with_ttl(ttl: Option<Duration>) -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            ttl,
        }
    }

    /// Get the cached entry for `(provider_id, region)` or build + insert.
    pub fn get_or_build(
        &self,
        provider_id: &str,
        region: &str,
    ) -> AppResult<Arc<dyn SecretProvider>> {
        let key = CacheKey {
            provider_id: provider_id.to_string(),
            region: region.to_string(),
        };

        // Fast path: read lock + TTL check.
        if let Some(entry) = self.inner.read().unwrap().get(&key) {
            if !self.is_expired(entry) {
                record_secret_provider_build(provider_id, region, "cache_hit");
                return Ok(entry.provider.clone());
            }
        }

        // Slow path: write lock, double-check the entry isn't already fresh
        // (another thread may have built it while we were waiting), then
        // build + insert.
        let mut guard = self.inner.write().unwrap();
        if let Some(entry) = guard.get(&key) {
            if !self.is_expired(entry) {
                record_secret_provider_build(provider_id, region, "cache_hit");
                return Ok(entry.provider.clone());
            }
        }

        match build_secret_provider(provider_id) {
            Ok(provider) => {
                record_secret_provider_build(provider_id, region, "ok");
                guard.insert(
                    key,
                    CacheEntry {
                        provider: provider.clone(),
                        built_at: now(),
                    },
                );
                Ok(provider)
            }
            Err(e) => {
                record_secret_provider_build(provider_id, region, "error");
                Err(e)
            }
        }
    }

    /// Number of entries currently cached.  Test + diagnostic use.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.read().unwrap().len()
    }

    fn is_expired(&self, entry: &CacheEntry) -> bool {
        match self.ttl {
            None => false,
            Some(ttl) => now().duration_since(entry.built_at) > ttl,
        }
    }
}

/// Indirection for tests — `Instant::now()` is opaque but monotonic;
/// production calls it directly, tests can rely on the `Instant` math
/// without needing to mock time.
fn now() -> Instant {
    Instant::now()
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;
    use async_trait::async_trait;

    use crate::error::AppError;
    use crate::secrets::{SecretRef, SecretValue};

    /// Provider used in tests — doesn't read env, just records calls.
    struct StubProvider;

    #[async_trait]
    impl SecretProvider for StubProvider {
        fn provider(&self) -> &'static str {
            "stub"
        }
        async fn fetch(&self, _: &SecretRef) -> AppResult<SecretValue> {
            Err(AppError::NotFound("stub: never reached".to_string()))
        }
    }

    /// Insert pre-built providers directly so tests don't depend on
    /// `build_secret_provider`'s env reads.
    fn insert_stub(reg: &ProviderRegistry, provider_id: &str, region: &str) {
        let mut g = reg.inner.write().unwrap();
        g.insert(
            CacheKey {
                provider_id: provider_id.to_string(),
                region: region.to_string(),
            },
            CacheEntry {
                provider: Arc::new(StubProvider),
                built_at: now(),
            },
        );
    }

    #[test]
    fn cache_hit_returns_same_arc() {
        let reg = ProviderRegistry::with_ttl(None);
        insert_stub(&reg, "stub", "us-east-1");
        let a = reg.get_or_build("stub", "us-east-1").unwrap();
        let b = reg.get_or_build("stub", "us-east-1").unwrap();
        assert!(Arc::ptr_eq(&a, &b), "same key must return same Arc");
    }

    #[test]
    fn different_region_is_different_cache_entry() {
        let reg = ProviderRegistry::with_ttl(None);
        insert_stub(&reg, "stub", "us-east-1");
        insert_stub(&reg, "stub", "eu-central-1");
        let a = reg.get_or_build("stub", "us-east-1").unwrap();
        let b = reg.get_or_build("stub", "eu-central-1").unwrap();
        assert!(
            !Arc::ptr_eq(&a, &b),
            "different regions must hand back different Arcs"
        );
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn different_provider_is_different_cache_entry() {
        let reg = ProviderRegistry::with_ttl(None);
        insert_stub(&reg, "stub", "us-east-1");
        insert_stub(&reg, "other", "us-east-1");
        assert_eq!(reg.len(), 2);
        let a = reg.get_or_build("stub", "us-east-1").unwrap();
        let b = reg.get_or_build("other", "us-east-1").unwrap();
        assert!(!Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn ttl_does_not_expire_freshly_built_entry() {
        let reg = ProviderRegistry::with_ttl(Some(Duration::from_secs(60)));
        insert_stub(&reg, "stub", "us-east-1");
        // Just-built; well within the TTL window.
        let a = reg.get_or_build("stub", "us-east-1").unwrap();
        let b = reg.get_or_build("stub", "us-east-1").unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn ttl_zero_treated_as_no_ttl_in_from_env() {
        // Belt-and-suspenders: from_env() filter discards `0`.
        // Run in a child process-less context — just assert the field.
        let saved = std::env::var("NOETL_SECRET_PROVIDER_TTL_SECONDS").ok();
        unsafe { std::env::set_var("NOETL_SECRET_PROVIDER_TTL_SECONDS", "0") };
        let reg = ProviderRegistry::from_env();
        assert!(reg.ttl.is_none());
        // Restore env so we don't poison sibling tests.
        match saved {
            Some(v) => unsafe { std::env::set_var("NOETL_SECRET_PROVIDER_TTL_SECONDS", v) },
            None => unsafe { std::env::remove_var("NOETL_SECRET_PROVIDER_TTL_SECONDS") },
        }
    }

    #[test]
    fn concurrent_get_or_build_returns_same_arc() {
        let reg = Arc::new(ProviderRegistry::with_ttl(None));
        insert_stub(&reg, "stub", "us-east-1");
        let mut handles = Vec::new();
        for _ in 0..8 {
            let r = reg.clone();
            handles.push(thread::spawn(move || {
                r.get_or_build("stub", "us-east-1").unwrap()
            }));
        }
        let arcs: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for a in &arcs[1..] {
            assert!(Arc::ptr_eq(&arcs[0], a));
        }
        assert_eq!(reg.len(), 1, "no duplicate insertions under contention");
    }

    #[test]
    fn missing_entry_attempts_build_and_records_error() {
        let reg = ProviderRegistry::with_ttl(None);
        // Don't pre-insert.  build_secret_provider("nonexistent") errors.
        match reg.get_or_build("nonexistent", "us-east-1") {
            Ok(_) => panic!("expected unsupported-provider error"),
            Err(e) => assert!(
                format!("{e:?}").contains("unsupported"),
                "expected unsupported-provider error, got: {e:?}"
            ),
        }
        // Failed build is NOT cached — next call retries.
        assert_eq!(reg.len(), 0);
    }
}
