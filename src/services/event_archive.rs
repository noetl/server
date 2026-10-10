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
    /// The producer's event id, **as the source actually carries it**.
    ///
    /// ⚠⚠ `Option<String>`, not `i64`. The shadow append path builds records with
    /// `EventRecord::new(..)`, which does not set that column, so it is `None` on **every
    /// record the shadow wrote** — and the shadow is exactly what gets archived. An `i64`
    /// would have forced a fabricated 0 for every record, collapsing the whole set onto
    /// one value: the same mistake `read_embedded` documents avoiding in the comparator.
    ///
    /// Changed before anything was archived. The archive is append-only-forever storage,
    /// so this was free now and would have been a migration later.
    pub event_id: Option<String>,
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
    sorted.sort_by(|a, b| (a.global_sequence, &a.event_id).cmp(&(b.global_sequence, &b.event_id)));
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

// ===========================================================================
// P5 — retrieval: hot first, archive on miss.
// ===========================================================================

/// Where an execution's events were served from.
///
/// ⚠⚠ Three states, never two. A pruned-and-archived execution, an execution that
/// predates the store, and a genuinely unknown id are different answers, and collapsing
/// any pair of them is the failure this codebase keeps paying for — a wrong lookup that
/// answers with silence rather than an error.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "source")]
pub enum ReadSource {
    /// Served from the hot store.
    Hot { records: usize },
    /// The hot store no longer holds it; served from the archive.
    Archive {
        records: usize,
        data_key: String,
        archived_at: DateTime<Utc>,
    },
    /// Neither the hot store nor the archive has it.
    NotFound,
}

impl ReadSource {
    pub fn label(&self) -> &'static str {
        match self {
            ReadSource::Hot { .. } => "hot",
            ReadSource::Archive { .. } => "archive",
            ReadSource::NotFound => "not_found",
        }
    }
}

/// The result of a fall-through read.
#[derive(Debug, Clone)]
pub struct FallthroughRead {
    pub source: ReadSource,
    pub records: Vec<ArchiveRecord>,
}

/// Read an execution's events: the hot store if it still has them, otherwise the archive.
///
/// `hot` is a closure rather than an engine handle so this is testable without opening an
/// L0 engine, and so the caller decides what "hot" means (the embedded shadow today; a
/// serving tier later).
///
/// ⚠⚠ An **empty** hot result is a miss, not an answer. After a prune the hot store
/// legitimately holds zero records for an old execution, and treating that as "the
/// execution has no events" would turn every pruned execution into a silent empty success.
/// That is precisely the shape of the bug this function exists to prevent.
///
/// ⚠⚠ A **broken** archive is an error, never a `NotFound`. If the manifest is present the
/// execution was archived, so a missing or corrupt data object is a broken archive — and
/// reporting it as absence would hide data loss behind a clean 404.
pub async fn read_with_fallthrough<F>(
    store: &dyn ArchiveStore,
    execution_id: i64,
    hot: F,
) -> Result<FallthroughRead, String>
where
    F: FnOnce() -> Option<Vec<ArchiveRecord>>,
{
    if let Some(records) = hot() {
        if !records.is_empty() {
            let n = records.len();
            return Ok(FallthroughRead {
                source: ReadSource::Hot { records: n },
                records,
            });
        }
        // Fall through: an open-but-empty hot store means pruned (or never held), and the
        // archive is the place to look.
    }
    match read_archived(store, execution_id).await? {
        Some((manifest, records)) => {
            let n = records.len();
            Ok(FallthroughRead {
                source: ReadSource::Archive {
                    records: n,
                    data_key: manifest.data_key,
                    archived_at: manifest.archived_at,
                },
                records,
            })
        }
        None => Ok(FallthroughRead {
            source: ReadSource::NotFound,
            records: Vec::new(),
        }),
    }
}

/// Open the archive store from env, or `None` when archiving is not configured.
///
/// ⚠ Returns `None` rather than a store pointed at nothing, so the read routes can answer
/// 503-with-a-reason instead of an empty 200. "Not configured" and "nothing archived" are
/// different facts and must not share a response.
pub fn open_archive_store() -> Option<std::sync::Arc<dyn ArchiveStore>> {
    let bucket = std::env::var("NOETL_EHDB_ARCHIVE_BUCKET").ok()?;
    let bucket = bucket.trim();
    if bucket.is_empty() {
        return None;
    }
    let endpoint = std::env::var("NOETL_EHDB_ARCHIVE_ENDPOINT")
        .or_else(|_| std::env::var("NOETL_OBJECT_STORE_GCS_ENDPOINT"))
        .unwrap_or_else(|_| "https://storage.googleapis.com".to_string());

    // Reuse the object backend's own resolution, so prod gets ADC/Workload Identity and a
    // non-Google endpoint gets the open emulator — one place decides how auth works.
    match crate::services::object_backend::GcsBackend::open_for_archive(&endpoint, bucket) {
        Ok(b) => Some(std::sync::Arc::new(b)),
        Err(e) => {
            tracing::warn!(
                bucket, endpoint = %endpoint, error = %e,
                "the event archive store could not be opened; archive reads will 503"
            );
            None
        }
    }
}

// ===========================================================================
// The pass driver — the piece that was missing.
// ===========================================================================
//
// ⚠⚠ P1–P6 shipped the primitives (config, archive, verify, floor, prune, metrics,
// retrieval) and NOTHING CALLED THEM. Verified with a control grep against `origin/main`:
// `archive_execution_indexed`, `archive_execution`, `verify_archived`, `archive_state`,
// `export_floor` and `export_scan` had zero callers outside their own module, and there was
// no spawn in `main.rs`. So `NOETL_EHDB_ARCHIVE_ENABLED=true` would have archived nothing
// while every flag and metric read exactly as a working deployment does.
//
// That is the "built ahead of its consumers" pattern this program keeps finding in other
// code, committed here across four PRs by the same author who was writing the issue
// comments about it. The driver below is the consumer.

/// One execution the pass may archive.
///
/// ⚠ A struct rather than a tuple: clippy flagged the 4-tuple as "very complex", and it was
/// right for a better reason than lint — the first draft of the pass re-found the same
/// candidate twice with `.iter().find(..)` to pull fields back out of positions, which is
/// exactly the fragility a named type removes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveCandidate {
    pub execution_id: i64,
    pub status: String,
    pub completed_at: Option<DateTime<Utc>>,
    pub started_at: DateTime<Utc>,
}

/// What one pass did, with the population it looked at.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PassReport {
    pub examined: usize,
    pub archivable: usize,
    pub already_archived: usize,
    pub archived: usize,
    pub out_of_coverage: usize,
    pub failed: usize,
    pub verified: usize,
    pub verify_failed: usize,
    /// Set when the pass did not run, with the reason.
    pub skipped: Option<String>,
}

impl PassReport {
    pub fn describe(&self) -> String {
        if let Some(why) = &self.skipped {
            return format!("skipped: {why}");
        }
        format!(
            "examined={} archivable={} archived={} already={} out_of_coverage={} \
             verified={} verify_failed={} failed={}",
            self.examined,
            self.archivable,
            self.archived,
            self.already_archived,
            self.out_of_coverage,
            self.verified,
            self.verify_failed,
            self.failed
        )
    }
}

/// One archive pass.
///
/// `candidates` is `(execution_id, status, completed_at, started_at)` — supplied by the
/// caller so this is testable without a database.
/// `read_records` sources the records; `None` means the hot store has nothing for that
/// execution (out of coverage), which is **skipped, never archived as empty**.
///
/// ⚠⚠ This pass **never prunes**. Pruning needs the per-execution minimum-sequence
/// footprint of the whole hot store to compute a safe floor, and there is no cheap source
/// for that yet — see [`prune_readiness_note`]. Archive-only is the correct first
/// increment: it is additive, and the prune cannot be safe before the archive is verified
/// anyway.
pub async fn archive_pass<F>(
    store: &dyn ArchiveStore,
    cfg: &RetentionConfig,
    candidates: &[ArchiveCandidate],
    now: DateTime<Utc>,
    known_archived: &mut std::collections::HashSet<i64>,
    known_out_of_coverage: &mut std::collections::HashSet<i64>,
    mut read_records: F,
) -> PassReport
where
    F: FnMut(i64) -> Option<Vec<ArchiveRecord>>,
{
    let mut rep = PassReport::default();

    let readiness = cfg.archive_readiness();
    if !readiness.is_ready() {
        rep.skipped = Some(readiness.reason());
        return rep;
    }

    let retention = cfg.retention();
    let mut scan = ArchivableScan::default();
    let rows: Vec<(i64, String, Option<DateTime<Utc>>)> = candidates
        .iter()
        .map(|c| (c.execution_id, c.status.clone(), c.completed_at))
        .collect();
    classify_into(&mut scan, &rows, now, retention);
    export_scan(&scan);
    rep.examined = scan.examined;
    rep.archivable = scan.archivable.len();

    // ⚠⚠ The budget bounds WORK, not candidates, and the known-archived set is skipped
    // before it is spent.
    //
    // The first version wrote `scan.archivable.iter().take(cfg.max_per_pass)`, which took
    // the SAME first 100 ids every pass. Once those were archived every later pass
    // re-checked them, reported `archived=0 already=100`, and THE BACKLOG STOPPED DRAINING
    // at exactly max_per_pass executions. Caught on prod by the `already` counter — the
    // metric earned its place.
    let mut budget = cfg.max_per_pass;
    for id in scan.archivable.iter() {
        if budget == 0 {
            break;
        }
        // Skipped for free: no GET, no budget. This is what lets a pass reach past the
        // first max_per_pass of a 6,230-long list.
        if known_archived.contains(id) {
            rep.already_archived += 1;
            continue;
        }
        // ⚠⚠ Same treatment, for the same reason, and its absence BLOCKED THE DRAIN on prod.
        //
        // An out-of-coverage execution can never become archivable: the engine holds no
        // records for it because it predates the volume. But the check that discovers that
        // happens AFTER `budget -= 1`, so each one cost a unit of budget — and nothing
        // remembered the answer. On prod `out_of_coverage` hit exactly `max_per_pass=100`
        // every pass: the budget was entirely consumed re-deciding the same 100 unarchivable
        // executions, `archived` sat at 0, and the ~2,479 genuinely archivable executions
        // further down the list were never reached. This is defect #514's shape reached by a
        // different route — work repeated because its result was not retained.
        if known_out_of_coverage.contains(id) {
            rep.out_of_coverage += 1;
            crate::metrics::record_ehdb_archive("out_of_coverage");
            continue;
        }
        // Not known: read the archive BACK to find out, which is also the idempotency
        // check — no separate bookkeeping store that could drift from the truth.
        match archive_state(store, *id).await {
            Ok(ArchiveState::SafeToPrune { .. }) => {
                known_archived.insert(*id);
                rep.already_archived += 1;
                crate::metrics::record_ehdb_archive("already_archived");
                continue;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(execution_id = id, error = %e, "archive state unreadable");
                budget -= 1;
                rep.failed += 1;
                crate::metrics::record_ehdb_archive("archive_failed");
                continue;
            }
        }
        budget -= 1;

        let Some(records) = read_records(*id) else {
            // The engine is not open at all — stop the pass rather than logging this once
            // per candidate.
            rep.skipped = Some("the embedded engine is not open".into());
            return rep;
        };
        if records.is_empty() {
            // ⚠ Out of coverage: the engine holds nothing for this execution because it
            // predates the volume. NOT an error, and NOT archivable — an empty archive
            // would later verify clean and authorise pruning events never copied.
            //
            // Remembered so the next pass skips it for free. The condition is monotonic: the
            // engine only ever holds records appended since it opened, so an execution with
            // none now will never acquire any.
            rep.out_of_coverage += 1;
            known_out_of_coverage.insert(*id);
            crate::metrics::record_ehdb_archive("out_of_coverage");
            continue;
        }

        let c = candidates
            .iter()
            .find(|c| c.execution_id == *id)
            .expect("the candidate the scan selected is in the input");
        let meta = ExecutionMeta {
            execution_id: c.execution_id,
            status: c.status.clone(),
            started_at: c.started_at,
            completed_at: c.completed_at,
        };

        match archive_execution_indexed(store, &meta, &records, now).await {
            Ok(_) => {
                rep.archived += 1;
                crate::metrics::record_ehdb_archive("archived");
                crate::metrics::record_ehdb_archive("indexed");
            }
            Err(e) => {
                tracing::warn!(execution_id = id, error = %e, "archive failed");
                rep.failed += 1;
                crate::metrics::record_ehdb_archive("archive_failed");
                continue;
            }
        }

        // ⭐ Verify by reading back, in the same pass. "Uploaded" and "verified durable"
        // must never be the same state — that distinction is what makes a later prune
        // expressible at all.
        match verify_archived(store, *id).await {
            Ok(d) if d.is_durable() => {
                // ⚠ Recorded as known-archived ONLY after verification, never after the
                // write. A set populated on write would let a later pass skip an execution
                // whose archive never landed.
                known_archived.insert(*id);
                rep.verified += 1;
                crate::metrics::record_ehdb_archive("verified_durable");
            }
            Ok(d) => {
                tracing::warn!(execution_id = id, reason = %d.reason(),
                    "archive wrote but did NOT verify durable");
                rep.verify_failed += 1;
                crate::metrics::record_ehdb_archive("verify_failed");
            }
            Err(e) => {
                tracing::warn!(execution_id = id, error = %e, "verification errored");
                rep.verify_failed += 1;
                crate::metrics::record_ehdb_archive("verify_failed");
            }
        }
    }
    rep
}

/// Why pruning is not yet performed even when `NOETL_EHDB_PRUNE_ENABLED=true`.
///
/// ⚠⚠ Returned and **logged loudly** rather than silently no-opping. An operator who sets
/// the flag and sees nothing happen must be told why — a flag that appears to work while
/// doing nothing is the defect this whole program keeps finding.
pub fn prune_readiness_note() -> &'static str {
    // ⚠⚠ This text was stale for one release and said the OPPOSITE of what the code does.
    //
    // It still claimed "pruning is NOT implemented ... nothing is deleted" after
    // noetl/server#516 wired the floor to `apply_retention`. On prod the two lines were
    // emitted together: "parts below the proven floor WILL be deleted" immediately followed
    // by a note saying nothing is deleted. A reader would reasonably have believed the note.
    //
    // It is the drift this whole tier is about, produced by me, in the one place an operator
    // looks when arming a destructive flag. Keep this string in step with the code or delete
    // it.
    "pruning IS wired: the pass computes a floor and drops only parts lying entirely below \
     it. The floor refuses while any archivable execution is neither verified-archived nor \
     out-of-coverage, so arming this flag reclaims nothing until the archive backlog is \
     drained — watch noetl_ehdb_retention_floor_sequence leave 0 and \
     noetl_ehdb_archive_total{outcome=\"pruned\"} move. See noetl/ai-meta#459."
}

/// Spawn the periodic archive pass.
///
/// ⚠ A no-op when archiving is not configured, and it says so **once** rather than
/// spinning a loop that logs every interval.
pub fn spawn_archive_pass(service: crate::services::execution::ExecutionService) {
    let cfg = match RetentionConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "retention config invalid; the archive pass will not run");
            return;
        }
    };
    let readiness = cfg.archive_readiness();
    if !readiness.is_ready() {
        tracing::info!(reason = %readiness.reason(), "archive pass not started");
        return;
    }
    let Some(store) = open_archive_store() else {
        tracing::warn!("archive pass not started: the store could not be opened");
        return;
    };
    if cfg.prune_enabled {
        // ⚠⚠ Loud, not silent. Pruning is now wired, but it only acts once the backlog is
        // fully archived — so an operator who sets the flag and sees no reclaimed bytes must
        // learn WHY from a log line rather than inferring it from the absence of an effect.
        tracing::warn!(
            note = prune_readiness_note(),
            "PRUNE_ENABLED is set: parts below the proven floor WILL be deleted from the log"
        );
    }
    tracing::info!(
        retention_hours = cfg.retention_hours,
        interval_secs = cfg.interval_secs,
        max_per_pass = cfg.max_per_pass,
        store = %store.describe(),
        // ⚠ The message can no longer say "archive-only" unconditionally: an operator
        // reading "nothing is deleted" on a pod that is in fact pruning has been told the
        // opposite of the truth. Fields rather than two literals, so the distinction is
        // queryable and not only readable.
        prune_armed = cfg.prune_enabled,
        deletes = if cfg.prune_enabled {
            "parts below the proven floor"
        } else {
            "nothing"
        },
        "archive pass starting"
    );
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(cfg.interval_secs));
        // ⚠ Lives across passes so a pass can progress past the first max_per_pass of the
        // archivable list. Empty on restart, which is correct: the GETs come back for one
        // cycle and re-establish the truth from the archive itself.
        let mut known_archived: std::collections::HashSet<i64> = std::collections::HashSet::new();
        // ⚠ Lives across passes for the same reason `known_archived` does, and its absence
        // was worse: an out-of-coverage execution costs budget to discover and can never
        // become archivable, so re-deciding it every pass consumed the entire budget and
        // the drain stopped dead. Empty on restart, which is correct — one pass re-discovers
        // the set, and the cost is bounded by `max_per_pass` per pass rather than forever.
        let mut known_out_of_coverage: std::collections::HashSet<i64> =
            std::collections::HashSet::new();
        // The last prune verdict, so a change is logged once at INFO rather than the same
        // line every interval forever.
        let mut last_prune_verdict: Option<String> = None;
        loop {
            tick.tick().await;
            let candidates = match collect_candidates(&service).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "archive pass could not list executions");
                    continue;
                }
            };
            let rep = archive_pass(
                store.as_ref(),
                &cfg,
                &candidates.candidates,
                Utc::now(),
                &mut known_archived,
                &mut known_out_of_coverage,
                |id: i64| crate::handlers::ehdb_embedded::read_archive_records(&id.to_string()),
            )
            .await;

            // ⚠ The prune step runs on the SAME known_archived set the archive pass just
            // updated — the executions this process has verified durable AND indexed. It is
            // deliberately a separate pass: archival is unaffected by whether pruning is on,
            // and the floor refuses by itself while any archivable execution is unarchived.
            // ⚠⚠ The floor's "satisfied" set is archived UNION out-of-coverage, not just
            // archived — and the difference is the whole reason prune could not run.
            //
            // An out-of-coverage execution is archivable by age and will never be archived
            // (the engine holds no records for it), so counting it as backlog means
            // `AwaitingBacklog` forever. Treating it as satisfied is safe because the floor
            // exists to protect parts that still hold an unarchived execution's records, and
            // an execution with no records is in no part.
            let satisfied: std::collections::HashSet<i64> = known_archived
                .union(&known_out_of_coverage)
                .copied()
                .collect();
            let prep = prune_pass(
                &cfg,
                &candidates,
                Utc::now(),
                &satisfied,
                crate::handlers::ehdb_embedded::hot_footprint,
                crate::handlers::ehdb_embedded::prune_below,
            );
            if prep.parts_dropped > 0 {
                tracing::info!(
                    parts_dropped = prep.parts_dropped,
                    bytes_reclaimed = prep.bytes_reclaimed,
                    keep_from_sequence = prep.keep_from_sequence,
                    "retention pruned parts below the proven floor"
                );
                last_prune_verdict = None;
            } else {
                // ⚠⚠ The refusal reason was `debug!` only, and prod does not emit debug.
                //
                // So while prune sat refused for hours, WHY was invisible: the
                // `prune_refused_*` counters said that it refused, and
                // `retention_floor_sequence` read 0 — but 0 is also what "never ran" looks
                // like, so neither answered the operator's question. Diagnosing it needed
                // the pass's own report, which was being written to a level nobody collects.
                //
                // Logged on TRANSITION rather than every pass: at INFO every interval this
                // would be ~288 identical lines a day and would be filtered out, which is
                // the same invisibility by a different route.
                let verdict = prep.describe();
                if last_prune_verdict.as_deref() != Some(verdict.as_str()) {
                    tracing::info!(summary = %verdict, "prune pass verdict changed");
                    last_prune_verdict = Some(verdict);
                } else {
                    tracing::debug!(summary = %verdict, "prune pass");
                }
            }
            tracing::info!(report = %rep.describe(), "archive pass complete");
        }
    });
}

/// Page the executions listing into pass candidates.
///
/// ⚠ Pages on `offset`. `GET /api/executions` caps `limit` at 100 and says so in
/// `x-noetl-limit-applied`; one call returned 100 where paging returned **6,467**, so a
/// single call would silently examine the newest page only — which is the page least
/// likely to contain anything archivable.
/// The candidate listing, carrying whether it is COMPLETE.
///
/// ⚠⚠ The completeness flag is load-bearing for pruning, not bookkeeping. The floor is
/// computed from the executions we classified; an execution the listing never returned is
/// invisible to it, and its records can sit below the floor and be deleted. The paging is
/// capped, so a large-enough population silently yields a short list — which is why the cap
/// being hit must travel with the data rather than being inferred from its length.
#[derive(Debug, Clone, Default)]
pub struct CandidateSet {
    pub candidates: Vec<ArchiveCandidate>,
    /// False when the page cap was reached with more rows possibly behind it.
    pub complete: bool,
}

pub async fn collect_candidates(
    service: &crate::services::execution::ExecutionService,
) -> Result<CandidateSet, String> {
    let mut out = Vec::new();
    let mut offset = 0i32;
    // Bounded so a pathological listing cannot spin the pass forever.
    const MAX_PAGES: usize = 200;
    let mut complete = false;
    for _ in 0..MAX_PAGES {
        let filter = crate::services::execution::ExecutionFilter {
            limit: Some(100),
            offset: Some(offset),
            ..Default::default()
        };
        let page = service.list(&filter).await.map_err(|e| e.to_string())?;
        if page.is_empty() {
            complete = true;
            break;
        }
        let n = page.len();
        for e in page {
            out.push(ArchiveCandidate {
                execution_id: e.execution_id,
                status: e.status,
                completed_at: e.completed_at,
                started_at: e.started_at,
            });
        }
        offset += n as i32;
        if n < 100 {
            complete = true;
            break;
        }
    }
    if !complete {
        tracing::warn!(
            collected = out.len(),
            "the execution listing hit its page cap; the candidate set is INCOMPLETE and              pruning will refuse this pass"
        );
    }
    Ok(CandidateSet { candidates: out, complete })
}

// ===========================================================================
// The prune floor — the piece the metadata could not give us.
// ===========================================================================
//
// ⚠⚠ The problem: `plan_retention` drops whole parts below a sort-key floor, and a part
// holds MANY executions interleaved. So the floor must be the minimum `global_sequence`
// over every execution that is NOT safe to prune — and `PartMeta` carries only a bloom,
// which cannot enumerate, so the metadata cannot answer it.
//
// ⭐ The way out came from the pass's own report: `examined=6496 archivable=6230`, i.e.
// only **266 executions are not archivable**. The constraining set is small and bounded by
// the retention window, not by history. Reading a min-sequence for 266 executions is
// cheap; reading it for 6,496 would not be.
//
// So the floor is computed from three groups:
//
//   1. NOT archivable (non-terminal, or still inside the window) → each one constrains.
//      ⚠ A live execution is in this group by construction, which is what makes the
//      data-loss case impossible rather than merely unlikely.
//   2. Archivable but NOT YET verified-archived → **the floor refuses to advance at all.**
//      Conservative on purpose: while a backlog exists, pruning could drop a part holding
//      an execution whose archive has not landed.
//   3. Out of coverage — the engine holds nothing for them, so they constrain nothing.

/// Why the floor is where it is. Named states, because "prune nothing" has causes that
/// need different operator responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FloorBasis {
    /// Every archivable execution is verified-archived; the floor is the minimum sequence
    /// of the executions that must stay hot.
    Computed {
        keep_from_sequence: u64,
        constrained_by: usize,
    },
    /// ⚠ A backlog exists: some archivable execution is not yet verified-archived.
    AwaitingBacklog { unarchived: usize },
    /// Nothing is hot that we could measure — prune nothing.
    NoFootprint,
}

impl FloorBasis {
    pub fn keep_from_sequence(&self) -> Option<u64> {
        match self {
            FloorBasis::Computed { keep_from_sequence, .. } => Some(*keep_from_sequence),
            _ => None,
        }
    }
    pub fn may_prune(&self) -> bool {
        matches!(self, FloorBasis::Computed { .. })
    }
    pub fn reason(&self) -> String {
        match self {
            FloorBasis::Computed { keep_from_sequence, constrained_by } => format!(
                "keep_from_sequence={keep_from_sequence}, constrained by {constrained_by} \
                 execution(s) that must stay hot"
            ),
            FloorBasis::AwaitingBacklog { unarchived } => format!(
                "refusing to advance: {unarchived} archivable execution(s) are not yet \
                 verified-archived, and a part may hold one of them"
            ),
            FloorBasis::NoFootprint => {
                "no measurable hot footprint; nothing to prune".to_string()
            }
        }
    }
}

/// Compute the floor from the scan, the verified-archived set, and the hot footprint of the
/// constraining executions.
///
/// `footprint` must cover **every** execution in groups 1 and 2 that the engine holds. An
/// execution missing from it is treated as out-of-coverage (constrains nothing) — ⚠ which is
/// only sound because the caller builds the footprint by *reading the engine*, so absence
/// there means the engine genuinely holds nothing.
pub fn compute_prune_floor(
    scan: &ArchivableScan,
    verified_archived: &std::collections::HashSet<i64>,
    footprint: &[HotExecution],
) -> FloorBasis {
    // Group 2 first: a backlog blocks everything.
    //
    // ⚠⚠ "Archived" here must include OUT-OF-COVERAGE executions, and leaving them out made
    // the floor permanently un-computable on prod. An out-of-coverage execution is archivable
    // by age and will never be archived, because the engine holds no records for it — so
    // counting it as a backlog item means `AwaitingBacklog` forever and prune can never run.
    //
    // It is also SAFE to treat it as satisfied, and that is the load-bearing half: the floor
    // exists to stop a part being dropped while it still holds an unarchived execution's
    // records. An execution the engine has no records for cannot be in any part, so no part
    // drop can lose it. Nothing to copy, nothing to protect.
    let unarchived = scan
        .archivable
        .iter()
        .filter(|id| !verified_archived.contains(id))
        .count();
    if unarchived > 0 {
        return FloorBasis::AwaitingBacklog { unarchived };
    }

    // Group 1: everything that must stay hot. The footprint holds exactly those the engine
    // has records for.
    let mut min_seq: Option<u64> = None;
    let mut constrained = 0usize;
    for h in footprint {
        if verified_archived.contains(&h.execution_id) {
            // Archived: does not constrain.
            continue;
        }
        constrained += 1;
        min_seq = Some(match min_seq {
            Some(m) => m.min(h.min_sequence),
            None => h.min_sequence,
        });
    }

    match min_seq {
        Some(s) => FloorBasis::Computed {
            keep_from_sequence: s,
            constrained_by: constrained,
        },
        // ⚠⚠ Nothing constrains — and the two reasons for that are NOT the same thing.
        //
        // An EMPTY footprint means we measured nothing, and absence of evidence is not
        // evidence of absence: refuse.
        //
        // A NON-EMPTY footprint whose every member is verified-archived is the opposite —
        // we measured, and everything we measured is safe. Refusing there is what the first
        // cut did, and it is why `max_sequence` exists on `HotExecution` and was then never
        // read: once prod's backlog drains, *that* is the steady state for an idle log, so
        // the whole prune path would have reclaimed zero while every call returned `Ok`.
        // Caught by a positive control asserting that a fully-archived log DOES reclaim.
        //
        // ⚠ The floor must clear the highest sequence those executions occupy, not their
        // highest *first* sequence — the same off-by-a-whole-log error `max_sequence` was
        // added for.
        None if footprint.is_empty() => FloorBasis::NoFootprint,
        None => {
            let tip = footprint.iter().map(|h| h.max_sequence).max().unwrap_or(0);
            FloorBasis::Computed {
                keep_from_sequence: tip.saturating_add(1),
                constrained_by: 0,
            }
        }
    }
}

/// The executions whose hot footprint the floor needs — groups 1 and 2 of the comment
/// above, never the whole population.
///
/// ⭐ Bounded by the retention window rather than by history: on prod this was **266 of
/// 6,496**. Returning the whole population would make the floor cost O(history) per pass.
/// What one prune pass did, or why it did nothing.
#[derive(Debug, Default, Clone)]
pub struct PruneReport {
    /// The floor basis, always reported — including when it refused.
    pub basis: Option<String>,
    pub keep_from_sequence: Option<u64>,
    pub parts_dropped: usize,
    pub bytes_reclaimed: u64,
    /// Set when the pass did not reach the retention call at all.
    pub skipped: Option<String>,
}

impl PruneReport {
    pub fn describe(&self) -> String {
        if let Some(why) = &self.skipped {
            return format!("prune skipped: {why}");
        }
        format!(
            "prune floor={} parts_dropped={} bytes_reclaimed={} basis=[{}]",
            self.keep_from_sequence
                .map(|s| s.to_string())
                .unwrap_or_else(|| "-".into()),
            self.parts_dropped,
            self.bytes_reclaimed,
            self.basis.as_deref().unwrap_or("-")
        )
    }
}

/// ⚠ Above this many constraining executions the pass refuses rather than computing a floor.
///
/// `compute_prune_floor`'s soundness rests on the footprint covering **every** constraining
/// execution — an absent one is read as "constrains nothing", so a truncated footprint
/// produces a floor ABOVE live data. There is therefore no safe way to cap the footprint by
/// truncating it; the only safe response to an implausibly large relevant set is to refuse.
pub const MAX_FOOTPRINT_IDS: usize = 20_000;

/// One prune pass: compute the floor, and drop parts below it only if it is `Computed`.
///
/// Kept separate from `archive_pass` so that the archive path is unchanged by the existence
/// of pruning, and so the safety property — never advance past a live execution — can be
/// tested against injected footprint and prune callbacks rather than a live engine.
///
/// `verified_archived` must be the set this process has *verified* durable and indexed, not
/// merely written.
pub fn prune_pass<FP, PR>(
    cfg: &RetentionConfig,
    set: &CandidateSet,
    now: DateTime<Utc>,
    verified_archived: &std::collections::HashSet<i64>,
    mut footprint: FP,
    mut prune: PR,
) -> PruneReport
where
    FP: FnMut(&[i64]) -> Option<Vec<HotExecution>>,
    PR: FnMut(u64) -> Option<Result<(usize, u64), String>>,
{
    let mut rep = PruneReport::default();
    if !cfg.prune_enabled {
        rep.skipped = Some("NOETL_EHDB_PRUNE_ENABLED is not set".into());
        return rep;
    }
    // Belt and braces: the readiness check already refuses prune-without-archive, but the
    // one call that deletes from the log should not depend on a caller having run it.
    let readiness = cfg.archive_readiness();
    if !readiness.is_ready() {
        rep.skipped = Some(readiness.reason());
        return rep;
    }

    // ⚠⚠ An incomplete candidate list makes the floor unsound, not merely approximate: an
    // execution the listing never returned is invisible to the floor, so its records can lie
    // below it and be deleted. Refuse rather than prune on a partial population.
    if !set.complete {
        rep.skipped = Some(format!(
            "refusing: the candidate listing is incomplete ({} rows, page cap reached) — a              floor computed from a partial population can sit above an execution we never              classified",
            set.candidates.len()
        ));
        return rep;
    }
    let candidates = set.candidates.as_slice();

    let retention = cfg.retention();
    let rows: Vec<(i64, String, Option<DateTime<Utc>>)> = candidates
        .iter()
        .map(|c| (c.execution_id, c.status.clone(), c.completed_at))
        .collect();
    let mut scan = ArchivableScan::default();
    classify_into(&mut scan, &rows, now, retention);

    // ⚠ The footprint covers EVERY candidate, not only the constraining ones. The archived
    // executions do not constrain the floor, but they are what establishes how far it may
    // rise when nothing constrains — ask only for the constraining set and a fully-archived
    // log measures an empty footprint and reclaims nothing.
    let relevant = floor_relevant_ids(&scan, verified_archived, candidates);
    let all_ids: Vec<i64> = candidates.iter().map(|c| c.execution_id).collect();
    if all_ids.len() > MAX_FOOTPRINT_IDS {
        rep.skipped = Some(format!(
            "refusing: {} candidates ({} constraining) exceeds MAX_FOOTPRINT_IDS={} — a \
             truncated footprint would place the floor above live data",
            all_ids.len(),
            relevant.len(),
            MAX_FOOTPRINT_IDS
        ));
        return rep;
    }

    let Some(fp) = footprint(&all_ids) else {
        // ⚠ Absent footprint is NOT "nothing constrains". Refuse.
        rep.skipped = Some("the embedded engine is not open; the footprint is unknown".into());
        return rep;
    };

    let basis = compute_prune_floor(&scan, verified_archived, &fp);
    rep.basis = Some(basis.reason());
    let Some(keep_from) = basis.keep_from_sequence() else {
        match basis {
            FloorBasis::AwaitingBacklog { .. } => {
                crate::metrics::record_ehdb_archive("prune_refused_not_durable")
            }
            _ => crate::metrics::record_ehdb_archive("prune_refused_unindexed"),
        }
        // ⚠ 0 means "no floor was computed", and is set explicitly so a refusal cannot be
        // read off a stale gauge left by an earlier pass that did compute one.
        crate::metrics::ehdb_retention_floor_sequence().set(0);
        return rep;
    };
    rep.keep_from_sequence = Some(keep_from);
    crate::metrics::ehdb_retention_floor_sequence().set(keep_from.min(i64::MAX as u64) as i64);

    match prune(keep_from) {
        Some(Ok((dropped, bytes))) => {
            rep.parts_dropped = dropped;
            rep.bytes_reclaimed = bytes;
            for _ in 0..dropped {
                crate::metrics::record_ehdb_archive("pruned");
            }
            // ⚠⚠ This line was MISSING through the first production prune.
            //
            // `noetl_ehdb_prune_bytes_reclaimed_total` was pinned at 0 and never fed, so
            // after reclaiming **2.30 GiB** on prod the counter still read 0 and the figure
            // had to be measured externally with `df` and `du`. `pruned` counted the parts;
            // nothing counted the bytes, which is the number anyone actually wants.
            //
            // A metric that exists, is pinned, and is never incremented is indistinguishable
            // from a system that reclaimed nothing — the exact failure this tier was built to
            // remove, reproduced by the tier.
            crate::metrics::ehdb_prune_bytes_reclaimed_total().inc_by(bytes);
        }
        Some(Err(e)) => {
            rep.skipped = Some(format!("retention call failed: {e}"));
        }
        None => {
            rep.skipped = Some("the embedded engine is not open".into());
        }
    }
    rep
}

pub fn floor_relevant_ids(
    scan: &ArchivableScan,
    verified_archived: &std::collections::HashSet<i64>,
    all: &[ArchiveCandidate],
) -> Vec<i64> {
    let archivable: std::collections::HashSet<i64> = scan.archivable.iter().copied().collect();
    all.iter()
        .filter(|c| {
            // Group 1: not archivable at all.
            !archivable.contains(&c.execution_id)
                // Group 2: archivable but not yet verified-archived.
                || !verified_archived.contains(&c.execution_id)
        })
        .map(|c| c.execution_id)
        .collect()
}
