//! NoETL Control Plane Server
//!
//! An async Rust server that provides the control plane API for NoETL,
//! handling workflow orchestration, catalog management, and event processing.

use axum::{
    routing::{delete, get, post},
    Router,
};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use noetl_server::{
    config::{AppConfig, DatabaseConfig, ShardingConfig},
    db::{create_pool, DbPool, DbPoolMap},
    handlers,
    services::{
        CatalogService, CredentialService, ExecutionService, KeychainService, RuntimeService,
    },
    state::AppState,
};

/// Default encryption key for development (should be overridden in production).
const DEFAULT_ENCRYPTION_KEY: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

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
fn build_router(
    state: AppState,
    db_pool: DbPool,
    catalog_service: CatalogService,
    credential_service: CredentialService,
    keychain_service: KeychainService,
    execution_service: ExecutionService,
    runtime_service: RuntimeService,
) -> Router {
    // CORS configuration - allow all origins for development
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

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
        .route(
            "/api/catalog/{*tail}",
            get(handlers::catalog::ui_schema),
        )
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
        .with_state(credential_service);

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
            "/api/internal/events/project",
            post(handlers::internal::events_project),
        )
        .with_state(db_pool.clone());

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
        .merge(keychain_routes)
        .merge(execution_routes)
        .merge(executions_routes)
        .merge(variable_routes)
        .merge(runtime_routes)
        .merge(sharding_routes)
        .merge(database_routes)
        .merge(internal_routes)
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
        Some((user, password)) => async_nats::ConnectOptions::with_user_and_password(user, password)
            .connect(&clean_url),
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

/// Get encryption key from environment or use default.
fn get_encryption_key() -> String {
    std::env::var("NOETL_ENCRYPTION_KEY").unwrap_or_else(|_| {
        tracing::warn!("NOETL_ENCRYPTION_KEY not set, using default (not secure for production)");
        DEFAULT_ENCRYPTION_KEY.to_string()
    })
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

    // Get encryption key
    let encryption_key = get_encryption_key();

    // Create application state first so the snowflake generator
    // (Phase F R1.5 of noetl/ai-meta#49) is initialized once and
    // shared with the services below.  Services that need to mint
    // ids take a clone of `state.snowflake` (an `Arc`).
    let state = AppState::new(db_pool.clone(), pools, app_config.clone(), nats_client);

    // Create services
    let catalog_service = CatalogService::new(db_pool.clone());
    let credential_service = CredentialService::new(db_pool.clone(), &encryption_key)?;
    let keychain_service = KeychainService::new(db_pool.clone(), &encryption_key)?;
    // Phase F R4-4b: ExecutionService now takes the DbPoolMap so
    // its per-execution methods route via pool_for(execution_id)
    // and `list()` fan-outs via for_each_shard.  In single-pool
    // fallback mode this is the same handle as db_pool.
    let execution_service = ExecutionService::new(state.pools.clone(), state.snowflake.clone());
    let runtime_service = RuntimeService::new(db_pool.clone(), state.snowflake.clone());

    // Build the router
    let app = build_router(
        state,
        db_pool,
        catalog_service,
        credential_service,
        keychain_service,
        execution_service,
        runtime_service,
    );

    // Bind to address
    let addr: SocketAddr = app_config.bind_address().parse()?;
    let listener = TcpListener::bind(addr).await?;

    tracing::info!(address = %addr, "Server listening");

    // Run the server with graceful shutdown
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

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
