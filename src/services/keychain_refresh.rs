//! Background-refresh inflight tracker for Phase 7c.3.
//!
//! When the resolver's cache-hit path finds a row inside the refresh window
//! (Phase 7c.2 `KeychainService::should_refresh` → true), it spawns a
//! background task to re-resolve via the provider and update the cache.
//! This tracker collapses stampedes: if N workers find the same row in
//! the refresh window at the same time, only the first claim wins the
//! refresh slot — the rest piggy-back via
//! `noetl_secret_refresh_total{outcome="stampede_collapsed"}`.
//!
//! Lifecycle:
//!
//! 1. `try_claim(key)` returns `true` only if the key was NOT in the set
//!    (and inserts it).  The caller MUST `release(key)` exactly once when
//!    the spawned refresh finishes (success or failure) — otherwise the
//!    key is permanently locked out from future refreshes until process
//!    restart.
//! 2. The companion span / metric instrumentation lives in the resolver
//!    (`src/services/credential.rs`), not here.  This module is purely
//!    the synchronisation primitive.
//!
//! The set is keyed by `(catalog_id, alias)` — distinct executions
//! refreshing the same `(catalog_id, alias)` collapse to one provider
//! call, exactly as Phase 7c.3 specifies.

use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::Mutex;

/// Refresh-inflight tracker.  Clone is cheap (Arc clone) and shares the
/// underlying set across all clones — that's the point.
#[derive(Clone, Debug, Default)]
pub struct RefreshInflight {
    inner: Arc<Mutex<HashSet<(i64, String)>>>,
}

impl RefreshInflight {
    /// Build a fresh tracker.  In production one per process; clone into
    /// every `CredentialService` instance.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Atomically reserve the refresh slot for `key`.  Returns `true` if
    /// the slot was free (and is now held by the caller); `false` if a
    /// concurrent refresh is already in flight for the same key.
    ///
    /// The caller MUST call [`Self::release`] on the same key when the
    /// refresh task finishes — even on the error path.
    pub async fn try_claim(&self, key: (i64, String)) -> bool {
        let mut set = self.inner.lock().await;
        set.insert(key)
    }

    /// Release the refresh slot for `key`.  Idempotent — calling this for
    /// a key that wasn't held is a no-op.
    pub async fn release(&self, key: &(i64, String)) {
        let mut set = self.inner.lock().await;
        set.remove(key);
    }

    /// Inspect whether a key currently has a refresh in flight.  Mostly
    /// useful for tests; production code uses [`Self::try_claim`].
    #[cfg(test)]
    pub async fn contains(&self, key: &(i64, String)) -> bool {
        let set = self.inner.lock().await;
        set.contains(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn try_claim_succeeds_for_free_slot() {
        let inflight = RefreshInflight::new();
        let key = (42, "duffel_token".to_string());
        assert!(inflight.try_claim(key.clone()).await);
        assert!(inflight.contains(&key).await);
    }

    #[tokio::test]
    async fn try_claim_fails_for_held_slot() {
        let inflight = RefreshInflight::new();
        let key = (42, "duffel_token".to_string());
        assert!(inflight.try_claim(key.clone()).await);
        // Second claim for the same key collapses — the stampede signal.
        assert!(!inflight.try_claim(key.clone()).await);
    }

    #[tokio::test]
    async fn release_allows_reclaim() {
        let inflight = RefreshInflight::new();
        let key = (42, "duffel_token".to_string());
        assert!(inflight.try_claim(key.clone()).await);
        inflight.release(&key).await;
        assert!(!inflight.contains(&key).await);
        // After release the slot can be claimed again — next refresh round.
        assert!(inflight.try_claim(key.clone()).await);
    }

    #[tokio::test]
    async fn distinct_keys_dont_collide() {
        let inflight = RefreshInflight::new();
        let key_a = (42, "duffel_token".to_string());
        let key_b = (42, "openai_key".to_string());
        let key_c = (43, "duffel_token".to_string());
        assert!(inflight.try_claim(key_a.clone()).await);
        assert!(inflight.try_claim(key_b.clone()).await);
        assert!(inflight.try_claim(key_c.clone()).await);
        // Each distinct key gets its own slot — alias-level and catalog-level
        // independence.
        assert!(inflight.contains(&key_a).await);
        assert!(inflight.contains(&key_b).await);
        assert!(inflight.contains(&key_c).await);
    }

    #[tokio::test]
    async fn release_is_idempotent() {
        let inflight = RefreshInflight::new();
        let key = (42, "missing".to_string());
        // Releasing a never-claimed slot is a no-op (mirror set.remove on absent key).
        inflight.release(&key).await;
        assert!(!inflight.contains(&key).await);
    }

    #[tokio::test]
    async fn clone_shares_inner_state() {
        // Critical invariant: cloned trackers share the same underlying set
        // so the resolver's stampede collapse works across CredentialService
        // clones (which clone the field).
        let inflight = RefreshInflight::new();
        let clone = inflight.clone();
        let key = (42, "duffel_token".to_string());
        assert!(inflight.try_claim(key.clone()).await);
        // The clone sees the same slot — second claim collapses.
        assert!(!clone.try_claim(key.clone()).await);
        clone.release(&key).await;
        // And release through the clone frees the slot for the original.
        assert!(inflight.try_claim(key.clone()).await);
    }
}
