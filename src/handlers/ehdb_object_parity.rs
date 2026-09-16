//! Cross-store parity for the **object** and **KV** shadow tiers.
//!
//! **Prerequisite 2 of the KV/object cutover
//! ([noetl/ai-meta#348](https://github.com/noetl/ai-meta/issues/348)).**
//! It became buildable only once those tiers got a durable store: before that
//! their records lived on ephemeral container storage and were destroyed on
//! every pod roll, so every verdict after a restart would have read `missing`
//! until the shadow caught up.
//!
//! # The shape of the comparison, and the trap in it
//!
//! The shadow tier is an **append log of writes**. The authoritative store is
//! **current state**. A key is written many times — a state shard's
//! `open.feather` is rewritten on every seal — so the log legitimately holds
//! several records per key with different digests.
//!
//! ⚠ **Comparing every shadow record's digest against the current object would
//! false-alarm on every overwrite.** That is not a hypothetical: the first kind
//! run produced two records for one `open.feather` within seconds, digests
//! `34c90d64…` and `4ddeeb6d…`, both correct at the moment they were written.
//!
//! This is the [noetl/ai-meta#346](https://github.com/noetl/ai-meta/issues/346)
//! failure mode exactly — a comparator asserting a property the system never
//! promised, firing on benign behaviour, and teaching operators to ignore the
//! one instrument that guards a tier. There the assertion was arrival order;
//! here it would be "every record is current". Same mistake, different axis.
//!
//! So the rule is: **the LATEST shadow record per key is compared; earlier ones
//! are counted as superseded.** `superseded` is an observation, never a verdict.
//!
//! # What is compared
//!
//! | axis | check |
//! | :-- | :-- |
//! | digest | the latest shadow record's digest equals the authoritative object's |
//! | size | …and so does `byte_len` |
//! | presence | a key the shadow says was written has an authoritative object |
//!
//! # What deliberately is not
//!
//! **Authoritative objects with no shadow record are NOT a divergence.** The
//! shadow is written after the authoritative put succeeds, so the mirror is
//! always behind by design, and it only ever saw writes made while it was
//! armed — objects predating it have no record and never will. Counting that as
//! divergence would report a number that is mostly history.
//!
//! It is counted as `unmirrored`, which is the honest name and the number an
//! operator actually wants: it says how much of the store the shadow has
//! *seen*, which is the question a cutover decision turns on.

use std::collections::BTreeMap;

use serde::Serialize;

/// One authoritative object, as the object store reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoritativeObject {
    pub key: String,
    pub digest: String,
    pub byte_len: usize,
}

/// One shadow record, in the tier's append order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShadowRecord {
    /// The tier's own ordering. Later means newer.
    pub global_sequence: u64,
    pub key: String,
    pub digest: String,
    pub byte_len: usize,
}

/// A parity divergence and why it is one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObjectDivergence {
    pub kind: &'static str,
    pub key: String,
    pub detail: String,
}

/// The kinds, pinned so the control battery can assert one planted defect per
/// kind and a renamed kind fails the build rather than silently losing a class.
pub const DIVERGENCE_KINDS: &[&str] = &["digest", "size", "missing_object"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObjectParityReport {
    pub tier: &'static str,
    /// Distinct keys the shadow holds at least one record for.
    pub shadow_keys: usize,
    /// Keys compared — the latest record per key, where an authoritative object
    /// exists to compare it against.
    pub compared: usize,
    /// Keys whose latest record agreed on digest and size.
    pub matched: usize,
    /// Shadow records superseded by a later write to the same key.
    ///
    /// ⚠ An OBSERVATION, never a verdict. The shadow is an append log; a key
    /// rewritten twice has one current record and one superseded, and both were
    /// correct when written. Asserting otherwise is the noetl/ai-meta#346
    /// mistake on a new axis.
    pub superseded: usize,
    /// Authoritative objects the shadow has no record for.
    ///
    /// Not a divergence: the mirror only ever saw writes made while it was
    /// armed. This is the coverage number — how much of the store the shadow has
    /// seen — which is the question a cutover turns on.
    pub unmirrored: usize,
    pub divergences: Vec<ObjectDivergence>,
    pub holds: bool,
}

impl ObjectParityReport {
    pub fn kinds(&self) -> Vec<&'static str> {
        let mut k: Vec<&'static str> = self.divergences.iter().map(|d| d.kind).collect();
        k.sort_unstable();
        k.dedup();
        k
    }
    /// The metric outcome label for this report.
    pub fn outcome(&self) -> &'static str {
        if !self.divergences.is_empty() {
            "divergent"
        } else if self.compared == 0 {
            // Nothing to compare is not agreement. Reporting `match` here is how
            // a comparator that never ran reads as a clean one.
            "no_comparable"
        } else {
            "match"
        }
    }
}

/// Compare one tier's shadow records against the authoritative objects.
///
/// Pure and side-effect free — metrics are recorded by the caller, so the
/// control suite can drive this exact function without polluting live counters
/// with synthetic verdicts (the same discipline `ehdb_parity` uses).
pub fn compare_object_tier(
    tier: &'static str,
    authoritative: &[AuthoritativeObject],
    shadow: &[ShadowRecord],
) -> ObjectParityReport {
    let auth_by_key: BTreeMap<&str, &AuthoritativeObject> =
        authoritative.iter().map(|o| (o.key.as_str(), o)).collect();

    // Latest record per key, by the tier's own sequence. `superseded` is
    // everything this fold displaced.
    let mut latest: BTreeMap<&str, &ShadowRecord> = BTreeMap::new();
    let mut superseded = 0usize;
    for rec in shadow {
        match latest.get(rec.key.as_str()) {
            Some(prev) if prev.global_sequence >= rec.global_sequence => superseded += 1,
            Some(_) => {
                superseded += 1;
                latest.insert(rec.key.as_str(), rec);
            }
            None => {
                latest.insert(rec.key.as_str(), rec);
            }
        }
    }

    let mut divergences = Vec::new();
    let mut compared = 0usize;
    let mut matched = 0usize;

    for (key, rec) in &latest {
        let Some(auth) = auth_by_key.get(key) else {
            divergences.push(ObjectDivergence {
                kind: "missing_object",
                key: (*key).to_string(),
                detail: format!(
                    "the shadow records a write of {} bytes (digest {}) but the \
                     authoritative store holds no object at this key",
                    rec.byte_len,
                    short(&rec.digest)
                ),
            });
            continue;
        };
        compared += 1;
        let mut ok = true;
        if auth.digest != rec.digest {
            ok = false;
            divergences.push(ObjectDivergence {
                kind: "digest",
                key: (*key).to_string(),
                detail: format!(
                    "authoritative {} vs shadow {}",
                    short(&auth.digest),
                    short(&rec.digest)
                ),
            });
        }
        if auth.byte_len != rec.byte_len {
            ok = false;
            divergences.push(ObjectDivergence {
                kind: "size",
                key: (*key).to_string(),
                detail: format!(
                    "authoritative {} bytes vs shadow {} bytes",
                    auth.byte_len, rec.byte_len
                ),
            });
        }
        if ok {
            matched += 1;
        }
    }

    let unmirrored = authoritative
        .iter()
        .filter(|o| !latest.contains_key(o.key.as_str()))
        .count();

    ObjectParityReport {
        tier,
        shadow_keys: latest.len(),
        compared,
        matched,
        superseded,
        unmirrored,
        holds: divergences.is_empty(),
        divergences,
    }
}

fn short(digest: &str) -> String {
    digest.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth(key: &str, digest: &str, n: usize) -> AuthoritativeObject {
        AuthoritativeObject {
            key: key.to_string(),
            digest: digest.to_string(),
            byte_len: n,
        }
    }
    fn rec(seq: u64, key: &str, digest: &str, n: usize) -> ShadowRecord {
        ShadowRecord {
            global_sequence: seq,
            key: key.to_string(),
            digest: digest.to_string(),
            byte_len: n,
        }
    }

    /// The healthy case, modelled on the kind run: a results object and a state
    /// shard, digests agreeing with the source.
    fn healthy() -> (Vec<AuthoritativeObject>, Vec<ShadowRecord>) {
        (
            vec![
                auth("…/results/emit/0/0/1.json", "7404912ce859053a", 172_577),
                auth("…/state/open.feather", "4ddeeb6d67d359d1", 4_104),
            ],
            vec![
                rec(1, "…/state/open.feather", "34c90d6498876a30", 2_952),
                rec(2, "…/state/open.feather", "4ddeeb6d67d359d1", 4_104),
                rec(3, "…/results/emit/0/0/1.json", "7404912ce859053a", 172_577),
            ],
        )
    }

    #[test]
    fn identical_stores_hold() {
        let (a, s) = healthy();
        let r = compare_object_tier("object", &a, &s);
        assert!(r.holds, "{:?}", r.divergences);
        assert_eq!(r.outcome(), "match");
        assert_eq!(r.compared, 2);
        assert_eq!(r.matched, 2);
    }

    /// ⚠ THE FALSE ALARM THIS COMPARATOR IS BUILT TO AVOID.
    ///
    /// The shadow is an append log; the authoritative store is current state. A
    /// rewritten key leaves an older record whose digest no longer matches the
    /// object — and was perfectly correct when written. Both records in
    /// `healthy()` are real, taken from a kind run where one `open.feather` was
    /// written twice within seconds.
    ///
    /// Asserting "every record is current" here would be noetl/ai-meta#346's
    /// mistake on a new axis: a comparator firing on behaviour the system was
    /// designed to produce, until operators stop reading it.
    #[test]
    fn a_superseded_record_is_counted_not_called_a_divergence() {
        let (a, s) = healthy();
        let r = compare_object_tier("object", &a, &s);
        assert!(
            r.holds,
            "an overwritten key must not diverge — got {:?}",
            r.divergences
        );
        assert_eq!(
            r.superseded, 1,
            "the displaced record must still be COUNTED; losing it would hide a \
             key being rewritten far more often than expected"
        );
    }

    /// …and the observation must be a measurement, not a constant.
    #[test]
    fn a_store_with_no_rewrites_reports_no_superseded_records() {
        let a = vec![auth("k", "d1", 10)];
        let s = vec![rec(1, "k", "d1", 10)];
        let r = compare_object_tier("object", &a, &s);
        assert!(r.holds);
        assert_eq!(r.superseded, 0);
    }

    /// ⭐ THE CONTROL BATTERY. One planted **real** defect per kind.
    ///
    /// ⚠ noetl/ai-meta#346's lesson, which is why this test is written this way:
    /// the event-log comparator's `order` control planted a record that had
    /// merely *arrived late* — a non-defect — and reported CAUGHT. That is how a
    /// check false-alarming on 36.9% of prod traffic passed a ten-class battery.
    /// **A control that plants a non-defect proves only that the comparator
    /// reacts.** Every mutation below is something genuinely wrong.
    #[test]
    fn every_control_plants_a_real_defect_and_is_caught() {
        type Mutation = (&'static str, fn(&mut Vec<ShadowRecord>));
        let mutations: &[Mutation] = &[
            // The mirror stored bytes that differ from what the authoritative
            // store holds — silent corruption of the derived copy.
            ("digest", |s: &mut Vec<ShadowRecord>| {
                let last = s.len() - 1;
                s[last].digest = "deadbeefdeadbeef".to_string();
            }),
            // Same bytes claimed, different length — a truncated mirror.
            ("size", |s: &mut Vec<ShadowRecord>| {
                let last = s.len() - 1;
                s[last].byte_len = 1;
            }),
            // The shadow says a write happened that the authoritative store has
            // no record of — the mirror inventing history.
            ("missing_object", |s: &mut Vec<ShadowRecord>| {
                s.push(rec(99, "…/ghost.bin", "cafebabecafebabe", 7));
            }),
        ];
        assert_eq!(
            mutations.len(),
            DIVERGENCE_KINDS.len(),
            "a divergence kind has no control; the battery would certify a class \
             it never exercises"
        );
        for (kind, mutate) in mutations {
            let (a, mut s) = healthy();
            mutate(&mut s);
            let r = compare_object_tier("object", &a, &s);
            assert!(
                r.kinds().contains(kind),
                "control {kind} NOT DETECTED — report {r:?}"
            );
            assert!(!r.holds);
            assert_eq!(r.outcome(), "divergent");
        }
        // Positive control: the unmutated fixture holds, so the battery is
        // detecting the mutations rather than a fixture that never agreed.
        let (a, s) = healthy();
        assert!(compare_object_tier("object", &a, &s).holds);
    }

    /// An authoritative object the shadow never saw is coverage, not divergence.
    ///
    /// The mirror is armed at some point in time and only records writes after
    /// it. Counting everything older as divergence would report a number that is
    /// mostly history — and would make the metric unusable exactly when a tier
    /// is first armed.
    #[test]
    fn an_object_the_mirror_never_saw_is_coverage_not_divergence() {
        let (mut a, s) = healthy();
        a.push(auth("…/written/before/the/mirror.bin", "0123456789ab", 5));
        let r = compare_object_tier("object", &a, &s);
        assert!(r.holds, "{:?}", r.divergences);
        assert_eq!(r.unmirrored, 1);
        assert_eq!(r.outcome(), "match");
    }

    /// Nothing comparable must not read as agreement.
    ///
    /// A comparator that never ran and one that ran clean look identical unless
    /// the empty case has its own label. `ehdb_parity` learned this as
    /// `nothing_comparable_is_pending_not_agreement`.
    #[test]
    fn nothing_comparable_is_not_a_match() {
        let r = compare_object_tier("object", &[], &[]);
        assert!(r.holds);
        assert_eq!(
            r.outcome(),
            "no_comparable",
            "an empty comparison reported as `match` is a clean verdict about \
             nothing"
        );
        // And an authoritative store the shadow has not seen at all.
        let r = compare_object_tier("object", &[auth("k", "d", 1)], &[]);
        assert_eq!(r.outcome(), "no_comparable");
        assert_eq!(r.unmirrored, 1);
    }

    /// The same code serves the KV tier — the record shape is identical, only
    /// the label differs. KV has no production traffic yet, so this is the only
    /// thing asserting the path exists for it.
    #[test]
    fn the_kv_tier_uses_the_same_comparison() {
        let a = vec![auth("circuit/k1", "aa11", 3)];
        let s = vec![rec(1, "circuit/k1", "aa11", 3)];
        let r = compare_object_tier("kv", &a, &s);
        assert_eq!(r.tier, "kv");
        assert!(r.holds);
        assert_eq!(r.outcome(), "match");
    }

    /// Out-of-order arrival must not change which record is latest.
    ///
    /// The tier's `global_sequence` decides, not the position in the slice — the
    /// mirror is asynchronous and a caller could hand these over in any order.
    /// Getting this wrong would resurrect a superseded record as current and
    /// report a digest divergence on a healthy store: #346, a third time.
    #[test]
    fn the_latest_record_is_decided_by_sequence_not_slice_order() {
        let a = vec![auth("k", "new", 2)];
        let s = vec![rec(2, "k", "new", 2), rec(1, "k", "old", 1)];
        let r = compare_object_tier("object", &a, &s);
        assert!(
            r.holds,
            "slice order decided the latest record — {:?}",
            r.divergences
        );
        assert_eq!(r.superseded, 1);
    }
}

// ---------------------------------------------------------------------------
// The endpoint
// ---------------------------------------------------------------------------

/// `GET /api/ehdb/object-parity/{tier}` — run the comparison for real.
///
/// Shadow side: the tier service, through the same worker relay
/// [`raw_tier_query`][super::ehdb::raw_tier_query] uses — no new data access,
/// the same read with the JSON parsed instead of forwarded.
/// Authoritative side: [`ObjectBackend`], so it serves GCS and Postgres alike.
/// Reading the authoritative store with SQL would be blind on production, which
/// is noetl/server#438 and was nearly shipped twice.
///
/// ⚠ **`unmirrored` is only meaningful when the scope covers the authoritative
/// store.** Without a `prefix` this compares exactly the keys the shadow names,
/// so `unmirrored` is 0 by construction and would be a reassuring number about
/// nothing. The response says which scope ran; the number is only published
/// when it was computed against a listing.
pub async fn object_parity(
    axum::extract::State(deps): axum::extract::State<ObjectParityDeps>,
    axum::extract::Path(tier): axum::extract::Path<String>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> impl axum::response::IntoResponse {
    use axum::http::StatusCode;
    use axum::Json;

    let tier_label: &'static str = match tier.trim().to_ascii_lowercase().as_str() {
        "object" => "object",
        "kv" => "kv",
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "action": "ehdb.object_parity",
                    "error": format!("unsupported tier {other:?}"),
                    "supported": ["object", "kv"],
                })),
            );
        }
    };
    let limit: usize = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(500)
        .clamp(1, 5_000);

    let shadow = match super::ehdb::fetch_shadow_records(&deps.relay, tier_label, limit).await {
        Ok(v) => v,
        // A failed fetch must NOT read as an empty shadow: every key would
        // score `missing_object` and a refusal would be published as a finding
        // (noetl/ai-meta#263).
        Err(reason) => {
            crate::metrics::record_ehdb_crossstore_parity(tier_label, "shadow_unreadable");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "action": "ehdb.object_parity",
                    "tier": tier_label,
                    "outcome": "shadow_unreadable",
                    "error": reason,
                })),
            );
        }
    };

    // Authoritative side. With a prefix we list, so `unmirrored` means
    // something; without one we fetch exactly the keys the shadow names.
    let prefix = params.get("prefix").map(String::as_str);
    let mut keys: Vec<String> = match prefix {
        Some(p) => match deps.backend.list(&deps.pool, p, limit).await {
            Ok(k) => k,
            Err(e) => {
                crate::metrics::record_ehdb_crossstore_parity(tier_label, "authoritative_unreadable");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({
                        "action": "ehdb.object_parity",
                        "tier": tier_label,
                        "outcome": "authoritative_unreadable",
                        "error": e.to_string(),
                    })),
                );
            }
        },
        None => Vec::new(),
    };
    for r in &shadow {
        if !keys.iter().any(|k| k == &r.key) {
            keys.push(r.key.clone());
        }
    }

    let mut authoritative = Vec::with_capacity(keys.len());
    for key in &keys {
        match deps.backend.get(&deps.pool, key).await {
            Ok(Some(row)) => authoritative.push(AuthoritativeObject {
                key: key.clone(),
                digest: row.digest,
                byte_len: row.bytes.len(),
            }),
            // Absent is a real state — the comparator reports it as
            // `missing_object` for a key the shadow claims.
            Ok(None) => {}
            Err(e) => {
                crate::metrics::record_ehdb_crossstore_parity(tier_label, "authoritative_unreadable");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(serde_json::json!({
                        "action": "ehdb.object_parity",
                        "tier": tier_label,
                        "outcome": "authoritative_unreadable",
                        "key": key,
                        "error": e.to_string(),
                    })),
                );
            }
        }
    }

    let report = compare_object_tier(tier_label, &authoritative, &shadow);
    crate::metrics::record_ehdb_crossstore_parity(tier_label, report.outcome());

    let mut body = serde_json::to_value(&report).unwrap_or(serde_json::Value::Null);
    if let Some(o) = body.as_object_mut() {
        o.insert(
            "scope".to_string(),
            serde_json::json!(match prefix {
                Some(p) => format!("authoritative listing under {p:?} plus every key the shadow names"),
                None => "exactly the keys the shadow names".to_string(),
            }),
        );
        if prefix.is_none() {
            // Do not publish a 0 that only means "we never looked".
            o.remove("unmirrored");
        }
        o.insert("action".to_string(), serde_json::json!("ehdb.object_parity"));
        o.insert("outcome".to_string(), serde_json::json!(report.outcome()));
        o.insert("shadow_records".to_string(), serde_json::json!(shadow.len()));
    }
    (StatusCode::OK, Json(body))
}

/// Dependencies for [`object_parity`].
#[derive(Clone)]
pub struct ObjectParityDeps {
    pub pool: crate::db::DbPool,
    pub backend: crate::services::object_backend::ObjectBackend,
    pub relay: super::ehdb::TierRelayState,
}
