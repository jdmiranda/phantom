//! `POST /mcp` and `GET /mcp/sse` — Claude-side MCP transport.
//!
//! # Phase 1 implementation (issue #397)
//!
//! This module implements the three fleet-control MCP tools that Claude can call:
//!
//! - `phantom.list_phantoms` — hub-local; reads from [`crate::registry::ConnectionRegistry`]
//!   without any Phantom round-trip.
//! - `phantom.run_command` — routes a `phantom.run_command` JSON-RPC frame to the target
//!   Phantom via [`crate::router::forward`]; awaits the response; returns it to Claude.
//! - `phantom.read_output` — routes a `phantom.read_output` JSON-RPC frame with an optional
//!   `since` cursor to the target Phantom; returns the text + next cursor.
//!
//! # Auth
//!
//! Both endpoints require `Authorization: Bearer <api-key>` where the key is
//! a `phk_<base64url>` value loaded from `HUB_API_KEYS` at startup.  The hub
//! stores SHA-256 hashes; comparison is constant-time via [`crate::auth::ApiKeyStore::validate`].
//! An absent or invalid key returns 401 immediately, before any registry access.
//!
//! # Transport
//!
//! `POST /mcp` — synchronous JSON-RPC 2.0 request/response over HTTP.  The body must be a
//! single JSON-RPC 2.0 object (`{"jsonrpc":"2.0","id":…,"method":…,"params":…}`).
//!
//! `GET /mcp/sse` — SSE stream delivering a single JSON-RPC 2.0 response per tool call.
//! Phase 1 delivers the tool response as a single `data:` event then closes the stream.
//! SSE streaming for long-running calls is deferred to Phase 2.
//!
//! # `tools/list`
//!
//! The hub has its own `tools/list` — Phase 1 does NOT proxy `tools/list` to any Phantom.
//! The three tools defined here are the entire surface area for Phase 1.
//!
//! # Output buffering for `read_output`
//!
//! The hub does not buffer Phantom output itself.  The `since` cursor is an opaque
//! string forwarded to Phantom.  Phantom returns `{text, next_cursor, complete}`.
//! The hub relays those fields back to Claude unchanged.  Cap and eviction are
//! Phantom-side concerns; the hub is a pure pass-through for `read_output` payloads.
//!
//! For Phase 1, the hub's default timeout (30 s, `HUB_FORWARD_TIMEOUT_SECS`) applies
//! to both `run_command` and `read_output`.  Long-running commands should use
//! `read_output` polling with the cursor rather than waiting on `run_command`.

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{info, warn};

use crate::AppState;
use crate::auth;
use crate::registry::PhantomId;
use crate::router::{self, JsonRpcRequest, JsonRpcResponse, RouteError};
// `new_idempotency_map` is a unit-type factory that exists for caller API compatibility.
// We pass `&()` directly to avoid the `let_unit_value` clippy warning.

// ---------------------------------------------------------------------------
// Tool definitions (static; returned by tools/list)
// ---------------------------------------------------------------------------

/// MCP tool schema returned by `tools/list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct McpTool {
    name: String,
    description: String,
    #[serde(rename = "inputSchema")]
    input_schema: serde_json::Value,
}

fn fleet_tools() -> Vec<McpTool> {
    vec![
        McpTool {
            name: "phantom.list_phantoms".into(),
            description: "List all Phantom instances currently connected to the hub. \
                Returns id, online status, host, version, and last_seen for each."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
        McpTool {
            name: "phantom.run_command".into(),
            description: "Send a shell command to a specific Phantom instance. \
                The command is written to the focused pane's PTY. \
                Use phantom.read_output to retrieve the output."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "phantom_id": {
                        "type": "string",
                        "description": "The stable peer ID of the target Phantom, \
                            as returned by phantom.list_phantoms."
                    },
                    "command": {
                        "type": "string",
                        "description": "The shell command to send."
                    },
                    "pane_id": {
                        "type": "string",
                        "description": "Optional pane ID. Omit to use the focused pane."
                    }
                },
                "required": ["phantom_id", "command"]
            }),
        },
        McpTool {
            name: "phantom.read_output".into(),
            description: "Read buffered terminal output from a Phantom pane. \
                Supports incremental polling via the since cursor. \
                Poll until complete is true."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "phantom_id": {
                        "type": "string",
                        "description": "The stable peer ID of the target Phantom."
                    },
                    "pane_id": {
                        "type": "string",
                        "description": "Optional pane ID. Omit to use the focused pane."
                    },
                    "since": {
                        "type": "string",
                        "description": "Opaque cursor from a previous read_output call. \
                            Omit to read from the beginning."
                    },
                    "lines": {
                        "type": "integer",
                        "description": "Maximum number of lines to return per call (default 200).",
                        "default": 200
                    }
                },
                "required": ["phantom_id"]
            }),
        },
    ]
}

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 request shape from Claude
// ---------------------------------------------------------------------------

/// A JSON-RPC 2.0 request from Claude, as received by `POST /mcp`.
#[derive(Debug, Deserialize)]
struct McpRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    #[serde(default)]
    id: serde_json::Value,
    method: String,
    #[serde(default)]
    params: serde_json::Value,
}

// ---------------------------------------------------------------------------
// POST /mcp
// ---------------------------------------------------------------------------

/// Handler for `POST /mcp` — Claude-side JSON-RPC 2.0 endpoint.
///
/// Accepts a JSON-RPC 2.0 request body and dispatches to the appropriate tool
/// handler.  Returns 200 with the JSON-RPC response on success (including
/// JSON-RPC-level errors).
pub async fn handle_jsonrpc(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::extract::Json<serde_json::Value>,
) -> Response {
    if let Err(resp) = require_api_key(&state, &headers, "POST /mcp") {
        return *resp;
    }

    let request: McpRequest = match serde_json::from_value(body.0) {
        Ok(r) => r,
        Err(e) => {
            let resp = json_rpc_parse_error(e.to_string());
            return Json(resp).into_response();
        }
    };

    let response = dispatch_mcp_request(&state, &request).await;
    Json(response).into_response()
}

// ---------------------------------------------------------------------------
// GET /mcp/sse
// ---------------------------------------------------------------------------

/// Handler for `GET /mcp/sse` — Claude-side SSE transport.
///
/// Expects the JSON-RPC request in the `request` query parameter (URL-encoded JSON).
/// Phase 1: delivers the tool response as a single `data:` SSE event, then closes
/// the stream.  Streaming for long-running calls is deferred to Phase 2.
pub async fn handle_sse(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    if let Err(resp) = require_api_key(&state, &headers, "GET /mcp/sse") {
        return *resp;
    }

    // Parse the JSON-RPC request from the `request` query parameter.
    let raw = match params.get("request") {
        Some(s) => s.clone(),
        None => {
            // Also accept an inline `method` + `params` + `id` for simple callers.
            let method = params.get("method").cloned().unwrap_or_default();
            let params_val = params
                .get("params")
                .and_then(|p| serde_json::from_str(p).ok())
                .unwrap_or(json!({}));
            let id_val: serde_json::Value = params
                .get("id")
                .and_then(|i| i.parse::<u64>().ok().map(|n| json!(n)))
                .unwrap_or(json!(null));
            let obj = json!({"jsonrpc":"2.0","id":id_val,"method":method,"params":params_val});
            obj.to_string()
        }
    };

    let request: McpRequest = match serde_json::from_str(&raw) {
        Ok(r) => r,
        Err(e) => {
            let resp = json_rpc_parse_error(e.to_string());
            return sse_event(resp);
        }
    };

    let response = dispatch_mcp_request(&state, &request).await;
    sse_event(response)
}

// ---------------------------------------------------------------------------
// Core dispatcher
// ---------------------------------------------------------------------------

async fn dispatch_mcp_request(state: &AppState, request: &McpRequest) -> serde_json::Value {
    let id = request.id.clone();

    match request.method.as_str() {
        "tools/list" => dispatch_tools_list(id),
        "tools/call" => dispatch_tools_call(state, id, &request.params).await,
        "initialize" => dispatch_initialize(id),
        other => {
            warn!("mcp: unknown method '{other}'");
            json_rpc_error(
                id,
                -32601,
                format!("Method not found: {other}"),
            )
        }
    }
}

// ---------------------------------------------------------------------------
// initialize
// ---------------------------------------------------------------------------

fn dispatch_initialize(id: serde_json::Value) -> serde_json::Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": "2024-11-05",
            "serverInfo": {
                "name": "phantom-hub",
                "version": env!("CARGO_PKG_VERSION")
            },
            "capabilities": {
                "tools": {}
            }
        }
    })
}

// ---------------------------------------------------------------------------
// tools/list
// ---------------------------------------------------------------------------

fn dispatch_tools_list(id: serde_json::Value) -> serde_json::Value {
    let tools = fleet_tools();
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "tools": tools
        }
    })
}

// ---------------------------------------------------------------------------
// tools/call
// ---------------------------------------------------------------------------

async fn dispatch_tools_call(
    state: &AppState,
    id: serde_json::Value,
    params: &serde_json::Value,
) -> serde_json::Value {
    let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or(json!({}));

    match tool_name {
        "phantom.list_phantoms" => dispatch_list_phantoms(state, id).await,
        "phantom.run_command" => dispatch_run_command(state, id, &args).await,
        "phantom.read_output" => dispatch_read_output(state, id, &args).await,
        other => {
            warn!("mcp: tools/call for unknown tool '{other}'");
            json_rpc_error(id, -32601, format!("Unknown tool: {other}"))
        }
    }
}

// ---------------------------------------------------------------------------
// phantom.list_phantoms
// ---------------------------------------------------------------------------

/// Hub-local: queries [`crate::registry::ConnectionRegistry::list_online`] directly.
/// No Phantom round-trip.  `panes_known` is always `false` in Phase 1.
async fn dispatch_list_phantoms(state: &AppState, id: serde_json::Value) -> serde_json::Value {
    let reg = state.registry.read().await;
    let phantoms: Vec<serde_json::Value> = reg
        .list_online()
        .into_iter()
        .map(|p| {
            json!({
                "id": p.id.0,
                "online": true,
                "panes_known": false,
                "host": p.host,
                "version": p.version,
                "last_seen": p.last_seen_secs_ago
            })
        })
        .collect();
    drop(reg);

    info!("mcp: list_phantoms → {} online", phantoms.len());

    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&json!({ "phantoms": &phantoms }))
                    .unwrap_or_default()
            }],
            "phantoms": phantoms
        }
    })
}

// ---------------------------------------------------------------------------
// phantom.run_command
// ---------------------------------------------------------------------------

/// Route a `phantom.run_command` JSON-RPC frame to the named Phantom, await
/// the response, and return it to Claude.  The hub does NOT execute the
/// command locally — it is forwarded verbatim over the registered WSS connection.
async fn dispatch_run_command(
    state: &AppState,
    id: serde_json::Value,
    args: &serde_json::Value,
) -> serde_json::Value {
    let phantom_id = match args.get("phantom_id").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p.to_owned(),
        _ => {
            return json_rpc_error(id, -32602, "missing or empty 'phantom_id' argument");
        }
    };

    let command = match args.get("command").and_then(|v| v.as_str()) {
        Some(c) if !c.is_empty() => c.to_owned(),
        _ => {
            return json_rpc_error(id, -32602, "missing or empty 'command' argument");
        }
    };

    let pane_id = args.get("pane_id").and_then(|v| v.as_str()).map(str::to_owned);

    info!("mcp: run_command phantom={phantom_id} command={command:?}");

    // Build the JSON-RPC request to forward to Phantom.
    let mut forward_params = json!({ "command": command });
    if let Some(pid) = pane_id {
        forward_params["pane_id"] = json!(pid);
    }

    let phantom_req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(id.clone()),
        method: "tools/call".into(),
        params: json!({
            "name": "phantom.run_command",
            "arguments": forward_params
        }),
    };

    let pid = PhantomId::new(&phantom_id);

    match router::forward(&state.registry, &pid, phantom_req, None, &()).await {
        Ok(resp) => phantom_response_to_mcp(id, resp),
        Err(e) => route_error_to_mcp(id, &phantom_id, e),
    }
}

// ---------------------------------------------------------------------------
// phantom.read_output
// ---------------------------------------------------------------------------

/// Route a `phantom.read_output` JSON-RPC frame to the named Phantom.
/// The `since` cursor is forwarded opaquely; Phantom returns `{text, next_cursor, complete}`.
/// The hub relays those fields unchanged.
async fn dispatch_read_output(
    state: &AppState,
    id: serde_json::Value,
    args: &serde_json::Value,
) -> serde_json::Value {
    let phantom_id = match args.get("phantom_id").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p.to_owned(),
        _ => {
            return json_rpc_error(id, -32602, "missing or empty 'phantom_id' argument");
        }
    };

    let lines = args.get("lines").and_then(|v| v.as_u64()).unwrap_or(200);
    let since = args.get("since").and_then(|v| v.as_str()).map(str::to_owned);
    let pane_id = args.get("pane_id").and_then(|v| v.as_str()).map(str::to_owned);

    info!("mcp: read_output phantom={phantom_id} lines={lines} since={since:?}");

    // Build the arguments for Phantom's dispatch_read_output.
    let mut forward_args = json!({ "lines": lines });
    if let Some(s) = since {
        forward_args["since"] = json!(s);
    }
    if let Some(pid) = pane_id {
        forward_args["pane_id"] = json!(pid);
    }

    let phantom_req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(id.clone()),
        method: "tools/call".into(),
        params: json!({
            "name": "phantom.read_output",
            "arguments": forward_args
        }),
    };

    let pid = PhantomId::new(&phantom_id);

    match router::forward(&state.registry, &pid, phantom_req, None, &()).await {
        Ok(resp) => phantom_response_to_mcp(id, resp),
        Err(e) => route_error_to_mcp(id, &phantom_id, e),
    }
}

// ---------------------------------------------------------------------------
// Response helpers
// ---------------------------------------------------------------------------

/// Convert a [`JsonRpcResponse`] received from Phantom into a Claude-facing
/// JSON-RPC 2.0 response, restoring the original Claude `id`.
fn phantom_response_to_mcp(original_id: serde_json::Value, resp: JsonRpcResponse) -> serde_json::Value {
    if let Some(err) = resp.error {
        return json!({
            "jsonrpc": "2.0",
            "id": original_id,
            "error": {
                "code": err.code,
                "message": err.message,
                "data": err.data
            }
        });
    }

    json!({
        "jsonrpc": "2.0",
        "id": original_id,
        "result": resp.result.unwrap_or(serde_json::Value::Null)
    })
}

/// Convert a [`RouteError`] into a JSON-RPC 2.0 error response for Claude.
fn route_error_to_mcp(
    id: serde_json::Value,
    phantom_id: &str,
    err: RouteError,
) -> serde_json::Value {
    let (code, message) = match &err {
        RouteError::NotFound(_) => (-32001, format!("phantom '{phantom_id}' is not connected")),
        RouteError::Timeout(_) => (-32002, format!("request to phantom '{phantom_id}' timed out")),
        RouteError::Disconnected(_) => (
            -32003,
            format!("phantom '{phantom_id}' disconnected during request"),
        ),
        RouteError::Backpressure(_) => (
            -32004,
            format!("phantom '{phantom_id}' outbound channel is at capacity"),
        ),
    };
    warn!("mcp: route error for {phantom_id}: {err}");
    json_rpc_error(id, code, message)
}

fn json_rpc_error(
    id: serde_json::Value,
    code: i64,
    message: impl Into<String>,
) -> serde_json::Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message.into()
        }
    })
}

fn json_rpc_parse_error(detail: String) -> serde_json::Value {
    json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": {
            "code": -32700,
            "message": format!("Parse error: {detail}")
        }
    })
}

// ---------------------------------------------------------------------------
// SSE helpers
// ---------------------------------------------------------------------------

/// Wrap a JSON-RPC response as a single SSE `data:` event and close the stream.
///
/// Phase 1: the entire response is delivered in one event.  Streaming is
/// deferred to Phase 2.
fn sse_event(payload: serde_json::Value) -> Response {
    let data = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into());
    let body = format!("data: {data}\n\n");
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/event-stream")
        .header("Cache-Control", "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

// ---------------------------------------------------------------------------
// Shared API key guard
// ---------------------------------------------------------------------------

/// Extract and validate the API key from headers.
///
/// Returns `Ok(())` when the key is present and valid.
/// Returns `Err(Box<Response>)` (a 401) otherwise.  The `Response` is boxed to
/// keep the `Err` variant small and satisfy `clippy::result_large_err`.
fn require_api_key(
    state: &AppState,
    headers: &HeaderMap,
    endpoint: &str,
) -> Result<(), Box<Response>> {
    let key = match auth::extract_bearer(headers) {
        Some(k) => k,
        None => {
            warn!("{endpoint}: missing Authorization header — auth_failure");
            return Err(Box::new(
                (
                    StatusCode::UNAUTHORIZED,
                    "Authorization: Bearer <api-key> required",
                )
                    .into_response(),
            ));
        }
    };

    match auth::validate_api_key(&key, &state.api_keys) {
        Ok(_session) => Ok(()),
        Err(_) => {
            warn!("{endpoint}: invalid or unknown API key — auth_failure");
            Err(Box::new(
                (StatusCode::UNAUTHORIZED, "invalid or unknown API key").into_response(),
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{ApiKeyStore, JwtAuthority};
    use crate::registry::{OUTBOUND_CHANNEL_CAPACITY, PhantomId};
    use crate::router::deliver_response;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use std::sync::Arc;
    use tower::ServiceExt;

    const TEST_SECRET: &[u8] = b"phantom-hub-test-secret-for-mcp-endpoint-tests";
    const TEST_API_KEY: &str = "phk_test-api-key-for-unit-tests";

    fn test_state_with_key(key: &str) -> crate::AppState {
        crate::AppState {
            jwt: Arc::new(JwtAuthority::from_secret(TEST_SECRET)),
            api_keys: Arc::new(ApiKeyStore::from_raw_keys(std::iter::once(key))),
            nonce_cache: Arc::new(crate::auth::NonceCache::new()),
            registry: crate::registry::new_shared(),
        }
    }

    fn test_state_no_keys() -> crate::AppState {
        crate::AppState {
            jwt: Arc::new(JwtAuthority::from_secret(TEST_SECRET)),
            api_keys: Arc::new(ApiKeyStore::default()),
            nonce_cache: Arc::new(crate::auth::NonceCache::new()),
            registry: crate::registry::new_shared(),
        }
    }

    async fn register_fake_phantom(
        state: &crate::AppState,
        id: &str,
        host: &str,
        version: &str,
    ) -> tokio::sync::mpsc::Receiver<JsonRpcRequest> {
        let (tx, rx) = tokio::sync::mpsc::channel(OUTBOUND_CHANNEL_CAPACITY);
        state
            .registry
            .write()
            .await
            .register(PhantomId::new(id), tx, host.into(), version.into())
            .unwrap();
        rx
    }

    fn auth_header(key: &str) -> String {
        format!("Bearer {key}")
    }

    fn mcp_request_body(method: &str, params: serde_json::Value) -> String {
        serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params
        }))
        .unwrap()
    }

    // -----------------------------------------------------------------------
    // Auth: missing key → 401
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn mcp_no_api_key_returns_401() {
        let app = crate::build_router(test_state_no_keys());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .header("Content-Type", "application/json")
                    .body(Body::from(mcp_request_body("tools/list", json!({}))))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // -----------------------------------------------------------------------
    // Auth: wrong key → 401
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn mcp_wrong_api_key_returns_401() {
        let app = crate::build_router(test_state_with_key(TEST_API_KEY));
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .header("Authorization", "Bearer phk_wrong-key")
                    .header("Content-Type", "application/json")
                    .body(Body::from(mcp_request_body("tools/list", json!({}))))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // -----------------------------------------------------------------------
    // tools/list → three tools
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn mcp_tools_list_returns_three_tools() {
        let app = crate::build_router(test_state_with_key(TEST_API_KEY));
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .header("Authorization", auth_header(TEST_API_KEY))
                    .header("Content-Type", "application/json")
                    .body(Body::from(mcp_request_body("tools/list", json!({}))))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let tools = &val["result"]["tools"];
        assert!(tools.is_array(), "expected tools array");
        let names: Vec<&str> = tools
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&"phantom.list_phantoms"), "names: {names:?}");
        assert!(names.contains(&"phantom.run_command"), "names: {names:?}");
        assert!(names.contains(&"phantom.read_output"), "names: {names:?}");
        assert_eq!(names.len(), 3, "expected exactly 3 tools, got: {names:?}");
    }

    // -----------------------------------------------------------------------
    // list_phantoms: 0 connected → empty array
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn list_phantoms_empty_registry() {
        let app = crate::build_router(test_state_with_key(TEST_API_KEY));
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .header("Authorization", auth_header(TEST_API_KEY))
                    .header("Content-Type", "application/json")
                    .body(Body::from(mcp_request_body(
                        "tools/call",
                        json!({ "name": "phantom.list_phantoms", "arguments": {} }),
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let phantoms = &val["result"]["phantoms"];
        assert!(phantoms.is_array());
        assert_eq!(phantoms.as_array().unwrap().len(), 0);
    }

    // -----------------------------------------------------------------------
    // list_phantoms: 1 connected → returns it
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn list_phantoms_one_registered() {
        let state = test_state_with_key(TEST_API_KEY);
        let _rx = register_fake_phantom(&state, "phantom-laptop", "192.168.1.2", "0.1.0").await;

        let app = crate::build_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .header("Authorization", auth_header(TEST_API_KEY))
                    .header("Content-Type", "application/json")
                    .body(Body::from(mcp_request_body(
                        "tools/call",
                        json!({ "name": "phantom.list_phantoms", "arguments": {} }),
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let phantoms = val["result"]["phantoms"].as_array().unwrap();
        assert_eq!(phantoms.len(), 1);
        assert_eq!(phantoms[0]["id"], "phantom-laptop");
        assert_eq!(phantoms[0]["online"], true);
        assert_eq!(phantoms[0]["panes_known"], false);
    }

    // -----------------------------------------------------------------------
    // list_phantoms: many connected
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn list_phantoms_many_registered() {
        let state = test_state_with_key(TEST_API_KEY);
        let _rx1 = register_fake_phantom(&state, "phantom-alpha", "10.0.0.1", "0.1.0").await;
        let _rx2 = register_fake_phantom(&state, "phantom-beta", "10.0.0.2", "0.1.1").await;
        let _rx3 = register_fake_phantom(&state, "phantom-gamma", "10.0.0.3", "0.2.0").await;

        let app = crate::build_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .header("Authorization", auth_header(TEST_API_KEY))
                    .header("Content-Type", "application/json")
                    .body(Body::from(mcp_request_body(
                        "tools/call",
                        json!({ "name": "phantom.list_phantoms", "arguments": {} }),
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let phantoms = val["result"]["phantoms"].as_array().unwrap();
        assert_eq!(phantoms.len(), 3);
        let ids: Vec<&str> = phantoms.iter().filter_map(|p| p["id"].as_str()).collect();
        assert!(ids.contains(&"phantom-alpha"));
        assert!(ids.contains(&"phantom-beta"));
        assert!(ids.contains(&"phantom-gamma"));
    }

    // -----------------------------------------------------------------------
    // run_command: unknown peer → JSON-RPC error (not 404, since it's MCP)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn run_command_unknown_peer_returns_rpc_error() {
        let app = crate::build_router(test_state_with_key(TEST_API_KEY));
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .header("Authorization", auth_header(TEST_API_KEY))
                    .header("Content-Type", "application/json")
                    .body(Body::from(mcp_request_body(
                        "tools/call",
                        json!({
                            "name": "phantom.run_command",
                            "arguments": {
                                "phantom_id": "ghost-phantom",
                                "command": "ls"
                            }
                        }),
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(val.get("error").is_some(), "expected error, got: {val}");
        let code = val["error"]["code"].as_i64().unwrap();
        assert_eq!(code, -32001, "expected NotFound code -32001, got {code}");
    }

    // -----------------------------------------------------------------------
    // run_command: missing API key → 401
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn run_command_no_api_key_returns_401() {
        let app = crate::build_router(test_state_with_key(TEST_API_KEY));
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .header("Content-Type", "application/json")
                    .body(Body::from(mcp_request_body(
                        "tools/call",
                        json!({
                            "name": "phantom.run_command",
                            "arguments": {
                                "phantom_id": "some-phantom",
                                "command": "ls"
                            }
                        }),
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // -----------------------------------------------------------------------
    // run_command: valid peer, fake Phantom echoes → response routed back
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn run_command_routes_to_phantom_and_returns_response() {
        let state = test_state_with_key(TEST_API_KEY);
        let mut rx = register_fake_phantom(&state, "my-phantom", "localhost", "0.1.0").await;

        // Spawn a fake Phantom that echoes back a success response.
        let reg_clone = Arc::clone(&state.registry);
        tokio::spawn(async move {
            let req = rx.recv().await.expect("fake phantom should receive request");
            let hub_id = req.id.clone().unwrap().as_u64().unwrap();
            let response = crate::router::JsonRpcResponse {
                jsonrpc: "2.0".into(),
                id: Some(serde_json::Value::Number(hub_id.into())),
                result: Some(json!({
                    "content": [{"type": "text", "text": "sent: ls"}]
                })),
                error: None,
            };
            deliver_response(&reg_clone, &PhantomId::new("my-phantom"), response).await;
        });

        let app = crate::build_router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .header("Authorization", auth_header(TEST_API_KEY))
                    .header("Content-Type", "application/json")
                    .body(Body::from(mcp_request_body(
                        "tools/call",
                        json!({
                            "name": "phantom.run_command",
                            "arguments": {
                                "phantom_id": "my-phantom",
                                "command": "ls"
                            }
                        }),
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(val.get("error").is_none(), "unexpected error: {val}");
        let text = val["result"]["content"][0]["text"].as_str().unwrap_or("");
        assert!(text.contains("ls"), "expected 'ls' in response, got: {text}");
    }

    // -----------------------------------------------------------------------
    // read_output: valid peer with cursor, fake Phantom replies with next_cursor
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn read_output_with_cursor_returns_text_and_next_cursor() {
        let state = test_state_with_key(TEST_API_KEY);
        let mut rx = register_fake_phantom(&state, "cursor-phantom", "localhost", "0.1.0").await;

        // First call: no cursor → Phantom returns text + next_cursor.
        let reg_clone = Arc::clone(&state.registry);
        tokio::spawn(async move {
            let req = rx.recv().await.expect("should receive first read_output");
            let hub_id = req.id.clone().unwrap().as_u64().unwrap();
            // Verify that `since` was forwarded.
            let args = req.params["arguments"].clone();
            let since = args.get("since");
            // First call has no since.
            let _ = since;

            deliver_response(
                &reg_clone,
                &PhantomId::new("cursor-phantom"),
                crate::router::JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id: Some(serde_json::Value::Number(hub_id.into())),
                    result: Some(json!({
                        "content": [{"type": "text", "text": "line1\nline2\n"}],
                        "text": "line1\nline2\n",
                        "next_cursor": "42",
                        "complete": false
                    })),
                    error: None,
                },
            )
            .await;
        });

        let app = crate::build_router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .header("Authorization", auth_header(TEST_API_KEY))
                    .header("Content-Type", "application/json")
                    .body(Body::from(mcp_request_body(
                        "tools/call",
                        json!({
                            "name": "phantom.read_output",
                            "arguments": {
                                "phantom_id": "cursor-phantom",
                                "lines": 10
                            }
                        }),
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(val.get("error").is_none(), "unexpected error: {val}");
        // The result passes through Phantom's fields directly.
        assert_eq!(val["result"]["next_cursor"], "42");
        assert_eq!(val["result"]["complete"], false);
    }

    // -----------------------------------------------------------------------
    // read_output: non-existent phantom_id → JSON-RPC error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn read_output_unknown_phantom_returns_rpc_error() {
        let app = crate::build_router(test_state_with_key(TEST_API_KEY));
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .header("Authorization", auth_header(TEST_API_KEY))
                    .header("Content-Type", "application/json")
                    .body(Body::from(mcp_request_body(
                        "tools/call",
                        json!({
                            "name": "phantom.read_output",
                            "arguments": {
                                "phantom_id": "nonexistent-phantom"
                            }
                        }),
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(val.get("error").is_some(), "expected error, got: {val}");
        let code = val["error"]["code"].as_i64().unwrap();
        assert_eq!(code, -32001, "expected NotFound -32001, got {code}");
    }

    // -----------------------------------------------------------------------
    // run_command: missing phantom_id argument → INVALID_PARAMS error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn run_command_missing_phantom_id_returns_invalid_params() {
        let app = crate::build_router(test_state_with_key(TEST_API_KEY));
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .header("Authorization", auth_header(TEST_API_KEY))
                    .header("Content-Type", "application/json")
                    .body(Body::from(mcp_request_body(
                        "tools/call",
                        json!({
                            "name": "phantom.run_command",
                            "arguments": { "command": "ls" }
                        }),
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let val: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let code = val["error"]["code"].as_i64().unwrap_or(0);
        assert_eq!(code, -32602, "expected INVALID_PARAMS -32602, got {code}");
    }

    // -----------------------------------------------------------------------
    // GET /mcp/sse: no API key → 401
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn mcp_sse_no_api_key_returns_401() {
        let app = crate::build_router(test_state_no_keys());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/mcp/sse?method=tools/list&params={}&id=1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // -----------------------------------------------------------------------
    // GET /mcp/sse: valid key, tools/list → SSE event with tools
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn mcp_sse_tools_list_returns_sse_event() {
        let app = crate::build_router(test_state_with_key(TEST_API_KEY));
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/mcp/sse?method=tools%2Flist&params=%7B%7D&id=1")
                    .header("Authorization", auth_header(TEST_API_KEY))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp.headers().get("Content-Type").unwrap().to_str().unwrap();
        assert!(ct.contains("text/event-stream"), "expected SSE content-type, got: {ct}");
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let body_str = std::str::from_utf8(&body).unwrap();
        assert!(body_str.starts_with("data: "), "expected SSE data prefix: {body_str}");
        assert!(body_str.contains("phantom.list_phantoms"), "expected tool name: {body_str}");
    }
}
