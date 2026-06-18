//! Seed built-in **system plug-ins** into the module registry on boot
//! (noetl/ai-meta#108 slice 3).
//!
//! The `system/orchestrate` plug-in (and any future built-in system plug-in) is
//! compiled from this repo's `plugins/` tree to `wasm32-unknown-unknown` and
//! baked into the server image. On startup the server reads those `.wasm` files
//! and upserts them into `noetl.plugin_module` so the worker pool's
//! `HttpPluginSource` can fetch them by `(path, version)` + digest — without an
//! out-of-band operator `POST`.
//!
//! **Hot-reload by re-seed.** Every boot re-seeds at version 1; the digest is
//! recomputed from the current bytes, so a new image with new plug-in bytes
//! publishes a new digest at `system/<name>@1`. The worker resolves the digest
//! from the registry on every dispatch, so it reloads on the next claim — the
//! same digest-keyed hot-replace path proven for the materialiser
//! (noetl/ai-meta#105). Version bumps are reserved for deliberate ABI breaks.
//!
//! The server seeds its **own** built-ins by calling `plugin_module::upsert`
//! directly (in-process, it holds the pool) — not through the token-gated
//! `/api/internal/plugins` HTTP surface, which is for *external* registration.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::db::{queries::plugin_module, DbPool};
use crate::error::AppResult;

/// Env var naming the directory the server reads built-in system plug-in
/// `.wasm` files from at boot. Defaults to the image's baked plug-in dir.
pub const PLUGIN_DIR_ENV: &str = "NOETL_SYSTEM_PLUGIN_DIR";
const DEFAULT_PLUGIN_DIR: &str = "/opt/noetl/plugins";
/// Built-in system plug-ins seed at this version; digest-keyed hot-reload
/// (re-seed with new bytes) makes a version bump unnecessary for updates.
const SEED_VERSION: i32 = 1;

/// One plug-in to register, resolved from a `.wasm` file. Pure data — the scan
/// half is DB-free and unit-testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedEntry {
    /// Catalog path, `system/<file-stem>` (e.g. `orchestrate.wasm` →
    /// `system/orchestrate`).
    pub path: String,
    pub version: i32,
    /// SHA-256 hex digest of `bytes` — must match the register endpoint's
    /// `hex::encode(Sha256::digest(..))` so worker cache keys agree.
    pub digest: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

/// Scan a directory for `*.wasm` files and resolve each into a [`SeedEntry`].
/// Pure (no DB): reads files, derives `system/<stem>`, computes the digest.
/// A missing directory yields an empty list (local dev without the baked image);
/// an unreadable individual file is skipped with a warning, not fatal.
pub fn scan_system_plugins(dir: &Path) -> Vec<SeedEntry> {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!(
                dir = %dir.display(),
                error = %e,
                "no system plug-in dir; nothing to seed"
            );
            return Vec::new();
        }
    };

    let mut entries: Vec<SeedEntry> = Vec::new();
    for dirent in read.flatten() {
        let file = dirent.path();
        if file.extension().and_then(|e| e.to_str()) != Some("wasm") {
            continue;
        }
        let Some(stem) = file.file_stem().and_then(|s| s.to_str()) else {
            tracing::warn!(file = %file.display(), "skipping plug-in with non-UTF8 name");
            continue;
        };
        let bytes = match std::fs::read(&file) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(file = %file.display(), error = %e, "skipping unreadable plug-in");
                continue;
            }
        };
        let digest = hex::encode(Sha256::digest(&bytes));
        entries.push(SeedEntry {
            path: format!("system/{stem}"),
            version: SEED_VERSION,
            digest,
            media_type: "application/wasm".to_string(),
            bytes,
        });
    }
    // Deterministic order (directory iteration order is unspecified) so logs and
    // any future digest-of-digests are stable.
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

/// The directory the server seeds from: `$NOETL_SYSTEM_PLUGIN_DIR` or the
/// image's baked default.
pub fn plugin_dir() -> PathBuf {
    std::env::var(PLUGIN_DIR_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_PLUGIN_DIR))
}

/// Seed every built-in system plug-in found under [`plugin_dir`] into
/// `noetl.plugin_module`. Returns the number seeded. Idempotent (upsert);
/// safe to run on every boot.
pub async fn seed_system_plugins(pool: &DbPool) -> AppResult<usize> {
    let dir = plugin_dir();
    let entries = scan_system_plugins(&dir);
    if entries.is_empty() {
        tracing::debug!(dir = %dir.display(), "no system plug-ins to seed");
        return Ok(0);
    }
    for e in &entries {
        plugin_module::upsert(pool, &e.path, e.version, &e.digest, &e.media_type, &e.bytes).await?;
        tracing::info!(
            plugin_path = %e.path,
            version = e.version,
            digest = %e.digest,
            bytes = e.bytes.len(),
            "seeded system plug-in"
        );
    }
    tracing::info!(count = entries.len(), dir = %dir.display(), "system plug-ins seeded");
    Ok(entries.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scan half is pure: a tmp dir with two `.wasm` files plus noise yields
    /// exactly the two entries, `system/<stem>` paths, correct digests, sorted.
    #[test]
    fn scan_resolves_wasm_files_only() {
        let dir = std::env::temp_dir().join(format!("noetl-seed-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("orchestrate.wasm"), b"\x00asm-fake-orchestrate").unwrap();
        std::fs::write(dir.join("materialiser.wasm"), b"\x00asm-fake-materialiser").unwrap();
        std::fs::write(dir.join("README.md"), b"not a plug-in").unwrap();

        let entries = scan_system_plugins(&dir);

        assert_eq!(entries.len(), 2, "only the .wasm files seed");
        // Sorted by path.
        assert_eq!(entries[0].path, "system/materialiser");
        assert_eq!(entries[1].path, "system/orchestrate");
        assert!(entries.iter().all(|e| e.version == 1));
        assert!(entries.iter().all(|e| e.media_type == "application/wasm"));

        // Digest matches the register endpoint's computation exactly.
        let expected = hex::encode(Sha256::digest(b"\x00asm-fake-orchestrate"));
        assert_eq!(entries[1].digest, expected);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A missing directory is a no-op (local dev without the baked image),
    /// not an error.
    #[test]
    fn scan_missing_dir_is_empty() {
        let dir = std::env::temp_dir().join("noetl-seed-test-does-not-exist-xyz");
        assert!(scan_system_plugins(&dir).is_empty());
    }
}
