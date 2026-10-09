//! B2 — the age-based seal trigger, [noetl/ai-meta#460](https://github.com/noetl/ai-meta/issues/460).
//!
//! ⚠⚠ The trigger has TWO halves and ehdb's own doc comment on `seal_aged_parts` says what
//! happens when only one ships:
//!
//! > `PartWriter::should_seal` is only consulted on append, and the shard the age trigger
//! > protects is by definition the one taking no appends. Setting `seal_max_age` without
//! > driving this on a timer leaves the trigger **inert on exactly the shard it was added
//! > for** — the flag would be present, the config would look correct, and nothing would
//! > ever fire.
//!
//! Before this change the server had NEITHER half: `seal_max_age` was never configured and
//! `seal_aged_parts` had no caller outside ehdb's own tests. So the whole mechanism existed
//! and was unreachable — the defect class this repo keeps paying for.
//!
//! These tests exist so it cannot regress to one half.

use noetl_server::handlers::ehdb_embedded::seal_max_age;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// ⚠ `cargo test` does NOT serialise tests, so two tests mutating the same env var race and
/// the failure is intermittent rather than loud. Poisoning is tolerated because a panic in
/// one test must not cascade into unrelated failures.
fn serialised() -> std::sync::MutexGuard<'static, ()> {
    match ENV_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

fn with_var<T>(v: Option<&str>, f: impl FnOnce() -> T) -> T {
    let _g = serialised();
    let key = "NOETL_EHDB_SEAL_MAX_AGE_SECS";
    let prev = std::env::var(key).ok();
    unsafe {
        match v {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
    let out = f();
    unsafe {
        match prev {
            Some(p) => std::env::set_var(key, p),
            None => std::env::remove_var(key),
        }
    }
    out
}

#[test]
fn unset_leaves_the_trigger_off() {
    assert_eq!(
        with_var(None, seal_max_age),
        None,
        "absent must mean OFF — today's prod behaviour, where a part seals only on \
         record/byte limits"
    );
}

#[test]
fn a_duration_is_parsed() {
    assert_eq!(
        with_var(Some("900"), seal_max_age).map(|d| d.as_secs()),
        Some(900)
    );
    assert_eq!(
        with_var(Some("  120 "), seal_max_age).map(|d| d.as_secs()),
        Some(120),
        "surrounding whitespace is a configuration typo, not a different value"
    );
}

/// ⚠⚠ Zero is REFUSED, not clamped.
///
/// A zero age seals on every tick, producing one part per tick and growing the manifest
/// without bound — the shape that filled the cmdbus PVC, where snapshot size tracked part
/// count while snapshot count tracked write count. Clamping to 1 s would be nearly as bad
/// and would hide the misconfiguration.
#[test]
fn zero_is_refused_rather_than_clamped() {
    assert_eq!(
        with_var(Some("0"), seal_max_age),
        None,
        "0 must leave the trigger OFF, loudly, not seal on every tick"
    );
}

#[test]
fn garbage_leaves_the_trigger_off_rather_than_guessing() {
    for bad in ["", "abc", "-5", "1.5", "900s"] {
        assert_eq!(
            with_var(Some(bad), seal_max_age),
            None,
            "{bad:?} must not be coerced into a duration"
        );
    }
}

/// ⚠⚠⚠ The structural guard: both halves, in the same build.
///
/// A config value with no timer is the inert-trigger failure ehdb's doc warns about. A timer
/// with no config value is harmless but pointless. Either alone compiles, tests clean, and
/// reports a healthy configuration.
#[test]
fn the_trigger_has_both_halves() {
    let embedded = include_str!("../src/handlers/ehdb_embedded.rs");
    let main_rs = include_str!("../src/main.rs");

    assert!(
        embedded.contains("with_seal_max_age"),
        "half 1 missing: the engine is never configured with a max age"
    );
    assert!(
        embedded.contains("seal_aged_parts()"),
        "half 2a missing: nothing calls the sealer"
    );
    assert!(
        embedded.contains("pub fn spawn_age_seal_task"),
        "half 2b missing: there is no timer task to call it from"
    );
    assert!(
        main_rs.contains("spawn_age_seal_task("),
        "⚠⚠ THE TIMER IS NEVER SPAWNED. `seal_max_age` would be configured and the trigger \
         would never fire on an idle shard — which is the only shard it exists for. This is \
         the exact failure ehdb's doc comment on `seal_aged_parts` describes, and it reads \
         as a correct configuration from every other angle."
    );
}

/// The exposure gauges must be pinned, and must not depend on the trigger being on.
///
/// ⚠ The unsealed tail is an RF=1 exposure whether or not anything is configured to bound
/// it. A gauge that only appeared once the fix was enabled would leave the problem invisible
/// in exactly the configuration that has it — prod today.
#[test]
fn the_exposure_gauges_are_pinned_independently_of_the_trigger() {
    noetl_server::metrics::init_age_seal_series();
    let text = noetl_server::metrics::gather_text().expect("render /metrics");
    for g in [
        "noetl_ehdb_oldest_unsealed_age_seconds",
        "noetl_ehdb_manifest_parts",
        "noetl_ehdb_age_sealed_total",
    ] {
        assert!(
            text.lines().any(|l| l.starts_with(g)),
            "{g} is ABSENT. Absence is not zero: a scrape that cannot see this cannot tell \
             an unbounded unsealed tail from a healthy one"
        );
    }
    // And the pin is honest about the trigger being off: 0 sealed, not a fabricated sample.
    assert!(
        text.contains("noetl_ehdb_age_sealed_total 0"),
        "the age-seal counter must read a true 0 before anything fires"
    );
}

/// ⭐ The counterweight gauge exists on purpose.
///
/// B2 makes parts smaller and more numerous, and manifest cost grows with part count.
/// Shipping the trigger without a parts gauge would trade an invisible durability risk for
/// an invisible capacity one — which is not an improvement.
#[test]
fn the_parts_counterweight_is_published() {
    noetl_server::metrics::init_age_seal_series();
    noetl_server::metrics::ehdb_manifest_parts().set(162);
    assert_eq!(noetl_server::metrics::ehdb_manifest_parts().get(), 162);
    let text = noetl_server::metrics::gather_text().unwrap();
    assert!(text.contains("noetl_ehdb_manifest_parts 162"));
}
