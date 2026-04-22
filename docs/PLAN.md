# Phantom Development Plan

**Last updated**: 2026-04-21
**Crates**: 18 | **Lines**: 27,939 | **Tests**: 663

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

---

## In Progress / Queued

### App Architecture (NEW — top priority)
- [ ] **ADR-003: App Adapter + Pub/Sub + Spatial Negotiation**
- [ ] AppAdapter trait definition (phantom-adapter crate)
- [ ] App lifecycle states (Initializing → Running → Suspended → Exiting → Dead)
- [ ] App discovery + auto-registration
- [ ] Pub/sub event bus between apps (structured data streams)
- [ ] Headless apps (no render, data processing only)
- [ ] Spatial negotiation protocol (preferences, arbiter, neighbor queries)
- [ ] Refactor panes from Vec<Pane> to Vec<Box<dyn AppAdapter>>
- [ ] TerminalApp implementation (dogfood)
- [ ] AgentApp implementation (dogfood)
- [ ] wasmtime integration (actually run .wasm binaries)

### Integration Wiring (CRITICAL)
- [ ] Wire semantic parser into PTY output (intercept real output)
- [ ] Wire AI brain to app event loop (send/receive events)
- [ ] Wire agent system to pane creation (spawn agent → create pane)
- [ ] Wire error detection → suggestion overlay in render loop
- [ ] Wire project context into status bar
- [ ] Wire session save on exit, restore on launch
- [ ] Wire NLP interpreter into command mode
- [ ] Wire scene graph into render pipeline (replace flat re-upload)

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
| ADR-003 | App lifecycle, pub/sub, spatial negotiation | Drafting |

## Research Docs

| Doc | Topic |
|-----|-------|
| [scene-graph.md](research/scene-graph.md) | Retained scene graph, dirty tracking, FrankenTUI, Bevy |
| [supervisor-architecture.md](research/supervisor-architecture.md) | Erlang/OTP two-process model |
| [ai-control-loop.md](research/ai-control-loop.md) | OODA, utility AI, ambient agents, Claude Code internals |
| [spatial-negotiation.md](research/spatial-negotiation.md) | Wayland protocol, Cassowary, constraint-based tiling |
