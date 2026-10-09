//! P6 (partial) of [noetl/ai-meta#459](https://github.com/noetl/ai-meta/issues/459) — the
//! pinned-floor hazard is observable.
//!
//! ⚠⚠ The property under test is that these series read **0** when nothing is happening,
//! not that they are absent. `Registry::gather` prunes empty families, so an unpinned
//! metric is absent until it first fires — indistinguishable from a build that does not
//! have it, or a pod nothing scrapes.

use noetl_server::services::event_archive as ea;

/// ⚠⚠ These tests mutate **process-global** Prometheus gauges, and `cargo test` runs the
/// tests in one binary **in parallel**. Without this lock they race: one test seeds the
/// series while another asserts an exact value, and the result is a failure that depends on
/// timing — it passed locally and failed in CI, which is the worst version of the bug.
///
/// The repo already carries this lesson for `env::set_var` ("cargo test does not serialise
/// tests"); a shared metric registry is the same hazard wearing different clothes.
static GAUGE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take the lock, tolerating a poisoned mutex so one failing test does not cascade into
/// every other test in the file reporting a lock error instead of its own verdict.
fn serialised() -> std::sync::MutexGuard<'static, ()> {
    match GAUGE_LOCK.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Renders through the SAME function `/metrics` serves, so the test asserts what a scrape
/// would actually see rather than what a private registry holds.
fn rendered() -> String {
    noetl_server::metrics::gather_text().expect("render /metrics")
}

/// ⭐ With archiving OFF and no pass ever run, every retention series must still be
/// present and 0. This is the server#315 lesson: a pin inside a config branch is not a pin.
#[test]
fn every_retention_series_is_pinned_at_zero_before_anything_runs() {
    let _g = serialised();
    noetl_server::metrics::init_ehdb_retention_series();
    let text = rendered();

    let gauges = [
        "noetl_ehdb_retention_oldest_non_archivable_age_seconds",
        "noetl_ehdb_retention_floor_blocked_by_execution",
        "noetl_ehdb_retention_floor_sequence",
        "noetl_ehdb_archive_scan_examined",
        "noetl_ehdb_archive_scan_archivable",
    ];
    for g in gauges {
        assert!(
            text.lines().any(|l| l.starts_with(g)),
            "{g} is ABSENT — absence is indistinguishable from a build that lacks it"
        );
    }
    assert!(
        text.lines().any(|l| l.starts_with("noetl_ehdb_prune_bytes_reclaimed_total")),
        "the reclaimed-bytes counter must be present at 0"
    );

    // ⭐ Every outcome label pinned — including the refusal reasons, so "a prune was never
    // refused" is readable as 0 rather than as an absent series.
    let mut missing = Vec::new();
    for o in noetl_server::metrics::EHDB_ARCHIVE_OUTCOMES {
        let want = format!("noetl_ehdb_archive_total{{outcome=\"{o}\"}}");
        if !text.contains(&want) {
            missing.push(o);
        }
    }
    assert!(
        missing.is_empty(),
        "unpinned archive outcomes {missing:?} of {} — each would be absent until it first \
         fires",
        noetl_server::metrics::EHDB_ARCHIVE_OUTCOMES.len()
    );
    // Denominator, so a vacuous pass over an empty label set is visible.
    assert_eq!(noetl_server::metrics::EHDB_ARCHIVE_OUTCOMES.len(), 10);
}

/// ⚠⚠ When the block clears, the id must be RESET. A stale id would name an execution that
/// is no longer blocking anything — a representation outliving what it described.
#[test]
fn the_blocking_execution_id_is_cleared_when_nothing_blocks() {
    let _g = serialised();
    noetl_server::metrics::init_ehdb_retention_series();
    let hot = vec![
        ea::HotExecution { execution_id: 4242, min_sequence: 7, max_sequence: 90 },
        ea::HotExecution { execution_id: 4243, min_sequence: 50, max_sequence: 99 },
    ];

    // Blocked: the id and the age are published.
    let blocked = ea::retention_floor(&hot, &[4243]);
    ea::export_floor(&blocked, Some(3_600));
    assert_eq!(
        noetl_server::metrics::ehdb_retention_floor_blocked_by_execution().get(),
        4242
    );
    assert_eq!(
        noetl_server::metrics::ehdb_retention_oldest_non_archivable_age_seconds().get(),
        3_600
    );

    // Cleared: both go back to 0.
    let open = ea::retention_floor(&hot, &[4242, 4243]);
    assert!(open.blocked_by.is_none());
    ea::export_floor(&open, None);
    assert_eq!(
        noetl_server::metrics::ehdb_retention_floor_blocked_by_execution().get(),
        0,
        "a stale blocking id must not survive the block clearing"
    );
    assert_eq!(
        noetl_server::metrics::ehdb_retention_oldest_non_archivable_age_seconds().get(),
        0
    );
    // The floor itself advanced to the log tip.
    assert_eq!(
        noetl_server::metrics::ehdb_retention_floor_sequence().get(),
        100
    );
}

/// A negative age (clock skew) must not become a negative gauge.
#[test]
fn a_negative_age_is_clamped_rather_than_published() {
    let _g = serialised();
    noetl_server::metrics::init_ehdb_retention_series();
    let hot = vec![ea::HotExecution { execution_id: 5, min_sequence: 1, max_sequence: 2 }];
    ea::export_floor(&ea::retention_floor(&hot, &[]), Some(-9_999));
    assert_eq!(
        noetl_server::metrics::ehdb_retention_oldest_non_archivable_age_seconds().get(),
        0,
        "negative age is clock skew, not a negative duration"
    );
}

/// The scan publishes both numbers, so a selection never travels without its population.
#[test]
fn the_scan_gauges_carry_the_denominator() {
    let _g = serialised();
    noetl_server::metrics::init_ehdb_retention_series();
    let mut scan = ea::ArchivableScan::default();
    let rows: Vec<(i64, String, Option<chrono::DateTime<chrono::Utc>>)> = (0..7)
        .map(|i| {
            (
                i,
                if i < 3 { "RUNNING".to_string() } else { "COMPLETED".to_string() },
                if i < 3 { None } else { Some(chrono::Utc::now() - chrono::Duration::days(5)) },
            )
        })
        .collect();
    ea::classify_into(&mut scan, &rows, chrono::Utc::now(), chrono::Duration::hours(48));
    ea::export_scan(&scan);
    assert_eq!(noetl_server::metrics::ehdb_archive_scan_examined().get(), 7);
    assert_eq!(noetl_server::metrics::ehdb_archive_scan_archivable().get(), 4);
}

/// ⚠ A huge sequence must saturate rather than wrap into a plausible negative.
#[test]
fn an_enormous_sequence_saturates_rather_than_wrapping_negative() {
    let _g = serialised();
    noetl_server::metrics::init_ehdb_retention_series();
    let hot = vec![ea::HotExecution {
        execution_id: 1,
        min_sequence: u64::MAX - 1,
        max_sequence: u64::MAX,
    }];
    ea::export_floor(&ea::retention_floor(&hot, &[1]), None);
    assert!(
        noetl_server::metrics::ehdb_retention_floor_sequence().get() > 0,
        "a wrapped sequence would read as a plausible negative number"
    );
}
