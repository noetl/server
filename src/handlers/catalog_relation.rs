//! The catalog relation — the read side of `docs/rfc/ehdb-catalog-relation-step3.md`.
//!
//! A relation is a **fold of a log**. This module is that fold: it takes catalog
//! log records and answers the five reads the catalog surface actually uses.
//!
//! # Pure on purpose
//!
//! Construction takes records and the reads take `&self`. No IO, no clock, no
//! environment. Every rule below is therefore reachable by a unit test rather
//! than only by a running server against a live tier — which is how the rules in
//! step 2 ended up asserted by nothing until they were extracted.
//!
//! # Serving nothing
//!
//! Nothing calls this on a read path. It exists to be folded and **compared**
//! against `noetl.catalog`; the flip to serving is a separate decision
//! (RFC §5).

use std::collections::BTreeMap;

/// Record types the fold understands.
pub const EVENT_REGISTERED: &str = "catalog.registered";
pub const EVENT_ARCHIVED: &str = "catalog.archived";
pub const EVENT_RESTORED: &str = "catalog.restored";

/// One entry in the relation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub catalog_id: i64,
    pub path: String,
    pub kind: String,
    /// `i32`, not the source's `smallint`. 32,767 versions per path is a real
    /// ceiling and an agent-driven registration loop is what finds it; widening
    /// here costs nothing and does not require touching the source column.
    pub version: i32,
    pub content_sha256: String,
    /// Derived from `catalog.archived` / `catalog.restored` applied in order —
    /// **not** a stored flag. That is what makes re-archiving idempotent by
    /// construction rather than by a `WHERE archived_at IS NULL` predicate
    /// happening to be written that way.
    pub archived: bool,
}

/// The folded catalog.
#[derive(Debug, Default, Clone)]
pub struct CatalogRelation {
    by_key: BTreeMap<(String, i32), Entry>,
    /// How many records the fold consumed, including ones it skipped. Reported
    /// so "the log was empty" and "the log was unparseable" are distinguishable.
    pub records_seen: usize,
    pub records_applied: usize,
}

impl CatalogRelation {
    /// Fold records **in the order given**.
    ///
    /// Order is the caller's responsibility and it is load-bearing: archive and
    /// restore are last-write-wins over the same key, so folding them out of
    /// sequence yields a different liveness answer. The tier returns records in
    /// `global_sequence` order.
    pub fn fold(records: &[serde_json::Value]) -> Self {
        let mut r = Self::default();
        for rec in records {
            r.records_seen += 1;
            let p = match rec.get("payload").and_then(|p| p.as_str()) {
                Some(s) => match serde_json::from_str::<serde_json::Value>(s) {
                    Ok(v) => v,
                    Err(_) => continue,
                },
                None => rec.clone(),
            };
            if r.apply(&p) {
                r.records_applied += 1;
            }
        }
        r
    }

    fn apply(&mut self, p: &serde_json::Value) -> bool {
        let Some(t) = p.get("event_type").and_then(|v| v.as_str()) else {
            return false;
        };
        let path = p.get("path").and_then(|v| v.as_str()).unwrap_or_default();
        if path.is_empty() {
            return false;
        }
        let version = p.get("version").and_then(|v| v.as_i64()).map(|v| v as i32);

        match t {
            EVENT_REGISTERED => {
                let (Some(version), Some(digest)) =
                    (version, p.get("content_sha256").and_then(|v| v.as_str()))
                else {
                    // A registration with no version or no digest is not a
                    // registration this fold can represent. Skipped rather than
                    // defaulted: an empty digest would compare as a mismatch
                    // against every source row.
                    return false;
                };
                self.by_key.insert(
                    (path.to_string(), version),
                    Entry {
                        catalog_id: p
                            .get("catalog_id")
                            .and_then(|v| {
                                v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                            })
                            .unwrap_or(0),
                        path: path.to_string(),
                        kind: p
                            .get("kind")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        version,
                        content_sha256: digest.to_string(),
                        archived: false,
                    },
                );
                true
            }
            EVENT_ARCHIVED | EVENT_RESTORED => {
                let archived = t == EVENT_ARCHIVED;
                match version {
                    // A version-scoped archive touches exactly that entry.
                    Some(v) => match self.by_key.get_mut(&(path.to_string(), v)) {
                        Some(e) => {
                            e.archived = archived;
                            true
                        }
                        None => false,
                    },
                    // An absent version means EVERY version of the path — the
                    // same semantics `CatalogDeleteRequest` documents, and the
                    // reason there is deliberately no "latest" shorthand.
                    None => {
                        let mut hit = false;
                        for ((p2, _), e) in self.by_key.iter_mut() {
                            if p2 == path {
                                e.archived = archived;
                                hit = true;
                            }
                        }
                        hit
                    }
                }
            }
            _ => false,
        }
    }

    /// Highest **non-archived** version of `path`.
    ///
    /// The archived filter is the whole subtlety: resolution by path is what
    /// every execution does, and an archived entry is retired.
    pub fn get_latest(&self, path: &str) -> Option<&Entry> {
        self.by_key
            .range((path.to_string(), i32::MIN)..=(path.to_string(), i32::MAX))
            .filter(|((p, _), e)| p == path && !e.archived)
            .next_back()
            .map(|(_, e)| e)
    }

    pub fn get(&self, path: &str, version: i32) -> Option<&Entry> {
        self.by_key.get(&(path.to_string(), version))
    }

    /// By surrogate key. Returns archived entries too — deliberately, matching
    /// `resolve_catalog`, so a historical version can be re-run on purpose.
    pub fn get_by_id(&self, catalog_id: i64) -> Option<&Entry> {
        self.by_key.values().find(|e| e.catalog_id == catalog_id)
    }

    /// Every version of `path`, ascending, archived included.
    pub fn list_versions(&self, path: &str) -> Vec<&Entry> {
        self.by_key
            .range((path.to_string(), i32::MIN)..=(path.to_string(), i32::MAX))
            .filter(|((p, _), _)| p == path)
            .map(|(_, e)| e)
            .collect()
    }

    /// Live entries, optionally filtered by kind.
    pub fn list_by_kind(&self, kind: Option<&str>) -> Vec<&Entry> {
        self.by_key
            .values()
            .filter(|e| !e.archived && kind.is_none_or(|k| e.kind == k))
            .collect()
    }

    /// Every entry, in key order.
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.by_key.values()
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }
    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg(path: &str, v: i64, sha: &str, kind: &str, id: i64) -> serde_json::Value {
        serde_json::json!({
            "event_type": EVENT_REGISTERED, "path": path, "version": v,
            "content_sha256": sha, "kind": kind, "catalog_id": id.to_string(),
        })
    }
    fn arch(path: &str, v: Option<i64>) -> serde_json::Value {
        let mut j = serde_json::json!({"event_type": EVENT_ARCHIVED, "path": path});
        if let Some(v) = v {
            j["version"] = serde_json::json!(v);
        }
        j
    }
    fn rest(path: &str, v: Option<i64>) -> serde_json::Value {
        let mut j = serde_json::json!({"event_type": EVENT_RESTORED, "path": path});
        if let Some(v) = v {
            j["version"] = serde_json::json!(v);
        }
        j
    }

    #[test]
    fn get_latest_is_the_highest_version() {
        let r = CatalogRelation::fold(&[
            reg("a", 1, "d1", "playbook", 1),
            reg("a", 3, "d3", "playbook", 3),
            reg("a", 2, "d2", "playbook", 2),
        ]);
        assert_eq!(r.get_latest("a").unwrap().version, 3);
        assert_eq!(r.get("a", 2).unwrap().content_sha256, "d2");
        assert_eq!(r.get_by_id(1).unwrap().version, 1);
        assert_eq!(
            r.list_versions("a").iter().map(|e| e.version).collect::<Vec<_>>(),
            vec![1, 2, 3],
            "list_versions must be ascending"
        );
    }

    /// ⭐ get_latest must SKIP archived entries.
    ///
    /// Resolution by path is what every execution does, and an archived entry is
    /// retired. A fold that agreed on every content digest while disagreeing
    /// about liveness would pass a digest-only comparator and then resolve
    /// get_latest to the wrong version — the exact trap the RFC names.
    #[test]
    fn get_latest_skips_archived_versions() {
        let r = CatalogRelation::fold(&[
            reg("a", 1, "d1", "playbook", 1),
            reg("a", 2, "d2", "playbook", 2),
            arch("a", Some(2)),
        ]);
        assert_eq!(
            r.get_latest("a").unwrap().version,
            1,
            "archiving v2 must make v1 the latest, not leave v2 resolving"
        );
        assert!(r.get("a", 2).is_some(), "the archived entry still EXISTS by key");
        assert!(r.get("a", 2).unwrap().archived);
    }

    /// An archive with no version retires every version of the path.
    #[test]
    fn a_versionless_archive_retires_the_whole_path() {
        let r = CatalogRelation::fold(&[
            reg("a", 1, "d1", "playbook", 1),
            reg("a", 2, "d2", "playbook", 2),
            reg("b", 1, "d3", "playbook", 3),
            arch("a", None),
        ]);
        assert!(r.get_latest("a").is_none(), "every version of `a` is retired");
        assert!(r.get_latest("b").is_some(), "a sibling path is untouched");
    }

    /// Archiving is idempotent, and restore is its inverse — by construction,
    /// because liveness is derived from the sequence rather than stored.
    #[test]
    fn archive_is_idempotent_and_restore_inverts_it() {
        let base = vec![reg("a", 1, "d1", "playbook", 1)];
        let twice = CatalogRelation::fold(
            &[base.clone(), vec![arch("a", Some(1)), arch("a", Some(1))]].concat(),
        );
        let once = CatalogRelation::fold(&[base.clone(), vec![arch("a", Some(1))]].concat());
        assert_eq!(twice.get("a", 1), once.get("a", 1));

        let restored =
            CatalogRelation::fold(&[base, vec![arch("a", Some(1)), rest("a", Some(1))]].concat());
        assert_eq!(restored.get_latest("a").unwrap().version, 1);
    }

    /// ⚠ ORDER is load-bearing: archive-then-restore and restore-then-archive
    /// are different states, so folding out of sequence changes the answer.
    #[test]
    fn order_decides_liveness() {
        let r1 = CatalogRelation::fold(&[
            reg("a", 1, "d", "playbook", 1),
            arch("a", Some(1)),
            rest("a", Some(1)),
        ]);
        let r2 = CatalogRelation::fold(&[
            reg("a", 1, "d", "playbook", 1),
            rest("a", Some(1)),
            arch("a", Some(1)),
        ]);
        assert!(r1.get_latest("a").is_some(), "restore last => live");
        assert!(r2.get_latest("a").is_none(), "archive last => retired");
    }

    #[test]
    fn list_by_kind_filters_and_excludes_archived() {
        let r = CatalogRelation::fold(&[
            reg("a", 1, "d1", "playbook", 1),
            reg("b", 1, "d2", "agent", 2),
            reg("c", 1, "d3", "playbook", 3),
            arch("c", Some(1)),
        ]);
        let pbs: Vec<_> = r.list_by_kind(Some("playbook")).iter().map(|e| e.path.clone()).collect();
        assert_eq!(pbs, vec!["a"], "archived `c` must not be listed");
        assert_eq!(r.list_by_kind(None).len(), 2);
        assert_eq!(r.list_by_kind(Some("agent")).len(), 1);
    }

    /// A malformed registration is SKIPPED, and the counters say so — "the log
    /// was empty" and "the log was unparseable" must not read the same.
    #[test]
    fn malformed_records_are_skipped_and_counted() {
        let mut nodigest = reg("a", 1, "d", "playbook", 1);
        nodigest.as_object_mut().unwrap().remove("content_sha256");
        let r = CatalogRelation::fold(&[nodigest, serde_json::json!({"nonsense": true})]);
        assert!(r.is_empty());
        assert_eq!(r.records_seen, 2);
        assert_eq!(
            r.records_applied, 0,
            "seen != applied is what distinguishes an unparseable log from an empty one"
        );
    }

    /// An archive for a path the fold has never seen changes nothing and is not
    /// counted as applied — it is a gap in coverage, not a state transition.
    #[test]
    fn an_archive_for_an_unknown_path_is_not_applied() {
        let r = CatalogRelation::fold(&[arch("ghost", Some(1))]);
        assert!(r.is_empty());
        assert_eq!(r.records_applied, 0);
    }

    /// ⭐ A BACKFILL record folds identically to a live registration.
    ///
    /// The whole point of the backfill emitting through the same record shape:
    /// an entry must be indistinguishable in the relation whether it arrived
    /// live or was seeded from an existing row. Only its provenance differs.
    #[test]
    fn a_backfilled_record_folds_the_same_as_a_live_one() {
        let live = reg("a", 7, "d7", "playbook", 77);
        let mut back = live.clone();
        back["backfilled"] = serde_json::json!(true);
        assert_eq!(
            CatalogRelation::fold(&[live]).get("a", 7),
            CatalogRelation::fold(&[back]).get("a", 7),
            "a backfilled entry must fold identically; only provenance differs"
        );
    }

    /// ⭐ The backfill must NOT bump versions.
    ///
    /// Re-registering the existing catalog through the normal path would run
    /// MAX(version)+1 and create a second version of every entry — describing a
    /// 2,518-row catalog by doubling it. This pins the property the backfill
    /// preserves: the folded version is the SOURCE version.
    #[test]
    fn the_backfill_preserves_the_source_version() {
        let r = CatalogRelation::fold(&[
            reg("a", 3, "d3", "playbook", 33),
            reg("a", 4, "d4", "playbook", 44),
        ]);
        assert_eq!(
            r.list_versions("a").iter().map(|e| e.version).collect::<Vec<_>>(),
            vec![3, 4],
            "backfilled versions must be the source's own, not 1..n"
        );
        assert_eq!(r.get_latest("a").unwrap().version, 4);
        assert!(r.get("a", 1).is_none(), "no phantom version was invented");
    }

    /// Re-folding a log that already contains an entry leaves the relation
    /// unchanged — the property the backfill's diff relies on.
    #[test]
    fn re_emitting_an_identical_record_does_not_change_the_relation() {
        let one = CatalogRelation::fold(&[reg("a", 1, "d1", "playbook", 1)]);
        let twice = CatalogRelation::fold(&[
            reg("a", 1, "d1", "playbook", 1),
            reg("a", 1, "d1", "playbook", 1),
        ]);
        assert_eq!(one.get("a", 1), twice.get("a", 1));
        assert_eq!(one.len(), twice.len(), "a duplicate must not create a second entry");
    }

    /// Records wrapped in a tier `payload` string fold identically to inline ones.
    #[test]
    fn wrapped_and_inline_records_fold_identically() {
        let inline = reg("a", 1, "d1", "playbook", 1);
        let wrapped = serde_json::json!({"payload": inline.to_string()});
        assert_eq!(
            CatalogRelation::fold(&[inline]).get("a", 1),
            CatalogRelation::fold(&[wrapped]).get("a", 1)
        );
    }
}
