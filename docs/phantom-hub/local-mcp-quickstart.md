# Local Unix-Socket MCP — Quickstart for Claude Code

> **Phase 0 — no hub required.** Drive a Phantom instance running on the same machine as Claude Code. No cloud relay, no auth token, no network. See [issue #392](https://github.com/jdmiranda/phantom/issues/392) for the Phase 1+ remote path.

---

## How it works

When Phantom starts it binds a Unix domain socket and accepts JSON-RPC 2.0 connections. Claude Code connects, calls `tools/list`, and can then run shell commands in your focused pane, take screenshots, read output, manage pane splits, and access project memory.

```
Claude Code  ──JSON-RPC 2.0 (newline-delimited) over Unix socket──►  Phantom
```

No HTTP, no TLS, no credentials. The socket is protected by standard filesystem permissions (`0600`, owned by your user).

---

## Step 1 — Find the socket path

Phantom resolves the path in this order (`crates/phantom-app/src/app.rs:930-934`):

1. `$PHANTOM_MCP_SOCK` environment variable (if set).
2. `/tmp/phantom-mcp-<pid>.sock` — per-process fallback.

**Discover the live path:**

```bash
lsof -U 2>/dev/null | grep phantom
# phantom  12345  you  12u  unix /tmp/phantom-mcp-12345.sock type=STREAM

# Linux alternative:
ss -x | grep phantom
```

**Pin to a stable path (recommended)** — set before launching Phantom:

```bash
export PHANTOM_MCP_SOCK="$HOME/.phantom/mcp.sock"
cargo run --bin phantom-supervisor
```

---

## Step 2 — Configure Claude Code

Add a `phantom` entry to `~/.claude/mcp.json` (global) or `.claude/mcp.json` (project-local):

```json
{
  "mcpServers": {
    "phantom": {
      "command": "nc",
      "args": ["-U", "/tmp/phantom-mcp-12345.sock"]
    }
  }
}
```

Replace the path with the one from Step 1. If you pinned via `PHANTOM_MCP_SOCK`, use that stable path instead.

> Config schema version `2024-11-05`. See the [Claude Code MCP docs](https://docs.anthropic.com/claude-code/mcp) for the full reference.

---

## Step 3 — Verify: list all 9 tools

Start a new Claude Code session (or run `/mcp` to reload). Ask Claude to list Phantom tools — you should see:

```
phantom.run_command   phantom.read_output  phantom.screenshot
phantom.send_key      phantom.split_pane   phantom.get_context
phantom.get_memory    phantom.set_memory   phantom.command
```

Raw JSON-RPC smoke-test:

```bash
SOCK=/tmp/phantom-mcp-12345.sock
echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}' | nc -U "$SOCK"
echo '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}' | nc -U "$SOCK"
```

---

## Step 4 — Run a command in Phantom

```
User: Run `echo hello` in Phantom.
Claude: [calls phantom.run_command {"command":"echo hello"}]
Result: sent: echo hello

User: What did that print?
Claude: [calls phantom.read_output {"lines":10}]
Result: hello
```

`echo hello` appears in whichever pane has focus in Phantom.

---

## Tools reference

All 9 tools — descriptions from `crates/phantom-mcp/src/server.rs:201-304`.

| Tool | Required | Optional | Description |
|---|---|---|---|
| `phantom.run_command` | `command` | `pane_id` | Execute a shell command in a pane |
| `phantom.read_output` | — | `pane_id`, `lines` (default 50) | Get the last command's parsed output |
| `phantom.screenshot` | — | `pane_id`, `path` | Capture the terminal state as text; returns PNG path + dimensions |
| `phantom.send_key` | `key` | — | Send a keypress. Named: `Enter Tab Escape Space Backspace Up Down Left Right`. Anything else sent verbatim. Also dismisses boot screen. |
| `phantom.split_pane` | — | `direction` (`horizontal`\|`vertical`), `pane_id` | Create a new pane by splitting an existing one |
| `phantom.get_context` | — | — | Get project context (language, framework, etc.) |
| `phantom.get_memory` | `key` | — | Read a value from project memory |
| `phantom.set_memory` | `key`, `value` | — | Write a value to project memory |
| `phantom.command` | `command` | — | Execute a Phantom backtick-mode command: `theme <name>`, `debug`, `plain`, `boot`, `agent <prompt>`, `reload`, `quit` |

**Resources** (`resources/list`): `phantom://terminal/state` (text/plain), `phantom://project/context` (application/json), `phantom://history/recent` (application/json). All require a live app connection.

---

## Wire protocol

JSON-RPC 2.0 over Unix domain socket, newline-delimited. One JSON object per line, no HTTP framing. Protocol version `2024-11-05`. Tool calls that need live app state time out after **10 seconds**.

**Sources:** `crates/phantom-mcp/src/listener.rs` (accept loop + dispatch), `crates/phantom-mcp/src/server.rs` (tool/resource registry), `crates/phantom-mcp/src/protocol.rs` (message types).

---

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `No such file or directory` | Phantom not running, or PID changed | Re-run `lsof -U \| grep phantom`; set `PHANTOM_MCP_SOCK` for a stable path |
| `Permission denied` | Socket owned by a different user | Run Phantom and Claude Code as the same user |
| `"requires a live app connection"` | Socket up, but app thread not ready | Wait for Phantom to finish booting, then retry |
| No response (10 s timeout) | App thread busy (long agent turn, render hang) | Wait for Phantom UI to become responsive |

---

## Limitations (Phase 0)

- **Single-host only** — Unix socket, not network-accessible.
- **No authentication** — gated by filesystem permissions (`0600`).
- **No pane targeting (yet)** — `pane_id` is accepted but not fully implemented; tools operate on the focused pane.
- **Screenshot is local** — PNG written to local disk, not transmitted over the socket.

Phase 1+ introduces Phantom Hub for remote access, auth, and multi-pane routing. See [#392](https://github.com/jdmiranda/phantom/issues/392).
