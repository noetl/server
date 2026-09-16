//! Readiness must exercise a real read path — noetl/server#443.
//!
//! ⚠⚠ `/api/health` returned **200 for the entire six minutes** that
//! `/api/catalog/list` was returning HTTP 500 in production. The rollout could
//! not halt itself, because nothing it probed was broken. On a single-replica
//! StatefulSet that means full user exposure with no staged-rollout safety net.
//!
//! A probe is only a safety mechanism if it fails when the thing users call
//! fails. This file pins the three properties that make it one.

const MAIN_RS: &str = include_str!("../src/main.rs");
const HEALTH_RS: &str = include_str!("../src/handlers/health.rs");

/// Source with comment lines removed — this crate documents the hazard in prose,
/// and a matcher that reads prose as code reports the warning as the defect.
fn code_only(src: &str) -> String {
    src.lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("//") && !t.starts_with("///") && !t.starts_with('*')
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// ⭐ The route must exist and reach the readiness handler.
#[test]
fn the_readiness_route_is_wired() {
    let code = code_only(MAIN_RS);
    assert!(
        code.contains("/api/health/ready"),
        "no readiness route — a handler nothing routes to guards nothing \
         (the noetl/ai-meta#343 failure mode)"
    );
    assert!(
        code.contains("handlers::health::readiness"),
        "the readiness route does not point at the readiness handler"
    );
}

/// ⭐ It must query the catalog READ path, not merely report liveness.
#[test]
fn readiness_actually_queries_the_path_that_broke() {
    let code = code_only(HEALTH_RS);
    let from = code
        .find("pub async fn readiness")
        .expect("the readiness handler");
    let body = &code[from..];
    let body = &body[..body.find("\n}").map(|i| i + 2).unwrap_or(body.len())];

    assert!(
        body.contains("list_catalog_entries("),
        "readiness does not call `list_catalog_entries` — it would pass while \
         /api/catalog/list 500s, which is exactly what happened:\n{body}"
    );
    assert!(
        body.contains("include_content: false"),
        "readiness must query with bodies OFF: that is the shape that selects a \
         typed NULL, and the decode of that NULL is the failure being guarded. \
         With bodies ON the query cannot reproduce it:\n{body}"
    );
    assert!(
        body.contains("limit: Some(1)"),
        "readiness must be bounded to one row — it runs every 10s and must never \
         become a reason the server is slow:\n{body}"
    );
}

/// ⚠ It must report NOT READY, not a server error. An unready pod stops taking
/// traffic and halts the rollout; a crashing one restarts into the same bad
/// image and keeps trying.
#[test]
fn a_failed_read_reports_unready_rather_than_crashing() {
    let code = code_only(HEALTH_RS);
    let from = code.find("pub async fn readiness").expect("the handler");
    let body = &code[from..];
    let body = &body[..body.find("\n}").map(|i| i + 2).unwrap_or(body.len())];

    assert!(
        body.contains("SERVICE_UNAVAILABLE"),
        "a failed catalog read must map to 503 SERVICE_UNAVAILABLE:\n{body}"
    );
    assert!(
        !body.contains("unwrap()") && !body.contains("expect("),
        "readiness must not panic on a failed read — a panicking probe takes the \
         process down instead of marking it unready:\n{body}"
    );
}

/// Positive control: the matchers must be able to fail, and must not fire on
/// prose describing what they look for.
#[test]
fn the_matchers_can_actually_fail() {
    let gutted =
        "pub async fn readiness() -> Json<Value> {\n    Json(json!({\"status\":\"ok\"}))\n}\n";
    assert!(
        !code_only(gutted).contains("list_catalog_entries("),
        "a liveness-only handler must NOT satisfy the query matcher"
    );
    let discussed = "/// readiness calls list_catalog_entries( here\nfn f() {}";
    assert!(
        !code_only(discussed).contains("list_catalog_entries("),
        "the matcher counts comments as code"
    );
}
