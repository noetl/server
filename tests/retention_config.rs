//! P1 of [noetl/ai-meta#459](https://github.com/noetl/ai-meta/issues/459) — the retention
//! window and the archivable-set decision.
//!
//! ⚠ Env is NOT mutated here. `cargo test` does not serialise tests (a SAFETY note in this
//! repo once claimed it did, and the tests raced), so every assertion drives the pure
//! functions or an explicitly-constructed config rather than the process environment.

use chrono::{Duration, TimeZone, Utc};
use noetl_server::services::event_archive as ea;

fn t(h: i64) -> chrono::DateTime<chrono::Utc> {
    Utc.timestamp_opt(1_790_000_000, 0).unwrap() + Duration::hours(h)
}

#[test]
fn the_window_defaults_to_48_hours() {
    let c = ea::RetentionConfig::default();
    assert_eq!(c.retention_hours, ea::DEFAULT_RETENTION_HOURS);
    assert_eq!(c.retention_hours, 48, "the specified default is 48h");
    assert_eq!(c.retention(), Duration::hours(48));
}

/// ⭐⭐ Both flags default OFF, and pruning is refused unless archiving is on. A retention
/// tier that defaults on deletes data in whichever deployment nobody configured.
#[test]
fn both_flags_default_off_and_prune_requires_archive() {
    let c = ea::RetentionConfig::default();
    assert!(!c.archive_enabled);
    assert!(!c.prune_enabled);
    assert!(!c.archive_readiness().is_ready());
    assert!(!c.prune_readiness().is_ready());

    // Archiving on, pointed nowhere: enabled is NOT ready.
    let c2 = ea::RetentionConfig {
        archive_enabled: true,
        ..Default::default()
    };
    assert_eq!(
        c2.archive_readiness(),
        ea::Readiness::MisconfiguredNoBucket("NOETL_EHDB_ARCHIVE_BUCKET"),
        "a flag with no bucket must not read as ready — it would upload nowhere"
    );

    // ⚠⚠ The one combination that loses data outright.
    let c3 = ea::RetentionConfig {
        prune_enabled: true,
        archive_enabled: false,
        bucket: Some("b".into()),
        ..Default::default()
    };
    assert_eq!(
        c3.prune_readiness(),
        ea::Readiness::PruneWithoutArchive,
        "pruning with archiving off must be refused, not obeyed"
    );
    assert!(c3.prune_readiness().reason().contains("no copy being produced"));

    // Positive control: the readiness check CAN return Ready, so the refusals above are
    // measuring the conditions and not a function that always says no.
    let ok = ea::RetentionConfig {
        archive_enabled: true,
        prune_enabled: true,
        bucket: Some("b".into()),
        ..Default::default()
    };
    assert!(ok.archive_readiness().is_ready() && ok.prune_readiness().is_ready());
}

/// ⚠ A terminal status is archivable, never discardable — FAILED especially. 458 of prod's
/// 6,467 executions are FAILED, and a failed execution is usually the one someone wants.
#[test]
fn failed_and_cancelled_are_terminal_and_therefore_archivable() {
    for s in ["COMPLETED", "FAILED", "CANCELLED", "CANCELED", "ERROR"] {
        assert!(ea::is_terminal_status(s), "{s} must be terminal");
        assert!(
            ea::archivability(s, Some(t(0)), t(100), Duration::hours(48)).is_archivable(),
            "{s} older than the window must be archivable"
        );
    }
    // Negative control: the predicate must still say no to something.
    for s in ["RUNNING", "PENDING", "QUEUED", "PAUSED", "", "COMPLETE"] {
        assert!(!ea::is_terminal_status(s), "{s:?} must NOT be terminal");
    }
}

/// The three not-archivable reasons must stay distinct. A bool would make a permanently
/// stuck execution look identical to one that finished four minutes ago — and it is the
/// stuck one that pins the retention floor.
#[test]
fn the_not_archivable_reasons_are_distinguishable() {
    let r = Duration::hours(48);
    assert_eq!(
        ea::archivability("RUNNING", None, t(100), r),
        ea::Archivability::NotTerminal
    );
    assert_eq!(
        ea::archivability("COMPLETED", None, t(100), r),
        ea::Archivability::TerminalWithoutCompletedAt,
        "terminal with no completion time must not be archived on a guess"
    );
    match ea::archivability("COMPLETED", Some(t(99)), t(100), r) {
        ea::Archivability::WithinRetention { age_hours } => assert_eq!(age_hours, 1),
        other => panic!("expected WithinRetention, got {other:?}"),
    }
    assert!(ea::archivability("COMPLETED", Some(t(0)), t(100), r).is_archivable());
}

/// ⚠⚠ The boundary. An execution exactly AT the window is not yet past it; one microsecond
/// later is. Asserted on both sides so an off-by-one cannot archive data a moment early.
#[test]
fn the_retention_boundary_is_exact_and_inclusive_of_the_window() {
    let r = Duration::hours(48);
    let now = t(100);
    let exactly = now - r;
    assert!(
        !ea::archivability("COMPLETED", Some(exactly), now, r).is_archivable(),
        "exactly at the window is still retained"
    );
    assert!(
        ea::archivability("COMPLETED", Some(exactly - Duration::microseconds(1)), now, r)
            .is_archivable(),
        "one microsecond past the window is archivable"
    );
    // And a 24h window must behave the same way — the spec calls 24h out as supported.
    let r24 = Duration::hours(24);
    assert!(!ea::archivability("COMPLETED", Some(now - r24), now, r24).is_archivable());
    assert!(ea::archivability("COMPLETED", Some(now - r24 - Duration::seconds(1)), now, r24)
        .is_archivable());
}

/// A future `completed_at` (clock skew) must not be archivable.
#[test]
fn a_completion_in_the_future_is_never_archivable() {
    let r = Duration::hours(48);
    assert!(
        !ea::archivability("COMPLETED", Some(t(200)), t(100), r).is_archivable(),
        "a completed_at in the future must not archive — negative age is not old age"
    );
}

/// ⭐ The scan publishes its denominator. `GET /api/executions` caps `limit` at 100: one
/// call returned 100 where paging returned 6,467, so a selection count without the
/// population it came from is consistent with having examined a single page.
#[test]
fn the_scan_accounts_for_every_row_it_examined() {
    let r = Duration::hours(48);
    let now = t(100);
    let rows: Vec<(i64, String, Option<chrono::DateTime<chrono::Utc>>)> = vec![
        (1, "COMPLETED".into(), Some(t(0))),   // archivable
        (2, "FAILED".into(), Some(t(1))),      // archivable
        (3, "COMPLETED".into(), Some(t(99))),  // within retention
        (4, "RUNNING".into(), None),           // not terminal
        (5, "COMPLETED".into(), None),         // terminal, no completed_at
    ];
    let mut scan = ea::ArchivableScan::default();
    ea::classify_into(&mut scan, &rows, now, r);
    assert_eq!(scan.examined, 5);
    assert_eq!(scan.archivable, vec![1, 2]);
    assert_eq!(scan.not_terminal, 1);
    assert_eq!(scan.within_retention, 1);
    assert_eq!(scan.terminal_without_completed_at, 1);
    assert_eq!(scan.pages, 1);
    // ⭐ Nothing may fall through the classification unaccounted.
    assert_eq!(
        scan.accounted(),
        scan.examined,
        "a row was examined but landed in no bucket — the scan's own arithmetic must close"
    );

    // Accumulates across pages, so a multi-page walk is visible in `pages`.
    ea::classify_into(&mut scan, &rows, now, r);
    assert_eq!(scan.pages, 2);
    assert_eq!(scan.examined, 10);
    assert_eq!(scan.accounted(), 10);
}

/// ⚠⚠ The rejection paths, driven through the REAL parsers. These are the branches that
/// decide whether data gets deleted, so they are the last place to accept a decorative
/// assertion — the first version of this test asserted
/// `MAX_RETENTION_HOURS > DEFAULT_RETENTION_HOURS`, which clippy correctly called out as a
/// constant that can never fail at runtime.
#[test]
fn a_zero_window_and_a_typo_are_refused_not_silently_accepted() {
    // 0 is refused, not clamped: it would archive and prune an execution as it completed.
    let e = ea::parse_retention_hours("0").unwrap_err();
    assert!(e.contains("retain nothing"), "{e}");
    assert!(e.contains("NOETL_EHDB_RETENTION_HOURS"), "must name the var: {e}");

    // Garbage is an error, not a fallback to the default.
    for bad in ["", " ", "forty-eight", "48h", "-1", "4.8", "0x30"] {
        assert!(
            ea::parse_retention_hours(bad).is_err(),
            "{bad:?} must be refused rather than read as the default"
        );
    }
    // A fat-fingered extra digit must not silently mean "never archive".
    assert!(ea::parse_retention_hours("480000").is_err());

    // Positive control: the parser DOES accept the real values, so the refusals above are
    // measuring the guards and not a function that rejects everything.
    assert_eq!(ea::parse_retention_hours("48").unwrap(), 48);
    assert_eq!(ea::parse_retention_hours("24").unwrap(), 24);
    assert_eq!(ea::parse_retention_hours(" 72 ").unwrap(), 72);
}

/// ⚠⚠ A typo in a data-deleting flag must not look like "off".
#[test]
fn an_unrecognised_flag_value_is_an_error_not_a_silent_false() {
    for bad in ["treu", "TRUE!", "enabled", "2", "", "y", "t"] {
        assert!(
            ea::parse_flag("NOETL_EHDB_PRUNE_ENABLED", bad).is_err(),
            "{bad:?} must be refused, not read as false"
        );
    }
    // Accepted forms, case- and space-insensitive — the gateway's NOETL_AUTH_SYNC takes
    // only exactly "true"/"1", so "TRUE" silently means false there; a value copied
    // between components changing meaning is a documented trap worth not repeating.
    for (v, want) in [
        ("true", true), ("TRUE", true), (" On ", true), ("yes", true), ("1", true),
        ("false", false), ("FALSE", false), ("off", false), ("no", false), ("0", false),
    ] {
        assert_eq!(
            ea::parse_flag("X", v).unwrap(),
            want,
            "{v:?} should parse to {want}"
        );
    }
}

/// Every `Readiness` variant must produce an actionable, distinct message. Three identical
/// strings would satisfy a naive "is it non-empty" test while reproducing the defect the
/// named states exist to prevent.
#[test]
fn every_readiness_reason_is_distinct_and_names_its_variable() {
    let rs = [
        ea::Readiness::Ready,
        ea::Readiness::Disabled("NOETL_EHDB_ARCHIVE_ENABLED"),
        ea::Readiness::MisconfiguredNoBucket("NOETL_EHDB_ARCHIVE_BUCKET"),
        ea::Readiness::PruneWithoutArchive,
    ];
    let msgs: Vec<String> = rs.iter().map(|r| r.reason()).collect();
    for (i, a) in msgs.iter().enumerate() {
        assert!(!a.is_empty());
        for b in msgs.iter().skip(i + 1) {
            assert_ne!(a, b, "two Readiness variants produce the same message");
        }
    }
    for m in &msgs[1..] {
        assert!(
            m.contains("NOETL_EHDB"),
            "a non-ready reason must name the variable to change: {m}"
        );
    }
}
