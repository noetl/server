//! The archive pass must page its candidate listing by keyset, not offset.
//!
//! `collect_candidates` used to walk `ExecutionService::list` with `offset`.
//! Each page re-aggregates `offset + limit` executions, so a full walk is
//! quadratic: in prod (2026-11-16) 137 pages climbing to ~4 s each, back to
//! back, every 5 min — the pass interval — keeping the DB busy continuously.
//!
//! Like `execution_list_path_filter.rs`, CI has no database, so the *shape* is
//! guarded here; the behaviour (a keyset walk returns exactly the full set) was
//! proven against prod data and in kind.

const ARCHIVE_RS: &str = include_str!("../src/services/event_archive.rs");
const EXECUTION_RS: &str = include_str!("../src/services/execution.rs");

fn code_only(src: &str) -> String {
    src.lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("//") && !t.starts_with("--")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn fn_body<'a>(src: &'a str, sig: &str) -> &'a str {
    let start = src.find(sig).unwrap_or_else(|| panic!("{sig} not found"));
    let rest = &src[start..];
    let end = rest.find("\n}\n").expect("fn end");
    &rest[..end]
}

#[test]
fn collect_candidates_pages_by_cursor_not_offset() {
    let src = code_only(ARCHIVE_RS);
    let body = fn_body(&src, "pub async fn collect_candidates(");
    assert!(
        !body.contains("offset: Some("),
        "collect_candidates must not page by offset (quadratic):\n{body}"
    );
    assert!(body.contains("before,"), "the keyset cursor must be passed");
    assert!(
        body.contains("before = Some((last.started_at, last.execution_id))"),
        "the cursor must advance to the last row of each page"
    );
}

#[test]
fn the_candidate_window_honours_the_cursor_with_a_total_order() {
    let src = code_only(EXECUTION_RS);
    let start = src.find("WITH recent AS (").expect("recent CTE");
    let recent = &src[start..src[start..].find("stats AS (").unwrap() + start];
    assert!(
        recent.contains("HAVING ($7::TIMESTAMPTZ IS NULL"),
        "rows strictly below the cursor:\n{recent}"
    );
    assert!(
        recent.contains("ORDER BY started_at DESC, execution_id DESC"),
        "the window needs a total order or ties straddling a page are skipped"
    );
    assert!(
        src.contains("ORDER BY r.started_at DESC, r.execution_id DESC"),
        "the final ORDER BY must match the window's"
    );
    assert!(
        src.contains("std::cmp::Reverse((b.3, b.0))"),
        "the shard merge must sort by the same (started_at, execution_id) key"
    );
}
