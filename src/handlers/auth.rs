//! Synchronous authentication handlers — the drive-immune fast path.
//!
//! ## Why this module exists
//!
//! `auth0_login` / `auth0_validate_session` historically executed as a
//! **multi-hop off-server orchestration drive** on the system worker pool:
//! the gateway called `POST /api/execute`, the server dispatched the playbook,
//! the worker ran each step (postgres → python → nats → http-callback) hop by
//! hop, and each hop could fall to the server's ~8s reconcile tick under load
//! (noetl/ai-meta#130 / #156).  Two slow hops blew the gateway's hard ~15s
//! auth deadline → the recurring Muno login lockout, even though the drive
//! completed ~24-38s later.  Every prior mitigation (OOM right-sizing #163,
//! bounded WAL index #166, tail-attach #156) removed a *contributing* load
//! source but not the structural cause: **session validation is a plain DB
//! lookup that never needed to run as a deadline-gated distributed workflow.**
//!
//! These handlers run the *exact same logic* the two auth playbooks run —
//! byte-for-byte the same SQL, the same JWT-claims checks — but **synchronously
//! and in-process**.  No NATS, no worker, no off-server drive, no reconcile
//! tick, no callback.  A validate/login request touches the auth Postgres
//! directly and returns in single-digit milliseconds regardless of the drive
//! state, so a wedged/paused system-pool (NATS bounce, OOM, index churn) can no
//! longer lock users out.
//!
//! The gateway opts into this path with `NOETL_AUTH_SYNC=true`; with the flag
//! OFF the gateway keeps dispatching the playbook drive, so these routes stay
//! inert (never called) and the change is fully reversible.
//!
//! ## Faithfulness contract
//!
//! The validation/login *decisions* MUST be identical to the playbook path:
//! same valid/invalid/expired outcomes, same token/session checks, same
//! `{valid, user, expires_at}` / `{status, session_token, user, expires_at}`
//! response contract the gateway already parses.  Only the *execution shape*
//! changes (synchronous vs orchestrated).  The auth Postgres is reached through
//! the same `pg_auth` credential the playbooks use (resolved via the credential
//! store), so the connection target + privileges are identical too.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{extract::State, Json};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use tokio::sync::Mutex;

use crate::error::{AppError, AppResult};
use crate::services::CredentialService;

/// Default credential alias for the auth database (matches the auth playbooks'
/// `db_credential: pg_auth`).
fn default_pg_auth() -> String {
    "pg_auth".to_string()
}

/// Process-global cache of auth-DB connection pools, keyed by credential alias.
///
/// The auth queries run against a **separate, small** pool built from the
/// resolved `pg_auth` credential — the same host/db/user the playbook's
/// `tool: postgres, auth: pg_auth` step connects with — rather than the
/// server's own `noetl`-user pool, so privileges on the `auth.*` schema are
/// identical to the playbook path (no grant surprises).  Cached because
/// resolving + connecting on every request would defeat the point.
fn auth_pools() -> &'static Mutex<HashMap<String, PgPool>> {
    static POOLS: OnceLock<Mutex<HashMap<String, PgPool>>> = OnceLock::new();
    POOLS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolve `credential` to a cached auth-DB pool, building it on first use.
async fn resolve_auth_pool(cred: &CredentialService, credential: &str) -> AppResult<PgPool> {
    if let Some(pool) = auth_pools().lock().await.get(credential).cloned() {
        return Ok(pool);
    }

    // Cache miss — resolve the credential (decrypted) and build a small pool.
    let resp = cred.get(credential, true, None).await?;
    let data = resp.data.ok_or_else(|| {
        AppError::Internal(format!("auth credential '{credential}' returned no data"))
    })?;

    // The postgres credential shape registered in practice is
    // `{host, port, user, password, database}` (what the worker's postgres tool
    // reads); accept the `db_*`-prefixed aliases too for provisioning scripts
    // that use them.  Either way the target is the SAME auth DB the playbook's
    // `tool: postgres, auth: pg_auth` step connects to.
    let str_field = |canon: &str, alt: &str| {
        data.get(canon)
            .or_else(|| data.get(alt))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    let host = str_field("host", "db_host")
        .ok_or_else(|| AppError::Internal(format!("auth credential '{credential}' missing host")))?;
    let port: u16 = data
        .get("port")
        .or_else(|| data.get("db_port"))
        .and_then(|v| {
            v.as_u64()
                .map(|n| n as u16)
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        })
        .unwrap_or(5432);
    let user = str_field("user", "db_user")
        .ok_or_else(|| AppError::Internal(format!("auth credential '{credential}' missing user")))?;
    let password = str_field("password", "db_password").unwrap_or_default();
    let database = str_field("database", "db_name").unwrap_or_else(|| "noetl".to_string());

    let opts = PgConnectOptions::new()
        .host(&host)
        .port(port)
        .username(&user)
        .password(&password)
        .database(&database);

    let pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(opts)
        .await
        .map_err(AppError::Database)?;

    // Insert-or-keep under the lock (a concurrent first-caller may have won the
    // race; keep theirs, drop ours — a harmless one-time extra pool).
    let mut map = auth_pools().lock().await;
    let pool = map.entry(credential.to_string()).or_insert(pool).clone();
    Ok(pool)
}

// ---------------------------------------------------------------------------
// Session validation — synchronous mirror of `auth0_validate_session`.
// ---------------------------------------------------------------------------

/// `POST /api/auth/session/validate` request body.  Matches the gateway's
/// `NoetlClient::validate_session_via_api` payload.
#[derive(Debug, Deserialize)]
pub struct ValidateSessionRequest {
    pub session_token: String,
    #[serde(default = "default_pg_auth")]
    pub credential: String,
}

/// `POST /api/auth/session/validate` response.  Matches the gateway's
/// `AuthSessionValidateResponse` parser (`status`, `valid`, `user`,
/// `expires_at`, `error`).
#[derive(Debug, Serialize)]
pub struct ValidateSessionResponse {
    /// `ok` on a completed validation (valid OR invalid); `error` when the
    /// lookup itself failed (DB unreachable) — the gateway surfaces the latter
    /// as a retryable backend error rather than falsely rejecting the session.
    pub status: String,
    pub valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl ValidateSessionResponse {
    fn invalid() -> Self {
        Self { status: "ok".into(), valid: false, user: None, expires_at: None, error: None }
    }
    fn error(msg: String) -> Self {
        Self { status: "error".into(), valid: false, user: None, expires_at: None, error: Some(msg) }
    }
}

/// Validate a session token synchronously against the auth database.
///
/// Runs the identical `auth0_validate_session` SQL: look up the session joined
/// to its user, compute `session_valid` from expiry + `is_active` flags, and —
/// when valid — fetch the user's non-expired roles.  Returns the same
/// `{valid, user, expires_at}` contract the playbook callback returns.
pub async fn validate_session(
    State(cred): State<CredentialService>,
    Json(req): Json<ValidateSessionRequest>,
) -> Json<ValidateSessionResponse> {
    match validate_session_inner(&cred, &req).await {
        Ok(resp) => {
            crate::metrics::record_auth_sync(
                "validate",
                if resp.valid { "valid" } else { "invalid" },
            );
            Json(resp)
        }
        Err(e) => {
            tracing::warn!(error = %e, "auth-sync validate_session lookup failed");
            crate::metrics::record_auth_sync("validate", "error");
            Json(ValidateSessionResponse::error(e.to_string()))
        }
    }
}

async fn validate_session_inner(
    cred: &CredentialService,
    req: &ValidateSessionRequest,
) -> AppResult<ValidateSessionResponse> {
    let pool = resolve_auth_pool(cred, &req.credential).await?;

    // Step `start` from auth0_validate_session: session + user join, with the
    // exact same validity CASE (expired / session-inactive / user-inactive →
    // invalid; else valid).  Parameterised bind replaces the playbook's
    // jsonb-escaped literal — same match semantics, safer quoting.
    #[allow(clippy::type_complexity)] // one-off sqlx query-row tuple
    let row: Option<(i32, Option<String>, String, Option<String>, bool)> = sqlx::query_as(
        r#"
        SELECT
            s.user_id,
            s.expires_at::text AS expires_at,
            u.email,
            u.display_name,
            CASE
                WHEN s.expires_at < NOW() THEN false
                WHEN NOT s.is_active THEN false
                WHEN NOT u.is_active THEN false
                ELSE true
            END AS session_valid
        FROM auth.sessions s
        JOIN auth.users u ON s.user_id = u.user_id
        WHERE s.session_token = $1
        "#,
    )
    .bind(&req.session_token)
    .fetch_optional(&pool)
    .await?;

    let (user_id, expires_at, email, display_name, _session_valid) = match row {
        Some(r) if r.4 => r,
        // No row, or found-but-invalid → the playbook's send_invalid branch.
        _ => return Ok(ValidateSessionResponse::invalid()),
    };

    // Step `get_user_roles`: non-expired roles for the validated user, as a
    // JSON array (identical aggregation to the playbook).
    let roles: serde_json::Value = sqlx::query_scalar(
        r#"
        SELECT COALESCE(
            json_agg(r.role_name ORDER BY r.role_name) FILTER (WHERE r.role_id IS NOT NULL),
            '[]'::json
        ) AS roles
        FROM auth.user_roles ur
        JOIN auth.roles r ON ur.role_id = r.role_id
        WHERE ur.user_id = $1
          AND (ur.expires_at IS NULL OR ur.expires_at > NOW())
        "#,
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await?;

    let user = serde_json::json!({
        "user_id": user_id,
        "email": email,
        "display_name": display_name.unwrap_or_else(|| email.clone()),
        "roles": roles,
    });

    Ok(ValidateSessionResponse {
        status: "ok".into(),
        valid: true,
        user: Some(user),
        expires_at,
        error: None,
    })
}

// ---------------------------------------------------------------------------
// Login — synchronous mirror of `auth0_login`.
// ---------------------------------------------------------------------------

/// `POST /api/auth/login` request body.  Matches the gateway's
/// `NoetlClient::login_via_api` payload.
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub auth0_token: String,
    #[serde(default)]
    pub auth0_domain: String,
    #[serde(default = "default_client_ip")]
    pub client_ip: String,
    #[serde(default = "default_pg_auth")]
    pub credential: String,
}

fn default_client_ip() -> String {
    "0.0.0.0".to_string()
}

/// `POST /api/auth/login` response — mirrors the gateway callback envelope so
/// the gateway's shared login-output parsing handles both paths identically.
///
/// `status` is `"success"` (then `data.status == "authenticated"`) or
/// `"error"` (then `data.error` carries the reason), exactly like the
/// `/api/internal/callback` body the playbook posts.
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub status: String,
    pub data: serde_json::Value,
}

impl LoginResponse {
    fn token_error(reason: String) -> Self {
        Self {
            status: "error".into(),
            data: serde_json::json!({ "error": reason, "message": "Token validation failed" }),
        }
    }
}

/// Decoded Auth0 ID-token claims we act on (matches the playbook's `start` step).
#[derive(Debug)]
struct TokenClaims {
    sub: String,
    email: Option<String>,
    name: Option<String>,
}

/// Decode + validate the Auth0 ID token claims exactly as the `auth0_login`
/// `start` step does: base64url-decode the JWT payload (no signature check —
/// the playbook does none either), then verify issuer, expiry, and subject.
fn decode_and_validate_token(token: &str, auth0_domain: &str) -> Result<TokenClaims, String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err("Invalid JWT format".to_string());
    }

    let decoded = URL_SAFE_NO_PAD
        .decode(parts[1].trim_end_matches('='))
        .map_err(|e| format!("Invalid JWT payload: {e}"))?;
    let claims: serde_json::Value =
        serde_json::from_slice(&decoded).map_err(|e| format!("Invalid JWT payload: {e}"))?;

    let issuer = claims.get("iss").and_then(|v| v.as_str()).unwrap_or_default();
    if !auth0_domain.is_empty() && issuer != format!("https://{auth0_domain}/") {
        return Err("Invalid token issuer".to_string());
    }

    if let Some(exp) = claims
        .get("exp")
        .and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)))
    {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if exp <= now {
            return Err("Token has expired".to_string());
        }
    }

    let sub = claims.get("sub").and_then(|v| v.as_str()).unwrap_or_default();
    if sub.is_empty() {
        return Err("Missing token subject".to_string());
    }

    let email = claims.get("email").and_then(|v| v.as_str()).map(str::to_string);
    // `name` defaults to email when absent (playbook: decoded.get("name", email)).
    let name = claims
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| email.clone());

    Ok(TokenClaims { sub: sub.to_string(), email, name })
}

/// Authenticate an Auth0 token synchronously and create a NoETL session.
///
/// Decodes the token claims, then runs the identical `auth0_login`
/// `create_user_session` CTE (upsert user, inherit same-email roles, create
/// session, return roles) in one statement, and returns the
/// `{status: authenticated, session_token, user, expires_at}` contract.
pub async fn login(
    State(cred): State<CredentialService>,
    Json(req): Json<LoginRequest>,
) -> Json<LoginResponse> {
    // 1. Token claims (mirrors the playbook `start` step + its error callback).
    let claims = match decode_and_validate_token(&req.auth0_token, &req.auth0_domain) {
        Ok(c) => c,
        Err(reason) => {
            crate::metrics::record_auth_sync("login", "invalid");
            return Json(LoginResponse::token_error(reason));
        }
    };

    // 1b. Auth0 JWT **signature** verification (noetl/ai-meta#169), shipped dark.
    // With `NOETL_AUTH_VERIFY_SIGNATURE` unset/off this is a no-op and the login
    // decision is byte-identical to the claims-decode above.  In `shadow` it
    // logs + meters a would-reject but still lets the login proceed; only in
    // `enforce` does a bad signature / bad standard-claim reject the login (same
    // `token_error` envelope a bad claims-decode returns).  See
    // handlers::auth_verify for the JWKS fetch/cache + kid-rotation logic.
    let verify_mode = crate::handlers::auth_verify::verify_mode();
    if let Err(reason) = crate::handlers::auth_verify::enforce_for_login(
        &req.auth0_token,
        &req.auth0_domain,
        verify_mode,
    )
    .await
    {
        crate::metrics::record_auth_sync("login", "invalid");
        return Json(LoginResponse::token_error(reason));
    }

    // 2. Upsert user + create session (mirrors `create_user_session`).
    match login_create_session(&cred, &req, &claims).await {
        Ok(resp) => {
            crate::metrics::record_auth_sync("login", "authenticated");
            Json(resp)
        }
        Err(e) => {
            tracing::warn!(error = %e, "auth-sync login session creation failed");
            crate::metrics::record_auth_sync("login", "error");
            Json(LoginResponse {
                status: "error".into(),
                data: serde_json::json!({ "error": "Database error", "message": "Failed to create session" }),
            })
        }
    }
}

async fn login_create_session(
    cred: &CredentialService,
    req: &LoginRequest,
    claims: &TokenClaims,
) -> AppResult<LoginResponse> {
    let pool = resolve_auth_pool(cred, &req.credential).await?;

    // Identical CTE to auth0_login.create_user_session: upsert the Auth0 user,
    // copy same-email roles, create a fresh session, resolve the role names.
    // Parameterised binds replace the playbook's jsonb-escaped literals.
    #[allow(clippy::type_complexity)] // one-off sqlx query-row tuple
    let row: Option<(String, i32, Option<String>, String, Option<String>, serde_json::Value)> =
        sqlx::query_as(
            r#"
            WITH upserted_user AS (
              INSERT INTO auth.users (auth0_id, email, display_name, last_login_at)
              VALUES ($1, $2, $3, NOW())
              ON CONFLICT (auth0_id)
              DO UPDATE SET
                email = EXCLUDED.email,
                display_name = EXCLUDED.display_name,
                last_login_at = NOW()
              RETURNING user_id, email, display_name, is_active
            ),
            copied_roles AS (
              INSERT INTO auth.user_roles (user_id, role_id)
              SELECT u.user_id, ur.role_id
              FROM upserted_user u
              JOIN auth.users same_email
                ON same_email.email = u.email
               AND same_email.user_id <> u.user_id
              JOIN auth.user_roles ur
                ON ur.user_id = same_email.user_id
              WHERE ur.expires_at IS NULL OR ur.expires_at > NOW()
              ON CONFLICT (user_id, role_id) DO NOTHING
              RETURNING role_id
            ),
            created_session AS (
              INSERT INTO auth.sessions (user_id, session_token, auth0_token, expires_at, ip_address)
              SELECT
                u.user_id,
                md5(random()::text || clock_timestamp()::text),
                $4,
                NOW() + INTERVAL '24 hours',
                $5::inet
              FROM upserted_user u
              RETURNING session_token, user_id, expires_at
            ),
            resolved_roles AS (
              SELECT COALESCE(
                json_agg(r.role_name ORDER BY r.role_name) FILTER (WHERE r.role_id IS NOT NULL),
                '[]'::json
              ) AS roles
              FROM upserted_user u
              LEFT JOIN auth.user_roles ur
                ON ur.user_id = u.user_id
               AND (ur.expires_at IS NULL OR ur.expires_at > NOW())
              LEFT JOIN auth.roles r
                ON r.role_id = ur.role_id
            )
            SELECT
              s.session_token AS sess_ref,
              s.user_id,
              s.expires_at::text AS expires_at,
              u.email,
              u.display_name,
              rr.roles
            FROM created_session s
            JOIN upserted_user u ON u.user_id = s.user_id
            CROSS JOIN resolved_roles rr
            "#,
        )
        .bind(&claims.sub)
        .bind(claims.email.clone().unwrap_or_default())
        .bind(claims.name.clone().unwrap_or_default())
        .bind(&req.auth0_token)
        .bind(&req.client_ip)
        .fetch_optional(&pool)
        .await?;

    let Some((session_token, user_id, expires_at, email, display_name, roles)) = row else {
        // Mirrors the playbook's send_db_error branch (no rows returned).
        return Ok(LoginResponse {
            status: "error".into(),
            data: serde_json::json!({ "error": "Database error", "message": "Failed to create session" }),
        });
    };

    let user = serde_json::json!({
        "user_id": user_id,
        "email": email,
        "display_name": display_name.unwrap_or_else(|| email.clone()),
        "roles": roles,
    });

    Ok(LoginResponse {
        status: "success".into(),
        data: serde_json::json!({
            "status": "authenticated",
            "session_token": session_token,
            "user": user,
            "expires_at": expires_at,
        }),
    })
}

// ---------------------------------------------------------------------------
// Playbook access — synchronous mirror of `check_playbook_access`.
// ---------------------------------------------------------------------------
//
// The per-turn authorization gate has the same structural fragility login had:
// every Muno turn, before the planner runs, the gateway authorizes the user for
// the target playbook by executing `api_integration/auth0/check_playbook_access`
// as a **multi-hop off-server orchestration drive** (session lookup → normalize
// → permission lookup → grant/deny callback).  Under drive load that gate took
// ~7s in the incident; stacked in front of the (also multi-hop) planner turn it
// blew the SPA/gateway request budget and the turn was dropped before the
// planner execution was even created — the UI showed "Load failed" with no
// execution.  Like session validation, the authorization decision is a plain
// auth-DB lookup (session row + role/grant rows) that never needed a
// deadline-gated distributed workflow.
//
// This handler runs the *byte-identical* SQL the playbook runs — the same
// session-validity filter and the same role → playbook_permissions grant query,
// with the same action → can_execute/can_view/can_modify mapping and the same
// allow/deny pattern logic — but synchronously and in-process.  No NATS, no
// worker, no drive, no callback.  It returns the same grant/deny decision (and
// the same `{allowed, user, message}` payload the drive callback delivers), so
// the gateway makes identical authorization decisions; only the execution shape
// changes.  Gated behind the same `NOETL_AUTH_SYNC` gateway flag as login.

/// `POST /api/auth/check-playbook-access` request body.  Matches the gateway's
/// `NoetlClient::check_access_via_api` payload.
#[derive(Debug, Deserialize)]
pub struct CheckAccessRequest {
    pub session_token: String,
    pub playbook_path: String,
    /// `execute` | `view` | `modify` — the playbook's `action` (default matches
    /// the playbook workload default).
    #[serde(default = "default_action")]
    pub action: String,
    #[serde(default = "default_pg_auth")]
    pub credential: String,
}

fn default_action() -> String {
    "execute".to_string()
}

/// `POST /api/auth/check-playbook-access` response.  The `data` object mirrors
/// the `check_playbook_access` playbook's `/api/internal/callback` body so the
/// gateway parses both paths through the same tail: `status == "success"` for a
/// completed check (grant OR deny), `status == "error"` when the lookup itself
/// failed (auth DB unreachable) — surfaced by the gateway as a retryable backend
/// error rather than a false deny.
#[derive(Debug, Serialize)]
pub struct CheckAccessResponse {
    pub status: String,
    pub data: serde_json::Value,
}

impl CheckAccessResponse {
    /// A completed check → the drive callback's granted/denied `data` shape.
    fn decided(allowed: bool, user: Option<serde_json::Value>, action: &str) -> Self {
        // Mirror what the gateway receives on the wire today: the granted
        // callback carries `"Access granted to <action> playbook"`, and BOTH
        // denial branches collapse to `"Access denied"` in the callback body.
        let mut data = serde_json::json!({
            "allowed": allowed,
            "message": if allowed {
                format!("Access granted to {action} playbook")
            } else {
                "Access denied".to_string()
            },
        });
        if let (true, Some(u)) = (allowed, user) {
            data.as_object_mut().unwrap().insert("user".to_string(), u);
        }
        Self { status: "success".into(), data }
    }

    fn error(msg: String) -> Self {
        Self {
            status: "error".into(),
            data: serde_json::json!({ "error": msg, "message": "Access check failed" }),
        }
    }
}

/// Authorize a user for a playbook synchronously against the auth database.
///
/// Runs the identical `check_playbook_access` SQL: the session-validity lookup
/// (`is_active` + non-expired session + active user), then — when a session is
/// found — the role → `playbook_permissions` grant query with the same
/// allow/deny-pattern and action → capability mapping.  Returns the same
/// grant/deny decision the playbook's callback returns.  Fails **closed**: a DB
/// lookup error returns `status: "error"` (no grant) so the gateway surfaces a
/// retryable backend error instead of wrongly granting access.
pub async fn check_playbook_access(
    State(cred): State<CredentialService>,
    Json(req): Json<CheckAccessRequest>,
) -> Json<CheckAccessResponse> {
    match check_playbook_access_inner(&cred, &req).await {
        Ok(resp) => {
            let outcome = if resp.status == "success" {
                if resp.data.get("allowed").and_then(|v| v.as_bool()).unwrap_or(false) {
                    "granted"
                } else {
                    "denied"
                }
            } else {
                "error"
            };
            crate::metrics::record_auth_sync("check_access", outcome);
            Json(resp)
        }
        Err(e) => {
            tracing::warn!(error = %e, "auth-sync check_playbook_access lookup failed");
            crate::metrics::record_auth_sync("check_access", "error");
            Json(CheckAccessResponse::error(e.to_string()))
        }
    }
}

async fn check_playbook_access_inner(
    cred: &CredentialService,
    req: &CheckAccessRequest,
) -> AppResult<CheckAccessResponse> {
    let pool = resolve_auth_pool(cred, &req.credential).await?;

    // Step `get_user_from_session` from check_playbook_access: the active-session
    // + active-user lookup.  This is the gate's OWN session-validity filter
    // (is_active AND expires_at > NOW() AND user is_active) — replicated exactly,
    // not the `auth0_validate_session` CASE variant.  No row → denied (no
    // session); the drive path's send_denied_callback returns allowed=false.
    let session: Option<(i32, String, Option<String>)> = sqlx::query_as(
        r#"
        SELECT s.user_id, u.email, u.display_name
        FROM auth.sessions s
        JOIN auth.users u ON s.user_id = u.user_id
        WHERE s.session_token = $1
          AND s.is_active = true
          AND s.expires_at > NOW()
          AND u.is_active = true
        "#,
    )
    .bind(&req.session_token)
    .fetch_optional(&pool)
    .await?;

    let Some((user_id, email, display_name)) = session else {
        return Ok(CheckAccessResponse::decided(false, None, &req.action));
    };

    // Step `check_permission`: role → playbook_permissions grant lookup, with the
    // identical allow-pattern / deny-pattern logic and action → capability
    // mapping.  Parameterised binds replace the playbook's jsonb-escaped literals
    // (same match semantics, safer quoting).  $2 is the playbook path (used for
    // exact match, allow-pattern LIKE, and deny-pattern NOT LIKE); $3 is the
    // action string.
    let has_permission: bool = sqlx::query_scalar(
        r#"
        SELECT COUNT(*) > 0 AS has_permission
        FROM auth.user_roles ur
        JOIN auth.roles r ON ur.role_id = r.role_id
        JOIN auth.playbook_permissions pp ON r.role_id = pp.role_id
        WHERE ur.user_id = $1
          AND (ur.expires_at IS NULL OR ur.expires_at > NOW())
          AND (
            pp.playbook_path = $2
            OR (pp.allow_pattern IS NOT NULL AND $2 LIKE pp.allow_pattern)
          )
          AND (pp.deny_pattern IS NULL OR $2 NOT LIKE pp.deny_pattern)
          AND (
            ($3 = 'execute' AND pp.can_execute = true)
            OR ($3 = 'view' AND pp.can_view = true)
            OR ($3 = 'modify' AND pp.can_modify = true)
          )
        "#,
    )
    .bind(user_id)
    .bind(&req.playbook_path)
    .bind(&req.action)
    .fetch_one(&pool)
    .await?;

    if !has_permission {
        // Mirrors access_denied_no_permission → send_denied_callback.
        return Ok(CheckAccessResponse::decided(false, None, &req.action));
    }

    // Mirrors access_granted → send_granted_callback: allowed=true with the
    // user object (user_id/email/display_name) the gateway echoes back.
    let user = serde_json::json!({
        "user_id": user_id,
        "email": email,
        "display_name": display_name.unwrap_or_else(|| email.clone()),
    });
    Ok(CheckAccessResponse::decided(true, Some(user), &req.action))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_jwt(payload: &serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap());
        format!("{header}.{body}.sig")
    }

    fn future_exp() -> i64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64 + 3600
    }

    #[test]
    fn token_valid_claims_accepted() {
        let jwt = make_jwt(&serde_json::json!({
            "iss": "https://tenant.us.auth0.com/",
            "exp": future_exp(),
            "sub": "auth0|abc",
            "email": "a@b.com",
            "name": "Ann",
        }));
        let c = decode_and_validate_token(&jwt, "tenant.us.auth0.com").expect("valid");
        assert_eq!(c.sub, "auth0|abc");
        assert_eq!(c.email.as_deref(), Some("a@b.com"));
        assert_eq!(c.name.as_deref(), Some("Ann"));
    }

    #[test]
    fn token_name_defaults_to_email() {
        let jwt = make_jwt(&serde_json::json!({
            "iss": "https://tenant.us.auth0.com/",
            "exp": future_exp(),
            "sub": "auth0|abc",
            "email": "a@b.com",
        }));
        let c = decode_and_validate_token(&jwt, "tenant.us.auth0.com").expect("valid");
        assert_eq!(c.name.as_deref(), Some("a@b.com"));
    }

    #[test]
    fn token_wrong_issuer_rejected() {
        let jwt = make_jwt(&serde_json::json!({
            "iss": "https://evil.example.com/",
            "exp": future_exp(),
            "sub": "auth0|abc",
        }));
        assert_eq!(
            decode_and_validate_token(&jwt, "tenant.us.auth0.com").unwrap_err(),
            "Invalid token issuer"
        );
    }

    #[test]
    fn token_expired_rejected() {
        let jwt = make_jwt(&serde_json::json!({
            "iss": "https://tenant.us.auth0.com/",
            "exp": 1000, // long past
            "sub": "auth0|abc",
        }));
        assert_eq!(
            decode_and_validate_token(&jwt, "tenant.us.auth0.com").unwrap_err(),
            "Token has expired"
        );
    }

    #[test]
    fn token_missing_sub_rejected() {
        let jwt = make_jwt(&serde_json::json!({
            "iss": "https://tenant.us.auth0.com/",
            "exp": future_exp(),
        }));
        assert_eq!(
            decode_and_validate_token(&jwt, "tenant.us.auth0.com").unwrap_err(),
            "Missing token subject"
        );
    }

    #[test]
    fn token_malformed_rejected() {
        assert_eq!(
            decode_and_validate_token("not-a-jwt", "tenant.us.auth0.com").unwrap_err(),
            "Invalid JWT format"
        );
    }

    #[test]
    fn check_access_granted_shape_matches_callback() {
        // Granted → allowed=true, user echoed, message mirrors the playbook's
        // access_granted step ("Access granted to <action> playbook").
        let user = serde_json::json!({"user_id": 7, "email": "a@b.com", "display_name": "Ann"});
        let resp = CheckAccessResponse::decided(true, Some(user.clone()), "execute");
        assert_eq!(resp.status, "success");
        assert_eq!(resp.data.get("allowed").and_then(|v| v.as_bool()), Some(true));
        assert_eq!(
            resp.data.get("message").and_then(|v| v.as_str()),
            Some("Access granted to execute playbook")
        );
        assert_eq!(resp.data.get("user"), Some(&user));
    }

    #[test]
    fn check_access_denied_shape_matches_callback() {
        // Denied → allowed=false, no user, message collapses to the callback's
        // hard-coded "Access denied" (both denial branches send the same body).
        let resp = CheckAccessResponse::decided(false, None, "execute");
        assert_eq!(resp.status, "success");
        assert_eq!(resp.data.get("allowed").and_then(|v| v.as_bool()), Some(false));
        assert_eq!(resp.data.get("message").and_then(|v| v.as_str()), Some("Access denied"));
        assert!(resp.data.get("user").is_none());
    }

    #[test]
    fn check_access_denied_never_leaks_user_even_if_passed() {
        // Fail-closed hygiene: a user object is only attached on a grant.
        let user = serde_json::json!({"user_id": 7});
        let resp = CheckAccessResponse::decided(false, Some(user), "view");
        assert!(resp.data.get("user").is_none());
    }

    #[test]
    fn check_access_error_shape_is_retryable() {
        // Lookup failure → status "error" (gateway surfaces retryable backend
        // error, never a false grant).
        let resp = CheckAccessResponse::error("db down".into());
        assert_eq!(resp.status, "error");
        assert_eq!(resp.data.get("allowed").and_then(|v| v.as_bool()), None);
    }

    #[test]
    fn token_issuer_check_skipped_when_domain_empty() {
        // Empty configured domain → issuer is not enforced (playbook parity).
        let jwt = make_jwt(&serde_json::json!({
            "iss": "https://anything.example.com/",
            "exp": future_exp(),
            "sub": "auth0|abc",
        }));
        assert!(decode_and_validate_token(&jwt, "").is_ok());
    }
}
