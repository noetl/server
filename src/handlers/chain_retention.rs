//! **Chain-store retention** — delete partitions nothing will read again
//! (noetl/ai-meta#506, RFC phase P1).
//!
//! # Why
//!
//! The chain store keeps two files per event plus six inodes per execution, and
//! until this module nothing ever removed a partition. On prod the 10Gi `/data`
//! volume reached 92% of its inodes at 33% of its bytes on 2026-11-03, growing
//! ~17.5k inodes/day. ext4 runs out of inodes long before bytes on a store of
//! small files, and when it does, file creation fails with ENOSPC while `df -h`
//! still shows free space.
//!
//! # Why deleting is safe
//!
//! The store is a **cache of `noetl.event`**, not a record:
//!
//! - its only writer is [`LogSourcedChainSource`](super::chain_populate::LogSourcedChainSource),
//!   which reads the execution's events from Postgres, populates the partition
//!   from them, and answers from it under one lock hold. A missing partition is
//!   simply repopulated from the log on the next read;
//! - its only caller is the reconcile poller, which walks
//!   `orch_cache.active_executions()`. A terminal event evicts the execution, so a
//!   finished execution's partition is never read again;
//! - state replay walks Postgres, not this store.
//!
//! Measured on kind (v3.147.0, prod's chain flags): deleting an ACTIVE run's
//! partition mid-run rebuilt it on the next tick and the run ended identical to an
//! untouched control; deleting a FINISHED one freed exactly `2·events + 6` inodes
//! and was never recreated.
//!
//! # What is kept
//!
//! A partition is deleted only when ALL of:
//!
//! 1. its execution is **not** in the active orchestration cache;
//! 2. its `wm` file — rewritten on every populate — is older than the retention
//!    window, so nothing has read it for that long;
//! 3. it has a `cov` file. `populate_from_log` deletes `cov` on a detected
//!    divergence and deliberately keeps the events as evidence; this module does
//!    not destroy that evidence.
//!
//! Each deletion re-checks (1) and (2) while holding the chain-store lock, so a
//! populate can never be mid-write on the partition being removed.
//!
//! # Default OFF
//!
//! `NOETL_CHAIN_RETENTION_HOURS` unset or `0` disables the sweeper entirely, and
//! `noetl_chain_retention_hours` then reads 0, so "off" is visible rather than an
//! absent series.

use std::collections::HashSet;
use std::path::Path;
use std::time::{Duration, SystemTime};

/// Retention window in hours. Unset or `0` = off.
pub const RETENTION_HOURS_ENV: &str = "NOETL_CHAIN_RETENTION_HOURS";
/// Seconds between sweeps. Default 600.
pub const RETENTION_INTERVAL_ENV: &str = "NOETL_CHAIN_RETENTION_INTERVAL_SECS";
/// Most partitions deleted per sweep. Default 500.
pub const RETENTION_MAX_PER_PASS_ENV: &str = "NOETL_CHAIN_RETENTION_MAX_PER_PASS";

/// Every outcome the sweeper records, pinned at 0 by
/// [`crate::metrics::init_chain_retention_series`].
pub const RETENTION_OUTCOMES: &[&str] = &[
    "deleted",
    "skipped_active",
    "skipped_young",
    "skipped_no_cov",
    "skipped_no_wm",
    "deferred",
    "failed",
];

fn env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok())
}

/// The configured window, or `None` when the sweeper is off.
pub fn retention() -> Option<Duration> {
    match env_u64(RETENTION_HOURS_ENV) {
        Some(h) if h > 0 => Some(Duration::from_secs(h * 3600)),
        _ => None,
    }
}

/// What one sweep did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepReport {
    /// ⭐ The denominator: partition directories looked at.
    pub examined: usize,
    pub deleted: usize,
    pub skipped_active: usize,
    pub skipped_young: usize,
    pub skipped_no_cov: usize,
    pub skipped_no_wm: usize,
    /// Eligible but over the per-pass cap.
    pub deferred: usize,
    pub failed: usize,
}

impl SweepReport {
    fn counts(&self) -> [(&'static str, usize); 7] {
        [
            ("deleted", self.deleted),
            ("skipped_active", self.skipped_active),
            ("skipped_young", self.skipped_young),
            ("skipped_no_cov", self.skipped_no_cov),
            ("skipped_no_wm", self.skipped_no_wm),
            ("deferred", self.deferred),
            ("failed", self.failed),
        ]
    }
}

enum Verdict {
    Delete,
    Active,
    Young,
    NoCov,
    NoWm,
}

fn verdict(
    dir: &Path,
    id: &str,
    retention: Duration,
    active: &HashSet<String>,
    now: SystemTime,
) -> Verdict {
    if active.contains(id) {
        return Verdict::Active;
    }
    let Ok(mtime) = std::fs::metadata(dir.join("wm")).and_then(|m| m.modified()) else {
        return Verdict::NoWm;
    };
    // A clock that moved backwards makes the age unknowable; keep the partition.
    match now.duration_since(mtime) {
        Ok(age) if age >= retention => {}
        _ => return Verdict::Young,
    }
    if !dir.join("cov").exists() {
        return Verdict::NoCov;
    }
    Verdict::Delete
}

/// One sweep over `<root>/chain`.
///
/// `lock` wraps each deletion; the live caller passes the chain-store lock, tests
/// pass a plain call. The verdict is computed twice — once to choose, once inside
/// `lock` — because the first look is taken without the lock.
pub fn sweep(
    root: &Path,
    retention: Duration,
    active: &HashSet<String>,
    max_deletes: usize,
    now: SystemTime,
    lock: &dyn Fn(&mut dyn FnMut()),
) -> std::io::Result<SweepReport> {
    let chain = root.join("chain");
    let mut report = SweepReport::default();
    let entries = match std::fs::read_dir(&chain) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name.to_str() else { continue };
        // Partition directories are named by execution id. Anything else is not ours.
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) || !entry.path().is_dir() {
            continue;
        }
        report.examined += 1;
        let dir = entry.path();
        match verdict(&dir, id, retention, active, now) {
            Verdict::Active => report.skipped_active += 1,
            Verdict::Young => report.skipped_young += 1,
            Verdict::NoCov => report.skipped_no_cov += 1,
            Verdict::NoWm => report.skipped_no_wm += 1,
            // Eligible, but over this pass's cap; the next pass takes it.
            Verdict::Delete if report.deleted >= max_deletes => report.deferred += 1,
            Verdict::Delete => {
                let mut outcome = None;
                lock(&mut || {
                    outcome = Some(match verdict(&dir, id, retention, active, now) {
                        Verdict::Delete => std::fs::remove_dir_all(&dir).map(|_| true),
                        _ => Ok(false),
                    });
                });
                match outcome {
                    Some(Ok(true)) => report.deleted += 1,
                    Some(Ok(false)) => report.skipped_young += 1,
                    Some(Err(e)) => {
                        tracing::warn!(target: "noetl_server::chain_retention",
                            execution_id = id, error = %e, "chain partition delete failed");
                        report.failed += 1;
                    }
                    None => report.failed += 1,
                }
            }
        }
    }
    Ok(report)
}

/// Start the sweeper when both the populator and retention are on.
///
/// ⚠ The active set is read from the orchestration cache at the start of each
/// sweep. An execution that becomes active after that read is still safe: it is
/// re-checked under the lock only for age, and a partition of an execution that
/// is running again is repopulated from the log on its next read.
pub fn spawn_chain_retention_sweeper(state: crate::state::AppState) {
    let Some(window) = retention() else {
        crate::metrics::set_chain_retention_hours(0);
        tracing::info!(target: "noetl_server::chain_retention",
            "chain retention sweeper OFF ({RETENTION_HOURS_ENV} unset or 0)");
        return;
    };
    if !ehdb_l0::chain_populator::populator_enabled() {
        crate::metrics::set_chain_retention_hours(0);
        tracing::info!(target: "noetl_server::chain_retention",
            "chain retention configured but the populator is off; sweeper not started");
        return;
    }
    crate::metrics::set_chain_retention_hours((window.as_secs() / 3600) as i64);
    let interval = Duration::from_secs(env_u64(RETENTION_INTERVAL_ENV).unwrap_or(600).max(30));
    let max = env_u64(RETENTION_MAX_PER_PASS_ENV).unwrap_or(500) as usize;
    let root = std::path::PathBuf::from(super::chain_populate::populate_dir());
    tracing::info!(target: "noetl_server::chain_retention", root = %root.display(),
        hours = window.as_secs() / 3600, interval_secs = interval.as_secs(), max,
        "chain retention sweeper ON");
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            let active: HashSet<String> = state
                .orch_cache
                .active_executions()
                .into_iter()
                .map(|id| id.to_string())
                .collect();
            let root = root.clone();
            let res = tokio::task::spawn_blocking(move || {
                // If the populator is not open there is no lock to take, and
                // deleting without it is exactly what the lock prevents: the closure
                // does not run and the sweep counts it as `failed`.
                sweep(&root, window, &active, max, SystemTime::now(), &|f| {
                    let _ = super::chain_populate::with_chain_lock(f);
                })
            })
            .await;
            match res {
                Ok(Ok(report)) => {
                    for (outcome, n) in report.counts() {
                        crate::metrics::record_chain_retention(outcome, n as u64);
                    }
                    crate::metrics::set_chain_retention_swept_at(chrono::Utc::now().timestamp());
                    tracing::info!(target: "noetl_server::chain_retention",
                        examined = report.examined, deleted = report.deleted,
                        skipped_active = report.skipped_active,
                        skipped_young = report.skipped_young,
                        skipped_no_cov = report.skipped_no_cov,
                        failed = report.failed, "chain retention sweep");
                }
                Ok(Err(e)) => {
                    tracing::warn!(target: "noetl_server::chain_retention", error = %e,
                        "chain retention sweep could not list the store");
                    crate::metrics::record_chain_retention("failed", 1);
                }
                Err(e) => {
                    tracing::warn!(target: "noetl_server::chain_retention", error = %e,
                        "chain retention sweep task failed");
                    crate::metrics::record_chain_retention("failed", 1);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: Duration = Duration::from_secs(86_400);

    /// ⭐ Against the REAL store, not a lookalike: populate from a log, sweep, and
    /// show the partition (a) reads as "cannot answer" rather than "no events" and
    /// (b) repopulates from the same log to the identical chain. This is the
    /// property the whole module rests on.
    #[test]
    fn a_swept_partition_reads_as_unknown_and_rebuilds_identically_from_the_log() {
        use ehdb_l0::chain_populator::{ChainPopulator, FromLog, LogEvent};
        use ehdb_l0::chain_store_durable::DurableChainStore;
        use ehdb_l0::substrate::{DurableSubstrate, LocalFsSubstrate};

        let root =
            std::env::temp_dir().join(format!("chain-retention-real-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let substrate: std::sync::Arc<dyn DurableSubstrate> =
            std::sync::Arc::new(LocalFsSubstrate::new(&root).unwrap());
        let store = DurableChainStore::new(std::sync::Arc::clone(&substrate));
        let pop = ChainPopulator::new(&store, substrate.as_ref());

        let log: Vec<LogEvent> = (1..=12)
            .map(|i| LogEvent {
                event_id: format!("9000{i:02}"),
                parent_execution_id: None,
                payload: String::new(),
                prev_event_id: None,
            })
            .collect();
        assert!(matches!(
            pop.populate_from_log("777", &log).unwrap(),
            FromLog::InSync { appended: 12, .. }
        ));
        let before: Vec<String> = pop
            .chain_if_authoritative("777")
            .unwrap()
            .expect("authoritative")
            .iter()
            .map(|e| e.event_id.clone())
            .collect();
        assert_eq!(before.len(), 12);

        // The real layout: 2 files per event + head, wm, cov + 3 directories.
        let count = |p: &Path| walk_count(p);
        assert_eq!(count(&root.join("chain/777")), 2 * 12 + 6);

        let r = sweep(
            &root,
            DAY,
            &HashSet::new(),
            10,
            SystemTime::now() + 2 * DAY,
            &|f| f(),
        )
        .unwrap();
        assert_eq!(r.deleted, 1, "{r:?}");
        assert!(!root.join("chain/777").exists());

        // (a) Gone means "cannot answer", never "no events".
        assert!(pop.chain_if_authoritative("777").unwrap().is_none());

        // (b) The next read repopulates from the same log, to the same chain.
        assert!(matches!(
            pop.populate_from_log("777", &log).unwrap(),
            FromLog::InSync { appended: 12, .. }
        ));
        let after: Vec<String> = pop
            .chain_if_authoritative("777")
            .unwrap()
            .expect("authoritative")
            .iter()
            .map(|e| e.event_id.clone())
            .collect();
        assert_eq!(before, after);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Entries under `p`, including `p` itself — what `df -i` counts.
    fn walk_count(p: &Path) -> usize {
        1 + std::fs::read_dir(p)
            .map(|rd| {
                rd.flatten()
                    .map(|e| {
                        if e.path().is_dir() {
                            walk_count(&e.path())
                        } else {
                            1
                        }
                    })
                    .sum()
            })
            .unwrap_or(0)
    }

    struct Store(std::path::PathBuf);

    impl Store {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("chain-retention-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join("chain")).unwrap();
            Store(root)
        }

        /// A partition shaped like the real one: ev/ + seq/ per event, head, wm, cov.
        fn partition(&self, id: &str, events: usize, cov: bool) -> std::path::PathBuf {
            let d = self.0.join("chain").join(id);
            std::fs::create_dir_all(d.join("ev")).unwrap();
            std::fs::create_dir_all(d.join("seq")).unwrap();
            for i in 0..events {
                std::fs::write(d.join("ev").join(i.to_string()), b"e").unwrap();
                std::fs::write(d.join("seq").join(format!("{i:020}")), b"s").unwrap();
            }
            std::fs::write(d.join("head"), b"h").unwrap();
            std::fs::write(d.join("wm"), b"1:1").unwrap();
            if cov {
                std::fs::write(d.join("cov"), b"r:1").unwrap();
            }
            d
        }

        fn exists(&self, id: &str) -> bool {
            self.0.join("chain").join(id).exists()
        }
    }

    impl Drop for Store {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn plain(f: &mut dyn FnMut()) {
        f()
    }

    /// "now" a given time after the partitions were written.
    fn later(by: Duration) -> SystemTime {
        SystemTime::now() + by
    }

    #[test]
    fn deletes_only_old_inactive_partitions_with_coverage() {
        let s = Store::new("verdicts");
        s.partition("100", 3, true); // old, inactive, covered -> delete
        s.partition("200", 3, true); // active -> keep
        s.partition("300", 3, false); // no cov (divergence evidence) -> keep
        s.partition("400", 3, true); // deleted below
        std::fs::remove_file(s.0.join("chain/400/wm")).unwrap(); // no wm -> keep
        std::fs::create_dir_all(s.0.join("chain/not-an-id")).unwrap(); // not ours
        let active: HashSet<String> = ["200".to_string()].into();

        let r = sweep(&s.0, 7 * DAY, &active, 100, later(8 * DAY), &plain).unwrap();

        assert_eq!(
            r.examined, 4,
            "denominator must count every partition dir: {r:?}"
        );
        assert_eq!(
            (
                r.deleted,
                r.skipped_active,
                r.skipped_no_cov,
                r.skipped_no_wm,
                r.failed
            ),
            (1, 1, 1, 1, 0),
            "{r:?}"
        );
        assert!(!s.exists("100"));
        assert!(s.exists("200") && s.exists("300") && s.exists("400"));
        assert!(
            s.exists("not-an-id"),
            "a non-numeric entry must never be touched"
        );
    }

    #[test]
    fn keeps_partitions_younger_than_the_window() {
        let s = Store::new("young");
        s.partition("100", 2, true);
        let r = sweep(&s.0, 7 * DAY, &HashSet::new(), 100, later(6 * DAY), &plain).unwrap();
        assert_eq!((r.deleted, r.skipped_young), (0, 1), "{r:?}");
        assert!(s.exists("100"));
    }

    /// Positive control for the age test: the same partition IS deleted once old.
    /// Without this, a sweep that never deletes anything passes the test above.
    #[test]
    fn the_same_partition_is_deleted_once_old_enough() {
        let s = Store::new("aged");
        s.partition("100", 2, true);
        let r = sweep(
            &s.0,
            7 * DAY,
            &HashSet::new(),
            100,
            later(7 * DAY + Duration::from_secs(60)),
            &plain,
        )
        .unwrap();
        assert_eq!(r.deleted, 1, "{r:?}");
        assert!(!s.exists("100"));
    }

    #[test]
    fn the_per_pass_cap_bounds_deletes() {
        let s = Store::new("cap");
        for id in ["1", "2", "3", "4", "5"] {
            s.partition(id, 1, true);
        }
        let r = sweep(&s.0, DAY, &HashSet::new(), 2, later(2 * DAY), &plain).unwrap();
        assert_eq!((r.examined, r.deleted, r.deferred), (5, 2, 3), "{r:?}");
        let left = ["1", "2", "3", "4", "5"]
            .iter()
            .filter(|id| s.exists(id))
            .count();
        assert_eq!(left, 3);
    }

    /// Every delete happens inside the lock closure, so a caller holding the
    /// chain-store lock there excludes a concurrent populate.
    #[test]
    fn every_delete_runs_inside_the_lock() {
        let s = Store::new("lock");
        s.partition("1", 1, true);
        s.partition("2", 1, true);
        let held = std::cell::Cell::new(0usize);
        let lock = |f: &mut dyn FnMut()| {
            held.set(held.get() + 1);
            f();
        };
        let r = sweep(&s.0, DAY, &HashSet::new(), 100, later(2 * DAY), &lock).unwrap();
        assert_eq!(r.deleted, 2);
        assert_eq!(held.get(), 2, "each delete must take the lock exactly once");
    }

    /// A lock that never runs the closure (the populator closed) deletes nothing
    /// and counts it as failed — never a silent delete outside the lock.
    #[test]
    fn no_lock_means_no_delete() {
        let s = Store::new("nolock");
        s.partition("1", 1, true);
        let r = sweep(&s.0, DAY, &HashSet::new(), 100, later(2 * DAY), &|_f| {}).unwrap();
        assert_eq!((r.deleted, r.failed), (0, 1), "{r:?}");
        assert!(s.exists("1"));
    }

    #[test]
    fn a_missing_store_is_an_empty_sweep() {
        let root =
            std::env::temp_dir().join(format!("chain-retention-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let r = sweep(&root, DAY, &HashSet::new(), 100, SystemTime::now(), &plain).unwrap();
        assert_eq!(r, SweepReport::default());
    }

    #[test]
    fn retention_is_off_unless_positive() {
        // Process-global env: read through the parser only.
        assert_eq!(env_u64("NOETL_CHAIN_RETENTION_TEST_UNSET_VAR"), None);
    }

    #[test]
    fn every_outcome_is_pinned() {
        let report_labels: Vec<&str> = SweepReport::default()
            .counts()
            .iter()
            .map(|(l, _)| *l)
            .collect();
        assert_eq!(report_labels.len(), RETENTION_OUTCOMES.len());
        for l in report_labels {
            assert!(
                RETENTION_OUTCOMES.contains(&l),
                "{l} is recorded but not pinned"
            );
        }
    }
}
