//! The path filter must run BEFORE the candidate window — noetl/server#435.
//!
//! # The false empty
//!
//! `ExecutionService::list` is candidate-first (noetl/ai-meta#62): stage 1 picks
//! the N most-recent executions, and the path filter used to run in stage 4, in
//! Rust, over that already-truncated set. So `?path=X&limit=N` meant *"of the N
//! most recent executions, those whose path matches"* rather than *"the N most
//! recent executions whose path matches"*.
//!
//! Measured on prod 2026-09-13, same filter, same data:
//!
//! ```text
//! ?path=saqbit/agents/orchestrator&limit=5   -> []        (0 matches)
//! ?path=saqbit/agents/orchestrator&limit=20  -> [1 row]
//! ?path=saqbit/agents/orchestrator&limit=50  -> [1 row]
//! ```
//!
//! The execution existed throughout. At `limit=5` it was the 6th most-recent
//! overall, so it never entered the candidate window.
//!
//! **The failure direction is what makes it serious.** A caller gets `[]` and
//! cannot distinguish "this playbook has never run" from "its runs are outside
//! the window" — no error, no truncation flag, identical across retries. That is
//! a confidently wrong answer, which this codebase has repeatedly found to be
//! worse than a loud failure.
//!
//! # Why a source guard
//!
//! The behavioural test the issue asks for — seed executions so a matching one
//! sits outside `limit`, then assert it is returned — needs a database, and this
//! repo's CI runs `cargo test` without one. So the *shape* is guarded here and
//! the *behaviour* is proven in kind against real data. The same split the
//! result-store tier fallback uses.

const EXECUTION_RS: &str = include_str!("../src/services/execution.rs");

/// Source with comments stripped and the test module removed.
///
/// Both exclusions matter: a doc comment naming a symbol has satisfied an
/// earlier reachability check in this repo, and a needle inside `#[cfg(test)]`
/// matches itself.
fn production_source(src: &str) -> String {
    let cut = src.find("\n#[cfg(test)]").unwrap_or(src.len());
    src[..cut]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//") && !l.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Extract `list`'s body.
fn list_body(src: &str) -> String {
    let start = src
        .find("pub async fn list(&self, filter: &ExecutionFilter)")
        .expect("ExecutionService::list not found — extraction broke");
    let rest = &src[start..];
    let end = rest
        .find("\n    /// Get detailed execution information.")
        .unwrap_or(rest.len());
    rest[..end].to_string()
}

/// The `recent` CTE — the candidate window itself. Scoped deliberately: see
/// below.
fn recent_cte(body: &str) -> String {
    let start = body
        .find("WITH recent AS (")
        .expect("the `recent` CTE not found — extraction broke");
    let rest = &body[start..];
    let end = rest
        .find("stats AS (")
        .expect("the `stats` CTE should follow `recent`");
    rest[..end].to_string()
}

/// Everything in `list` BEFORE the per-shard fan-out.
fn before_fanout(body: &str) -> String {
    let end = body
        .find("for_each_shard")
        .expect("per-shard fan-out not found — extraction broke");
    body[..end].to_string()
}

#[test]
fn the_path_filter_reaches_the_candidate_query() {
    let src = production_source(EXECUTION_RS);
    let body = list_body(&src);
    assert!(
        body.len() > 1_000,
        "list body slice too small ({}) — extraction broke",
        body.len()
    );

    // ⚠ BOTH assertions below are scoped, and the first draft of this test was
    // not. It searched the WHOLE of `list` for `FROM noetl.catalog` and
    // `catalog_id = ANY(` — and stage 3's cluster-master catalog lookup contains
    // both. So the guard passed with the fix reverted: a negative control that
    // deleted the `recent` CTE's filter changed nothing it could see.
    //
    // A guard that matches the wrong occurrence is worse than no guard: it
    // reports coverage for a property it never checked. Scope first, assert
    // second.
    let pre = before_fanout(&body);
    assert!(
        pre.contains("FROM noetl.catalog"),
        "the path filter is no longer resolved to catalog ids BEFORE the \
         per-shard fan-out. Without that, `?path=X&limit=N` silently means \"of \
         the N most recent executions, those matching X\" and returns a false \
         empty whenever the matching runs sit outside the window."
    );

    let recent = recent_cte(&body);
    assert!(
        recent.contains("catalog_id = ANY("),
        "the resolved catalog ids are not applied inside the `recent` CTE, so \
         the candidate window is still chosen without regard to the path \
         filter. (Stage 3's catalog lookup also contains this needle — that is \
         why this assertion is scoped to the CTE.)"
    );
}

/// A path matching no catalog entry is a true empty, and must not be confused
/// with an unfiltered query.
///
/// ⚠ Without the early return, `path_catalog_ids` would be `Some(vec![])` and
/// the bind would have to produce `catalog_id = ANY('{}')`. That is correct but
/// only by accident of SQL semantics; a future refactor treating an empty set as
/// "no filter" would turn a true empty into *every* execution — the opposite
/// wrong answer, and a much louder one.
#[test]
fn a_path_matching_no_catalog_entry_short_circuits() {
    let body = list_body(&production_source(EXECUTION_RS));
    assert!(
        body.contains("return Ok(Vec::new());"),
        "list no longer short-circuits when the path matches no catalog entry"
    );
}

/// The stage-4 filter stays as a belt.
///
/// It is not redundant: stage 1 filters on catalog id, stage 4 on the path text
/// actually stitched in at stage 3. If those two ever disagree — a catalog row
/// changing path, a stale id — the stage-4 filter is what keeps the answer
/// consistent with what the caller asked for.
#[test]
fn the_post_merge_path_filter_is_kept_as_well() {
    let body = list_body(&production_source(EXECUTION_RS));
    assert!(
        body.contains("path_pattern_lower"),
        "the post-merge path filter was removed; stage 1 filters on catalog id, \
         which is not the same statement as the path the caller asked for"
    );
}
