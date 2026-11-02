//! The three `ehdb-*` pins must name the SAME tag.
//!
//! ⚠⚠ `ehdb-feed` depends on `ehdb-l0`, and `ehdb-core` carries the `EhdbError`/`Result`
//! that `DurableSubstrate`'s signatures are written in. Pinning them at different tags puts
//! **two copies of those types in one dependency graph** — `D1EventLog` from one and
//! `Dataset` from the other.
//!
//! The lucky outcome is a compile error (it surfaced once as E0277 in `command_bus.rs`). The
//! unlucky one is a pair of tags whose types happen to line up, and then **two engine
//! versions open the same on-disk layout** — which is a corruption vector, not an
//! inconvenience. `FORMAT_VERSION` is the belt to this braces, but it only refuses a layout
//! it cannot read; it does not notice two readers that both can.
//!
//! ## ⚠ What this guard is and is NOT worth
//!
//! Measured, not assumed. Skewing one pin in `Cargo.toml` and running the suite produces
//! **29 compile errors**, including `E0277` on `PublishRouter<D: Dataset>` — two `Dataset`
//! types in one graph. **So the compiler is the real gate for a genuine skew, and it is
//! louder than this test.**
//!
//! What this adds is narrower, and worth stating so nobody over-trusts it:
//!
//! - it fails on a *recorded* disagreement immediately and legibly, instead of through 29
//!   trait-bound errors pointing into a vendored dependency's `publish.rs`;
//! - it asserts the **lock's** three entries resolve to one commit, which is a property of
//!   the recorded state rather than of the compile;
//! - it puts the invariant somewhere that fails, because it had been recorded only as a
//!   comment in `Cargo.toml`, and a comment cannot fail a build.
//!
//! ⚠⚠ It does NOT protect against the dangerous case the comment describes — two tags whose
//! types happen to line up, so the compile succeeds and two engine versions open one
//! on-disk layout. Nothing here can see that; `FORMAT_VERSION` is the only thing between
//! that and corruption, and it only refuses a layout it cannot read.

/// The pins, read out of the real manifest.
fn ehdb_pins() -> Vec<(String, String)> {
    const MANIFEST: &str = include_str!("../Cargo.toml");
    let mut out = Vec::new();
    for line in MANIFEST.lines() {
        let line = line.trim();
        if !line.starts_with("ehdb-") {
            continue;
        }
        let Some((name, rest)) = line.split_once('=') else { continue };
        // tag = "vX.Y.Z"
        let Some(i) = rest.find("tag = \"") else { continue };
        let after = &rest[i + 7..];
        let Some(j) = after.find('"') else { continue };
        out.push((name.trim().to_string(), after[..j].to_string()));
    }
    out
}

#[test]
fn all_three_ehdb_pins_name_the_same_tag() {
    let pins = ehdb_pins();

    // ⚠ Print the denominator: a clean pass over zero pins is not a pass. If the parse ever
    // stops matching the manifest's shape, this is what says so instead of reporting success.
    assert!(
        pins.len() >= 3,
        "parsed only {} ehdb pin(s) from Cargo.toml — the parse no longer matches the \
         manifest, so agreement cannot be checked: {pins:?}",
        pins.len()
    );

    let first = &pins[0].1;
    let disagreeing: Vec<_> = pins.iter().filter(|(_, t)| t != first).collect();
    assert!(
        disagreeing.is_empty(),
        "the ehdb pins DISAGREE: {pins:?}\n\n\
         `ehdb-feed` depends on `ehdb-l0` and `ehdb-core` carries EhdbError/Result, so \
         different tags put two copies of those types in one graph. The lucky outcome is a \
         compile error; the unlucky one is two engine versions opening the same on-disk \
         layout."
    );
}

/// The lock must agree with the manifest, and all three must resolve to ONE commit.
///
/// ⚠ The manifest names a tag; the lock records the commit that tag pointed at. Three pins
/// on the same tag that resolved at different times could in principle carry different
/// commits if the tag were ever moved — so the commit is the thing to compare, not the tag.
#[test]
fn the_lock_resolves_all_three_to_one_commit() {
    const LOCK: &str = include_str!("../Cargo.lock");
    let mut revs: Vec<(String, String)> = Vec::new();
    let mut name: Option<String> = None;
    for line in LOCK.lines() {
        if let Some(n) = line.strip_prefix("name = \"") {
            name = n.strip_suffix('"').map(|s| s.to_string());
        }
        if let Some(src) = line.strip_prefix("source = \"") {
            if let Some(n) = &name {
                if n.starts_with("ehdb-") {
                    let rev = src.rsplit('#').next().unwrap_or("").trim_end_matches('"');
                    revs.push((n.clone(), rev.to_string()));
                }
            }
        }
    }
    assert!(
        revs.len() >= 3,
        "parsed only {} ehdb entries from Cargo.lock — the parse no longer matches: {revs:?}",
        revs.len()
    );
    let first = &revs[0].1;
    assert!(
        revs.iter().all(|(_, r)| r == first),
        "the lock resolves the ehdb crates to DIFFERENT commits, so the build contains more \
         than one ehdb: {revs:?}"
    );
    assert!(
        first.len() >= 7,
        "the resolved commit looks implausible ({first:?}) — the parse probably captured the \
         wrong field, which would make the agreement check above vacuous"
    );
}
