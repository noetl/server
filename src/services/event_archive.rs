//! Retention + object-store archival of the event log — P1: configuration and the
//! archivable-set decision.
//!
//! Design: `noetl/ehdb design/retention-archival-tier.md`.
//! Tracking: [noetl/ai-meta#459](https://github.com/noetl/ai-meta/issues/459), which closes
//! [#457](https://github.com/noetl/ai-meta/issues/457) by making the hot volume a *window*
//! rather than an accumulator.
//!
//! This phase decides **which executions are archivable** and nothing else. It writes
//! nothing, uploads nothing and deletes nothing.

use chrono::{DateTime, Duration, Utc};

/// `NOETL_EHDB_RETENTION_HOURS` — how long a completed execution stays hot.
const RETENTION_VAR: &str = "NOETL_EHDB_RETENTION_HOURS";
/// Default window. 48h, with 24h an explicitly supported setting.
pub const DEFAULT_RETENTION_HOURS: u64 = 48;
/// Upper bound on the configured window, so a typo cannot silently mean "never archive".
/// One year: far beyond any intended retention, close enough to catch a fat-fingered
/// `480000`.
pub const MAX_RETENTION_HOURS: u64 = 24 * 365;

const ARCHIVE_ENABLED_VAR: &str = "NOETL_EHDB_ARCHIVE_ENABLED";
const PRUNE_ENABLED_VAR: &str = "NOETL_EHDB_PRUNE_ENABLED";
const BUCKET_VAR: &str = "NOETL_EHDB_ARCHIVE_BUCKET";
const INTERVAL_VAR: &str = "NOETL_EHDB_ARCHIVE_INTERVAL_SECS";
const MAX_PER_PASS_VAR: &str = "NOETL_EHDB_ARCHIVE_MAX_PER_PASS";

/// Retention / archival configuration.
///
/// ⭐⭐ `archive_enabled` and `prune_enabled` are **two independent flags on purpose**.
/// Archiving is additive — it writes to the object store and deletes nothing. Pruning
/// destroys the only other copy of the data. There has to be a reachable state in which
/// the archive is provably complete and *nothing has been deleted*; a single flag makes
/// "verify before deleting" impossible to express.
///
/// Both default **false**. A retention tier that defaults on is a tier that deletes data in
/// whichever deployment nobody configured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionConfig {
    pub retention_hours: u64,
    pub archive_enabled: bool,
    pub prune_enabled: bool,
    pub bucket: Option<String>,
    pub interval_secs: u64,
    pub max_per_pass: usize,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            retention_hours: DEFAULT_RETENTION_HOURS,
            archive_enabled: false,
            prune_enabled: false,
            bucket: None,
            interval_secs: 900,
            max_per_pass: 200,
        }
    }
}

impl RetentionConfig {
    pub fn retention(&self) -> Duration {
        Duration::hours(self.retention_hours as i64)
    }

    /// Can the archive pass actually do anything?
    ///
    /// ⚠ A flag without a bucket is not "enabled" — it is a misconfiguration that would
    /// otherwise look enabled while uploading nowhere. Reported as a distinct reason rather
    /// than folded into a bool.
    pub fn archive_readiness(&self) -> Readiness {
        if !self.archive_enabled {
            return Readiness::Disabled(ARCHIVE_ENABLED_VAR);
        }
        match self.bucket.as_deref() {
            None | Some("") => Readiness::MisconfiguredNoBucket(BUCKET_VAR),
            Some(_) => Readiness::Ready,
        }
    }

    /// ⚠⚠ Pruning additionally requires archiving to be on. Pruning with archiving off
    /// would delete hot data whose archive is not being produced — the one combination that
    /// loses data outright, so it is refused rather than obeyed.
    pub fn prune_readiness(&self) -> Readiness {
        if !self.prune_enabled {
            return Readiness::Disabled(PRUNE_ENABLED_VAR);
        }
        match self.archive_readiness() {
            Readiness::Ready => Readiness::Ready,
            Readiness::Disabled(_) => Readiness::PruneWithoutArchive,
            other => other,
        }
    }

    pub fn from_env() -> Result<Self, String> {
        let mut c = Self::default();

        if let Some(raw) = present(RETENTION_VAR) {
            c.retention_hours = parse_retention_hours(&raw)?;
        }

        c.archive_enabled = flag(ARCHIVE_ENABLED_VAR)?;
        c.prune_enabled = flag(PRUNE_ENABLED_VAR)?;
        c.bucket = present(BUCKET_VAR);

        if let Some(raw) = present(INTERVAL_VAR) {
            let v: u64 = raw
                .parse()
                .map_err(|_| format!("{INTERVAL_VAR}={raw:?} is not a number of seconds"))?;
            if v == 0 {
                return Err(format!("{INTERVAL_VAR}=0 would spin; set a positive interval"));
            }
            c.interval_secs = v;
        }
        if let Some(raw) = present(MAX_PER_PASS_VAR) {
            let v: usize = raw
                .parse()
                .map_err(|_| format!("{MAX_PER_PASS_VAR}={raw:?} is not a count"))?;
            if v == 0 {
                return Err(format!(
                    "{MAX_PER_PASS_VAR}=0 would archive nothing while reading enabled; \
                     use {ARCHIVE_ENABLED_VAR}=false to disable"
                ));
            }
            c.max_per_pass = v;
        }
        Ok(c)
    }
}

/// Why a pass can or cannot run. Named states rather than a bool, so "off" and
/// "on but pointed nowhere" are never the same reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    Disabled(&'static str),
    MisconfiguredNoBucket(&'static str),
    PruneWithoutArchive,
}

impl Readiness {
    pub fn is_ready(&self) -> bool {
        matches!(self, Readiness::Ready)
    }
    pub fn reason(&self) -> String {
        match self {
            Readiness::Ready => "ready".into(),
            Readiness::Disabled(v) => format!("{v} is not true"),
            Readiness::MisconfiguredNoBucket(v) => {
                format!("enabled but {v} is unset — it would upload nowhere")
            }
            Readiness::PruneWithoutArchive => format!(
                "{PRUNE_ENABLED_VAR} is true while {ARCHIVE_ENABLED_VAR} is not: pruning \
                 without archiving would delete hot data with no copy being produced"
            ),
        }
    }
}

/// Is this execution archivable, and if not, why not?
///
/// ⚠ Three distinct not-archivable reasons rather than a bool. "Still running" and
/// "completed four minutes ago" need different operator responses, and a bool would make a
/// permanently-stuck execution look identical to a fresh one — which is exactly what pins
/// the retention floor (design §8.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Archivability {
    Archivable,
    /// Not in a terminal state — this is what can pin the floor indefinitely.
    NotTerminal,
    /// Terminal, but still inside the retention window.
    WithinRetention { age_hours: i64 },
    /// ⚠⚠ Terminal status with no `completed_at`. Treated as NOT archivable: without a
    /// completion time there is no way to know the window has elapsed, and guessing would
    /// archive-and-prune an execution that may have completed seconds ago.
    TerminalWithoutCompletedAt,
}

impl Archivability {
    pub fn is_archivable(&self) -> bool {
        matches!(self, Archivability::Archivable)
    }
}

/// The terminal statuses.
///
/// ⚠ `FAILED` and `CANCELLED` are terminal and therefore **archivable — never
/// discardable**. Measured on prod 2026-10-09: 458 of 6,467 executions are FAILED, and a
/// failed execution is usually the one someone wants to read later.
pub fn is_terminal_status(status: &str) -> bool {
    matches!(
        status.trim().to_ascii_uppercase().as_str(),
        "COMPLETED" | "FAILED" | "CANCELLED" | "CANCELED" | "ERROR"
    )
}

/// Decide archivability for one execution.
///
/// ⚠⚠ `status` must be the **event-derived** status. `noetl.execution.status` froze at the
/// Python retirement and still claims thousands of executions RUNNING
/// ([#235](https://github.com/noetl/ai-meta/issues/235)); driving retention off that column
/// would pin the floor forever on executions that finished weeks ago.
pub fn archivability(
    status: &str,
    completed_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    retention: Duration,
) -> Archivability {
    if !is_terminal_status(status) {
        return Archivability::NotTerminal;
    }
    let Some(done) = completed_at else {
        return Archivability::TerminalWithoutCompletedAt;
    };
    let age = now - done;
    // ⚠ `<=`, not `<`. "Older than the window" is strict: an execution that completed
    // exactly `retention` ago is not yet older than it. The first cut used `<` and made the
    // exact boundary archivable, which the boundary test caught — a one-microsecond-early
    // archive is a one-microsecond-early prune.
    if age <= retention {
        return Archivability::WithinRetention {
            age_hours: age.num_hours(),
        };
    }
    Archivability::Archivable
}

/// What one archivable-set scan looked at, alongside what it selected.
///
/// ⭐ The population is part of the result, not a detail. `GET /api/executions` caps `limit`
/// at 100 — a single call returned 100 where paging returned **6,467** — so a selection
/// count without its denominator is consistent with having examined one page.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ArchivableScan {
    pub examined: usize,
    pub archivable: Vec<i64>,
    pub not_terminal: usize,
    pub within_retention: usize,
    pub terminal_without_completed_at: usize,
    /// Pages fetched, so a truncated walk is visible in the result.
    pub pages: usize,
}

impl ArchivableScan {
    /// ⚠ The oldest non-terminal execution is what pins the retention floor. Surfaced from
    /// the scan so it can be exported as a metric rather than rediscovered later.
    pub fn accounted(&self) -> usize {
        self.archivable.len()
            + self.not_terminal
            + self.within_retention
            + self.terminal_without_completed_at
    }
}

/// Classify a page of summaries into a scan, accumulating.
pub fn classify_into(
    scan: &mut ArchivableScan,
    rows: &[(i64, String, Option<DateTime<Utc>>)],
    now: DateTime<Utc>,
    retention: Duration,
) {
    scan.pages += 1;
    for (id, status, completed) in rows {
        scan.examined += 1;
        match archivability(status, *completed, now, retention) {
            Archivability::Archivable => scan.archivable.push(*id),
            Archivability::NotTerminal => scan.not_terminal += 1,
            Archivability::WithinRetention { .. } => scan.within_retention += 1,
            Archivability::TerminalWithoutCompletedAt => {
                scan.terminal_without_completed_at += 1
            }
        }
    }
}

/// Parse the retention window.
///
/// Public and pure **so the rejection paths are testable without mutating process env** —
/// `cargo test` does not serialise tests, and these are the branches that decide whether
/// data gets deleted, so they are the last place to settle for a decorative assertion.
pub fn parse_retention_hours(raw: &str) -> Result<u64, String> {
    let hours: u64 = raw.trim().parse().map_err(|_| {
        format!(
            "{RETENTION_VAR}={raw:?} is not a whole number of hours. \
             Default {DEFAULT_RETENTION_HOURS}; 24 is supported."
        )
    })?;
    if hours == 0 {
        // ⚠⚠ Zero is refused, not clamped. "Retain nothing" would archive and prune an
        // execution the instant it completed — indistinguishable from a misconfiguration,
        // and never what an operator means.
        return Err(format!(
            "{RETENTION_VAR}=0 would retain nothing and prune executions as they complete. \
             Set a positive number of hours (default {DEFAULT_RETENTION_HOURS})."
        ));
    }
    if hours > MAX_RETENTION_HOURS {
        return Err(format!(
            "{RETENTION_VAR}={hours} exceeds the {MAX_RETENTION_HOURS}h ceiling; a value \
             this large is almost always a typo and would silently never archive."
        ));
    }
    Ok(hours)
}

/// Parse a boolean flag value. Public and pure for the same reason as
/// [`parse_retention_hours`].
pub fn parse_flag(var: &str, raw: &str) -> Result<bool, String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(format!(
            "{var}={raw:?} is not a boolean (use true/false, 1/0, yes/no, on/off). Refused \
             rather than read as false, because a typo in a data-deleting flag must not \
             look like 'off'."
        )),
    }
}

fn present(var: &str) -> Option<String> {
    match std::env::var(var) {
        Ok(v) if !v.trim().is_empty() => Some(v.trim().to_string()),
        _ => None,
    }
}

/// Parse a boolean flag.
///
/// ⚠ Accepts the forms the rest of the worker/server fleet accepts, case-insensitively and
/// trimmed. The gateway's `NOETL_AUTH_SYNC` takes only exactly `"true"`/`"1"` — so `"TRUE"`
/// silently means false there — and a value copied between components changing meaning is
/// a documented trap. An unrecognised value is an **error**, never a silent false.
fn flag(var: &str) -> Result<bool, String> {
    match present(var) {
        None => Ok(false),
        Some(raw) => parse_flag(var, &raw),
    }
}
