//! Where a catalog `get_latest` resolves from (`docs/rfc/ehdb-catalog-relation-step3.md` §5).
//!
//! **Staged, not flipped.** The default is `postgres`, which is byte-identical
//! to today and costs a single branch. The cutover is a flag flip, and it is an
//! owner decision.
//!
//! # The ladder
//!
//! | mode | behaviour |
//! | :-- | :-- |
//! | `postgres` *(default)* | today. No fold, no relay call, no extra query. |
//! | `verify` | answer from Postgres **and** the relation, compare, record — serve **Postgres**. |
//! | `tier` | serve the relation, falling back to Postgres when it has no answer. |
//!
//! Same shape as `ehdb_projection_read`'s ladder, deliberately: that one is
//! live on prod and the discipline it encodes — measure before serving — is
//! what made noetl/ai-meta#307 safe to enable.
//!
//! # Why the relation is cached
//!
//! A fold is over the whole catalog log — 1,601 records on prod, read in pages
//! because the tier-service frame caps at 1 MiB. Folding that per execution
//! would make `verify` far more expensive than the read it is checking, and an
//! observability mode that degrades the thing it observes gets switched off and
//! learns nothing. The fold is cached with a TTL, so `verify` costs at most one
//! fold per interval.
//!
//! ⚠ The cache is the reason `tier` is not simply "flip it and go": a cached
//! relation is **stale by construction** for up to the TTL, which is exactly the
//! read-your-writes problem the RFC names (register-then-run). Serving from a
//! cached fold without a read barrier would sometimes run the previous version —
//! and that looks exactly like a successful run.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// `NOETL_CATALOG_READ_SOURCE` — where `get_latest` resolves from.
pub const MODE_ENV: &str = "NOETL_CATALOG_READ_SOURCE";

/// `NOETL_CATALOG_READ_CACHE_SECS` — how long a fold is reused.
pub const CACHE_SECS_ENV: &str = "NOETL_CATALOG_READ_CACHE_SECS";

pub const DEFAULT_CACHE_SECS: u64 = 60;

/// Modes, pinned as metric labels.
pub const MODES: [&str; 3] = ["postgres", "verify", "tier"];

/// Comparison outcomes, pinned as metric labels.
///
/// `fold_missing` is deliberately distinct from `disagree`: "the relation does
/// not have this path yet" and "the relation has a different answer" want
/// opposite responses — the first is a coverage gap, the second is a fault.
pub const OUTCOMES: [&str; 5] = [
    "agree",
    "disagree",
    "fold_missing",
    "source_missing",
    "fold_unavailable",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Postgres,
    Verify,
    Tier,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Verify => "verify",
            Self::Tier => "tier",
        }
    }
    /// Whether the relation is consulted at all.
    pub fn consults_relation(self) -> bool {
        !matches!(self, Self::Postgres)
    }
    /// Whether the relation's answer may be **served**.
    pub fn serves_relation(self) -> bool {
        matches!(self, Self::Tier)
    }
}

pub fn mode() -> Mode {
    parse_mode(std::env::var(MODE_ENV).ok().as_deref())
}

/// The parse, without the environment — testable without `set_var`, which races.
///
/// Unrecognised resolves to `Postgres`, never forward to `tier`: a typo must not
/// silently move catalog resolution onto an unproven path.
pub fn parse_mode(raw: Option<&str>) -> Mode {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("verify") => Mode::Verify,
        Some("tier") => Mode::Tier,
        _ => Mode::Postgres,
    }
}

fn cache_ttl() -> Duration {
    Duration::from_secs(
        std::env::var(CACHE_SECS_ENV)
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_CACHE_SECS),
    )
}

type Cached = Option<(Instant, crate::handlers::catalog_relation::CatalogRelation)>;

fn cache() -> &'static Mutex<Cached> {
    static C: OnceLock<Mutex<Cached>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

/// The folded relation, refreshed at most once per TTL.
///
/// `None` when the fold could not be produced — which is reported as
/// `fold_unavailable` rather than as an empty relation, because an unreachable
/// log and an empty one must not read the same. That conflation cost a full gate
/// round on the catalog-log verifier.
pub async fn cached_relation() -> Option<crate::handlers::catalog_relation::CatalogRelation> {
    {
        let g = cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, rel)) = g.as_ref() {
            if at.elapsed() < cache_ttl() {
                return Some(rel.clone());
            }
        }
    }
    let fresh =
        crate::handlers::catalog_log::read_and_fold(crate::handlers::catalog_log::FOLD_READ_CAP)
            .await
            .ok()?;
    let mut g = cache().lock().unwrap_or_else(|e| e.into_inner());
    *g = Some((Instant::now(), fresh.clone()));
    Some(fresh)
}

/// Compare the relation's `get_latest` against the incumbent answer.
///
/// Returns the outcome label. **Never returns the relation's answer for use** —
/// serving is a separate decision and a separate mode.
/// The answer `tier` mode serves, or `None` to keep the incumbent.
///
/// ⚠⚠ **Fail-closed by construction.** The relation's answer is served *only*
/// when it AGREES with the incumbent on the resolved version. Every other
/// outcome — the relation missing the entry (`fold_missing`), unavailable
/// (`fold_unavailable`), holding an entry the source retired
/// (`source_missing`), or disagreeing on the version — keeps the incumbent.
///
/// That makes the cutover observationally identical to `postgres` in steady
/// state, which is exactly what a safe cutover should look like: the relation
/// enters the serving path and its failures become visible, without any window
/// in which it can serve a *worse* answer than the database.
///
/// ⚠ The case this specifically protects against is the read-your-writes window:
/// `cached_relation()` is up to `NOETL_CATALOG_READ_CACHE_SECS` (default 60s)
/// stale, so a playbook registered seconds ago resolves as `fold_missing` — and
/// a version of this that served the relation regardless would fail to find a
/// playbook that demonstrably exists.
pub fn serve_decision<'a>(
    mode: Mode,
    relation: Option<&'a crate::handlers::catalog_relation::CatalogRelation>,
    outcome: &str,
    path: &str,
) -> Option<&'a crate::handlers::catalog_relation::Entry> {
    if !mode.serves_relation() || outcome != "agree" {
        return None;
    }
    relation.and_then(|r| r.get_latest(path))
}

/// Which source actually answered. Pinned so `relation` reading 0 is
/// distinguishable from the family being absent.
pub const SERVED_BY: [&str; 2] = ["incumbent", "relation"];

pub fn compare_latest(
    relation: Option<&crate::handlers::catalog_relation::CatalogRelation>,
    path: &str,
    incumbent_version: Option<i32>,
) -> &'static str {
    let Some(rel) = relation else {
        return "fold_unavailable";
    };
    match (rel.get_latest(path), incumbent_version) {
        (Some(e), Some(v)) if e.version == v => "agree",
        (Some(_), Some(_)) => "disagree",
        // The relation has an answer the source does not — the log claims a
        // registration the catalog has retired or never had.
        (Some(_), None) => "source_missing",
        // The source has an answer the relation does not: a coverage gap, which
        // the backfill exists to close. Not a fault.
        (None, _) => "fold_missing",
    }
}

/// Record one comparison.
pub fn record(outcome: &str) {
    crate::metrics::record_catalog_relation_read("get_latest", outcome);
}

/// Which source actually answered this resolution.
///
/// ⚠ The number that makes a cutover demonstrable rather than asserted: under
/// `tier`, `served_by="relation"` climbing is the only evidence the relation is
/// really in the serving path. Under `verify` it stays at `incumbent` by
/// construction, which is the control.
pub fn record_served(by: &str) {
    crate::metrics::record_catalog_read_served(by);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::catalog_relation::CatalogRelation;

    fn rel(path: &str, versions: &[(i64, bool)]) -> CatalogRelation {
        let mut recs = Vec::new();
        for (v, archived) in versions {
            recs.push(serde_json::json!({
                "event_type": "catalog.registered", "path": path, "version": v,
                "content_sha256": format!("d{v}"), "kind": "playbook", "catalog_id": "1",
            }));
            if *archived {
                recs.push(serde_json::json!({
                    "event_type": "catalog.archived", "path": path, "version": v
                }));
            }
        }
        CatalogRelation::fold(&recs)
    }

    /// The default is postgres, so this lands without changing resolution.
    #[test]
    fn the_default_is_postgres() {
        assert_eq!(parse_mode(None), Mode::Postgres);
        assert_eq!(parse_mode(Some("")), Mode::Postgres);
        assert!(!Mode::Postgres.consults_relation());
        assert!(!Mode::Postgres.serves_relation());
    }

    /// A typo must not move catalog resolution onto the relation.
    #[test]
    fn an_unrecognised_mode_does_not_move_resolution() {
        for junk in ["teir", "TIER!", "relation", "on", "true", "1"] {
            assert_eq!(parse_mode(Some(junk)), Mode::Postgres, "{junk}");
        }
        assert_eq!(parse_mode(Some(" Verify ")), Mode::Verify);
        assert_eq!(parse_mode(Some("TIER")), Mode::Tier);
    }

    /// ⭐ `verify` observes; only `tier` serves.
    #[test]
    fn verify_never_serves() {
        assert!(Mode::Verify.consults_relation());
        assert!(
            !Mode::Verify.serves_relation(),
            "verify is the measure-before-serving rung; if it serves, the rung does not exist"
        );
        assert!(Mode::Tier.serves_relation());
    }

    #[test]
    fn agreement_is_on_the_resolved_version() {
        let r = rel("a", &[(1, false), (2, false)]);
        assert_eq!(compare_latest(Some(&r), "a", Some(2)), "agree");
        assert_eq!(compare_latest(Some(&r), "a", Some(1)), "disagree");
    }

    /// ⚠ An archived latest changes the answer, and the comparison must see it.
    ///
    /// This is the trap the RFC names: a comparison on content alone would agree
    /// while the two disagreed about which version resolves.
    #[test]
    fn archiving_the_latest_changes_what_agreement_means() {
        let r = rel("a", &[(1, false), (2, true)]);
        assert_eq!(
            compare_latest(Some(&r), "a", Some(1)),
            "agree",
            "with v2 archived, v1 is the latest — agreeing with a source that says v1"
        );
        assert_eq!(
            compare_latest(Some(&r), "a", Some(2)),
            "disagree",
            "a source still resolving to the archived v2 is a real disagreement"
        );
    }

    /// A coverage gap is NOT a fault, and must not be reported as one.
    #[test]
    fn a_missing_path_is_a_coverage_gap_not_a_disagreement() {
        let r = rel("a", &[(1, false)]);
        assert_eq!(
            compare_latest(Some(&r), "unknown", Some(1)),
            "fold_missing",
            "the relation not having a path yet is what the backfill closes, not a fault"
        );
        assert_ne!(compare_latest(Some(&r), "unknown", Some(1)), "disagree");
    }

    /// An unreachable fold is not an empty one.
    #[test]
    fn an_unavailable_fold_is_distinguishable_from_an_empty_one() {
        assert_eq!(compare_latest(None, "a", Some(1)), "fold_unavailable");
        let empty = CatalogRelation::fold(&[]);
        assert_eq!(compare_latest(Some(&empty), "a", Some(1)), "fold_missing");
    }

    #[test]
    fn every_label_is_pinned() {
        for m in [Mode::Postgres, Mode::Verify, Mode::Tier] {
            let e = match m {
                Mode::Postgres => "postgres",
                Mode::Verify => "verify",
                Mode::Tier => "tier",
            };
            assert_eq!(m.as_str(), e);
            assert!(MODES.contains(&m.as_str()));
        }
        assert_eq!(MODES.len(), 3);
        assert_eq!(OUTCOMES.len(), 5);
        for o in [
            "agree",
            "disagree",
            "fold_missing",
            "source_missing",
            "fold_unavailable",
        ] {
            assert!(OUTCOMES.contains(&o));
        }
    }
}

#[cfg(test)]
mod serve_decision_tests {
    use super::*;
    use crate::handlers::catalog_relation::CatalogRelation;

    fn rel_with(path: &str, version: i32) -> CatalogRelation {
        CatalogRelation::fold(&[serde_json::json!({
            "event_type": "catalog.registered", "path": path, "version": version,
            "content_sha256": "d1", "kind": "playbook", "catalog_id": "42",
        })])
    }

    #[test]
    fn verify_never_serves_the_relation() {
        // ⭐ The control that keeps `verify` an observation mode. If this ever
        // fails, enabling `verify` has silently become a cutover.
        let r = rel_with("p", 3);
        assert!(serve_decision(Mode::Verify, Some(&r), "agree", "p").is_none());
        assert!(serve_decision(Mode::Postgres, Some(&r), "agree", "p").is_none());
    }

    #[test]
    fn tier_serves_only_on_agree() {
        let r = rel_with("p", 3);
        assert!(
            serve_decision(Mode::Tier, Some(&r), "agree", "p").is_some(),
            "the positive case — otherwise the cutover serves nothing and is a no-op"
        );
        // ⚠⚠ Every other outcome must keep the incumbent. `fold_missing` is the
        // read-your-writes window: a playbook registered inside the 60s cache
        // window is absent from the relation, and serving that absence would
        // fail to find a playbook that demonstrably exists.
        for bad in [
            "fold_missing",
            "fold_unavailable",
            "source_missing",
            "disagree",
        ] {
            assert!(
                serve_decision(Mode::Tier, Some(&r), bad, "p").is_none(),
                "tier must NOT serve on {bad}"
            );
        }
    }

    #[test]
    fn tier_with_no_relation_keeps_the_incumbent() {
        assert!(serve_decision(Mode::Tier, None, "agree", "p").is_none());
    }

    #[test]
    fn tier_serves_the_relations_own_catalog_id() {
        // The substitution must actually take the relation's value — returning
        // the incumbent's would make the cutover invisible and untestable.
        let r = rel_with("p", 3);
        let e = serve_decision(Mode::Tier, Some(&r), "agree", "p").expect("serves");
        assert_eq!(e.catalog_id, 42);
        assert_eq!(e.path, "p");
    }

    #[test]
    fn the_served_by_labels_are_the_two_that_get_pinned() {
        assert_eq!(SERVED_BY.len(), 2);
        assert!(SERVED_BY.contains(&"incumbent"));
        assert!(SERVED_BY.contains(&"relation"));
    }
}
