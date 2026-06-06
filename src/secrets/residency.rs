//! Residency policy enforcement (Secrets Wallet Phase 6c,
//! [`noetl/ai-meta#61`](https://github.com/noetl/ai-meta/issues/61)).
//!
//! A credential tagged `region: eu-central-1` exists for a reason —
//! usually data-residency obligations (GDPR's "data must stay in the
//! EU") or contractual constraints.  Phase 6a/b ensured the *fetch* is
//! routed to that region's endpoint, but the resolved cleartext still
//! lands in this server's memory.  When that server runs outside the
//! credential's home jurisdiction, the cleartext has effectively
//! crossed the boundary.
//!
//! This module is the gate that prevents the crossing.
//!
//! Three policies:
//!
//! - [`Residency::None`] — no check (default; back-compat for entries
//!   that pre-date 6c).
//! - [`Residency::Advisory`] — check, but on mismatch let the resolution
//!   proceed AND record the violation on the metric.  Operator-facing
//!   surface for the migration period before flipping a credential to
//!   `strict`.
//! - [`Residency::Strict`] — check; on mismatch the resolver short-circuits
//!   with [`crate::error::AppError::ResidencyViolation`] BEFORE any
//!   provider call.  Cleartext never enters this server's memory.
//!
//! Cross-region routing is the natural follow-up (Phase 6e): "if a server
//! in A is denied a credential whose home is B, route the request through
//! a broker in B that re-seals to A's worker."  Until that ships, `strict`
//! mode is a fail-closed boundary, not a routed-around constraint.

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::metrics::record_secret_residency_check;
use crate::playbook::types::KeychainDef;
use crate::secrets::server_region;

/// Residency policy for a [`KeychainDef`].  See module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Residency {
    /// No check; resolution proceeds regardless of server region.
    /// Back-compat default for entries without an explicit policy.
    #[default]
    None,
    /// Check but proceed on mismatch — observability without enforcement.
    /// Used during the migration window before flipping to `strict`.
    Advisory,
    /// Fail-closed on mismatch; resolution short-circuits with
    /// [`AppError::ResidencyViolation`] before any provider call.
    Strict,
}

impl Residency {
    fn as_label(self) -> &'static str {
        match self {
            Residency::None => "none",
            Residency::Advisory => "advisory",
            Residency::Strict => "strict",
        }
    }
}

/// Outcome of [`evaluate`] — used by the resolver to decide whether to
/// proceed and which metric label to record.  Not `Clone`/`PartialEq`
/// because [`AppError`] isn't — `Deny`'s payload is consumed by the
/// resolver (`to_result` moves the error out) so no caller needs to
/// copy or compare a denial.
#[derive(Debug)]
pub enum ResidencyDecision {
    /// Resolution allowed (with the matching decision label for the metric).
    Allow(&'static str),
    /// Resolution allowed despite a region mismatch (advisory mode);
    /// metric records `violation_allowed`.
    AllowWithViolationLogged,
    /// Resolution denied (strict mode + mismatch).  The resolver returns
    /// the wrapped error verbatim.
    Deny(AppError),
}

/// Evaluate the residency policy for one resolution attempt.
///
/// - `entry_region` is what [`crate::secrets::resolver::effective_region`]
///   already computed (KeychainDef.region OR `NOETL_SERVER_REGION` OR empty).
/// - This server's home region comes from [`server_region`].
///
/// Both empty strings ⇒ legacy / unconfigured deployment: treat as
/// `Allow("allowed_no_policy")` regardless of policy, since there's no
/// region claim to enforce against.
pub fn evaluate(kc: &KeychainDef, entry_region: &str) -> ResidencyDecision {
    let policy = kc.residency;
    let server_region = server_region();

    // No region claim on the entry ⇒ no policy to evaluate.  Same for
    // strict mode when neither side has a region (operator hasn't
    // declared the boundary yet).
    if entry_region.is_empty() {
        let decision = ResidencyDecision::Allow("allowed_no_policy");
        record_secret_residency_check(policy.as_label(), label_of(&decision));
        return decision;
    }

    let decision = match policy {
        Residency::None => ResidencyDecision::Allow("allowed_no_policy"),
        Residency::Advisory | Residency::Strict => {
            if entry_region == server_region {
                ResidencyDecision::Allow("allowed_same_region")
            } else if kc
                .allowed_regions
                .iter()
                .any(|r| r.as_str() == server_region && !server_region.is_empty())
            {
                ResidencyDecision::Allow("allowed_in_allowlist")
            } else {
                match policy {
                    Residency::Strict => ResidencyDecision::Deny(AppError::ResidencyViolation {
                        credential: kc.name.clone(),
                        entry_region: entry_region.to_string(),
                        server_region: server_region.to_string(),
                    }),
                    _ => ResidencyDecision::AllowWithViolationLogged,
                }
            }
        }
    };

    record_secret_residency_check(policy.as_label(), label_of(&decision));
    decision
}

/// Map a decision to its metric label.
fn label_of(d: &ResidencyDecision) -> &'static str {
    match d {
        ResidencyDecision::Allow(l) => l,
        ResidencyDecision::AllowWithViolationLogged => "violation_allowed",
        ResidencyDecision::Deny(_) => "violation_blocked",
    }
}

/// Resolver-side convenience: take a [`ResidencyDecision`] and convert it
/// to `AppResult<()>`.  Allow / AllowWithViolationLogged ⇒ `Ok(())`;
/// `Deny` ⇒ propagate the error.
pub fn to_result(d: ResidencyDecision) -> AppResult<()> {
    match d {
        ResidencyDecision::Allow(_) | ResidencyDecision::AllowWithViolationLogged => Ok(()),
        ResidencyDecision::Deny(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kc(name: &str, region: Option<&str>, residency: Residency, allowed: &[&str]) -> KeychainDef {
        KeychainDef {
            name: name.to_string(),
            credential: None,
            token_type: None,
            scope: None,
            provider: None,
            auth: None,
            map: None,
            region: region.map(|s| s.to_string()),
            residency,
            allowed_regions: allowed.iter().map(|s| s.to_string()).collect(),
            no_broker_fallback: false,
            auto_renew: false,
            extra: Default::default(),
        }
    }

    // server_region() is a process-global OnceLock that reads
    // NOETL_SERVER_REGION at init time.  In the test process it's empty
    // by default — most tests below pin the entry_region argument so the
    // empty server region exercises the "mismatch" path deterministically.

    #[test]
    fn none_policy_allows_everything() {
        let entry = kc("eu_token", Some("eu-central-1"), Residency::None, &[]);
        let d = evaluate(&entry, "eu-central-1");
        assert!(matches!(d, ResidencyDecision::Allow("allowed_no_policy")));
        // Even with a server-side mismatch, none still allows.
        let d = evaluate(&entry, "us-east-1");
        assert!(matches!(d, ResidencyDecision::Allow("allowed_no_policy")));
    }

    #[test]
    fn strict_same_region_allows() {
        // server_region() is "" (env unset in tests); when both sides
        // claim the empty string they trivially match — but the gate
        // short-circuits on empty `entry_region` before that check.
        // Use a non-empty entry_region that happens to match server_region.
        let entry = kc("local", Some(server_region()), Residency::Strict, &[]);
        if server_region().is_empty() {
            // Treated as "no region claim" by the early-return guard.
            let d = evaluate(&entry, server_region());
            assert!(matches!(d, ResidencyDecision::Allow("allowed_no_policy")));
        } else {
            let d = evaluate(&entry, server_region());
            assert!(matches!(d, ResidencyDecision::Allow("allowed_same_region")));
        }
    }

    #[test]
    fn strict_mismatch_denies_with_residency_violation() {
        // Entry in eu-central-1, server region is empty (or whatever
        // env is set to) — guaranteed mismatch when entry_region is
        // non-empty.
        let entry = kc("eu_token", Some("eu-central-1"), Residency::Strict, &[]);
        let d = evaluate(&entry, "eu-central-1");
        // server_region() may or may not equal eu-central-1; only
        // exercise the deny branch when the environment guarantees
        // a mismatch.
        if server_region() != "eu-central-1" {
            match d {
                ResidencyDecision::Deny(AppError::ResidencyViolation {
                    credential,
                    entry_region,
                    server_region: srv,
                }) => {
                    assert_eq!(credential, "eu_token");
                    assert_eq!(entry_region, "eu-central-1");
                    assert_eq!(srv, server_region());
                }
                other => panic!("expected ResidencyViolation, got {other:?}"),
            }
        }
    }

    #[test]
    fn strict_allowlist_hit_allows_when_server_region_matches() {
        // The allowlist branch only fires when server_region() is in the
        // list — which means we need a known server region.  Skip when
        // the env doesn't supply one.
        let srv = server_region();
        if srv.is_empty() {
            return;
        }
        let entry = kc("eu_token", Some("eu-central-1"), Residency::Strict, &[srv]);
        let d = evaluate(&entry, "eu-central-1");
        assert!(matches!(
            d,
            ResidencyDecision::Allow("allowed_in_allowlist")
        ));
    }

    #[test]
    fn advisory_mismatch_allows_and_records_violation() {
        let entry = kc("eu_token", Some("eu-central-1"), Residency::Advisory, &[]);
        if server_region() != "eu-central-1" {
            let d = evaluate(&entry, "eu-central-1");
            assert!(matches!(d, ResidencyDecision::AllowWithViolationLogged));
            // to_result still says Ok on advisory.
            assert!(to_result(d).is_ok());
        }
    }

    #[test]
    fn empty_entry_region_short_circuits_to_allow_no_policy() {
        // No region claim ⇒ no policy to enforce, even under strict.
        let entry = kc("legacy", None, Residency::Strict, &[]);
        let d = evaluate(&entry, "");
        assert!(matches!(d, ResidencyDecision::Allow("allowed_no_policy")));
    }

    #[test]
    fn empty_allowlist_entry_does_not_falsely_match_empty_server_region() {
        // Defensive: an empty server region must NOT match an empty
        // string accidentally present in allowed_regions.  Add an empty
        // string to the allowlist and a non-matching entry region.
        let entry = kc("eu_token", Some("eu-central-1"), Residency::Strict, &[""]);
        if server_region().is_empty() {
            let d = evaluate(&entry, "eu-central-1");
            assert!(
                matches!(d, ResidencyDecision::Deny(_)),
                "empty string in allowlist must not match empty server region"
            );
        }
    }

    #[test]
    fn to_result_propagates_deny() {
        let err = AppError::ResidencyViolation {
            credential: "c".to_string(),
            entry_region: "eu".to_string(),
            server_region: "us".to_string(),
        };
        let r = to_result(ResidencyDecision::Deny(err));
        match r {
            Err(AppError::ResidencyViolation { .. }) => {}
            other => panic!("expected ResidencyViolation, got {other:?}"),
        }
    }
}
