//! `phantom-hub` — Railway-hosted MCP fleet control hub.
//!
//! # Phase 1 scope
//!
//! This crate provides an HTTP server with the core route layout.
//! Authentication (issue #398) is live.  The following modules remain
//! stubbed for their respective issues:
//!
//! - [`registry`] — connection registry (issue #396)
//! - [`router`] — JSON-RPC frame router (issue #396)
//!
//! # Authentication (issue #398)
//!
//! [`auth::JwtAuthority`] and [`auth::ApiKeyStore`] are initialised from
//! environment variables at startup and injected into [`AppState`].  Every
//! handler that touches a Phantom connection or an MCP endpoint receives
//! [`AppState`] via `axum::extract::State`.
//!
//! See [`auth`] for the full token model, library choice, and threat model.

pub mod auth;
pub mod health;
pub mod mcp_endpoint;
pub mod phantom_endpoint;
pub mod registry;
pub mod router;

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing::info;

// ---------------------------------------------------------------------------
// AppState — shared across all request handlers
// ---------------------------------------------------------------------------

/// Shared hub state injected into every Axum handler.
///
/// Cloning this is cheap — the expensive fields are behind [`Arc`].
#[derive(Clone)]
pub struct AppState {
    /// JWT authority — issues and verifies Phantom device tokens.
    pub jwt: Arc<auth::JwtAuthority>,
    /// API key store — validates Claude session API keys.
    pub api_keys: Arc<auth::ApiKeyStore>,
}

impl AppState {
    /// Construct [`AppState`] from environment variables.
    ///
    /// Reads `HUB_JWT_SECRET` (required) and `HUB_API_KEYS` (optional).
    ///
    /// # Errors
    ///
    /// Returns an error if `HUB_JWT_SECRET` is absent or empty.  Callers
    /// (typically `main`) treat this as a fatal startup error.
    pub fn from_env() -> Result<Self> {
        let jwt = auth::JwtAuthority::from_env()?;
        let api_keys = auth::ApiKeyStore::from_env();
        Ok(Self {
            jwt: Arc::new(jwt),
            api_keys: Arc::new(api_keys),
        })
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Build the application router with `state` injected into every handler.
///
/// Route layout:
/// - `GET  /healthz`         — liveness / readiness probe
/// - `POST /auth/register`   — Phantom registration + JWT issuance (issue #398)
/// - `GET  /phantom/connect` — Phantom-side WSS dial-in (stub, issue #396)
/// - `POST /mcp`             — Claude-side MCP JSON-RPC (stub, issue #396/#397)
/// - `GET  /mcp/sse`         — Claude-side MCP SSE transport (stub, issue #397)
pub fn build_router(state: AppState) -> Router {
    Router::new()
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
        .with_state(state)
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
    axum::serve(listener, app).await?;
    Ok(())
}
