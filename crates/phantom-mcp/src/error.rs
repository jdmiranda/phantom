//! Error types for the MCP client.
//!
//! `McpError` is distinct from Phantom's internal `anyhow` errors so that
//! callers can match on specific failure modes (transport, timeout, server-
//! side JSON-RPC errors) without stringly-typed inspection.

use thiserror::Error;

/// Errors that can be returned by [`McpClient`](crate::client::McpClient)
/// operations.
#[derive(Debug, Error)]
pub enum McpError {
    /// A TCP/WebSocket-level failure (connect refused, connection reset, etc.).
    #[error("MCP transport error: {0}")]
    Transport(String),

    /// The server returned a JSON-RPC error object.
    #[error("MCP server error {code}: {message}")]
    ServerError {
        /// JSON-RPC error code (e.g. -32601 for method-not-found).
        code: i32,
        /// Human-readable error message from the server.
        message: String,
    },

    /// No response arrived within the request timeout window.
    #[error("MCP request timed out waiting for response to '{method}'")]
    Timeout {
        /// The JSON-RPC method that timed out.
        method: String,
    },

    /// A message could not be serialized before sending.
    #[error("MCP serialization error: {0}")]
    Serialization(String),

    /// The client is not yet connected (e.g. `connect` was never called or the
    /// connection has been lost and not re-established).
    #[error("MCP client not connected")]
    NotConnected,
}
