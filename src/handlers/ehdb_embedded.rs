//! **The embedded engine, in shadow** (noetl/ai-meta#332 step 5).
//!
//! # What this is, and what it deliberately is not
//!
//! The server opens an `ehdb-l0` event-log engine **in-process** and appends to
//! it in parallel with the authoritative write, then compares. That is the whole
//! of it.
//!
//! It does **not** serve any read or write from the embedded engine. Postgres
//! remains the system of record; nothing here can change what a caller sees.
//! Flipping the serve path is a separate, later checkpoint.
//!
//! # Why shadow first
//!
//! The embedded design rests on the claim that a local engine holds the same
//! events as the authoritative path. That claim is checkable *without depending
//! on it* — append to both, compare, emit a metric. If it disagrees at N=1,
//! where ownership is trivially true and nothing forwards, it would disagree
//! worse everywhere else.
//!
//! # Default OFF
//!
//! `NOETL_EHDB_EMBEDDED` defaults to false, so the deploy is **inert on
//! arrival**: with the flag off, not a byte of this module runs and the server's
//! behaviour is identical to the release before it. That is what makes the
//! rollout reversible by configuration rather than by redeploy.

use std::sync::Arc;

use ehdb_l0::dataset::D1EventLog;
use ehdb_l0::engine::{L0Config, L0Engine};
use ehdb_l0::substrate::{DurableSubstrate, LocalFsSubstrate};

/// Env flag. Absent or anything but `true` means the whole module is inert.
pub const EMBEDDED_ENV: &str = "NOETL_EHDB_EMBEDDED";
/// Where the embedded engine keeps its local root.
pub const EMBEDDED_DIR_ENV: &str = "NOETL_EHDB_EMBEDDED_DIR";
/// Default local root. Under `/data` so it lands on a mounted volume when one
/// exists and fails loudly at open when it does not.
pub const DEFAULT_EMBEDDED_DIR: &str = "/data/ehdb-embedded";

/// Is the embedded shadow armed?
///
/// ⚠ Strict `== "true"`. A flag that accepts `1`, `yes`, `TRUE` and `on` is a
/// flag whose state nobody can read off a manifest with confidence.
pub fn embedded_enabled() -> bool {
    std::env::var(EMBEDDED_ENV)
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// The local root for the embedded engine.
pub fn embedded_dir() -> String {
    std::env::var(EMBEDDED_DIR_ENV).unwrap_or_else(|_| DEFAULT_EMBEDDED_DIR.to_string())
}

/// Open the embedded engine, or `None` when the flag is off.
///
/// ⚠ Returns `None` rather than erroring when disabled, and logs-and-returns
/// `None` when an *enabled* open fails. A shadow that could take the server down
/// on a storage problem would be a liability rather than evidence — the whole
/// point is that it cannot affect serving.
pub fn open_embedded() -> Option<Arc<std::sync::Mutex<L0Engine<D1EventLog>>>> {
    if !embedded_enabled() {
        return None;
    }
    let dir = embedded_dir();
    let substrate: Arc<dyn DurableSubstrate> =
        match LocalFsSubstrate::new(format!("{dir}/substrate")) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                tracing::error!(target: "noetl_server::ehdb_embedded", dir = %dir, error = %e,
                "embedded substrate could not be opened; shadow stays off");
                crate::metrics::record_embedded_shadow("open_failed");
                return None;
            }
        };
    // ⚠ This is where ehdb-l0's FORMAT_VERSION gate fires: an on-disk layout
    // written by a different build refuses here rather than being misread.
    match L0Engine::<D1EventLog>::open(
        L0Config::for_dataset("d1_event_log", format!("{dir}/local")),
        substrate,
    ) {
        Ok(engine) => {
            tracing::info!(target: "noetl_server::ehdb_embedded", dir = %dir,
                "embedded EHDB engine open in SHADOW — not serving reads or writes");
            crate::metrics::record_embedded_shadow("opened");
            Some(Arc::new(std::sync::Mutex::new(engine)))
        }
        Err(e) => {
            tracing::error!(target: "noetl_server::ehdb_embedded", dir = %dir, error = %e,
                "embedded engine open failed; shadow stays off");
            crate::metrics::record_embedded_shadow("open_failed");
            None
        }
    }
}

/// The verdict of one shadow comparison.
///
/// ⚠ `Skipped` is distinct from `Agreed`. A shadow that reports agreement when it
/// compared nothing is the "clean result computed over zero rows" failure this
/// codebase keeps producing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShadowVerdict {
    /// Embedded and authoritative agree on the count for this batch.
    Agreed,
    /// They disagree — the number the whole shadow exists to surface.
    Diverged {
        embedded: usize,
        authoritative: usize,
    },
    /// Nothing was compared (flag off, engine absent, or an empty batch).
    Skipped,
}

/// Compare what the shadow appended against what the authoritative path wrote.
///
/// Pure, so the verdict logic is testable without an engine or a database.
pub fn verdict(embedded: usize, authoritative: usize, compared: bool) -> ShadowVerdict {
    if !compared {
        return ShadowVerdict::Skipped;
    }
    if embedded == authoritative {
        ShadowVerdict::Agreed
    } else {
        ShadowVerdict::Diverged {
            embedded,
            authoritative,
        }
    }
}

impl ShadowVerdict {
    /// The metric label. Closed set, pinned at 0 in `metrics`, so "never
    /// diverged" reads as `0` rather than as an absent series.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Agreed => "agreed",
            Self::Diverged { .. } => "diverged",
            Self::Skipped => "skipped",
        }
    }
}

/// The process-wide embedded engine, opened once on first use.
///
/// `OnceLock` rather than eager construction in `main`: with the flag off this
/// never allocates, so the disabled path costs nothing at startup either.
static EMBEDDED: std::sync::OnceLock<Option<Arc<std::sync::Mutex<L0Engine<D1EventLog>>>>> =
    std::sync::OnceLock::new();

fn engine() -> Option<&'static Arc<std::sync::Mutex<L0Engine<D1EventLog>>>> {
    EMBEDDED.get_or_init(open_embedded).as_ref()
}

/// Append the batch that is about to become authoritative to the embedded
/// engine, and record whether the engine accepted all of it.
///
/// Called from `handlers::event_write::emit_events` — the one chokepoint every
/// server-originated event passes through, on both sides of the CQRS gate
/// (noetl/ai-meta#332). ⚠ Not from `services::internal::project_events`, where
/// it lived until 2026-09-08: that is the materializer, and prod's scheduled
/// traffic is system executions, which `should_publish` excludes by design and
/// which therefore never reach it. The shadow sat armed and unexercised for a
/// full window, reporting the same all-zero series a healthy shadow reports.
///
/// ⚠ The comparison is `appended` vs `rows.len()` — *"did the engine accept
/// every record the log is about to take"*. It is **not** a comparison against
/// what Postgres ultimately stored: at this call site the write has not happened
/// yet. A batch whose subsequent write fails is counted here and not in the log.
///
/// ⚠ Never returns an error and never panics on a poisoned lock. This runs on
/// the live write path; a shadow that can fail a real write is a liability, not
/// evidence.
pub fn shadow_append(rows: &[crate::handlers::event_write::EventRow]) {
    let Some(engine) = engine() else {
        // Flag off, or the open failed and already recorded why.
        return;
    };
    if rows.is_empty() {
        crate::metrics::record_embedded_shadow(ShadowVerdict::Skipped.label());
        return;
    }
    let mut guard = match engine.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let mut appended = 0usize;
    for row in rows {
        // Same construction the command bus uses (`command_bus.rs:361`), so the
        // shadow's records are shaped exactly like the ones the networked path
        // publishes — a shadow that stored a different shape would prove nothing
        // about the embedded engine's fitness to replace it.
        let record = ehdb_l0::dataset::EventRecord::new(
            row.event_id as u64,
            row.execution_id.to_string(),
            String::new(),
            row.to_stream_json().to_string(),
        );
        match guard.append_record(record) {
            Ok(_) => appended += 1,
            Err(e) => {
                tracing::warn!(target: "noetl_server::ehdb_embedded", error = %e,
                    "embedded shadow append failed");
                crate::metrics::record_embedded_shadow("append_failed");
            }
        }
    }
    let v = verdict(appended, rows.len(), true);
    if let ShadowVerdict::Diverged {
        embedded,
        authoritative,
    } = v
    {
        tracing::warn!(target: "noetl_server::ehdb_embedded", embedded, authoritative,
            "embedded shadow DIVERGED from the authoritative write");
    }
    crate::metrics::record_embedded_shadow(v.label());
}

/// One event as the comparator sees it, from either side.
///
/// Only the fields both stores genuinely carry. ⚠ Deliberately NOT a struct with
/// every column: the noetl/ai-meta#325 comparator deserialised five fields and
/// compared three, and reported `match` on executions that differed for weeks.
/// What is compared here is exactly what is listed here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComparableEvent {
    pub event_id: i64,
    pub event_type: String,
}

/// The verdict of comparing one execution's event log across the two stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadVerdict {
    /// Same event ids, same types, same count.
    Agreed { events: usize },
    /// They differ — the number this whole exercise exists to surface.
    Diverged {
        embedded_only: Vec<i64>,
        authoritative_only: Vec<i64>,
        type_mismatches: Vec<i64>,
    },
    /// The embedded engine holds nothing for this execution.
    ///
    /// ⚠ NOT a divergence on a fresh volume. The engine only holds what was
    /// appended since it opened, so an execution older than the engine is
    /// **out of coverage**, not lost. Counting it as divergence would produce a
    /// false alarm; counting it as agreement would produce a false clean — which
    /// is why it is a third outcome rather than folded into either.
    OutOfCoverage,
}

impl ReadVerdict {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Agreed { .. } => "agreed",
            Self::Diverged { .. } => "diverged",
            Self::OutOfCoverage => "out_of_coverage",
        }
    }
}

/// Compare the two event sets. Pure, so the comparator is testable without a
/// database or an engine — and so the positive control below can drive it
/// directly with a known-divergent input.
pub fn compare_reads(
    embedded: &[ComparableEvent],
    authoritative: &[ComparableEvent],
) -> ReadVerdict {
    if embedded.is_empty() && !authoritative.is_empty() {
        return ReadVerdict::OutOfCoverage;
    }
    use std::collections::HashMap;
    let e: HashMap<i64, &ComparableEvent> = embedded.iter().map(|x| (x.event_id, x)).collect();
    let a: HashMap<i64, &ComparableEvent> = authoritative.iter().map(|x| (x.event_id, x)).collect();
    let mut embedded_only: Vec<i64> = e.keys().filter(|k| !a.contains_key(k)).copied().collect();
    let mut authoritative_only: Vec<i64> =
        a.keys().filter(|k| !e.contains_key(k)).copied().collect();
    let mut type_mismatches: Vec<i64> = e
        .iter()
        .filter(|(k, v)| {
            a.get(*k)
                .map(|o| o.event_type != v.event_type)
                .unwrap_or(false)
        })
        .map(|(k, _)| *k)
        .collect();
    if embedded_only.is_empty() && authoritative_only.is_empty() && type_mismatches.is_empty() {
        return ReadVerdict::Agreed { events: a.len() };
    }
    embedded_only.sort_unstable();
    authoritative_only.sort_unstable();
    type_mismatches.sort_unstable();
    ReadVerdict::Diverged {
        embedded_only,
        authoritative_only,
        type_mismatches,
    }
}

/// Read one execution's events back out of the embedded engine.
///
/// `None` when the engine is not open (flag off / open failed) — distinct from
/// `Some(vec![])`, which means the engine IS open and holds nothing for it.
pub fn read_embedded(execution_id: &str) -> Option<Vec<ComparableEvent>> {
    let engine = engine()?;
    let mut guard = match engine.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let recs = guard.read_execution_after(execution_id, 0).ok()?;
    Some(
        recs.iter()
            .map(|r| {
                // ⚠ The id comes from the PAYLOAD, not the `event_id` column.
                // `shadow_append` builds records with `EventRecord::new(..)`, which
                // does not set that column, so it is `None` on every record the
                // shadow wrote. Keying on it would collapse the whole set onto 0 and
                // report a spectacular false divergence. The column is preferred
                // when a producer did set it.
                let v: Option<serde_json::Value> = serde_json::from_str(&r.payload).ok();
                let from_payload = v.as_ref().and_then(|v| {
                    v.get("event_id").and_then(|e| {
                        e.as_i64()
                            .or_else(|| e.as_str().and_then(|s| s.parse::<i64>().ok()))
                    })
                });
                ComparableEvent {
                    event_id: r
                        .event_id
                        .as_ref()
                        .and_then(|c| c.parse::<i64>().ok())
                        .or(from_payload)
                        .unwrap_or(0),
                    event_type: v
                        .as_ref()
                        .and_then(|v| v.get("event_type").and_then(|t| t.as_str()))
                        .unwrap_or_default()
                        .to_string(),
                }
            })
            .collect(),
    )
}

/// The comparator's **positive control**.
///
/// ⚠⚠ The single most important function here. A `diverged=0` result is
/// meaningless unless the comparator can be shown to fire, and this codebase has
/// repeatedly produced clean results from checks that could not. Each case is
/// driven through the real `compare_reads`, and every one must land on its
/// expected verdict or the whole report is refused.
pub fn comparator_controls() -> Vec<(&'static str, bool, String)> {
    let ev = |id: i64, t: &str| ComparableEvent {
        event_id: id,
        event_type: t.to_string(),
    };
    let mut out = Vec::new();

    let same = vec![ev(1, "a"), ev(2, "b")];
    out.push((
        "identical",
        matches!(
            compare_reads(&same, &same),
            ReadVerdict::Agreed { events: 2 }
        ),
        "two identical sets must agree".to_string(),
    ));

    let missing = vec![ev(1, "a")];
    out.push((
        "missing_event",
        matches!(
            compare_reads(&missing, &same),
            ReadVerdict::Diverged { ref authoritative_only, .. } if authoritative_only == &vec![2]
        ),
        "an event only Postgres has must be DETECTED".to_string(),
    ));

    let extra = vec![ev(1, "a"), ev(2, "b"), ev(3, "c")];
    out.push((
        "extra_event",
        matches!(
            compare_reads(&extra, &same),
            ReadVerdict::Diverged { ref embedded_only, .. } if embedded_only == &vec![3]
        ),
        "an event only the engine has must be DETECTED".to_string(),
    ));

    let retyped = vec![ev(1, "a"), ev(2, "DIFFERENT")];
    out.push((
        "type_mismatch",
        matches!(
            compare_reads(&retyped, &same),
            ReadVerdict::Diverged { ref type_mismatches, .. } if type_mismatches == &vec![2]
        ),
        "same ids with a changed type must be DETECTED, not counted as agreement".to_string(),
    ));

    out.push((
        "out_of_coverage_is_not_agreement",
        matches!(compare_reads(&[], &same), ReadVerdict::OutOfCoverage),
        "an empty engine side must be OutOfCoverage, never Agreed".to_string(),
    ));

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ⚠ `cargo test` does NOT serialise tests — two tests mutating the same env
    /// var race, and the failure is intermittent rather than loud. The same
    /// pattern is used in `config::database` and `secrets::broker` for the same
    /// reason.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn agreement_and_divergence_are_distinguished() {
        assert_eq!(verdict(5, 5, true), ShadowVerdict::Agreed);
        assert_eq!(
            verdict(4, 5, true),
            ShadowVerdict::Diverged {
                embedded: 4,
                authoritative: 5
            }
        );
    }

    /// ⚠ The one that matters. A shadow that compared nothing must not report
    /// agreement — that is a clean result computed over zero rows, which reads
    /// exactly like a healthy one.
    #[test]
    fn comparing_nothing_is_skipped_not_agreed() {
        assert_eq!(verdict(0, 0, false), ShadowVerdict::Skipped);
        assert_ne!(verdict(0, 0, false), ShadowVerdict::Agreed);
        // And zero-vs-zero WITH a comparison is genuinely agreement.
        assert_eq!(verdict(0, 0, true), ShadowVerdict::Agreed);
    }

    /// ⚠⚠ Default OFF. This is what makes the deploy inert on arrival; a flag
    /// that defaults on turns a shadow into a change.
    #[test]
    fn the_flag_defaults_off_and_is_strict() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Not set at all.
        std::env::remove_var(EMBEDDED_ENV);
        assert!(!embedded_enabled(), "absent must mean off");
        for v in ["", "1", "yes", "TRUE", "on", "false"] {
            std::env::set_var(EMBEDDED_ENV, v);
            assert!(
                !embedded_enabled(),
                "{v:?} must not arm the shadow — a flag that accepts many \
                 spellings is one nobody can read off a manifest"
            );
        }
        std::env::set_var(EMBEDDED_ENV, "true");
        assert!(embedded_enabled());
        std::env::remove_var(EMBEDDED_ENV);
    }

    /// With the flag off, no engine is opened — so the flag-off path touches no
    /// storage at all.
    ///
    /// ⚠ The directory is pointed at a WRITABLE temp dir on purpose. The first
    /// version left it at the `/data` default, where the open fails anyway — so
    /// `None` came back for the wrong reason and a mutation deleting the flag
    /// check passed. A test that cannot tell "disabled" from "broken" is not
    /// testing the flag.
    #[test]
    fn nothing_opens_when_the_flag_is_off() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var(EMBEDDED_DIR_ENV, tmp.path().to_str().unwrap());

        std::env::remove_var(EMBEDDED_ENV);
        assert!(
            open_embedded().is_none(),
            "flag-off must open nothing — even where opening WOULD succeed"
        );

        // Positive control: the same directory DOES open once the flag is on, so
        // the None above is the flag's doing and not a broken fixture.
        std::env::set_var(EMBEDDED_ENV, "true");
        assert!(
            open_embedded().is_some(),
            "the fixture must be openable, or the assertion above proves nothing"
        );

        std::env::remove_var(EMBEDDED_ENV);
        std::env::remove_var(EMBEDDED_DIR_ENV);
    }

    #[test]
    /// The positive controls must all pass on the real comparator.
    ///
    /// ⚠ This is the guard on the guard. If `compare_reads` is ever weakened so a
    /// planted divergence goes undetected, every `agreed` this endpoint reports
    /// becomes meaningless — and would still read as a clean result.
    #[test]
    fn every_comparator_control_fires() {
        let controls = comparator_controls();
        assert!(
            controls.len() >= 5,
            "expected >=5 controls, found {}",
            controls.len()
        );
        for (name, passed, detail) in &controls {
            assert!(passed, "control {name} did NOT fire: {detail}");
        }
    }

    /// Out-of-coverage must never be reported as agreement.
    ///
    /// On a fresh volume the engine holds nothing for older executions. Folding
    /// that into `Agreed` would manufacture a clean result over zero compared
    /// events — the failure this codebase keeps producing.
    #[test]
    fn an_empty_engine_side_is_not_agreement() {
        let a = vec![ComparableEvent {
            event_id: 1,
            event_type: "x".into(),
        }];
        assert_eq!(compare_reads(&[], &a), ReadVerdict::OutOfCoverage);
        assert_ne!(compare_reads(&[], &a), ReadVerdict::Agreed { events: 0 });
    }

    /// Both stores empty is agreement over zero, and is labelled as such.
    #[test]
    fn both_empty_agrees_on_zero() {
        assert_eq!(compare_reads(&[], &[]), ReadVerdict::Agreed { events: 0 });
    }

    /// Same ids, different types must diverge — the #325 shape in miniature:
    /// comparing only ids would call these equal.
    #[test]
    fn same_ids_different_types_diverge() {
        let a = vec![ComparableEvent {
            event_id: 1,
            event_type: "a".into(),
        }];
        let b = vec![ComparableEvent {
            event_id: 1,
            event_type: "b".into(),
        }];
        match compare_reads(&a, &b) {
            ReadVerdict::Diverged {
                type_mismatches, ..
            } => assert_eq!(type_mismatches, vec![1]),
            other => panic!("expected Diverged, got {other:?}"),
        }
    }

    fn labels_are_a_closed_set() {
        assert_eq!(ShadowVerdict::Agreed.label(), "agreed");
        assert_eq!(ShadowVerdict::Skipped.label(), "skipped");
        assert_eq!(
            ShadowVerdict::Diverged {
                embedded: 1,
                authoritative: 2
            }
            .label(),
            "diverged"
        );
    }
}
