# ARD-002: WASM App Adapter — Everything Is An App

**Status**: Accepted
**Date**: 2026-04-21
**Authors**: Jeremy Miranda, Claude

---

## Decision

Every pane in Phantom is an **App** implementing the `AppAdapter` trait. Apps run sandboxed in WASM via wasmtime. The terminal, agent panes, status bar, debug HUD, and third-party plugins all use the same interface. The AI brain can see and control all apps equally.

## Context

Phantom needs a unified way to:
1. Render arbitrary content in panes (terminals, agents, database browsers, dashboards)
2. Let the AI brain inspect and control everything in the workspace
3. Let third-party developers build apps without recompiling Phantom
4. Sandbox untrusted code so a bad plugin can't crash the system or steal data

## The App Adapter Trait

```rust
trait AppAdapter: Send {
    fn app_type(&self) -> &str;
    fn permissions(&self) -> &[Permission];
    fn render(&self, rect: &Rect) -> (Vec<QuadInstance>, Vec<GlyphInstance>);
    fn handle_input(&mut self, key: &KeyEvent) -> bool;
    fn get_state(&self) -> serde_json::Value;
    fn accept_command(&mut self, cmd: &str, args: &Value) -> Result<String>;
    fn update(&mut self, dt: f32);
    fn is_alive(&self) -> bool;
}
```

**Dogfooding**: Phantom's own components implement this trait:
- `TerminalApp` — wraps PhantomTerminal + PTY
- `AgentApp` — wraps Agent + Claude API
- `StatusBarApp` — wraps StatusBar widget
- `TabBarApp` — wraps TabBar widget
- `BootApp` — wraps BootSequence

Third-party apps implement the same trait, compiled to WASM.

## Why WASM

### Options Considered

| Option | Pros | Cons |
|--------|------|------|
| **Native Rust plugins** (dylib) | Fast, full API access | Must recompile. Unsafe — can crash Phantom. Language-locked to Rust. |
| **Lua scripting** | Simple, lightweight | Slow for heavy computation. Limited ecosystem. No sandboxing. |
| **JavaScript (V8/Deno)** | Huge ecosystem, fast JIT | Massive runtime dependency (~30MB). GC pauses. Overkill for plugins. |
| **WASM (chosen)** | Sandboxed. Any language. Fast (near-native). Small runtime. | Slightly more complex host API. No direct filesystem/network access (by design). |

### WASM Advantages for Phantom

1. **Language agnostic**: developers write in Rust, Go, C, Python, AssemblyScript, Zig — anything that compiles to WASM.

2. **Sandboxed by default**: WASM runs in a linear memory sandbox. A plugin cannot:
   - Access Phantom's memory
   - Read arbitrary files
   - Open network connections
   - Call system APIs
   
   Unless Phantom explicitly grants these through WASI (WebAssembly System Interface) or host functions.

3. **Crash isolation**: if a WASM module panics or traps, Phantom catches it. The plugin dies, Phantom keeps running. Compare to native plugins where a segfault kills the entire process.

4. **Permission enforcement at compile time**: the WASM binary literally cannot contain syscalls. All capabilities are injected by the host. The `Permission` enum maps directly to which host functions are made available to the module.

5. **Hot reload**: load a new `.wasm` file, instantiate it, swap the old module. No process restart. Live plugin updates.

6. **Size**: a typical WASM plugin is 10-100KB. Compare to native dylibs (1-10MB) or embedding V8 (30MB+).

7. **Deterministic execution**: same input → same output. Useful for testing and reproducibility.

## WASM in the Agent Context

Agents benefit from WASM in two ways:

### Agents AS WASM Apps
An agent's UI (the pane with animated borders, streaming output, status header) can be a WASM app. The agent logic runs on the brain thread, but the agent's visual representation is a WASM module that renders into its pane via AppAdapter.

### Agent Tools AS WASM Modules
Third-party agent tools can be distributed as WASM modules. A "database query" tool doesn't need to be built into Phantom — it's a WASM plugin that the agent loads on demand. The tool runs sandboxed, gets only the permissions it declared, and returns structured results.

```
Agent Brain Thread
    ↓
Decides to use "db-query" tool
    ↓
Loads db-query.wasm (if not already loaded)
    ↓
Calls tool through WASM host interface
    ↓
WASM module gets Network permission (declared in manifest)
    ↓
Executes query, returns JSON result
    ↓
Agent processes result, continues reasoning
```

### AI Visibility Into WASM Apps
Every WASM app implements `get_state()` which returns structured JSON. The AI brain calls this on every app in the workspace to maintain its world model. A database browser app might return:

```json
{
  "app_type": "db-browser",
  "connected_to": "postgres://localhost:5432/mydb",
  "current_table": "users",
  "visible_rows": 25,
  "selected_row": 3,
  "query": "SELECT * FROM users WHERE active = true"
}
```

The AI can read this and say "I see you're looking at the users table. The auth bug is in the `last_login` column — want me to show you the affected rows?"

## Security Model

```
Permission Layers:
─────────────────────────────────────────────────
Layer 1: WASM sandbox (cannot escape linear memory)
Layer 2: Host function gating (only permitted APIs injected)  
Layer 3: Permission manifest (declared up front, user approves)
Layer 4: Capability tokens (revocable at runtime)
Layer 5: OS sandbox (bubblewrap/landlock for extra isolation)
```

The manifest declares permissions:
```toml
[permissions]
read_files = true
write_files = false
run_commands = false
network = ["api.example.com"]  # allowlisted domains only
```

Phantom prompts the user on first load: "db-browser wants Network access to api.example.com. Allow?" Once granted, the permission is stored in memory. Can be revoked via `` ` plugin disable db-browser ``.

## Implementation Plan

1. Define `AppAdapter` trait in a new `phantom-adapter` crate
2. Implement `TerminalApp` wrapping current PhantomTerminal
3. Implement `AgentApp` wrapping current Agent
4. Refactor `App.panes` from `Vec<Pane>` to `Vec<Box<dyn AppAdapter>>`
5. Add wasmtime dependency, implement WASM host functions
6. WASM apps call host functions → Phantom routes to AppAdapter methods
7. Plugin marketplace delivers `.wasm` files
8. AI brain calls `get_state()` on all apps in every OODA cycle

## References

- [WebAssembly specification](https://webassembly.org/)
- [wasmtime — Rust WASM runtime](https://wasmtime.dev/)
- [WASI — WebAssembly System Interface](https://wasi.dev/)
- [Bevy's next-gen scene/UI system](https://github.com/bevyengine/bevy/discussions/14437) — similar "everything is a node" philosophy
- [Emacs "everything is a buffer"](https://www.gnu.org/software/emacs/) — the original dogfooding architecture
- [Plan 9 "everything is a file"](https://9p.io/plan9/) — uniform interfaces for all resources
