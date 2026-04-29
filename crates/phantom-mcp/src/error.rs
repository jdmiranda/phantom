//! Error types for the `phantom-mcp` crate.
//!
//! [`McpError`] is the single error type shared across the client, registry,
//! and server layers. Variants cover both transport-level failures and
//! higher-level routing errors surfaced by [`crate::registry::McpToolRegistry`].

use thiserror::Error;

/// All failure modes in the MCP subsystem.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
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

    /// No registered server advertises the requested tool name.
    #[error("unknown MCP tool: {name}")]
    UnknownTool { name: String },

    /// The client produced an error response or the call could not be
    /// completed. `tool` names the tool being invoked; `detail` carries the
    /// error message from the server or a description of the local failure.
    #[error("MCP invoke error for tool '{tool}': {detail}")]
    InvokeError { tool: String, detail: String },
}
