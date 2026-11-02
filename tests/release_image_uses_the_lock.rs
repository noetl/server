//! **The release image must build against the committed lockfiles**
//! (noetl/ai-meta#494; the server twin of noetl/worker's guard of the same name).
//!
//! Without `--locked`, every cargo invocation in the image resolves the
//! dependency graph against whatever the registry index offers at build time,
//! so the shipped binary's graph is not the one CI tested. noetl/worker learned
//! this the hard way on 2026-10-10: a floating `noetl-tools` range resolved to a
//! newer release in the image than in CI, and the release failed with E0308
//! after a 47-minute build.
//!
//! Server has TWO lockfiles: the workspace's, and `plugins/orchestrate`'s (a
//! standalone wasm32 cdylib excluded from the workspace). Both are honoured.
//! Adding `--locked` here found the plugin's committed lock stale — it had never
//! been updated for orchestrate-core's `hex` dependency, so every release had
//! been silently re-resolving it.

const DOCKERFILE: &str = include_str!("../Dockerfile");

fn cargo_lines() -> Vec<&'static str> {
    DOCKERFILE
        .lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .filter(|l| l.contains("cargo build") || l.contains("cargo chef cook"))
        .collect()
}

#[test]
fn every_cargo_invocation_in_the_release_image_is_locked() {
    assert!(
        DOCKERFILE.len() > 500,
        "Dockerfile read as {} bytes — the checks below would be vacuous",
        DOCKERFILE.len()
    );
    let lines = cargo_lines();
    // The denominator: chef cook, the server binary, and the wasm plug-in.
    assert!(
        lines.len() >= 3,
        "expected cook + server build + wasm build; found {}: {lines:?}",
        lines.len()
    );
    for line in &lines {
        assert!(
            line.contains("--locked"),
            "⚠ this cargo invocation in the release image does not pass --locked, so the \
             shipped artifact's dependency graph is whatever the registry offers at build \
             time rather than the graph CI tested:\n  {line}"
        );
    }
}

#[test]
fn the_cook_stage_has_a_lock_to_honour() {
    // The builder stage copies only recipe.json before cooking; `--locked`
    // there needs the workspace lock copied explicitly.
    let cook_at = DOCKERFILE
        .find("cargo chef cook")
        .expect("the cook stage exists");
    assert!(
        DOCKERFILE[..cook_at].contains("COPY Cargo.lock"),
        "the cook stage runs with --locked but nothing copies Cargo.lock before it"
    );
}

#[test]
fn both_lockfiles_are_tracked_and_not_dockerignored() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for lock in ["Cargo.lock", "plugins/orchestrate/Cargo.lock"] {
        assert!(root.join(lock).is_file(), "{lock} is missing");
    }
    let ignore = std::fs::read_to_string(root.join(".dockerignore")).unwrap_or_default();
    for pat in ["Cargo.lock", "*.lock", "/Cargo.lock", "**/Cargo.lock"] {
        assert!(
            !ignore.lines().map(str::trim).any(|l| l == pat),
            ".dockerignore excludes {pat}, so the build context has no lock"
        );
    }
}
