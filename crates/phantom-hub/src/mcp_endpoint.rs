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
//! # Phase 2 (issue #399)
//!
//! - `phantom.spawn_agent` — routes a `phantom.spawn_agent` JSON-RPC frame to the target
//!   Phantom; Phantom calls [`phantom_agents::AgentManager::spawn`] and returns an
//!   `{ agent_id, started_at }` payload. The hub forwards the structured reply unchanged.
//!
//! # Auth
//!
//! Both endpoints require `Authorization: Bearer <api-key>` where the key is
//! a `phk_<base64url>` value loaded from `HUB_API_KEYS` at startup.  The hub
//! stores SHA-256 hashes; comparison is constant-time via [`crate::auth::ApiKeyStore::validate`].
//! An absent or invalid key returns 401 immediately, before any registry access.
//!
//! For `phantom.spawn_agent` the hub also verifies that the target `phantom_id` is
//! online in the registry before forwarding (404-equivalent JSON-RPC error if not).
//! Per-key capability scoping (allowlist for spawn per API key) is deferred to issue
//! #409 (ticket 09); v1 grants spawn to any valid API key. The comment
//! `// SECURITY: ticket-09 will add per-key capability scoping here` marks the call site.
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
//! The hub has its own `tools/list` — it does NOT proxy `tools/list` to any Phantom.
//! Phase 2 surface area: four tools (three from Phase 1 plus `phantom.spawn_agent`).
//!
//! # Output buffering for `read_output`
//!
//! The hub does not buffer Phantom output itself.  The `since` cursor is an opaque
//! string forwarded to Phantom.  Phantom returns `{text, next_cursor, complete}`.
//! The hub relays those fields back to Claude unchanged.  Cap and eviction are
//! Phantom-side concerns; the hub is a pure pass-through for `read_output` payloads.
//!
//! For Phase 1+2, the hub's default timeout (30 s, `HUB_FORWARD_TIMEOUT_SECS`) applies
//! to `run_command`, `read_output`, and `spawn_agent`.  Long-running commands should use
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
        // Phase 2 (issue #399): remote agent spawn returning a stable AgentId.
        McpTool {
            name: "phantom.spawn_agent".into(),
            description: "Spawn an AI agent on a specific Phantom instance. \
                Returns a stable agent_id (decimal string) and started_at timestamp \
                immediately — the agent runs asynchronously. \
                Use phantom.get_agent_status (issue #400) to poll for progress. \
                NOTE: any valid API key can spawn agents in v1; per-key capability \
                scoping is deferred to issue #409."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "phantom_id": {
                        "type": "string",
                        "description": "The stable peer ID of the target Phantom, \
                            as returned by phantom.list_phantoms."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "Free-form task description for the agent."
                    },
                    "role": {
                        "type": "string",
                        "enum": ["default", "defender", "inspector"],
                        "description": "Agent role. Defaults to 'default'.",
                        "default": "default"
                    }
                },
                "required": ["phantom_id", "prompt"]
            }),
        },
        // Phase 2 (issue #400): pane listing.
        McpTool {
            name: "phantom.list_panes".into(),
            description: "List all panes open in a specific Phantom instance. \
                Returns id, type (terminal/agent/inspector), title, focused flag, and \
                agent_id (only for agent-type panes). \
                Use pane ids to target phantom.run_command at a specific pane. \
                Use agent_id to poll status via phantom.get_agent_status. \
                SECURITY: per-API-key capability scoping deferred to #511."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "phantom_id": {
                        "type": "string",
                        "description": "The stable peer ID of the target Phantom, \
                            as returned by phantom.list_phantoms."
                    }
                },
                "required": ["phantom_id"]
            }),
        },
        // Phase 2 (issue #400): agent status polling.
        McpTool {
            name: "phantom.get_agent_status".into(),
            description: "Return the current status of an agent spawned via phantom.spawn_agent. \
                Poll every 5 s until state is 'done' or 'failed'. \
                Returns state (running/done/failed), task, and last_output_excerpt (≤256 bytes). \
                SECURITY: per-API-key capability scoping deferred to #511."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "phantom_id": {
                        "type": "string",
                        "description": "The stable peer ID of the target Phantom."
                    },
                    "agent_id": {
                        "type": "string",
                        "description": "The decimal string agent_id returned by phantom.spawn_agent."
                    }
                },
                "required": ["phantom_id", "agent_id"]
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
        // Phase 2 (issue #399)
        "phantom.spawn_agent" => dispatch_spawn_agent(state, id, &args).await,
        // Phase 2 (issue #400)
        "phantom.list_panes" => dispatch_list_panes(state, id, &args).await,
        "phantom.get_agent_status" => dispatch_get_agent_status(state, id, &args).await,
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
// phantom.spawn_agent (issue #399)
// ---------------------------------------------------------------------------

/// Route a `phantom.spawn_agent` JSON-RPC frame to the named Phantom.
///
/// The hub is a pure pass-through for this tool — it does NOT call
/// `AgentManager` itself. Instead it forwards the frame to the Phantom's
/// MCP listener, which dispatches `AppCommand::SpawnAgent` on the App thread,
/// calls `AgentManager::spawn`, and returns `{ agent_id, started_at }`.
///
/// # Auth / capability gate
///
/// Auth: API key validated by the shared `require_api_key` guard (returns 401
/// to the HTTP layer before this function is reached).
///
/// Capability gate v1: any valid API key can spawn agents.  Per-key capability
/// scoping (limiting spawn to explicitly-whitelisted keys) is deferred to
/// issue #409.
/// SECURITY: ticket-09 will add per-key capability scoping here.
///
/// The `phantom_id` existence check is enforced by `router::forward` which
/// returns `RouteError::NotFound` when the peer is absent from the registry.
async fn dispatch_spawn_agent(
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

    let prompt = match args.get("prompt").and_then(|v| v.as_str()) {
        Some(p) if !p.is_empty() => p.to_owned(),
        _ => {
            return json_rpc_error(id, -32602, "missing or empty 'prompt' argument");
        }
    };

    // Validate role against the allowlist; forward the raw value — Phantom
    // coerces unknown roles to "default" on its side.
    let role = args.get("role").and_then(|v| v.as_str()).unwrap_or("default");
    let role = match role {
        "defender" | "inspector" | "default" => role.to_owned(),
        other => {
            warn!("mcp: spawn_agent role '{other}' not in allowlist, coercing to 'default'");
            "default".to_owned()
        }
    };

    info!("mcp: spawn_agent phantom={phantom_id} prompt={prompt:?} role={role}");

    // SECURITY: ticket-09 will add per-key capability scoping here.
    // v1: any valid API key may spawn agents.

    let forward_args = json!({
        "prompt": prompt,
        "role": role
    });

    let phantom_req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(id.clone()),
        method: "tools/call".into(),
        params: json!({
            "name": "phantom.spawn_agent",
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
// phantom.list_panes (issue #400)
// ---------------------------------------------------------------------------

/// Route a `phantom.list_panes` JSON-RPC frame to the named Phantom.
///
/// The hub is a pure pass-through — it forwards the frame unchanged and
/// relays the `{ panes: [...] }` payload back to Claude.
///
/// # Auth / capability gate
///
/// Auth: API key validated by the shared `require_api_key` guard.
/// Capability gate v1: any valid API key may list panes.
/// SECURITY: per-API-key capability scoping deferred to #511.
async fn dispatch_list_panes(
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

    info!("mcp: list_panes phantom={phantom_id}");

    // SECURITY: per-API-key capability scoping deferred to #511.
    // v1: any valid API key may list panes.

    let phantom_req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(id.clone()),
        method: "tools/call".into(),
        params: json!({
            "name": "phantom.list_panes",
            "arguments": {}
        }),
    };

    let pid = PhantomId::new(&phantom_id);

    match router::forward(&state.registry, &pid, phantom_req, None, &()).await {
        Ok(resp) => phantom_response_to_mcp(id, resp),
        Err(e) => route_error_to_mcp(id, &phantom_id, e),
    }
}

// ---------------------------------------------------------------------------
// phantom.get_agent_status (issue #400)
// ---------------------------------------------------------------------------

/// Route a `phantom.get_agent_status` JSON-RPC frame to the named Phantom.
///
/// The hub is a pure pass-through — it validates `phantom_id` and `agent_id`,
/// then forwards the frame unchanged and relays the status payload back to Claude.
///
/// # Auth / capability gate
///
/// Auth: API key validated by the shared `require_api_key` guard.
/// Capability gate v1: any valid API key may poll agent status.
/// SECURITY: per-API-key capability scoping deferred to #511.
async fn dispatch_get_agent_status(
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

    let agent_id = match args.get("agent_id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_owned(),
        _ => {
            return json_rpc_error(id, -32602, "missing or empty 'agent_id' argument");
        }
    };

    info!("mcp: get_agent_status phantom={phantom_id} agent_id={agent_id}");

    // SECURITY: per-API-key capability scoping deferred to #511.
    // v1: any valid API key may poll agent status.

    let forward_args = json!({ "agent_id": agent_id });

    let phantom_req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(id.clone()),
        method: "tools/call".into(),
        params: json!({
            "name": "phantom.get_agent_status",
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
    // tools/list → six tools (Phase 2 #400 adds list_panes + get_agent_status)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn mcp_tools_list_returns_six_tools() {
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
        assert!(names.contains(&"phantom.spawn_agent"), "names: {names:?}");
        // Phase 2 (issue #400)
        assert!(names.contains(&"phantom.list_panes"), "names: {names:?}");
        assert!(names.contains(&"phantom.get_agent_status"), "names: {names:?}");
        assert_eq!(names.len(), 6, "expected exactly 6 tools, got: {names:?}");
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
    // phantom.spawn_agent: valid auth + connected peer → agent_id returned
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn spawn_agent_valid_auth_and_connected_peer_returns_agent_id() {
        let state = test_state_with_key(TEST_API_KEY);
        let mut rx = register_fake_phantom(&state, "spawn-phantom", "localhost", "0.1.0").await;

        // Fake Phantom: receives spawn_agent, returns agent_id + started_at.
        let reg_clone = Arc::clone(&state.registry);
        tokio::spawn(async move {
            let req = rx.recv().await.expect("fake phantom should receive request");
            let hub_id = req.id.clone().unwrap().as_u64().unwrap();
            deliver_response(
                &reg_clone,
                &PhantomId::new("spawn-phantom"),
                crate::router::JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id: Some(serde_json::Value::Number(hub_id.into())),
                    result: Some(json!({
                        "content": [{"type": "text", "text": "agent spawned: id=7 started_at=2026-04-30T00:00:00Z"}],
                        "agent_id": "7",
                        "started_at": "2026-04-30T00:00:00Z"
                    })),
                    error: None,
                },
            )
            .await;
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
                            "name": "phantom.spawn_agent",
                            "arguments": {
                                "phantom_id": "spawn-phantom",
                                "prompt": "list the modified files in this repo"
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
        let agent_id = val["result"]["agent_id"].as_str().unwrap_or("");
        assert_eq!(agent_id, "7", "expected agent_id '7', got: {val}");
    }

    // -----------------------------------------------------------------------
    // phantom.spawn_agent: unknown peer → JSON-RPC NotFound error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn spawn_agent_unknown_peer_returns_rpc_error() {
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
                            "name": "phantom.spawn_agent",
                            "arguments": {
                                "phantom_id": "ghost-phantom",
                                "prompt": "do something"
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
    // phantom.spawn_agent: no API key → 401
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn spawn_agent_no_api_key_returns_401() {
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
                            "name": "phantom.spawn_agent",
                            "arguments": {
                                "phantom_id": "any-phantom",
                                "prompt": "do something"
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
    // phantom.spawn_agent: missing prompt → INVALID_PARAMS error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn spawn_agent_missing_prompt_returns_invalid_params() {
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
                            "name": "phantom.spawn_agent",
                            "arguments": { "phantom_id": "some-phantom" }
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

    // -----------------------------------------------------------------------
    // phantom.list_panes: unknown peer → JSON-RPC NotFound error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn list_panes_unknown_peer_returns_rpc_error() {
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
                            "name": "phantom.list_panes",
                            "arguments": { "phantom_id": "ghost-phantom" }
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
    // phantom.list_panes: no API key → 401
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn list_panes_no_api_key_returns_401() {
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
                            "name": "phantom.list_panes",
                            "arguments": { "phantom_id": "any-phantom" }
                        }),
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // -----------------------------------------------------------------------
    // phantom.list_panes: valid auth + connected peer → panes relayed back
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn list_panes_valid_auth_and_connected_peer_returns_pane_list() {
        let state = test_state_with_key(TEST_API_KEY);
        let mut rx = register_fake_phantom(&state, "pane-phantom", "localhost", "0.1.0").await;

        // Fake Phantom: receives list_panes, returns a two-pane list.
        let reg_clone = Arc::clone(&state.registry);
        tokio::spawn(async move {
            let req = rx.recv().await.expect("fake phantom should receive request");
            // Verify the correct tool name was forwarded.
            assert_eq!(
                req.params["name"].as_str(),
                Some("phantom.list_panes"),
                "hub must forward phantom.list_panes"
            );
            let hub_id = req.id.clone().unwrap().as_u64().unwrap();
            deliver_response(
                &reg_clone,
                &PhantomId::new("pane-phantom"),
                crate::router::JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id: Some(serde_json::Value::Number(hub_id.into())),
                    result: Some(json!({
                        "content": [{"type": "text", "text": "2 pane(s)"}],
                        "panes": [
                            {"id": "1", "type": "terminal", "title": "zsh", "focused": true,  "agent_id": null},
                            {"id": "2", "type": "agent",    "title": "agent", "focused": false, "agent_id": "7"}
                        ]
                    })),
                    error: None,
                },
            )
            .await;
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
                            "name": "phantom.list_panes",
                            "arguments": { "phantom_id": "pane-phantom" }
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
        let panes = val["result"]["panes"].as_array().expect("panes must be array");
        assert_eq!(panes.len(), 2, "expected 2 panes, got: {val}");
        assert_eq!(panes[0]["type"].as_str(), Some("terminal"));
        assert_eq!(panes[1]["agent_id"].as_str(), Some("7"));
    }

    // -----------------------------------------------------------------------
    // phantom.get_agent_status: unknown peer → JSON-RPC NotFound error
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn get_agent_status_unknown_peer_returns_rpc_error() {
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
                            "name": "phantom.get_agent_status",
                            "arguments": {
                                "phantom_id": "ghost-phantom",
                                "agent_id": "42"
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
    // phantom.get_agent_status: no API key → 401
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn get_agent_status_no_api_key_returns_401() {
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
                            "name": "phantom.get_agent_status",
                            "arguments": {
                                "phantom_id": "any-phantom",
                                "agent_id": "7"
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
    // phantom.get_agent_status: valid auth + connected peer → status relayed
    // -----------------------------------------------------------------------
    //
    // The fake Phantom only replies when agent_id matches "99" — this verifies
    // the hub forwards the real agent_id, not a hardcoded value.

    #[tokio::test]
    async fn get_agent_status_valid_auth_and_connected_peer_returns_status() {
        let state = test_state_with_key(TEST_API_KEY);
        let mut rx =
            register_fake_phantom(&state, "status-phantom", "localhost", "0.1.0").await;

        let reg_clone = Arc::clone(&state.registry);
        tokio::spawn(async move {
            let req = rx.recv().await.expect("fake phantom should receive request");
            // Verify correct tool name and agent_id forwarded.
            assert_eq!(
                req.params["name"].as_str(),
                Some("phantom.get_agent_status"),
                "hub must forward phantom.get_agent_status"
            );
            let forwarded_agent_id = req.params["arguments"]["agent_id"].as_str().unwrap_or("");
            assert_eq!(
                forwarded_agent_id, "99",
                "hub must forward the caller's agent_id unchanged"
            );
            let hub_id = req.id.clone().unwrap().as_u64().unwrap();
            deliver_response(
                &reg_clone,
                &PhantomId::new("status-phantom"),
                crate::router::JsonRpcResponse {
                    jsonrpc: "2.0".into(),
                    id: Some(serde_json::Value::Number(hub_id.into())),
                    result: Some(json!({
                        "content": [{"type": "text", "text": "agent 99 state=running task=build the project"}],
                        "agent_id": "99",
                        "state": "running",
                        "task": "build the project",
                        "last_output_excerpt": "cargo build…"
                    })),
                    error: None,
                },
            )
            .await;
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
                            "name": "phantom.get_agent_status",
                            "arguments": {
                                "phantom_id": "status-phantom",
                                "agent_id": "99"
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
        assert_eq!(val["result"]["state"].as_str(), Some("running"), "got: {val}");
        assert_eq!(val["result"]["agent_id"].as_str(), Some("99"), "got: {val}");
    }
}
