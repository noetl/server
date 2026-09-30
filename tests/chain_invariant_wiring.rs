//! noetl/ai-meta#362 (c) + (d) — structural guards, DB-free.
//!
//! These read the source. That is unusual, and the reason is specific: both defects
//! they guard are ABSENCES, and an absence cannot be caught by exercising the code —
//! there is nothing to exercise.
//!
//! - (c) `events_materialize` wrote 10 columns and `prev_event_id` was not among
//!   them, so every row it materialised became a chain ROOT. Nothing failed. The
//!   rows were valid; they were just all roots.
//! - (d) a handler with no registered route. `services/execution.rs` states that
//!   "the `/api/executions/{id}/events` endpoint can serve the full history" and
//!   **that route does not exist** — found while investigating #362, after reading a
//!   truncated window and drawing two wrong conclusions from it. A doc comment
//!   naming a route is not a route.

const INTERNAL: &str = include_str!("../src/handlers/internal.rs");
const MAIN: &str = include_str!("../src/main.rs");

/// (c) The materializer must persist the chain link, and must compute it.
#[test]
fn a362_events_materialize_stamps_the_chain_link() {
    let start = INTERNAL
        .find("pub async fn events_materialize")
        .expect("events_materialize must exist");
    let end = INTERNAL[start..]
        .find("\n/// `POST /api/internal/cleanup/purge`")
        .map(|i| start + i)
        .unwrap_or(INTERNAL.len());
    let body = &INTERNAL[start..end];
    assert!(
        body.len() > 1200,
        "extracted {} bytes for events_materialize — implausibly small, so this \
         guard would pass while measuring nothing. The end anchor probably moved.",
        body.len()
    );

    let insert = body
        .find("INSERT INTO noetl.event")
        .expect("the function must insert into noetl.event");
    let col_list_end = body[insert..]
        .find(") ")
        .map(|i| insert + i)
        .expect("column list must terminate");
    let columns = &body[insert..col_list_end];

    assert!(
        columns.contains("prev_event_id"),
        "prev_event_id is MISSING from the INSERT column list, so every row this \
         endpoint writes gets a NULL prev — i.e. becomes a chain ROOT. One root per \
         execution is the genesis and correct; one per ROW makes the partition \
         unbuildable under link-defined ordering (noetl/ai-meta#362).\ncolumns: {columns}"
    );
    // ⚠⚠ STRIP COMMENTS FIRST, and match the CALL, not the word.
    //
    // This assertion was `body.contains("link_batch")` and a mutant that deleted the
    // call SURVIVED it — because the comment above the call says "link_batch". That
    // is the comments-counting-as-callers failure this codebase has hit before, and
    // the guard reproduced it exactly.
    let code: String = body
        .lines()
        .map(str::trim_start)
        .filter(|l| !l.starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        code.contains("chain_heads.link_batch("),
        "the column is present but nothing computes a value for it, so it binds NULL \
         and the fix is cosmetic. `chain_heads.link_batch(` must be CALLED — and this \
         checks comment-stripped code, because the prose above the call is not a call."
    );
}

/// ⭐ The positive control for the check above. If the extraction were broken, the
/// assertions would pass against an empty string and report a false clean.
#[test]
fn a362_the_materialize_check_can_fail() {
    let fake = "pub async fn events_materialize() { INSERT INTO noetl.event (event_id, \
                execution_id) VALUES ($1,$2) }";
    let insert = fake.find("INSERT INTO noetl.event").unwrap();
    let cols = &fake[insert..insert + fake[insert..].find(") ").unwrap()];
    assert!(
        !cols.contains("prev_event_id"),
        "the extraction must be able to SEE a missing column, or the real check is \
         decorative"
    );
    assert!(!fake.contains("link_batch"));
}

/// (d) The invariant endpoint must be REACHABLE — registered on a router in main.
#[test]
fn a362_the_invariant_route_is_registered() {
    // ⚠ The QUOTED path, including the closing quote. `contains` on the bare path
    // matched `"/api/internal/chain/invariant-DISABLED"` too, so a mutant that
    // renamed the route survived this guard.
    assert!(
        MAIN.contains("\"/api/internal/chain/invariant\""),
        "the chain-invariant route is not registered in main.rs. The handler would \
         compile, be documented, and be unreachable — exactly the shape of \
         `/api/executions/{{id}}/events`, which a doc comment names and no router \
         serves."
    );
    assert!(
        MAIN.contains("handlers::chain_populate::chain_invariant"),
        "the path is registered but not wired to the handler"
    );
    // ⚠ And it must be on the GATED side. A read-only endpoint is still a surface,
    // and this codebase has shipped two that returned more than intended without
    // authentication (noetl/ai-meta#303, #312).
    let at = MAIN.find("\"/api/internal/chain/invariant\"").unwrap();
    assert!(
        MAIN[..at].contains("/api/internal/events/materialize"),
        "the route must sit inside the internal router group, whose merge is gated"
    );
}

/// (d) All three invariant labels pinned — a labelled family is pruned from
/// `/metrics` until something sets it, so an unpinned `multi_root` is absent and
/// reads exactly like a build that cannot report it.
#[test]
fn a362_invariant_labels_are_pinned() {
    let outcomes = noetl_server::metrics::CHAIN_ROOT_INVARIANT_OUTCOMES;
    assert_eq!(outcomes.len(), 3, "one_root / multi_root / no_root");
    for want in ["one_root", "multi_root", "no_root"] {
        assert!(outcomes.contains(&want), "{want} is not pinned");
    }
    noetl_server::metrics::init_chain_root_invariant_series();
    let g = noetl_server::metrics::chain_root_invariant();
    for want in outcomes {
        // `get` on a pinned label must not create a new child silently; the point is
        // that the series EXISTS at 0 before anything fires.
        assert_eq!(
            g.with_label_values(&[want]).get(),
            0,
            "{want} must be pinned at 0, not absent"
        );
    }
}

/// The hydrate labels must cover the new ambiguous outcome.
#[test]
fn a362_head_ambiguous_is_a_pinned_hydrate_label() {
    let all = noetl_server::state::HydrateOutcome::ALL_LABELS;
    assert!(
        all.contains(&"head_ambiguous"),
        "HeadAmbiguous is recorded but not pinned, so a forked-chain repair is an \
         absent series until the first one happens — and the first one is exactly \
         when you want to see it. labels={all:?}"
    );
    assert_eq!(all.len(), 5);
}
