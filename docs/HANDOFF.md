# Phantom Handoff Document

**Date**: 2026-04-21 (end of session)
**Session**: Initial build session — spec to working product
**Author**: Jeremy Miranda + Claude

---

## What Was Built This Session

From a spec document (PHANTOM.md) to a 19-crate, 29,953-line Rust application with 703 tests in one session. Every phase of the original spec is implemented plus significant extensions.

### Final Stats
- **19 crates** in a Cargo workspace
- **29,953 lines** of Rust
- **703 tests**, 0 failures, 0 warnings (deny(warnings) enforced)
- **31 commits** on main
- **Repo**: https://github.com/jdmiranda/phantom

---

## What's Working Right Now

### Headless REPL Mode (fully functional)
```bash
cargo run --bin phantom -- --headless
```
- Project context auto-detected (Rust, Cargo, git state)
- AI brain thread running (OODA loop, utility scoring)
- `chat <message>` — talk to Claude with session context. Chat spawns least-privilege agents to read files/run commands. Conversation persists across messages.
- `agent "prompt"` — spawn AI agent with full tool use (ReadFile, WriteFile, RunCommand, SearchFiles, GitStatus, GitDiff, ListFiles)
- Natural language commands: `build` → `cargo build`, `what changed` → `git log`, `fix it` → spawn agent
- Semantic parsing of all command output
- Error detection → agent suggestions
- Persistent memory + command history
- `.env` file loading for API key

### GUI Mode (renders but not fully wired)
```bash
cargo run --bin phantom
```
- GPU-accelerated terminal (wgpu, Metal/Vulkan)
- CRT post-processing shaders (5 themes, live-tweakable)
- Cinematic boot sequence (noise, skull, glitch logo, progress bars, keypress to proceed)
- Tmux-style pane splitting (Cmd+D/Shift+D)
- Command mode (backtick): `set`, `theme`, `debug`, `plain`, `agent`, `boot`, `quit`
- Debug shader HUD with live parameter sliders
- Fullscreen, Retina/HiDPI scaling
- Process detach (alt-screen detection, animated borders)

### API Key Setup
```bash
# .env file in project root (already created, gitignored)
ANTHROPIC_API_KEY=sk-ant-...
```

---

## Architecture — 19 Crates

```
phantom                  # main binary (winit + headless REPL)
phantom-supervisor       # Erlang/OTP two-process monitor
phantom-app              # GUI app orchestrator
phantom-renderer         # GPU: wgpu, atlas, quads, grid, post-fx, images, screenshots
phantom-terminal         # PTY: alacritty_terminal, input, output, kitty, alt-screen, process
phantom-ui               # themes, layout (taffy), keybinds, widgets
phantom-semantic         # command parser, error detection, highlighting
phantom-agents           # agent runtime, tools, permissions, Claude API, CLI, suggestions, render
phantom-brain            # ambient OODA loop, utility scoring, events
phantom-context          # project detection, framework, git, commands
phantom-memory           # per-project key-value memory
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

1. **`PHANTOM.md`** — original spec/manifesto
2. **`README.md`** — comprehensive feature list + commands
3. **`docs/VISION.md`** — updated vision (app platform, ambient AI)
4. **`docs/PLAN.md`** — master task tracker
5. **`docs/HANDOFF.md`** — this file
6. **`docs/ARD-001-architecture-decisions.md`** — core tech choices
7. **`docs/ARD-002-wasm-app-adapter.md`** — everything is an app
8. **`docs/ARD-003-app-lifecycle-pubsub.md`** — lifecycle, pub/sub, spatial negotiation
9. **`docs/research/ai-control-loop.md`** — OODA, utility AI, ambient agents, Claude Code internals
10. **`docs/research/scene-graph.md`** — FrankenTUI, Bevy, dirty tracking
11. **`docs/research/spatial-negotiation.md`** — Wayland, Cassowary, constraint tiling
12. **`crates/phantom/src/headless.rs`** — the headless REPL (most integrated code)
13. **`crates/phantom-app/src/app.rs`** — the GUI app struct + render loop

---

## Priority Queue for Next Session

### P0: Telemetry (quick, high value)
- Wrap `tracing` + `tracing-subscriber` with Phantom-specific event types
- Non-blocking structured events (AgentSpawned, CommandExecuted, BrainDecision, etc.)
- JSONL subscriber for history, brain subscriber for observation
- ~100 lines wrapping existing ecosystem

### P1: Integration Wiring (CRITICAL — biggest gap)
The code exists in 19 crates but many aren't wired into the running app:

| What | Wire From | Wire To |
|------|-----------|---------|
| Semantic parser | PTY output stream | Brain + history |
| AI brain | App event loop | Suggestion overlay |
| Agent panes | Agent spawn | GUI pane creation |
| Error suggestions | Error detection | Render overlay |
| Project context | Startup detection | Status bar |
| Session restore | App startup | Pane recreation |
| NLP interpreter | Command mode | Fallback handler |
| Scene graph | Layout changes | Render pipeline |

### P2: AppAdapter Refactor
- Implement `TerminalApp` wrapping PhantomTerminal
- Implement `AgentApp` wrapping Agent
- Refactor `App.panes` from `Vec<Pane>` to `Vec<Box<dyn AppAdapter>>`
- Wire event bus between apps
- Spatial negotiation in the arbiter

### P3: Remaining Features
- TCP/WebSocket remote control listener (#45)
- wasmtime integration (actually run .wasm plugins)
- GPU visual regression tests (screenshot comparison)
- Test hardening (integration tests across crates)
- Demo script

---

## Design Decisions — Open Questions

| Question | Context |
|----------|---------|
| Telemetry: wrap tracing or build custom? | **Decided: wrap tracing.** Don't rebuild what exists. |
| Chat tools: direct file access or spawn agents? | **Decided: spawn agents.** Chat is commander, agents are workers. Least privilege. |
| AppAdapter: own crate or merge into phantom-app? | Currently own crate. May merge later for simplicity. |
| WASM vs native for built-in apps? | Performance says native, dogfooding says WASM. Start native, migrate to WASM when runtime is stable. |
| AI brain aggressiveness? | quiet_score = 0.5 baseline. May need to be user-configurable. |
| MCP transport? | Need stdio (Claude Code), Unix socket (local), TCP (remote). Start with all three. |

---

## Known Bugs

| Bug | Severity | Notes |
|-----|----------|-------|
| Supervisor heartbeat flaky | Medium | Timeout increased to 10s workaround. Root cause: GPU init timing. |
| Zoom doesn't resize terminal grid | Low | Font changes but cols/rows not recomputed. |
| Boot sequence log noise | Low | `[INFO phantom_brain]` prints during boot before prompt. |
| Chat tool_use_id tracking | Medium | Multi-turn tool calls may not have correct IDs for the API. |

---

## Research Docs

| Doc | Key Insight |
|-----|-------------|
| `research/ai-control-loop.md` | OODA + Utility AI. Event-driven (70-90% less latency than polling). Quiet score prevents annoying suggestions. Claude Code is reactive; Phantom is ambient. |
| `research/scene-graph.md` | FrankenTUI diff-based rendering. 95% GPU upload reduction. Dirty flags + retained subtrees. |
| `research/spatial-negotiation.md` | Wayland two-phase negotiation + Cassowary constraint solving. Apps declare preferences, arbiter resolves. |
| `research/supervisor-architecture.md` | Erlang/OTP one_for_one. Separate process survives crashes. |

---

## User Preferences (Jeremy)

- Ambitious, "impress me not please me"
- Loves cyberpunk aesthetic but subtle effects (curvature 0.0, scanlines 0.08)
- Boot sequence should be cinematic and pausable
- AI must be ambient/proactive, not reactive like Claude Code
- Inspired by Yahoo Pipes for data flow between apps
- Apps negotiate spatial layout, not just shoved into splits
- Values research-backed decisions with ADRs
- Warnings as errors — zero tolerance for compiler warnings
- Wants the app to build itself (headless mode + agents working on the codebase)
- Chat = commander (spawns agents), Agent = worker (has tools)
- Prompt should say [USER] not >
- Runs on Apple M3 Max (Retina 2x display)
- API key stored in .env (gitignored), not environment

---

## How to Start

```bash
# Headless mode (brain + agents + chat)
cargo run --bin phantom -- --headless

# GUI mode (terminal + shaders + panes)
cargo run --bin phantom

# With supervisor
cargo run --bin phantom-supervisor

# Run tests
cargo test --workspace

# CLI help
cargo run --bin phantom -- --help
```

### In Headless Mode
```
[USER]: help                          # see all commands
[USER]: chat who are you              # talk to AI
[USER]: chat read docs/PLAN.md        # AI spawns agent to read, then discusses
[USER]: agent "fix the failing tests" # spawn agent with full tools
[USER]: status                        # project/agent/memory status
[USER]: build                         # runs cargo build (NLP)
[USER]: what changed                  # runs git log (NLP)
```
