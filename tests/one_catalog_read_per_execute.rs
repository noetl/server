//! Guards for noetl/ai-meta#319 P2 — the catalog row is read ONCE per execute.
//!
//! The defect these guard against is not a bug in any one function. Every one of
//! the four reads was locally reasonable; the cost was only visible by counting
//! them together, on a path where a Postgres round-trip measures 13-20 ms. So the
//! guards are about the *call graph*, which no unit test on a single function can
//! see — the same reason `catalog_snapshot_wiring.rs` exists.

const EXECUTE: &str = include_str!("../src/handlers/execute.rs");
const SNAPSHOT: &str = include_str!("../src/handlers/catalog_snapshot.rs");
const EVENT_WRITE: &str = include_str!("../src/handlers/event_write.rs");

fn non_test(src: &str) -> &str {
    src.split_once("\n#[cfg(test)]").map_or(src, |(b, _)| b)
}

/// Count the reads of `noetl.catalog` a source file performs.
///
/// ⚠ Comment lines are excluded. The first version of this counter matched the
/// phrase anywhere and scored 4 against a file with 3 queries, because a doc
/// comment mentioning the removed query counted as one — the same
/// comments-as-callers idiom that has produced false readings in this repo
/// before. A guard whose count includes prose measures the prose.
fn catalog_reads(src: &str) -> usize {
    non_test(src)
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .filter(|l| l.contains("FROM noetl.catalog"))
        .count()
}

/// The execute path performs exactly the reads it needs and no repeats.
///
/// **Two** in `execute.rs`: the by-`catalog_id` resolution and the by-`path`
/// resolution — mutually exclusive branches of one request — plus
/// `load_catalog_by_id`, which is reached only when the read-source ladder elects
/// to serve a row other than the incumbent (not a shipping mode).
///
/// **Zero** in `catalog_snapshot.rs` and, on the execute path, zero in
/// `event_write.rs`: both used to re-read a row the caller already held.
#[test]
fn the_catalog_row_is_read_once_per_execution() {
    // resolve by id, resolve by path, and the ladder's non-incumbent loader.
    assert_eq!(
        catalog_reads(EXECUTE),
        3,
        "execute.rs reads noetl.catalog a different number of times than the \
         two resolution branches plus the ladder's loader — a re-read has been \
         added back, or a branch has been lost"
    );

    assert_eq!(
        catalog_reads(SNAPSHOT),
        0,
        "catalog_snapshot re-reads noetl.catalog; it is handed the CatalogItem \
         the caller already resolved, and a fourth read of that row was ~15 ms \
         on every production execute (NOETL_CATALOG_SNAPSHOT=digest is live)"
    );
}

/// The publish gate's memo is seeded BEFORE any event is emitted.
///
/// `should_publish` consults `is_system_execution`, which reads
/// `SELECT path FROM noetl.catalog` on a memo miss. Seeding after the first
/// `emit_event` would let that read fire once per cold catalog_id — the memo
/// would still fill, so the query would be rare and the regression invisible in
/// aggregate latency while being a full round-trip for the request that pays it.
#[test]
fn the_system_path_memo_is_seeded_before_the_first_event() {
    let seed = EXECUTE
        .find("memoize_system_path(")
        .expect("execute_one must seed the system-path memo from the resolved row");
    let emit = EXECUTE
        .find("emit_playbook_started_event(")
        .expect("execute_one must still emit playbook_started");
    assert!(
        seed < emit,
        "the system-path memo is seeded AFTER the first event is emitted, so \
         should_publish still pays a catalog read on a cold catalog_id"
    );
}

/// The memo is seeded from the catalog's `path` COLUMN, never a request string.
///
/// `is_system_path` is a `system/` prefix test and the memo is what the publish
/// gate consults. Seeding it from `request.path` would let a caller pick which
/// side of that gate its execution lands on by how it spells the path — the
/// request's spelling and the stored one can differ.
#[test]
fn the_memo_is_seeded_from_the_catalog_path_not_the_request() {
    let at = EXECUTE.find("memoize_system_path(").unwrap();
    let call = &EXECUTE[at..at + 60];
    assert!(
        !call.contains("request"),
        "the system-path memo is seeded from the request rather than the \
         resolved catalog row:\n{call}"
    );
    assert!(
        EVENT_WRITE.contains("is_system_path(path)"),
        "memoize_system_path must classify with the same predicate \
         is_system_execution uses, or the two disagree on the same row"
    );
}

/// ⚠ NEGATIVE CONTROL for the invalidation gate: nothing is cached.
///
/// The round-trips were removed by not *repeating* a read, not by retaining its
/// result. That is what makes "a changed definition is picked up on the next
/// request" true by construction rather than by a TTL.
///
/// Without this test the suite would pass just as happily on an implementation
/// that memoised catalog content in a static — which would be faster still, and
/// would serve a stale playbook after a re-register. `SYSTEM_CATALOG` is the one
/// permitted memo (a `system/` prefix on an immutable path column) and predates
/// this change.
#[test]
fn no_playbook_content_is_retained_between_requests() {
    let src = non_test(EXECUTE);
    // ⚠ `static ` alone is not the pattern: every `&'static str` in a signature
    // matches it, and the first version of this test failed on the `entry:
    // &'static str` metrics label rather than on any retained state.
    for retainer in ["\nstatic ", "LazyLock", "OnceLock", "thread_local"] {
        assert!(
            !src.contains(retainer),
            "execute.rs now holds process-wide state (`{retainer}`). If that \
             caches catalog content or a parsed playbook, a re-registered \
             definition is served stale — the one property this change promises \
             it does not touch."
        );
    }
    assert!(
        src.contains("let catalog = resolve_catalog(state, &request).await?;"),
        "execute_one no longer resolves the catalog unconditionally on every \
         request; if resolution became conditional on a cache hit, freshness is \
         no longer guaranteed"
    );
}
