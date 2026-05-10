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
//! - [`McpError::NotConnected`] — the routed server has no live [`McpClient`]
//!   in the registry (e.g. the test helper registered a server without a client).
//! - [`McpError::InvokeError`] — the client call step returned an error
//!   (e.g. the server rejected the request).
//!
//! ## Threading
//!
//! [`McpToolRegistry`] is `Send + Sync`. Clients are stored behind
//! `tokio::sync::Mutex` so the async [`invoke`](McpToolRegistry::invoke) can
//! lock them without blocking the executor thread. Callers that need shared
//! registry access should wrap it in `Arc<tokio::sync::RwLock<McpToolRegistry>>`
//! (or `Arc<tokio::sync::Mutex<McpToolRegistry>>`).

use std::collections::{HashMap, HashSet};

use tokio::sync::Mutex as TokioMutex;

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
    #[must_use] 
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
    ///
    /// Each client is behind a `tokio::sync::Mutex` so the async `invoke`
    /// method can lock it without blocking the executor.
    clients: HashMap<String, TokioMutex<McpClient>>,
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
    #[must_use] 
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
        self.clients.insert(name.to_owned(), TokioMutex::new(client));
    }

    /// Find which server handles `tool_name`.
    ///
    /// Returns `None` when no registered server advertises the tool.
    #[must_use] 
    pub fn resolve_tool(&self, tool_name: &str) -> Option<McpToolRoute> {
        self.tool_index.get(tool_name).map(|server_name| McpToolRoute {
            server_name: server_name.clone(),
            tool_name: tool_name.to_owned(),
        })
    }

    /// Invoke `tool_name` with `args` on the owning server.
    ///
    /// Resolves which server handles the tool, acquires a lock on the
    /// [`McpClient`] for that server, and performs a live `tools/call`
    /// JSON-RPC round-trip over the WebSocket connection.
    ///
    /// Returns `(result_payload, provenance)` on success.
    ///
    /// # Errors
    ///
    /// - [`McpError::UnknownTool`] — no registered server advertises the name.
    /// - [`McpError::NotConnected`] — the server was registered without a live
    ///   client (e.g. via the test-only helper path that only populates the index).
    /// - Any [`McpError`] propagated from [`McpClient::call_tool`] (transport,
    ///   timeout, server error, etc.).
    pub async fn invoke(
        &self,
        tool_name: &str,
        args: serde_json::Value,
    ) -> Result<(serde_json::Value, ToolProvenance), McpError> {
        let route = self
            .resolve_tool(tool_name)
            .ok_or_else(|| McpError::UnknownTool { name: tool_name.to_owned() })?;

        // Look up the live client for the resolved server.
        let client_mutex = self
            .clients
            .get(&route.server_name)
            .ok_or(McpError::NotConnected)?;

        let provenance = ToolProvenance {
            tool_name: route.provenance_tag(),
        };

        // Acquire the per-client lock and call the remote server.
        let mut client = client_mutex.lock().await;
        let result = client
            .call_tool(tool_name, args)
            .await
            .map_err(|e| McpError::InvokeError {
                tool: provenance.tool_name.clone(),
                detail: e.to_string(),
            })?;

        Ok((result, provenance))
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
    #[must_use] 
    pub fn server_count(&self) -> usize {
        self.registered_servers.len()
    }

    /// Number of indexed tools across all servers.
    #[must_use]
    pub fn tool_count(&self) -> usize {
        self.tool_index.len()
    }

    /// Iterate over every indexed tool name across all registered servers.
    ///
    /// Used by external callers — for example
    /// [`phantom_loop::preflight::check_mcp_collisions`] — that need to refuse
    /// to start when an MCP server has shadowed a reserved tool name. Order
    /// is not stable; callers that need a stable comparison should collect
    /// into a `BTreeSet`.
    pub fn tool_names(&self) -> impl Iterator<Item = &str> {
        self.tool_index.keys().map(String::as_str)
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
    /// Populates `tool_index` and `registered_servers` only — no entry is
    /// added to `clients`. Calls to `invoke` on these tools will return
    /// [`McpError::NotConnected`] because there is no real transport.
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

    // ---- Invoke — no live client (index-only registration) ------------------

    /// When a server is registered without a real client (index-only via the
    /// test helper), `invoke` must return `NotConnected`, not the old hardcoded
    /// stub payload.
    #[tokio::test]
    async fn invoke_without_client_returns_not_connected() {
        let mut reg = McpToolRegistry::new();
        register_test_server(&mut reg, "fs", &["fs.read_file"]);

        let err = reg
            .invoke("fs.read_file", json!({"path": "/tmp/test.txt"}))
            .await
            .expect_err("should fail: no live client");

        assert!(
            matches!(err, McpError::NotConnected),
            "expected NotConnected, got {err:?}"
        );
    }

    #[tokio::test]
    async fn invoke_unknown_tool_returns_mcp_error() {
        let reg = McpToolRegistry::new();

        let err = reg
            .invoke("ghost.tool", json!({}))
            .await
            .expect_err("should fail for unknown tool");

        assert_eq!(
            err,
            McpError::UnknownTool { name: "ghost.tool".to_owned() },
        );
    }

    // ---- Invoke — real client round-trip ------------------------------------

    /// Spawn a minimal mock WebSocket MCP server, register a real [`McpClient`]
    /// with the registry, and verify that `invoke` reaches the server and
    /// returns the server's real response — not the old hardcoded stub.
    #[tokio::test]
    async fn registry_invoke_calls_real_client_not_stub() {
        use crate::client::McpClient;
        use crate::protocol::{self, JsonRpcRequest};
        use futures_util::{SinkExt, StreamExt};
        use std::net::SocketAddr;
        use tokio::net::TcpListener;
        use tokio_tungstenite::accept_async;
        use tokio_tungstenite::tungstenite::Message;

        // Spawn a mock MCP server that handles initialize, tools/list, and tools/call.
        async fn spawn_mock() -> SocketAddr {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else { break };
                    tokio::spawn(async move {
                        let mut ws = accept_async(stream).await.unwrap();
                        while let Some(Ok(msg)) = ws.next().await {
                            let text = match msg {
                                Message::Text(t) => t,
                                Message::Close(_) => break,
                                _ => continue,
                            };
                            let req: JsonRpcRequest =
                                match serde_json::from_str(&text) {
                                    Ok(r) => r,
                                    Err(_) => continue,
                                };
                            let id = req.id.clone().unwrap_or(json!(0));
                            let resp = match req.method.as_str() {
                                "initialize" => protocol::create_response(
                                    id,
                                    json!({
                                        "protocolVersion": "2024-11-05",
                                        "capabilities": {"tools": {}},
                                        "serverInfo": {"name": "mock", "version": "0.0.1"},
                                    }),
                                ),
                                "tools/list" => protocol::create_response(
                                    id,
                                    json!({ "tools": [{
                                        "name": "echo",
                                        "description": "Echo",
                                        "inputSchema": {"type": "object"},
                                    }]}),
                                ),
                                "tools/call" => {
                                    let msg_val = req
                                        .params
                                        .as_ref()
                                        .and_then(|p| p.get("arguments"))
                                        .and_then(|a| a.get("message"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("(none)");
                                    protocol::create_response(
                                        id,
                                        json!({
                                            "content": [{"type": "text", "text": format!("real:{msg_val}")}]
                                        }),
                                    )
                                }
                                other => protocol::create_error(
                                    id,
                                    protocol::METHOD_NOT_FOUND,
                                    &format!("unknown: {other}"),
                                ),
                            };
                            let text = serde_json::to_string(&resp).unwrap();
                            let _ = ws.send(Message::Text(text.into())).await;
                        }
                    });
                }
            });
            addr
        }

        let addr = spawn_mock().await;
        let url = format!("ws://{addr}/mcp");

        // Connect a real client and prime its tool list.
        let mut client = McpClient::connect(&url).await.expect("connect");
        client.list_tools().await.expect("list_tools");

        // Register the live client in the registry.
        let mut reg = McpToolRegistry::new();
        reg.register_server("mock", client);

        // invoke must reach the real server, not return the old hardcoded stub.
        let (result, provenance) = reg
            .invoke("echo", json!({"message": "hello"}))
            .await
            .expect("invoke should succeed with real client");

        // The server echoes "real:<message>" — the old stub returned "ok".
        let text = result["content"][0]["text"].as_str().unwrap_or("");
        assert_eq!(
            text, "real:hello",
            "invoke must return the server's real response, not the hardcoded stub"
        );
        assert_eq!(provenance.tool_name, "mcp:mock/echo");
    }

    // ---- Two-server routing: each tool resolves to the right server ---------

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

    /// invoke without a live client returns NotConnected (provenance prefix is
    /// still correct when the route resolves but the client map has no entry).
    #[tokio::test]
    async fn two_server_routing_invoke_without_clients_returns_not_connected() {
        let mut reg = McpToolRegistry::new();
        register_test_server(&mut reg, "fs", &["read_file"]);
        register_test_server(&mut reg, "browser", &["navigate"]);

        let err_fs = reg.invoke("read_file", json!({})).await.unwrap_err();
        assert!(matches!(err_fs, McpError::NotConnected));

        let err_br = reg
            .invoke("navigate", json!({"url": "https://example.com"}))
            .await
            .unwrap_err();
        assert!(matches!(err_br, McpError::NotConnected));
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

    #[tokio::test]
    async fn empty_registry_invoke_returns_unknown_tool() {
        let reg = McpToolRegistry::new();
        let err = reg.invoke("any.tool", json!({})).await.unwrap_err();
        assert!(matches!(err, McpError::UnknownTool { .. }));
    }

    // ---- tool_names enumeration (used by phantom-loop preflight collision check) -

    #[test]
    fn tool_names_enumerates_every_indexed_tool() {
        let mut reg = McpToolRegistry::new();
        register_test_server(&mut reg, "fs", &["fs.read_file", "fs.write_file"]);
        register_test_server(&mut reg, "db", &["db.query"]);

        let names: std::collections::BTreeSet<&str> = reg.tool_names().collect();
        let expected: std::collections::BTreeSet<&str> =
            ["fs.read_file", "fs.write_file", "db.query"].into_iter().collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn tool_names_empty_registry_yields_nothing() {
        let reg = McpToolRegistry::new();
        assert_eq!(reg.tool_names().count(), 0);
    }
}
