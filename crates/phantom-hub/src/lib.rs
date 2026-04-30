//! `phantom-hub` — Railway-hosted MCP fleet control hub.
//!
//! # Phase 1 scope
//!
//! - [`registry`] — connection registry with live `ConnState` per Phantom
//! - [`router`] — JSON-RPC frame router with id rewriting, timeout, backpressure
//! - [`phantom_endpoint`] — WSS upgrade handler: binary relay-envelope
//!   handshake (HELLO/HELLO_ACK), JWT verification, inbound/outbound loop
//! - [`health`] — `GET /healthz` liveness probe (always-on)
//! - [`auth`] — JWT device token issuance/verification and API key validation
//!   (live, issue #398)
//! - [`mcp_endpoint`] — `POST /mcp` and `GET /mcp/sse` stubs (issue #397)
//!
//! # Authentication (issue #398)
//!
//! [`auth::JwtAuthority`] and [`auth::ApiKeyStore`] are initialised from
//! environment variables at startup and injected into [`AppState`].  Every
//! handler receives [`AppState`] via `axum::extract::State`.
//!
//! See [`auth`] for the full token model, library choice, and threat model.
//!
//! # Protocol
//!
//! `GET /phantom/connect` speaks **binary relay-envelope** — the same
//! protocol implemented by `phantom-net::RelayClient` / `phantom-mcp::hub_listener`.
//! See [`phantom_endpoint`] for the handshake sequence.
//!
//! # Debug endpoint
//!
//! When the `HUB_REGISTRY_DEBUG` environment variable is set to `1`, a
//! `GET /registry` endpoint is available that returns the list of currently
//! connected Phantom peer IDs. This is intentionally scoped behind an env flag
//! and must not be exposed in production without auth (pre-staging issue).

pub mod auth;
pub mod health;
pub mod mcp_endpoint;
pub mod phantom_endpoint;
pub mod registry;
pub mod router;

use std::sync::Arc;

use anyhow::Result;
use axum::{Router, extract::State, response::IntoResponse, Json};
use registry::SharedRegistry;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing::info;

// ---------------------------------------------------------------------------
// AppState — shared across all request handlers
// ---------------------------------------------------------------------------

/// Shared hub state injected into every Axum handler.
///
/// Cloning this is cheap — all expensive fields are behind [`Arc`].
///
/// The `nonce_cache` field enforces single-use semantics on registration
/// nonces (replay protection, issue #398).  It is wrapped in `Arc` so that
/// cloned `AppState` values — including those shared across unit-test calls —
/// all operate on the same underlying cache.
#[derive(Clone)]
pub struct AppState {
    /// JWT authority — issues and verifies Phantom device tokens.
    pub jwt: Arc<auth::JwtAuthority>,
    /// API key store — validates Claude session API keys.
    pub api_keys: Arc<auth::ApiKeyStore>,
    /// Nonce replay-protection cache.  Every nonce presented at registration
    /// is recorded here; a second presentation of the same nonce within the
    /// TTL window is rejected with `409 Conflict`.
    pub nonce_cache: Arc<auth::NonceCache>,
    /// Connection registry — tracks live Phantom WebSocket connections.
    pub registry: SharedRegistry,
}

impl AppState {
    /// Construct [`AppState`] from environment variables.
    ///
    /// Reads `HUB_JWT_SECRET` (required) and `HUB_API_KEYS` (optional).
    /// The [`auth::NonceCache`] is always initialised with production defaults
    /// (capacity [`auth::NONCE_CACHE_CAPACITY`], TTL [`auth::NONCE_CACHE_TTL`]).
    ///
    /// # Errors
    ///
    /// Returns an error if `HUB_JWT_SECRET` is absent or empty.
    pub fn from_env() -> Result<Self> {
        let jwt = auth::JwtAuthority::from_env()?;
        let api_keys = auth::ApiKeyStore::from_env();
        Ok(Self {
            jwt: Arc::new(jwt),
            api_keys: Arc::new(api_keys),
            nonce_cache: Arc::new(auth::NonceCache::new()),
            registry: registry::new_shared(),
        })
    }
}

// ---------------------------------------------------------------------------
// Debug registry endpoint
// ---------------------------------------------------------------------------

/// Handler for `GET /registry` — returns connected peer IDs.
///
/// Gated behind `HUB_REGISTRY_DEBUG=1`. Must not be exposed without auth
/// before any staging deployment.
async fn registry_debug(State(state): State<AppState>) -> impl IntoResponse {
    let reg = state.registry.read().await;
    let peers: Vec<serde_json::Value> = reg
        .list_online()
        .into_iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id.0,
                "host": p.host,
                "version": p.version,
                "last_seen_secs_ago": p.last_seen_secs_ago,
            })
        })
        .collect();
    Json(serde_json::json!({ "phantoms": peers }))
}

// ---------------------------------------------------------------------------
// Router builder
// ---------------------------------------------------------------------------

/// Build the application router with `state` injected into every handler.
///
/// Route layout:
/// - `GET  /healthz`         — liveness / readiness probe
/// - `POST /auth/register`   — Phantom registration + JWT issuance (issue #398)
/// - `GET  /phantom/connect` — Phantom-side WSS dial-in (binary relay-envelope)
/// - `POST /mcp`             — Claude-side MCP JSON-RPC (stub, issue #397)
/// - `GET  /mcp/sse`         — Claude-side MCP SSE transport (stub, issue #397)
/// - `GET  /registry`        — debug endpoint (only when `HUB_REGISTRY_DEBUG=1`)
pub fn build_router(state: AppState) -> Router {
    let mut app = Router::new()
        .route("/healthz", axum::routing::get(health::healthz))
        .route(
            "/auth/register",
            axum::routing::post(phantom_endpoint::register),
        )
        .route(
            "/phantom/connect",
            axum::routing::get(phantom_endpoint::connect),
        )
        .route("/mcp", axum::routing::post(mcp_endpoint::handle_jsonrpc))
        .route("/mcp/sse", axum::routing::get(mcp_endpoint::handle_sse))
        .with_state(state.clone());

    if std::env::var("HUB_REGISTRY_DEBUG").as_deref() == Ok("1") {
        app = app.route(
            "/registry",
            axum::routing::get(registry_debug).with_state(state),
        );
    }

    app
}

/// Bind and serve the hub on `addr` until the process is killed.
///
/// # Errors
///
/// Returns an error if `HUB_JWT_SECRET` is absent, the TCP listener cannot
/// bind to `addr`, or the server fails after startup.
pub async fn serve(addr: SocketAddr) -> Result<()> {
    let state = AppState::from_env()?;
    let app = build_router(state);
    let listener = TcpListener::bind(addr).await?;
    info!("phantom-hub listening on {}", addr);
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
