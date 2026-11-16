//! How the `database_routes` group is protected (noetl/ai-meta#312).
//!
//! **Staged, not chosen.** The default is `open`, which is today's behaviour
//! exactly. All four options from the issue are reachable by setting one
//! variable, so the owner's decision is a flag flip rather than another change.
//!
//! # Why a policy rather than a fix
//!
//! `/api/postgres/execute` runs arbitrary SQL unauthenticated, and
//! `/api/db/init` and `/api/db/validate` share its router. The evidence gathered
//! on #312 shows no prod or CI path depends on the router being open — the prod
//! schema comes from `schema_ddl.sql` in the Postgres container, not over HTTP —
//! but the choice between gating everything, gating one route, restricting the
//! statement class, or removing the endpoint is a judgement about who should be
//! able to do what. That is not the agent's call, so all four are staged.

use serde::Serialize;

/// `NOETL_DATABASE_ROUTES_AUTH` — which of #312's four options is in force.
pub const POLICY_ENV: &str = "NOETL_DATABASE_ROUTES_AUTH";

/// Policies, pinned as metric labels.
pub const POLICIES: [&str; 5] = [
    "open",
    "gate_all",
    "gate_execute",
    "readonly_execute",
    "disable_execute",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    /// Today. Nothing gated, no statement restriction. **Default.**
    Open,
    /// #312 option 1 — gate the whole router, `init` and `validate` included.
    GateAll,
    /// #312 option 2 — gate `/api/postgres/execute` only.
    GateExecute,
    /// #312 option 3 — gate `/api/postgres/execute` **and** restrict it to
    /// read-only statements.
    ReadonlyExecute,
    /// #312 option 4 — `/api/postgres/execute` answers 501 and runs nothing.
    DisableExecute,
}

impl Policy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::GateAll => "gate_all",
            Self::GateExecute => "gate_execute",
            Self::ReadonlyExecute => "readonly_execute",
            Self::DisableExecute => "disable_execute",
        }
    }
    /// Whether `/api/postgres/execute` carries the auth gate.
    pub fn gates_execute(self) -> bool {
        matches!(self, Self::GateAll | Self::GateExecute | Self::ReadonlyExecute)
    }
    /// Whether `/api/db/init` and `/api/db/validate` carry the auth gate.
    ///
    /// Only `gate_all` does. This is the distinction the whole decision turns
    /// on: those two are the ones a local `noetl db init` calls without a token.
    pub fn gates_init_validate(self) -> bool {
        matches!(self, Self::GateAll)
    }
    /// Whether the endpoint refuses to run anything at all.
    pub fn execute_disabled(self) -> bool {
        matches!(self, Self::DisableExecute)
    }
    /// Whether only read-only statements are permitted.
    pub fn read_only(self) -> bool {
        matches!(self, Self::ReadonlyExecute)
    }
}

pub fn policy() -> Policy {
    parse_policy(std::env::var(POLICY_ENV).ok().as_deref())
}

/// The parse, without the environment — testable without `set_var`, which races.
///
/// Unrecognised resolves to `Open`. That direction is uncomfortable and it is
/// deliberate: this stages a change nobody has approved, so a typo must not
/// silently start rejecting requests that work today. It becomes the wrong
/// default the moment an option is chosen, and the chooser should change it.
pub fn parse_policy(raw: Option<&str>) -> Policy {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("gate_all") => Policy::GateAll,
        Some("gate_execute") => Policy::GateExecute,
        Some("readonly_execute") => Policy::ReadonlyExecute,
        Some("disable_execute") => Policy::DisableExecute,
        _ => Policy::Open,
    }
}

/// Is this SQL read-only?
///
/// **Fail-closed**: anything not recognised as a single read is refused. A
/// permissive check here would be worse than no check, because it would carry
/// the reassurance of a restriction without the restriction.
///
/// Deliberately conservative rather than clever:
///
/// * multiple statements are refused outright — `SELECT 1; DROP TABLE x` starts
///   with `SELECT`, and a prefix check is exactly the mistake this avoids;
/// * only `SELECT`, `WITH` and `SHOW` open a read, and a `WITH` carrying a
///   data-modifying CTE (`INSERT`/`UPDATE`/`DELETE`/`MERGE`) is refused;
/// * comments are stripped first, so `/*x*/DROP` cannot hide behind one.
pub fn is_read_only(sql: &str) -> bool {
    let stripped = strip_comments(sql);
    let t = stripped.trim();
    if t.is_empty() {
        return false;
    }
    // One statement only. A trailing semicolon is fine; a second statement is not.
    let body = t.strip_suffix(';').unwrap_or(t);
    if body.contains(';') {
        return false;
    }
    let upper = body.to_ascii_uppercase();
    let first = upper.split_whitespace().next().unwrap_or("");
    if !matches!(first, "SELECT" | "WITH" | "SHOW") {
        return false;
    }
    // A data-modifying CTE is a write wearing a SELECT's clothes.
    // Belt and braces. `DO`, `COPY`, `CALL`, `VACUUM` and `SET` are already
    // refused by the first-word check above, so these entries add no reachable
    // protection today — mutation-testing showed the code-execution test still
    // passes without them. They are kept as defence in depth against a future
    // relaxation of that first-word rule, not because a gap was found.
    //
    // ⚠⚠ THE REAL LIMIT, which no keyword list can close: a plain
    // `SELECT pg_read_file(...)` or `SELECT dblink_exec(...)` is a function call,
    // not a keyword, and this check ALLOWS it. `readonly_execute` therefore means
    // "no DDL/DML statement", NOT "no side effects". If side-effect-free is the
    // requirement, the answer is `disable_execute` or a function allowlist —
    // and pretending otherwise would be worse than no restriction, because it
    // would carry the reassurance without the property.
    for w in [
        "INSERT", "UPDATE", "DELETE", "MERGE", "DROP", "TRUNCATE", "ALTER",
        "CREATE", "GRANT", "REVOKE", "DO", "COPY", "CALL", "VACUUM", "REINDEX",
        "REFRESH", "SET", "RESET", "LOCK", "COMMENT", "SECURITY",
    ] {
        if word_present(&upper, w) {
            return false;
        }
    }
    true
}

/// Whole-word match, so `SELECT created_at` is not read as `CREATE`.
fn word_present(haystack_upper: &str, word: &str) -> bool {
    haystack_upper
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|tok| tok == word)
}

fn strip_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let b: Vec<char> = sql.chars().collect();
    let (mut i, mut in_block, mut in_line) = (0usize, false, false);
    while i < b.len() {
        if in_block {
            if b[i] == '*' && i + 1 < b.len() && b[i + 1] == '/' {
                in_block = false;
                i += 2;
                out.push(' ');
                continue;
            }
        } else if in_line {
            if b[i] == '\n' {
                in_line = false;
                out.push('\n');
            }
        } else if b[i] == '/' && i + 1 < b.len() && b[i + 1] == '*' {
            in_block = true;
            i += 2;
            continue;
        } else if b[i] == '-' && i + 1 < b.len() && b[i + 1] == '-' {
            in_line = true;
            i += 2;
            continue;
        } else {
            out.push(b[i]);
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_open_which_is_todays_behaviour() {
        assert_eq!(parse_policy(None), Policy::Open);
        assert_eq!(parse_policy(Some("")), Policy::Open);
        assert!(!Policy::Open.gates_execute());
        assert!(!Policy::Open.gates_init_validate());
        assert!(!Policy::Open.read_only());
        assert!(!Policy::Open.execute_disabled());
    }

    /// A typo must not silently start rejecting requests that work today.
    #[test]
    fn an_unrecognised_policy_stays_open() {
        for junk in ["gateall", "GATE-ALL", "true", "1", "on", "strict"] {
            assert_eq!(parse_policy(Some(junk)), Policy::Open, "{junk}");
        }
        assert_eq!(parse_policy(Some(" Gate_All ")), Policy::GateAll);
    }

    /// ⭐ Only `gate_all` gates init/validate — the distinction the decision turns on.
    #[test]
    fn only_gate_all_touches_init_and_validate() {
        assert!(Policy::GateAll.gates_init_validate());
        for p in [Policy::GateExecute, Policy::ReadonlyExecute, Policy::DisableExecute] {
            assert!(
                !p.gates_init_validate(),
                "{} must leave db/init and db/validate alone — they are what a local \
                 `noetl db init` calls without a token",
                p.as_str()
            );
        }
    }

    #[test]
    fn every_policy_that_should_gate_execute_does() {
        for p in [Policy::GateAll, Policy::GateExecute, Policy::ReadonlyExecute] {
            assert!(p.gates_execute(), "{}", p.as_str());
        }
        assert!(!Policy::Open.gates_execute());
    }

    /// ⭐ A prefix check is the mistake this avoids.
    #[test]
    fn a_second_statement_is_refused_however_it_starts() {
        assert!(!is_read_only("SELECT 1; DROP TABLE noetl.event"));
        assert!(!is_read_only("SELECT 1;DELETE FROM noetl.outbox"));
        assert!(is_read_only("SELECT 1;"), "a single trailing semicolon is fine");
    }

    /// ⭐ Isolates the MULTI-STATEMENT guard from the keyword scan.
    ///
    /// The two cases above are caught by the keyword scan as well, so they pass
    /// even with the semicolon check removed — mutation-testing showed exactly
    /// that. This one uses a second statement whose verb the scan did not
    /// originally know, so only the multi-statement rule can refuse it.
    #[test]
    fn a_second_statement_is_refused_even_when_its_verb_is_unusual() {
        assert!(
            !is_read_only("SELECT 1; ANALYZE noetl.event"),
            "a second statement must be refused by the multi-statement rule, not \
             by luck of the keyword list"
        );
    }

    /// Statements that execute code are refused — by the first-word rule.
    ///
    /// ⚠ Recorded honestly: mutation-testing showed this test still passes with
    /// the DO/COPY/CALL keywords removed, because the first-word check already
    /// refuses anything that is not SELECT/WITH/SHOW. The keyword entries are
    /// defence in depth, not a fix for a demonstrated gap.
    #[test]
    fn code_executing_statements_are_refused() {
        for s in [
            "DO $$ BEGIN PERFORM 1; END $$",
            "COPY (SELECT 1) TO PROGRAM 'sh -c id'",
            "CALL some_procedure()",
            "WITH x AS (SELECT 1) SELECT 1; DO $$ BEGIN END $$",
            "VACUUM FULL noetl.event",
            "SET ROLE postgres",
        ] {
            assert!(!is_read_only(s), "must refuse code execution: {s:?}");
        }
    }

    /// A comment must not hide a write.
    #[test]
    fn a_comment_cannot_hide_a_write() {
        assert!(!is_read_only("/* harmless */ DROP TABLE noetl.event"));
        assert!(!is_read_only("-- ok\nTRUNCATE noetl.outbox"));
        assert!(is_read_only("/* count them */ SELECT COUNT(*) FROM noetl.outbox"));
    }

    /// A data-modifying CTE is a write wearing a SELECT's clothes.
    #[test]
    fn a_writable_cte_is_not_read_only() {
        assert!(!is_read_only(
            "WITH d AS (DELETE FROM noetl.outbox RETURNING *) SELECT * FROM d"
        ));
        assert!(is_read_only(
            "WITH c AS (SELECT 1 AS n) SELECT n FROM c"
        ));
    }

    /// ⚠ A column named like a keyword must not be refused.
    ///
    /// `created_at` contains "CREATE". A substring check would reject the most
    /// ordinary query in this schema, the restriction would be switched off, and
    /// the endpoint would be back to unrestricted.
    #[test]
    fn a_column_named_like_a_keyword_is_still_read_only() {
        assert!(is_read_only(
            "SELECT created_at, updated_at FROM noetl.event LIMIT 1"
        ));
        assert!(is_read_only("SELECT MAX(created_at) FROM noetl.catalog"));
    }

    #[test]
    fn writes_are_refused_and_reads_are_allowed() {
        for w in [
            "DELETE FROM noetl.outbox",
            "UPDATE noetl.catalog SET kind='x'",
            "DROP TABLE noetl.projection",
            "TRUNCATE noetl.event",
            "ALTER TABLE noetl.event ADD COLUMN x INT",
            "GRANT ALL ON noetl.event TO PUBLIC",
            "",
            "   ",
        ] {
            assert!(!is_read_only(w), "must refuse: {w:?}");
        }
        for r in [
            "SELECT 1",
            "select count(*) from noetl.outbox",
            "SHOW server_version",
        ] {
            assert!(is_read_only(r), "must allow: {r:?}");
        }
    }

    /// ⚠⚠ The documented LIMIT of `readonly_execute`, asserted so nobody mistakes
    /// it for something stronger.
    ///
    /// A function call is not a keyword. `SELECT pg_read_file(...)` passes this
    /// check. The mode means "no DDL/DML statement", not "no side effects", and a
    /// test that pretends otherwise would be the reassurance without the property.
    #[test]
    fn readonly_execute_does_not_mean_side_effect_free() {
        assert!(
            is_read_only("SELECT pg_read_file('/etc/passwd')"),
            "this is ALLOWED — the check is statement-class, not side-effect. If \
             that is unacceptable, the answer is disable_execute or a function \
             allowlist, not a longer keyword list."
        );
    }

    #[test]
    fn every_policy_label_is_pinned() {
        for p in [
            Policy::Open,
            Policy::GateAll,
            Policy::GateExecute,
            Policy::ReadonlyExecute,
            Policy::DisableExecute,
        ] {
            assert!(POLICIES.contains(&p.as_str()));
        }
        assert_eq!(POLICIES.len(), 5);
    }
}
