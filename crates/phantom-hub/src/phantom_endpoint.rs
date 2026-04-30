//! `GET /phantom/connect` — Phantom-side WSS dial-in.
//!
//! # SCAFFOLD — issue #396 fills this in
//!
//! Phase 1 acceptance: the route is registered and returns `501 Not
//! Implemented`. The real upgrade logic (WebSocket handshake, device-token
//! validation, registry insertion) lands in issue #396.
//!
//! Expected flow once implemented:
//! 1. Validate `Authorization: Bearer <device-token>` via [`crate::auth`].
//! 2. Upgrade HTTP connection to WebSocket.
//! 3. Insert a [`crate::registry::PhantomHandle`] into the
//!    [`crate::registry::Registry`].
//! 4. Drive the read/write loop, forwarding JSON-RPC frames via
//!    [`crate::router`].
//! 5. Remove the handle from the registry on disconnect.

use axum::http::StatusCode;
use axum::response::IntoResponse;

/// Handler for `GET /phantom/connect`.
///
/// SCAFFOLD: returns `501 Not Implemented`. Real WSS upgrade in issue #396.
pub async fn connect() -> impl IntoResponse {
    (
        StatusCode::NOT_IMPLEMENTED,
        "phantom/connect: not yet implemented (issue #396)",
    )
}
