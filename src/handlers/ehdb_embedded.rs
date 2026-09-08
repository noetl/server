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

/// Append the rows Postgres accepted to the embedded engine, and record whether
/// the two agree.
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

#[cfg(test)]
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
