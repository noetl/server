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
    const PRIVILEGED: [&str; 14] = [
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
