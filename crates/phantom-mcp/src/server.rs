//! Phantom as an MCP server.
//!
//! When an external MCP client (e.g. Claude Code) connects, Phantom
//! advertises terminal-native tools and resources. This module owns the
//! request dispatch loop and tool/resource registries.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::protocol::{
    self, JsonRpcRequest, JsonRpcResponse, McpResource, McpTool, INVALID_PARAMS,
    METHOD_NOT_FOUND,
};

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Phantom's MCP server — exposes terminal capabilities to external AI clients.
pub struct PhantomMcpServer {
    tools: Vec<McpTool>,
    resources: Vec<McpResource>,
}

/// Parameters for `tools/call`.
#[derive(Debug, Deserialize)]
struct ToolCallParams {
    name: String,
    #[serde(default)]
    arguments: serde_json::Value,
}

/// Parameters for `resources/read`.
#[derive(Debug, Deserialize)]
struct ResourceReadParams {
    uri: String,
}

/// MCP server info returned during initialization.
#[derive(Debug, Serialize)]
struct ServerInfo {
    name: String,
    version: String,
}

impl PhantomMcpServer {
    /// Create a new server pre-populated with Phantom's built-in tools and
    /// resources.
    pub fn new() -> Self {
        Self {
            tools: builtin_tools(),
            resources: builtin_resources(),
        }
    }

    // -- public API --------------------------------------------------------

    /// Dispatch an incoming JSON-RPC request to the appropriate handler.
    pub fn handle_request(&self, request: &JsonRpcRequest) -> JsonRpcResponse {
        let id = request.id.clone().unwrap_or(serde_json::Value::Null);

        match request.method.as_str() {
            "initialize" => self.handle_initialize(id),
            "tools/list" => {
                let result = self.handle_tools_list();
                protocol::create_response(id, result)
            }
            "tools/call" => {
                let Some(params) = &request.params else {
                    return protocol::create_error(id, INVALID_PARAMS, "missing params");
                };
                match serde_json::from_value::<ToolCallParams>(params.clone()) {
                    Ok(p) => {
                        let result = self.handle_tool_call(&p.name, &p.arguments);
                        protocol::create_response(id, result)
                    }
                    Err(e) => {
                        protocol::create_error(id, INVALID_PARAMS, &format!("bad params: {e}"))
                    }
                }
            }
            "resources/list" => {
                let result = self.handle_resources_list();
                protocol::create_response(id, result)
            }
            "resources/read" => {
                let Some(params) = &request.params else {
                    return protocol::create_error(id, INVALID_PARAMS, "missing params");
                };
                match serde_json::from_value::<ResourceReadParams>(params.clone()) {
                    Ok(p) => {
                        let result = self.handle_resource_read(&p.uri);
                        protocol::create_response(id, result)
                    }
                    Err(e) => {
                        protocol::create_error(id, INVALID_PARAMS, &format!("bad params: {e}"))
                    }
                }
            }
            other => protocol::create_error(
                id,
                METHOD_NOT_FOUND,
                &format!("unknown method: {other}"),
            ),
        }
    }

    /// Return the tools registered on this server.
    pub fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    /// Return the resources registered on this server.
    pub fn resources(&self) -> &[McpResource] {
        &self.resources
    }

    // -- private handlers --------------------------------------------------

    fn handle_initialize(&self, id: serde_json::Value) -> JsonRpcResponse {
        let info = ServerInfo {
            name: "phantom".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        protocol::create_response(
            id,
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "tools": {},
                    "resources": {},
                },
                "serverInfo": info,
            }),
        )
    }

    fn handle_tools_list(&self) -> serde_json::Value {
        json!({ "tools": self.tools })
    }

    fn handle_tool_call(&self, name: &str, args: &serde_json::Value) -> serde_json::Value {
        // Verify the tool exists.
        let known = self.tools.iter().any(|t| t.name == name);
        if !known {
            return json!({
                "content": [{
                    "type": "text",
                    "text": format!("unknown tool: {name}"),
                }],
                "isError": true,
            });
        }

        // Dispatch to stub implementations. Real implementations will be
        // wired in when the terminal runtime is connected.
        match name {
            "phantom.run_command" => {
                let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
                json!({
                    "content": [{
                        "type": "text",
                        "text": format!("[stub] would execute: {cmd}"),
                    }],
                })
            }
            "phantom.read_output" => json!({
                "content": [{"type": "text", "text": "[stub] no output captured yet"}],
            }),
            "phantom.screenshot" => json!({
                "content": [{"type": "text", "text": "[stub] screenshot not available"}],
            }),
            "phantom.split_pane" => {
                let direction = args.get("direction").and_then(|v| v.as_str()).unwrap_or("horizontal");
                json!({
                    "content": [{"type": "text", "text": format!("[stub] would split pane {direction}")}],
                })
            }
            "phantom.get_context" => json!({
                "content": [{"type": "text", "text": "[stub] project context unavailable"}],
            }),
            "phantom.get_memory" => {
                let key = args.get("key").and_then(|v| v.as_str()).unwrap_or("");
                json!({
                    "content": [{"type": "text", "text": format!("[stub] memory read: {key}")}],
                })
            }
            "phantom.set_memory" => {
                let key = args.get("key").and_then(|v| v.as_str()).unwrap_or("");
                json!({
                    "content": [{"type": "text", "text": format!("[stub] memory written: {key}")}],
                })
            }
            _ => json!({
                "content": [{"type": "text", "text": format!("unimplemented tool: {name}")}],
                "isError": true,
            }),
        }
    }

    fn handle_resources_list(&self) -> serde_json::Value {
        json!({ "resources": self.resources })
    }

    fn handle_resource_read(&self, uri: &str) -> serde_json::Value {
        match uri {
            "phantom://terminal/state" => json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "text/plain",
                    "text": "[stub] terminal state",
                }],
            }),
            "phantom://project/context" => json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": "[stub] project context",
                }],
            }),
            "phantom://history/recent" => json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": "[stub] recent history",
                }],
            }),
            _ => json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "text/plain",
                    "text": format!("unknown resource: {uri}"),
                }],
                "isError": true,
            }),
        }
    }
}

impl Default for PhantomMcpServer {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Built-in definitions
// ---------------------------------------------------------------------------

fn builtin_tools() -> Vec<McpTool> {
    vec![
        McpTool {
            name: "phantom.run_command".to_owned(),
            description: "Execute a shell command in a pane".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The shell command to run"},
                    "pane_id": {"type": "string", "description": "Target pane (optional)"},
                },
                "required": ["command"],
            }),
        },
        McpTool {
            name: "phantom.read_output".to_owned(),
            description: "Get the last command's parsed output".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pane_id": {"type": "string", "description": "Pane to read from (optional)"},
                    "lines": {"type": "integer", "description": "Number of lines to read"},
                },
            }),
        },
        McpTool {
            name: "phantom.screenshot".to_owned(),
            description: "Capture the terminal state as text".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pane_id": {"type": "string", "description": "Pane to capture (optional)"},
                },
            }),
        },
        McpTool {
            name: "phantom.send_key".to_owned(),
            description: "Send a keypress to the app. State-aware: dismisses the boot screen if active; otherwise goes to the focused pane's PTY. Named keys supported: Enter, Tab, Escape, Space, Backspace, Up, Down, Left, Right. Anything else is sent verbatim.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string", "description": "Named key or literal character(s) to send"},
                },
                "required": ["key"],
            }),
        },
        McpTool {
            name: "phantom.split_pane".to_owned(),
            description: "Create a new pane by splitting an existing one".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "direction": {
                        "type": "string",
                        "enum": ["horizontal", "vertical"],
                        "description": "Split direction",
                    },
                    "pane_id": {"type": "string", "description": "Pane to split (optional)"},
                },
            }),
        },
        McpTool {
            name: "phantom.get_context".to_owned(),
            description: "Get project context (language, framework, etc.)".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {},
            }),
        },
        McpTool {
            name: "phantom.get_memory".to_owned(),
            description: "Read a value from project memory".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string", "description": "Memory key to read"},
                },
                "required": ["key"],
            }),
        },
        McpTool {
            name: "phantom.set_memory".to_owned(),
            description: "Write a value to project memory".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string", "description": "Memory key"},
                    "value": {"type": "string", "description": "Value to store"},
                },
                "required": ["key", "value"],
            }),
        },
        McpTool {
            name: "phantom.command".to_owned(),
            description: "Execute a Phantom command (same as backtick mode). Commands: theme <name>, debug, plain, boot, agent <prompt>, reload, quit.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The Phantom command to execute (e.g. 'theme pipboy', 'debug', 'agent fix the bug')"},
                },
                "required": ["command"],
            }),
        },
    ]
}

fn builtin_resources() -> Vec<McpResource> {
    vec![
        McpResource {
            uri: "phantom://terminal/state".to_owned(),
            name: "Terminal State".to_owned(),
            description: "Current terminal grid text".to_owned(),
            mime_type: Some("text/plain".to_owned()),
        },
        McpResource {
            uri: "phantom://project/context".to_owned(),
            name: "Project Context".to_owned(),
            description: "Project detection info (language, framework, etc.)".to_owned(),
            mime_type: Some("application/json".to_owned()),
        },
        McpResource {
            uri: "phantom://history/recent".to_owned(),
            name: "Recent History".to_owned(),
            description: "Recent command history".to_owned(),
            mime_type: Some("application/json".to_owned()),
        },
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::create_request;

    fn server() -> PhantomMcpServer {
        PhantomMcpServer::new()
    }

    #[test]
    fn initialize_returns_server_info() {
        let s = server();
        let req = create_request(1, "initialize", json!({}));
        let resp = s.handle_request(&req);
        let result = resp.result.unwrap();
        assert_eq!(result["serverInfo"]["name"], "phantom");
        assert!(result["capabilities"]["tools"].is_object());
        assert!(result["capabilities"]["resources"].is_object());
    }

    #[test]
    fn tools_list_returns_all_builtin_tools() {
        let s = server();
        let req = create_request(2, "tools/list", json!({}));
        let resp = s.handle_request(&req);
        let tools = resp.result.unwrap()["tools"].as_array().unwrap().clone();
        assert_eq!(tools.len(), 9);
        let names: Vec<String> = tools.iter().map(|t| t["name"].as_str().unwrap().to_owned()).collect();
        assert!(names.contains(&"phantom.run_command".to_owned()));
        assert!(names.contains(&"phantom.screenshot".to_owned()));
        assert!(names.contains(&"phantom.set_memory".to_owned()));
    }

    #[test]
    fn tool_call_run_command() {
        let s = server();
        let req = create_request(3, "tools/call", json!({
            "name": "phantom.run_command",
            "arguments": {"command": "ls -la"}
        }));
        let resp = s.handle_request(&req);
        let text = resp.result.unwrap()["content"][0]["text"].as_str().unwrap().to_owned();
        assert!(text.contains("ls -la"));
    }

    #[test]
    fn tool_call_unknown_tool_returns_error_content() {
        let s = server();
        let req = create_request(4, "tools/call", json!({
            "name": "phantom.nonexistent",
            "arguments": {}
        }));
        let resp = s.handle_request(&req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn resources_list_returns_all_builtin_resources() {
        let s = server();
        let req = create_request(5, "resources/list", json!({}));
        let resp = s.handle_request(&req);
        let resources = resp.result.unwrap()["resources"].as_array().unwrap().clone();
        assert_eq!(resources.len(), 3);
    }

    #[test]
    fn resource_read_terminal_state() {
        let s = server();
        let req = create_request(6, "resources/read", json!({"uri": "phantom://terminal/state"}));
        let resp = s.handle_request(&req);
        let contents = &resp.result.unwrap()["contents"];
        assert_eq!(contents[0]["uri"], "phantom://terminal/state");
    }

    #[test]
    fn resource_read_unknown_uri() {
        let s = server();
        let req = create_request(7, "resources/read", json!({"uri": "phantom://nope"}));
        let resp = s.handle_request(&req);
        let result = resp.result.unwrap();
        assert_eq!(result["isError"], true);
    }

    #[test]
    fn unknown_method_returns_error() {
        let s = server();
        let req = create_request(8, "bogus/method", json!({}));
        let resp = s.handle_request(&req);
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, METHOD_NOT_FOUND);
    }

    #[test]
    fn tools_call_missing_params_returns_error() {
        let s = server();
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_owned(),
            id: Some(json!(9)),
            method: "tools/call".to_owned(),
            params: None,
        };
        let resp = s.handle_request(&req);
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, INVALID_PARAMS);
    }

    #[test]
    fn server_has_eight_tools_and_three_resources() {
        let s = server();
        assert_eq!(s.tools().len(), 9);
        assert_eq!(s.resources().len(), 3);
    }
}
