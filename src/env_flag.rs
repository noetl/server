//! **One truthy parse for every `NOETL_*` boolean flag.**
//!
//! # The footgun this removes
//!
//! Boolean env flags were parsed at least four different ways in this binary, and
//! the divergence was invisible: each site looked reasonable on its own.
//! Audited 2026-09-29 across `server`, `ehdb` and `worker` — 33 boolean flags,
//! and the spread was:
//!
//! | semantics | flags |
//! | :-- | :-- |
//! | `1\|true\|yes\|on`, trimmed, case-insensitive | 24 (+8 via four copy-pasted helpers) |
//! | `== "true"` byte-exact — no trim, no lowercase | 2 (`NOETL_EHDB_EMBEDDED`, `NOETL_EHDB_PROJECTION_SERVE_ON_BEHIND`) |
//! | `true\|1\|yes` — same idea, one value short | 1 (`NOETL_SEALED_CREDENTIALS`, worker) |
//!
//! The consequence is not abstract. An operator reading a manifest cannot tell
//! whether a flag is armed without knowing which of four parsers reads it, and
//! that ambiguity is what made the multi-gate chain-store arming
//! ([noetl/ai-meta#357](https://github.com/noetl/ai-meta/issues/357)) so hard to
//! reason about.
//!
//! # ⚠⚠ Why byte-exact `== "true"` was NOT preserved
//!
//! Both strict sites justified themselves the same way: *"a flag that accepts
//! `1`, `yes`, `TRUE` and `on` is a flag whose state nobody can read off a
//! manifest with confidence."* That reasoning is sound about the ACCEPTED SET and
//! wrong about the trimming, and the second half is what bites:
//!
//! ```yaml
//! - name: NOETL_EHDB_EMBEDDED
//!   value: "true "        # trailing space — silently OFF under == "true"
//! - name: NOETL_EHDB_EMBEDDED
//!   value: "True"         # capitalised — silently OFF
//! ```
//!
//! A manifest that reads as armed and is not is strictly worse than a permissive
//! parse, because nothing anywhere reports it. So the strictness was not
//! load-bearing for safety — it was a readability argument that backfires — and
//! both sites now use the shared parse. Their prod values are `"true"`, which
//! resolves identically, so this is a no-op for the running configuration
//! (asserted by `prod_values_resolve_identically`).
//!
//! If a future flag genuinely needs stricter parsing, add it to
//! [`DIVERGENT_BY_DESIGN`] with the reason. That list is the point: divergence is
//! allowed, silent divergence is not.

/// The one accepted set. Compared after `trim()` and `to_ascii_lowercase()`.
///
/// ⚠ Order is not significant, but the CONTENTS are the contract: every
/// `NOETL_*` boolean in this binary answers to exactly these and nothing else.
pub const TRUTHY: [&str; 4] = ["1", "true", "yes", "on"];

/// Values that are explicitly false rather than merely unrecognised.
///
/// Not used for the decision — anything outside [`TRUTHY`] is false — but
/// published so an operator can see that `false`/`0`/`off`/`no` are *intended*
/// off rather than accidents, and so [`explain`] can say which it is.
pub const FALSY: [&str; 4] = ["0", "false", "no", "off"];

/// Flags that deliberately do NOT use the shared parse, with the reason.
///
/// ⚠ Empty today, and that is the desired state. A flag belongs here only when
/// stricter or looser parsing is load-bearing for **correctness**, not for
/// readability — the readability argument is what produced the divergence this
/// module removes. [`tests::no_silent_divergence`] fails the build if a boolean
/// flag parses its own way without appearing here.
pub const DIVERGENT_BY_DESIGN: &[(&str, &str)] = &[];

/// `NOETL_*` variables that are **not booleans**, listed so the guard's silence
/// about them is a decision rather than an oversight.
///
/// ⚠⚠ This list exists because two of them were miscounted as booleans during the
/// audit. `NOETL_AUTH_VERIFY_SIGNATURE` reads `off|shadow|enforce` and prod sets
/// it to `shadow` — a boolean parse would have silently collapsed a three-state
/// security control to `false`, and the audit's first pass did exactly that on
/// paper. Naming them here is cheaper than re-deriving the distinction each time.
pub const NON_BOOLEAN: &[(&str, &str)] = &[
    (
        "NOETL_AUTH_VERIFY_SIGNATURE",
        "tri-state: off | shadow | enforce. Prod = 'shadow'. Must NOT become a bool.",
    ),
    (
        "NOETL_CHAIN_SOURCE",
        "enum: postgres | chain. Only 'chain'/'chain_store' selects the chain store.",
    ),
    (
        "NOETL_EHDB_READ_CONSISTENCY",
        "enum parsed with NOETL_EHDB_MAX_STALENESS_MS by ReadConsistency::parse.",
    ),
    (
        "NOETL_COMMAND_BUS",
        "enum: ehdb | shadow. No default — an unknown value refuses startup.",
    ),
    (
        "NOETL_EVENT_BUS",
        "enum: ehdb | shadow. No default — an unknown value refuses startup.",
    ),
];

/// Is this flag armed?
///
/// Absent, empty, or any value outside [`TRUTHY`] is **false** — the fail-safe
/// direction: a typo must never arm a gate.
pub fn truthy(name: &str) -> bool {
    match std::env::var(name) {
        Ok(v) => is_truthy(&v),
        Err(_) => false,
    }
}

/// As [`truthy`], but an ABSENT variable takes `default`.
///
/// ⚠ A present-but-unrecognised value is still `false`, not `default`. Those are
/// different situations and collapsing them is how `NOETL_X=flase` would silently
/// inherit a `true` default.
pub fn truthy_or(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(v) => is_truthy(&v),
        Err(_) => default,
    }
}

/// The parse itself, on an already-read value. Separated so it is testable
/// without touching process env — which matters because `cargo test` does NOT
/// serialise tests within a binary.
pub fn is_truthy(raw: &str) -> bool {
    let v = raw.trim().to_ascii_lowercase();
    TRUTHY.contains(&v.as_str())
}

/// How a value was read, for logs and for the armed-state report.
///
/// ⭐ `Unrecognised` is a distinct arm on purpose. It resolves to `false` like
/// `ExplicitlyOff`, but an operator needs to know the difference: one is a
/// decision, the other is a typo that looks like a decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagReading {
    /// Variable not set at all.
    Absent,
    /// Set to something in [`TRUTHY`].
    On,
    /// Set to something in [`FALSY`].
    ExplicitlyOff,
    /// Set to something in neither list — reads as OFF, and that is worth saying.
    Unrecognised,
}

impl FlagReading {
    pub fn armed(self) -> bool {
        matches!(self, Self::On)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::On => "on",
            Self::ExplicitlyOff => "off",
            Self::Unrecognised => "unrecognised",
        }
    }
    /// Every label, for pinning a metric at 0 — absence is the default for a
    /// labelled series.
    pub const ALL_LABELS: [&'static str; 4] = ["absent", "on", "off", "unrecognised"];
}

/// Read a flag and say HOW it read, not just whether it armed.
pub fn explain(name: &str) -> FlagReading {
    match std::env::var(name) {
        Err(_) => FlagReading::Absent,
        Ok(raw) => {
            let v = raw.trim().to_ascii_lowercase();
            if TRUTHY.contains(&v.as_str()) {
                FlagReading::On
            } else if FALSY.contains(&v.as_str()) {
                FlagReading::ExplicitlyOff
            } else {
                FlagReading::Unrecognised
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ⭐⭐ Multi-gate arming, made readable.
// ---------------------------------------------------------------------------

/// One gate in a multi-gate feature.
#[derive(Debug, Clone, Copy)]
pub struct Gate {
    pub name: &'static str,
    pub reading: FlagReading,
    /// True when this gate is REQUIRED for the feature to arm at all.
    pub required: bool,
}

/// The resolved state of a multi-gate feature.
///
/// # Why this type exists
///
/// The chain store needs THREE gates on (`NOETL_CHAIN_ADVANCE`,
/// `NOETL_CHAIN_SOURCE=chain`, `NOETL_CHAIN_POPULATE`), and the dangerous
/// configuration is not "all off" — it is **some on**. Two advance gates on with
/// the populator off would point a reader at a store nothing fills, which makes
/// `decide()` report `Empty` on a running execution
/// ([noetl/ai-meta#357](https://github.com/noetl/ai-meta/issues/357)).
///
/// The resolver already refuses that combination. What was missing is that the
/// refusal was **invisible**: a partially-armed deployment looked exactly like an
/// unarmed one, because both end up not using the feature. [`Arming::Partial`]
/// is the distinction, and it is logged and countable.
#[derive(Debug, Clone)]
pub enum Arming {
    /// No gate is on. The steady state, and silent by design.
    Off,
    /// Every required gate is on.
    Armed,
    /// ⚠ Some gates on, some off. The feature does NOT run, and somebody almost
    /// certainly meant it to.
    Partial {
        on: Vec<&'static str>,
        missing: Vec<&'static str>,
    },
}

impl Arming {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Armed => "armed",
            Self::Partial { .. } => "partial",
        }
    }
    /// Every label, for pinning at 0.
    pub const ALL_LABELS: [&'static str; 3] = ["off", "armed", "partial"];

    pub fn is_armed(&self) -> bool {
        matches!(self, Self::Armed)
    }
}

/// Resolve a feature's arming from its gates.
///
/// A gate that is present-but-unrecognised counts as OFF (fail-safe) and is
/// reported in `missing`, so `NOETL_CHAIN_POPULATE=ture` shows up as a missing
/// gate rather than as an absent one.
pub fn resolve_arming(gates: &[Gate]) -> Arming {
    let required: Vec<&Gate> = gates.iter().filter(|g| g.required).collect();
    let on: Vec<&'static str> = required
        .iter()
        .filter(|g| g.reading.armed())
        .map(|g| g.name)
        .collect();
    let missing: Vec<&'static str> = required
        .iter()
        .filter(|g| !g.reading.armed())
        .map(|g| g.name)
        .collect();
    if on.is_empty() {
        Arming::Off
    } else if missing.is_empty() {
        Arming::Armed
    } else {
        Arming::Partial { on, missing }
    }
}

/// Log a feature's arming once, at the level its state deserves.
///
/// ⚠ `Partial` is a `warn`, and `Off` is silent. A partially-armed feature is the
/// one state an operator needs told; logging `Off` on every boot would bury it.
pub fn log_arming(feature: &str, arming: &Arming) {
    match arming {
        Arming::Off => {}
        Arming::Armed => tracing::info!(
            target: "noetl_server::env_flag", feature, state = "armed",
            "feature ARMED — every required gate is on"
        ),
        Arming::Partial { on, missing } => tracing::warn!(
            target: "noetl_server::env_flag", feature, state = "partial",
            gates_on = ?on, gates_missing = ?missing,
            "feature PARTIALLY armed and therefore NOT running — some gates are \
             on and some are off, which looks identical to unarmed unless \
             somebody reads this line"
        ),
    }
}

/// The chain store's three gates, resolved.
///
/// ⚠ `NOETL_CHAIN_SOURCE` is an ENUM, not a boolean — only the value `chain` /
/// `chain_store` selects the store, so it is not read through [`truthy`]. It is
/// treated as a gate that is "on" when it names the chain store, which is the
/// only reading that matters for arming.
pub fn chain_store_arming() -> Arming {
    let source_selects_chain = matches!(
        std::env::var("NOETL_CHAIN_SOURCE")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "chain" | "chain_store"
    );
    resolve_arming(&[
        Gate {
            name: "NOETL_CHAIN_ADVANCE",
            reading: explain("NOETL_CHAIN_ADVANCE"),
            required: true,
        },
        Gate {
            name: "NOETL_CHAIN_SOURCE=chain",
            reading: if source_selects_chain {
                FlagReading::On
            } else {
                explain("NOETL_CHAIN_SOURCE")
            },
            required: true,
        },
        Gate {
            name: "NOETL_CHAIN_POPULATE",
            reading: explain("NOETL_CHAIN_POPULATE"),
            required: true,
        },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ⚠ `cargo test` does NOT serialise tests within a binary — an `EnvGuard`
    /// SAFETY note in this program once claimed it did, and the tests raced.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn the_accepted_set_is_exactly_four_values() {
        assert_eq!(TRUTHY.len(), 4);
        for v in ["1", "true", "yes", "on"] {
            assert!(is_truthy(v), "{v:?} must arm");
        }
    }

    /// ⭐ Trimming and case-folding are the part that was missing, and the part
    /// that actually bit — a YAML value with a trailing space read as OFF.
    #[test]
    fn whitespace_and_case_do_not_change_the_answer() {
        for v in ["true ", " true", "  TRUE  ", "True", "YES", "On", "\tON\n"] {
            assert!(
                is_truthy(v),
                "{v:?} must arm — this is the manifest footgun"
            );
        }
    }

    #[test]
    fn everything_else_is_off_including_near_misses() {
        for v in [
            "", " ", "false", "0", "no", "off", "FALSE", "ture", "treu", "yess", "onn", "2", "-1",
            "enabled", "y", "t",
        ] {
            assert!(!is_truthy(v), "{v:?} must NOT arm");
        }
    }

    /// ⚠⚠ **The prod-equivalence proof.** Every value prod actually sets for the
    /// flags whose parse changed must resolve the same before and after.
    ///
    /// Measured against the live cluster 2026-09-29:
    ///   NOETL_EHDB_EMBEDDED = "true"  (both server workloads)
    ///   NOETL_EHDB_PROJECTION_SERVE_ON_BEHIND = "true"  (embedded STS)
    #[test]
    fn prod_values_resolve_identically() {
        // The OLD semantics, reproduced here so the comparison is real rather
        // than asserted.
        fn old_strict(v: &str) -> bool {
            v == "true"
        }
        // Prod's only value for the two changed flags.
        {
            let v = "true";
            assert_eq!(
                old_strict(v),
                is_truthy(v),
                "prod value {v:?} changes meaning — this change is NOT a no-op for \
                 the running configuration"
            );
        }
        // ⭐ And the negative control: the two DO differ, on exactly the inputs
        // this change is about. Without this the test above would pass for a
        // helper that had not changed anything at all.
        let differs: Vec<&str> = ["true ", "True", "1", "yes", "on"]
            .into_iter()
            .filter(|v| old_strict(v) != is_truthy(v))
            .collect();
        assert_eq!(
            differs.len(),
            5,
            "expected all five to differ from the strict parse; got {differs:?}. \
             If none differ, the shared parse is not actually more permissive and \
             this whole change is inert."
        );
    }

    #[test]
    fn absent_is_false_and_truthy_or_takes_the_default() {
        let name = "NOETL_TEST_DEFINITELY_UNSET_FLAG_9e3a";
        assert!(!truthy(name));
        assert!(truthy_or(name, true), "absent takes the default");
        assert!(!truthy_or(name, false));
    }

    /// ⚠⚠ **A present-but-unrecognised value must NOT inherit the default.**
    ///
    /// Found by a surviving mutant: `truthy_or` documented this and nothing tested
    /// it, so a version that returned `is_truthy(v) || default` passed the whole
    /// suite. Under that version `NOETL_X=flase` on a flag whose default is `true`
    /// would arm — a typo silently taking the ON path, which is the precise
    /// failure mode the fail-safe direction exists to prevent.
    ///
    /// Tested on `is_truthy` + the documented rule rather than through process env,
    /// because `cargo test` does not serialise tests and a `set_var` here would
    /// race every other test in this binary.
    #[test]
    fn an_unrecognised_value_is_false_even_when_the_default_is_true() {
        // The rule `truthy_or` implements: PRESENT delegates to `is_truthy`, and
        // only ABSENT consults the default.
        for v in ["flase", "ture", "", "2", "enabled", "y"] {
            assert!(
                !is_truthy(v),
                "{v:?} must be false regardless of any default"
            );
        }
        // ⚠⚠ And the real function, called with a real env var.
        //
        // The first version of this test defined a local `resolve()` with the same
        // shape and asserted against THAT — so mutating `truthy_or` itself changed
        // nothing and the mutant survived a second time. A test that
        // reimplements the logic it checks is decorative. Call the function.
        //
        // The var name is unique to this test, so no other test can collide with
        // it; the lock is held because `set_var` is process-global.
        let name = "NOETL_TEST_TRUTHY_OR_UNRECOGNISED_4f7b";
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var(name, "flase") };
        let got = truthy_or(name, true);
        unsafe { std::env::remove_var(name) };
        assert!(
            !got,
            "truthy_or returned true for a PRESENT unrecognised value with a true \
             default. A typo must never inherit an ON default — only an ABSENT \
             variable consults it."
        );

        unsafe { std::env::set_var(name, "yes") };
        let got_on = truthy_or(name, false);
        unsafe { std::env::remove_var(name) };
        assert!(
            got_on,
            "a recognised truthy value arms regardless of the default"
        );
    }

    #[test]
    fn a_reading_distinguishes_a_typo_from_a_decision() {
        // Both resolve false; an operator needs to know which.
        assert!(!FlagReading::ExplicitlyOff.armed());
        assert!(!FlagReading::Unrecognised.armed());
        assert_ne!(
            FlagReading::ExplicitlyOff.label(),
            FlagReading::Unrecognised.label(),
            "`false` and `flase` must not report the same state"
        );
        for r in [
            FlagReading::Absent,
            FlagReading::On,
            FlagReading::ExplicitlyOff,
            FlagReading::Unrecognised,
        ] {
            assert!(FlagReading::ALL_LABELS.contains(&r.label()));
        }
    }

    // -----------------------------------------------------------------------
    // Multi-gate arming.
    // -----------------------------------------------------------------------

    fn gate(name: &'static str, on: bool) -> Gate {
        Gate {
            name,
            reading: if on {
                FlagReading::On
            } else {
                FlagReading::Absent
            },
            required: true,
        }
    }

    #[test]
    fn no_gates_on_is_off_and_all_on_is_armed() {
        assert_eq!(
            resolve_arming(&[gate("A", false), gate("B", false)]).label(),
            "off"
        );
        assert_eq!(
            resolve_arming(&[gate("A", true), gate("B", true)]).label(),
            "armed"
        );
    }

    /// ⭐⭐ The state that used to be invisible.
    #[test]
    fn some_gates_on_is_partial_and_names_what_is_missing() {
        match resolve_arming(&[gate("A", true), gate("B", false), gate("C", false)]) {
            Arming::Partial { on, missing } => {
                assert_eq!(on, vec!["A"]);
                assert_eq!(missing, vec!["B", "C"], "must NAME the missing gates");
            }
            other => panic!("expected Partial, got {other:?}"),
        }
    }

    /// ⚠ A typo'd gate must count as MISSING, not as absent-and-fine.
    #[test]
    fn an_unrecognised_gate_value_counts_as_missing() {
        let gates = [
            gate("A", true),
            Gate {
                name: "B",
                reading: FlagReading::Unrecognised,
                required: true,
            },
        ];
        match resolve_arming(&gates) {
            Arming::Partial { missing, .. } => assert_eq!(missing, vec!["B"]),
            other => panic!("`NOETL_B=ture` must read as a MISSING gate, got {other:?}"),
        }
    }

    #[test]
    fn arming_labels_are_enumerated_for_pinning() {
        for a in [
            Arming::Off,
            Arming::Armed,
            Arming::Partial {
                on: vec![],
                missing: vec![],
            },
        ] {
            assert!(Arming::ALL_LABELS.contains(&a.label()));
        }
        assert_eq!(Arming::ALL_LABELS.len(), 3);
    }

    /// ⚠⚠ **The build-breaking guard.** No boolean flag may parse its own way
    /// unless it is listed in [`DIVERGENT_BY_DESIGN`].
    ///
    /// Scans the non-test source of this crate for the parse idioms that used to
    /// diverge. Divergence is allowed; SILENT divergence is not.
    #[test]
    fn no_silent_divergence() {
        use std::path::Path;
        let mut offenders: Vec<String> = Vec::new();
        let mut scanned = 0usize;
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                if p.extension().and_then(|s| s.to_str()) != Some("rs") {
                    continue;
                }
                let Ok(src) = std::fs::read_to_string(&p) else {
                    continue;
                };
                // This module legitimately contains the idioms it forbids.
                if p.ends_with("env_flag.rs") {
                    continue;
                }
                let non_test = src.split("#[cfg(test)]").next().unwrap_or("");
                scanned += 1;
                let rel = p
                    .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                    .unwrap_or(&p)
                    .display()
                    .to_string();
                let lines: Vec<&str> = non_test.lines().collect();
                for (i, line) in lines.iter().enumerate() {
                    let l = line.trim();
                    if l.starts_with("//") || l.starts_with("///") {
                        continue;
                    }
                    let idiom = l.contains("== \"true\"")
                        || l.contains("\"1\" | \"true\"")
                        || l.contains("\"true\" | \"1\"")
                        || (l.contains("v == \"1\"") && l.contains("||"));
                    if !idiom {
                        continue;
                    }
                    // ⚠⚠ Only flag it when the surrounding lines actually read an
                    // ENV VAR. Without this the guard also flagged
                    // `q.get("dry_run").map(|v| v == "true")` — a URL query
                    // parameter, which has nothing to do with env flags. A check
                    // that cries wolf trains people to skim past the one finding
                    // that is real, so precision here is not politeness.
                    let lo = i.saturating_sub(6);
                    let hi = (i + 4).min(lines.len());
                    let ctx = lines[lo..hi].join("\n");
                    if !ctx.contains("env::var") {
                        continue;
                    }
                    // Documented non-booleans are allowed to parse their own way.
                    if NON_BOOLEAN.iter().any(|(v, _)| ctx.contains(v)) {
                        continue;
                    }
                    offenders.push(format!("{rel}:{}  {l}", i + 1));
                }
            }
        }
        assert!(
            scanned > 50,
            "scanned only {scanned} files — the walk is broken, and a guard that \
             measures nothing passes. This is the denominator."
        );
        let allowed: Vec<&str> = DIVERGENT_BY_DESIGN.iter().map(|(v, _)| *v).collect();
        let real: Vec<&String> = offenders
            .iter()
            .filter(|o| !allowed.iter().any(|a| o.contains(a)))
            .collect();
        assert!(
            real.is_empty(),
            "{} boolean-flag parse site(s) bypass env_flag::truthy and are not \
             listed in DIVERGENT_BY_DESIGN (scanned {scanned} files):\n{}\n\n\
             Either route them through `env_flag::truthy`, or add the flag to \
             DIVERGENT_BY_DESIGN with the reason the difference is load-bearing.",
            real.len(),
            real.iter()
                .map(|s| format!("  {s}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    /// ⚠⚠ **The prod-arming proof.** With prod's ACTUAL environment — none of the
    /// three chain gates set — the chain store must resolve `Off`, exactly as it
    /// does today, and `decide()` must therefore never be reached through it.
    ///
    /// Measured against the live cluster 2026-09-29: all 4 chain env vars unset
    /// across all 6 prod workloads, and the ops IaC does not set them either.
    #[test]
    fn prod_environment_resolves_the_chain_store_to_off() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for v in [
            "NOETL_CHAIN_ADVANCE",
            "NOETL_CHAIN_SOURCE",
            "NOETL_CHAIN_POPULATE",
        ] {
            unsafe { std::env::remove_var(v) };
        }
        let a = chain_store_arming();
        assert_eq!(
            a.label(),
            "off",
            "prod sets none of the gates, so the resolved state must be Off — not \
             Partial and certainly not Armed"
        );
        assert!(!a.is_armed());

        // ⭐ NEGATIVE CONTROL: the resolver can produce the other two states, so an
        // `off` above is a reading and not a constant.
        unsafe { std::env::set_var("NOETL_CHAIN_ADVANCE", "true") };
        let partial = chain_store_arming();
        unsafe { std::env::remove_var("NOETL_CHAIN_ADVANCE") };
        assert_eq!(
            partial.label(),
            "partial",
            "one gate on must read Partial — if this is also `off`, the check above \
             proves nothing"
        );
    }
}
