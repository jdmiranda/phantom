//! MCP Unix-socket listener.
//!
//! Binds a Unix domain socket and accepts JSON-RPC 2.0 requests from external
//! clients (Claude Code, `nc -U`, any MCP-speaking peer). Stateless requests
//! (initialize, tools/list, resources/list) are answered directly. Requests
//! that need live app state — `phantom.screenshot`, `phantom.run_command`,
//! reading `phantom://terminal/state`, etc. — are forwarded to the main
//! application via an mpsc command channel; the listener blocks on a
//! per-request reply channel before writing the response to the socket.
//!
//! One thread binds the socket and accepts; each accepted connection gets its
//! own worker thread. This handles multiple concurrent clients without async.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender, SyncSender};
use std::thread::{self, JoinHandle};

use anyhow::{Context, Result};
use log::{debug, error, info, warn};
use serde_json::json;

use crate::protocol::{self, JsonRpcRequest, INVALID_PARAMS, INTERNAL_ERROR};
use crate::server::PhantomMcpServer;

// ---------------------------------------------------------------------------
// Command channel
// ---------------------------------------------------------------------------

/// Commands sent from the listener thread(s) to the main App thread.
///
/// Each variant carries a `reply` sender. The listener thread blocks on
/// `reply.recv()` until the App has produced a result, then serializes it
/// into the JSON-RPC response.
#[derive(Debug)]
pub enum AppCommand {
    /// Capture the current frame and save it as a PNG at `path`.
    Screenshot {
        path: PathBuf,
        reply: SyncSender<Result<ScreenshotReply, String>>,
    },
    /// Write bytes to the focused pane's PTY (simulate typing).
    RunCommand {
        command: String,
        reply: SyncSender<Result<(), String>>,
    },
    /// Send a keypress to the app. State-aware: dismisses the boot screen
    /// if active; otherwise translates to terminal input bytes. Supports
    /// named keys ("Enter", "Tab", "Escape", "Space", "Up", "Down", "Left",
    /// "Right", "Backspace") and plain character strings.
    SendKey {
        key: String,
        reply: SyncSender<Result<String, String>>,
    },
    /// Extract visible terminal grid from the focused pane as plain text.
    ReadTerminalState {
        reply: SyncSender<Result<String, String>>,
    },
    /// Return project context as JSON.
    GetContext {
        reply: SyncSender<Result<serde_json::Value, String>>,
    },
    /// Execute a Phantom command (backtick mode: theme, debug, plain, agent, etc).
    PhantomCommand {
        command: String,
        reply: SyncSender<Result<String, String>>,
    },
}

/// Payload returned for a successful screenshot.
#[derive(Debug, Clone)]
pub struct ScreenshotReply {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
}

// ---------------------------------------------------------------------------
// Listener handle
// ---------------------------------------------------------------------------

/// A running MCP listener bound to a Unix socket.
///
/// Dropping the handle does not stop the listener (threads are detached);
/// the socket is cleaned up on process exit. Call [`stop`](Self::stop) for
/// an explicit shutdown.
pub struct McpListener {
    socket_path: PathBuf,
    _accept_thread: JoinHandle<()>,
}

impl McpListener {
    /// The socket path this listener is bound to.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Remove the socket file. Called on drop.
    fn cleanup(path: &Path) {
        if path.exists() {
            if let Err(e) = std::fs::remove_file(path) {
                warn!("Failed to remove MCP socket {}: {}", path.display(), e);
            }
        }
    }
}

impl Drop for McpListener {
    fn drop(&mut self) {
        Self::cleanup(&self.socket_path);
    }
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

/// Bind a Unix socket at `socket_path` and spawn the accept loop.
///
/// All tool calls that need app state are forwarded over `cmd_tx` to the main
/// thread. Stateless MCP methods are handled entirely inside the listener.
pub fn spawn(socket_path: PathBuf, cmd_tx: Sender<AppCommand>) -> Result<McpListener> {
    // Clean up any stale socket from a previous run.
    if socket_path.exists() {
        let _ = std::fs::remove_file(&socket_path);
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind MCP socket at {}", socket_path.display()))?;
    info!("MCP listener bound: {}", socket_path.display());

    let accept_path = socket_path.clone();
    let accept_thread = thread::Builder::new()
        .name("mcp-accept".into())
        .spawn(move || accept_loop(listener, accept_path, cmd_tx))
        .context("failed to spawn mcp-accept thread")?;

    Ok(McpListener {
        socket_path,
        _accept_thread: accept_thread,
    })
}

// ---------------------------------------------------------------------------
// Accept loop
// ---------------------------------------------------------------------------

fn accept_loop(listener: UnixListener, socket_path: PathBuf, cmd_tx: Sender<AppCommand>) {
    let _ = socket_path; // retained for debug/log
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let tx = cmd_tx.clone();
                if let Err(e) = thread::Builder::new()
                    .name("mcp-conn".into())
                    .spawn(move || handle_connection(stream, tx))
                {
                    error!("Failed to spawn mcp-conn thread: {e}");
                }
            }
            Err(e) => {
                warn!("MCP accept error: {e}");
                // Keep looping — a transient accept failure shouldn't kill the listener.
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Per-connection handler
// ---------------------------------------------------------------------------

fn handle_connection(stream: UnixStream, cmd_tx: Sender<AppCommand>) {
    let peer_addr = stream
        .peer_addr()
        .ok()
        .and_then(|a| a.as_pathname().map(|p| p.display().to_string()))
        .unwrap_or_else(|| "<anonymous>".into());
    debug!("MCP client connected: {peer_addr}");

    // We own one PhantomMcpServer instance per connection for tool/resource
    // registries. Stateful handlers go through the command channel instead.
    let server = PhantomMcpServer::new();

    // Stream wrapper: JSON-RPC 2.0 over MCP uses newline-delimited JSON.
    let mut write_half = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            warn!("mcp: try_clone failed: {e}");
            return;
        }
    };
    let reader = BufReader::new(stream);

    for line in reader.lines() {
        let line = match line {
            Ok(l) if l.trim().is_empty() => continue,
            Ok(l) => l,
            Err(e) => {
                debug!("mcp: client {peer_addr} read error: {e}");
                break;
            }
        };

        let request: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let resp = protocol::create_error(
                    serde_json::Value::Null,
                    protocol::PARSE_ERROR,
                    &format!("parse error: {e}"),
                );
                if write_response(&mut write_half, &resp).is_err() {
                    break;
                }
                continue;
            }
        };

        let response = dispatch(&server, &request, &cmd_tx);

        if write_response(&mut write_half, &response).is_err() {
            debug!("mcp: failed to write response to {peer_addr}; closing");
            break;
        }
    }

    debug!("MCP client disconnected: {peer_addr}");
}

fn write_response(
    stream: &mut UnixStream,
    response: &protocol::JsonRpcResponse,
) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(response).unwrap_or_else(|_| b"{}".to_vec());
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    stream.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Request dispatch
// ---------------------------------------------------------------------------

/// Dispatch a single JSON-RPC request. For tool calls that need live app
/// state, forward to the app thread via `cmd_tx` and block on reply.
fn dispatch(
    server: &PhantomMcpServer,
    request: &JsonRpcRequest,
    cmd_tx: &Sender<AppCommand>,
) -> protocol::JsonRpcResponse {
    // Tool calls are the only methods we intercept for live state; every
    // other method (initialize, tools/list, etc.) goes straight to the
    // stub server — which returns correct, static data for those.
    if request.method != "tools/call" {
        return server.handle_request(request);
    }

    let id = request.id.clone().unwrap_or(serde_json::Value::Null);
    let Some(params) = &request.params else {
        return protocol::create_error(id, INVALID_PARAMS, "missing params");
    };

    let tool_name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    match tool_name {
        "phantom.screenshot" => dispatch_screenshot(id, &args, cmd_tx),
        "phantom.run_command" => dispatch_run_command(id, &args, cmd_tx),
        "phantom.send_key" => dispatch_send_key(id, &args, cmd_tx),
        "phantom.get_context" => dispatch_get_context(id, cmd_tx),
        "phantom.command" => dispatch_phantom_command(id, &args, cmd_tx),
        // For every other tool, defer to the stub implementation in `server`.
        _ => server.handle_request(request),
    }
}

fn dispatch_send_key(
    id: serde_json::Value,
    args: &serde_json::Value,
    cmd_tx: &Sender<AppCommand>,
) -> protocol::JsonRpcResponse {
    let key = args
        .get("key")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if key.is_empty() {
        return protocol::create_error(id, INVALID_PARAMS, "missing 'key' argument");
    }

    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    if cmd_tx
        .send(AppCommand::SendKey {
            key: key.clone(),
            reply: reply_tx,
        })
        .is_err()
    {
        return protocol::create_error(id, INTERNAL_ERROR, "app command channel closed");
    }

    match reply_rx.recv() {
        Ok(Ok(note)) => protocol::create_response(
            id,
            json!({
                "content": [{"type": "text", "text": format!("key '{key}' sent: {note}")}],
                "key": key,
                "note": note,
            }),
        ),
        Ok(Err(e)) => protocol::create_response(
            id,
            json!({
                "content": [{"type": "text", "text": format!("send_key failed: {e}")}],
                "isError": true,
            }),
        ),
        Err(e) => protocol::create_error(id, INTERNAL_ERROR, &format!("app reply dropped: {e}")),
    }
}

fn dispatch_screenshot(
    id: serde_json::Value,
    args: &serde_json::Value,
    cmd_tx: &Sender<AppCommand>,
) -> protocol::JsonRpcResponse {
    // Default path: /tmp/phantom-screenshot-<timestamp>.png
    let default_path = format!(
        "/tmp/phantom-screenshot-{}.png",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or(&default_path);
    let path = PathBuf::from(path);

    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    if cmd_tx
        .send(AppCommand::Screenshot {
            path: path.clone(),
            reply: reply_tx,
        })
        .is_err()
    {
        return protocol::create_error(id, INTERNAL_ERROR, "app command channel closed");
    }

    match reply_rx.recv() {
        Ok(Ok(shot)) => protocol::create_response(
            id,
            json!({
                "content": [{
                    "type": "text",
                    "text": format!("screenshot saved: {} ({}x{})", shot.path.display(), shot.width, shot.height),
                }],
                "path": shot.path.display().to_string(),
                "width": shot.width,
                "height": shot.height,
            }),
        ),
        Ok(Err(e)) => protocol::create_response(
            id,
            json!({
                "content": [{"type": "text", "text": format!("screenshot failed: {e}")}],
                "isError": true,
            }),
        ),
        Err(e) => protocol::create_error(id, INTERNAL_ERROR, &format!("app reply dropped: {e}")),
    }
}

fn dispatch_run_command(
    id: serde_json::Value,
    args: &serde_json::Value,
    cmd_tx: &Sender<AppCommand>,
) -> protocol::JsonRpcResponse {
    let command = args
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if command.is_empty() {
        return protocol::create_error(id, INVALID_PARAMS, "missing 'command' argument");
    }

    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    if cmd_tx
        .send(AppCommand::RunCommand {
            command: command.clone(),
            reply: reply_tx,
        })
        .is_err()
    {
        return protocol::create_error(id, INTERNAL_ERROR, "app command channel closed");
    }

    match reply_rx.recv() {
        Ok(Ok(())) => protocol::create_response(
            id,
            json!({
                "content": [{"type": "text", "text": format!("sent: {command}")}],
            }),
        ),
        Ok(Err(e)) => protocol::create_response(
            id,
            json!({
                "content": [{"type": "text", "text": format!("run_command failed: {e}")}],
                "isError": true,
            }),
        ),
        Err(e) => protocol::create_error(id, INTERNAL_ERROR, &format!("app reply dropped: {e}")),
    }
}

fn dispatch_get_context(
    id: serde_json::Value,
    cmd_tx: &Sender<AppCommand>,
) -> protocol::JsonRpcResponse {
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    if cmd_tx.send(AppCommand::GetContext { reply: reply_tx }).is_err() {
        return protocol::create_error(id, INTERNAL_ERROR, "app command channel closed");
    }

    match reply_rx.recv() {
        Ok(Ok(ctx_json)) => protocol::create_response(
            id,
            json!({
                "content": [{"type": "text", "text": ctx_json.to_string()}],
                "context": ctx_json,
            }),
        ),
        Ok(Err(e)) => protocol::create_response(
            id,
            json!({
                "content": [{"type": "text", "text": format!("get_context failed: {e}")}],
                "isError": true,
            }),
        ),
        Err(e) => protocol::create_error(id, INTERNAL_ERROR, &format!("app reply dropped: {e}")),
    }
}

fn dispatch_phantom_command(
    id: serde_json::Value,
    args: &serde_json::Value,
    cmd_tx: &Sender<AppCommand>,
) -> protocol::JsonRpcResponse {
    let command = args
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if command.is_empty() {
        return protocol::create_error(id, INVALID_PARAMS, "missing 'command' argument");
    }

    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    if cmd_tx
        .send(AppCommand::PhantomCommand {
            command: command.clone(),
            reply: reply_tx,
        })
        .is_err()
    {
        return protocol::create_error(id, INTERNAL_ERROR, "app command channel closed");
    }

    match reply_rx.recv() {
        Ok(Ok(msg)) => protocol::create_response(
            id,
            json!({
                "content": [{"type": "text", "text": msg}],
            }),
        ),
        Ok(Err(e)) => protocol::create_response(
            id,
            json!({
                "content": [{"type": "text", "text": format!("command failed: {e}")}],
                "isError": true,
            }),
        ),
        Err(e) => protocol::create_error(id, INTERNAL_ERROR, &format!("app reply dropped: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};

    fn send_and_recv(stream: &mut UnixStream, req: &str) -> String {
        stream.write_all(req.as_bytes()).unwrap();
        stream.write_all(b"\n").unwrap();
        stream.flush().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        line
    }

    #[test]
    fn initialize_works_over_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("phantom-mcp-test.sock");
        let (cmd_tx, _cmd_rx) = mpsc::channel();
        let _listener = spawn(sock.clone(), cmd_tx).unwrap();

        // Give the accept thread a moment to settle.
        std::thread::sleep(std::time::Duration::from_millis(50));

        let mut stream = UnixStream::connect(&sock).unwrap();
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let resp = send_and_recv(&mut stream, req);
        assert!(resp.contains("\"serverInfo\""));
        assert!(resp.contains("phantom"));
    }

    #[test]
    fn tools_list_over_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("phantom-mcp-test.sock");
        let (cmd_tx, _cmd_rx) = mpsc::channel();
        let _listener = spawn(sock.clone(), cmd_tx).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));

        let mut stream = UnixStream::connect(&sock).unwrap();
        let req = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#;
        let resp = send_and_recv(&mut stream, req);
        assert!(resp.contains("phantom.screenshot"));
        assert!(resp.contains("phantom.run_command"));
    }

    #[test]
    fn screenshot_forwards_to_app_thread() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("phantom-mcp-test.sock");
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let _listener = spawn(sock.clone(), cmd_tx).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));

        // Spawn a fake app thread that fulfills the screenshot request.
        thread::spawn(move || {
            for cmd in cmd_rx {
                if let AppCommand::Screenshot { reply, .. } = cmd {
                    let _ = reply.send(Ok(ScreenshotReply {
                        path: "/tmp/fake.png".into(),
                        width: 100,
                        height: 50,
                    }));
                }
            }
        });

        let mut stream = UnixStream::connect(&sock).unwrap();
        let req = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"phantom.screenshot","arguments":{"path":"/tmp/fake.png"}}}"#;
        let resp = send_and_recv(&mut stream, req);
        assert!(resp.contains("/tmp/fake.png"), "resp was: {resp}");
        assert!(resp.contains("100"));
    }
}
