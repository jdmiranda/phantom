# Phantom Development Plan

**Last updated**: 2026-04-23
**Crates**: 19 | **Lines**: 37,645 | **Tests**: 817

---

## Completed

### Phase 0: Foundation
- [x] GPU rendering (wgpu, Metal/Vulkan/DX12)
- [x] Terminal emulation (alacritty_terminal)
- [x] CRT post-processing shaders (scanlines, bloom, curvature, etc)
- [x] 5 built-in themes (phosphor, amber, ice, blood, vapor)
- [x] Cinematic boot sequence (noise, skull, glitch logo, progress bars)
- [x] Tab bar + status bar widgets
- [x] Tmux-style pane splitting (Cmd+D, Cmd+Shift+D)
- [x] Config system (~/.config/phantom/config.toml + CLI args)
- [x] Command mode (backtick key) with live shader tuning
- [x] Debug shader HUD
- [x] Fullscreen launch, Retina/HiDPI scaling
- [x] Glyph caching (shape once per unique char)

### Phase 1: Semantic Layer
- [x] Command output parser (git, cargo, docker, npm, HTTP)
- [x] Error detection + highlighting (file:line:col across languages)
- [x] Kitty image protocol parser + GPU image manager
- [x] Structured command history (JSONL, searchable)

### Phase 2: Agents
- [x] Agent runtime (lifecycle, sandboxed tools, manager)
- [x] Claude API integration (background thread, tool use)
- [x] Agent pane rendering (animated borders, status headers)
- [x] Error → agent suggestion pipeline
- [x] Agent CLI commands
- [x] Process detach (alt-screen detection, animated borders)
- [x] Permission-based sandboxing

### Phase 3: Context & Memory
- [x] Project detection (11 languages, frameworks, package managers)
- [x] Persistent per-project memory
- [x] Session save/restore
- [x] Natural language command interpretation

### Phase 4: Ecosystem
- [x] WASM plugin system (trait-based, manifest, registry)
- [x] 5 official plugin manifests
- [x] Marketplace (search, install, uninstall)

### Extensions
- [x] Two-process supervisor (Erlang/OTP)
- [x] MCP server + client (JSON-RPC 2.0)
- [x] Screenshot capture (GPU readback + PNG + metadata)
- [x] Scene graph (retained, dirty tracking, z-order, layers)
- [x] AI brain thread (OODA loop, utility scoring, ambient)
- [x] Kitty image handler (chunk assembly, format decoding)
- [x] Quake-style console overlay (slide-down, input, scrollback)
- [x] Per-keystroke glitch FX (boot aesthetic extended to typing)
- [x] Video renderer + playback (H.264 frame decode, GPU texture upload)
- [x] Clippy must_use_candidate lint (workspace-wide)
- [x] Hardening: catch_unwind cleanup, char boundary fix, safe indexing

---

## In Progress / Queued

### App Architecture — Phase 1: AppCoordinator + Terminal Adapter (TOP PRIORITY)
> Full execution plan: [PHASE1-EXECUTION.md](PHASE1-EXECUTION.md)

**Framework (done):**
- [x] ADR-003: App Adapter + Pub/Sub + Spatial Negotiation (Accepted)
- [x] AppAdapter trait definition (phantom-adapter crate — adapter.rs)
- [x] App lifecycle states (Initializing → Running → Suspended → Exiting → Dead — lifecycle.rs)
- [x] AppRegistry with parallel vecs + gc() (registry.rs)
- [x] Pub/sub event bus with 256-msg cap, drain_for() (bus.rs)
- [x] SpatialPreference types (spatial.rs)

**Phase 1 (in progress) — wire first adapter end-to-end:**
- [ ] AppCoordinator (registry + bus + pane mapping + focus)
- [ ] TerminalAdapter (wrap PhantomTerminal as AppAdapter)
- [ ] Extend RenderOutput with GridData for terminal cells
- [ ] Wire coordinator into update loop (replace direct PTY read)
- [ ] Wire coordinator into render loop (replace render_terminal)
- [ ] Wire coordinator into input routing (replace direct PTY write)
- [ ] Wire coordinator into split/close commands
- [ ] Unit tests + integration tests + screenshot regression

**Phase 2+ (queued):**
- [ ] VideoAdapter, AgentAdapter, MonitorAdapter
- [ ] Floating panes + spatial negotiation in layout
- [ ] Bus wiring between adapters + user-created pipes
- [ ] AI command routing to adapters + dynamic MCP tools
- [ ] Headless apps (no render, data processing only)
- [ ] wasmtime integration (actually run .wasm binaries)

### Integration Wiring (CRITICAL)
- [x] Wire semantic parser into PTY output (error pattern scanner + brain events)
- [x] Wire AI brain to app event loop (spawn thread, idle events, action drain)
- [x] Wire error detection → suggestion overlay in render loop
- [x] Wire project context into status bar (auto-detect, git branch, refresh)
- [x] Wire session save on exit, restore on launch
- [x] Wire NLP interpreter into command mode (fallback handler)
- [x] Wire scene graph into app (structural nodes, resize sync, dirty tracking)
- [ ] Wire agent system to pane creation (spawn agent → create pane) — stub wired
- [ ] Scene graph: replace flat quad/glyph collection with scene-driven traversal
- [ ] Scene graph: dirty-flag GPU upload optimization

### Infrastructure
- [ ] TCP/WebSocket remote control listener
- [ ] Test hardening (integration tests, GPU visual regression)
- [ ] Performance profiling + scene graph integration
- [ ] Demo script

---

## Architecture Decision Records

| ADR | Title | Status |
|-----|-------|--------|
| [ADR-001](ARD-001-architecture-decisions.md) | Core architecture (terminal, GPU, text, layout, shaders) | Accepted |
| [ADR-002](ARD-002-wasm-app-adapter.md) | WASM App Adapter — everything is an app | Accepted |
| [ADR-003](ARD-003-app-lifecycle-pubsub.md) | App lifecycle, pub/sub, spatial negotiation | Accepted |
| [ADR-004](ARD-004-rust-skills-audit.md) | Rust skills integration + codebase audit | Accepted |
| [ADR-005](ARD-005-keystroke-glitch-fx.md) | Per-keystroke glitch animation | Accepted |

## Research Docs

| Doc | Topic |
|-----|-------|
| [scene-graph.md](research/scene-graph.md) | Retained scene graph, dirty tracking, FrankenTUI, Bevy |
| [supervisor-architecture.md](research/supervisor-architecture.md) | Erlang/OTP two-process model |
| [ai-control-loop.md](research/ai-control-loop.md) | OODA, utility AI, ambient agents, Claude Code internals |
| [spatial-negotiation.md](research/spatial-negotiation.md) | Wayland protocol, Cassowary, constraint-based tiling |

## Execution Plans

| Plan | Scope |
|------|-------|
| [PHASE1-EXECUTION.md](PHASE1-EXECUTION.md) | AppCoordinator + TerminalAdapter — failure analysis, agent coordination, testing |
| [plan-adapter-integration.md](plan-adapter-integration.md) | Full 5-phase adapter integration roadmap (Phases 1-5) |
