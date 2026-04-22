# Phantom Handoff Document

**Date**: 2026-04-21
**Session**: Initial build session
**Author**: Jeremy Miranda + Claude

---

## What Was Built

In one session, Phantom went from a spec document (PHANTOM.md) to a 19-crate, 29,206-line Rust application with 703 tests. Every phase of the original spec is implemented, plus extensions.

### Stats
- **19 crates** in a Cargo workspace
- **29,206 lines** of Rust
- **703 tests**, 0 failures
- **25 commits** on main
- **Repo**: https://github.com/jdmiranda/phantom

### What's Working (runs and renders)
- GPU-accelerated terminal emulator (fullscreen, Retina-scaled)
- CRT post-processing shaders (5 themes, live-tweakable)
- Cinematic boot sequence (noise, skull, glitch logo, progress bars, keypress to proceed)
- Tmux-style pane splitting (Cmd+D/Shift+D, focus cycling, close)
- Command mode (backtick): `set`, `theme`, `debug`, `plain`, `agent`, `boot`, `quit`
- Debug shader HUD with live parameter adjustment
- Config file (`~/.config/phantom/config.toml`) + CLI args

### What's Built But Not Wired Into App
These crates exist with full APIs and tests, but are NOT yet connected to the running application's event loop. This is the #1 priority for the next session.

| Crate | What It Does | Wiring Needed |
|-------|-------------|---------------|
| `phantom-semantic` | Parses git/cargo/docker output into structured data | Hook into PTY output stream |
| `phantom-agents` | Agent runtime, tools, Claude API, CLI, suggestions | Create agent panes from app, connect to Claude API |
| `phantom-brain` | OODA loop, utility scoring, ambient intelligence | Spawn brain thread from app, wire event channels |
| `phantom-context` | Project detection (language, framework, git) | Call on startup, feed to status bar + brain |
| `phantom-memory` | Per-project key-value persistence | Connect to brain orient phase |
| `phantom-history` | Structured command history (JSONL) | Append after each command completes |
| `phantom-session` | Session save/restore | Save on exit, restore on launch |
| `phantom-nlp` | Natural language command interpreter | Wire into command mode as fallback |
| `phantom-plugins` | WASM plugin host, registry, marketplace | Add wasmtime, load plugins on startup |
| `phantom-mcp` | MCP server + client (JSON-RPC) | Start server on startup, expose tools |
| `phantom-scene` | Retained scene graph, dirty tracking | Replace flat quad/glyph re-upload in render |
| `phantom-adapter` | AppAdapter trait, registry, pub/sub, spatial | Refactor panes to Box<dyn AppAdapter> |

### Known Bugs
- **Supervisor restart loop**: supervisor spawns phantom but heartbeat connection is flaky. Timeout was increased to 10s as a workaround. Root cause: need to verify PTY + GPU init complete before heartbeat starts.
- **Boot pause timing**: boot sequence pause at "SYSTEM READY" works but had a race condition (fixed, but should be verified on different frame rates).
- **Zoom doesn't resize terminal grid**: Cmd+= changes font size but doesn't recompute terminal cols/rows.

---

## Architecture Overview

```
phantom                  # winit event loop, window management
phantom-supervisor       # Erlang/OTP two-process monitor
phantom-app              # App orchestrator (owns all subsystems)
phantom-renderer         # GPU: wgpu, atlas, quads, grid, post-fx, images, screenshots
phantom-terminal         # PTY: alacritty_terminal, input, output, kitty, alt-screen, process
phantom-ui               # themes, layout (taffy), keybinds, widgets
phantom-semantic         # command parser, error detection, highlighting
phantom-agents           # agent runtime, tools, permissions, Claude API, CLI, suggestions, render
phantom-brain            # ambient OODA loop, utility scoring, events
phantom-context          # project detection, framework, git, commands
phantom-memory           # per-project key-value store
phantom-history          # structured command history (JSONL)
phantom-session          # session save/restore
phantom-nlp              # natural language command interpreter
phantom-plugins          # WASM plugin host, manifests, registry, marketplace, builtins
phantom-mcp              # MCP server + client (JSON-RPC 2.0)
phantom-protocol         # supervisor socket communication
phantom-scene            # retained scene graph, dirty tracking
phantom-adapter          # AppAdapter trait, registry, pub/sub, spatial negotiation
```

---

## Key Files to Read First

1. **`PHANTOM.md`** — the original spec/manifesto
2. **`docs/VISION.md`** — updated vision (app platform, not just terminal)
3. **`docs/PLAN.md`** — master task tracker (what's done, what's queued)
4. **`docs/ARD-001-architecture-decisions.md`** — why wgpu, alacritty_terminal, cosmic-text, etc
5. **`docs/ARD-002-wasm-app-adapter.md`** — everything is an app, WASM sandbox
6. **`docs/ARD-003-app-lifecycle-pubsub.md`** — lifecycle states, pub/sub event bus, spatial negotiation
7. **`crates/phantom-app/src/app.rs`** — the main App struct, render loop, key handling

---

## Priority Queue for Next Session

### P0: Integration Wiring (CRITICAL)
The code exists but isn't connected. This is the difference between "bunch of crates" and "working product."

1. **Wire brain thread**: spawn `phantom-brain` from `App::new()`, send `AiEvent::CommandComplete` after each PTY read, receive `AiAction` in update loop
2. **Wire semantic parser**: intercept PTY output, run through `SemanticParser::parse()`, emit to brain
3. **Wire error → suggestion**: when brain emits `AiAction::ShowSuggestion`, render the suggestion overlay
4. **Wire project context**: call `ProjectContext::detect()` on startup, display in status bar
5. **Wire session restore**: load last session on startup, save on exit
6. **Wire NLP into command mode**: if command doesn't match built-in, try `NlpInterpreter::interpret()`

### P1: AppAdapter Refactor
7. Implement `TerminalApp` wrapping `PhantomTerminal`
8. Implement `AgentApp` wrapping `Agent`
9. Refactor `App.panes` from `Vec<Pane>` to `Vec<Box<dyn AppAdapter>>`
10. Wire event bus between apps

### P2: Scene Graph Integration
11. Replace flat quad/glyph re-upload with scene graph dirty tracking
12. Only re-render panes that received PTY output

### P3: WASM Runtime
13. Add wasmtime dependency
14. Implement WASM host functions bridging AppAdapter methods
15. Load `.wasm` plugins at runtime

### P4: Remaining Features
16. TCP/WebSocket remote control listener
17. Test hardening (integration tests, GPU visual regression)
18. Demo script

---

## Design Decisions Still Open

- **App Adapter**: should the adapter trait live in its own crate (current) or be merged into phantom-app?
- **WASM vs native plugins**: should built-in apps (terminal, agent) run through WASM or stay native? Performance vs dogfooding purity.
- **Scene graph granularity**: per-pane dirty tracking vs per-row vs per-cell? Tradeoff: more granularity = less GPU work but more CPU bookkeeping.
- **AI brain trigger policy**: how aggressive should the ambient AI be? Current quiet_score baseline is 0.5. May need user-configurable threshold.
- **MCP transport**: stdio (like Claude Code) vs Unix socket vs TCP? May need all three for different use cases.

---

## Research Docs (read for context)

| Doc | Key Insight |
|-----|-------------|
| `docs/research/ai-control-loop.md` | OODA + Utility AI. Quiet score baseline prevents annoying suggestions. Event-driven, not polling. |
| `docs/research/scene-graph.md` | FrankenTUI's diff-based rendering. 95% GPU upload reduction for typical terminal use. |
| `docs/research/spatial-negotiation.md` | Wayland's two-phase negotiation + Cassowary constraint solving. Apps declare preferences, arbiter resolves. |
| `docs/research/supervisor-architecture.md` | Erlang/OTP one_for_one restart. Separate process = survives crashes. |

---

## How to Run

```bash
# Build
cargo build --release

# Run standalone
cargo run --bin phantom

# Run with supervisor
cargo run --bin phantom-supervisor

# Run tests
cargo test --workspace

# CLI options
cargo run --bin phantom -- --help
cargo run --bin phantom -- --theme amber --no-boot
```

## How to Test

```bash
# All tests
cargo test --workspace

# Specific crate
cargo test -p phantom-agents
cargo test -p phantom-semantic

# With output
cargo test --workspace -- --nocapture
```

---

## User Preferences (Jeremy)

- Wants ambitious, "impress me not please me" approach
- Loves the cyberpunk aesthetic but wants effects subtle (curvature 0.0, scanlines 0.08)
- Wants the boot sequence to be cinematic and pausable
- Cares about the AI being ambient/proactive, not reactive like Claude Code
- Inspired by Yahoo Pipes for data flow between apps
- Wants apps to negotiate spatial layout, not just get shoved into splits
- Values research-backed decisions with ADRs
- Runs on Apple M3 Max (Retina 2x display)
