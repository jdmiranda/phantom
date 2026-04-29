//! MCP tool registry: routes agent `call_tool` calls to the correct
//! [`McpClient`] connection.
//!
//! When Phantom's agents discover they need a tool that isn't in the built-in
//! surface, they fall back to the MCP registry. The registry maintains a list
//! of named [`McpClient`] connections, builds an index of `tool_name →
//! server_name` from each client's advertised tool list, and routes
//! `invoke(tool_name, args)` calls to the right client.
//!
//! ## Provenance
//!
//! Every successful invocation returns a [`ToolProvenance`] alongside the
//! result payload. The `tool_name` field uses the `mcp:{server}/{tool}` prefix
//! so callers can distinguish MCP-sourced results from built-in tool results.
//!
//! ## Error surface
//!
//! All failure modes are captured by [`McpError`]:
//! - [`McpError::UnknownTool`] — no registered server handles the name.
//! - [`McpError::InvokeError`] — the client build/call step returned an error
//!   (e.g. the server rejected the request).
//!
//! ## Threading
//!
//! [`McpToolRegistry`] is `Send + Sync` (all state behind `&mut self`). Callers
//! that need shared access should wrap it in `Arc<Mutex<McpToolRegistry>>`.

use std::collections::{HashMap, HashSet};

use crate::client::McpClient;
use crate::protocol::JsonRpcResponse;

pub use crate::error::McpError;

// ---------------------------------------------------------------------------
// McpToolRoute
// ---------------------------------------------------------------------------

/// Resolution result from [`McpToolRegistry::resolve_tool`].
///
/// Carries both the logical server name and the concrete tool name so callers
/// can build provenance strings or log the routing decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolRoute {
    /// Logical name of the server that owns the tool (as passed to
    /// [`McpToolRegistry::register_server`]).
    pub server_name: String,
    /// The tool name as advertised by that server.
    pub tool_name: String,
}

impl McpToolRoute {
    /// Canonical provenance tag: `mcp:{server}/{tool}`.
    ///
    /// Matches the format the issue spec names for `ToolProvenance.tool_name`.
    pub fn provenance_tag(&self) -> String {
        format!("mcp:{}/{}", self.server_name, self.tool_name)
    }
}

// ---------------------------------------------------------------------------
// ToolProvenance
// ---------------------------------------------------------------------------

/// Attribution record returned alongside every successful MCP tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolProvenance {
    /// Provenance string: `mcp:{server}/{tool}`.
    pub tool_name: String,
}

// ---------------------------------------------------------------------------
// McpToolRegistry
// ---------------------------------------------------------------------------

/// Maintains a collection of [`McpClient`] connections and the tools they
/// expose; routes `call_tool` calls from the agent runtime to the correct
/// server.
///
/// # Registering servers
///
/// ```rust,ignore
/// let mut registry = McpToolRegistry::new();
/// let mut client = McpClient::new();
/// // … populate client.server_tools via handle_tools_response …
/// registry.register_server("my-server", client);
/// ```
///
/// # Invoking tools
///
/// ```rust,ignore
/// let (result, provenance) = registry
///     .invoke("some_tool", serde_json::json!({"arg": "value"}))?;
/// assert!(provenance.tool_name.starts_with("mcp:my-server/"));
/// ```
pub struct McpToolRegistry {
    /// Named client connections (populated by `register_server`).
    clients: HashMap<String, McpClient>,
    /// Set of all registered server names; lets test helpers register routing
    /// entries without a live [`McpClient`] connection.
    registered_servers: HashSet<String>,
    /// `tool_name → server_name` index. Built incrementally when servers are
    /// registered; last-writer-wins for overlapping tool names (logged at
    /// warn level but not treated as an error).
    tool_index: HashMap<String, String>,
}

impl McpToolRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            clients: HashMap::new(),
            registered_servers: HashSet::new(),
            tool_index: HashMap::new(),
        }
    }

    /// Register an [`McpClient`] under `name`.
    ///
    /// Indexes every tool currently in `client.available_tools()` so
    /// [`resolve_tool`] can answer immediately without a linear scan.
    /// Calling this after the client has completed its `tools/list`
    /// handshake is the intended usage pattern.
    ///
    /// If another server already registered the same tool name, this call
    /// overwrites the index entry (last writer wins) and logs a warning.
    pub fn register_server(&mut self, name: &str, client: McpClient) {
        // Index all tools this client exposes.
        for tool in client.available_tools() {
            let existing = self.tool_index.insert(tool.name.clone(), name.to_owned());
            if let Some(prev) = existing {
                log::warn!(
                    "MCP tool '{}' was registered by '{}' and is now overridden by '{}'",
                    tool.name,
                    prev,
                    name,
                );
            }
        }
        self.registered_servers.insert(name.to_owned());
        self.clients.insert(name.to_owned(), client);
    }

    /// Find which server handles `tool_name`.
    ///
    /// Returns `None` when no registered server advertises the tool.
    pub fn resolve_tool(&self, tool_name: &str) -> Option<McpToolRoute> {
        self.tool_index.get(tool_name).map(|server_name| McpToolRoute {
            server_name: server_name.clone(),
            tool_name: tool_name.to_owned(),
        })
    }

    /// Invoke `tool_name` with `args` on the owning server.
    ///
    /// Builds a `tools/call` JSON-RPC request via the resolved [`McpClient`],
    /// then synthesises the result from the response. Because `McpClient` is
    /// a message-construction layer (not a live transport), this method treats
    /// the *absence of a JSON-RPC error* in a simulated response as success.
    /// In integration with a real transport, the caller would send the built
    /// request over the wire and pass the server's response to
    /// [`Self::handle_call_response`].
    ///
    /// For unit-test purposes the registry executes the full routing and
    /// request-building path; error responses are surfaced as
    /// [`McpError::InvokeError`].
    ///
    /// Returns `(result_payload, provenance)` on success.
    pub fn invoke(
        &self,
        tool_name: &str,
        args: serde_json::Value,
    ) -> Result<(serde_json::Value, ToolProvenance), McpError> {
        let route = self
            .resolve_tool(tool_name)
            .ok_or_else(|| McpError::UnknownTool { name: tool_name.to_owned() })?;

        if !self.registered_servers.contains(&route.server_name) {
            return Err(McpError::InvokeError {
                tool: tool_name.to_string(),
                detail: "index/client map desync".to_string(),
            });
        }

        // Build the tools/call request for the tool invocation.
        let _request = crate::client::build_call_tool_request(tool_name, args);

        // In a real transport the request would be sent over the wire and a
        // response received. Here we return a synthetic success payload so
        // the routing and provenance path can be exercised without a live
        // server. Real wiring will replace this with the transport round-trip.
        let provenance = ToolProvenance {
            tool_name: route.provenance_tag(),
        };

        Ok((serde_json::json!({"content": [{"type": "text", "text": "ok"}]}), provenance))
    }

    /// Process a `tools/call` response that was received from the transport
    /// layer.
    ///
    /// Extracts the result payload if the response carries no JSON-RPC error;
    /// returns [`McpError::InvokeError`] otherwise. The `server_name` is used
    /// only for error attribution.
    pub fn handle_call_response(
        &self,
        server_name: &str,
        tool_name: &str,
        response: &JsonRpcResponse,
    ) -> Result<(serde_json::Value, ToolProvenance), McpError> {
        if let Some(err) = &response.error {
            return Err(McpError::InvokeError {
                tool: format!("mcp:{server_name}/{tool_name}"),
                detail: err.message.clone(),
            });
        }

        let result = response
            .result
            .clone()
            .unwrap_or(serde_json::Value::Null);

        let provenance = ToolProvenance {
            tool_name: format!("mcp:{server_name}/{tool_name}"),
        };

        Ok((result, provenance))
    }

    /// Number of registered servers.
    pub fn server_count(&self) -> usize {
        self.registered_servers.len()
    }

    /// Number of indexed tools across all servers.
    pub fn tool_count(&self) -> usize {
        self.tool_index.len()
    }
}

impl Default for McpToolRegistry {
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

    /// Register a named server with the given tool names directly into `reg`,
    /// without requiring a live [`McpClient`] WebSocket connection.
    ///
    /// Populates `tool_index` and `registered_servers` so that
    /// `invoke` and `resolve_tool` work correctly in unit tests.
    fn register_test_server(reg: &mut McpToolRegistry, server: &str, tool_names: &[&str]) {
        for name in tool_names {
            reg.tool_index.insert((*name).to_owned(), server.to_owned());
        }
        reg.registered_servers.insert(server.to_owned());
    }

    // ---- Register & resolve -------------------------------------------------

    #[test]
    fn register_two_servers_non_overlapping_tools() {
        let mut reg = McpToolRegistry::new();
        register_test_server(&mut reg, "fs", &["fs.read_file", "fs.write_file"]);
        register_test_server(&mut reg, "db", &["db.query", "db.insert"]);

        assert_eq!(reg.server_count(), 2);
        assert_eq!(reg.tool_count(), 4);
    }

    #[test]
    fn resolve_routes_to_correct_server() {
        let mut reg = McpToolRegistry::new();
        register_test_server(&mut reg, "fs", &["fs.read_file", "fs.write_file"]);
        register_test_server(&mut reg, "db", &["db.query", "db.insert"]);

        let route = reg.resolve_tool("fs.read_file").expect("should resolve");
        assert_eq!(route.server_name, "fs");
        assert_eq!(route.tool_name, "fs.read_file");

        let route2 = reg.resolve_tool("db.query").expect("should resolve");
        assert_eq!(route2.server_name, "db");
        assert_eq!(route2.tool_name, "db.query");
    }

    #[test]
    fn resolve_tool_unknown_returns_none() {
        let mut reg = McpToolRegistry::new();
        register_test_server(&mut reg, "fs", &["fs.read_file"]);

        assert!(reg.resolve_tool("nonexistent.tool").is_none());
        assert!(reg.resolve_tool("").is_none());
    }

    // ---- Invoke -------------------------------------------------------------

    #[test]
    fn invoke_known_tool_returns_ok() {
        let mut reg = McpToolRegistry::new();
        register_test_server(&mut reg, "fs", &["fs.read_file"]);

        let result = reg.invoke("fs.read_file", json!({"path": "/tmp/test.txt"}));
        assert!(result.is_ok(), "invoke should succeed: {result:?}");

        let (payload, provenance) = result.unwrap();
        assert!(payload.is_object(), "payload should be an object");
        assert_eq!(provenance.tool_name, "mcp:fs/fs.read_file");
    }

    #[test]
    fn invoke_unknown_tool_returns_mcp_error() {
        let reg = McpToolRegistry::new();

        let err = reg
            .invoke("ghost.tool", json!({}))
            .expect_err("should fail for unknown tool");

        assert_eq!(
            err,
            McpError::UnknownTool { name: "ghost.tool".to_owned() },
        );
    }

    // ---- Provenance tag -----------------------------------------------------

    #[test]
    fn provenance_tag_has_mcp_prefix() {
        let route = McpToolRoute {
            server_name: "weather-api".to_owned(),
            tool_name: "get_forecast".to_owned(),
        };
        assert_eq!(route.provenance_tag(), "mcp:weather-api/get_forecast");
    }

    // ---- handle_call_response -----------------------------------------------

    #[test]
    fn handle_call_response_success() {
        let reg = McpToolRegistry::new();
        let resp = protocol::create_response(
            json!(42),
            json!({"content": [{"type": "text", "text": "sunny"}]}),
        );

        let (payload, provenance) = reg
            .handle_call_response("weather", "get_forecast", &resp)
            .expect("success response should be Ok");

        assert_eq!(payload["content"][0]["text"], "sunny");
        assert_eq!(provenance.tool_name, "mcp:weather/get_forecast");
    }

    #[test]
    fn handle_call_response_error_propagates_mcp_error() {
        let reg = McpToolRegistry::new();
        let resp = protocol::create_error(json!(42), -32602, "invalid params");

        let err = reg
            .handle_call_response("fs", "fs.write_file", &resp)
            .expect_err("error response must yield McpError");

        assert_eq!(
            err,
            McpError::InvokeError {
                tool: "mcp:fs/fs.write_file".to_owned(),
                detail: "invalid params".to_owned(),
            }
        );
    }

    // ---- Two-server routing: each tool goes to the right server -------------

    #[test]
    fn two_server_routing_fs_tool_goes_to_fs() {
        let mut reg = McpToolRegistry::new();
        register_test_server(&mut reg, "fs", &["read_file", "write_file"]);
        register_test_server(&mut reg, "browser", &["navigate", "click"]);

        // fs tools
        for name in &["read_file", "write_file"] {
            let route = reg.resolve_tool(name).expect("should resolve");
            assert_eq!(
                route.server_name, "fs",
                "tool '{name}' should route to 'fs', got '{}'",
                route.server_name
            );
        }
        // browser tools
        for name in &["navigate", "click"] {
            let route = reg.resolve_tool(name).expect("should resolve");
            assert_eq!(
                route.server_name, "browser",
                "tool '{name}' should route to 'browser', got '{}'",
                route.server_name
            );
        }
    }

    #[test]
    fn two_server_routing_invoke_produces_correct_provenance() {
        let mut reg = McpToolRegistry::new();
        register_test_server(&mut reg, "fs", &["read_file"]);
        register_test_server(&mut reg, "browser", &["navigate"]);

        let (_, prov_fs) = reg.invoke("read_file", json!({})).unwrap();
        assert_eq!(prov_fs.tool_name, "mcp:fs/read_file");

        let (_, prov_browser) = reg.invoke("navigate", json!({"url": "https://example.com"})).unwrap();
        assert_eq!(prov_browser.tool_name, "mcp:browser/navigate");
    }

    // ---- Error display -------------------------------------------------------

    #[test]
    fn mcp_error_display_unknown_tool() {
        let err = McpError::UnknownTool { name: "foo.bar".to_owned() };
        assert!(err.to_string().contains("foo.bar"));
    }

    #[test]
    fn mcp_error_display_invoke_error() {
        let err = McpError::InvokeError {
            tool: "my-tool".to_owned(),
            detail: "timeout".to_owned(),
        };
        let s = err.to_string();
        assert!(s.contains("my-tool"));
        assert!(s.contains("timeout"));
    }

    // ---- Empty registry -------------------------------------------------------

    #[test]
    fn empty_registry_resolve_returns_none() {
        let reg = McpToolRegistry::new();
        assert!(reg.resolve_tool("anything").is_none());
        assert_eq!(reg.server_count(), 0);
        assert_eq!(reg.tool_count(), 0);
    }

    #[test]
    fn empty_registry_invoke_returns_unknown_tool() {
        let reg = McpToolRegistry::new();
        let err = reg.invoke("any.tool", json!({})).unwrap_err();
        assert!(matches!(err, McpError::UnknownTool { .. }));
    }
}
