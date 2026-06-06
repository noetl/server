//! Dynamic / short-lived secret support (Secrets Wallet Phase 6d,
//! [`noetl/ai-meta#61`](https://github.com/noetl/ai-meta/issues/61)).
//!
//! Some providers return secrets that the issuer expires on a clock —
//! AWS STS bearer tokens (15 min – 12 h), AAD access tokens (1 h
//! default), GCP `iamcredentials.generateAccessToken` (1 h default, 12 h
//! max), OAuth2 access tokens (issuer-controlled `expires_in`).  The
//! Phase-3c keychain cache used a fixed 600 s TTL.  Caching a token
//! past its `expires_at` means the next worker fetch gets a 401 and the
//! playbook step fails.
//!
//! This module computes the **effective cache TTL** — the smaller of the
//! wallet's default TTL and `expires_at - now - safety_margin` — and
//! signals back when the issuer-supplied expiry is already past (the
//! resolver should skip the cache write entirely rather than store
//! something that's already dead).
//!
//! The actual cloud-specific provider implementations (STS / AAD /
//! iamcredentials) ride follow-up rounds (6d.1 / 6d.2 / 6d.3); this
//! round establishes the primitives + the cache plumbing so those land
//! cleanly.

use std::time::Duration;

use chrono::{DateTime, Utc};

/// Default safety margin (seconds) when the env override is unset.
/// 60 s buffers against clock skew + the wall-clock between cache write
/// and the next worker fetch.
const DEFAULT_SAFETY_MARGIN_SECS: u64 = 60;

/// Default refresh window (seconds) when the env override is unset.
/// Secrets Wallet Phase 7c: a cached short-lived token gets a
/// background refresh once its remaining lifetime drops below this
/// threshold.  60 s covers a typical OAuth2 refresh round-trip (~200ms)
/// plus headroom.
const DEFAULT_REFRESH_WINDOW_SECS: u64 = 60;

/// Floor for the effective TTL — never cache for less than this even
/// when `expires_at - now - safety_margin` would compute to something
/// smaller.  Below this floor we'd be paying the cache cost (round-trip
/// to the keychain table + envelope encryption) for negligible reuse.
/// Caller can still opt out via [`effective_cache_ttl_decision`]'s
/// `SkipCache` arm when `expires_at` is already past.
const MIN_EFFECTIVE_TTL_SECS: u64 = 5;

/// Read the safety margin from `KEYCHAIN_CACHE_DYNAMIC_SAFETY_MARGIN_SECS`
/// (defaults to [`DEFAULT_SAFETY_MARGIN_SECS`]).  Process-global, read
/// once at startup.
pub fn safety_margin_secs() -> u64 {
    use std::sync::OnceLock;
    static M: OnceLock<u64> = OnceLock::new();
    *M.get_or_init(|| {
        std::env::var("KEYCHAIN_CACHE_DYNAMIC_SAFETY_MARGIN_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_SAFETY_MARGIN_SECS)
    })
}

/// Secrets Wallet Phase 7c: how long before `expires_at` to mark a
/// cached row "renewable."  Read once at startup from
/// `KEYCHAIN_CACHE_REFRESH_WINDOW_SECS` (defaults to
/// [`DEFAULT_REFRESH_WINDOW_SECS`]).
pub fn refresh_window_secs() -> u64 {
    use std::sync::OnceLock;
    static M: OnceLock<u64> = OnceLock::new();
    *M.get_or_init(|| {
        std::env::var("KEYCHAIN_CACHE_REFRESH_WINDOW_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(DEFAULT_REFRESH_WINDOW_SECS)
    })
}

/// What the cache layer should do with a freshly-resolved secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheDecision {
    /// Cache for this many seconds.
    CacheFor(u64),
    /// `expires_at` is in the past (or within the safety margin past
    /// `now`) — skip the cache write.  The next worker fetch will
    /// re-resolve from the provider, which is the correct outcome.
    SkipCacheAlreadyExpired,
}

/// Decide how to cache a freshly-resolved secret based on the issuer's
/// reported `expires_at`, the wallet's `default_ttl`, the operator's
/// `safety_margin`, and the current wall-clock `now`.
///
/// Rules, in order:
/// 1. `expires_at = None` ⇒ caller is a long-lived secret; cache for
///    `default_ttl` (back-compat with pre-6d behaviour).
/// 2. `expires_at <= now + safety_margin` ⇒ token is already (or about
///    to be) dead.  Return [`CacheDecision::SkipCacheAlreadyExpired`].
/// 3. Otherwise ⇒ cache for `min(default_ttl, expires_at - now -
///    safety_margin)`, floored at [`MIN_EFFECTIVE_TTL_SECS`].
pub fn cache_decision(
    expires_at: Option<DateTime<Utc>>,
    default_ttl: Duration,
    safety_margin: Duration,
    now: DateTime<Utc>,
) -> CacheDecision {
    let Some(expires_at) = expires_at else {
        return CacheDecision::CacheFor(default_ttl.as_secs());
    };

    let safety =
        chrono::Duration::from_std(safety_margin).unwrap_or_else(|_| chrono::Duration::seconds(0));
    let effective_deadline = expires_at - safety;
    if effective_deadline <= now {
        return CacheDecision::SkipCacheAlreadyExpired;
    }

    let remaining = (effective_deadline - now).num_seconds().max(0) as u64;
    let capped = remaining
        .min(default_ttl.as_secs())
        .max(MIN_EFFECTIVE_TTL_SECS);
    CacheDecision::CacheFor(capped)
}

/// Convenience entry point that reads the safety margin from env.
pub fn effective_cache_ttl(
    expires_at: Option<DateTime<Utc>>,
    default_ttl: Duration,
    now: DateTime<Utc>,
) -> CacheDecision {
    cache_decision(
        expires_at,
        default_ttl,
        Duration::from_secs(safety_margin_secs()),
        now,
    )
}

/// Secrets Wallet Phase 7c — should the cached row be refreshed *now*
/// in the background?
///
/// Returns `true` iff:
/// - `expires_at` is set (long-lived secrets without an issuer expiry
///   stay on the periodic-eviction path and never get a refresh
///   triggered);
/// - the token is STILL VALID (`expires_at > now`) — we don't trigger
///   a refresh for something already dead; that's
///   [`CacheDecision::SkipCacheAlreadyExpired`]'s job at the next
///   resolve;
/// - the remaining lifetime is within the refresh window
///   (`now + refresh_window >= expires_at`).
///
/// The caller (resolver / cache layer) treats `true` as "return the
/// still-valid cached value AND spawn a background refresh."
/// Stampede collapse + the actual refresh path live in
/// `services::credential`; this is the pure decision primitive.
pub fn should_refresh(
    expires_at: Option<DateTime<Utc>>,
    refresh_window: Duration,
    now: DateTime<Utc>,
) -> bool {
    let Some(expires_at) = expires_at else {
        return false;
    };
    if expires_at <= now {
        // Already past the deadline — eviction path, not refresh path.
        return false;
    }
    let window = chrono::Duration::from_std(refresh_window)
        .unwrap_or_else(|_| chrono::Duration::seconds(0));
    expires_at - window <= now
}

/// Convenience entry point that reads the refresh window from env.
pub fn should_refresh_default(
    expires_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> bool {
    should_refresh(
        expires_at,
        Duration::from_secs(refresh_window_secs()),
        now,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(secs, 0).unwrap()
    }

    #[test]
    fn no_expires_at_uses_default_ttl() {
        let d = cache_decision(
            None,
            Duration::from_secs(600),
            Duration::from_secs(60),
            at(1000),
        );
        assert_eq!(d, CacheDecision::CacheFor(600));
    }

    #[test]
    fn expires_at_far_future_is_capped_at_default_ttl() {
        // expires_at way past default_ttl; the wallet's own TTL still
        // bounds reuse so the cache doesn't outlive the operator's
        // refresh policy.
        let now = at(1000);
        let expires_at = at(10_000);
        let d = cache_decision(
            Some(expires_at),
            Duration::from_secs(600),
            Duration::from_secs(60),
            now,
        );
        assert_eq!(d, CacheDecision::CacheFor(600));
    }

    #[test]
    fn expires_at_near_future_uses_expires_minus_margin() {
        // expires_at = now + 300 s; safety_margin = 60 s; default_ttl = 600 s
        // ⇒ effective = 300 - 60 = 240 s.
        let now = at(1000);
        let expires_at = at(1000 + 300);
        let d = cache_decision(
            Some(expires_at),
            Duration::from_secs(600),
            Duration::from_secs(60),
            now,
        );
        assert_eq!(d, CacheDecision::CacheFor(240));
    }

    #[test]
    fn expires_at_already_past_skips_cache() {
        let now = at(1000);
        let expires_at = at(900);
        let d = cache_decision(
            Some(expires_at),
            Duration::from_secs(600),
            Duration::from_secs(60),
            now,
        );
        assert_eq!(d, CacheDecision::SkipCacheAlreadyExpired);
    }

    #[test]
    fn expires_at_inside_safety_margin_skips_cache() {
        // expires_at = now + 30 s; safety_margin = 60 s ⇒ deadline = now - 30.
        // Treated as already-expired (caching for less time than the
        // operator's safety buffer is worse than not caching).
        let now = at(1000);
        let expires_at = at(1000 + 30);
        let d = cache_decision(
            Some(expires_at),
            Duration::from_secs(600),
            Duration::from_secs(60),
            now,
        );
        assert_eq!(d, CacheDecision::SkipCacheAlreadyExpired);
    }

    #[test]
    fn very_small_remaining_clamped_to_min_ttl() {
        // expires_at = now + 70 s; safety_margin = 60 s ⇒ remaining = 10 s.
        // Above MIN_EFFECTIVE_TTL_SECS = 5; caches for 10 s.
        let now = at(1000);
        let expires_at = at(1000 + 70);
        let d = cache_decision(
            Some(expires_at),
            Duration::from_secs(600),
            Duration::from_secs(60),
            now,
        );
        assert_eq!(d, CacheDecision::CacheFor(10));

        // expires_at = now + 62 s; remaining = 2 s; clamps up to MIN = 5.
        let expires_at = at(1000 + 62);
        let d = cache_decision(
            Some(expires_at),
            Duration::from_secs(600),
            Duration::from_secs(60),
            now,
        );
        assert_eq!(d, CacheDecision::CacheFor(5));
    }

    #[test]
    fn zero_safety_margin_treats_expires_at_as_hard_deadline() {
        // expires_at exactly equal to now + safety_margin (0) should skip.
        let now = at(1000);
        let d = cache_decision(
            Some(now),
            Duration::from_secs(600),
            Duration::from_secs(0),
            now,
        );
        assert_eq!(d, CacheDecision::SkipCacheAlreadyExpired);
    }

    // -------- Phase 7c: refresh-before-expiry --------

    #[test]
    fn should_refresh_returns_false_when_no_expires_at() {
        // Long-lived secrets without an issuer expiry stay on the
        // periodic-eviction path; never trigger a refresh.
        assert!(!should_refresh(None, Duration::from_secs(60), at(1000)));
    }

    #[test]
    fn should_refresh_returns_false_when_already_expired() {
        // expires_at past now — defensive.  Already-expired tokens go
        // through the cache eviction path (SkipCacheAlreadyExpired),
        // not the refresh path.
        let now = at(1000);
        let expires_at = at(990); // 10 s past
        assert!(!should_refresh(
            Some(expires_at),
            Duration::from_secs(60),
            now
        ));
    }

    #[test]
    fn should_refresh_returns_false_when_outside_window() {
        // expires_at far in the future — no refresh needed.
        let now = at(1000);
        let expires_at = at(2000); // 1000 s away
        assert!(!should_refresh(
            Some(expires_at),
            Duration::from_secs(60),
            now
        ));
    }

    #[test]
    fn should_refresh_returns_true_inside_window() {
        // expires_at = now + 30 s; window = 60 s → inside the window,
        // still valid → refresh.
        let now = at(1000);
        let expires_at = at(1030);
        assert!(should_refresh(
            Some(expires_at),
            Duration::from_secs(60),
            now
        ));
    }

    #[test]
    fn should_refresh_at_exact_window_boundary() {
        // Boundary case: expires_at = now + window.  The condition
        // `expires_at - window <= now` is `now <= now` → true.  Refresh.
        let now = at(1000);
        let expires_at = at(1060);
        assert!(should_refresh(
            Some(expires_at),
            Duration::from_secs(60),
            now
        ));
    }
}
