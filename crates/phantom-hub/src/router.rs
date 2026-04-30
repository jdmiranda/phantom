//! JSON-RPC frame router — routes MCP tool calls to the right Phantom.
//!
//! # SCAFFOLD — issue #396 fills this in
//!
//! Phase 1: type definitions only. The router accepts no real frames.
//! Issue #396 wires the full forwarding pipeline:
//! `mcp_endpoint` → `Router::dispatch` → `PhantomHandle::send` → response.

use crate::registry::PhantomId;
use serde::{Deserialize, Serialize};

/// A JSON-RPC 2.0 request frame.
///
/// SCAFFOLD: used as the canonical in-memory type. Issue #396 adds
/// deserialization from the HTTP body and forwarding logic.
#[derive(Debug, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// A JSON-RPC 2.0 response frame.
///
/// SCAFFOLD: issue #396 constructs real instances from Phantom replies.
#[derive(Debug, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl JsonRpcError {
    /// Standard "method not found" error (code -32601).
    #[must_use]
    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("Method not found: {method}"),
            data: None,
        }
    }

    /// Standard "not implemented yet" error for scaffold stubs.
    #[must_use]
    pub fn not_implemented(issue: &str) -> Self {
        Self {
            code: -32000,
            message: format!("Not implemented (see issue {issue})"),
            data: None,
        }
    }
}

/// Frame router.
///
/// SCAFFOLD: no dispatch logic yet. Issue #396 adds:
/// - `dispatch(&self, phantom_id: &PhantomId, request: JsonRpcRequest) -> JsonRpcResponse`
/// - Timeout handling
/// - Correlation-id tracking for in-flight requests
#[derive(Debug, Default)]
pub struct FrameRouter;

impl FrameRouter {
    /// Create a new router.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// Placeholder — returns "not implemented" for every call.
    ///
    /// SCAFFOLD: real dispatch in issue #396.
    #[must_use]
    pub fn dispatch(&self, _phantom_id: &PhantomId, request: &JsonRpcRequest) -> JsonRpcResponse {
        JsonRpcResponse {
            jsonrpc: "2.0".into(),
            id: request.id.clone(),
            result: None,
            error: Some(JsonRpcError::not_implemented("#396")),
        }
    }
}
