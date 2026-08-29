//! The catalog's log (noetl/ai-meta#311 step 2, `docs/rfc/ehdb-catalog-relation.md`).
//!
//! `noetl.catalog` is **not event-sourced**: `POST /api/catalog/register`
//! performs a direct `INSERT` and there is no emit site anywhere in the catalog
//! service or handler. A relation is by definition a fold of a log, so the
//! catalog relation had nothing to fold. This module is the producer that gives
//! it one.
//!
//! # Where the records go, and why not the event log
//!
//! Into [`StoreTier::Catalog`](../../../worker) — its own store. A catalog record
//! carries no `execution_id` and has no row in `noetl.event`, so appending one to
//! the event-log tier would make the cross-store parity comparator report
//! `extra_event` — a tier record with no authoritative row — which **pages**.
//! Giving the new log its own store is what keeps it from setting off the alarm
//! that guards the old one.
//!
//! # Dual-write, not a cutover
//!
//! The `INSERT` into `noetl.catalog` is untouched and remains the source every
//! read resolves from. This only *also* records the registration, so the fold
//! can be built and compared before anything depends on it.

use serde::Serialize;

/// `NOETL_CATALOG_LOG` — whether registrations are also recorded to the tier.
pub const MODE_ENV: &str = "NOETL_CATALOG_LOG";

/// The tier path segment. Must match the worker's `StoreTier::Catalog::as_str`.
pub const TIER: &str = "catalog";

/// The record shape's own version, so a reader can tell a v1 record from a
/// future shape without inferring it from which keys happen to be present.
pub const RECORD_VERSION: i64 = 1;

/// Event type recorded for a registration.
pub const EVENT_REGISTERED: &str = "catalog.registered";

/// How long an append may take. Short: registration must not wait on a mirror.
const APPEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// Outcomes, pinned as metric labels.
pub const OUTCOMES: [&str; 5] = [
    "recorded",
    "disabled",
    "unconfigured",
    "append_failed",
    "serialise_failed",
];

/// Whether registrations are mirrored into the catalog log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Not recorded. Default, so this lands inert.
    Off,
    /// Recorded to the tier. Nothing reads it yet — the fold is verified against
    /// `noetl.catalog`, and catalog reads still resolve from Postgres.
    Shadow,
}

pub const MODES: [&str; 2] = ["off", "shadow"];

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shadow => "shadow",
        }
    }
    pub fn enabled(self) -> bool {
        matches!(self, Self::Shadow)
    }
}

pub fn mode() -> Mode {
    parse_mode(std::env::var(MODE_ENV).ok().as_deref())
}

/// The parse, without the environment — testable without `set_var`, which would
/// race (`cargo test` does not serialise tests).
///
/// Unrecognised resolves to `Off`. A typo must not start writing to a tier.
pub fn parse_mode(raw: Option<&str>) -> Mode {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("shadow") => Mode::Shadow,
        _ => Mode::Off,
    }
}

/// One catalog-log record.
#[derive(Debug, Clone, Serialize)]
pub struct CatalogRecord {
    pub record_version: i64,
    pub event_type: &'static str,
    pub catalog_id: String,
    pub path: String,
    pub kind: String,
    pub version: i16,
    /// Pins the exact registered bytes. The fold compares on this rather than on
    /// the content itself, so a comparison is cheap and a mismatch is exact.
    pub content_sha256: String,
    pub content_bytes: usize,
    /// The registered content. The catalog log is the *only* event-sourced record
    /// of catalog content, so unlike the per-execution snapshot it carries the
    /// bytes — there is one record per registration, not one per run.
    pub content: String,
    pub registered_at: String,
}

/// Build a record. Pure, so the shape is asserted by a test rather than by a
/// running server.
pub fn build(
    catalog_id: i64,
    path: &str,
    kind: &str,
    version: i16,
    content: &str,
    registered_at: String,
) -> CatalogRecord {
    CatalogRecord {
        record_version: RECORD_VERSION,
        event_type: EVENT_REGISTERED,
        catalog_id: catalog_id.to_string(),
        path: path.to_string(),
        kind: kind.to_string(),
        version,
        content_sha256: crate::handlers::catalog_snapshot::sha256_hex(content.as_bytes()),
        content_bytes: content.len(),
        content: content.to_string(),
        registered_at,
    }
}

/// Record one registration into the catalog log.
///
/// # Fail-safe
///
/// Returns `()`. Registration has already committed to `noetl.catalog` by the
/// time this runs; failing the request now would report a failure for work that
/// succeeded, and the caller would re-register, creating a second version of an
/// identical playbook.
pub async fn record_registration(
    catalog_id: i64,
    path: &str,
    kind: &str,
    version: i16,
    content: &str,
) {
    let mode = mode();
    if !mode.enabled() {
        crate::metrics::record_catalog_log(mode.as_str(), "disabled");
        return;
    }
    let Some(base) = std::env::var(crate::handlers::ehdb::WORKER_QUERY_URL_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        // Asked to record with nowhere to record to. A misconfiguration, not a
        // quiet no-op: without this the tier stays empty while the mode says it
        // is on, and the fold would later report "no records" for a cause two
        // hops away.
        crate::metrics::record_catalog_log(mode.as_str(), "unconfigured");
        tracing::warn!(
            target: "noetl_server::catalog_log",
            "{MODE_ENV}=shadow but {} is unset — registration was NOT recorded",
            crate::handlers::ehdb::WORKER_QUERY_URL_ENV
        );
        return;
    };

    let rec = build(
        catalog_id,
        path,
        kind,
        version,
        content,
        chrono::Utc::now().to_rfc3339(),
    );
    let Ok(payload) = serde_json::to_string(&rec) else {
        crate::metrics::record_catalog_log(mode.as_str(), "serialise_failed");
        return;
    };

    // The relay keys records by an opaque partition string. `catalog_id` is the
    // natural one: unique per record, and it makes a keyed read return exactly
    // the registration asked for.
    let body = serde_json::json!({
        "execution_id": catalog_id.to_string(),
        "records": [payload],
    });
    let url = format!("{}/ehdb/tiers/{TIER}", base.trim_end_matches('/'));
    let resp = reqwest::Client::new()
        .post(&url)
        .json(&body)
        .timeout(APPEND_TIMEOUT)
        .send()
        .await;

    match resp {
        Ok(r) if r.status().is_success() => {
            // ⚠ A 2xx is NOT sufficient. The tier relay answers 200 with an
            // outcome in the BODY, so a refused append arrives as a successful
            // HTTP response. Trusting the status alone lost three registrations
            // in kind while reporting `recorded` for every one of them.
            let body = r.text().await.unwrap_or_default();
            let outcome = classify_append_reply(&body);
            crate::metrics::record_catalog_log(mode.as_str(), outcome);
            if outcome != "recorded" {
                tracing::warn!(
                    target: "noetl_server::catalog_log",
                    path, version, body = %body.chars().take(200).collect::<String>(),
                    "catalog log append did not land; the catalog row is committed regardless"
                );
            }
        }
        Ok(r) => {
            crate::metrics::record_catalog_log(mode.as_str(), "append_failed");
            tracing::warn!(
                target: "noetl_server::catalog_log",
                status = %r.status(), path, version,
                "catalog log append refused; the catalog row is committed regardless"
            );
        }
        Err(e) => {
            crate::metrics::record_catalog_log(mode.as_str(), "append_failed");
            tracing::warn!(
                target: "noetl_server::catalog_log",
                error = %e, path, version,
                "catalog log append failed; the catalog row is committed regardless"
            );
        }
    }
}

/// Emit a **backfill** registration for an existing catalog row.
///
/// ⚠⚠ This is NOT `register`. It deliberately does **not** touch
/// `noetl.catalog`: the row already exists, and putting it through the normal
/// registration path would run `MAX(version)+1` and create a *second* version of
/// every one of the 2,518 existing entries — doubling the catalog to describe it.
///
/// It emits the same `catalog.registered` record the live path emits, carrying
/// the row's **existing** `catalog_id`, `path`, `kind`, `version` and `content`,
/// so the relation folds identically whether an entry arrived live or by
/// backfill. `backfilled: true` rides along so provenance stays honest — the
/// event says when the log learned about the registration, not when the
/// registration happened.
pub async fn record_backfill(
    catalog_id: i64,
    path: &str,
    kind: &str,
    version: i16,
    content: &str,
    archived: bool,
) -> bool {
    let mode = mode();
    if !mode.enabled() {
        return false;
    }
    let Some(base) = relay_base() else {
        return false;
    };

    let mut rec = serde_json::to_value(build(
        catalog_id,
        path,
        kind,
        version,
        content,
        chrono::Utc::now().to_rfc3339(),
    ))
    .unwrap_or_default();
    if let Some(o) = rec.as_object_mut() {
        o.insert("backfilled".into(), serde_json::json!(true));
    }

    let mut ok = append(&base, catalog_id, &rec.to_string()).await;
    // Liveness is derived from the sequence, so an archived row needs its
    // archive event too — otherwise the fold reports a retired entry as live and
    // `get_latest` resolves to a version the source would not serve.
    if ok && archived {
        let tomb = serde_json::json!({
            "record_version": RECORD_VERSION,
            "event_type": EVENT_ARCHIVED,
            "catalog_id": catalog_id.to_string(),
            "path": path,
            "version": version,
            "backfilled": true,
            "registered_at": chrono::Utc::now().to_rfc3339(),
        });
        ok = append(&base, catalog_id, &tomb.to_string()).await;
    }
    crate::metrics::record_catalog_log(
        mode.as_str(),
        if ok { "recorded" } else { "append_failed" },
    );
    ok
}

/// `catalog.archived` — emitted by the backfill for a retired row.
pub const EVENT_ARCHIVED: &str = "catalog.archived";

/// The relay base, or `None` when unconfigured.
fn relay_base() -> Option<String> {
    std::env::var(crate::handlers::ehdb::WORKER_QUERY_URL_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// POST one record and report whether it actually landed.
async fn append(base: &str, key: i64, payload: &str) -> bool {
    let body = serde_json::json!({ "execution_id": key.to_string(), "records": [payload] });
    let url = format!("{}/ehdb/tiers/{TIER}", base.trim_end_matches('/'));
    match reqwest::Client::new()
        .post(&url)
        .json(&body)
        .timeout(APPEND_TIMEOUT)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            classify_append_reply(&r.text().await.unwrap_or_default()) == "recorded"
        }
        _ => false,
    }
}

/// Classify a tier-append reply body.
///
/// The relay answers **200 with the outcome in the body**, so the HTTP status
/// says only that the request was understood. This reads what actually happened.
/// Split out so the rule is asserted by a test rather than only by a live relay.
pub fn classify_append_reply(body: &str) -> &'static str {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        // A non-JSON 200 is not a success we can confirm. Refusing to call it
        // one is the whole point: an unconfirmable append must not be counted
        // as a landed record.
        return "append_failed";
    };
    if let Some(o) = v.get("outcome").and_then(|o| o.as_str()) {
        if o != "ok" {
            return "append_failed";
        }
    }
    if let Some(n) = v.get("appended").and_then(|n| n.as_i64()) {
        if n < 1 {
            return "append_failed";
        }
    }
    "recorded"
}

/// One folded catalog entry, keyed by `(path, version)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldedEntry {
    pub catalog_id: String,
    pub kind: String,
    pub content_sha256: String,
}

/// Fold catalog-log records into the relation's shape.
///
/// Pure, and the whole point of step 2: this is the function that turns a log
/// into a relation. Later records for the same `(path, version)` win, which
/// matters only for a duplicate append — `(path, version)` is immutable in the
/// source, so two records for one key must agree, and the verifier is what says
/// whether they do.
///
/// Returns a `BTreeMap` so the ordering is deterministic and two folds of the
/// same log compare equal.
pub fn fold_records(
    records: &[serde_json::Value],
) -> std::collections::BTreeMap<(String, i64), FoldedEntry> {
    let mut out = std::collections::BTreeMap::new();
    for r in records {
        // A tier record may arrive as `{payload: "<json>"}` or inline.
        let p = match r.get("payload").and_then(|p| p.as_str()) {
            Some(s) => match serde_json::from_str::<serde_json::Value>(s) {
                Ok(v) => v,
                Err(_) => continue,
            },
            None => r.clone(),
        };
        if p.get("event_type").and_then(|v| v.as_str()) != Some(EVENT_REGISTERED) {
            continue;
        }
        let (Some(path), Some(version)) = (
            p.get("path").and_then(|v| v.as_str()),
            p.get("version").and_then(|v| v.as_i64()),
        ) else {
            continue;
        };
        let Some(digest) = p.get("content_sha256").and_then(|v| v.as_str()) else {
            continue;
        };
        out.insert(
            (path.to_string(), version),
            FoldedEntry {
                catalog_id: p
                    .get("catalog_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                kind: p
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                content_sha256: digest.to_string(),
            },
        );
    }
    out
}

/// Does the fold agree with the source?
///
/// **Both conditions, and the first is the one that matters.** A fold that
/// covered nothing has zero mismatches, so `mismatched == 0` alone would report
/// an empty log as agreeing with the catalog — the vacuous pass this codebase
/// keeps refusing.
pub fn fold_agrees(compared: usize, mismatched: usize) -> bool {
    compared > 0 && mismatched == 0
}

/// `GET /api/catalog-log/verify?limit=N` — fold the catalog log and compare it
/// against `noetl.catalog`.
///
/// Read-only and independent of the mode, so evidence can be gathered while
/// catalog reads still resolve entirely from Postgres.
///
/// Goes through [`read_and_fold`] rather than reading the tier itself: this
/// endpoint originally did its own single unpaged read and answered
/// `unavailable` once the log outgrew a 1 MiB frame — a verifier that stops
/// working at exactly the coverage it exists to confirm.
pub async fn verify_endpoint(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> crate::error::AppResult<axum::Json<serde_json::Value>> {
    let limit: usize = q
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(FOLD_READ_CAP)
        .clamp(1, FOLD_READ_CAP);

    let folded = match read_and_fold(limit).await {
        Ok(r) => r,
        Err(e) => {
            return Ok(axum::Json(serde_json::json!({
                "action": "catalog.log.verify",
                "outcome": "unavailable",
                "error": e,
            })))
        }
    };

    let pool = state.pools.cluster();
    let (mut compared, mut mismatched, mut missing_in_source) = (0usize, 0usize, 0usize);
    let mut mismatches: Vec<serde_json::Value> = Vec::new();
    for e in folded.entries() {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT COALESCE(content, ''), kind FROM noetl.catalog WHERE path = $1 AND version = $2",
        )
        .bind(&e.path)
        .bind(e.version as i16)
        .fetch_optional(pool)
        .await?;
        let Some((content, kind)) = row else {
            missing_in_source += 1;
            continue;
        };
        compared += 1;
        let src = crate::handlers::catalog_snapshot::sha256_hex(content.as_bytes());
        if src != e.content_sha256 || kind != e.kind {
            mismatched += 1;
            if mismatches.len() < 10 {
                mismatches.push(serde_json::json!({
                    "path": e.path, "version": e.version,
                    "source_sha256": &src[..16.min(src.len())],
                    "folded_sha256": &e.content_sha256[..16.min(e.content_sha256.len())],
                    "source_kind": kind, "folded_kind": e.kind,
                }));
            }
        }
    }

    Ok(axum::Json(serde_json::json!({
        "action": "catalog.log.verify",
        "outcome": "ok",
        "mode": mode().as_str(),
        "records_read": folded.records_seen,
        "records_applied": folded.records_applied,
        "folded_entries": folded.len(),
        "compared": compared,
        "mismatched": mismatched,
        "missing_in_source": missing_in_source,
        "agrees": fold_agrees(compared, mismatched) && missing_in_source == 0,
        "mismatches": mismatches,
    })))
}

pub async fn read_and_fold(
    limit: usize,
) -> Result<crate::handlers::catalog_relation::CatalogRelation, String> {
    let base = relay_base().ok_or_else(|| {
        format!("{} is unset", crate::handlers::ehdb::WORKER_QUERY_URL_ENV)
    })?;
    let client = reqwest::Client::new();
    let mut all: Vec<serde_json::Value> = Vec::new();
    let mut after: u64 = 0;
    let mut page = FOLD_PAGE;

    // ⚠⚠ Paged AND adaptive, and both are forced by the store rather than
    // chosen. A tier-service frame is capped at 1 MiB; catalog records carry
    // playbook content, and a single entry can be 267 KB. So neither a whole-log
    // scan nor a fixed page size works — a page of 25 measured 1,311,308 bytes
    // because a few large entries landed together. On a frame-cap refusal the
    // page halves and retries; a page that cannot be made to fit is reported,
    // not silently skipped, because a fold missing records is a WRONG answer and
    // not a smaller one.
    for _ in 0..MAX_FOLD_PAGES {
        // The FIRST page must omit `after`: the store rejects `after=0` with
        // "stream sequence must be greater than zero" — starting from the
        // beginning is the ABSENCE of a cursor, not a zero one.
        let url = if after == 0 {
            format!("{}/ehdb/tiers/{TIER}?limit={page}", base.trim_end_matches('/'))
        } else {
            format!(
                "{}/ehdb/tiers/{TIER}?limit={page}&after={after}",
                base.trim_end_matches('/')
            )
        };
        let resp = client
            .get(&url)
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            let text = resp.text().await.unwrap_or_default();
            if text.contains("exceeds the") && text.contains("cap") && page > 1 {
                page = (page / 2).max(1);
                continue;
            }
            return Err(format!(
                "tier read failed at page size {page} after={after}: {}",
                text.chars().take(240).collect::<String>()
            ));
        }

        let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        let recs: Vec<serde_json::Value> = body
            .get("records")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        if recs.is_empty() {
            break;
        }
        let next = recs
            .iter()
            .filter_map(|r| r.get("global_sequence").and_then(|v| v.as_u64()))
            .max()
            .unwrap_or(0);
        let short = recs.len() < page;
        all.extend(recs);
        // A cursor that did not advance would re-read the same page forever.
        if short || next <= after {
            break;
        }
        after = next;
        if all.len() >= limit {
            break;
        }
    }
    Ok(crate::handlers::catalog_relation::CatalogRelation::fold(&all))
}

/// Upper bound on records a single fold will accumulate.
///
/// Generous, because a partial fold is a WRONG answer rather than a smaller one;
/// bounded anyway so a caller cannot ask for unbounded work.
pub const FOLD_READ_CAP: usize = 50_000;

/// Records per fold page. Small on purpose: one catalog entry can be 267 KB, and
/// the tier-service frame cap is 1 MiB.
const FOLD_PAGE: usize = 16;

/// Page walk bound, so a stalled cursor cannot spin.
const MAX_FOLD_PAGES: usize = 4000;

/// `POST /api/catalog-log/backfill` — bring the catalog log to full coverage.
///
/// # Why this exists
///
/// 2,518 catalog rows predate the log, so the relation's coverage is a strict
/// subset of the catalog — and a fold-served read on a missing entry would be
/// **wrong**, not merely stale, while `list_by_kind` would silently
/// under-report. Full coverage is the gate for serving.
///
/// # Idempotent by diffing, not by hoping
///
/// It folds the log **first** and emits only rows the fold is missing or
/// disagrees with. A second run emits nothing. That is a stronger property than
/// "re-running is harmless": the log is append-only, so an emit-everything
/// backfill would keep appending duplicates forever even though the folded
/// relation stayed the same.
///
/// # Cursor
///
/// `after_catalog_id` makes it resumable, so a 2,518-row backfill is a sequence
/// of bounded calls rather than one request that must not fail.
pub async fn backfill_endpoint(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> crate::error::AppResult<axum::Json<serde_json::Value>> {
    let limit: i64 = q
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(200)
        .clamp(1, 1000);
    let after: i64 = q
        .get("after_catalog_id")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let dry_run = q.get("dry_run").map(|v| v == "true").unwrap_or(false);

    if !mode().enabled() {
        return Ok(axum::Json(serde_json::json!({
            "action": "catalog.log.backfill",
            "outcome": "disabled",
            "hint": format!("set {MODE_ENV}=shadow"),
        })));
    }

    // Fold what the log already has, so this run only emits the difference.
    let existing = match read_and_fold(2000).await {
        Ok(r) => r,
        Err(e) => {
            return Ok(axum::Json(serde_json::json!({
                "action": "catalog.log.backfill",
                "outcome": "unavailable",
                "error": e,
            })))
        }
    };

    let archived_sel = if crate::db::queries::catalog::archived_column_present() {
        "archived_at IS NOT NULL"
    } else {
        "false"
    };
    let sql = format!(
        "SELECT catalog_id, path, kind, version, COALESCE(content, ''), {archived_sel} \
         FROM noetl.catalog WHERE catalog_id > $1 ORDER BY catalog_id ASC LIMIT $2"
    );
    let rows: Vec<(i64, String, String, i16, String, bool)> = sqlx::query_as(&sql)
        .bind(after)
        .bind(limit)
        .fetch_all(state.pools.cluster())
        .await?;

    let scanned = rows.len();
    let (mut emitted, mut already, mut failed) = (0usize, 0usize, 0usize);
    let mut next = after;
    for (cid, path, kind, version, content, archived) in rows {
        next = cid;
        let digest = crate::handlers::catalog_snapshot::sha256_hex(content.as_bytes());
        // Covered means the fold has this key with the SAME digest AND the same
        // liveness. Digest-only would let an archived row read as covered while
        // the fold still served it as live.
        let covered = existing
            .get(&path, version as i32)
            .is_some_and(|e| e.content_sha256 == digest && e.archived == archived);
        if covered {
            already += 1;
            continue;
        }
        if dry_run {
            emitted += 1;
            continue;
        }
        if record_backfill(cid, &path, &kind, version, &content, archived).await {
            emitted += 1;
        } else {
            failed += 1;
        }
    }

    Ok(axum::Json(serde_json::json!({
        "action": "catalog.log.backfill",
        "outcome": "ok",
        "dry_run": dry_run,
        "scanned": scanned,
        "already_covered": already,
        "emitted": emitted,
        "failed": failed,
        "next_after_catalog_id": next,
        // `scanned < limit` is the only honest "done" signal; a caller that
        // stopped on `emitted == 0` would stop at the first fully-covered page.
        "done": (scanned as i64) < limit,
    })))
}

/// `GET /api/catalog-log/coverage` — is the relation complete enough to serve?
///
/// The blocker for the read cutover is coverage, so it gets its own answer
/// rather than being inferred from the verifier's counts.
pub async fn coverage_endpoint(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
) -> crate::error::AppResult<axum::Json<serde_json::Value>> {
    let rel = match read_and_fold(FOLD_READ_CAP).await {
        Ok(r) => r,
        Err(e) => {
            return Ok(axum::Json(serde_json::json!({
                "action": "catalog.log.coverage",
                "outcome": "unavailable",
                "error": e,
            })))
        }
    };
    let (source_rows,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM noetl.catalog")
        .fetch_one(state.pools.cluster())
        .await?;
    let folded = rel.len() as i64;
    Ok(axum::Json(serde_json::json!({
        "action": "catalog.log.coverage",
        "outcome": "ok",
        "source_rows": source_rows,
        "folded_entries": folded,
        "fold_missing": (source_rows - folded).max(0),
        "records_seen": rel.records_seen,
        "records_applied": rel.records_applied,
        // Serving is permissible only at FULL coverage. Under-reporting is the
        // failure mode nobody notices, so this is stated rather than left to a
        // reader comparing two numbers.
        "full_coverage": folded >= source_rows && source_rows > 0
            && rel.records_seen < FOLD_READ_CAP,
        // Full coverage computed from a truncated read is not full coverage.
        "fold_read_capped": rel.records_seen >= FOLD_READ_CAP,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_is_off_unless_asked_for() {
        assert_eq!(parse_mode(None), Mode::Off);
        assert_eq!(parse_mode(Some("")), Mode::Off);
        assert!(!Mode::Off.enabled());
    }

    #[test]
    fn a_typo_does_not_start_writing_to_a_tier() {
        for junk in ["shadwo", "SHADOW!", "on", "true", "1", "tier", "serve"] {
            assert_eq!(parse_mode(Some(junk)), Mode::Off, "{junk} must not enable writes");
        }
        assert_eq!(parse_mode(Some(" Shadow ")), Mode::Shadow);
    }

    /// The record pins the registered CONTENT, not just its version.
    #[test]
    fn the_record_pins_content_not_just_the_version() {
        let a = build(1, "p", "playbook", 3, "steps: [a]", "t".into());
        let b = build(2, "p", "playbook", 3, "steps: [b]", "t".into());
        assert_eq!(a.version, b.version);
        assert_ne!(
            a.content_sha256, b.content_sha256,
            "two different registrations at the same version must not pin identically"
        );
        assert_eq!(a.content, "steps: [a]", "the log must carry the bytes");
    }

    /// The digest is over the content, and matches the snapshot module's — the
    /// two are compared against each other by the verifier, so a divergence in
    /// hashing would read as a divergence in data.
    #[test]
    fn the_digest_agrees_with_the_execution_snapshots() {
        let c = "apiVersion: noetl.io/v2\nkind: Playbook\n";
        let r = build(1, "p", "playbook", 1, c, "t".into());
        assert_eq!(
            r.content_sha256,
            crate::handlers::catalog_snapshot::sha256_hex(c.as_bytes()),
            "the catalog log and the execution snapshot must hash content the same \
             way, or comparing them reports data divergence for a hashing difference"
        );
    }

    fn rec(path: &str, version: i64, sha: &str) -> serde_json::Value {
        serde_json::json!({
            "record_version": 1, "event_type": EVENT_REGISTERED,
            "catalog_id": "7", "path": path, "kind": "playbook",
            "version": version, "content_sha256": sha,
        })
    }

    /// ⭐ The fold turns a log into the relation's shape, keyed by (path, version).
    #[test]
    fn the_fold_keys_by_path_and_version() {
        let f = fold_records(&[rec("a", 1, "d1"), rec("a", 2, "d2"), rec("b", 1, "d3")]);
        assert_eq!(f.len(), 3);
        assert_eq!(f[&("a".into(), 2)].content_sha256, "d2");
        assert_eq!(f[&("b".into(), 1)].content_sha256, "d3");
    }

    /// Records arriving wrapped in a tier `payload` string fold identically.
    #[test]
    fn a_wrapped_payload_folds_the_same_as_an_inline_one() {
        let inline = rec("a", 1, "d1");
        let wrapped = serde_json::json!({ "payload": inline.to_string() });
        assert_eq!(fold_records(&[inline]), fold_records(&[wrapped]));
    }

    /// Foreign record types are ignored rather than folded into nonsense.
    #[test]
    fn records_of_another_type_are_skipped() {
        let mut other = rec("a", 1, "d1");
        other["event_type"] = serde_json::json!("catalog.archived");
        assert!(
            fold_records(&[other]).is_empty(),
            "an unrecognised record type must not be folded as a registration"
        );
    }

    /// A malformed record is skipped, not folded with defaults.
    #[test]
    fn a_record_missing_its_key_is_skipped() {
        let mut bad = rec("a", 1, "d1");
        bad.as_object_mut().unwrap().remove("version");
        assert!(fold_records(&[bad]).is_empty());
        let mut nodigest = rec("a", 1, "d1");
        nodigest.as_object_mut().unwrap().remove("content_sha256");
        assert!(
            fold_records(&[nodigest]).is_empty(),
            "a record with no digest must not fold to an empty digest that then \
             compares as a mismatch against every source row"
        );
    }

    /// The verifier must be able to FAIL, including on the shape most like success.
    #[test]
    fn a_fold_that_compared_nothing_is_not_agreement() {
        assert!(
            !fold_agrees(0, 0),
            "an empty log has zero mismatches; calling that agreement is the \
             vacuous pass"
        );
        assert!(!fold_agrees(0, 3));
        assert!(!fold_agrees(9, 1));
        assert!(fold_agrees(9, 0));
    }

    /// ⚠ A 200 with a failure body must NOT count as a landed record.
    ///
    /// This is the regression that lost three registrations in kind: the relay
    /// answers 200 with the outcome in the body, and trusting the status alone
    /// reported every one of them as `recorded`.
    #[test]
    fn a_two_hundred_with_a_failure_body_is_not_a_landed_record() {
        assert_eq!(classify_append_reply(r#"{"outcome":"ok","appended":1}"#), "recorded");
        assert_eq!(
            classify_append_reply(r#"{"outcome":"unconfigured"}"#),
            "append_failed",
            "an unconfigured mirror reported as recorded is a silently lost record"
        );
        assert_eq!(classify_append_reply(r#"{"outcome":"ok","appended":0}"#), "append_failed");
        assert_eq!(
            classify_append_reply("not json at all"),
            "append_failed",
            "an unconfirmable append must not be counted as a landed record"
        );
    }

    #[test]
    fn every_label_is_pinned() {
        for m in [Mode::Off, Mode::Shadow] {
            let expect = match m {
                Mode::Off => "off",
                Mode::Shadow => "shadow",
            };
            assert_eq!(m.as_str(), expect);
            assert!(MODES.contains(&m.as_str()));
        }
        assert_eq!(MODES.len(), 2);
        assert_eq!(OUTCOMES.len(), 5);
    }

    #[test]
    fn the_tier_segment_matches_the_workers_store_tier_name() {
        // If these drift, appends 400 with "unknown tier" and the log stays
        // silently empty while the mode says it is on.
        assert_eq!(TIER, "catalog");
    }
}
