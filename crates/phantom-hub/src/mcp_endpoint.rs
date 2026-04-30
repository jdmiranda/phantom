//! `POST /mcp` and `GET /mcp/sse` — Claude-side MCP transport.
//!
//! # SCAFFOLD — issues #396/#397 fill this in
//!
//! Phase 1 acceptance: both routes are registered and return `501 Not
//! Implemented`. Real MCP handling lands in issues #396 (JSON-RPC frame
//! routing) and #397 (SSE streaming transport).
//!
//! Expected flow once implemented:
//!
//! ## `POST /mcp` (JSON-RPC 2.0 batch/single)
//! 1. Validate API key via [`crate::auth`].
//! 2. Parse the JSON-RPC envelope.
//! 3. Resolve the target Phantom via `phantom_id` parameter.
//! 4. Forward the frame through [`crate::router`].
//! 5. Await the response and return it as JSON.
//!
//! ## `GET /mcp/sse` (Server-Sent Events)
//! 1. Validate API key via [`crate::auth`].
//! 2. Open an SSE stream.
//! 3. Push JSON-RPC responses as `data:` events as they arrive from the router.

use axum::http::StatusCode;
use axum::response::IntoResponse;

/// Handler for `POST /mcp` — Claude-side JSON-RPC 2.0 endpoint.
///
/// SCAFFOLD: returns `501 Not Implemented`. Real router in issue #396.
pub async fn handle_jsonrpc() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        "mcp: not yet implemented (issue #396)",
    )
}

/// Handler for `GET /mcp/sse` — Claude-side SSE transport.
///
/// SCAFFOLD: returns `501 Not Implemented`. Real SSE stream in issue #397.
pub async fn handle_sse() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        "mcp/sse: not yet implemented (issue #397)",
    )
}
