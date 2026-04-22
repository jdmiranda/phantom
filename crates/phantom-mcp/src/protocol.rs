//! MCP wire format based on JSON-RPC 2.0.
//!
//! All MCP messages are JSON-RPC 2.0 requests, responses, or notifications.
//! This module defines the core message types and helper constructors used by
//! both the server and client halves of the crate.

use serde::{Deserialize, Serialize};

/// Standard JSON-RPC version string.
pub const JSONRPC_VERSION: &str = "2.0";

// ---------------------------------------------------------------------------
// JSON-RPC envelope types
// ---------------------------------------------------------------------------

/// A JSON-RPC 2.0 request (or notification when `id` is `None`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// A JSON-RPC 2.0 response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// MCP domain types
// ---------------------------------------------------------------------------

/// An MCP tool definition exposed by a server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Value,
}

/// An MCP resource definition exposed by a server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpResource {
    pub uri: String,
    pub name: String,
    pub description: String,
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

// ---------------------------------------------------------------------------
// Well-known JSON-RPC error codes
// ---------------------------------------------------------------------------

pub const PARSE_ERROR: i32 = -32700;
pub const INVALID_REQUEST: i32 = -32600;
pub const METHOD_NOT_FOUND: i32 = -32601;
pub const INVALID_PARAMS: i32 = -32602;
pub const INTERNAL_ERROR: i32 = -32603;

// ---------------------------------------------------------------------------
// Constructors
// ---------------------------------------------------------------------------

/// Build a JSON-RPC request with a numeric `id`.
pub fn create_request(id: u64, method: &str, params: serde_json::Value) -> JsonRpcRequest {
    JsonRpcRequest {
        jsonrpc: JSONRPC_VERSION.to_owned(),
        id: Some(serde_json::Value::Number(id.into())),
        method: method.to_owned(),
        params: Some(params),
    }
}

/// Build a successful JSON-RPC response.
pub fn create_response(id: serde_json::Value, result: serde_json::Value) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: JSONRPC_VERSION.to_owned(),
        id: Some(id),
        result: Some(result),
        error: None,
    }
}

/// Build an error JSON-RPC response.
pub fn create_error(id: serde_json::Value, code: i32, message: &str) -> JsonRpcResponse {
    JsonRpcResponse {
        jsonrpc: JSONRPC_VERSION.to_owned(),
        id: Some(id),
        result: None,
        error: Some(JsonRpcError {
            code,
            message: message.to_owned(),
            data: None,
        }),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn create_request_sets_version_and_id() {
        let req = create_request(42, "tools/list", json!({}));
        assert_eq!(req.jsonrpc, "2.0");
        assert_eq!(req.id, Some(json!(42)));
        assert_eq!(req.method, "tools/list");
    }

    #[test]
    fn create_request_stores_params() {
        let params = json!({"name": "phantom.run_command", "arguments": {"cmd": "ls"}});
        let req = create_request(1, "tools/call", params.clone());
        assert_eq!(req.params, Some(params));
    }

    #[test]
    fn create_response_has_no_error() {
        let resp = create_response(json!(1), json!({"ok": true}));
        assert!(resp.error.is_none());
        assert_eq!(resp.result, Some(json!({"ok": true})));
    }

    #[test]
    fn create_error_has_no_result() {
        let resp = create_error(json!(7), METHOD_NOT_FOUND, "unknown method");
        assert!(resp.result.is_none());
        let err = resp.error.unwrap();
        assert_eq!(err.code, METHOD_NOT_FOUND);
        assert_eq!(err.message, "unknown method");
    }

    #[test]
    fn request_roundtrips_through_json() {
        let req = create_request(99, "initialize", json!({"capabilities": {}}));
        let serialized = serde_json::to_string(&req).unwrap();
        let deser: JsonRpcRequest = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deser.jsonrpc, "2.0");
        assert_eq!(deser.method, "initialize");
        assert_eq!(deser.id, Some(json!(99)));
    }

    #[test]
    fn response_roundtrips_through_json() {
        let resp = create_response(json!(5), json!({"tools": []}));
        let serialized = serde_json::to_string(&resp).unwrap();
        let deser: JsonRpcResponse = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deser.id, Some(json!(5)));
        assert!(deser.error.is_none());
    }

    #[test]
    fn error_response_roundtrips_through_json() {
        let resp = create_error(json!(10), INTERNAL_ERROR, "boom");
        let serialized = serde_json::to_string(&resp).unwrap();
        let deser: JsonRpcResponse = serde_json::from_str(&serialized).unwrap();
        let err = deser.error.unwrap();
        assert_eq!(err.code, INTERNAL_ERROR);
        assert_eq!(err.message, "boom");
    }

    #[test]
    fn mcp_tool_serializes_correctly() {
        let tool = McpTool {
            name: "phantom.run_command".to_owned(),
            description: "Execute a shell command".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"}
                },
                "required": ["command"]
            }),
        };
        let val = serde_json::to_value(&tool).unwrap();
        assert_eq!(val["name"], "phantom.run_command");
        assert!(val["inputSchema"]["properties"]["command"].is_object());
    }

    #[test]
    fn mcp_resource_serializes_with_optional_mime() {
        let res = McpResource {
            uri: "phantom://terminal/state".to_owned(),
            name: "Terminal State".to_owned(),
            description: "Current terminal grid text".to_owned(),
            mime_type: Some("text/plain".to_owned()),
        };
        let val = serde_json::to_value(&res).unwrap();
        assert_eq!(val["mimeType"], "text/plain");

        let res_no_mime = McpResource {
            uri: "phantom://history/recent".to_owned(),
            name: "Recent History".to_owned(),
            description: "Recent commands".to_owned(),
            mime_type: None,
        };
        let val2 = serde_json::to_value(&res_no_mime).unwrap();
        assert!(val2.get("mimeType").is_none());
    }
}
