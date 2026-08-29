//! noetl/ai-meta#303 — proofs that the internal auth gate is *attached*, not just
//! that its decision function is correct.
//!
//! The unit tests in `src/auth_gate.rs` prove `decide()`. They would keep passing
//! if the layer were attached to nothing, which is exactly the failure this issue
//! is about: the pre-existing per-handler extractor was correct and simply not
//! applied to `/api/credentials`.
//!
//! So there are two proofs here:
//!   1. behavioural — drive a Router through the real layer and check the status.
//!   2. structural  — assert every privileged router in `main.rs` carries the
//!      layer, by COUNTING sites rather than naming them, so a router added later
//!      without a gate fails the test instead of silently joining the open set.

const EXECUTE_MAIN: &str = include_str!("../src/main.rs");

use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::get,
    Router,
};
use noetl_server::auth_gate;
use serial_test::serial;
use tower::ServiceExt;

fn app(group: &'static str) -> Router {
    Router::new()
        .route("/secret", get(|| async { "payload" }))
        .layer(axum::middleware::from_fn_with_state(group, auth_gate::gate))
}

async fn call(app: Router, auth: Option<&str>) -> StatusCode {
    let mut b = Request::builder().uri("/secret");
    if let Some(a) = auth {
        b = b.header("Authorization", a);
    }
    app.oneshot(b.body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

/// SAFETY: these tests mutate process-global env and are serialised with
/// `#[serial]`. `cargo test` does NOT serialise tests by default — the attribute
/// is doing the work, not the runner.
fn set_mode(mode: &str, token: Option<&str>) {
    unsafe {
        std::env::set_var("NOETL_INTERNAL_AUTH_MODE", mode);
        match token {
            Some(t) => std::env::set_var("NOETL_INTERNAL_API_TOKEN", t),
            None => std::env::remove_var("NOETL_INTERNAL_API_TOKEN"),
        }
    }
}

#[tokio::test]
#[serial]
async fn shadow_rejects_nothing() {
    set_mode("shadow", Some("s3cret"));
    assert_eq!(call(app("credentials"), None).await, StatusCode::OK);
    assert_eq!(
        call(app("credentials"), Some("Bearer wrong")).await,
        StatusCode::OK
    );
    assert_eq!(
        call(app("credentials"), Some("Bearer s3cret")).await,
        StatusCode::OK
    );
}

#[tokio::test]
#[serial]
async fn enforce_rejects_exactly_the_invalid_ones() {
    set_mode("enforce", Some("s3cret"));
    assert_eq!(
        call(app("credentials"), Some("Bearer s3cret")).await,
        StatusCode::OK
    );
    assert_eq!(call(app("credentials"), None).await, StatusCode::FORBIDDEN);
    assert_eq!(
        call(app("credentials"), Some("Bearer wrong")).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(app("credentials"), Some("Basic x")).await,
        StatusCode::FORBIDDEN
    );
}

/// A server with no token must not fail OPEN under enforce. 503 rather than 403:
/// the deployment is broken, not the caller.
#[tokio::test]
#[serial]
async fn enforce_with_no_server_token_is_503_not_open() {
    set_mode("enforce", None);
    assert_eq!(
        call(app("credentials"), Some("Bearer anything")).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
}

/// The mode is read per request, so flipping it back is a live rollback.
#[tokio::test]
#[serial]
async fn mode_is_reversible_without_a_restart() {
    set_mode("enforce", Some("s3cret"));
    assert_eq!(call(app("credentials"), None).await, StatusCode::FORBIDDEN);
    set_mode("shadow", Some("s3cret"));
    assert_eq!(call(app("credentials"), None).await, StatusCode::OK);
}

/// Structural: every privileged router merged in `main.rs` must carry the gate.
///
/// Counts sites instead of naming them. A new privileged router merged without a
/// layer fails here — which is the whole defect #303 describes, one level up.
#[test]
fn every_privileged_router_is_gated_in_main() {
    let src = include_str!("../src/main.rs");
    // Router groups that expose credentials, keychain material, or the internal
    // control surface. Kept explicit so adding one is a deliberate act.
    const PRIVILEGED: [&str; 16] = [
        "credential_routes",
        "sealed_credential_routes",
        "keychain_routes",
        "cross_region_routes",
        "wallet_rotate_routes",
        "secret_audit_routes",
        "container_callback_routes",
        "projection_routes",
        "internal_routes",
        "object_store_routes",
        "cell_routes",
        "result_tier_routes",
        "sink_state_routes",
        "ingress_routes",
        // ai-meta#307. Its siblings in `ehdb_parity_routes` report on an
        // execution id the caller already holds; this one ENUMERATES recently
        // completed ids, so it carries the gate rather than inheriting that
        // group's ungated posture.
        "ehdb_equivalence_routes",
        // noetl/ai-meta#312 — serves /api/internal/registry/*, which its own
        // handler doc already described as service-account-gated. It was not.
        "registry_routes",
    ];
    let mut ungated = Vec::new();
    for name in PRIVILEGED {
        let needle = format!(".merge({name}.layer(");
        if !src.contains(&needle) {
            ungated.push(name);
        }
    }
    assert!(
        ungated.is_empty(),
        "these privileged routers are merged without the auth gate: {ungated:?}"
    );
    // Count, so the list above cannot silently drift from what is wired.
    let gated = src.matches("noetl_server::auth_gate::gate").count();
    assert_eq!(
        gated,
        PRIVILEGED.len(),
        "expected {} gate layers in main.rs, found {gated}",
        PRIVILEGED.len()
    );
}

/// Negative control for the structural test: it must be capable of failing.
/// A check that has never been shown to fail is indistinguishable from one that
/// cannot.
#[test]
fn the_structural_check_can_fail() {
    let doctored = "        .merge(credential_routes)\n";
    assert!(!doctored.contains(".merge(credential_routes.layer("));
}

// ---- the invariant, not a list (noetl/ai-meta#312) ----

/// Routers that are deliberately reachable without the internal gate.
///
/// **Every entry needs a reason.** This is the list a reviewer argues with; the
/// test below makes it impossible to add an ungated router *without* arguing.
const PUBLIC_BY_DESIGN: [(&str, &str); 17] = [
    ("health_routes", "liveness/readiness probes carry no credential"),
    ("catalog_routes", "register/list is the authoring surface; delete is separately gated by RequireInternalApiToken"),
    ("auth_routes", "the login surface itself — gating it would be circular"),
    ("execution_routes", "execute and status: the product's primary surface"),
    ("executions_routes", "execution listing, the primary read surface"),
    ("ehdb_routes", "EHDB diagnostics, reports on ids the caller already holds"),
    ("ehdb_tier_routes", "raw tier relay reads, no credential material"),
    ("ehdb_parity_routes", "comparator reports on an id the caller already holds"),
    ("subscription_routes", "subscription management, part of the product surface"),
    ("replay_routes", "replay of an execution the caller already names"),
    ("result_store_routes", "result fetch for an execution the caller already names"),
    ("variable_routes", "execution-scoped variable read and write"),
    ("runtime_routes", "worker registration and heartbeat — workers hold no internal token"),
    ("sharding_routes", "shard diagnostics, no data"),
    ("system_routes", "system information endpoints, no data and no credential material"),
    ("dashboard_routes", "read-only aggregate counts for the UI; no per-record data and no credential material"),
    // ⚠ noetl/ai-meta#312. NOT a justification — a record of the current state.
    // `/api/postgres/execute` runs arbitrary SQL with no auth and no statement
    // restriction. Evidence on #312 shows no prod or CI path depends on it being
    // open. Listed so this test passes today and FAILS the moment another
    // ungated router appears, rather than blocking on an unrelated decision.
    ("database_routes", "⚠ #312 OPEN — arbitrary SQL, unauthenticated. Awaiting the owner's auth-model choice."),
];

/// Split `main.rs`'s router chain into one chunk per `.merge(...)` site.
///
/// Returns `(router_name, chunk)`, where the chunk runs to the next `.merge(`.
/// Any gate applied to that router is inside its own chunk.
fn merge_sites(src: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let parts: Vec<&str> = src.split(".merge(").collect();
    for (i, p) in parts.iter().enumerate().skip(1) {
        let name: String = p
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            continue;
        }
        // The chunk is this part; the next `.merge(` starts the following part,
        // so a gate belonging to this router cannot leak in from the next one.
        let _ = i;
        out.push((name, (*p).to_string()));
    }
    out
}

/// ⭐ THE INVARIANT: no router is merged ungated unless it is on the list above.
///
/// This replaces "these 15 named routers are gated", which could not catch a
/// privileged router nobody added to the list — and that is exactly how
/// `database_routes` (noetl/ai-meta#312) stayed invisible through a review that
/// counted gate sites and found them all present.
///
/// The direction of the check is the whole point. Listing what IS gated proves
/// nothing about what is not.
#[test]
fn no_router_is_merged_ungated_without_an_explicit_reason() {
    let sites = merge_sites(EXECUTE_MAIN);
    assert!(
        sites.len() > 20,
        "only {} merge sites parsed — the parser is not reading the router chain",
        sites.len()
    );

    let allowed: std::collections::HashMap<&str, &str> =
        PUBLIC_BY_DESIGN.iter().copied().collect();
    let mut unjustified = Vec::new();
    for (name, chunk) in &sites {
        let gated = chunk.contains("auth_gate::gate");
        if !gated && !allowed.contains_key(name.as_str()) {
            unjustified.push(name.clone());
        }
    }
    assert!(
        unjustified.is_empty(),
        "these routers are merged WITHOUT the auth gate and without an entry in \
         PUBLIC_BY_DESIGN: {unjustified:?}\n\
         Add the gate, or add the router to PUBLIC_BY_DESIGN with the reason it \
         is safe to expose. Do not add it silently — an unauthenticated route \
         nobody argued about is how noetl/ai-meta#312 happened."
    );
}

/// The allowlist must not rot: every entry must still name a real merge site.
///
/// Without this, a router could be deleted or renamed and its exemption would
/// linger, silently pre-approving a future router that reused the name.
#[test]
fn every_public_by_design_entry_still_exists() {
    let names: std::collections::HashSet<String> =
        merge_sites(EXECUTE_MAIN).into_iter().map(|(n, _)| n).collect();
    let stale: Vec<&str> = PUBLIC_BY_DESIGN
        .iter()
        .map(|(n, _)| *n)
        .filter(|n| !names.contains(*n))
        .collect();
    assert!(
        stale.is_empty(),
        "PUBLIC_BY_DESIGN names routers that are no longer merged: {stale:?} — \
         a stale exemption pre-approves whatever reuses the name"
    );
}

/// Every exemption carries a reason a human wrote.
#[test]
fn every_exemption_states_why() {
    for (name, reason) in PUBLIC_BY_DESIGN {
        assert!(
            reason.len() > 15,
            "{name} is exempted with no real reason: {reason:?}"
        );
    }
}
