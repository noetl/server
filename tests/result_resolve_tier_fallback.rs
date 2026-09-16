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
const EVENTS_HANDLER: &str = include_str!("../src/handlers/events.rs");
const EXECUTION_SERVICE: &str = include_str!("../src/services/execution.rs");

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
fn the_store_read_falls_back_to_the_tier_before_reporting_not_found() {
    let src = production_source(RESULT_STORE_SERVICE);

    let body_start = src
        .find("pub async fn resolve(&self, noetl_ref: &NoetlRef)")
        .expect("ResultStoreService::resolve no longer exists");
    let body_end = src[body_start..]
        .find("pub async fn resolve_store_only")
        .map(|i| body_start + i)
        .expect("resolve_store_only no longer follows resolve");
    let resolve = &src[body_start..body_end];

    assert!(
        resolve.contains("resolve_legacy_from_tier"),
        "ResultStoreService::resolve no longer falls back to the #104 tier.\n\
         With NOETL_RESULT_STORE_DUAL_WRITE=false the legacy row is never\n\
         written, so every legacy ref reads as not-found while its bytes sit in\n\
         the tier — and the consumer silently receives a bare reference.\n\
         Prod 2026-09-15: hotel-cards returned 0 hotels for two days."
    );
    assert!(
        resolve.contains("queries::get_by_ref"),
        "resolve must still read noetl.result_store FIRST — the fallback is\n\
         belt-and-suspenders, not a replacement. Rows written before the\n\
         dual-write was retired remain authoritative for their refs."
    );

    // Order is the whole point: consulting the tier after returning is the same
    // as not consulting it.
    let store_read = resolve.find("queries::get_by_ref").unwrap();
    let tier_read = resolve.find("resolve_legacy_from_tier").unwrap();
    assert!(
        store_read < tier_read,
        "the tier fallback runs before the store read; it must run only on a\n\
         MISS, so a stored row is never shadowed by a tier object."
    );
}

#[test]
fn the_fallback_cannot_turn_a_missing_result_into_an_error() {
    // The fallback is additive. If its own query fails — the object table is
    // absent on an older deployment, say — every caller must still get the
    // not-found it got before, not an error. Two of the six read sites are on
    // the event-read path, where propagating an error would fail a request that
    // used to succeed with a bare reference.
    let src = production_source(RESULT_STORE_SERVICE);
    let body_start = src
        .find("pub async fn resolve(&self, noetl_ref: &NoetlRef)")
        .unwrap();
    let body_end = src[body_start..]
        .find("pub async fn resolve_store_only")
        .map(|i| body_start + i)
        .unwrap();
    let resolve = &src[body_start..body_end];
    let err_arm_at = resolve
        .find("Err(e) =>")
        .expect("resolve no longer handles the tier fallback's own error case");
    let err_arm = &resolve[err_arm_at..];
    assert!(
        err_arm.contains("Ok(None)"),
        "a failing tier fallback must degrade to not-found, not propagate.\n\
         Found:\n{}",
        &err_arm[..err_arm.len().min(400)]
    );
}

#[test]
fn every_result_store_read_site_goes_through_the_fallback() {
    // THE LESSON FROM THE FIRST ATTEMPT AT THIS FIX. It wired the fallback into
    // `handlers::result_store::resolve_ref` only. That handler is ONE of six
    // read sites, and it is not the one that feeds a parent step: an
    // over-budget child result reaches the parent through
    // `services::execution`'s status view and `hydrate_result_references`,
    // both of which call the service directly. Measured in kind on 2026-09-16 —
    // the endpoint logged "legacy store miss served from the #104 tier" four
    // times while the parent step still received 0 items.
    //
    // So the guard is not "the handler calls the fallback" but "no read site
    // bypasses it".
    for (label, src) in [
        ("handlers/result_store.rs", RESULT_STORE_HANDLER),
        ("handlers/events.rs", EVENTS_HANDLER),
        ("services/execution.rs", EXECUTION_SERVICE),
    ] {
        let prod = production_source(src);
        assert!(
            !prod.contains("resolve_store_only"),
            "{label} calls `resolve_store_only`, which deliberately skips the\n\
             #104 tier fallback. A read path that wants the RESULT (not the\n\
             legacy row's existence) must call `resolve`."
        );
    }
}

#[test]
fn the_fallback_is_wired_from_the_service_down_to_a_query() {
    // Reachability. This class of bug is "implemented but unreachable", and the
    // three previous fixes for it were all reachable-looking code that nothing
    // called on the path that mattered.
    let service = production_source(RESULT_STORE_SERVICE);
    assert!(
        service.contains("pub async fn resolve_legacy_from_tier"),
        "the tier-fallback service method does not exist."
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
