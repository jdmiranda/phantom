//! Phantom as an MCP client.
//!
//! Phantom connects to external MCP servers so its agents can invoke
//! third-party tools (file systems, databases, APIs, etc.). This module
//! owns the client-side handshake, tool discovery, resource discovery,
//! and call construction over a WebSocket + JSON-RPC 2.0 transport.
//!
//! # Transport
//!
//! Messages are sent as WebSocket text frames; each frame carries exactly
//! one JSON-RPC 2.0 object. Responses are matched to requests by the
//! numeric `id` field. A background receive task runs on a `tokio` runtime
//! and forwards incoming messages to waiting callers via oneshot channels.
//!
//! # Reconnect
//!
//! [`McpClient::connect`] accepts a URL and performs the MCP `initialize`
//! handshake. If the connection drops, call `connect` again — the existing
//! `McpClient` instance is replaced so callers get a fresh connection.
//! Exponential back-off durations are provided by [`McpClient::backoff_ms`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use log::{debug, info, warn};
use tokio::sync::{oneshot, Mutex};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::error::McpError;
use crate::protocol::{self, JsonRpcRequest, JsonRpcResponse, McpResource, McpTool};

// Re-export McpTool as McpToolDef for the public API surface requested by the issue.
/// A tool definition returned by a remote MCP server's `tools/list` response.
pub type McpToolDef = McpTool;

/// A resource definition returned by a remote MCP server's `resources/list` response.
pub type McpResourceDef = McpResource;

// ---------------------------------------------------------------------------
// Internal pending-request map
// ---------------------------------------------------------------------------

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>;

// ---------------------------------------------------------------------------
// McpClient
// ---------------------------------------------------------------------------

/// Async MCP client that connects to an external MCP server over WebSocket.
///
/// Create a connected client with [`McpClient::connect`]. Methods like
/// [`list_tools`](Self::list_tools), [`call_tool`](Self::call_tool),
/// [`list_resources`](Self::list_resources), and
/// [`read_resource`](Self::read_resource) perform round-trip JSON-RPC 2.0
/// calls over the WebSocket connection.
///
/// # Reconnect
///
/// On disconnect, create a new `McpClient` via `connect`. Use
/// [`backoff_ms`](Self::backoff_ms) to compute retry delays.
///
/// # Example
///
/// ```no_run
/// # async fn example() -> Result<(), phantom_mcp::McpError> {
/// use phantom_mcp::McpClient;
///
/// let mut client = McpClient::connect("ws://127.0.0.1:9999/mcp").await?;
/// let tools = client.list_tools().await?;
/// for tool in &tools {
///     println!("{}: {}", tool.name, tool.description);
/// }
/// let result = client.call_tool("echo", serde_json::json!({"message": "hi"})).await?;
/// println!("{result}");
/// # Ok(())
/// # }
/// ```
pub struct McpClient {
    /// Monotone ID issued to our requests.
    id_gen: Arc<AtomicU64>,
    /// Pending requests waiting for a response keyed by request id.
    pending: PendingMap,
    /// Sender half of the WebSocket outbound channel.
    tx: Arc<Mutex<futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >>>,
    /// Cached tools list from the last `list_tools` call.
    cached_tools: Vec<McpToolDef>,
    /// Cached resources list from the last `list_resources` call.
    cached_resources: Vec<McpResourceDef>,
    /// Server capabilities returned during `initialize`.
    server_capabilities: serde_json::Value,
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("cached_tools_count", &self.cached_tools.len())
            .field("cached_resources_count", &self.cached_resources.len())
            .finish()
    }
}

impl McpClient {
    // ------------------------------------------------------------------
    // Construction / connect
    // ------------------------------------------------------------------

    /// Connect to an MCP server at `url` (e.g. `ws://127.0.0.1:9999/mcp`)
    /// and complete the `initialize` handshake.
    ///
    /// Returns an `Err` if the TCP/WS connection fails or if the server
    /// returns an error response to `initialize`.
    pub async fn connect(url: &str) -> Result<Self, McpError> {
        debug!("mcp: connecting to {url}");
        let (ws_stream, _) = connect_async(url)
            .await
            .map_err(|e| McpError::Transport(format!("WebSocket connect failed: {e}")))?;

        let (sink, mut stream) = ws_stream.split();
        let tx = Arc::new(Mutex::new(sink));
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let id_gen = Arc::new(AtomicU64::new(1));

        // Spawn the receive task that routes responses to waiting callers.
        let pending_clone = Arc::clone(&pending);
        tokio::spawn(async move {
            while let Some(msg) = stream.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        match serde_json::from_str::<JsonRpcResponse>(&text) {
                            Ok(resp) => {
                                if let Some(id_num) =
                                    resp.id.as_ref().and_then(|v| v.as_u64())
                                {
                                    let mut map = pending_clone.lock().await;
                                    if let Some(sender) = map.remove(&id_num) {
                                        let _ = sender.send(resp);
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("mcp: failed to parse incoming message: {e}");
                            }
                        }
                    }
                    Ok(Message::Close(_)) => {
                        debug!("mcp: server closed the WebSocket");
                        break;
                    }
                    Ok(_) => {} // ping/pong/binary — ignore
                    Err(e) => {
                        warn!("mcp: WebSocket receive error: {e}");
                        break;
                    }
                }
            }
            // On disconnect, drain pending requests so callers don't hang.
            let mut map = pending_clone.lock().await;
            map.drain();
        });

        let mut client = Self {
            id_gen,
            pending,
            tx,
            cached_tools: Vec::new(),
            cached_resources: Vec::new(),
            server_capabilities: serde_json::Value::Null,
        };

        // Perform the MCP initialize handshake.
        client.initialize().await?;
        Ok(client)
    }

    // ------------------------------------------------------------------
    // Public API
    // ------------------------------------------------------------------

    /// Fetch the tool catalog from the server.
    ///
    /// Results are cached in `available_tools`; call again to refresh.
    pub async fn list_tools(&mut self) -> Result<Vec<McpToolDef>, McpError> {
        let resp = self.request("tools/list", serde_json::json!({})).await?;
        let result = Self::unwrap_result(resp)?;
        let tools = result
            .get("tools")
            .and_then(|v| serde_json::from_value::<Vec<McpToolDef>>(v.clone()).ok())
            .unwrap_or_default();
        self.cached_tools = tools.clone();
        info!("mcp: discovered {} tools", tools.len());
        Ok(tools)
    }

    /// Invoke a named tool on the server and return its raw result value.
    pub async fn call_tool(
        &mut self,
        name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        let resp = self
            .request(
                "tools/call",
                serde_json::json!({ "name": name, "arguments": args }),
            )
            .await?;
        Self::unwrap_result(resp)
    }

    /// Fetch the resource catalog from the server.
    pub async fn list_resources(&mut self) -> Result<Vec<McpResourceDef>, McpError> {
        let resp = self.request("resources/list", serde_json::json!({})).await?;
        let result = Self::unwrap_result(resp)?;
        let resources = result
            .get("resources")
            .and_then(|v| serde_json::from_value::<Vec<McpResourceDef>>(v.clone()).ok())
            .unwrap_or_default();
        self.cached_resources = resources.clone();
        info!("mcp: discovered {} resources", resources.len());
        Ok(resources)
    }

    /// Read a resource by URI from the server.
    pub async fn read_resource(
        &mut self,
        uri: &str,
    ) -> Result<serde_json::Value, McpError> {
        let resp = self
            .request("resources/read", serde_json::json!({ "uri": uri }))
            .await?;
        Self::unwrap_result(resp)
    }

    /// Tools discovered during the last `list_tools` call (cached locally).
    pub fn available_tools(&self) -> &[McpToolDef] {
        &self.cached_tools
    }

    /// Resources discovered during the last `list_resources` call (cached locally).
    pub fn available_resources(&self) -> &[McpResourceDef] {
        &self.cached_resources
    }

    /// Server capabilities returned during `initialize`.
    pub fn server_capabilities(&self) -> &serde_json::Value {
        &self.server_capabilities
    }

    // ------------------------------------------------------------------
    // Backoff helper
    // ------------------------------------------------------------------

    /// Compute the back-off duration (milliseconds) for the given retry
    /// attempt (0-indexed).
    ///
    /// Uses exponential back-off capped at 30 seconds:
    /// attempt 0 → 100 ms, 1 → 200 ms, 2 → 400 ms … max 30 000 ms.
    #[must_use]
    pub fn backoff_ms(attempt: u32) -> u64 {
        const BASE: u64 = 100;
        const CAP: u64 = 30_000;
        // Limit the shift to avoid overflow; 2^9 * 100 = 51_200 which already
        // exceeds CAP, so any attempt >= 9 will be clamped to CAP.
        let ms = BASE.saturating_mul(1u64 << attempt.min(9));
        ms.min(CAP)
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    /// MCP `initialize` handshake — called automatically by `connect`.
    async fn initialize(&mut self) -> Result<(), McpError> {
        let resp = self
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "phantom",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            )
            .await?;
        let result = Self::unwrap_result(resp)?;
        self.server_capabilities = result
            .get("capabilities")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        info!("mcp: initialized, capabilities: {}", self.server_capabilities);
        Ok(())
    }

    /// Send a JSON-RPC request and await its response.
    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<JsonRpcResponse, McpError> {
        let id = self.id_gen.fetch_add(1, Ordering::Relaxed);
        let req = protocol::create_request(id, method, params);

        let text = serde_json::to_string(&req)
            .map_err(|e| McpError::Serialization(e.to_string()))?;

        let (resp_tx, resp_rx) = oneshot::channel::<JsonRpcResponse>();
        {
            let mut map = self.pending.lock().await;
            map.insert(id, resp_tx);
        }

        {
            let mut sink = self.tx.lock().await;
            sink.send(Message::Text(text.into()))
                .await
                .map_err(|e| McpError::Transport(format!("send failed: {e}")))?;
        }

        tokio::time::timeout(Duration::from_secs(30), resp_rx)
            .await
            .map_err(|_| McpError::Timeout {
                method: method.to_owned(),
            })?
            .map_err(|_| McpError::Transport("response channel dropped".to_owned()))
    }

    /// Extract the `result` field from a JSON-RPC response, or return an
    /// `McpError::ServerError` if the response carries an `error`.
    fn unwrap_result(resp: JsonRpcResponse) -> Result<serde_json::Value, McpError> {
        if let Some(err) = resp.error {
            return Err(McpError::ServerError {
                code: err.code,
                message: err.message,
            });
        }
        Ok(resp.result.unwrap_or(serde_json::Value::Null))
    }
}

// ---------------------------------------------------------------------------
// Back-compat synchronous builder API
// ---------------------------------------------------------------------------
//
// These synchronous helpers let non-async callers build JSON-RPC messages
// manually, preserving the API surface of the original stub. New callers
// should prefer the async `McpClient::connect` flow.

/// Monotone ID counter for synchronous builder helpers.
static BUILDER_NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_builder_id() -> u64 {
    BUILDER_NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// Build the MCP `initialize` request message (synchronous helper).
#[must_use]
pub fn build_initialize_request() -> JsonRpcRequest {
    protocol::create_request(
        next_builder_id(),
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "phantom",
                "version": env!("CARGO_PKG_VERSION"),
            },
        }),
    )
}

/// Build a `tools/list` request message (synchronous helper).
#[must_use]
pub fn build_list_tools_request() -> JsonRpcRequest {
    protocol::create_request(next_builder_id(), "tools/list", serde_json::json!({}))
}

/// Build a `tools/call` request message (synchronous helper).
#[must_use]
pub fn build_call_tool_request(name: &str, args: serde_json::Value) -> JsonRpcRequest {
    protocol::create_request(
        next_builder_id(),
        "tools/call",
        serde_json::json!({ "name": name, "arguments": args }),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::McpError;
    use crate::protocol;
    use serde_json::json;
    use std::net::SocketAddr;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    // -----------------------------------------------------------------------
    // Synchronous builder tests
    // -----------------------------------------------------------------------

    #[test]
    fn build_initialize_produces_valid_request() {
        let req = build_initialize_request();
        assert_eq!(req.method, "initialize");
        assert_eq!(req.jsonrpc, "2.0");
        let params = req.params.unwrap();
        assert_eq!(params["clientInfo"]["name"], "phantom");
        assert_eq!(params["protocolVersion"], "2024-11-05");
    }

    #[test]
    fn build_list_tools_request_correct_method() {
        let req = build_list_tools_request();
        assert_eq!(req.method, "tools/list");
        assert_eq!(req.jsonrpc, "2.0");
    }

    #[test]
    fn build_call_tool_request_correct_shape() {
        let req = build_call_tool_request("fs.read_file", json!({"path": "/tmp/test.txt"}));
        assert_eq!(req.method, "tools/call");
        let params = req.params.unwrap();
        assert_eq!(params["name"], "fs.read_file");
        assert_eq!(params["arguments"]["path"], "/tmp/test.txt");
    }

    #[test]
    fn builder_ids_are_unique() {
        let r1 = build_initialize_request();
        let r2 = build_list_tools_request();
        let r3 = build_call_tool_request("x", json!({}));
        let ids: Vec<_> = [&r1, &r2, &r3]
            .iter()
            .map(|r| r.id.clone().unwrap())
            .collect();
        assert_ne!(ids[0], ids[1]);
        assert_ne!(ids[1], ids[2]);
        assert_ne!(ids[0], ids[2]);
    }

    // -----------------------------------------------------------------------
    // Protocol helpers remain functional (back-compat)
    // -----------------------------------------------------------------------

    #[test]
    fn handle_initialize_success_via_protocol() {
        let resp = protocol::create_response(
            json!(1),
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "test-server", "version": "0.1.0"},
            }),
        );
        assert!(resp.error.is_none());
        assert!(resp.result.is_some());
    }

    #[test]
    fn tools_list_roundtrip_via_protocol() {
        let resp = protocol::create_response(
            json!(2),
            json!({
                "tools": [
                    {
                        "name": "fs.read_file",
                        "description": "Read a file from disk",
                        "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}},
                    },
                    {
                        "name": "fs.write_file",
                        "description": "Write a file to disk",
                        "inputSchema": {"type": "object"},
                    },
                ]
            }),
        );
        let tools: Vec<McpToolDef> = serde_json::from_value(
            resp.result.unwrap().get("tools").unwrap().clone(),
        )
        .unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "fs.read_file");
    }

    // -----------------------------------------------------------------------
    // Backoff helper
    // -----------------------------------------------------------------------

    #[test]
    fn backoff_ms_increases_exponentially() {
        assert_eq!(McpClient::backoff_ms(0), 100);
        assert_eq!(McpClient::backoff_ms(1), 200);
        assert_eq!(McpClient::backoff_ms(2), 400);
        assert_eq!(McpClient::backoff_ms(3), 800);
    }

    #[test]
    fn backoff_ms_caps_at_30_seconds() {
        // attempt=9 → 100 * 512 = 51_200, clamped to 30_000.
        assert_eq!(McpClient::backoff_ms(9), 30_000);
        // All high attempts return the same capped value.
        assert_eq!(McpClient::backoff_ms(20), 30_000);
        assert_eq!(McpClient::backoff_ms(100), 30_000);
    }

    // -----------------------------------------------------------------------
    // McpError display
    // -----------------------------------------------------------------------

    #[test]
    fn mcp_error_transport_display() {
        let e = McpError::Transport("connection refused".to_owned());
        let s = format!("{e}");
        assert!(s.contains("connection refused"));
    }

    #[test]
    fn mcp_error_server_error_display() {
        let e = McpError::ServerError {
            code: -32601,
            message: "method not found".to_owned(),
        };
        let s = format!("{e}");
        assert!(s.contains("-32601"));
    }

    #[test]
    fn mcp_error_timeout_display() {
        let e = McpError::Timeout {
            method: "tools/call".to_owned(),
        };
        let s = format!("{e}");
        assert!(s.contains("tools/call"));
    }

    #[test]
    fn mcp_error_not_connected_display() {
        let e = McpError::NotConnected;
        let s = format!("{e}");
        assert!(!s.is_empty());
    }

    // -----------------------------------------------------------------------
    // Integration tests — in-process mock WebSocket server
    // -----------------------------------------------------------------------

    /// Minimal mock MCP server that handles one connection, answers all
    /// standard MCP methods, then shuts down.
    async fn spawn_mock_server() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();

            while let Some(Ok(msg)) = ws.next().await {
                let text = match msg {
                    Message::Text(t) => t,
                    Message::Close(_) => break,
                    _ => continue,
                };

                let req: JsonRpcRequest = match serde_json::from_str(&text) {
                    Ok(r) => r,
                    Err(_) => continue,
                };

                let id = req.id.clone().unwrap_or(json!(0));
                let resp = match req.method.as_str() {
                    "initialize" => protocol::create_response(
                        id,
                        json!({
                            "protocolVersion": "2024-11-05",
                            "capabilities": {"tools": {}, "resources": {}},
                            "serverInfo": {"name": "mock-server", "version": "0.0.1"},
                        }),
                    ),
                    "tools/list" => protocol::create_response(
                        id,
                        json!({
                            "tools": [
                                {
                                    "name": "echo",
                                    "description": "Echo the input back",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": {"message": {"type": "string"}},
                                        "required": ["message"],
                                    },
                                },
                                {
                                    "name": "add",
                                    "description": "Add two numbers",
                                    "inputSchema": {
                                        "type": "object",
                                        "properties": {
                                            "a": {"type": "number"},
                                            "b": {"type": "number"},
                                        },
                                    },
                                },
                            ]
                        }),
                    ),
                    "tools/call" => {
                        let name = req
                            .params
                            .as_ref()
                            .and_then(|p| p.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let args = req
                            .params
                            .as_ref()
                            .and_then(|p| p.get("arguments"))
                            .cloned()
                            .unwrap_or(json!({}));

                        match name {
                            "echo" => {
                                let msg = args
                                    .get("message")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                protocol::create_response(
                                    id,
                                    json!({"content": [{"type": "text", "text": msg}]}),
                                )
                            }
                            "add" => {
                                let a = args
                                    .get("a")
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0);
                                let b = args
                                    .get("b")
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0);
                                protocol::create_response(
                                    id,
                                    json!({
                                        "content": [{"type": "text", "text": format!("{}", a + b)}],
                                        "result": a + b,
                                    }),
                                )
                            }
                            other => protocol::create_error(
                                id,
                                protocol::METHOD_NOT_FOUND,
                                &format!("unknown tool: {other}"),
                            ),
                        }
                    }
                    "resources/list" => protocol::create_response(
                        id,
                        json!({
                            "resources": [{
                                "uri": "mock://data/hello",
                                "name": "Hello",
                                "description": "A mock resource",
                                "mimeType": "text/plain",
                            }]
                        }),
                    ),
                    "resources/read" => {
                        let uri = req
                            .params
                            .as_ref()
                            .and_then(|p| p.get("uri"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        protocol::create_response(
                            id,
                            json!({
                                "contents": [{
                                    "uri": uri,
                                    "mimeType": "text/plain",
                                    "text": format!("content of {uri}"),
                                }],
                            }),
                        )
                    }
                    other => protocol::create_error(
                        id,
                        protocol::METHOD_NOT_FOUND,
                        &format!("unknown method: {other}"),
                    ),
                };

                let text = serde_json::to_string(&resp).unwrap();
                ws.send(Message::Text(text.into())).await.unwrap();
            }
        });

        addr
    }

    /// Multi-connection mock server — each connection handled independently.
    async fn spawn_mock_server_multi() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut ws = accept_async(stream).await.unwrap();
                    while let Some(Ok(msg)) = ws.next().await {
                        let text = match msg {
                            Message::Text(t) => t,
                            Message::Close(_) => break,
                            _ => continue,
                        };
                        let req: JsonRpcRequest = match serde_json::from_str(&text) {
                            Ok(r) => r,
                            Err(_) => continue,
                        };
                        let id = req.id.clone().unwrap_or(json!(0));
                        let resp = match req.method.as_str() {
                            "initialize" => protocol::create_response(
                                id,
                                json!({
                                    "protocolVersion": "2024-11-05",
                                    "capabilities": {"tools": {}, "resources": {}},
                                    "serverInfo": {"name": "mock", "version": "0.0.1"},
                                }),
                            ),
                            "tools/list" => protocol::create_response(
                                id,
                                json!({ "tools": [
                                    {
                                        "name": "ping",
                                        "description": "ping tool",
                                        "inputSchema": {"type": "object"},
                                    },
                                ]}),
                            ),
                            "tools/call" => protocol::create_response(
                                id,
                                json!({ "content": [{"type": "text", "text": "pong"}] }),
                            ),
                            "resources/list" => protocol::create_response(
                                id,
                                json!({ "resources": [] }),
                            ),
                            other => protocol::create_error(
                                id,
                                protocol::METHOD_NOT_FOUND,
                                &format!("unknown: {other}"),
                            ),
                        };
                        let text = serde_json::to_string(&resp).unwrap();
                        ws.send(Message::Text(text.into())).await.unwrap();
                    }
                });
            }
        });

        addr
    }

    #[tokio::test]
    async fn connect_and_initialize_succeeds() {
        let addr = spawn_mock_server_multi().await;
        let url = format!("ws://{addr}/mcp");
        let client = McpClient::connect(&url).await;
        assert!(client.is_ok(), "connect failed: {client:?}");
    }

    #[tokio::test]
    async fn list_tools_round_trip() {
        let addr = spawn_mock_server_multi().await;
        let url = format!("ws://{addr}/mcp");
        let mut client = McpClient::connect(&url).await.unwrap();

        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "ping");

        // Cached copy matches.
        assert_eq!(client.available_tools().len(), 1);
    }

    #[tokio::test]
    async fn call_tool_echo_round_trip() {
        let addr = spawn_mock_server().await;
        let url = format!("ws://{addr}/mcp");
        let mut client = McpClient::connect(&url).await.unwrap();

        let result = client
            .call_tool("echo", json!({"message": "hello world"}))
            .await
            .unwrap();
        let text = result["content"][0]["text"].as_str().unwrap();
        assert_eq!(text, "hello world");
    }

    #[tokio::test]
    async fn call_tool_add_round_trip() {
        let addr = spawn_mock_server().await;
        let url = format!("ws://{addr}/mcp");
        let mut client = McpClient::connect(&url).await.unwrap();

        let result = client
            .call_tool("add", json!({"a": 3.0, "b": 4.0}))
            .await
            .unwrap();
        let sum = result["result"].as_f64().unwrap();
        assert!((sum - 7.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn call_tool_unknown_returns_server_error() {
        let addr = spawn_mock_server().await;
        let url = format!("ws://{addr}/mcp");
        let mut client = McpClient::connect(&url).await.unwrap();

        let result = client.call_tool("nonexistent", json!({})).await;
        assert!(
            matches!(result, Err(McpError::ServerError { .. })),
            "expected ServerError, got {result:?}"
        );
    }

    #[tokio::test]
    async fn list_resources_round_trip() {
        let addr = spawn_mock_server().await;
        let url = format!("ws://{addr}/mcp");
        let mut client = McpClient::connect(&url).await.unwrap();

        let resources = client.list_resources().await.unwrap();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].uri, "mock://data/hello");
        assert_eq!(client.available_resources().len(), 1);
    }

    #[tokio::test]
    async fn read_resource_round_trip() {
        let addr = spawn_mock_server().await;
        let url = format!("ws://{addr}/mcp");
        let mut client = McpClient::connect(&url).await.unwrap();

        let result = client.read_resource("mock://data/hello").await.unwrap();
        let text = result["contents"][0]["text"].as_str().unwrap();
        assert!(text.contains("mock://data/hello"));
    }

    #[tokio::test]
    async fn server_capabilities_populated_after_connect() {
        let addr = spawn_mock_server_multi().await;
        let url = format!("ws://{addr}/mcp");
        let client = McpClient::connect(&url).await.unwrap();

        let caps = client.server_capabilities();
        assert!(caps.is_object(), "expected object, got {caps}");
    }

    #[tokio::test]
    async fn connect_to_invalid_address_returns_transport_error() {
        // Port 1 is privileged and never has an MCP server — connect must fail.
        let result = McpClient::connect("ws://127.0.0.1:1").await;
        assert!(
            matches!(result, Err(McpError::Transport(_))),
            "expected Transport error, got {result:?}"
        );
    }
}
