//! `phantom-hub` — Railway-hosted MCP fleet control hub.
//!
//! # Phase 1 scope
//!
//! This crate is a **scaffold only**. It stands up an HTTP server with the
//! correct route layout so subsequent issues can fill in logic without
//! restructuring. The following modules are intentionally stubbed:
//!
//! - [`registry`] — connection registry (issue #396)
//! - [`router`] — JSON-RPC frame router (issue #396)
//! - [`auth`] — device-token authentication (issue #398)
//!
//! The only production-ready endpoint is `GET /healthz`.

pub mod auth;
pub mod health;
pub mod mcp_endpoint;
pub mod phantom_endpoint;
pub mod registry;
pub mod router;

use anyhow::Result;
use axum::Router;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tracing::info;

/// Build the application router.
///
/// Route layout:
/// - `GET /healthz` — liveness / readiness probe
/// - `GET /phantom/connect` — Phantom-side WSS dial-in (stub, issue #396)
/// - `POST /mcp` — Claude-side MCP JSON-RPC (stub, issue #396/#397)
/// - `GET /mcp/sse` — Claude-side MCP SSE transport (stub, issue #397)
pub fn build_router() -> Router {
    Router::new()
        .route("/healthz", axum::routing::get(health::healthz))
        .route(
            "/phantom/connect",
            axum::routing::get(phantom_endpoint::connect),
        )
        .route("/mcp", axum::routing::post(mcp_endpoint::handle_jsonrpc))
        .route("/mcp/sse", axum::routing::get(mcp_endpoint::handle_sse))
}

/// Bind and serve the hub on `addr` until the process is killed.
pub async fn serve(addr: SocketAddr) -> Result<()> {
    let app = build_router();
    let listener = TcpListener::bind(addr).await?;
    info!("phantom-hub listening on {}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}
