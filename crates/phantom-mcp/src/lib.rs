//! MCP (Model Context Protocol) support for Phantom.
//!
//! Phantom participates in the MCP ecosystem in two roles:
//!
//! - **Server** (`PhantomMcpServer`): exposes terminal capabilities so that
//!   external AI clients (Claude Code, etc.) can drive Phantom.
//! - **Client** (`McpClient`): connects to external MCP servers so that
//!   Phantom's own agents can use third-party tools.
//!
//! The wire format is JSON-RPC 2.0 as defined by the MCP specification.
//! The client transport is WebSocket (text frames containing JSON-RPC 2.0
//! objects). The server uses Unix domain sockets (newline-delimited JSON).

pub mod client;
pub mod error;
pub mod listener;
pub mod protocol;
pub mod registry;
pub mod server;

pub use client::{McpClient, McpResourceDef, McpToolDef};
pub use error::McpError;
pub use listener::{spawn as spawn_listener, AppCommand, McpListener, ScreenshotReply};
pub use protocol::{
    JsonRpcError, JsonRpcRequest, JsonRpcResponse, McpResource, McpTool,
    create_error, create_request, create_response,
};
pub use registry::{McpToolRegistry, McpToolRoute, ToolProvenance};
pub use server::PhantomMcpServer;
