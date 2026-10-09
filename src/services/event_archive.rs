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

// ===========================================================================
// P2 — archive one execution, in the exact layout, verifiably.
// ===========================================================================

use crate::services::archive_store::ArchiveStore;
use sha2::{Digest, Sha256};

/// One record as the archive stores it.
///
/// Deliberately not `EventRecord`: the archive layer needs a sequence, an identity and a
/// body, and coupling it to the live wire type would make a future field change a format
/// change in the archive — which is append-only-forever storage.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArchiveRecord {
    pub global_sequence: u64,
    pub event_id: i64,
    pub event_type: String,
    pub body: serde_json::Value,
}

/// What the archive knows about one archived execution. Written as `manifest.json`.
///
/// ⭐ Carries the digest and count so a retrieval can be **verified**, not merely fetched.
/// An archive you cannot check is an archive you cannot safely prune behind.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArchiveManifest {
    pub schema_version: u32,
    pub execution_id: i64,
    pub status: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub record_count: u64,
    pub min_sequence: u64,
    pub max_sequence: u64,
    /// `sha256` of the data object, hex.
    pub data_sha256: String,
    pub data_key: String,
    pub archived_at: DateTime<Utc>,
    pub store: String,
}

/// Current archive schema. Bumped only for a breaking layout change.
pub const ARCHIVE_SCHEMA_VERSION: u32 = 1;

/// `execution_id=<id>/` — the data prefix required by the design.
pub fn execution_prefix(execution_id: i64) -> String {
    format!("execution_id={execution_id}/")
}

/// The data object key for a sequence range.
pub fn data_key(execution_id: i64, min_sequence: u64, max_sequence: u64) -> String {
    format!(
        "{}events-{:020}-{:020}.jsonl",
        execution_prefix(execution_id),
        min_sequence,
        max_sequence
    )
}

/// `execution_id=<id>/manifest.json`.
pub fn manifest_key(execution_id: i64) -> String {
    format!("{}manifest.json", execution_prefix(execution_id))
}

/// Serialise records as JSONL — one record per line, ascending by sequence.
///
/// ⚠ Sorted here rather than trusting the caller. The archive is the surviving copy, and a
/// replay reads it in order; an unsorted archive would be a correct-looking object that
/// replays wrong. JSONL also matches the `tier/eventbus/*.jsonl` snapshots already in that
/// bucket, so the format is greppable with tools an operator already has.
pub fn encode_records(records: &[ArchiveRecord]) -> Result<Vec<u8>, String> {
    let mut sorted: Vec<&ArchiveRecord> = records.iter().collect();
    sorted.sort_by_key(|r| (r.global_sequence, r.event_id));
    let mut out = Vec::new();
    for r in sorted {
        let line = serde_json::to_vec(r).map_err(|e| format!("encode record: {e}"))?;
        out.extend_from_slice(&line);
        out.push(b'\n');
    }
    Ok(out)
}

/// Decode a JSONL archive object.
pub fn decode_records(bytes: &[u8]) -> Result<Vec<ArchiveRecord>, String> {
    let mut out = Vec::new();
    for (i, line) in bytes.split(|b| *b == b'\n').enumerate() {
        if line.is_empty() {
            continue;
        }
        out.push(
            serde_json::from_slice::<ArchiveRecord>(line)
                .map_err(|e| format!("decode record on line {}: {e}", i + 1))?,
        );
    }
    Ok(out)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Metadata the archive needs about the execution being archived.
#[derive(Debug, Clone)]
pub struct ExecutionMeta {
    pub execution_id: i64,
    pub status: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

/// Archive one execution's records.
///
/// Writes the data object, then `manifest.json`. **Does not verify** — verification is a
/// separate call (P3) so that "uploaded" and "verified durable" can never be the same
/// state, which is what makes prune-after-durable expressible at all.
pub async fn archive_execution(
    store: &dyn ArchiveStore,
    meta: &ExecutionMeta,
    records: &[ArchiveRecord],
    now: DateTime<Utc>,
) -> Result<ArchiveManifest, String> {
    if records.is_empty() {
        // ⚠ Refused rather than written as an empty archive. An execution with no records
        // is either a read bug or a genuinely empty execution, and an empty archive object
        // would later verify fine and authorise pruning events that were never copied.
        return Err(format!(
            "execution {} has no records to archive; refusing to write an empty archive \
             that would later verify clean and authorise a prune",
            meta.execution_id
        ));
    }
    let bytes = encode_records(records)?;
    let min = records.iter().map(|r| r.global_sequence).min().unwrap();
    let max = records.iter().map(|r| r.global_sequence).max().unwrap();
    let dkey = data_key(meta.execution_id, min, max);
    let digest = sha256_hex(&bytes);

    store.put(&dkey, "application/x-ndjson", &bytes).await?;

    let manifest = ArchiveManifest {
        schema_version: ARCHIVE_SCHEMA_VERSION,
        execution_id: meta.execution_id,
        status: meta.status.clone(),
        started_at: meta.started_at,
        completed_at: meta.completed_at,
        record_count: records.len() as u64,
        min_sequence: min,
        max_sequence: max,
        data_sha256: digest,
        data_key: dkey,
        archived_at: now,
        store: store.describe(),
    };
    let mbytes = serde_json::to_vec_pretty(&manifest).map_err(|e| format!("encode manifest: {e}"))?;
    store
        .put(&manifest_key(meta.execution_id), "application/json", &mbytes)
        .await?;
    Ok(manifest)
}

/// Why an archive is or is not durable. Named states, because "not durable" has causes that
/// need different responses and a bool would collapse them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Durability {
    Durable { record_count: u64 },
    ManifestMissing,
    DataMissing { key: String },
    DigestMismatch { expected: String, actual: String },
    CountMismatch { expected: u64, actual: u64 },
    Undecodable(String),
}

impl Durability {
    pub fn is_durable(&self) -> bool {
        matches!(self, Durability::Durable { .. })
    }
    pub fn reason(&self) -> String {
        match self {
            Durability::Durable { record_count } => {
                format!("durable ({record_count} records verified)")
            }
            Durability::ManifestMissing => "manifest.json is absent".into(),
            Durability::DataMissing { key } => format!("data object {key} is absent"),
            Durability::DigestMismatch { expected, actual } => format!(
                "sha256 mismatch: manifest says {expected}, stored bytes hash to {actual}"
            ),
            Durability::CountMismatch { expected, actual } => {
                format!("record count mismatch: manifest says {expected}, object holds {actual}")
            }
            Durability::Undecodable(e) => format!("archive object does not decode: {e}"),
        }
    }
}

/// Verify an archived execution by **reading it back**.
///
/// ⚠⚠ This is the gate the prune depends on, so it reads the bytes and re-hashes them. A
/// `put` returning `Ok` is not durability — and a check that consults only the manifest
/// would be reading the thing it is verifying, the [#332 AC14](https://github.com/noetl/ai-meta/issues/332)
/// defect where a verification performed 2,212 prod reads on a check that could not fail.
pub async fn verify_archived(
    store: &dyn ArchiveStore,
    execution_id: i64,
) -> Result<Durability, String> {
    let Some(mbytes) = store.get(&manifest_key(execution_id)).await? else {
        return Ok(Durability::ManifestMissing);
    };
    let manifest: ArchiveManifest =
        serde_json::from_slice(&mbytes).map_err(|e| format!("manifest decode: {e}"))?;
    let Some(dbytes) = store.get(&manifest.data_key).await? else {
        return Ok(Durability::DataMissing {
            key: manifest.data_key,
        });
    };
    let actual = sha256_hex(&dbytes);
    if actual != manifest.data_sha256 {
        return Ok(Durability::DigestMismatch {
            expected: manifest.data_sha256,
            actual,
        });
    }
    let records = match decode_records(&dbytes) {
        Ok(r) => r,
        Err(e) => return Ok(Durability::Undecodable(e)),
    };
    if records.len() as u64 != manifest.record_count {
        return Ok(Durability::CountMismatch {
            expected: manifest.record_count,
            actual: records.len() as u64,
        });
    }
    Ok(Durability::Durable {
        record_count: manifest.record_count,
    })
}

/// Read an archived execution back — retrieval by execution id.
///
/// ⚠⚠ Returns `Ok(None)` **only** when the execution is genuinely absent from the archive,
/// and an error when the archive is present but broken. A pruned-but-corrupt execution must
/// never read as "no such execution": this codebase's most expensive recurring failure is a
/// wrong lookup answering with silence (a wrong GCP project returns an empty collection, not
/// a 404).
pub async fn read_archived(
    store: &dyn ArchiveStore,
    execution_id: i64,
) -> Result<Option<(ArchiveManifest, Vec<ArchiveRecord>)>, String> {
    let Some(mbytes) = store.get(&manifest_key(execution_id)).await? else {
        return Ok(None);
    };
    let manifest: ArchiveManifest =
        serde_json::from_slice(&mbytes).map_err(|e| format!("manifest decode: {e}"))?;
    let Some(dbytes) = store.get(&manifest.data_key).await? else {
        return Err(format!(
            "execution {execution_id} is archived (manifest present) but its data object \
             {} is MISSING — this is a broken archive, not an absent execution",
            manifest.data_key
        ));
    };
    let actual = sha256_hex(&dbytes);
    if actual != manifest.data_sha256 {
        return Err(format!(
            "execution {execution_id} archive is CORRUPT: manifest sha256 {} vs stored {}",
            manifest.data_sha256, actual
        ));
    }
    let records = decode_records(&dbytes)?;
    Ok(Some((manifest, records)))
}

// ===========================================================================
// P3 — the by-date reference index.
// ===========================================================================

/// `date=YYYY-MM-DD/` — the reference prefix required by the design.
pub fn date_prefix(date: &str) -> String {
    format!("date={date}/")
}

/// `date=YYYY-MM-DD/execution_id=<id>.json`.
///
/// ⭐⭐ **One object per execution, not one appended manifest per day.** GCS has no append:
/// a single per-day object would be read-modify-write, and two executions archiving
/// concurrently would lose one of the entries with nothing reporting it. One object per
/// execution makes every write independent and idempotent, and turns discovery-by-date into
/// a `list(prefix)` — which the object client already supports. A rolled-up daily index can
/// be *derived* later by a compaction pass; it must never be the write path.
pub fn date_ref_key(date: &str, execution_id: i64) -> String {
    format!("{}execution_id={}.json", date_prefix(date), execution_id)
}

/// The UTC date an execution **started**, as the index partitions on.
pub fn start_date(started_at: DateTime<Utc>) -> String {
    started_at.format("%Y-%m-%d").to_string()
}

/// A by-date index entry: enough to decide whether to fetch the execution, without
/// fetching it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DateReference {
    pub execution_id: i64,
    pub status: String,
    pub started_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
    pub record_count: u64,
    pub data_key: String,
    pub archived_at: DateTime<Utc>,
}

impl DateReference {
    pub fn from_manifest(m: &ArchiveManifest) -> Self {
        Self {
            execution_id: m.execution_id,
            status: m.status.clone(),
            started_at: m.started_at,
            completed_at: m.completed_at,
            record_count: m.record_count,
            data_key: m.data_key.clone(),
            archived_at: m.archived_at,
        }
    }
}

/// Write the by-date reference for an archived execution.
pub async fn write_date_reference(
    store: &dyn ArchiveStore,
    manifest: &ArchiveManifest,
) -> Result<String, String> {
    let date = start_date(manifest.started_at);
    let key = date_ref_key(&date, manifest.execution_id);
    let body = serde_json::to_vec_pretty(&DateReference::from_manifest(manifest))
        .map_err(|e| format!("encode date reference: {e}"))?;
    store.put(&key, "application/json", &body).await?;
    Ok(key)
}

/// Parse an execution id out of a `date=…/execution_id=<id>.json` key.
///
/// ⚠ Returns `None` rather than guessing for anything that is not that shape, so a stray
/// object under the prefix cannot become a phantom execution id in a listing.
pub fn execution_id_from_date_key(key: &str) -> Option<i64> {
    let file = key.rsplit('/').next()?;
    let rest = file.strip_prefix("execution_id=")?.strip_suffix(".json")?;
    rest.parse::<i64>().ok()
}

/// Discovery by date — every execution the archive holds that **started** on `date`.
///
/// ⚠ `truncated` is returned rather than hidden. A listing capped at `limit` that silently
/// returned a short list would read exactly like a quiet day, which is the same class of
/// defect as `GET /api/executions` capping at 100 — that one at least says so in a header.
pub async fn list_executions_for_date(
    store: &dyn ArchiveStore,
    date: &str,
    limit: usize,
) -> Result<DateListing, String> {
    let prefix = date_prefix(date);
    let keys = store.list(&prefix, limit).await?;
    let truncated = keys.len() >= limit;
    let mut execution_ids = Vec::new();
    let mut unrecognised = 0usize;
    for k in &keys {
        match execution_id_from_date_key(k) {
            Some(id) => execution_ids.push(id),
            None => unrecognised += 1,
        }
    }
    execution_ids.sort_unstable();
    Ok(DateListing {
        date: date.to_string(),
        execution_ids,
        keys_seen: keys.len(),
        unrecognised,
        truncated,
    })
}

/// A by-date listing, with the population it was drawn from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateListing {
    pub date: String,
    pub execution_ids: Vec<i64>,
    /// ⭐ The denominator: how many keys the prefix listing returned at all.
    pub keys_seen: usize,
    /// Keys under the prefix that are not references — surfaced, not silently dropped.
    pub unrecognised: usize,
    pub truncated: bool,
}

/// Fetch one by-date reference.
pub async fn read_date_reference(
    store: &dyn ArchiveStore,
    date: &str,
    execution_id: i64,
) -> Result<Option<DateReference>, String> {
    let Some(b) = store.get(&date_ref_key(date, execution_id)).await? else {
        return Ok(None);
    };
    serde_json::from_slice(&b)
        .map(Some)
        .map_err(|e| format!("date reference decode: {e}"))
}

/// Archive an execution **and** index it by date — the complete archive operation.
///
/// ⚠ The date reference is part of the archive, not an optional extra: an execution that is
/// archived but absent from the index is undiscoverable by date, and nothing else would ever
/// report the hole. So a failure here fails the whole operation, and the prune gate requires
/// the reference too (see [`archive_state`]).
pub async fn archive_execution_indexed(
    store: &dyn ArchiveStore,
    meta: &ExecutionMeta,
    records: &[ArchiveRecord],
    now: DateTime<Utc>,
) -> Result<(ArchiveManifest, String), String> {
    let manifest = archive_execution(store, meta, records, now).await?;
    let key = write_date_reference(store, &manifest).await?;
    Ok((manifest, key))
}

// ===========================================================================
// P4 — prune only after the archive is durable.
// ===========================================================================

/// Whether an execution's archive is complete enough to prune its hot copy behind.
///
/// ⚠⚠ Requires the **date reference** as well as a verified data object. An execution that
/// is archived but absent from the by-date index is undiscoverable by date, and nothing
/// else in the system would ever report that hole — so pruning behind it would leave data
/// that exists and cannot be found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveState {
    /// Verified durable AND indexed. The only state a prune may advance past.
    SafeToPrune { record_count: u64 },
    /// Not archived at all.
    NotArchived,
    /// Archived but the data failed verification.
    NotDurable(Durability),
    /// Data is durable but the by-date reference is missing.
    DurableButUnindexed { date: String },
}

impl ArchiveState {
    pub fn is_safe_to_prune(&self) -> bool {
        matches!(self, ArchiveState::SafeToPrune { .. })
    }
    pub fn reason(&self) -> String {
        match self {
            ArchiveState::SafeToPrune { record_count } => {
                format!("safe to prune ({record_count} records verified and indexed)")
            }
            ArchiveState::NotArchived => "not archived".into(),
            ArchiveState::NotDurable(d) => format!("archive not durable: {}", d.reason()),
            ArchiveState::DurableButUnindexed { date } => format!(
                "data is durable but date={date} holds no reference — the execution would \
                 be unfindable by date after a prune"
            ),
        }
    }
}

/// The prune gate for one execution: read the archive back and check the index.
pub async fn archive_state(
    store: &dyn ArchiveStore,
    execution_id: i64,
) -> Result<ArchiveState, String> {
    let Some(mbytes) = store.get(&manifest_key(execution_id)).await? else {
        return Ok(ArchiveState::NotArchived);
    };
    let manifest: ArchiveManifest =
        serde_json::from_slice(&mbytes).map_err(|e| format!("manifest decode: {e}"))?;
    let durability = verify_archived(store, execution_id).await?;
    let Durability::Durable { record_count } = durability else {
        return Ok(ArchiveState::NotDurable(durability));
    };
    let date = start_date(manifest.started_at);
    if read_date_reference(store, &date, execution_id).await?.is_none() {
        return Ok(ArchiveState::DurableButUnindexed { date });
    }
    Ok(ArchiveState::SafeToPrune { record_count })
}

/// One execution's footprint in the hot log, for the floor computation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotExecution {
    pub execution_id: i64,
    /// The lowest `global_sequence` this execution has any record at.
    pub min_sequence: u64,
    /// The highest `global_sequence` this execution has any record at.
    ///
    /// ⚠ Needed for the all-archived case. The first cut computed that floor as
    /// `max(min_sequence) + 1`, which is the highest *first* sequence — far below the log
    /// tip — so a fully-archived log reclaimed **nothing** while every call returned `Ok`.
    /// Caught only because the end-to-end test asserts on *reclaimed bytes* rather than on
    /// the call succeeding.
    pub max_sequence: u64,
}

/// The retention floor: the lowest sequence that must be KEPT.
///
/// ⭐ The bridge between per-execution archival and per-part pruning. Archival is exact
/// (one execution at a time); pruning is coarse (whole immutable parts). The floor is the
/// minimum `min_sequence` over every execution that is **not** safe to prune, so a part
/// entirely below it can only contain executions whose archives are verified and indexed.
///
/// ⚠⚠ `None` means **prune nothing**. That is the correct answer whenever any execution is
/// unarchived — including a still-running one, whose earliest events sit in the oldest
/// parts. Dropping those would destroy the first half of a live execution, and
/// `noetl.event` is append-only with replay as the source of truth, so that is data loss
/// rather than eviction.
pub fn retention_floor(hot: &[HotExecution], safe_to_prune: &[i64]) -> FloorDecision {
    let safe: std::collections::HashSet<i64> = safe_to_prune.iter().copied().collect();
    let mut blocking: Option<&HotExecution> = None;
    for e in hot {
        if safe.contains(&e.execution_id) {
            continue;
        }
        if blocking.is_none_or(|b| e.min_sequence < b.min_sequence) {
            blocking = Some(e);
        }
    }
    match blocking {
        // ⚠ Every execution is archived: the floor is above everything, so all parts are
        // droppable. Expressed as the max sequence + 1 rather than u64::MAX so the number
        // means something in a log line.
        None => {
            // The log tip, not the highest first-sequence: when everything is archived,
            // nothing must be kept.
            let tip = hot.iter().map(|e| e.max_sequence).max().unwrap_or(0);
            FloorDecision {
                keep_from_sequence: Some(tip.saturating_add(1)),
                blocked_by: None,
                hot_examined: hot.len(),
                safe_count: safe.len(),
            }
        }
        Some(b) => FloorDecision {
            keep_from_sequence: Some(b.min_sequence),
            blocked_by: Some(b.clone()),
            hot_examined: hot.len(),
            safe_count: safe.len(),
        },
    }
}

/// The floor, plus **what is holding it back** and the population it was computed over.
///
/// ⚠⚠ `blocked_by` is the load-bearing field, not a diagnostic. One stuck execution pins
/// the floor and blocks ALL reclamation while every other counter reads healthy — the
/// drift hazard of this whole design (spec §8.1). It is surfaced here so it can be
/// exported as a metric rather than rediscovered during an incident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FloorDecision {
    pub keep_from_sequence: Option<u64>,
    pub blocked_by: Option<HotExecution>,
    /// ⭐ The denominator.
    pub hot_examined: usize,
    pub safe_count: usize,
}

impl FloorDecision {
    /// Would pruning at this floor drop anything, given a part's sequence range?
    ///
    /// Mirrors `ehdb_l0::plan_retention`: a part is dropped only when it lies **entirely**
    /// below the floor. A part straddling the floor is kept whole — drop-partition never
    /// splits a part.
    pub fn part_is_droppable(&self, part_min: u64, part_max: u64) -> bool {
        debug_assert!(part_min <= part_max);
        match self.keep_from_sequence {
            None => false,
            Some(floor) => part_max < floor,
        }
    }
    pub fn describe(&self) -> String {
        match (&self.keep_from_sequence, &self.blocked_by) {
            (None, _) => "prune nothing".into(),
            (Some(f), None) => format!(
                "keep_from={f} (all {} hot executions archived)",
                self.hot_examined
            ),
            (Some(f), Some(b)) => format!(
                "keep_from={f}, pinned by execution {} at sequence {} ({} of {} hot \
                 executions safe to prune)",
                b.execution_id, b.min_sequence, self.safe_count, self.hot_examined
            ),
        }
    }
}

// ===========================================================================
// P6 (partial) — export the pinned-floor hazard.
// ===========================================================================

/// Publish a floor decision to the gauges.
///
/// ⚠⚠ The age is passed in rather than derived here, because "how old is the execution
/// pinning the floor" is a question about wall-clock completion times that this module
/// deliberately does not own — and a gauge computed from the wrong clock would be worse
/// than no gauge.
pub fn export_floor(decision: &FloorDecision, blocking_age_secs: Option<i64>) {
    if let Some(f) = decision.keep_from_sequence {
        // Saturating: a u64 sequence does not fit an i64 gauge, and a wrapped negative
        // sequence would read as a plausible number rather than as an overflow.
        crate::metrics::ehdb_retention_floor_sequence().set(f.min(i64::MAX as u64) as i64);
    }
    match &decision.blocked_by {
        Some(b) => {
            crate::metrics::ehdb_retention_floor_blocked_by_execution().set(b.execution_id);
            crate::metrics::ehdb_retention_oldest_non_archivable_age_seconds()
                .set(blocking_age_secs.unwrap_or(0).max(0));
        }
        None => {
            // ⚠ Reset BOTH. A stale id left behind after the block clears would name an
            // execution that is no longer blocking anything — a representation that
            // outlived the thing it described.
            crate::metrics::ehdb_retention_floor_blocked_by_execution().set(0);
            crate::metrics::ehdb_retention_oldest_non_archivable_age_seconds().set(0);
        }
    }
}

/// Publish an archivable-set scan to the gauges — both numbers, so the selection always
/// carries the population it came from.
pub fn export_scan(scan: &ArchivableScan) {
    crate::metrics::ehdb_archive_scan_examined().set(scan.examined as i64);
    crate::metrics::ehdb_archive_scan_archivable().set(scan.archivable.len() as i64);
}
