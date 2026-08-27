//! `<VAR>_FILE` hydration for the three bootstrap secrets (noetl/ai-meta#267 Tier 2).
//!
//! # Why this exists at all
//!
//! `NOETL_ENCRYPTION_KEY`, `POSTGRES_PASSWORD` and `NOETL_INTERNAL_API_TOKEN`
//! cannot come from the Secrets Wallet, and the reason is structural rather than
//! incidental: **the wallet reads the credential table over Postgres, and
//! Postgres needs `POSTGRES_PASSWORD`.** `NOETL_ENCRYPTION_KEY` protects that
//! table's contents. A credential the credential system depends on cannot be
//! stored in the credential system.
//!
//! So these three arrive as files, mounted by the GKE Secret Manager CSI driver
//! before the container starts. A file is present or absent; unlike an SM client
//! call at startup there is nothing to hang — which matters, because
//! noetl/ai-meta#297 was an unbounded startup park that ran ~36h unnoticed.
//!
//! # Why hydration rather than per-read-site changes
//!
//! `POSTGRES_PASSWORD` is not read by `env::var` anywhere — it is deserialised by
//! `envy::prefixed("POSTGRES_").from_env::<DatabaseConfig>()`. Touching read
//! sites would therefore miss it, and would have to touch the worker's sites too.
//! Hydrating the process environment *before* anything reads it covers every
//! consumer — `env::var`, `envy`, and any future one — with no read-site change.
//!
//! # Inert by default
//!
//! With no `<VAR>_FILE` set this function does nothing at all. That is what makes
//! every rollout stage reversible: until the CSI mount and the `_FILE` variable
//! are both present, the existing `secretKeyRef` path is the only path, and
//! behaviour is byte-identical to today.
//!
//! Precedence is deliberately **file wins**: during the dual-run both sources are
//! present, and the migration is only meaningful if the file is the one that
//! takes effect. An empty or unreadable file is *not* treated as an override —
//! falling back is safer than starting with an empty credential, which would fail
//! later and further from the cause.

use std::path::Path;

/// The variables this hydrates. Deliberately a fixed list rather than "any env
/// var ending `_FILE`": an allowlist cannot be widened by an unrelated variable
/// that happens to match the pattern.
pub const HYDRATED: [&str; 3] = [
    "NOETL_ENCRYPTION_KEY",
    "POSTGRES_PASSWORD",
    "NOETL_INTERNAL_API_TOKEN",
];

/// What happened for one variable — the shape the metric and the log line record.
/// Never carries a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `<VAR>_FILE` was set, readable and non-empty; `<VAR>` now holds its contents.
    File,
    /// No `<VAR>_FILE`; whatever `<VAR>` already held is untouched.
    Env,
    /// `<VAR>_FILE` was set but unusable (missing, unreadable, or empty), so the
    /// env value stands. Distinct from `Env` on purpose: it means an operator
    /// *intended* file delivery and did not get it, which is a misconfiguration
    /// that must be visible rather than silently degraded.
    FileUnusable,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Env => "env",
            Self::FileUnusable => "file_unusable",
        }
    }
}

/// Decide the outcome for one variable, without touching the process or the disk.
/// Split out so every arm is testable — including the ones that need a file that
/// does not exist.
pub fn decide(file_var: Option<&str>, file_contents: Option<&str>) -> Source {
    match file_var {
        None => Source::Env,
        Some(p) if p.trim().is_empty() => Source::Env,
        Some(_) => match file_contents {
            Some(c) if !c.trim().is_empty() => Source::File,
            _ => Source::FileUnusable,
        },
    }
}

/// Hydrate `<VAR>` from `<VAR>_FILE` for each entry of [`HYDRATED`].
///
/// Returns what happened per variable, for logging and metrics.
///
/// # Safety
///
/// Calls `std::env::set_var`, which is `unsafe` because concurrent readers race
/// it. This MUST be called at the very top of `main`, before any thread is
/// spawned and before any configuration is read — that is the whole contract, and
/// it is why this is a single early call rather than something invoked lazily.
/// The guard test below pins the call site so a later refactor cannot quietly
/// move it after the runtime starts.
pub fn hydrate() -> Vec<(&'static str, Source)> {
    let mut out = Vec::with_capacity(HYDRATED.len());
    for var in HYDRATED {
        let file_var = std::env::var(format!("{var}_FILE")).ok();
        let contents = file_var.as_deref().and_then(|p| {
            if p.trim().is_empty() {
                None
            } else {
                std::fs::read_to_string(Path::new(p.trim())).ok()
            }
        });
        let decision = decide(file_var.as_deref(), contents.as_deref());
        if decision == Source::File {
            if let Some(c) = contents.as_deref() {
                // Trailing newline is the norm for a mounted file and is not part
                // of the secret; a token with \n appended fails auth in a way that
                // looks like a wrong value rather than a formatting bug.
                // SAFETY: see the function contract — called before any thread.
                unsafe { std::env::set_var(var, c.trim_end_matches(['\n', '\r'])) };
            }
        }
        if decision == Source::FileUnusable {
            tracing::warn!(
                target: "noetl_server::secrets",
                var,
                "{var}_FILE is set but the file is missing, unreadable or empty — \
                 falling back to the environment value (noetl/ai-meta#267)"
            );
        }
        out.push((var, decision));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The inert case, and the one that makes the rollout reversible: no `_FILE`
    /// anywhere means nothing changes.
    #[test]
    fn no_file_var_leaves_the_environment_alone() {
        assert_eq!(decide(None, None), Source::Env);
        assert_eq!(decide(None, Some("ignored")), Source::Env);
    }

    /// File wins during the dual-run — otherwise the migration would be a no-op
    /// that still reported success.
    #[test]
    fn a_readable_non_empty_file_wins() {
        assert_eq!(decide(Some("/x"), Some("value")), Source::File);
    }

    /// An unusable file must NOT be reported as `env`. An operator who mounted a
    /// file and got the env value silently has a misconfiguration that will
    /// resurface later and further from its cause.
    #[test]
    fn an_unusable_file_is_named_as_such_not_silently_degraded() {
        assert_eq!(decide(Some("/missing"), None), Source::FileUnusable);
        assert_eq!(decide(Some("/empty"), Some("")), Source::FileUnusable);
        assert_eq!(decide(Some("/blank"), Some("   \n")), Source::FileUnusable);
        assert_ne!(decide(Some("/missing"), None), Source::Env);
    }

    /// An empty `_FILE` value is "unset", not "a file at path ''".
    #[test]
    fn an_empty_file_var_is_treated_as_unset() {
        assert_eq!(decide(Some(""), None), Source::Env);
        assert_eq!(decide(Some("   "), None), Source::Env);
    }

    /// The list is an allowlist. If this ever grows implicitly, a variable could
    /// be hydrated that nobody intended.
    #[test]
    fn the_hydrated_set_is_exactly_the_three_bootstrap_secrets() {
        assert_eq!(
            HYDRATED,
            [
                "NOETL_ENCRYPTION_KEY",
                "POSTGRES_PASSWORD",
                "NOETL_INTERNAL_API_TOKEN"
            ]
        );
    }

    /// `hydrate()` must run before anything reads configuration. Pinned as a
    /// source check because a later refactor moving it after the runtime starts
    /// would be both a data race and a silently wrong config.
    #[test]
    fn hydrate_is_called_at_the_top_of_main() {
        let src = include_str!("../main.rs");
        // Scope the search to the BODY of main. `use` imports naturally mention
        // config types above it, and matching those made this assert on import
        // order rather than on call order — a guard that fails for the wrong
        // reason is worse than no guard.
        let body_start = src
            .find("async fn main")
            .expect("main.rs must define async fn main");
        let body = &src[body_start..];
        let call = body
            .find("secrets::file_env::hydrate()")
            .expect("main() must call secrets::file_env::hydrate()");
        // Anything that READS configuration must come after it.
        //
        // ⚠ `envy::` is deliberately written as `envy::prefixed`: the bare
        // substring also matches `dotenvy::`, which appears one line ABOVE the
        // hydrate call, so the naive pattern failed on the correct code.
        for reader in [
            "DatabaseConfig::from_env",
            "envy::prefixed",
            "AppConfig::from_env",
        ] {
            if let Some(at) = body.find(reader) {
                assert!(
                    call < at,
                    "hydrate() must run before {reader} — it is what puts the \
                     secret in the environment that {reader} then reads"
                );
            }
        }
    }

    /// The guard above must actually be able to fail. A source check that can
    /// only pass is indistinguishable from no check.
    #[test]
    fn the_placement_guard_can_fail() {
        let bad = "async fn main() {\n let c = DatabaseConfig::from_env();\n secrets::file_env::hydrate();\n}";
        let body = &bad[bad.find("async fn main").unwrap()..];
        let call = body.find("secrets::file_env::hydrate()").unwrap();
        let at = body.find("DatabaseConfig::from_env").unwrap();
        assert!(call > at, "fixture must represent the WRONG order");
    }
}
