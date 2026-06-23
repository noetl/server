//! NoETL Control Plane Server
//!
//! An async Rust server that provides the control plane API for NoETL,
//! handling workflow orchestration, catalog management, and event processing.

use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{delete, get, post, put},
};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use noetl_server::{
    config::{AppConfig, DatabaseConfig, ShardingConfig},
    db::{DbPool, DbPoolMap, create_pool},
    handlers,
    services::{
        CatalogService, CredentialService, ExecutionService, KeychainService, ReplayService,
        ResultStoreService, RuntimeService,
    },
    state::AppState,
};

// NOTE (noetl/ai-meta#61, Phase 1a): the hardcoded all-zeros
// DEFAULT_ENCRYPTION_KEY was REMOVED.  A publicly-known / shared default key
// is equivalent to no encryption and fails security review.  The server now
// fails closed when no key is configured (see `resolve_encryption_key`).

/// Initialize tracing/logging.
fn init_tracing() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,noetl_control_plane=debug,tower_http=debug".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();
}

/// Build the application router with all routes.
#[allow(clippy::too_many_arguments)]
fn build_router(
    state: AppState,
    db_pool: DbPool,
    catalog_service: CatalogService,
    credential_service: CredentialService,
    keychain_service: KeychainService,
    execution_service: ExecutionService,
    runtime_service: RuntimeService,
    replay_service: ReplayService,
    result_store_service: ResultStoreService,
    wallet_cipher: noetl_server::crypto::EnvelopeCipher,
) -> Router {
    // CORS configuration - allow all origins for development
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Object-store backend + cell endpoint registry (RFC noetl/ai-meta#104
    // Phase C).  Both are env-driven (default off → Postgres object backend,
    // single-cell seed), built once here and cloned into their route state.
    let object_backend =
        noetl_server::services::object_backend::ObjectBackend::from_env();
    let cell_registry =
        noetl_server::services::cell_registry::CellRegistry::from_env(&object_backend);

    // Health check routes (no auth required)
    let mut health_routes = Router::new()
        .route("/health", get(handlers::health_check))
        .route("/api/health", get(handlers::api_health))
        .route("/api/pool/status", get(handlers::health::pool_status));
    // Prometheus metrics endpoint — gated by AppConfig.disable_metrics
    // per agents/rules/observability.md.  Default-on; ingress / netpol
    // can restrict reach in prod.
    if !state.config.disable_metrics {
        health_routes = health_routes.route("/metrics", get(handlers::health::metrics));
    }
    let health_routes = health_routes.with_state(state.clone());

    // Catalog routes
    let catalog_routes = Router::new()
        .route("/api/catalog/register", post(handlers::catalog::register))
        .route("/api/catalog/list", post(handlers::catalog::list))
        .route(
            "/api/catalog/resource",
            post(handlers::catalog::get_resource),
        )
        // ui_schema route — wildcard tail because the Python contract
        // `GET /api/catalog/{path:path}/ui_schema` accepts slash-bearing
        // paths (e.g. `system/outbox_publisher`).  The handler routes
        // by suffix: only requests whose tail ends with `/ui_schema`
        // are served; everything else returns 404.
        .route("/api/catalog/{*tail}", get(handlers::catalog::ui_schema))
        .with_state(catalog_service);

    // Credential routes
    let credential_routes = Router::new()
        .route(
            "/api/credentials",
            post(handlers::credentials::create_or_update),
        )
        .route("/api/credentials", get(handlers::credentials::list))
        .route(
            "/api/credentials/{identifier}",
            get(handlers::credentials::get),
        )
        .route(
            "/api/credentials/{identifier}",
            delete(handlers::credentials::delete),
        )
        .with_state(credential_service.clone());

    // Sealed-credential endpoint (Secrets Wallet Phase 5b, noetl/ai-meta#61).
    // Returns a SealedEnvelope (X25519-sealed credential JSON) addressed to
    // the worker named via `?worker_id=<name>`.  Defense-in-depth on top of
    // Phase-4 mTLS — cleartext never enters the response body.
    let sealed_credential_routes = Router::new()
        .route(
            "/api/credentials/{identifier}/sealed",
            get(handlers::credentials::get_sealed),
        )
        .with_state(handlers::credentials::SealedCredentialDeps {
            credentials: credential_service.clone(),
            runtime: runtime_service.clone(),
        });

    // Cross-region broker endpoint (Secrets Wallet Phase 6e, noetl/ai-meta#61).
    // Peer-server side of the cross-region broker — receives a request from a
    // sibling server whose local residency policy denied a credential,
    // resolves the credential locally subject to ITS own residency + provider
    // chain, and returns a SealedEnvelope addressed directly to the
    // requesting worker's pubkey.  Cleartext never leaves this server's
    // memory.  Internal-only endpoint; production gates via peer-cert mTLS.
    let cross_region_routes = Router::new()
        .route(
            "/api/internal/cross-region/resolve",
            post(handlers::cross_region::resolve),
        )
        .with_state(handlers::cross_region::CrossRegionDeps {
            credentials: credential_service.clone(),
        });

    // Wallet KEK rotation endpoints (Secrets Wallet Phase 7a.2,
    // noetl/ai-meta#61).  POST /api/internal/wallet/rotate-kek runs a
    // batched re-wrap pass; GET /api/internal/wallet/key-status reports
    // per-version row counts so an operator can confirm rotation
    // completion before retiring an old KEK version.
    let wallet_rotate_service = noetl_server::services::wallet_rotate::WalletRotateService::new(
        db_pool.clone(),
        wallet_cipher.clone(),
    );
    let wallet_rotate_routes = Router::new()
        .route(
            "/api/internal/wallet/rotate-kek",
            post(handlers::wallet_rotate::rotate_kek),
        )
        .route(
            "/api/internal/wallet/key-status",
            get(handlers::wallet_rotate::key_status),
        )
        .with_state(handlers::wallet_rotate::WalletRotateDeps {
            service: wallet_rotate_service,
        });

    // Secret-audit query endpoint (Secrets Wallet Phase 7b.2,
    // noetl/ai-meta#61).  GET /api/internal/secret-audit returns
    // bounded rows from `noetl.secret_audit` (table created at
    // startup via `db::queries::secret_audit::ensure_table`).
    let secret_audit_routes = Router::new()
        .route(
            "/api/internal/secret-audit",
            get(handlers::secret_audit::query),
        )
        .with_state(handlers::secret_audit::SecretAuditDeps {
            pool: db_pool.clone(),
        });

    // Container Tool Callback endpoint (Round 2 of
    // noetl/ai-meta#43, noetl/server#140).  POST
    // /api/internal/container-callback/{execution_id}/{step}
    // consumes the watcher's POST and emits a call.done event;
    // returns 202 even on stale callbacks (no matching execution).
    let container_callback_routes = Router::new()
        .route(
            "/api/internal/container-callback/{execution_id}/{step}",
            post(handlers::container_callback::container_callback),
        )
        .with_state(state.clone());

    // CQRS read-model advance endpoint (noetl/ai-meta#103 phase 2b).  The
    // system/projector playbook posts the execution_ids from a noetl_events
    // stream batch; the server recomputes + saves each one's
    // projection_snapshot.  Carries AppState (needs pools + snowflake), so it's
    // a separate router from the DbPool-stated internal group above.
    let projection_routes = Router::new()
        .route(
            "/api/internal/projection/advance",
            post(handlers::internal::projection_advance),
        )
        // CQRS write-path cutover (#103 phase 2d): materialize noetl.event from
        // native producer events (normalized via the shared ingest path).
        .route(
            "/api/internal/events/materialize",
            post(handlers::internal::events_materialize),
        )
        // events/project (the materializer's row-shape writer) carries AppState
        // so it can fire the relocated orchestrator trigger after materializing
        // a batch under NOETL_EVENT_INGEST_PUBLISH_ONLY (#103 phase 2d-3).
        .route(
            "/api/internal/events/project",
            post(handlers::internal::events_project),
        )
        .with_state(state.clone());

    // Keychain routes
    let keychain_routes = Router::new()
        .route(
            "/api/keychain/{catalog_id}/{keychain_name}",
            get(handlers::keychain::get),
        )
        .route(
            "/api/keychain/{catalog_id}/{keychain_name}",
            post(handlers::keychain::set),
        )
        .route(
            "/api/keychain/{catalog_id}/{keychain_name}",
            delete(handlers::keychain::delete),
        )
        .route(
            "/api/keychain/catalog/{catalog_id}",
            get(handlers::keychain::list_by_catalog),
        )
        .with_state(keychain_service);

    // Execution routes (v2 event-driven)
    let execution_routes = Router::new()
        .route("/api/execute", post(handlers::execute))
        .route("/api/execute/batch", post(handlers::execute_batch))
        .route("/api/events", post(handlers::handle_event))
        .route(
            "/api/events/batch",
            post(handlers::events::handle_batch_events),
        )
        .route("/api/commands/{event_id}", get(handlers::get_command))
        .route(
            "/api/commands/{event_id}/claim",
            post(handlers::events::claim_command),
        )
        .with_state(state.clone());

    // Subscription lifecycle routes (noetl/ai-meta#90 Phase 2).  A
    // `kind: Subscription` is registered + activated here; the continuous
    // runtime drives pause/resume/drain/deactivate, each event-logged.
    let subscription_routes = Router::new()
        .route(
            "/api/subscriptions",
            get(handlers::subscription::list).post(handlers::subscription::register),
        )
        .route(
            "/api/subscriptions/register",
            post(handlers::subscription::register),
        )
        .route(
            "/api/subscriptions/{id}",
            get(handlers::subscription::get),
        )
        .route(
            "/api/subscriptions/{id}/{action}",
            post(handlers::subscription::lifecycle),
        )
        .with_state(state.clone());

    // Execution management routes
    let executions_routes = Router::new()
        .route("/api/executions", get(handlers::executions::list))
        .route(
            "/api/executions/{execution_id}",
            get(handlers::executions::get),
        )
        .route(
            "/api/executions/{execution_id}/status",
            get(handlers::executions::get_status),
        )
        .route(
            "/api/executions/{execution_id}/cancel",
            post(handlers::executions::cancel),
        )
        .route(
            "/api/executions/{execution_id}/cancellation-check",
            get(handlers::executions::cancellation_check),
        )
        .route(
            "/api/executions/{execution_id}/finalize",
            post(handlers::executions::finalize),
        )
        .with_state(execution_service);

    // Replay engine routes (Phase D R5 of noetl/ai-meta#49 →
    // noetl/server#148).  Round 1 ships `GET /api/replay/state`
    // with the minimal `execution` projection.  Service uses
    // `state.pools` for shard-aware reads.
    let replay_routes = Router::new()
        .route("/api/replay/state", get(handlers::replay::replay_state))
        .with_state(replay_service);

    // Result-store routes (noetl/ai-meta#70).
    //
    // `PUT /api/result/{execution_id}` — worker calls this after a step
    // result exceeds the inline budget; stores JSON + mints a
    // `noetl://` URI.
    // `GET /api/result/resolve?ref=<uri>` — tools::result_fetch HTTP
    // fallback; returns the stored payload body directly.
    //
    // NOTE: axum matches routes in registration order within a Router.
    // `GET /api/result/resolve` must be registered BEFORE any wildcard
    // path like `GET /api/result/{execution_id}/{step_name}` (future
    // endpoints) to avoid shadowing.  Keeping resolve in its own Router
    // guarantees order isolation.
    let result_store_routes = Router::new()
        .route(
            "/api/result/{execution_id}",
            put(handlers::result_store::put_result),
        )
        .route(
            "/api/result/resolve",
            get(handlers::result_store::resolve_ref),
        )
        // Workers stage over-budget tool results via PUT; payloads can
        // reach 10s of MB (e.g. test_storage_tiers generates 15 MB).
        // Axum's default 2 MB body limit rejects these with HTTP 413,
        // which breaks the `_ref` propagation path (noetl/ai-meta#69).
        // 64 MB is generous enough for any realistic tool result while
        // still bounding memory.
        .layer(DefaultBodyLimit::max(64 * 1024 * 1024))
        .with_state(handlers::result_store::ResultStoreDeps {
            service: result_store_service,
            mint_authoritative: state.config.result_mint_authoritative,
        });

    // Variable routes (transient table)
    let variable_routes = Router::new()
        .route("/api/vars/{execution_id}", get(handlers::variables::list))
        .route("/api/vars/{execution_id}", post(handlers::variables::set))
        .route(
            "/api/vars/{execution_id}",
            delete(handlers::variables::cleanup),
        )
        .route(
            "/api/vars/{execution_id}/{var_name}",
            get(handlers::variables::get),
        )
        .route(
            "/api/vars/{execution_id}/{var_name}",
            delete(handlers::variables::delete_var),
        )
        .with_state(db_pool.clone());

    // Runtime/Worker pool routes
    let runtime_routes = Router::new()
        .route(
            "/api/worker/pool/register",
            post(handlers::runtime::register_pool),
        )
        .route(
            "/api/worker/pool/deregister",
            delete(handlers::runtime::deregister_pool),
        )
        .route(
            "/api/worker/pool/heartbeat",
            post(handlers::runtime::heartbeat),
        )
        .route("/api/worker/pools", get(handlers::runtime::list_pools))
        // NOTE: `/api/runtimes` (no-filter list) was a Rust-side innovation
        // with no Python equivalent — removed for Phase A parity per #49
        // constraint #2 ("byte-identical contracts during migration").
        // Handler `runtime::list_all` retained for the eventual Python-side
        // backport; see noetl/server follow-up issue.
        .with_state(runtime_service);

    // Sharding diagnostic — Phase F R3b-1 of noetl/ai-meta#49.
    // Public endpoint; pure math; no auth gate.  Pair with the
    // gateway twin (Phase F R3b-2) and the integration test
    // (R3b-3 in noetl/ops) for the end-to-end drift-guard.
    let sharding_routes = Router::new()
        .route(
            "/api/runtime/shard-info",
            get(handlers::sharding::get_shard_info),
        )
        .with_state(state.clone());

    // Database routes
    let database_routes = Router::new()
        .route(
            "/api/postgres/execute",
            post(handlers::database::execute_postgres),
        )
        .route("/api/db/init", post(handlers::database::init_database))
        .route(
            "/api/db/validate",
            get(handlers::database::validate_database),
        )
        .with_state(db_pool.clone());

    // Internal API — system worker pool only.  Gated by the
    // ``RequireInternalApiToken`` extractor in
    // ``handlers::internal`` which constant-time-compares the
    // request bearer token to ``NOETL_INTERNAL_API_TOKEN``.  Mirror of
    // the Python implementation in
    // ``repos/noetl/noetl/server/api/internal/`` (noetl v4.10.0).
    // Tracks noetl/server#11 → noetl/ai-meta#49 Phase C.
    let internal_routes = Router::new()
        .route(
            "/api/internal/outbox/claim",
            post(handlers::internal::outbox_claim),
        )
        .route(
            "/api/internal/outbox/mark-published",
            post(handlers::internal::outbox_mark_published),
        )
        .route(
            "/api/internal/outbox/mark-failed",
            post(handlers::internal::outbox_mark_failed),
        )
        .route(
            "/api/internal/outbox/pending-count",
            get(handlers::internal::outbox_pending_count),
        )
        .route(
            "/api/internal/cleanup/purge",
            post(handlers::internal::cleanup_purge),
        )
        // Plug-in module registry (noetl/ai-meta#105 Round 4) — the live
        // PluginSource backend the system worker pool's wasmtime host fetches
        // from. Catch-all `{*path}` so `system/materialiser`-style paths resolve.
        .route(
            "/api/internal/plugins/{*path}",
            post(handlers::plugins::register).get(handlers::plugins::fetch),
        )
        .with_state(db_pool.clone());

    // Object store (noetl/ai-meta#105 Round 5; backend selector noetl/ai-meta#104
    // Phase C) — server-mediated backend for a plug-in's `noetl.object_put`
    // capability (the Feather tier), keyed by the §7 physical object key. Its own
    // group because it carries the resolved `ObjectBackend` (Postgres | GCS) in
    // state alongside the pool.
    let object_store_routes = Router::new()
        .route(
            "/api/internal/objects/{*key}",
            put(handlers::objects::put).get(handlers::objects::get),
        )
        .with_state(handlers::objects::ObjectStoreDeps {
            pool: db_pool.clone(),
            backend: object_backend.clone(),
        });

    // Cell endpoint registry (noetl/ai-meta#104 Phase C) — the read-side cell map
    // the resolve-by-URN path consults. Single-cell seed today; fail-safe miss.
    let cell_routes = Router::new()
        .route("/api/internal/cells", get(handlers::cells::list_cells))
        .with_state(handlers::cells::CellRegistryDeps {
            registry: cell_registry,
        });

    // Result-tier GC (noetl/ai-meta#104 Phase F) — the conservative, dry-run-first
    // sweeper that reclaims only provably-dead tier objects (execution aged out of
    // the event log; never a live-referenced object). Gated `NOETL_RESULT_TIER_GC`
    // (default off → no-op). Carries the pool (liveness query) + the object backend
    // (list + delete) in state.
    let result_tier_routes = Router::new()
        .route(
            "/api/internal/result-tier/gc",
            post(handlers::result_tier::gc),
        )
        .with_state(handlers::result_tier::ResultTierDeps {
            pool: db_pool.clone(),
            backend: object_backend.clone(),
        });

    // Gateway push-ingress config endpoint (noetl/ai-meta#90 Phase 3).  The
    // gateway calls GET /api/internal/ingress/{listener} (service-account
    // gated) to resolve a push subscription's verify scheme + Wallet-resolved
    // secret + dispatch + directive allowlist, so it can verify-then-forward a
    // webhook/Pub-Sub-push delivery without holding a DB connection
    // (data-access-boundary.md).  Carries its own deps state (control-plane
    // state + credential service).
    let ingress_routes = Router::new()
        .route(
            "/api/internal/ingress/{listener}",
            get(handlers::ingress::get_ingress_config),
        )
        .with_state(handlers::ingress::IngressDeps {
            state: state.clone(),
            credentials: credential_service.clone(),
        });

    // System monitoring routes
    let system_routes = Router::new()
        .route("/api/status", get(handlers::system::get_status))
        .route("/api/threads", get(handlers::system::get_threads))
        .route(
            "/api/profiler/status",
            get(handlers::system::get_profiler_status),
        )
        .route(
            "/api/profiler/memory/start",
            post(handlers::system::start_memory_profiler),
        )
        .route(
            "/api/profiler/memory/stop",
            post(handlers::system::stop_memory_profiler),
        )
        .with_state(state);

    // Dashboard routes
    let dashboard_routes = Router::new()
        .route("/api/dashboard/stats", get(handlers::dashboard::get_stats))
        .route(
            "/api/dashboard/widgets",
            get(handlers::dashboard::get_widgets),
        )
        .with_state(db_pool);

    // Combine all routes
    Router::new()
        .merge(health_routes)
        .merge(catalog_routes)
        .merge(credential_routes)
        .merge(sealed_credential_routes)
        .merge(cross_region_routes)
        .merge(wallet_rotate_routes)
        .merge(secret_audit_routes)
        .merge(container_callback_routes)
        .merge(projection_routes)
        .merge(keychain_routes)
        .merge(execution_routes)
        .merge(executions_routes)
        .merge(subscription_routes)
        .merge(replay_routes)
        .merge(result_store_routes)
        .merge(variable_routes)
        .merge(runtime_routes)
        .merge(sharding_routes)
        .merge(database_routes)
        .merge(internal_routes)
        .merge(object_store_routes)
        .merge(cell_routes)
        .merge(result_tier_routes)
        .merge(ingress_routes)
        .merge(system_routes)
        .merge(dashboard_routes)
        .layer(TraceLayer::new_for_http())
        .layer(cors)
}

/// Connect to NATS if configured.
///
/// `async_nats::connect()` only parses the addr portion of the
/// URL — it does NOT pick up the `user:pass@` segment, so an
/// account-authenticated server (the cluster's `NOETL` account
/// with `noetl/noetl`) rejects the connection with
/// "authorization violation".  Mirror the worker's
/// `subscriber.rs` shape: strip the userinfo, build
/// `ConnectOptions::with_user_and_password`, pass the cleaned
/// URL.  See noetl/server#26 for the discovery (Phase B R3 of
/// noetl/ai-meta#49 surfaced this — pre-existing but unnoticed
/// since prior rounds didn't need a Rust-side NATS publish).
async fn connect_nats(config: &AppConfig) -> Option<async_nats::Client> {
    let Some(ref nats_url) = config.nats_url else {
        tracing::info!("NATS not configured, running without messaging");
        return None;
    };

    // Strip + parse userinfo if present.
    let (clean_url, creds) = strip_nats_userinfo(nats_url);
    let connect_future = match creds {
        Some((user, password)) => {
            async_nats::ConnectOptions::with_user_and_password(user, password).connect(&clean_url)
        }
        None => async_nats::ConnectOptions::new().connect(&clean_url),
    };

    match connect_future.await {
        Ok(client) => {
            tracing::info!(url = %clean_url, "Connected to NATS");
            Some(client)
        }
        Err(e) => {
            tracing::warn!(error = %e, url = %clean_url, "Failed to connect to NATS, continuing without it");
            None
        }
    }
}

/// Strip the `user:pass@` portion from a NATS URL and return the
/// cleaned URL alongside the parsed credentials, if any.
///
/// `async_nats::ConnectOptions` rejects URLs with embedded creds,
/// so we feed it the cleaned form + the creds via
/// `with_user_and_password`.  Mirrors the equivalent helper in
/// `noetl-worker::nats::subscriber`.
fn strip_nats_userinfo(url: &str) -> (String, Option<(String, String)>) {
    // Match `<scheme>://<userinfo>@<rest>` — userinfo is the
    // `user:password` pair the standard `host:port` URL parser
    // ignores when it's embedded.
    let scheme_sep = "://";
    let Some(scheme_idx) = url.find(scheme_sep) else {
        return (url.to_string(), None);
    };
    let after_scheme = &url[scheme_idx + scheme_sep.len()..];
    let Some(at_idx) = after_scheme.find('@') else {
        return (url.to_string(), None);
    };
    let userinfo = &after_scheme[..at_idx];
    let rest = &after_scheme[at_idx + 1..];
    let mut parts = userinfo.splitn(2, ':');
    let user = parts.next().unwrap_or("").to_string();
    let password = parts.next().unwrap_or("").to_string();
    if user.is_empty() {
        return (url.to_string(), None);
    }
    let cleaned = format!("{}{}{}", &url[..scheme_idx], scheme_sep, rest);
    (cleaned, Some((user, password)))
}

/// Resolve the at-rest encryption key (noetl/ai-meta#61, Phase 1a).
///
/// Security policy:
/// - `NOETL_ENCRYPTION_KEY` (base64-encoded 32-byte key) is the supported
///   source of a real key. When set + non-empty, it is used.
/// - When absent/empty, the server **fails closed** (refuses to start) —
///   the old silent fallback to a hardcoded all-zeros key is removed.
/// - The only exception is the explicit non-production escape hatch
///   `NOETL_ALLOW_INSECURE_DEFAULT_KEY=true` (for kind / local dev), which
///   generates a **random ephemeral** key (never a shared/known constant)
///   and logs a loud warning. Credentials encrypted under it do NOT survive
///   a restart — set `NOETL_ENCRYPTION_KEY` for stable dev data.
///
/// Pure helper (env-free) so it is unit-testable without mutating process env.
fn resolve_encryption_key(key_env: Option<String>, allow_insecure: bool) -> anyhow::Result<String> {
    if let Some(k) = key_env {
        if !k.trim().is_empty() {
            return Ok(k);
        }
    }
    if allow_insecure {
        use base64::Engine as _;
        use rand::RngCore;
        let mut key = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut key);
        let b64 = base64::engine::general_purpose::STANDARD.encode(key);
        tracing::warn!(
            "NOETL_ENCRYPTION_KEY is not set and NOETL_ALLOW_INSECURE_DEFAULT_KEY is \
             enabled: generated a RANDOM ephemeral key for this process. Encrypted \
             credentials will NOT decrypt after a restart. Never use in production."
        );
        return Ok(b64);
    }
    anyhow::bail!(
        "NOETL_ENCRYPTION_KEY is not set. Refusing to start: the insecure all-zeros \
         default key was removed (noetl/ai-meta#61). Provide a base64-encoded 32-byte \
         key via NOETL_ENCRYPTION_KEY (production), or set \
         NOETL_ALLOW_INSECURE_DEFAULT_KEY=true for ephemeral non-production use."
    )
}

/// Read the encryption key from the environment, applying the fail-closed
/// policy in `resolve_encryption_key`.
fn get_encryption_key() -> anyhow::Result<String> {
    let key_env = std::env::var("NOETL_ENCRYPTION_KEY").ok();
    let allow_insecure = std::env::var("NOETL_ALLOW_INSECURE_DEFAULT_KEY")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false);
    resolve_encryption_key(key_env, allow_insecure)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Load environment variables from .env file if present
    dotenvy::dotenv().ok();

    // Initialize tracing
    init_tracing();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "Starting NoETL Control Plane"
    );

    // Load configuration
    let app_config = AppConfig::from_env().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "Failed to load app config, using defaults");
        AppConfig::default()
    });

    let db_config = DatabaseConfig::from_env().unwrap_or_else(|e| {
        tracing::warn!(error = %e, "Failed to load database config, using defaults");
        DatabaseConfig::default()
    });

    tracing::info!(
        host = %app_config.host,
        port = app_config.port,
        debug = app_config.debug,
        "Configuration loaded"
    );

    // Create database connection pool (legacy single pool — kept
    // as the cluster-wide pool in DbPoolMap's fallback branch
    // and as `AppState.db` for handlers that haven't migrated
    // to the pool map yet, per R4-2).
    let db_pool = create_pool(&db_config).await?;

    // Phase F R4-2 of noetl/ai-meta#49: load sharding config
    // and build the DbPoolMap.  When NOETL_SHARDS is empty
    // (today's default), DbPoolMap::new short-circuits to a
    // single-pool fallback that wraps `db_pool` itself —
    // behaviour bit-identical to pre-R4 single-host deployments.
    let sharding_config = ShardingConfig::from_env().unwrap_or_else(|e| {
        tracing::warn!(
            error = %e,
            "Failed to parse NOETL_SHARDS / NOETL_CLUSTER_DSN; falling back to single-pool mode"
        );
        ShardingConfig::default()
    });
    let pools = if sharding_config.is_disabled() {
        DbPoolMap::from_single_pool(db_pool.clone())
    } else {
        DbPoolMap::new(&db_config, &sharding_config).await?
    };
    tracing::info!(
        shard_count = pools.shard_count(),
        single_pool_mode = pools.is_single_pool(),
        "Database pool map ready"
    );

    // Connect to NATS (optional)
    let nats_client = connect_nats(&app_config).await;

    // Get encryption key (fails closed if unset; see resolve_encryption_key).
    let encryption_key = get_encryption_key()?;

    // Create application state first so the snowflake generator
    // (Phase F R1.5 of noetl/ai-meta#49) is initialized once and
    // shared with the services below.  Services that need to mint
    // ids take a clone of `state.snowflake` (an `Arc`).
    let state = AppState::new(db_pool.clone(), pools, app_config.clone(), nats_client);

    // Background reconcile poller (noetl/ai-meta#101 block b): periodically
    // force-advances any cached execution that got stuck on a missed
    // non-triggering straggler, so the orchestrator never permanently stalls
    // under DB backpressure (e.g. a small Cloud SQL tier behind PgBouncer).
    handlers::events::spawn_orchestrator_reconciler(state.clone());

    // CQRS write-path producer (noetl/ai-meta#103 phase 2a): a background tailer
    // that batch-publishes committed `noetl.event` rows onto the `noetl_events`
    // JetStream stream for the system/projector playbook to fold.  Default OFF
    // (`NOETL_EVENT_STREAM_ENABLED` unset) so landing 2a publishes nothing until
    // ops opts the cluster into the CQRS write path; no-op without NATS.
    noetl_server::services::event_stream::spawn_event_stream_tailer(
        state.clone(),
        noetl_server::services::event_stream::EventStreamConfig::from_env(),
    );

    // CQRS write-path cutover (noetl/ai-meta#103 phase 2d-3): when
    // `NOETL_EVENT_INGEST_PUBLISH_ONLY` is on, server-originated events publish to
    // `noetl_events` instead of INSERTing — the materializer is the sole writer.
    // Loud at startup because it changes the durability boundary.  Every
    // server-originated producer — including ExecutionService cancel/finalize —
    // now routes through the chokepoint, so the server writes ZERO noetl.event
    // rows under the gate (the materializer is the only writer).
    if app_config.event_ingest_publish_only {
        if state.nats.is_some() {
            tracing::warn!(
                target: "noetl_server::startup",
                "NOETL_EVENT_INGEST_PUBLISH_ONLY=ON — ALL server-originated noetl.event writes PUBLISH to noetl_events (materializer is the sole writer; the server writes zero event rows)"
            );
        } else {
            tracing::warn!(
                target: "noetl_server::startup",
                "NOETL_EVENT_INGEST_PUBLISH_ONLY set but NATS is not connected — falling back to synchronous INSERT (gate inert)"
            );
        }
    }

    // Create services. The wallet's envelope cipher is built once over the
    // configured KEK provider (NOETL_KMS_PROVIDER: `local` default, or
    // `gcp-kms` over Cloud KMS — noetl/ai-meta#61 Phase 2) and shared between
    // the credential and keychain services.
    let catalog_service = CatalogService::new(db_pool.clone());
    let wallet_cipher = noetl_server::crypto::build_envelope_cipher(&encryption_key)?;
    let wallet_cipher_for_router = wallet_cipher.clone();

    // Secrets Wallet Phase 7b.2 — `noetl.secret_audit` table is owned
    // entirely by noetl/server (no other component writes to it), so a
    // CREATE TABLE IF NOT EXISTS at startup is the right shape — no
    // out-of-band migration step required for first-boot deployments.
    noetl_server::db::queries::secret_audit::ensure_table(&db_pool).await?;

    // Result-store MVP (noetl/ai-meta#70) — same idempotent startup-DDL
    // pattern as secret_audit above.  The table is server-owned end-to-end;
    // no out-of-band migration required.
    noetl_server::db::queries::result_store::ensure_table(&db_pool).await?;
    // Plug-in module registry (noetl/ai-meta#105 Round 4) — the durable backing
    // for the system worker pool's wasmtime PluginSource.  Same idempotent
    // startup-DDL pattern; server-owned end-to-end.
    noetl_server::db::queries::plugin_module::ensure_table(&db_pool).await?;
    // Seed built-in system plug-ins (noetl/ai-meta#108 slice 3) — the
    // server-owned `system/orchestrate` (+ future built-ins) compiled to wasm32
    // and baked into the image are registered into noetl.plugin_module on boot,
    // so the worker pool can fetch them without an out-of-band operator POST.
    // Non-fatal: a seed failure must not block the server from starting.
    match noetl_server::system_plugins::seed_system_plugins(&db_pool).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(count = n, "seeded built-in system plug-ins"),
        Err(e) => tracing::warn!(error = %e, "failed to seed system plug-ins; continuing"),
    }
    // Object store (noetl/ai-meta#105 Round 5) — durable Feather tier backing a
    // plug-in's `noetl.object_put`. Same idempotent startup-DDL pattern.
    noetl_server::db::queries::object_store::ensure_table(&db_pool).await?;
    // Opt-in subscription dedup window (noetl/ai-meta#90 Phase 7, RFC §10
    // OQ1) — same idempotent startup-DDL pattern.  The table is server-owned
    // (only /api/execute writes it) and bounded by age; dedup is opt-in per
    // subscription so the table stays empty unless a critical stream uses it.
    noetl_server::db::queries::subscription_dedup::ensure_table(&db_pool).await?;
    // One-level event chain (RFC #115 Phase 2, noetl/ai-meta#115 §4) — add the
    // additive `prev_event_id` link to noetl.event + noetl.command so the
    // populate-on-emit code never writes a column the running DB is missing
    // (the gate-off INSERT binds an explicit column list).  Idempotent; the
    // canonical definition also lives in noetl/noetl's schema_ddl.sql.
    noetl_server::db::queries::event_chain::ensure_columns(&db_pool).await?;
    // kind: Subscription (noetl/ai-meta#90 Phase 2) — seed the `subscription`
    // resource kind so a catalog register doesn't trip the
    // `noetl.catalog.kind -> noetl.resource(name)` FK.  Idempotent.
    noetl_server::db::queries::catalog::ensure_builtin_kinds(&db_pool).await?;
    // Keychain is the execution-scoped cache for credential resolution
    // (Secrets Wallet Phase 3c), so it is built first + shared into the
    // credential service.
    let keychain_service = KeychainService::new(db_pool.clone(), wallet_cipher.clone());
    let credential_service =
        CredentialService::new(db_pool.clone(), wallet_cipher, keychain_service.clone());
    // Phase F R4-4b: ExecutionService now takes the DbPoolMap so
    // its per-execution methods route via pool_for(execution_id)
    // and `list()` fan-outs via for_each_shard.  In single-pool
    // fallback mode this is the same handle as db_pool.
    // Pass the full `state` so the service's cancel/finalize paths can
    // route their noetl.event writes through the emit_event chokepoint
    // (noetl/ai-meta#103 2d-3) — honouring NOETL_EVENT_INGEST_PUBLISH_ONLY.
    let execution_service =
        ExecutionService::new(state.pools.clone(), state.snowflake.clone(), state.clone());
    let runtime_service = RuntimeService::new(db_pool.clone(), state.snowflake.clone());

    // Phase D R5 Round 1 (noetl/ai-meta#49 → noetl/server#148).
    // Replay engine — per-execution event reconstruction; uses
    // `state.pools` for shard-aware reads of `noetl.event`.
    let replay_service = ReplayService::new(state.pools.clone());

    // Result-store service (noetl/ai-meta#70).  Uses the cluster-wide
    // pool (same as catalog / credential) because `noetl.result_store`
    // is a server-owned table — not per-execution-shard.  The
    // `execution_id` column is present for data locality but the
    // table itself is cluster-scoped in the MVP.
    let result_store_service =
        ResultStoreService::new(db_pool.clone(), state.snowflake.clone());

    // Build the router
    let app = build_router(
        state,
        db_pool,
        catalog_service,
        credential_service,
        keychain_service,
        execution_service,
        runtime_service,
        replay_service,
        result_store_service,
        wallet_cipher_for_router,
    );

    // Bind to address
    let addr: SocketAddr = app_config.bind_address().parse()?;

    // Phase 4a (noetl/ai-meta#61): opt-in TLS / mTLS listener.  Plain HTTP
    // (unchanged default) unless NOETL_TLS_CERT + NOETL_TLS_KEY are set;
    // NOETL_TLS_CLIENT_CA additionally requires + verifies client certs (mTLS),
    // so the worker↔server credential channel is authenticated + encrypted.
    match noetl_server::tls::tls_params_from_env()? {
        Some(params) => {
            let mtls = params.mtls();
            let server_config = noetl_server::tls::build_server_config(&params)?;
            let rustls_config = axum_server::tls_rustls::RustlsConfig::from_config(
                std::sync::Arc::new(server_config),
            );
            tracing::info!(address = %addr, tls = true, mtls, "Server listening (TLS)");
            let handle = axum_server::Handle::new();
            let shutdown_handle = handle.clone();
            tokio::spawn(async move {
                shutdown_signal().await;
                shutdown_handle.graceful_shutdown(Some(std::time::Duration::from_secs(10)));
            });
            axum_server::bind_rustls(addr, rustls_config)
                .handle(handle)
                .serve(app.into_make_service())
                .await?;
        }
        None => {
            let listener = TcpListener::bind(addr).await?;
            tracing::info!(address = %addr, "Server listening");
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal())
                .await?;
        }
    }

    tracing::info!("Server shutdown complete");

    Ok(())
}

/// Wait for shutdown signal (Ctrl+C or SIGTERM).
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            tracing::info!("Received Ctrl+C, starting graceful shutdown");
        }
        _ = terminate => {
            tracing::info!("Received SIGTERM, starting graceful shutdown");
        }
    }
}

#[cfg(test)]
mod encryption_key_tests {
    use super::resolve_encryption_key;

    #[test]
    fn uses_explicit_key_when_present() {
        let k = resolve_encryption_key(Some("dGVzdC1rZXk=".to_string()), false).unwrap();
        assert_eq!(k, "dGVzdC1rZXk=");
    }

    #[test]
    fn fails_closed_when_absent_and_not_insecure() {
        assert!(resolve_encryption_key(None, false).is_err());
    }

    #[test]
    fn fails_closed_when_empty_and_not_insecure() {
        assert!(resolve_encryption_key(Some("   ".to_string()), false).is_err());
    }

    #[test]
    fn insecure_escape_hatch_generates_a_key() {
        // Random ephemeral key (base64 of 32 bytes) when the explicit flag is on.
        let k = resolve_encryption_key(None, true).unwrap();
        assert!(!k.is_empty());
        // Two calls produce different keys (random, not a shared constant).
        let k2 = resolve_encryption_key(None, true).unwrap();
        assert_ne!(k, k2);
    }

    #[test]
    fn explicit_key_wins_over_insecure_flag() {
        let k = resolve_encryption_key(Some("ZXhwbGljaXQ=".to_string()), true).unwrap();
        assert_eq!(k, "ZXhwbGljaXQ=");
    }
}
