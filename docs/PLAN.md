# Phantom Development Plan

**Last updated**: 2026-04-24
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

### Engine Foundations (from Game Engine Architecture audit — 2026-04-24)

> Source: Jason Gregory, *Game Engine Architecture* (chs 5, 6, 7, 9, 10, 14). Full notes in [references/Game-Engine-Architecture.pdf](references/Game-Engine-Architecture.pdf) (gitignored). These are not optional polish — they're the missing substrate that every mature engine has and Phantom currently doesn't. Folded into Phase 1 execution.

**Tier 1 — foundational (blocks Phase 1 sign-off):**
- [ ] **Split render loop from update loop** [ch7]. Render at 60Hz; update subsystems at their own cadences (agents 1–2Hz, context engine 2–5Hz, brain 5–10Hz, sysmon 1Hz, renderer 60Hz). Cadence table lives on AppCoordinator; each adapter declares its own rate. Currently everything ticks off the frame — wastes CPU and stutters agent work.
- [ ] **`Clock` type with pause/scale/single-step** [ch7.4-7.5]. One type, many instances: real-time, session-time (pausable), per-video playback, per-glitch-FX, per-animation. Slo-mo / freeze-frame / FX scrubbing all fall out. Crate: [phantom-scene](../crates/phantom-scene/).
- [ ] **dt clamp on main loop** [ch7.5.5]. If measured dt > 100ms (debugger pause, OS suspend), clamp to target frame time. One-line fix in App::tick; prevents physics/animation/FX explosion on resume.
- [ ] **Explicit `start_up()` / `shut_down()` per subsystem** [ch5.1]. 19 crates have an implicit dep DAG (renderer ↔ scene ↔ terminal ↔ supervisor ↔ brain ↔ adapter). Replace scattered `::new()` calls with an ordered init sequence in `App::new()`, reverse order on drop. Enables clean restart-in-place and crash-report consistency.

**Tier 2 — ship with Phase 1 (user-visible payoff):**
- [ ] **`DebugDrawManager` with queued primitives** [ch9.2]. `AddLine`, `AddSphere`, `AddCross`, `AddString` — each takes `(color, duration, depth_tested, world_or_screen_space)`. Queue drained by renderer end-of-frame. Unblocks: agent annotation overlays, layout debug boxes for taffy, video bounds, glitch FX origins, scene-node axes. Crate: new `phantom-debug-draw` OR module inside [phantom-renderer](../crates/phantom-renderer/).
- [ ] **Console overlay speaks full command stack** [ch9.4]. The console (already shipped) must execute anything the brain router / NLP can execute — not a hard-coded command list. Wire [phantom-nlp](../crates/phantom-nlp/) and [phantom-brain](../crates/phantom-brain/) as the console's evaluator. `> deploy staging` works identically typed vs. spoken.
- [ ] **Channel-tagged logging + file mirror + panic flush** [ch9.1]. Per-subsystem channel (`RENDERER|AGENTS|BRAIN|TERMINAL|MCP|SEMANTIC|...`) as bitmask filter at runtime. Always mirror to `~/.config/phantom/logs/phantom.log`. Flush on panic. Crash report dump: stack + active sessions + open agents + last N commands. `log` crate already present — add target convention + custom logger.
- [ ] **In-frame hierarchical profiler** [ch9.8]. `profile_scope!("render.shader.crt")` macro; high-res timer builds a per-frame tree; overlay draws flame-graph + per-subsystem bars + timeline. Integrate tracy (don't roll own). Non-negotiable at 60fps with the full shader stack plus agent work plus terminal reflow.

**Tier 3 — architectural upgrades (ALL ship in Phase 1 — no deferrals):**
- [ ] **Job queue for agent work** [ch7.6.5-7.6.6]. Replace per-agent threads with a worker pool picking up (prompt, context, tools) jobs. Scales 1→N agents naturally; main thread never blocks on agent thread. Aligns perfectly with existing AgentAdapter shape. Crate: [phantom-supervisor](../crates/phantom-supervisor/) + [phantom-agents](../crates/phantom-agents/).
- [ ] **Async result pattern throughout** [ch7.6.6]. Request on frame N, pick up on frame N+1. Applies to: semantic parse, NLP interpret, memory query, brain decision, MCP call. Never block the render loop. Wired into the job queue.
- [ ] **Unified `ResourceManager` with GUID registry + ref-count + streaming** [ch6.2]. Single loader for shaders, fonts, videos, themes, plugin WASM, agent system prompts. One copy per GUID, ref-counted across sessions, async loading so frame never stalls. Today these are scattered across [assets/](../assets/), [shaders/](../shaders/), [themes/](../themes/), [crates/phantom-plugins/](../crates/phantom-plugins/) with ad-hoc loaders. Crate: new `phantom-resources` OR module inside [phantom-app](../crates/phantom-app/).
- [ ] **Typed event bus with topic registry** [ch14.7]. Replace `serde_json::Value` payloads with typed enum (`Event::BuildFailed { ... }`, `Event::AgentTaskComplete { ... }`, `Event::SessionSwitched { ... }`). Compile-time topic checking. Event decouples shader FX (red flash on build fail), supervisor (offer fix), memory (log incident). Crate: [phantom-protocol](../crates/phantom-protocol/).

**Integration with existing Phase 1:** all 11 items (Tiers 1, 2, 3) ship in Phase 1 as WU-6 through WU-16. See [PHASE1-EXECUTION.md](PHASE1-EXECUTION.md) §8a and §11–13. Nothing is deferred to Phase 1.5; Phase 2 (VideoAdapter, AgentAdapter, MonitorAdapter) builds directly on the completed foundations.

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
