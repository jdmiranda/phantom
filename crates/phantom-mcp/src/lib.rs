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

pub mod client;
pub mod listener;
pub mod protocol;
pub mod server;

pub use client::McpClient;
pub use listener::{spawn as spawn_listener, AppCommand, McpListener, ScreenshotReply};
pub use protocol::{
    JsonRpcError, JsonRpcRequest, JsonRpcResponse, McpResource, McpTool,
    create_error, create_request, create_response,
};
pub use server::PhantomMcpServer;
