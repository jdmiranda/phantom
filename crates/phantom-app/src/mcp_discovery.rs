//! MCP server discovery and startup connection.
//!
//! On startup, [`discover_and_connect`] iterates over the `mcp_servers` list
//! from [`PhantomConfig`], skips disabled entries, and connects a live
//! [`McpClient`] for each enabled server. Connected clients are registered in
//! the shared [`McpToolRegistry`] so agent tool dispatch can fall back to them
//! when a tool name is not in the built-in surface.
//!
//! # Error handling
//!
//! Connection failures are logged at `warn` level but never propagate — a
//! single unreachable MCP server must not block Phantom from starting.
//!
//! # Threading
//!
//! The function is `async` and is called from a `tokio::spawn`'ed background
//! task so the App constructor does not block the GPU / render thread.

use std::sync::Arc;

use tokio::sync::RwLock;

use phantom_mcp::{McpClient, McpToolRegistry};

use crate::config::McpServerConfig;

/// Connect all enabled MCP servers and register them in `registry`.
///
/// Servers with `enabled = false` are silently skipped. For enabled servers
/// a WebSocket connection is attempted; on success the client is registered
/// under `server.name`. On failure a `warn!` log is emitted and the loop
/// continues to the next server.
pub async fn discover_and_connect(
    servers: &[McpServerConfig],
    registry: Arc<RwLock<McpToolRegistry>>,
) {
    for server in servers {
        if !server.enabled {
            log::debug!(
                "MCP server '{}' is disabled — skipping",
                server.name
            );
            continue;
        }
        match McpClient::connect(&server.url).await {
            Ok(mut client) => {
                // Fetch the tool list so the registry index is populated
                // before we hand the client off. Failure here means tool
                // routing won't work for this server, so we log and skip.
                match client.list_tools().await {
                    Ok(tools) => {
                        log::info!(
                            "MCP server '{}' connected at {} ({} tool(s))",
                            server.name,
                            server.url,
                            tools.len(),
                        );
                    }
                    Err(e) => {
                        log::warn!(
                            "MCP server '{}': connected but tool listing failed: {e}",
                            server.name
                        );
                        // Still register — the client is live even if the
                        // tool index is empty; the model may call raw names.
                    }
                }
                let mut reg = registry.write().await;
                reg.register_server(&server.name, client);
            }
            Err(e) => {
                log::warn!(
                    "MCP server '{}' failed to connect to {}: {e}",
                    server.name,
                    server.url
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_mcp::McpToolRegistry;
    use std::net::SocketAddr;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;
    use futures_util::{SinkExt, StreamExt};

    /// Spawn a minimal mock MCP server that handles `initialize` and
    /// `tools/list`. Accepts multiple connections (each in its own task).
    async fn spawn_mock_mcp_server(tool_names: Vec<&'static str>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { break };
                let tool_names = tool_names.clone();
                tokio::spawn(async move {
                    let mut ws = accept_async(stream).await.unwrap();
                    while let Some(Ok(msg)) = ws.next().await {
                        use tokio_tungstenite::tungstenite::Message;
                        let text = match msg {
                            Message::Text(t) => t,
                            Message::Close(_) => break,
                            _ => continue,
                        };
                        let req: serde_json::Value =
                            serde_json::from_str(&text).unwrap_or_default();
                        let method = req["method"].as_str().unwrap_or("");
                        let id = req["id"].clone();
                        let resp = match method {
                            "initialize" => serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "result": {
                                    "protocolVersion": "2024-11-05",
                                    "capabilities": {"tools": {}},
                                    "serverInfo": {"name": "mock", "version": "0.1"},
                                }
                            }),
                            "tools/list" => {
                                let tools: Vec<_> = tool_names
                                    .iter()
                                    .map(|n| serde_json::json!({
                                        "name": n,
                                        "description": n,
                                        "inputSchema": {"type": "object"},
                                    }))
                                    .collect();
                                serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {"tools": tools},
                                })
                            }
                            _ => serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {"code": -32601, "message": "method not found"},
                            }),
                        };
                        let text = serde_json::to_string(&resp).unwrap();
                        let _ = ws.send(Message::Text(text.into())).await;
                    }
                });
            }
        });

        addr
    }

    #[tokio::test]
    async fn mcp_discovery_skips_disabled_servers() {
        let registry = Arc::new(RwLock::new(McpToolRegistry::new()));

        let servers = vec![McpServerConfig {
            name: "disabled-server".to_owned(),
            // Use an address that doesn't exist; if we try to connect it
            // would fail, but we should never even attempt it.
            url: "ws://127.0.0.1:1/mcp".to_owned(),
            enabled: false,
        }];

        // Must complete without error or connection attempt.
        discover_and_connect(&servers, Arc::clone(&registry)).await;

        let reg = registry.read().await;
        assert_eq!(
            reg.server_count(),
            0,
            "disabled server must not be registered"
        );
    }

    #[tokio::test]
    async fn mcp_discovery_connects_enabled_servers() {
        let addr = spawn_mock_mcp_server(vec!["ping", "pong"]).await;
        let url = format!("ws://{addr}/mcp");

        let registry = Arc::new(RwLock::new(McpToolRegistry::new()));

        let servers = vec![McpServerConfig {
            name: "test-server".to_owned(),
            url,
            enabled: true,
        }];

        discover_and_connect(&servers, Arc::clone(&registry)).await;

        let reg = registry.read().await;
        assert_eq!(reg.server_count(), 1, "one server should be registered");
        assert_eq!(reg.tool_count(), 2, "two tools should be indexed");

        // Routing should work.
        let route = reg.resolve_tool("ping").expect("ping must resolve");
        assert_eq!(route.server_name, "test-server");
    }

    #[tokio::test]
    async fn mcp_discovery_connect_failure_is_non_fatal() {
        let registry = Arc::new(RwLock::new(McpToolRegistry::new()));

        let servers = vec![McpServerConfig {
            name: "unreachable".to_owned(),
            url: "ws://127.0.0.1:1/mcp".to_owned(), // port 1 — always refused
            enabled: true,
        }];

        // Must return without panicking even though the connection fails.
        discover_and_connect(&servers, Arc::clone(&registry)).await;

        let reg = registry.read().await;
        assert_eq!(
            reg.server_count(),
            0,
            "failed connection must not register a server"
        );
    }

    #[tokio::test]
    async fn mcp_registry_routes_to_correct_client() {
        // Spin up two servers with non-overlapping tool names.
        let addr_a = spawn_mock_mcp_server(vec!["tool_a1", "tool_a2"]).await;
        let addr_b = spawn_mock_mcp_server(vec!["tool_b1"]).await;

        let registry = Arc::new(RwLock::new(McpToolRegistry::new()));

        let servers = vec![
            McpServerConfig {
                name: "server-a".to_owned(),
                url: format!("ws://{addr_a}/mcp"),
                enabled: true,
            },
            McpServerConfig {
                name: "server-b".to_owned(),
                url: format!("ws://{addr_b}/mcp"),
                enabled: true,
            },
        ];

        discover_and_connect(&servers, Arc::clone(&registry)).await;

        let reg = registry.read().await;
        assert_eq!(reg.server_count(), 2);

        let route_a = reg.resolve_tool("tool_a1").expect("tool_a1 must resolve");
        assert_eq!(route_a.server_name, "server-a");

        let route_b = reg.resolve_tool("tool_b1").expect("tool_b1 must resolve");
        assert_eq!(route_b.server_name, "server-b");

        // Unknown tool must not resolve.
        assert!(reg.resolve_tool("does_not_exist").is_none());
    }
}
