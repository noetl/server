//! noetl/ai-meta#506 P0b — a bloom false positive must not pin the retention floor.
//!
//! The shape is the one measured on prod 2026-11-03: the oldest part holds only a COLD
//! execution, and a HOT execution matched its bloom by chance, so the bloom footprint put the
//! hot one's first sequence at that part and the floor sat there for days.

use std::collections::HashSet;

use noetl_server::services::event_archive::{
    refine_floor_holders, HotExecution, MAX_FLOOR_REFINEMENTS,
};

fn h(id: i64, min: u64, max: u64) -> HotExecution {
    HotExecution {
        execution_id: id,
        min_sequence: min,
        max_sequence: max,
    }
}

fn floor(fp: &[HotExecution], archived: &HashSet<i64>) -> Option<u64> {
    fp.iter()
        .filter(|e| !archived.contains(&e.execution_id))
        .map(|e| e.min_sequence)
        .min()
}

/// The prod shape: hot id 2 bloom-matches the old part at 100; its real records start at 900.
#[test]
fn a_false_positive_holder_is_corrected_to_its_real_first_record() {
    let mut fp = vec![h(1, 100, 110), h(2, 100, 950), h(3, 800, 990)];
    let archived: HashSet<i64> = [1].into(); // the cold execution actually in the old part
    assert_eq!(
        floor(&fp, &archived),
        Some(100),
        "precondition: the bloom floor is pinned"
    );

    let r = refine_floor_holders(
        &mut fp,
        &archived,
        |id| match id {
            2 => Some(Some((900, 950))),
            3 => Some(Some((800, 990))),
            _ => panic!("refined a non-holder {id}"),
        },
        MAX_FLOOR_REFINEMENTS,
    );

    // 2 was the holder and moved to 900, which made 3 (at 800) the holder; 3 is exact too.
    assert_eq!(floor(&fp, &archived), Some(800));
    assert_eq!(
        (r.refined, r.absent, r.unreadable, r.bounded),
        (2, 0, false, false)
    );
}

/// Positive control for safety: a holder whose records REALLY start low keeps the floor low.
/// Without this, a refiner that always raised the floor would pass the test above.
#[test]
fn a_genuine_holder_keeps_the_floor_where_its_data_is() {
    let mut fp = vec![h(2, 100, 950)];
    let r = refine_floor_holders(&mut fp, &HashSet::new(), |_| Some(Some((100, 950))), 8);
    assert_eq!(floor(&fp, &HashSet::new()), Some(100));
    assert_eq!(r.refined, 1);
}

/// A hot id the engine holds nothing for constrains nothing.
#[test]
fn a_holder_with_no_records_is_dropped() {
    let mut fp = vec![h(2, 100, 950), h(3, 700, 990)];
    let r = refine_floor_holders(
        &mut fp,
        &HashSet::new(),
        |id| match id {
            2 => Some(None),
            _ => Some(Some((700, 990))),
        },
        8,
    );
    assert_eq!(floor(&fp, &HashSet::new()), Some(700));
    assert_eq!((r.absent, r.refined), (1, 1));
}

/// An unreadable engine keeps the bloom floor: the conservative answer, never a higher one.
#[test]
fn unreadable_keeps_the_bloom_floor() {
    let mut fp = vec![h(2, 100, 950)];
    let r = refine_floor_holders(&mut fp, &HashSet::new(), |_| None, 8);
    assert_eq!(floor(&fp, &HashSet::new()), Some(100));
    assert!(r.unreadable);
}

/// The read budget bounds the work; when it runs out the floor is whatever is lowest so far,
/// which is never above the true floor because every unrefined entry is a lower bound.
#[test]
fn the_read_budget_is_bounded_and_stays_conservative() {
    let mut fp: Vec<HotExecution> = (0..10).map(|i| h(i, 100 + i as u64, 1000)).collect();
    let r = refine_floor_holders(&mut fp, &HashSet::new(), |_| Some(Some((999, 1000))), 3);
    assert!(r.bounded);
    assert_eq!(r.refined, 3);
    // The true floor is 999; with three reads it is the lowest unrefined entry, 103.
    assert_eq!(floor(&fp, &HashSet::new()), Some(103));
}

/// Archived executions are never the holder and are never read.
#[test]
fn archived_executions_are_not_refined() {
    let mut fp = vec![h(1, 10, 20), h(2, 500, 600)];
    let archived: HashSet<i64> = [1].into();
    refine_floor_holders(
        &mut fp,
        &archived,
        |id| {
            assert_ne!(id, 1, "an archived execution was read");
            Some(Some((500, 600)))
        },
        8,
    );
}

// --- End to end through the prune pass, against a fake engine with real droppability. ---

mod pass {
    use std::cell::RefCell;
    use std::collections::HashSet;

    use chrono::{Duration, TimeZone, Utc};
    use noetl_server::services::event_archive as ea;

    fn t(h: i64) -> chrono::DateTime<chrono::Utc> {
        Utc.timestamp_opt(1_790_000_000, 0).unwrap() + Duration::hours(h)
    }

    fn cand(id: i64, status: &str, done_h: Option<i64>) -> ea::ArchiveCandidate {
        ea::ArchiveCandidate {
            execution_id: id,
            status: status.into(),
            completed_at: done_h.map(t),
            started_at: t(0),
        }
    }

    fn cfg() -> ea::RetentionConfig {
        ea::RetentionConfig {
            retention_hours: 48,
            archive_enabled: true,
            prune_enabled: true,
            bucket: Some("b".into()),
            interval_secs: 300,
            max_per_pass: 100,
        }
    }

    struct Part {
        min: u64,
        max: u64,
        holds: Vec<i64>,
        /// Ids the part's bloom wrongly admits.
        bloom_false: Vec<i64>,
    }

    struct Engine(RefCell<Vec<Part>>);

    impl Engine {
        /// Bloom view: true members AND false positives.
        fn footprint(&self, ids: &[i64]) -> Option<Vec<ea::HotExecution>> {
            let parts = self.0.borrow();
            Some(
                ids.iter()
                    .filter_map(|id| {
                        let hit: Vec<&Part> = parts
                            .iter()
                            .filter(|p| p.holds.contains(id) || p.bloom_false.contains(id))
                            .collect();
                        let lo = hit.iter().map(|p| p.min).min()?;
                        let hi = hit.iter().map(|p| p.max).max()?;
                        Some(ea::HotExecution {
                            execution_id: *id,
                            min_sequence: lo,
                            max_sequence: hi,
                        })
                    })
                    .collect(),
            )
        }
        /// Exact view: true members only.
        fn exact(&self, id: i64) -> Option<Option<(u64, u64)>> {
            let parts = self.0.borrow();
            let hit: Vec<&Part> = parts.iter().filter(|p| p.holds.contains(&id)).collect();
            Some(
                hit.iter()
                    .map(|p| p.min)
                    .min()
                    .zip(hit.iter().map(|p| p.max).max()),
            )
        }
        fn prune(&self, keep_from: u64) -> Option<Result<(usize, u64), String>> {
            let mut parts = self.0.borrow_mut();
            let before = parts.len();
            parts.retain(|p| p.max >= keep_from);
            Some(Ok((before - parts.len(), 0)))
        }
        fn survivors(&self) -> HashSet<i64> {
            self.0
                .borrow()
                .iter()
                .flat_map(|p| p.holds.clone())
                .collect()
        }
    }

    /// Part 1 holds only archived execution 1, but its bloom also admits live execution 9,
    /// whose records are really in part 3.
    fn engine() -> Engine {
        Engine(RefCell::new(vec![
            Part {
                min: 1,
                max: 10,
                holds: vec![1],
                bloom_false: vec![9],
            },
            Part {
                min: 11,
                max: 20,
                holds: vec![2],
                bloom_false: vec![],
            },
            Part {
                min: 21,
                max: 40,
                holds: vec![9],
                bloom_false: vec![],
            },
        ]))
    }

    fn inputs() -> (ea::CandidateSet, HashSet<i64>) {
        let set = ea::CandidateSet {
            candidates: vec![
                cand(1, "COMPLETED", Some(-100)),
                cand(2, "COMPLETED", Some(-100)),
                cand(9, "RUNNING", None),
            ],
            complete: true,
        };
        (set, [1, 2].into())
    }

    /// Today's behaviour, pinned: the false positive holds the floor at part 1.
    #[test]
    fn without_refinement_the_false_positive_pins_the_floor() {
        let eng = engine();
        let (set, archived) = inputs();
        let rep = ea::prune_pass(
            &cfg(),
            &set,
            t(0),
            &archived,
            |ids| eng.footprint(ids),
            |f| eng.prune(f),
        );
        assert_eq!(rep.keep_from_sequence, Some(1), "{}", rep.describe());
        assert_eq!(rep.parts_dropped, 0);
    }

    /// With refinement the floor moves to where execution 9 really starts: the archived
    /// parts go, and the part holding the live execution stays.
    #[test]
    fn with_refinement_archived_parts_go_and_the_live_one_stays() {
        let eng = engine();
        let (set, archived) = inputs();
        let rep = ea::prune_pass_refined(
            &cfg(),
            &set,
            t(0),
            &archived,
            |ids| eng.footprint(ids),
            |id| eng.exact(id),
            |f| eng.prune(f),
        );
        assert_eq!(rep.keep_from_sequence, Some(21), "{}", rep.describe());
        assert_eq!(rep.parts_dropped, 2);
        assert!(
            eng.survivors().contains(&9),
            "the live execution's part was dropped"
        );
    }
}
