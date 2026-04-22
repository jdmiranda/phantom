//! Phantom as an MCP client.
//!
//! Phantom connects to external MCP servers so its agents can invoke
//! third-party tools (file systems, databases, APIs, etc.). This module
//! owns the client-side handshake, tool discovery, and call construction.

use serde_json::json;

use crate::protocol::{self, JsonRpcRequest, JsonRpcResponse, McpTool};

/// Monotonically increasing request ID generator.
///
/// In a real async transport this would be `AtomicU64`; the synchronous
/// counter is fine for the message-construction layer.
static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// MCP client that connects Phantom's agent runtime to external MCP servers.
pub struct McpClient {
    server_tools: Vec<McpTool>,
    initialized: bool,
}

impl McpClient {
    /// Create a new, uninitialized client.
    pub fn new() -> Self {
        Self {
            server_tools: Vec::new(),
            initialized: false,
        }
    }

    /// Build the `initialize` handshake request.
    pub fn initialize(&mut self) -> JsonRpcRequest {
        protocol::create_request(
            next_id(),
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {
                    "name": "phantom",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        )
    }

    /// Process the server's `initialize` response, marking the client as
    /// ready.
    pub fn handle_initialize_response(&mut self, response: &JsonRpcResponse) {
        if response.error.is_none() && response.result.is_some() {
            self.initialized = true;
            log::info!("MCP client initialized");
        } else {
            log::warn!("MCP initialize failed: {:?}", response.error);
        }
    }

    /// Build a `tools/list` request.
    pub fn list_tools_request(&self) -> JsonRpcRequest {
        protocol::create_request(next_id(), "tools/list", json!({}))
    }

    /// Process a `tools/list` response, storing discovered tools.
    pub fn handle_tools_response(&mut self, response: &JsonRpcResponse) {
        let Some(result) = &response.result else {
            log::warn!("tools/list returned no result");
            return;
        };
        let Some(tools_val) = result.get("tools") else {
            log::warn!("tools/list result missing 'tools' key");
            return;
        };
        match serde_json::from_value::<Vec<McpTool>>(tools_val.clone()) {
            Ok(tools) => {
                log::info!("discovered {} tools from remote server", tools.len());
                self.server_tools = tools;
            }
            Err(e) => {
                log::warn!("failed to parse tools list: {e}");
            }
        }
    }

    /// Build a `tools/call` request for the given tool name and arguments.
    pub fn call_tool_request(&self, name: &str, args: serde_json::Value) -> JsonRpcRequest {
        protocol::create_request(
            next_id(),
            "tools/call",
            json!({
                "name": name,
                "arguments": args,
            }),
        )
    }

    /// Tools discovered from the remote server.
    pub fn available_tools(&self) -> &[McpTool] {
        &self.server_tools
    }

    /// Whether the initialize handshake completed successfully.
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }
}

impl Default for McpClient {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol;
    use serde_json::json;

    #[test]
    fn initialize_produces_valid_request() {
        let mut client = McpClient::new();
        let req = client.initialize();
        assert_eq!(req.method, "initialize");
        assert_eq!(req.jsonrpc, "2.0");
        let params = req.params.unwrap();
        assert_eq!(params["clientInfo"]["name"], "phantom");
    }

    #[test]
    fn handle_initialize_success() {
        let mut client = McpClient::new();
        assert!(!client.is_initialized());

        let resp = protocol::create_response(json!(1), json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "test-server", "version": "0.1.0"},
        }));
        client.handle_initialize_response(&resp);
        assert!(client.is_initialized());
    }

    #[test]
    fn handle_initialize_error_stays_uninitialized() {
        let mut client = McpClient::new();
        let resp = protocol::create_error(json!(1), -32600, "bad request");
        client.handle_initialize_response(&resp);
        assert!(!client.is_initialized());
    }

    #[test]
    fn tools_list_roundtrip() {
        let mut client = McpClient::new();
        let _req = client.list_tools_request();

        let resp = protocol::create_response(json!(2), json!({
            "tools": [
                {
                    "name": "fs.read_file",
                    "description": "Read a file from disk",
                    "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}},
                },
                {
                    "name": "fs.write_file",
                    "description": "Write a file to disk",
                    "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}}},
                },
            ]
        }));
        client.handle_tools_response(&resp);
        assert_eq!(client.available_tools().len(), 2);
        assert_eq!(client.available_tools()[0].name, "fs.read_file");
    }

    #[test]
    fn call_tool_request_has_correct_shape() {
        let client = McpClient::new();
        let req = client.call_tool_request("fs.read_file", json!({"path": "/tmp/test.txt"}));
        assert_eq!(req.method, "tools/call");
        let params = req.params.unwrap();
        assert_eq!(params["name"], "fs.read_file");
        assert_eq!(params["arguments"]["path"], "/tmp/test.txt");
    }

    #[test]
    fn ids_are_unique_across_calls() {
        let mut client = McpClient::new();
        let r1 = client.initialize();
        let r2 = client.list_tools_request();
        let r3 = client.call_tool_request("x", json!({}));
        let ids: Vec<_> = [r1, r2, r3].iter().map(|r| r.id.clone().unwrap()).collect();
        // All three should be distinct numbers.
        assert_ne!(ids[0], ids[1]);
        assert_ne!(ids[1], ids[2]);
        assert_ne!(ids[0], ids[2]);
    }

    #[test]
    fn handle_tools_response_tolerates_missing_tools_key() {
        let mut client = McpClient::new();
        let resp = protocol::create_response(json!(3), json!({"something_else": []}));
        client.handle_tools_response(&resp);
        assert!(client.available_tools().is_empty());
    }
}
