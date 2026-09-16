//! **Legacy-ref tier fallback guard** (noetl/ai-meta#343 FIX 3).
//!
//! ## The failure this exists for
//!
//! `GET /api/result/resolve?ref=…` accepts two reference shapes. The canonical
//! one resolves from the #104 object tier. The legacy
//! `noetl://execution/<eid>/result/<name>/<id>` one resolved *only* from
//! `noetl.result_store` — a table that stops being written the moment
//! `NOETL_RESULT_STORE_DUAL_WRITE=false`.
//!
//! Prod ran exactly that way. Measured 2026-09-15:
//!
//! ```text
//! deploy/noetl-server-rust   MINT_AUTHORITATIVE=true   DUAL_WRITE=false
//! deploy/noetl-worker-rust   MINT_AUTHORITATIVE=UNSET  URI_RESOLVE=true
//! ```
//!
//! So one worker pool minted legacy refs and the server had stopped writing the
//! rows that would resolve them. The endpoint answered **404 in 0.19 s** while
//! the bytes — 214,805 of them — sat in `noetl.object_store` under the canonical
//! key. The worker maps 404 to `Ok(None)` and leaves the step bound to its
//! summary, so `muno/playbooks/hotel-cards` returned **0 hotels with
//! `status: success` and no error anywhere**, for two days.
//!
//! ## Why a source-level guard
//!
//! The fallback needs a database, and this repo's CI runs `cargo test` with no
//! Postgres — so the wiring cannot be proven by executing it here. What *can* be
//! proven, and is what actually broke, is that the not-found path consults the
//! tier **before** answering 404, and that it is the legacy arm which does so.
//! Behaviour is measured end-to-end in kind; the anchoring of the query itself
//! is unit-tested in `src/db/queries/object_store.rs` (a too-loose pattern would
//! serve another step's result, which is worse than the 404 it replaces).
//!
//! Companion: `noetl/worker` `tests/reference_hydration_gate.rs` guards the
//! producer half (FIX 2) and the consume path.

const RESULT_STORE_HANDLER: &str = include_str!("../src/handlers/result_store.rs");
const RESULT_STORE_SERVICE: &str = include_str!("../src/services/result_store.rs");
const OBJECT_STORE_QUERIES: &str = include_str!("../src/db/queries/object_store.rs");

/// Source with comments stripped and the test module removed.
///
/// Both exclusions matter: a doc comment naming a symbol has satisfied an
/// earlier reachability check in this repo, and a needle written inside
/// `#[cfg(test)]` matches itself.
fn production_source(src: &str) -> String {
    let cut = src.find("\n#[cfg(test)]").unwrap_or(src.len());
    src[..cut]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn the_legacy_arm_consults_the_tier_before_answering_not_found() {
    let src = production_source(RESULT_STORE_HANDLER);

    let legacy_arm_start = src
        .find("ResultRef::Legacy(l)")
        .expect("resolve_ref no longer has a Legacy arm");
    let canonical_arm_start = src[legacy_arm_start..]
        .find("ResultRef::Canonical(")
        .map(|i| legacy_arm_start + i)
        .expect("resolve_ref no longer has a Canonical arm");
    let legacy_arm = &src[legacy_arm_start..canonical_arm_start];

    assert!(
        legacy_arm.contains("resolve_legacy_from_tier"),
        "the legacy arm of resolve_ref no longer falls back to the #104 tier.\n\
         With NOETL_RESULT_STORE_DUAL_WRITE=false the legacy row is never\n\
         written, so every legacy ref 404s while its bytes sit in the tier —\n\
         and the worker turns that 404 into a silently empty step result.\n\
         Prod 2026-09-15: hotel-cards returned 0 hotels for two days."
    );
    assert!(
        legacy_arm.contains("deps.service.resolve(l)"),
        "the legacy arm must still try noetl.result_store FIRST — the fallback\n\
         is belt-and-suspenders, not a replacement. Rows written before the\n\
         dual-write was retired are still authoritative for their refs."
    );

    // Order matters and is the whole point: consulting the tier *after*
    // returning 404 is the same as not consulting it.
    let store_read = legacy_arm.find("deps.service.resolve(l)").unwrap();
    let tier_read = legacy_arm.find("resolve_legacy_from_tier").unwrap();
    assert!(
        store_read < tier_read,
        "the tier fallback runs before the legacy store read; it must run only\n\
         on a MISS, so a stored row is never shadowed by a tier object."
    );
}

#[test]
fn the_fallback_cannot_turn_a_missing_result_into_a_server_error() {
    // The fallback is additive. If its own query fails — the object table is
    // absent on an older deployment, say — the endpoint must still answer the
    // 404 it answered before, not a 500. A belt-and-suspenders path that can
    // fail the request is worse than no path at all.
    let src = production_source(RESULT_STORE_HANDLER);
    let legacy_arm_start = src.find("ResultRef::Legacy(l)").unwrap();
    let legacy_arm = &src[legacy_arm_start..];
    let err_arm = legacy_arm
        .find("Err(e) =>")
        .map(|i| &legacy_arm[i..i + 400])
        .expect("the tier fallback no longer handles its own error case");
    assert!(
        err_arm.contains("Ok(None)") || err_arm.contains("warn"),
        "a failing tier fallback must degrade to not-found, not propagate.\n\
         Found:\n{err_arm}"
    );
}

#[test]
fn the_fallback_is_wired_from_the_handler_down_to_a_query() {
    // Reachability. This class of bug is "implemented but unreachable", and the
    // three previous fixes for it were all reachable-looking code that nothing
    // called on the path that mattered.
    let service = production_source(RESULT_STORE_SERVICE);
    assert!(
        service.contains("pub async fn resolve_legacy_from_tier"),
        "the service method the handler calls does not exist."
    );
    assert!(
        service.contains("get_result_tier_json_by_step"),
        "resolve_legacy_from_tier does not reach a tier query."
    );

    let queries = production_source(OBJECT_STORE_QUERIES);
    assert!(
        queries.contains("pub async fn get_result_tier_json_by_step"),
        "the by-step tier query does not exist."
    );
    assert!(
        queries.contains("tier_step_pattern"),
        "the by-step query no longer builds its pattern through the helper the\n\
         anchoring tests cover — an inlined pattern is an untested pattern, and\n\
         a too-loose one serves another step's result."
    );
}
