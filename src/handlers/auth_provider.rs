//! Sessions for gateway auth providers (spec
//! `noetl/ai-meta specs/active/2026-10-21-gateway-auth-providers`).
//!
//! An auth provider is a playbook the gateway runs to prove who a caller is
//! (an emailed one-time code, a Google or Apple sign-in, …). The playbook
//! returns only a verified identity; the gateway then calls
//! `POST /api/auth/provider-session` here, and this handler turns the identity
//! into a user + session in the auth database. The session token goes back to
//! the gateway in this response only, never through a playbook result.
//!
//! ## Who may call it
//!
//! Minting a session for an arbitrary identity is the most privileged thing
//! the auth API does, so it is **not** behind `NOETL_INTERNAL_API_TOKEN` (the
//! workers hold that one, and workers run tenant playbooks). It needs its own
//! key, `NOETL_AUTH_MINT_KEY` (≥ 32 bytes), held by the gateway and the server
//! only, sent as `X-NoETL-Auth-Mint`. With the key unset the route answers 404,
//! so a server without auth providers exposes nothing new.
//!
//! ## Identity
//!
//! The user row is keyed `ap:<provider>|<subject>` in `auth.users.auth0_id`.
//! Auth0 subjects are `<strategy>|<id>` with no `:`, so the two can't collide,
//! and identity binding (`uid` / `user_uid` = the session's `auth0_id`) works
//! unchanged. `auth.users.email` is unique: a sign-in whose email already
//! belongs to another account is refused (`email_in_use`) rather than merged,
//! so verifying a mailbox can never take over an existing account. Provider
//! users start with no roles; roles stay manual in `auth.user_roles`.
//!
//! ## Logout
//!
//! `POST /api/auth/logout` deactivates one session by token (possession of the
//! token is the authority, as for validate).

use axum::{extract::State, http::HeaderMap, http::StatusCode, response::IntoResponse, Json};
use serde::Deserialize;
use subtle::ConstantTimeEq;

use crate::handlers::auth::{resolve_auth_pool, session_user_json, LoginResponse};
use crate::services::CredentialService;

const KEY_ENV: &str = "NOETL_AUTH_MINT_KEY";
const KEY_HEADER: &str = "x-noetl-auth-mint";
const MIN_KEY_LEN: usize = 32;
/// Longest session a provider may ask for: 90 days.
const MAX_SESSION_HOURS: i64 = 24 * 90;

fn default_pg_auth() -> String {
    "pg_auth".to_string()
}

/// `POST /api/auth/provider-session` body (sent by the gateway).
#[derive(Debug, Deserialize)]
pub struct ProviderSessionRequest {
    /// The gateway's provider name (config key), e.g. `msr-email`.
    pub provider: String,
    /// The provider's stable id for the person.
    pub subject: String,
    pub email: String,
    #[serde(default)]
    pub email_verified: bool,
    #[serde(default)]
    pub name: Option<String>,
    pub session_hours: i64,
    #[serde(default)]
    pub client_ip: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default = "default_pg_auth")]
    pub credential: String,
}

/// Why a request is refused before touching the database.
#[derive(Debug, PartialEq)]
pub(crate) enum Invalid {
    Provider,
    Subject,
    Email,
    EmailNotVerified,
    SessionHours,
}

impl Invalid {
    fn message(&self) -> &'static str {
        match self {
            Invalid::Provider => "provider must be 2-32 chars of a-z, 0-9 and '-', starting with a letter or digit",
            Invalid::Subject => "subject must be 1-200 printable characters",
            Invalid::Email => "a valid email address is required",
            Invalid::EmailNotVerified => "the provider did not verify the email address",
            Invalid::SessionHours => "session_hours must be 1-2160",
        }
    }
}

/// The `auth.users.auth0_id` key for a provider identity, after checking the
/// request. Pure, so it is unit-tested without a database.
pub(crate) fn identity_key(req: &ProviderSessionRequest) -> Result<String, Invalid> {
    let p = req.provider.as_bytes();
    let provider_ok = (2..=32).contains(&p.len())
        && p[0].is_ascii_alphanumeric()
        && p.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-');
    if !provider_ok {
        return Err(Invalid::Provider);
    }
    let s = req.subject.trim();
    if s.is_empty() || s.chars().count() > 200 || s.chars().any(char::is_control) {
        return Err(Invalid::Subject);
    }
    let e = req.email.trim();
    let at = e.find('@');
    let email_ok = e.len() <= 255
        && !e.chars().any(|c| c.is_whitespace() || c.is_control())
        && at.is_some_and(|i| i > 0 && e[i + 1..].contains('.') && !e[i + 1..].contains('@'));
    if !email_ok {
        return Err(Invalid::Email);
    }
    if !req.email_verified {
        return Err(Invalid::EmailNotVerified);
    }
    if !(1..=MAX_SESSION_HOURS).contains(&req.session_hours) {
        return Err(Invalid::SessionHours);
    }
    Ok(format!("ap:{}|{}", req.provider, s))
}

/// The mint key check. `None` = the route is off (key unset or too short).
pub(crate) fn key_ok(configured: Option<&str>, presented: Option<&str>) -> Option<bool> {
    let key = configured.filter(|k| k.len() >= MIN_KEY_LEN)?;
    Some(presented.is_some_and(|p| p.as_bytes().ct_eq(key.as_bytes()).into()))
}

fn session_token() -> String {
    use rand::RngCore;
    let mut b = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut b);
    hex::encode(b)
}

fn refused(error: &str, message: &str) -> Json<LoginResponse> {
    Json(LoginResponse {
        status: "error".into(),
        data: serde_json::json!({ "error": error, "message": message }),
    })
}

/// Create (or reuse) the provider identity's user and a new session.
pub async fn provider_session(
    State(cred): State<CredentialService>,
    headers: HeaderMap,
    Json(req): Json<ProviderSessionRequest>,
) -> axum::response::Response {
    let configured = std::env::var(KEY_ENV).ok();
    let presented = headers.get(KEY_HEADER).and_then(|v| v.to_str().ok());
    match key_ok(configured.as_deref(), presented) {
        None => return StatusCode::NOT_FOUND.into_response(),
        Some(false) => return StatusCode::FORBIDDEN.into_response(),
        Some(true) => {}
    }
    let key = match identity_key(&req) {
        Ok(k) => k,
        Err(why) => return refused("invalid_identity", why.message()).into_response(),
    };
    match create(&cred, &req, &key).await {
        Ok(resp) => resp.into_response(),
        Err(e) => {
            tracing::warn!(error = %e, provider = %req.provider, "provider session creation failed");
            refused("database_error", "Failed to create session").into_response()
        }
    }
}

async fn create(
    cred: &CredentialService,
    req: &ProviderSessionRequest,
    key: &str,
) -> crate::error::AppResult<Json<LoginResponse>> {
    let pool = resolve_auth_pool(cred, &req.credential).await?;
    let email = req.email.trim();
    let name = req.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).unwrap_or(email);
    let mut tx = pool.begin().await?;

    // Separate accounts: an email already used by another identity is refused.
    let taken: Option<(String,)> = sqlx::query_as(
        "SELECT auth0_id FROM auth.users WHERE lower(email) = lower($1) AND auth0_id <> $2 LIMIT 1",
    )
    .bind(email)
    .bind(key)
    .fetch_optional(&mut *tx)
    .await?;
    if taken.is_some() {
        return Ok(refused(
            "email_in_use",
            "This email address is already used by an account with a different sign-in method.",
        ));
    }

    let user: Option<(i32, String, Option<String>, bool)> = sqlx::query_as(
        r#"
        INSERT INTO auth.users (auth0_id, email, display_name, last_login_at)
        VALUES ($1, $2, $3, NOW())
        ON CONFLICT (auth0_id)
        DO UPDATE SET email = EXCLUDED.email, display_name = EXCLUDED.display_name, last_login_at = NOW()
        RETURNING user_id::int, email, display_name, COALESCE(is_active, true)
        "#,
    )
    .bind(key)
    .bind(email)
    .bind(name)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((user_id, email, display_name, active)) = user else {
        return Ok(refused("database_error", "Failed to create user"));
    };
    if !active {
        return Ok(refused("user_inactive", "This account is disabled."));
    }

    let token = session_token();
    let (expires_at,): (String,) = sqlx::query_as(
        r#"
        INSERT INTO auth.sessions (user_id, session_token, expires_at, ip_address, user_agent)
        VALUES ($1, $2, NOW() + make_interval(hours => $3::int), $4::inet, $5)
        RETURNING expires_at::text
        "#,
    )
    .bind(user_id as i64)
    .bind(&token)
    .bind(req.session_hours as i32)
    .bind(req.client_ip.as_deref().filter(|ip| ip.parse::<std::net::IpAddr>().is_ok()))
    .bind(req.user_agent.as_deref().map(|u| u.chars().take(500).collect::<String>()))
    .fetch_one(&mut *tx)
    .await?;

    let (roles,): (serde_json::Value,) = sqlx::query_as(
        r#"
        SELECT COALESCE(json_agg(r.role_name ORDER BY r.role_name) FILTER (WHERE r.role_id IS NOT NULL), '[]'::json)
        FROM auth.user_roles ur
        JOIN auth.roles r ON r.role_id = ur.role_id
        WHERE ur.user_id = $1 AND (ur.expires_at IS NULL OR ur.expires_at > NOW())
        "#,
    )
    .bind(user_id as i64)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    let user = session_user_json(user_id, email, display_name, roles, Some(key));
    Ok(Json(LoginResponse {
        status: "success".into(),
        data: serde_json::json!({
            "status": "authenticated",
            "session_token": token,
            "user": user,
            "expires_at": expires_at,
        }),
    }))
}

/// `POST /api/auth/logout` body.
#[derive(Debug, Deserialize)]
pub struct LogoutRequest {
    pub session_token: String,
    #[serde(default = "default_pg_auth")]
    pub credential: String,
}

/// Deactivate one session. `{status: "ok", revoked: bool}`.
pub async fn logout(
    State(cred): State<CredentialService>,
    Json(req): Json<LogoutRequest>,
) -> axum::response::Response {
    if req.session_token.trim().is_empty() || req.session_token.len() > 255 {
        return Json(serde_json::json!({ "status": "ok", "revoked": false })).into_response();
    }
    let result = async {
        let pool = resolve_auth_pool(&cred, &req.credential).await?;
        let done = sqlx::query("UPDATE auth.sessions SET is_active = false WHERE session_token = $1 AND is_active")
            .bind(req.session_token.trim())
            .execute(&pool)
            .await?;
        crate::error::AppResult::Ok(done.rows_affected() > 0)
    }
    .await;
    match result {
        Ok(revoked) => Json(serde_json::json!({ "status": "ok", "revoked": revoked })).into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "logout failed");
            Json(serde_json::json!({ "status": "error", "error": "Failed to end the session" })).into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req() -> ProviderSessionRequest {
        ProviderSessionRequest {
            provider: "msr-email".into(),
            subject: "5f2a".into(),
            email: "guest@example.org".into(),
            email_verified: true,
            name: None,
            session_hours: 720,
            client_ip: None,
            user_agent: None,
            credential: default_pg_auth(),
        }
    }

    #[test]
    fn identity_key_is_namespaced_and_cannot_look_like_an_auth0_subject() {
        let key = identity_key(&req()).unwrap();
        assert_eq!(key, "ap:msr-email|5f2a");
        // Auth0 subjects are `<strategy>|<id>` with no ':' before the '|'.
        assert!(key.split('|').next().unwrap().contains(':'));
    }

    #[test]
    fn identity_key_rejects_bad_input() {
        let bad = |f: fn(&mut ProviderSessionRequest)| {
            let mut r = req();
            f(&mut r);
            identity_key(&r).unwrap_err()
        };
        assert_eq!(bad(|r| r.provider = "X".into()), Invalid::Provider);
        assert_eq!(bad(|r| r.provider = "ap:x".into()), Invalid::Provider);
        assert_eq!(bad(|r| r.provider = "-x".into()), Invalid::Provider);
        assert_eq!(bad(|r| r.provider = "a".repeat(33)), Invalid::Provider);
        assert_eq!(bad(|r| r.subject = " ".into()), Invalid::Subject);
        assert_eq!(bad(|r| r.subject = "a\nb".into()), Invalid::Subject);
        assert_eq!(bad(|r| r.subject = "s".repeat(201)), Invalid::Subject);
        assert_eq!(bad(|r| r.email = "nope".into()), Invalid::Email);
        assert_eq!(bad(|r| r.email = "a b@example.org".into()), Invalid::Email);
        assert_eq!(bad(|r| r.email = "a@b@example.org".into()), Invalid::Email);
        assert_eq!(bad(|r| r.email_verified = false), Invalid::EmailNotVerified);
        assert_eq!(bad(|r| r.session_hours = 0), Invalid::SessionHours);
        assert_eq!(bad(|r| r.session_hours = MAX_SESSION_HOURS + 1), Invalid::SessionHours);
    }

    #[test]
    fn mint_key_gate() {
        let key = "k".repeat(32);
        assert_eq!(key_ok(None, Some("x")), None, "unset = route off");
        assert_eq!(key_ok(Some("short"), Some("short")), None, "a short key turns the route off");
        assert_eq!(key_ok(Some(&key), None), Some(false));
        assert_eq!(key_ok(Some(&key), Some("k")), Some(false));
        assert_eq!(key_ok(Some(&key), Some(&key)), Some(true));
    }

    #[test]
    fn session_tokens_are_long_and_distinct() {
        let a = session_token();
        assert_eq!(a.len(), 64);
        assert_ne!(a, session_token());
    }
}
