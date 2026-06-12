//! Gateway push-ingress config endpoint (`GET /api/internal/ingress/{listener}`).
//!
//! Phase 3 of the subscription/listener RFC
//! ([noetl/ai-meta#90](https://github.com/noetl/ai-meta/issues/90), RFC §6).
//!
//! ### Why this exists
//!
//! The gateway terminates untrusted inbound push/webhook traffic and is the
//! component that **verifies** a delivery (RFC §6: "verification lives there").
//! To do that it needs the subscription's verify scheme + the decrypted verify
//! secret — but the gateway holds **no** database connection and never reads
//! domain data directly (`agents/rules/data-access-boundary.md`).  So the
//! server exposes this single internal endpoint: the gateway calls it (gated by
//! `RequireInternalApiToken`, the same service-account bearer the
//! `/api/internal/*` family uses), and the server:
//!
//! 1. resolves the `kind: Subscription` whose `spec.ingress.gateway_path`
//!    trailing segment matches the requested `listener`,
//! 2. resolves the verify-secret **alias** through the Secrets Wallet
//!    (`CredentialService`) — never a gateway env var (RFC §6),
//! 3. idempotently registers the subscription so per-delivery executions get a
//!    parent-execution lineage + lifecycle events (Mode C parity with Mode B),
//! 4. returns the verify + dispatch + directive config the gateway needs to
//!    verify-then-forward.
//!
//! The decrypted secret crosses the internal boundary to the gateway because
//! HMAC/bearer verification genuinely needs the key material at the edge; the
//! endpoint is privileged (service-account-gated) and the value never reaches
//! a user-facing response.
//!
//! ### Security ordering (RFC §7.5)
//!
//! This endpoint returns *config*, including the directive allowlist
//! (`spec.headers`) verbatim.  The gateway parses + applies those directives
//! **only after** it verifies the request (auth-gated directive trust).  The
//! server does not apply directives here — it only hands the gateway the
//! allowlist to run post-verification.

use axum::{
    extract::{Path, State},
    Json,
};
use serde::Serialize;
use serde_json::Value;

use crate::error::{AppError, AppResult};
use crate::handlers::internal::RequireInternalApiToken;
use crate::services::CredentialService;
use crate::state::AppState;

/// Route state: the control-plane state (catalog lookup + register) plus the
/// credential service (Wallet secret resolution).
#[derive(Clone)]
pub struct IngressDeps {
    pub state: AppState,
    pub credentials: CredentialService,
}

/// The verify config the gateway needs to authenticate a delivery (RFC §6).
#[derive(Debug, Serialize)]
pub struct VerifyConfig {
    /// `hmac_sha256` | `bearer` | `pubsub_oidc`.
    #[serde(rename = "type")]
    pub verify_type: String,
    /// HMAC signature header name (hmac_sha256), or the bearer header
    /// (defaults to `authorization` when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    /// Decrypted secret (HMAC signing key / expected bearer token).  Absent
    /// for `pubsub_oidc` (Google JWKS is public; no Wallet secret).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// Expected OIDC audience (`pubsub_oidc`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    /// Expected Google push service-account email (`pubsub_oidc`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_account: Option<String>,
}

/// The dispatch config the gateway applies per delivery (mirrors the worker's
/// Mode-B `dispatch` block).
#[derive(Debug, Serialize)]
pub struct DispatchConfig {
    pub playbook: String,
    pub payload_from: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_pool: Option<String>,
}

/// The full ingress config response.
#[derive(Debug, Serialize)]
pub struct IngressConfig {
    pub listener: String,
    pub catalog_path: String,
    /// Source backend (`webhook` | `pubsub` | ...) — tells the gateway whether
    /// to unwrap a Pub/Sub push envelope (attributes channel) or treat the body
    /// as a generic webhook payload (HTTP-header channel).
    pub source: String,
    /// Registered subscription id — the parent execution for per-delivery runs.
    pub subscription_id: String,
    pub verify: VerifyConfig,
    pub dispatch: DispatchConfig,
    /// The raw `spec.headers` directive allowlist (JSON), or null when the
    /// subscription declares none.  The gateway parses it with its own
    /// directive engine and applies it **only after** verification.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directives: Option<Value>,
}

/// `GET /api/internal/ingress/{listener}` — resolve the push-ingress config for
/// the gateway.  Gated by `RequireInternalApiToken`.
pub async fn get_ingress_config(
    _auth: RequireInternalApiToken,
    State(deps): State<IngressDeps>,
    Path(listener): Path<String>,
) -> AppResult<Json<IngressConfig>> {
    let span = tracing::info_span!("gateway.ingress.config", listener = %listener);
    let _g = span.enter();

    // Find the subscription whose ingress.gateway_path matches this listener.
    let (catalog_path, spec) = resolve_subscription_by_listener(&deps.state, &listener)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!("No push subscription for ingress listener '{}'", listener))
        })?;

    let ingress = spec
        .get("ingress")
        .ok_or_else(|| AppError::Validation("subscription has no 'ingress' block".into()))?;
    let verify_block = ingress
        .get("verify")
        .ok_or_else(|| AppError::Validation("subscription ingress has no 'verify' block".into()))?;

    let verify_type = verify_block
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::Validation("ingress.verify.type missing".into()))?
        .to_string();

    let header = verify_block
        .get("header")
        .and_then(|v| v.as_str())
        .map(|s| s.to_ascii_lowercase());
    let audience = verify_block
        .get("audience")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let service_account = verify_block
        .get("service_account")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    // Resolve the verify secret from the Wallet for the schemes that need it.
    let secret = match verify_type.as_str() {
        "hmac_sha256" | "bearer" => {
            let alias = verify_block
                .get("secret")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    AppError::Validation(format!(
                        "ingress.verify.secret (Wallet alias) is required for '{}'",
                        verify_type
                    ))
                })?;
            Some(resolve_secret_alias(&deps.credentials, alias).await?)
        }
        "pubsub_oidc" => None,
        other => {
            return Err(AppError::Validation(format!(
                "unsupported ingress.verify.type '{}'",
                other
            )))
        }
    };

    // dispatch block.
    let dispatch_block = spec
        .get("dispatch")
        .ok_or_else(|| AppError::Validation("subscription has no 'dispatch' block".into()))?;
    let playbook = dispatch_block
        .get("playbook")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::Validation("dispatch.playbook missing".into()))?
        .to_string();
    let payload_from = dispatch_block
        .get("payload_from")
        .and_then(|v| v.as_str())
        .unwrap_or("message.json")
        .to_string();
    let execution_pool = dispatch_block
        .get("execution_pool")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    // The directive allowlist (raw) — gateway applies it post-verification.
    let directives = spec
        .get("headers")
        .map(|h| serde_json::to_value(h).unwrap_or(Value::Null))
        .filter(|v| !v.is_null());

    // Idempotently register so per-delivery runs get parent lineage + lifecycle.
    let registered =
        crate::handlers::subscription::ensure_registered(&deps.state, &catalog_path).await?;

    tracing::info!(
        listener = %listener,
        catalog_path = %catalog_path,
        subscription_id = %registered.subscription_id,
        verify_type = %verify_type,
        "Resolved push-ingress config for gateway"
    );

    let source = spec
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("webhook")
        .to_string();

    Ok(Json(IngressConfig {
        listener,
        catalog_path,
        source,
        subscription_id: registered.subscription_id,
        verify: VerifyConfig {
            verify_type,
            header,
            secret,
            audience,
            service_account,
        },
        dispatch: DispatchConfig {
            playbook,
            payload_from,
            execution_pool,
        },
        directives,
    }))
}

/// Scan `kind: subscription` catalog entries (latest version per path) for the
/// one whose `spec.ingress.gateway_path` trailing segment equals `listener`.
/// Subscriptions are low-cardinality, so an O(n) scan is fine; the gateway
/// caches the resolved config so this runs once per listener per cache window.
async fn resolve_subscription_by_listener(
    state: &AppState,
    listener: &str,
) -> AppResult<Option<(String, serde_yaml::Value)>> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        r#"
        SELECT DISTINCT ON (path) path, content
        FROM noetl.catalog
        WHERE LOWER(kind) = 'subscription'
        ORDER BY path, version DESC
        "#,
    )
    .fetch_all(state.pools.cluster())
    .await?;

    for (path, content) in rows {
        let Ok(spec_doc) = serde_yaml::from_str::<serde_yaml::Value>(&content) else {
            continue;
        };
        let Some(spec) = spec_doc.get("spec") else {
            continue;
        };
        let gateway_path = spec
            .get("ingress")
            .and_then(|i| i.get("gateway_path"))
            .and_then(|v| v.as_str());
        let Some(gp) = gateway_path else { continue };
        if listener_matches(gp, listener) {
            return Ok(Some((path, spec.clone())));
        }
    }
    Ok(None)
}

/// True when `gateway_path`'s trailing segment equals `listener`, or the whole
/// path equals `/ingress/{listener}` or `/{listener}`.
fn listener_matches(gateway_path: &str, listener: &str) -> bool {
    let trimmed = gateway_path.trim_end_matches('/');
    trimmed
        .rsplit('/')
        .next()
        .map(|seg| seg == listener)
        .unwrap_or(false)
}

/// Resolve a verify-secret alias through the Wallet and extract the secret
/// string.  The credential's decrypted `data` is expected to carry the secret
/// under `secret` (preferred), `token`, `value`, `key`, or `password`, or be a
/// bare string.
async fn resolve_secret_alias(credentials: &CredentialService, alias: &str) -> AppResult<String> {
    let resolved = credentials.get(alias, true, None).await?;
    let data = resolved.data.ok_or_else(|| {
        AppError::Internal(format!("verify secret alias '{}' resolved with no data", alias))
    })?;

    let value = match &data {
        Value::String(s) => Some(s.clone()),
        Value::Object(map) => ["secret", "token", "value", "key", "password"]
            .iter()
            .find_map(|k| map.get(*k).and_then(|v| v.as_str()).map(str::to_string)),
        _ => None,
    };

    value.ok_or_else(|| {
        AppError::Validation(format!(
            "verify secret alias '{}' did not yield a string secret (expected a bare string or a \
             'secret'/'token'/'value'/'key'/'password' field)",
            alias
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listener_match_trailing_segment() {
        assert!(listener_matches("/ingress/stripe", "stripe"));
        assert!(listener_matches("/ingress/stripe/", "stripe"));
        assert!(listener_matches("/stripe", "stripe"));
        assert!(listener_matches("/api/hooks/billing", "billing"));
    }

    #[test]
    fn listener_no_false_match() {
        assert!(!listener_matches("/ingress/stripe", "billing"));
        assert!(!listener_matches("/ingress/stripe-events", "stripe"));
    }
}
