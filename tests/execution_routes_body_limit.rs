//! adiona/frontend#122 — the execution routes accept bodies over axum's 2 MB
//! default.
//!
//! A parked playbook step whose child returned a large result (Duffel flight
//! offers ~1.2 MB, HotelBeds hotels ~0.8 MB) produced an `__orchestrate__` drive
//! `call.done` over 2 MB. `/api/events` answered 413 and the execution stayed
//! RUNNING forever. Two proofs:
//!   1. behavioural: a Router carrying the same layer accepts a 5 MB JSON body
//!      that the default rejects with 413;
//!   2. structural: `main.rs` attaches the layer to `execution_routes` after the
//!      advance route is added and before `with_state`.

const MAIN: &str = include_str!("../src/main.rs");

use axum::{
    body::Body,
    extract::DefaultBodyLimit,
    http::{Request, StatusCode},
    routing::post,
    Json, Router,
};
use noetl_server::handlers::events::EXECUTION_ROUTES_BODY_LIMIT_BYTES;
use tower::ServiceExt;

async fn echo_len(Json(v): Json<serde_json::Value>) -> String {
    v.to_string().len().to_string()
}

fn body_of(bytes: usize) -> String {
    format!("{{\"blob\":\"{}\"}}", "x".repeat(bytes))
}

async fn post_to(app: Router, body: String) -> StatusCode {
    app.oneshot(
        Request::post("/api/events")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap(),
    )
    .await
    .unwrap()
    .status()
}

#[tokio::test]
async fn default_limit_rejects_a_5mb_event_and_the_execution_limit_accepts_it() {
    let five_mb = body_of(5 * 1024 * 1024);

    // Control: without the layer, axum's 2 MB default answers 413 — the prod
    // failure in adiona/frontend#122.
    let default_app = Router::new().route("/api/events", post(echo_len));
    assert_eq!(
        post_to(default_app, five_mb.clone()).await,
        StatusCode::PAYLOAD_TOO_LARGE
    );

    let app = Router::new()
        .route("/api/events", post(echo_len))
        .layer(DefaultBodyLimit::max(EXECUTION_ROUTES_BODY_LIMIT_BYTES));
    assert_eq!(post_to(app, five_mb).await, StatusCode::OK);
}

#[test]
fn limit_matches_the_result_store_cap() {
    assert_eq!(EXECUTION_ROUTES_BODY_LIMIT_BYTES, 64 * 1024 * 1024);
}

#[test]
fn main_layers_the_limit_on_execution_routes_after_advance() {
    let advance = MAIN
        .find("\"/api/executions/{execution_id}/advance\"")
        .expect("advance route present");
    let layer = MAIN
        .find("handlers::events::EXECUTION_ROUTES_BODY_LIMIT_BYTES")
        .expect("execution routes carry the body limit");
    let with_state = MAIN[layer..]
        .find(".with_state(state.clone())")
        .map(|i| i + layer)
        .expect("with_state after the layer");
    assert!(
        advance < layer,
        "layer must come after the advance route so it covers it"
    );
    // The layer and with_state belong to the same `let execution_routes` binding.
    let binding = MAIN[..layer]
        .rfind("let execution_routes = execution_routes")
        .expect("layer sits on the execution_routes binding");
    assert!(binding > advance && with_state > layer);
    // `/api/events` itself is registered on execution_routes.
    let events = MAIN.find(".route(\"/api/events\", post(handlers::handle_event))");
    let exec_def = MAIN.find("let execution_routes = Router::new()");
    assert!(matches!((exec_def, events), (Some(d), Some(e)) if d < e && e < advance));
}
