# Phase 1 Execution Plan: AppCoordinator + Terminal Adapter + Engine Foundations

**Date**: 2026-04-24 (expanded from 2026-04-23 draft)
**Status**: COMPLETE — tagged v0.2.0-phase1 on 2026-04-24
**Completed by**: Jeremy Miranda + Claude Opus 4.6
**Depends on**: ARD-002, ARD-003, plan-adapter-integration.md, *Game Engine Architecture* (ch 5, 6, 7, 9)
**Estimated scope**: ~2,400 lines new code, ~400 lines migration

**Scope expansion (2026-04-24)**: After auditing Jason Gregory's *Game Engine Architecture* against the Phantom codebase, seven foundational patterns that every mature engine has and Phantom currently doesn't were folded into Phase 1. These are not Phase 2 polish — they change the shape of `App::tick()`, the coordinator's update model, and the render/input paths. Adding adapters on top of the current single-rate, single-clock, no-channel-logging, no-debug-draw, no-profiler substrate bakes in tech debt that will cost more to rip out later than to do now. See §11–13 for the new work units (WU-6 through WU-12).

---

## 1. Objective

Wire the first real AppAdapter into the running system. Prove that the phantom-adapter framework (trait, registry, bus, lifecycle) works end-to-end with the most critical component: terminal panes.

After Phase 1, the terminal renders identically to today, but through the adapter abstraction. Every subsequent adapter (video, agent, monitor) follows the same pattern.

---

## 2. Architecture Validation

External research validates the core approach. One significant design change required.

### 2.1 What's Validated

| Decision | Pattern | Evidence | Sources |
|----------|---------|----------|---------|
| AppCoordinator | Mediator (GoF) | Apple Cocoa "coordinating controllers," UIKit Coordinator pattern. Right pattern class for 4-8 heterogeneous components. ECS is overkill (Rust forum: "Please don't put ECS into your game engine"). | Apple Cocoa Design Patterns, Wikipedia Mediator pattern |
| Dual-path migration | Strangler Fig | Gold standard. Martin Fowler (2004). Shopify used it at CODE level to extract a 3,000-line God Object. Microsoft Azure + AWS document as first-class pattern. | martinfowler.com/bliki/StranglerFigApplication, Shopify Engineering blog, Azure Architecture Center |
| Two-phase render | Render World / App World | Exactly how Bevy works ("sidestep Rust's borrow checker by ensuring rendering never contends with application logic"). wgpu wiki explicitly recommends `prepare()` → `render()` separation. | Bevy Cheat Book gpu/intro, wgpu Wiki "Encapsulating Graphics Work" |
| "Everything is an app" | Uniform interface | Emacs (everything is a buffer), Plan 9 (everything is a file), Bevy (everything is an entity) all validate. Alan Perlis: "100 functions on 1 data structure > 10 on 10." Caveat: Smalltalk MVC collapsed in non-"everything is an object" languages — our EventBus + frame-drained bus is the correct mitigation. | HN Emacs discussion, stlab.cc MVC history, Bevy discussions |
| EventBus pub/sub | Topic-based messaging | Correct for ~6 topics, ~4-8 subscribers. Known risks: silent failures, hidden coupling, debugging at scale (Uber surge pricing incident). Our bus is synchronous/frame-drained, eliminating ordering issues. | TechYourChance EventBus guide, DEV.to "Hidden Cost of Event-Driven Architecture" |
| Parallel worktree + interface contract | Agent coordination | Industry standard (2025-2026). Claude Code v2.1.50 worktrees, AGENTS.md spec (60K+ repos, Linux Foundation). Sweet spot: 2-4 parallel agents. | Claude Code docs, AGENTS.md spec, Verdent deep dive |

### 2.2 Peer Architecture Comparison

| Project | Pattern | Ownership | Communication | Relevance |
|---------|---------|-----------|---------------|-----------|
| **Bevy** | ECS + Plugins | World owns everything, entity IDs | Typed events (2-frame TTL), Resources, Observers | Render World split validates our two-phase render. Plugin system validates "everything is an app." ECS itself is overkill for our scale. |
| **Zed/GPUI** | Centralized entity store | App owns everything, `Entity<T>` handles with leasing | observe/notify, subscribe/emit (effect queue) | Leasing mechanism is a more elegant solve for F7 borrow conflicts. Effect queue design validates our frame-drained bus. |
| **WezTerm** | Trait objects + Coordinator | Mux owns `Box<dyn Pane>` | MuxNotification callbacks | **Closest peer.** Pane trait ≈ AppAdapter. Mux ≈ AppCoordinator. Enables GUI/headless/CLI modes sharing same core. Study `mux/src/lib.rs` and `mux/src/localpane.rs` before building. |

### 2.3 Required Change: Split the AppAdapter Trait

**Problem**: The current `AppAdapter` has 15 methods. This violates the Interface Segregation Principle. Every piece of evidence points away from a monolithic trait:
- Rust stdlib uses small focused traits: `Read`, `Write`, `Seek`, `BufRead` — not a single `IO` trait
- Xilem's View trait has only 3 methods (`build`, `rebuild`, `event`)
- Tokio's actor pattern uses NO trait — just structs with channel receivers
- Bevy uses zero traits for components — pure data queried by systems
- WezTerm's fat `Pane` trait only works because all panes are terminals. Our adapters are heterogeneous.

**Concrete problems in our code**: `MonitorAdapter` will never meaningfully use `handle_input()`. Headless apps must implement `render()` returning empty output. `permissions()` is a WASM concern but native adapters carry it. `process()` exists only for headless apps.

**Trait split (Phase 1 pre-requisite — WU-0):**

```rust
/// Required by all adapters. The coordinator stores Box<dyn AppCore>.
pub trait AppCore: Send {
    fn app_type(&self) -> &str;
    fn is_alive(&self) -> bool;
    fn update(&mut self, dt: f32);
    fn get_state(&self) -> serde_json::Value;
}

/// Visual adapters that render into a rect.
pub trait Renderable {
    fn render(&self, rect: &Rect) -> RenderOutput;
    fn is_visual(&self) -> bool { true }
    fn spatial_preference(&self) -> Option<SpatialPreference> { None }
}

/// Adapters that accept keyboard input.
pub trait InputHandler {
    fn handle_input(&mut self, key: &str) -> bool;
}

/// Adapters that accept commands from AI or other apps.
pub trait Commandable {
    fn accept_command(&mut self, cmd: &str, args: &serde_json::Value) -> anyhow::Result<String>;
}

/// Adapters that participate in the event bus.
pub trait BusParticipant {
    fn publishes(&self) -> Vec<TopicDeclaration> { vec![] }
    fn subscribes_to(&self) -> Vec<String> { vec![] }
    fn on_message(&mut self, _msg: &BusMessage) {}
}

/// Adapters with lifecycle hooks.
pub trait Lifecycled {
    fn on_init(&mut self) -> anyhow::Result<()> { Ok(()) }
    fn on_state_change(&mut self, _new_state: AppState) {}
}

/// Permission declarations (WASM sandbox boundary).
pub trait Permissioned {
    fn permissions(&self) -> Vec<String> { vec![] }
}

/// Headless processing tick (non-visual adapters only).
pub trait Processable {
    fn process(&mut self) {}
}
```

**Coordinator stores**: `Box<dyn AppCore>`. For optional capabilities, the coordinator checks trait implementations via downcast or capability flags. `TerminalAdapter` implements `AppCore + Renderable + InputHandler + Commandable + BusParticipant + Lifecycled`. `MonitorAdapter` (Phase 2) implements `AppCore + Renderable + BusParticipant` — no `InputHandler`, no `Commandable`.

**Backward compatibility**: Keep the existing `AppAdapter` as a convenience super-trait that blanket-implements when all sub-traits are present. This means existing tests (MockApp) continue to work.

```rust
/// Convenience: implement all sub-traits and get AppAdapter for free.
pub trait AppAdapter: AppCore + Renderable + InputHandler + Commandable
    + BusParticipant + Lifecycled + Permissioned {}

impl<T> AppAdapter for T where T: AppCore + Renderable + InputHandler + Commandable
    + BusParticipant + Lifecycled + Permissioned {}
```

### 2.4 Flagged for Phase 4: Typed Events

Our `serde_json::Value` bus payloads lose type safety. Every framework that scaled (Bevy's `EventWriter<T>`, Zed's `cx.emit(typed_event)`) uses typed events with compile-time checking. For Phase 1, JSON payloads are acceptable. Phase 4 (bus wiring) should migrate to typed event channels. Also add frame number to `BusMessage` for traceability.

---

## 3. Pre-Mortem: How This Fails

### F1: RenderOutput Can't Carry Terminal Grid Data
**Risk**: HIGH
**What happens**: The current `RenderOutput` has `Vec<QuadData>` + `Vec<TextData>`. Terminal rendering needs the full cell grid (char + fg color per cell, positioned on a grid). If we try to decompose the grid into individual `TextData` items, we lose the grid structure the `TextRenderer` needs and pay O(cols*rows) allocations per frame.
**Research**: Current `render.rs` uses `GridRenderData::prepare()` (line ~462) which internally calls `text_renderer.prepare_glyphs()` with `&[TerminalCell]` — a flat array of `{ch, fg}` structs. The renderer converts these to `GlyphInstance` for the GPU. See `crates/phantom-renderer/src/text.rs`. Note: `prepare_glyphs` takes `origin: (f32, f32)` (tuple, not array).
**Mitigation**: Extend `RenderOutput` with a `GridData` variant:
```rust
pub struct GridData {
    pub cells: Vec<TerminalCell>,  // reuse existing type from phantom-renderer
    pub cols: usize,
    pub origin: (f32, f32),          // top-left pixel position
}
```
The adapter fills `GridData`; the render loop calls `text_renderer.prepare_glyphs()` on it. Zero new GPU pipeline code. The adapter doesn't touch wgpu.
**Validation**: Write a unit test that creates a `GridData` with known cells and verifies `prepare_glyphs()` produces the expected `GlyphInstance` count and positions.

### F2: Dual-Path Migration Causes Rendering Drift
**Risk**: MEDIUM
**What happens**: During migration, both the old `render_terminal()` and the new coordinator-driven path exist. If the adapter's render output differs from the old path in any way (cell colors, cursor position, background fills, chrome/borders), the user sees visual glitches when switching between paths.
**Research**: Current `render.rs:render_terminal()` (lines 305-624) does: (1) container background + drop shadow + title strip quads (lines 381-403), (2) container border quads (lines 420-433), (3) grid cells + background quads via `GridRenderData::prepare()` (line 462), (4) cursor quad (lines 477-509), (5) title text (lines 441-443, rendered at 593-600), (6) detach label + tether border (lines 549-591). All of this must be reproduced exactly.
**Mitigation**: Screenshot-based regression. Before starting migration:
1. Capture reference screenshots via MCP (`phantom.screenshot`) for: single pane, split panes, alt-screen detached, cursor at various positions
2. After adapter render path is wired, capture comparison screenshots
3. Pixel-diff with a tolerance threshold (CRT noise means exact match is impossible; use structural similarity)
**Validation**: The review agent compares before/after screenshots and flags drift > 2% SSIM.

### F3: Coordinator Update Loop Adds Frame Latency
**Risk**: MEDIUM
**What happens**: The current update loop reads PTY directly. With the coordinator, the flow becomes: `coordinator.update_all()` → adapter's `update()` reads PTY → adapter stores output → coordinator drains bus → next frame renders. If any step is slow or if bus draining introduces a frame delay, typing latency increases.
**Research**: Current update loop in `update.rs:~36-42` calls `pane.terminal.pty_read()` which is already non-blocking — it returns `Ok(0)` on `WouldBlock` (EAGAIN), no timeout, no sleep. See `crates/phantom-terminal/src/terminal.rs:233`. The adapter's `update()` must preserve this non-blocking behavior.
**Mitigation**: The adapter's `update(dt)` calls `self.terminal.pty_read()` directly — same non-blocking call, just wrapped. Bus drain happens after all adapters update, within the same frame. No extra frame delay.
**Validation**: Instrument frame time before and after. Assert < 1ms regression at P99.

### F4: Pane Split/Close Through Coordinator Breaks Layout
**Risk**: MEDIUM
**What happens**: Current split uses `layout.split_horizontal(pane_id)` which returns two new `PaneId`s and rearranges the Taffy tree. The coordinator needs to map `AppId ↔ PaneId` and keep both in sync during splits. If the mapping gets stale, input goes to the wrong pane or rendering misaligns.
**Research**: See `crates/phantom-app/src/pane.rs:110` (`split_focused_pane()`) and `crates/phantom-ui/src/layout.rs` (`split_horizontal()` → internal `split()`). The original pane becomes a column container holding two child panes. Returns `(existing_child, new_child)`. The old PaneId is invalidated.
**Mitigation**: On split, coordinator must: (1) remove old AppId→PaneId mapping, (2) create two new TerminalAdapters, (3) register both, (4) map new AppIds to new PaneIds. The old adapter is destroyed. This is a clean break, not a partial update.
**Validation**: Unit test: register adapter → split → verify old adapter is Dead, two new adapters are Running, layout has correct node count.

### F5: EventBus Queue Overflow Loses Terminal Output Events
**Risk**: LOW (but known)
**What happens**: EventBus caps at 256 messages. High-frequency PTY output (e.g., `cat /dev/urandom | xxd`) could overflow the queue, dropping events. The brain misses `CommandComplete` signals.
**Research**: ARD-004 already identified this. See `crates/phantom-adapter/src/bus.rs:58` (`MAX_QUEUE_SIZE = 256`, enforced at line 129) — oldest messages dropped when queue is full. Current code in `update.rs:69` already throttles: only emits if `event_bus.queue_len() < 128`.
**Mitigation**: Keep existing throttle. The adapter's `update()` checks `bus.queue_len() < 128` before emitting. For Phase 1, this is acceptable. Phase 4 (bus wiring) will add per-topic ring buffers.
**Validation**: Stress test: run `yes` in terminal for 5 seconds, verify no panic and brain still receives final `CommandComplete`.

### F6: AppAdapter Must Be Send — Terminal Has Arc<Mutex<Vec<u8>>>
**Risk**: HIGH
**What happens**: `AppAdapter: Send` is required. `PhantomTerminal` internally uses `Arc<Mutex<Vec<u8>>>` for the PTY write queue (see ARD-004 finding C3). This is `Send`, but if any field in the adapter is not `Send`, compilation fails.
**Research**: `PhantomTerminal` struct in `crates/phantom-terminal/src/terminal.rs:125`. PTY write queue is `Arc<Mutex<Vec<Vec<u8>>>>` (type alias `PtyWriteQueue` at line 38). All fields are owned types or `Arc<Mutex<_>>`. `alacritty_terminal::Term` may have non-Send internals — must verify.
**Mitigation**: Write a compile-time check:
```rust
fn _assert_send<T: Send>() {}
fn _check() { _assert_send::<TerminalAdapter>(); }
```
If `Term` is not `Send`, the adapter must own the terminal on the main thread and proxy through channels. This would be a significant redesign — identify early.
**Validation**: The compile-time check either passes or fails immediately. If it fails, escalate before writing more code.

### F7: Borrow Conflicts on App During Coordinator Operations
**Risk**: HIGH
**What happens**: `App` currently owns both the coordinator and the rendering resources (GPU, atlas, text_renderer). When the coordinator calls `adapter.render()`, the adapter produces `RenderOutput`. Then `App` needs to convert that to GPU calls using `&mut self.text_renderer` etc. But if the coordinator borrows `&self.coordinator` while also borrowing `&mut self.text_renderer`, Rust's borrow checker blocks it.
**Research**: This is the classic "self-referential borrow" problem in Rust. The current code avoids it because `render_terminal()` directly accesses `self.panes` and `self.text_renderer` in the same scope.
**Mitigation**: Two-phase render:
1. Phase A: `let outputs: Vec<(AppId, Rect, RenderOutput)> = self.coordinator.render_all(&self.layout);` — borrows coordinator immutably, collects outputs into an owned Vec.
2. Phase B: iterate `outputs`, use `&mut self.text_renderer`, `&mut self.grid_renderer` etc. to convert to GPU calls. Coordinator borrow is released.
**Validation**: The code compiles. If it doesn't compile with this pattern, the borrow conflict is real and we need to restructure ownership (e.g., move renderers into a separate `RenderContext` struct passed by `&mut`).

---

## 4. Research References

### Existing Codebase (read before writing any code)

| What | File | Why |
|------|------|-----|
| AppAdapter trait | `crates/phantom-adapter/src/adapter.rs` | The contract every adapter implements |
| EventBus | `crates/phantom-adapter/src/bus.rs` | Pub/sub; 256 message cap; drain_for() pattern |
| AppRegistry | `crates/phantom-adapter/src/registry.rs` | Lifecycle state machine; parallel vecs; gc() |
| AppLifecycle | `crates/phantom-adapter/src/lifecycle.rs` | Valid state transitions |
| SpatialPreference | `crates/phantom-adapter/src/spatial.rs` | Layout hints (min/preferred/max, priority) |
| RenderOutput | `crates/phantom-adapter/src/adapter.rs:~L20-40` | Current: QuadData + TextData only |
| App struct | `crates/phantom-app/src/app.rs` | 30+ fields; owns everything |
| Update loop | `crates/phantom-app/src/update.rs` | PTY read, semantic scan, brain events, MCP drain |
| Render loop | `crates/phantom-app/src/render.rs` | 3-pass: scene→postfx→overlay |
| Input routing | `crates/phantom-app/src/input.rs` | Keybind dispatch, terminal ANSI encoding |
| Pane struct | `crates/phantom-app/src/pane.rs` | Owns PhantomTerminal + layout/scene IDs |
| Terminal render | `crates/phantom-app/src/render.rs:~298-520` | Background fills + grid cells + cursor + chrome |
| Layout engine | `crates/phantom-ui/src/layout.rs` | Taffy flexbox; split_horizontal/vertical |
| TextRenderer | `crates/phantom-renderer/src/text.rs` | prepare_glyphs() takes &[TerminalCell] |
| TerminalCell | `crates/phantom-renderer/src/text.rs` | {ch: char, fg: [f32;4]} |
| MCP dispatch | `crates/phantom-mcp/src/listener.rs` | AppCommand enum; blocking reply channels |

### Design Docs

| Doc | Key Insight |
|-----|-------------|
| ARD-002 | "Everything is an app" — AppAdapter is the universal interface |
| ARD-003 | Lifecycle states, pub/sub bus, spatial negotiation design |
| ARD-004 | Known unwrap() crashes, memory leaks, concurrency risks — don't introduce more |
| research/spatial-negotiation.md | Wayland two-phase, Cassowary constraints, priority-based resolution |

### Known Bugs to Not Re-Introduce

From ARD-004 and commit history:

| Bug | Source | Status | Rule |
|-----|--------|--------|------|
| `assertion failed: self.is_char_boundary(end)` | Multi-byte UTF-8 in output_buf drain | FIXED (commit b1e94d0) | Never index into String by byte offset without checking char boundary |
| `catch_unwind(AssertUnwindSafe(...))` | Papering over panics instead of fixing root cause | FIXED (commit 98f30fb) | Fix the bug, don't catch the panic |
| `.unwrap()` in production paths | Was 8 instances in headless.rs | FIXED (0 remain in headless.rs) | Use `?`, `let-else`, or `if let` |
| `format!()` in render hot path | render.rs | OPEN | Use `write!()` or pre-allocated buffers |
| Fire-and-forget thread spawns | Multiple files with discarded JoinHandle | OPEN | Store handles; propagate panics |
| Blocking `.recv()` without timeout | 3 remain: brain.rs:146, lib.rs:629, lib.rs:722 | PARTIALLY FIXED (was 13, now 3) | Always use `recv_timeout()` |

---

## 5. Agent Coordination: Work Decomposition

### Work Units

Phase 1 decomposes into **3 pre-requisites**, **10 independent work units** that can be built in parallel, **1 integration unit**, and **1 test unit**. Engine foundations (WU-6 through WU-16) were folded in from the *Game Engine Architecture* audit — see §11 for context. All three tiers ship in Phase 1; nothing is deferred to Phase 1.5.

```
Wave 0 — Pre-requisites (gate Wave 1):
  WU-0:  Trait Split                    (phantom-adapter/src/*.rs)
  WU-6:  Clock + dt clamp               (phantom-time or phantom-scene; App::tick)
  WU-15: Typed event bus                (phantom-protocol; replaces serde_json::Value payloads)
         All three must merge before Wave 1.
         WU-0, WU-6, WU-15 can themselves run in parallel (no file overlap).
         Gates downstream: WU-0 gates WU-1/2/3; WU-6 gates WU-1; WU-15 gates WU-1/2.

Wave 1 — Parallel build (no file overlap across all 10):
  WU-1:  AppCoordinator                 (coordinator.rs — new; uses Clock + Cadence + typed bus)
  WU-2:  TerminalAdapter                (adapters/terminal.rs — new)
  WU-3:  RenderOutput Extension         (phantom-adapter/src/adapter.rs)
  WU-7:  Explicit start_up/shut_down    (phantom-app/src/boot.rs — new)
  WU-8:  Job queue + worker pool        (new phantom-jobs crate or phantom-supervisor module)
  WU-9:  DebugDrawManager               (phantom-renderer/src/debug_draw.rs — new)
  WU-10: Console evaluator wiring       (phantom-app console command handler)
  WU-11: Channel logging + file mirror  (phantom-app/src/logging.rs — new)
  WU-12: Tracy profiler integration     (phantom-app + macro crate)
  WU-14: Unified ResourceManager        (new phantom-resources crate or phantom-app module)

Wave 2 — Integration (sequential, depends on all of Wave 1):
  WU-5:  Integration Wiring             (app.rs, update.rs, render.rs, input.rs, commands.rs, lib.rs)
         Also: retrofit agent system to use WU-8 job queue;
               retrofit brain + MCP + semantic + NLP to async result pattern (WU-13 = async)

Wave 3 — Tests (sequential, depends on WU-5):
  WU-4:  Unit + integration + regression + stress tests (covers WU-0 through WU-16)
```

**Note on WU-13**: "Async result pattern" is not a separate WU but a conversion pattern applied during WU-5 (integration). Semantic parse, NLP interpret, brain decision, memory query, and MCP calls all become "request frame N, pick up frame N+1" via WU-8's job queue. Tracked inside WU-5's checklist.

### File Claims (Conflict Prevention)

Each work unit claims exclusive write access to specific files. No two agents touch the same file.

| Work Unit | Creates | Modifies | Claims |
|-----------|---------|----------|--------|
| WU-0 | — | `phantom-adapter/src/adapter.rs`, `phantom-adapter/src/lib.rs` | EXCLUSIVE: phantom-adapter/src/ (during WU-0 only) |
| WU-1 | `coordinator.rs` | — | EXCLUSIVE: coordinator.rs |
| WU-2 | `adapters/mod.rs`, `adapters/terminal.rs` | — | EXCLUSIVE: adapters/ |
| WU-3 | — | `phantom-adapter/src/adapter.rs` | EXCLUSIVE: phantom-adapter/src/adapter.rs (after WU-0 merges) |
| WU-4 | test files | — | EXCLUSIVE: test files only |
| WU-5 | — | `app.rs`, `update.rs`, `render.rs`, `input.rs`, `lib.rs`, `commands.rs` | EXCLUSIVE: phantom-app/src/{app,update,render,input,commands,lib}.rs |
| WU-6 | `phantom-time/` (new crate) OR `phantom-scene/src/clock.rs` | `phantom-app/src/app.rs` (dt clamp only) | EXCLUSIVE: phantom-time/ or phantom-scene/src/clock.rs; READ-MODIFY-WRITE: app.rs tick region only |
| WU-7 | `phantom-app/src/boot.rs` | `phantom-app/src/app.rs` (New::new body only) | EXCLUSIVE: boot.rs |
| WU-8 | `phantom-jobs/` (new crate) OR `phantom-supervisor/src/jobs.rs` | — | EXCLUSIVE: phantom-jobs/ or phantom-supervisor/src/jobs.rs |
| WU-9 | `phantom-renderer/src/debug_draw.rs`, `phantom-renderer/src/debug_draw/*.rs` | `phantom-renderer/src/lib.rs` (re-export) | EXCLUSIVE: debug_draw module |
| WU-10 | — | console command handler in phantom-app | EXCLUSIVE: console eval path only |
| WU-11 | `phantom-app/src/logging.rs` | `phantom-app/src/main.rs` or entry-point init | EXCLUSIVE: logging.rs |
| WU-12 | `phantom-app/src/profiler.rs` OR macro crate | `Cargo.toml` workspace deps | EXCLUSIVE: profiler.rs + workspace tracy dep |
| WU-14 | `phantom-resources/` (new crate) OR `phantom-app/src/resources.rs` | — | EXCLUSIVE: phantom-resources/ or phantom-app/src/resources.rs |
| WU-15 | `phantom-protocol/src/events.rs` (new or expanded) | `phantom-adapter/src/bus.rs` (payload type) | EXCLUSIVE: phantom-protocol/src/events.rs; READ-MODIFY-WRITE: bus.rs payload field only |

### Dependency Graph

```
Wave 0 (pre-requisites — all three can run in parallel; all gate Wave 1):
  WU-0  (Trait Split)       ─► gates WU-1, WU-2, WU-3
  WU-6  (Clock + dt clamp)  ─► gates WU-1
  WU-15 (Typed event bus)   ─► gates WU-1, WU-2

Wave 1 (10 parallel work units — zero file overlap):
  ┌───────┬───────┬───────┬───────┬──────┬──────┬──────┬──────┬──────┬──────┐
  ▼       ▼       ▼       ▼       ▼      ▼      ▼      ▼      ▼      ▼
 WU-1   WU-2    WU-3    WU-7    WU-8  WU-9   WU-10  WU-11  WU-12  WU-14
 Coord  Term    Render  Boot    Jobs  Debug  Consl  Log    Prof   Resrc
                                      Draw

Wave 2 (sequential, depends on ALL of Wave 1):
                        WU-5 (Integration)
                        - wire coordinator through app.rs/update.rs/render.rs/input.rs
                        - retrofit agents to WU-8 job queue
                        - convert semantic/NLP/brain/MCP to async result pattern (WU-13)
                        - wire shaders/fonts/videos/themes/plugins through WU-14
                        - wire all bus payloads through WU-15 typed events
                                │
Wave 3 (sequential):            ▼
                        WU-4 (Tests — full matrix: WU-0 through WU-16)
```

### Parallelism

- **Wave 0** (3 parallel agents): WU-0, WU-6, WU-15 — all must merge before Wave 1. No file overlap between them, so they can run concurrently.
- **Wave 1** (10 parallel agents across 3 batches — recommended per ARD-003 finding that 2–4 concurrent agents is the sweet spot):
  - **Batch A** (adapter framework): WU-1, WU-2, WU-3, WU-7
  - **Batch B** (engine foundations I): WU-8, WU-9, WU-14
  - **Batch C** (engine foundations II): WU-10, WU-11, WU-12
- **Wave 2** (sequential): WU-5 — integration touches app.rs, update.rs, render.rs, input.rs, commands.rs, lib.rs. Single agent. Also absorbs WU-13 (async result pattern — not a standalone WU, converts existing blocking calls to jobs).
- **Wave 3** (sequential): WU-4 — depends on WU-5 for integration test surface.

### Interface Contract (shared agreement before Wave 1 starts)

All agents in Wave 1 must agree on these types before starting. WU-0 (trait split) must be merged first — these signatures reference the split traits.

```rust
// --- Trait hierarchy (WU-0 delivers, all others depend on) ---

// Core trait — required by all adapters
pub trait AppCore: Send {
    fn app_type(&self) -> &str;
    fn is_alive(&self) -> bool;
    fn update(&mut self, dt: f32);
    fn get_state(&self) -> serde_json::Value;
}

// Optional capability traits
pub trait Renderable        { fn render(&self, rect: &Rect) -> RenderOutput; ... }
pub trait InputHandler      { fn handle_input(&mut self, key: &str) -> bool; }
pub trait Commandable       { fn accept_command(&mut self, cmd: &str, args: &Value) -> Result<String>; }
pub trait BusParticipant    { fn publishes(&self) -> Vec<TopicDeclaration>; ... }
pub trait Lifecycled        { fn on_init(&mut self) -> Result<()>; ... }

// Convenience super-trait (backward compat)
pub trait AppAdapter: AppCore + Renderable + InputHandler + Commandable
    + BusParticipant + Lifecycled + Permissioned {}

// --- RenderOutput (WU-3 implements, WU-2 produces, WU-5 consumes) ---

pub struct RenderOutput {
    pub quads: Vec<QuadData>,
    pub text_segments: Vec<TextData>,
    pub grid: Option<GridData>,  // NEW — terminal grid cells
}

pub struct GridData {
    pub cells: Vec<TerminalCell>,
    pub cols: usize,
    pub origin: (f32, f32),
    pub cursor: Option<CursorData>,
}

pub struct CursorData {
    pub col: usize,
    pub row: usize,
    pub shape: CursorShape,
    pub visible: bool,
}

// --- AppCoordinator public API (WU-1 implements, WU-5 calls) ---
// Stores Box<dyn AppCore>. Checks for optional traits via downcast.

impl AppCoordinator {
    pub fn new(bus: EventBus) -> Self;
    pub fn register_adapter(
        &mut self,
        adapter: Box<dyn AppAdapter>,  // full adapter for Phase 1; loosen to AppCore in Phase 2
        layout: &mut LayoutEngine,
        scene: &mut SceneTree,
        content_node: NodeId,
    ) -> AppId;
    pub fn remove_adapter(
        &mut self,
        app_id: AppId,
        layout: &mut LayoutEngine,
        scene: &mut SceneTree,
    );
    pub fn update_all(&mut self, dt: f32);
    pub fn render_all(
        &self,
        layout: &LayoutEngine,
    ) -> Vec<(AppId, Rect, RenderOutput)>;
    pub fn route_input(&mut self, key: &str) -> bool;
    pub fn set_focus(&mut self, app_id: AppId);
    pub fn focused(&self) -> Option<AppId>;
    pub fn get_state(&self, app_id: AppId) -> Option<serde_json::Value>;
    pub fn send_command(
        &mut self,
        app_id: AppId,
        cmd: &str,
        args: &serde_json::Value,
    ) -> anyhow::Result<String>;
    pub fn split_adapter(
        &mut self,
        app_id: AppId,
        horizontal: bool,
        layout: &mut LayoutEngine,
        scene: &mut SceneTree,
        content_node: NodeId,
        terminal_factory: impl FnOnce(u16, u16) -> PhantomTerminal,
    ) -> anyhow::Result<(AppId, AppId)>;
    pub fn adapter_count(&self) -> usize;
    pub fn all_app_ids(&self) -> Vec<AppId>;
    pub fn pane_id_for(&self, app_id: AppId) -> Option<PaneId>;
}

// --- TerminalAdapter (WU-2 implements) ---
// Implements: AppCore + Renderable + InputHandler + Commandable + BusParticipant + Lifecycled
// Constructor + accessor methods:

impl TerminalAdapter {
    pub fn new(terminal: PhantomTerminal) -> Self;
    pub fn terminal(&self) -> &PhantomTerminal;
    pub fn terminal_mut(&mut self) -> &mut PhantomTerminal;
    pub fn output_buf(&self) -> &str;
    pub fn has_new_output(&self) -> bool;
    pub fn clear_new_output_flag(&mut self);
    pub fn is_detached(&self) -> bool;
    pub fn detached_label(&self) -> &str;
    pub fn error_notified(&self) -> bool;
    pub fn set_error_notified(&mut self, val: bool);
}
```

---

## 6. Agent Pipeline: Build → Review → Merge

### Worktree Spawn Convention

All agent worktrees MUST branch from the most recent clean baseline tag, not from HEAD of main.

```bash
# Correct — branch from baseline tag
git checkout $(git describe --tags --match 'v*.baseline' --abbrev=0 2>/dev/null || git rev-parse --short origin/main) -b <branch-name>

# Wrong — never do this
git checkout main -b <branch-name>
```

The baseline tag marks a commit where `cargo build --workspace` and `cargo test --workspace --no-run` passed cleanly. Branching from it prevents agents from inheriting mid-stream work-in-progress from main that hasn't been gated. Current baseline: `v0.2.0-phase1.baseline`.

### Stage 0: Trait Split (Wave 0)

**Agent Z — Trait Split** (WU-0)
- Branch: `phase1/trait-split`
- Modifies: `crates/phantom-adapter/src/adapter.rs`, `crates/phantom-adapter/src/lib.rs`
- What:
  1. Split `AppAdapter` into 7 focused traits (AppCore, Renderable, InputHandler, Commandable, BusParticipant, Lifecycled, Permissioned)
  2. Add blanket `AppAdapter` super-trait for backward compatibility
  3. Update `MockApp` in tests to implement all sub-traits
  4. Verify all 47 existing phantom-adapter tests still pass
  5. Verify `cargo check --workspace` passes (phantom-app uses AppAdapter — must still compile)
- **Gate**: Must merge to main before Wave 1 starts
- Output: PR to main, reviewed and merged

### Stage 1: Interface Lock

**Who**: Human (Jeremy) or lead agent
**What**: Review the interface contract above. Approve or modify.
**Gate**: Contract is frozen. No changes after this point without re-review.

### Stage 2: Parallel Build (Wave 1)

Three agents work simultaneously in isolated worktrees:

**Agent A — Coordinator** (WU-1)
- Branch: `phase1/coordinator`
- Creates: `crates/phantom-app/src/coordinator.rs`
- Implements: `AppCoordinator` struct per the interface contract
- Constraints: Must not import anything from `phantom-renderer` (no GPU types)
- Output: PR to main

**Agent B — TerminalAdapter** (WU-2)
- Branch: `phase1/terminal-adapter`
- Creates: `crates/phantom-app/src/adapters/mod.rs`, `crates/phantom-app/src/adapters/terminal.rs`
- Implements: `TerminalAdapter` wrapping `PhantomTerminal`
- Constraints: Must produce `GridData` in `render()`, must not touch wgpu
- Output: PR to main

**Agent C — RenderOutput Extension** (WU-3)
- Branch: `phase1/render-output`
- Modifies: `crates/phantom-adapter/src/adapter.rs`
- Adds: `GridData`, `CursorData`, `CursorShape` to `RenderOutput`
- Constraints: Must preserve existing `QuadData`/`TextData` API; no breaking changes
- Output: PR to main

### Stage 3: Code Review (per PR)

Each PR from Stage 2 gets reviewed by a **review agent** before merge:

**Review checklist**:
- [ ] Implements the interface contract exactly (no surprise API changes)
- [ ] No `unwrap()` / `expect()` / `panic!()` in non-test code (ARD-004)
- [ ] No `format!()` in paths that could be called per-frame (ARD-004 M1)
- [ ] No fire-and-forget thread spawns (ARD-004 C1)
- [ ] All public functions have `#[must_use]` where appropriate
- [ ] `let-else` for early returns (rust-style skill)
- [ ] For-loops over iterators where clearer (rust-style skill)
- [ ] Send bound verified (compile-time assert for TerminalAdapter)
- [ ] Tests pass: `cargo test -p phantom-adapter -p phantom-app`
- [ ] No warnings: `cargo check --workspace` with `deny(warnings)`
- [ ] Diff is minimal — no drive-by refactors, no unrelated changes

**Review verdict**: APPROVE / REQUEST CHANGES / REJECT
- APPROVE: merge to main
- REQUEST CHANGES: specific line comments, agent fixes and re-submits
- REJECT: fundamental design flaw, escalate to human

### Stage 4: Integration Build (Wave 2)

**Agent D — Integration** (WU-5)
- Branch: `phase1/integration`
- Base: main (after WU-1, WU-2, WU-3 merged)
- Modifies: `app.rs`, `update.rs`, `render.rs`, `input.rs`, `commands.rs`, `lib.rs`
- What:
  1. Add `coordinator: AppCoordinator` to `App` struct
  2. On startup, wrap initial terminal in `TerminalAdapter`, register with coordinator
  3. Replace pane update loop with `coordinator.update_all(dt)` + semantic scan on adapter output
  4. Replace `render_terminal()` with two-phase render (collect `RenderOutput`, then GPU convert)
  5. Replace direct PTY write in input.rs with `coordinator.route_input()`
  6. Replace `split_focused_pane()` with `coordinator.split_adapter()`
  7. Keep old `panes: Vec<Pane>` field temporarily as `_legacy_panes` (empty, compiles, removed in follow-up)
- Output: PR to main

### Stage 5: Integration Review

Same review agent, same checklist, plus:
- [ ] Terminal renders identically (screenshot regression)
- [ ] PTY I/O works (MCP `phantom.send_key` + `phantom.read_output`)
- [ ] Split/close works (MCP `phantom.split_pane`)
- [ ] Brain still receives `AiEvent::CommandComplete`
- [ ] Sysmon/agent panes still work (not broken by coordinator)
- [ ] Console commands still work
- [ ] Boot sequence unaffected

### Stage 6: Integration Tests (Wave 3)

**Agent E — Tests** (WU-4)
- Branch: `phase1/tests`
- Creates: test modules in `phantom-adapter` and `phantom-app`
- See Section 7 for full test plan
- Output: PR to main

### Stage 7: Final Review + Tag

Human reviews the complete Phase 1 diff (all PRs merged to main). If satisfactory:
- Tag: `v0.2.0-phase1`
- Update `docs/PLAN.md` — mark AppAdapter tasks as complete
- Update `docs/HANDOFF.md` — record what was built and what's next

---

## 7. Testing Plan

### 7.0 Unit Tests — Trait Split (WU-0)

All 47 existing tests in `phantom-adapter` must pass unchanged. Additionally:

```
test_app_core_is_object_safe
test_renderable_is_object_safe
test_input_handler_is_object_safe
test_commandable_is_object_safe
test_bus_participant_is_object_safe
test_blanket_app_adapter_impl
test_mock_implements_all_sub_traits
```

Location: `crates/phantom-adapter/src/lib.rs` (extend existing `#[cfg(test)]` module)

### 7.1 Unit Tests — AppCoordinator

```
test_register_adapter_assigns_unique_id
test_register_adapter_transitions_to_running
test_remove_adapter_transitions_to_dead
test_update_all_calls_adapter_update
test_render_all_returns_outputs_for_visual_adapters
test_route_input_to_focused_adapter
test_set_focus_changes_target
test_split_creates_two_new_adapters
test_split_removes_original_adapter
test_adapter_count_reflects_registrations
test_get_state_returns_adapter_state_json
test_send_command_proxies_to_adapter
```

Location: `crates/phantom-app/src/coordinator.rs` (inline `#[cfg(test)]` module)

### 7.2 Unit Tests — TerminalAdapter

```
test_app_type_returns_terminal
test_is_visual_returns_true
test_spatial_preference_has_sane_defaults
test_render_produces_grid_data
test_render_grid_has_correct_cols
test_handle_input_returns_true_for_printable
test_get_state_includes_cursor_position
test_accept_command_write_sends_to_pty
test_accept_command_unknown_returns_error
test_is_alive_true_when_pty_connected
test_publishes_terminal_output_topic
test_subscribes_to_nothing
test_output_buf_accumulates_pty_data
test_has_new_output_flag_lifecycle
test_error_notified_flag
test_send_assert — compile-time Send check
```

Location: `crates/phantom-app/src/adapters/terminal.rs` (inline `#[cfg(test)]` module)

### 7.3 Unit Tests — RenderOutput Extension

```
test_render_output_default_has_no_grid
test_grid_data_with_cells
test_cursor_data_fields
test_existing_quad_text_api_unchanged
```

Location: `crates/phantom-adapter/src/adapter.rs` (inline `#[cfg(test)]` module)

### 7.4 Integration Tests — Full Pipeline

```
test_app_startup_registers_terminal_adapter
test_terminal_adapter_renders_through_coordinator
test_split_creates_two_terminal_adapters
test_close_pane_removes_adapter_and_rebalances
test_input_routes_to_focused_terminal
test_semantic_scan_fires_through_adapter
test_brain_receives_events_from_adapter_bus
test_mcp_screenshot_works_with_adapter_render_path
test_mcp_send_key_works_through_coordinator
test_mcp_read_output_works_through_adapter
```

Location: `crates/phantom-app/tests/integration_coordinator.rs` (new file)

### 7.5 Regression Tests — Screenshot Comparison

Before migration begins, capture reference screenshots:
1. Single terminal pane, idle (cursor blinking)
2. Single terminal pane, with colored output (`ls --color`)
3. Two horizontal split panes
4. Two vertical split panes
5. Terminal in alt-screen (vim open)
6. Terminal with detach label

After migration, recapture and compare. Acceptable SSIM threshold: 0.95 (CRT noise varies frame-to-frame).

### 7.6 Stress Tests

```
test_rapid_split_close_cycle_50x — no crash, no leak
test_high_frequency_pty_output — bus doesn't overflow dangerously
test_rapid_focus_switching — input always routes correctly
```

### 7.7 Compile-Time Checks

```
assert_send::<TerminalAdapter>()
assert_send::<AppCoordinator>()
```

### 7.8 Quality Gates

Every PR must pass ALL of the following before merge:

| Gate | Command | Pass criteria |
|------|---------|---------------|
| Compile | `cargo check --workspace` | 0 errors, 0 warnings |
| Clippy | `cargo clippy --workspace` | 0 warnings (deny level) |
| Tests | `cargo test --workspace` | 0 failures |
| New tests | `cargo test -p phantom-app -p phantom-adapter` | All new tests pass |
| No unwrap | `rg '\.unwrap\(\)' crates/phantom-app/src/coordinator.rs crates/phantom-app/src/adapters/` | 0 matches in non-test code |

---

## 8. Detailed Work Unit Specs

### WU-0: Trait Split (~100 lines of changes)

**File**: `crates/phantom-adapter/src/adapter.rs`, `crates/phantom-adapter/src/lib.rs`

Split the monolithic 15-method `AppAdapter` into focused traits per Section 2.3. Key constraints:

- **Backward compatible**: The `AppAdapter` super-trait auto-implements via blanket impl. Existing code that uses `Box<dyn AppAdapter>` or `impl AppAdapter` continues to compile.
- **MockApp updates**: The test mock in `lib.rs` must implement each sub-trait explicitly. All 47 existing tests must pass unchanged.
- **Re-exports**: `lib.rs` must re-export all new trait names at crate root.
- **No new dependencies**: The split is purely a refactor of `adapter.rs`. No new crate dependencies.
- **`phantom-app` must still compile**: `phantom-app` imports `AppAdapter` — verify `cargo check -p phantom-app` passes.

**Verification**:
```bash
cargo test -p phantom-adapter   # all 47 tests pass
cargo check --workspace          # 0 errors, 0 warnings
```

### WU-1: AppCoordinator (~200 lines)

**File**: `crates/phantom-app/src/coordinator.rs`

```rust
pub struct AppCoordinator {
    registry: AppRegistry,
    bus: EventBus,
    pane_map: HashMap<PaneId, AppId>,
    app_pane_map: HashMap<AppId, PaneId>,
    scene_map: HashMap<AppId, NodeId>,
    focused: Option<AppId>,
}
```

**Key implementation notes**:
- `register_adapter()` takes `&mut LayoutEngine` and `&mut SceneTree` by reference — it doesn't own them. This avoids the borrow conflict from F7.
- `render_all()` takes `&LayoutEngine` (immutable) and returns owned `Vec<(AppId, Rect, RenderOutput)>`. The caller then uses GPU resources to convert.
- `update_all()` iterates registry, calls `adapter.update(dt)`, then drains bus messages and delivers via `on_message()`.
- `split_adapter()` takes a `terminal_factory` closure so it can create new terminals with correct dimensions without owning the terminal creation logic.
- All methods that can fail return `anyhow::Result`, not `unwrap()`.

### WU-2: TerminalAdapter (~300 lines)

**File**: `crates/phantom-app/src/adapters/terminal.rs`

```rust
pub struct TerminalAdapter {
    terminal: PhantomTerminal,
    output_buf: String,
    has_new_output: bool,
    error_notified: bool,
    is_detached: bool,
    detached_label: String,
    was_alt_screen: bool,
}
```

**Key implementation notes**:
- `render()` reads the terminal grid via `self.terminal.term()`, extracts cells into `Vec<TerminalCell>`, returns `RenderOutput` with `GridData`. Also produces background quad + cursor quad + chrome quads in the `quads` vec.
- `update(dt)` calls `self.terminal.pty_read()` — same non-blocking call as current code (returns `Ok(0)` on WouldBlock). Accumulates into `output_buf` (capped at 8192 bytes). Detects alt-screen, sets flags.
- `handle_input(key)` encodes key string to ANSI bytes and writes to PTY. Returns `true` always (terminal consumes all input when focused).
- `get_state()` returns JSON: `{ "type": "terminal", "cursor": [col, row], "last_output": "...", "is_detached": bool, "detached_label": "..." }`.
- `accept_command("write", {"text": "..."})` writes to PTY. `accept_command("resize", {"cols": N, "rows": N})` resizes terminal.
- `is_alive()` checks PTY connection status.

### WU-3: RenderOutput Extension (~50 lines)

**File**: `crates/phantom-adapter/src/adapter.rs`

Add to existing `RenderOutput`:
```rust
pub struct RenderOutput {
    pub quads: Vec<QuadData>,
    pub text_segments: Vec<TextData>,
    pub grid: Option<GridData>,       // NEW
}

pub struct GridData {
    pub cells: Vec<TerminalCell>,     // reuse from phantom-renderer
    pub cols: usize,
    pub origin: (f32, f32),
    pub cursor: Option<CursorData>,
}

pub struct CursorData {
    pub col: usize,
    pub row: usize,
    pub shape: CursorShape,
    pub visible: bool,
}

pub enum CursorShape {
    Block,
    Underline,
    Bar,
}
```

**Dependency note**: `phantom-adapter` will need `phantom-renderer` as a dependency for `TerminalCell`. If this creates a circular dependency, define `TerminalCell` in `phantom-adapter` instead and have `phantom-renderer` re-export it. Check the dependency graph before implementing.

### WU-5: Integration Wiring (~200 lines of changes)

**Files modified**: `app.rs`, `update.rs`, `render.rs`, `input.rs`, `commands.rs`, `lib.rs`

**app.rs changes**:
- Add field: `pub(crate) coordinator: AppCoordinator`
- In `App::new()`: create coordinator, wrap initial terminal in TerminalAdapter, register
- Remove: direct `panes` usage (keep field as `_legacy_panes: Vec<Pane>` temporarily)

**update.rs changes**:
- Replace PTY read loop with `self.coordinator.update_all(dt)`
- After update_all, iterate adapters for semantic scan (check `has_new_output()`, run scanner, send brain events)
- Replace pane exit detection with registry GC (dead adapters)
- MCP command handling: route through coordinator instead of direct pane access

**render.rs changes**:
- Replace `render_terminal()` with:
  ```rust
  let outputs = self.coordinator.render_all(&self.layout);
  for (app_id, rect, output) in outputs {
      // Render background quads
      for quad in &output.quads {
          self.pool_quads.push(quad.into());
      }
      // Render grid cells (if terminal)
      if let Some(grid) = &output.grid {
          let glyphs = self.text_renderer.prepare_glyphs(
              &mut self.atlas, &self.gpu.queue,
              &grid.cells, grid.cols, grid.origin,
          );
          self.pool_glyphs.extend(glyphs);
      }
      // Render text segments (for future non-terminal adapters)
      for text in &output.text_segments {
          // convert to glyphs
      }
  }
  ```

**input.rs changes**:
- Replace `self.panes[self.focused_pane].terminal.pty_write(bytes)` with `self.coordinator.route_input(key_str)`
- Keep keybind dispatch (split, close, focus next/prev) but route through coordinator

**commands.rs changes**:
- `split` command calls `self.coordinator.split_adapter(...)` instead of `split_focused_pane()`
- `close` command calls `self.coordinator.remove_adapter(...)` instead of `close_focused_pane()`

---

## 8a. Engine Foundation Work Units (WU-6 through WU-15)

These were added 2026-04-24 based on the *Game Engine Architecture* audit. Each is self-contained, tested in isolation, and integrated via WU-5. See §11 for why they're here (not Phase 2 or Phase 1.5).

All eleven patterns from the audit ship in Phase 1:
- **Foundational** (Tier 1): WU-6 (Clock + dt clamp + Cadence), WU-7 (start_up / shut_down)
- **User-visible** (Tier 2): WU-9 (DebugDraw), WU-10 (console evaluator), WU-11 (channel logging), WU-12 (Tracy profiler)
- **Architectural** (Tier 3): WU-8 (job queue), WU-14 (ResourceManager), WU-15 (typed event bus), WU-13 (async results, absorbed into WU-5)

Listed below in tiered order (Tier 1 → Tier 2 → Tier 3):

### WU-6: Clock + dt Clamp (~250 lines, new crate)

**Crate**: `phantom-time` (new) OR module `phantom-scene::clock` if adding a crate feels heavy.

**Why**: Every subsystem updating at the frame rate is wrong. Phantom will have a brain thinking every 200ms, agents polling MCP every few seconds, the renderer at 16.6ms. Without per-subsystem clocks and cadences, everything shares one tick and the brain either wakes too often (CPU waste) or not often enough (laggy agent coordination).

**Types**:
```rust
pub struct Clock {
    time_cycles: u64,        // monotonic, CPU-cycle-scale
    time_scale: f32,         // 1.0 normal, 0.5 half-speed, 0.0 paused-equivalent
    is_paused: bool,
    created_at: Instant,
}

impl Clock {
    pub fn new() -> Self;
    pub fn new_paused() -> Self;
    pub fn tick(&mut self, real_dt: Duration);        // advances time_cycles by real_dt * time_scale
    pub fn elapsed_seconds(&self) -> f64;             // since creation
    pub fn dt_seconds(&self, other: &Clock) -> f64;   // delta between two clocks
    pub fn pause(&mut self);
    pub fn resume(&mut self);
    pub fn set_scale(&mut self, scale: f32);          // negative = reverse
    pub fn single_step(&mut self, step: Duration);    // force-advance while paused
    pub fn is_paused(&self) -> bool;
}

pub struct DtClamp {
    target_dt: Duration,      // e.g. 16.6ms for 60fps
    max_dt: Duration,         // e.g. 100ms — anything above clamps to target_dt
}

impl DtClamp {
    pub fn apply(&self, measured: Duration) -> Duration {
        if measured > self.max_dt { self.target_dt } else { measured }
    }
}
```

**App integration** (minimal, WU-6 delivers this one-liner in `app.rs`):
```rust
let measured = instant_now - last_tick;
let dt = self.dt_clamp.apply(measured);
self.real_clock.tick(dt);
self.session_clock.tick(dt);  // paused during console-overlay, resume on close
self.fx_clock.tick(dt);       // scaled for slo-mo FX debugging
```

**Per-subsystem cadences** (WU-6 declares the type; WU-1 consumes):
```rust
pub struct Cadence {
    pub target_hz: f32,       // e.g. 1.0 for agents, 5.0 for brain, 60.0 for renderer
    last_tick: Duration,
}

impl Cadence {
    pub fn should_tick(&mut self, clock: &Clock) -> bool {
        let now = Duration::from_secs_f64(clock.elapsed_seconds());
        if now - self.last_tick >= Duration::from_secs_f32(1.0 / self.target_hz) {
            self.last_tick = now;
            true
        } else {
            false
        }
    }
}
```

**Tests**:
```
test_clock_advances_monotonic
test_clock_pause_freezes_time
test_clock_scale_slows_time
test_clock_negative_scale_reverses
test_clock_single_step_while_paused
test_dt_clamp_passes_normal_values
test_dt_clamp_clamps_breakpoint_lag
test_cadence_fires_at_target_hz
test_cadence_skips_when_too_soon
```

**Gate**: Must merge before WU-1 (AppCoordinator uses Cadence per adapter).

### WU-7: Explicit Subsystem Start-Up / Shut-Down (~150 lines)

**File**: `crates/phantom-app/src/boot.rs` (new), modifies `crates/phantom-app/src/app.rs::new()`.

**Why**: `App::new()` today is a wall of `.new()` calls in whatever order compiled first. 19 crates with real deps (renderer needs GPU device before atlas; atlas before text renderer; text renderer before scene; scene before adapters; adapters before coordinator; coordinator before MCP listener; etc). One subtle bug — brain started before supervisor — and the symptom appears in a completely different module. An explicit ordered sequence makes the DAG legible and crash-report-friendly.

**Pattern** (Gregory ch 5.1.2 — "brute force wins"):
```rust
pub struct Subsystems {
    // Leaves (no deps): logging, gpu, time
    pub logging: LoggingSystem,
    pub gpu: GpuContext,
    pub clocks: ClockBank,

    // Mid-layer (deps: above)
    pub atlas: GlyphAtlas,
    pub text_renderer: TextRenderer,
    pub shader_pipeline: ShaderPipeline,
    pub resources: ResourceManager,

    // Scene (deps: renderer stack)
    pub scene: SceneTree,
    pub layout: LayoutEngine,

    // App layer (deps: scene)
    pub registry: AppRegistry,
    pub bus: EventBus,
    pub coordinator: AppCoordinator,

    // Agents (deps: coordinator + bus)
    pub supervisor: Supervisor,
    pub brain: BrainHandle,
    pub mcp_listener: McpListener,
}

impl Subsystems {
    pub fn start_up(config: &Config) -> anyhow::Result<Self> {
        // Ordered, top-down. Each line can panic-eject with context.
        let logging = LoggingSystem::start_up(config)?;
        let gpu = GpuContext::start_up(config)?;
        let clocks = ClockBank::start_up()?;
        let atlas = GlyphAtlas::start_up(&gpu)?;
        let text_renderer = TextRenderer::start_up(&gpu, &atlas)?;
        // ... etc
        Ok(Self { logging, gpu, /* ... */ })
    }

    pub fn shut_down(self) -> anyhow::Result<()> {
        // Reverse order. Consuming self so drop order is enforced.
        self.mcp_listener.shut_down()?;
        self.brain.shut_down()?;
        self.supervisor.shut_down()?;
        // ... etc
        Ok(())
    }
}
```

**Tests**:
```
test_subsystems_start_in_declared_order
test_subsystems_shut_down_in_reverse
test_start_up_failure_unwinds_prior
test_full_boot_then_shut_down_no_leak
```

### WU-9: DebugDrawManager (~400 lines)

**File**: `crates/phantom-renderer/src/debug_draw.rs` + `debug_draw/primitives.rs`.

**Why**: Phantom has a scene graph, video playback, glitch FX origins, layout boxes, agent pane borders, sysmon widgets — all producing visual artifacts whose position/bounds/state are currently invisible to the developer. Gregory ch 9.2: "a picture is worth 1,000 minutes of debugging." The pattern is a global queue any code can push into; renderer drains end-of-frame.

**API**:
```rust
pub struct DebugDrawManager {
    primitives: Vec<(DebugPrimitive, DrawOptions, f32 /*lifetime*/)>,
    enabled: bool,
}

pub enum DebugPrimitive {
    Line { from: Vec3, to: Vec3 },
    Cross { at: Vec3, size: f32 },
    Sphere { center: Vec3, radius: f32 },
    AxisBox { min: Vec3, max: Vec3 },
    OrientedBox { center: Mat4, scale: Vec3 },
    Axes { at: Mat4, size: f32 },           // XYZ in R/G/B
    String { at: Vec3, text: String },
    Rect2D { min: Vec2, max: Vec2 },        // screen-space
    String2D { at: Vec2, text: String },    // screen-space
}

pub struct DrawOptions {
    pub color: [f32; 4],
    pub line_width: f32,
    pub depth_tested: bool,
    pub space: DrawSpace,  // World | Screen
}

impl DebugDrawManager {
    pub fn add_line(&mut self, from: Vec3, to: Vec3, opts: DrawOptions, lifetime: f32);
    pub fn add_cross(&mut self, at: Vec3, size: f32, opts: DrawOptions, lifetime: f32);
    pub fn add_sphere(&mut self, center: Vec3, radius: f32, opts: DrawOptions, lifetime: f32);
    pub fn add_axes(&mut self, at: Mat4, size: f32, opts: DrawOptions, lifetime: f32);
    pub fn add_string(&mut self, at: Vec3, text: &str, opts: DrawOptions, lifetime: f32);
    pub fn add_rect_2d(&mut self, min: Vec2, max: Vec2, opts: DrawOptions, lifetime: f32);
    pub fn add_string_2d(&mut self, at: Vec2, text: &str, opts: DrawOptions, lifetime: f32);

    pub fn flush(&mut self, dt: f32, render_pass: &mut RenderPass);  // drain + decay lifetimes
    pub fn clear(&mut self);
    pub fn set_enabled(&mut self, on: bool);
}
```

**Wire-up in WU-5**: add `debug_draw: DebugDrawManager` to `App`, call `debug_draw.flush(dt, &mut pass)` at end of frame. Toggle via console command `debug.draw on|off`.

**Screen-space debug strings are the killer feature** — gives agent panes their own overlay annotations, shows layout box rects for every taffy node in debug mode, marks glitch FX origins, draws video bounds, labels scene nodes.

**Tests**:
```
test_add_line_queues_primitive
test_lifetime_decays_on_flush
test_expired_primitives_removed
test_clear_empties_queue
test_disabled_manager_skips_flush
test_world_vs_screen_space_dispatch
```

### WU-10: Console Evaluator Wiring (~100 lines)

**File**: Console command handler in [phantom-app](../crates/phantom-app/).

**Why**: The console overlay exists and reads input, but it evaluates a hardcoded switch of commands. Gregory ch 9.4: console and scripting must share an evaluator or the console becomes a second-class surface that drifts. Phantom's NLP + brain already parse natural language into actions — the console should route through that same pipeline.

**Flow**:
```
console input text
    ↓
    phantom-nlp::interpret(text) → Action | Unknown
    ↓
    if Unknown: phantom-brain::route(text) → Action
    ↓
    dispatch Action via coordinator.send_command() OR direct App method
    ↓
    render result as console output line
```

**Changes**:
- Console's current hardcoded command table becomes a fallback (fast-path for trivial `clear`, `quit`, `help`).
- Anything else → NLP → Brain → Action.
- `AiEvent::AgentResponse` and other brain replies surface as console output lines.

**Tests**:
```
test_console_exec_trivial_command
test_console_exec_nlp_parsed_command
test_console_exec_brain_routed_command
test_console_unknown_command_shows_suggestions
test_console_agent_response_renders_inline
```

### WU-11: Channel-Tagged Logging + File Mirror + Panic Flush (~300 lines)

**File**: `crates/phantom-app/src/logging.rs` (new), modifies `main.rs` or `lib.rs` entry point.

**Why**: Today Phantom uses `env_logger` with one global stream. When 19 subsystems all shout at once, debugging is reading a wall of text. Gregory ch 9.1: channels + verbosity + file mirror + flush-on-panic are table stakes.

**Design**:
```rust
bitflags! {
    pub struct Channels: u32 {
        const RENDERER   = 1 << 0;
        const SHADER     = 1 << 1;
        const TERMINAL   = 1 << 2;
        const ADAPTER    = 1 << 3;
        const COORDINATOR= 1 << 4;
        const SCENE      = 1 << 5;
        const SEMANTIC   = 1 << 6;
        const NLP        = 1 << 7;
        const BRAIN      = 1 << 8;
        const SUPERVISOR = 1 << 9;
        const AGENTS     = 1 << 10;
        const MCP        = 1 << 11;
        const PLUGINS    = 1 << 12;
        const MEMORY     = 1 << 13;
        const CONTEXT    = 1 << 14;
        const SESSION    = 1 << 15;
        const BOOT       = 1 << 16;
        const INPUT      = 1 << 17;
        const FX         = 1 << 18;
        const PROFILER   = 1 << 19;
        const ALL        = u32::MAX;
    }
}

pub struct PhantomLogger {
    active_channels: AtomicU32,     // bitmask, runtime-mutable
    verbosity: AtomicU8,            // 0=error, 1=warn, 2=info, 3=debug, 4=trace
    file: Mutex<BufWriter<File>>,   // ~/.config/phantom/logs/phantom-{timestamp}.log
    stderr: bool,
}

impl log::Log for PhantomLogger { /* ... */ }

pub fn start_up(config: &Config) -> anyhow::Result<()> {
    let logger = PhantomLogger::new(config)?;
    log::set_boxed_logger(Box::new(logger))?;
    log::set_max_level(log::LevelFilter::Trace);
    install_panic_hook();   // on panic: flush file, dump crash report
    Ok(())
}

fn install_panic_hook() {
    let orig = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(logger) = logger::current() {
            logger.flush_all();
            logger.dump_crash_report(info);
        }
        orig(info);
    }));
}
```

**Call-site convention**:
```rust
log::info!(target: "phantom::brain", "routing command: {}", text);
log::debug!(target: "phantom::renderer", "frame {} — {} quads, {} glyphs", ...);
```

Target string prefix maps to a channel bit. Runtime commands: `log.channel brain off`, `log.verbose 3`.

**Crash report contents** (Gregory ch 9.1.5):
- Timestamp + build hash + panic message + location
- Stack trace (via `backtrace` crate)
- Active sessions list, focused app, open agents
- Last 50 log lines across all channels
- Memory allocator stats (if available)
- Last 20 user commands

Writes to `~/.config/phantom/crashes/crash-{timestamp}.txt`.

**Tests**:
```
test_channel_filter_suppresses_muted
test_verbosity_filter_suppresses_below_threshold
test_file_mirror_writes_all_messages
test_runtime_channel_toggle
test_panic_hook_flushes_file
test_crash_report_has_required_fields
```

### WU-12: Tracy Profiler Integration (~150 lines)

**Files**: `Cargo.toml` workspace deps, `crates/phantom-app/src/profiler.rs` (new), profile macros in hot paths.

**Why**: 60fps with full shader stack + terminal reflow + agent coordination is not free. Gregory ch 9.8: you need hierarchical timing or you guess. Tracy is the industry-standard tool (used by AAA game studios), `tracy-client` crate exists, integration is ~50 lines + sprinkle macros.

**Design**: Don't roll own. Add `tracy-client = { version = "...", features = ["enable"], optional = true }` as workspace dep. Gated behind `phantom-profile` feature flag — zero cost in release builds without the flag.

```rust
// phantom-app/src/profiler.rs

#[macro_export]
macro_rules! profile_scope {
    ($name:literal) => {
        #[cfg(feature = "phantom-profile")]
        let _span = tracy_client::span!($name);
    };
}

#[macro_export]
macro_rules! profile_frame {
    () => {
        #[cfg(feature = "phantom-profile")]
        tracy_client::frame_mark();
    };
}

pub fn start_up() {
    #[cfg(feature = "phantom-profile")]
    tracy_client::Client::start();
}
```

**Instrumentation points** (WU-12 adds these to existing hot paths):
- `App::tick` → `profile_scope!("tick")`
- `App::render` → `profile_scope!("render")`, nested: `"render.scene"`, `"render.postfx"`, `"render.overlay"`
- `AppCoordinator::update_all` → `profile_scope!("coord.update")`
- `TextRenderer::prepare_glyphs` → `profile_scope!("text.prepare")`
- `ShaderPipeline::frame` → `profile_scope!("shader.frame")`
- `BrainHandle::tick` → `profile_scope!("brain.tick")`
- `Supervisor::tick` → `profile_scope!("supervisor.tick")`
- End of frame → `profile_frame!()`

**Usage**: `cargo run --features phantom-profile` and connect Tracy GUI. Flamegraph + per-subsystem timeline + frame graph all appear automatically.

**Tests**:
```
test_profile_scope_macro_compiles_without_feature  // zero-cost when off
test_profile_scope_macro_compiles_with_feature
test_frame_mark_fires_once_per_frame
```

### WU-8: Job Queue + Worker Pool (~500 lines, new crate)

**Crate**: `phantom-jobs` (new) OR module `phantom-supervisor::jobs`.

**Why**: [ch7.6.5-7.6.6] Phantom's agent model today spawns a thread per agent. That works for 2–3 agents; it falls over at 10+ (thread contention, scheduler thrashing, no priority, no cancellation). The book's prescription is a job queue: small `(code, data)` units picked up by a fixed worker pool. Phantom's agent tasks are already job-shaped — `(prompt, context, tools, callback)`. This WU formalizes it and future-proofs agent coordination for Phase 2+.

**Types**:
```rust
pub struct Job {
    pub id: JobId,
    pub priority: JobPriority,       // High | Normal | Low | Background
    pub payload: Box<dyn JobPayload + Send>,
    pub cancel: Arc<AtomicBool>,
    pub submitted_at: Instant,
}

pub trait JobPayload {
    fn run(&mut self, ctx: &JobContext) -> JobResult;
    fn describe(&self) -> &str;
}

pub enum JobResult {
    Done(serde_json::Value),
    Err(anyhow::Error),
    Cancelled,
}

pub struct JobPool {
    senders: [Sender<Job>; 4],        // one queue per priority level
    workers: Vec<JoinHandle<()>>,
    in_flight: DashMap<JobId, JobStatus>,
    results: Receiver<(JobId, JobResult)>,
}

impl JobPool {
    pub fn start_up(worker_count: usize) -> anyhow::Result<Self>;
    pub fn submit(&self, job: Job) -> JobHandle;
    pub fn try_poll(&self, handle: &JobHandle) -> Option<JobResult>;
    pub fn cancel(&self, handle: &JobHandle);
    pub fn drain_completed(&self) -> Vec<(JobId, JobResult)>;
    pub fn shut_down(self) -> anyhow::Result<()>;    // drains in-flight, joins workers
}

pub struct JobHandle {
    id: JobId,
    cancel: Arc<AtomicBool>,
    _marker: PhantomData<*const ()>,  // !Send, owned by caller
}
```

**Priority discipline**: High = user-facing (agent responding to user question). Normal = background agent work. Low = speculative (brain predictions). Background = housekeeping (context index rebuild).

**Integration points** (WU-5 wires these up, WU-8 only provides the pool):
- Agent supervisor submits agent work as jobs instead of spawning threads
- Brain submits route/decide calls as jobs
- Semantic parser submits parse work as jobs
- NLP interpreter submits interpretation as jobs
- MCP listener wraps request handlers as jobs
- Memory queries submit as low-priority jobs

**Tests**:
```
test_submit_job_returns_handle
test_poll_none_when_in_flight
test_poll_some_when_complete
test_cancel_aborts_before_run
test_cancel_sets_flag_during_run
test_priority_high_preempts_normal
test_worker_panic_doesnt_kill_pool     // caught, reported, worker respawns
test_shutdown_drains_in_flight
test_shutdown_joins_all_workers
test_stress_1000_jobs_4_workers
```

### WU-14: Unified ResourceManager (~450 lines, new crate)

**Crate**: `phantom-resources` (new) OR module `phantom-app::resources`.

**Why**: [ch6.2] Today Phantom loads shaders, fonts, videos, themes, and WASM plugins through separate ad-hoc code paths scattered across 5 crates. No single source of truth for "what's loaded," no ref-counting (switching themes reloads shaders from disk), no streaming (opening a large video blocks the render thread), no GUID system (hash collisions possible). Every mature engine has one loader.

**API**:
```rust
pub trait Resource: Send + Sync + 'static {
    fn kind(&self) -> &'static str;
    fn size_hint(&self) -> usize;
}

pub struct ResourceId(pub u64);  // hash of canonical path

pub struct ResourceManager {
    registry: DashMap<ResourceId, Arc<dyn Resource>>,
    ref_counts: DashMap<ResourceId, AtomicUsize>,
    loaders: HashMap<&'static str, Box<dyn ResourceLoader>>,
    streaming_queue: Sender<StreamRequest>,
    results: Receiver<(ResourceId, Result<Arc<dyn Resource>, LoadError>)>,
}

impl ResourceManager {
    pub fn start_up(job_pool: &JobPool) -> anyhow::Result<Self>;  // uses WU-8 for async loads

    pub fn register_loader<L: ResourceLoader + 'static>(&mut self, kind: &'static str, loader: L);

    pub fn load_blocking<R: Resource>(&self, path: &str) -> anyhow::Result<ResourceHandle<R>>;
    pub fn load_streaming<R: Resource>(&self, path: &str) -> ResourceHandle<R>;
    pub fn try_get<R: Resource>(&self, handle: &ResourceHandle<R>) -> Option<Arc<R>>;

    pub fn release(&self, id: ResourceId);   // decrement ref count; unload if 0

    pub fn gc(&self);                        // sweep zero-refcount resources
    pub fn memory_usage(&self) -> usize;
    pub fn shut_down(self) -> anyhow::Result<()>;
}

pub trait ResourceLoader: Send + Sync {
    fn load(&self, path: &str, bytes: &[u8]) -> Result<Arc<dyn Resource>, LoadError>;
}

pub struct ResourceHandle<R: Resource> {
    id: ResourceId,
    manager: Weak<ResourceManager>,
    _phantom: PhantomData<R>,
}

impl<R: Resource> Drop for ResourceHandle<R> {
    fn drop(&mut self) {
        if let Some(mgr) = self.manager.upgrade() {
            mgr.release(self.id);
        }
    }
}
```

**Loaders to register** (WU-5 plumbs these in):
- `ShaderLoader` → compiles WGSL/GLSL into `CompiledShader` resource
- `FontLoader` → produces `FontFace` resource via cosmic-text
- `ThemeLoader` → parses TOML theme into `Theme` resource
- `VideoLoader` → opens H.264 stream, produces `VideoResource` with frame buffer
- `WasmPluginLoader` → compiles WASM module into `LoadedPlugin` resource
- `SystemPromptLoader` → loads agent system prompts from disk into `SystemPrompt` resource

**Streaming design**: `load_streaming` returns handle immediately; manager submits a high-priority job to WU-8 that reads bytes from disk (background thread) and decodes (worker thread). `try_get` returns `None` until load completes. `ResourceHandle` drops release the ref count; `gc()` unloads.

**Tests**:
```
test_load_blocking_returns_handle
test_load_same_path_returns_same_id      // no duplicate load
test_ref_count_tracks_handle_lifetime
test_drop_handle_decrements_refcount
test_gc_unloads_zero_refcount
test_load_streaming_returns_none_until_ready
test_load_streaming_completes_via_poll
test_register_loader_new_kind
test_load_with_unknown_kind_returns_err
test_memory_usage_sums_loaded
```

### WU-15: Typed Event Bus (~300 lines)

**Files**: `crates/phantom-protocol/src/events.rs` (new or expanded), modifies `crates/phantom-adapter/src/bus.rs` (payload type only).

**Why**: [ch14.7] Today `BusMessage.payload: serde_json::Value` — stringly-typed, runtime-only errors, no compile check that publishers and subscribers agree on shape. Every framework that scaled (Bevy `EventWriter<T>`, Zed `cx.emit(event)`, SwiftUI `@Published`) is compile-time typed. Originally flagged for Phase 4 in the adapter plan — pulled forward because every Phase 2+ adapter adds more topics and the JSON tax compounds.

**Design**:
```rust
// phantom-protocol/src/events.rs

#[derive(Clone, Debug)]
pub enum Event {
    // Terminal / PTY
    TerminalOutput { app_id: AppId, bytes: u64 },
    CommandStarted { app_id: AppId, command: String },
    CommandComplete { app_id: AppId, exit_code: i32 },
    BuildFailed { app_id: AppId, error: BuildError },
    BuildSucceeded { app_id: AppId, duration: Duration },

    // Agents
    AgentSpawned { agent_id: AgentId, task: String },
    AgentProgress { agent_id: AgentId, fraction: f32, message: String },
    AgentTaskComplete { agent_id: AgentId, result: AgentResult },
    AgentError { agent_id: AgentId, error: String },

    // Sessions / Focus
    SessionSwitched { from: SessionId, to: SessionId },
    FocusChanged { from: Option<AppId>, to: Option<AppId> },

    // Brain / NLP
    BrainDecision { decision: Decision, confidence: f32 },
    NlpInterpreted { input: String, action: Action },

    // Video / FX
    VideoPlaybackStateChanged { app_id: AppId, playing: bool },
    GlitchFxTriggered { origin: [f32; 2], intensity: f32 },

    // System
    MemoryPressure { bytes_free: usize },
    JobCompleted { job_id: JobId },
    Shutdown,
}

#[derive(Clone, Debug)]
pub struct BusMessage {
    pub topic: Topic,
    pub event: Event,
    pub from: Option<AppId>,
    pub frame: u64,          // NEW: frame number when published (Phase 4 item, pulled forward)
    pub timestamp: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Topic {
    Terminal,
    Agents,
    Sessions,
    Brain,
    Video,
    Fx,
    System,
}

impl Event {
    pub const fn topic(&self) -> Topic {
        match self {
            Event::TerminalOutput { .. } | Event::CommandStarted { .. }
                | Event::CommandComplete { .. } | Event::BuildFailed { .. }
                | Event::BuildSucceeded { .. } => Topic::Terminal,
            Event::AgentSpawned { .. } | Event::AgentProgress { .. }
                | Event::AgentTaskComplete { .. } | Event::AgentError { .. } => Topic::Agents,
            Event::SessionSwitched { .. } | Event::FocusChanged { .. } => Topic::Sessions,
            Event::BrainDecision { .. } | Event::NlpInterpreted { .. } => Topic::Brain,
            Event::VideoPlaybackStateChanged { .. } => Topic::Video,
            Event::GlitchFxTriggered { .. } => Topic::Fx,
            Event::MemoryPressure { .. } | Event::JobCompleted { .. }
                | Event::Shutdown => Topic::System,
        }
    }
}
```

**Bus change** (minimal, WU-15 delivers):
```rust
// phantom-adapter/src/bus.rs — change payload field from serde_json::Value to Event
pub struct BusMessage {
    // existing fields + payload: serde_json::Value  ← REMOVED
    pub event: Event,   // NEW
    pub frame: u64,     // NEW
    // ...
}
```

**Backward-compat**: Adapters previously publishing JSON blobs now publish typed `Event` variants. Unknown / extension events use `Event::Custom(serde_json::Value)` escape hatch for plugins.

**Tests**:
```
test_event_topic_routing
test_event_matches_declared_topic
test_bus_publish_typed_event
test_subscriber_receives_only_subscribed_topic
test_frame_number_monotonic_across_publishes
test_custom_event_payload_roundtrip
test_all_events_are_send_sync_clone
```

### WU-13 (not a standalone WU): Async result pattern

Rolled into WU-5 (integration). During integration wiring, every existing blocking call in the update/render path is converted to the job-queue pattern from WU-8:

| Current (blocking) | After WU-5 |
|---|---|
| `let parsed = semantic::parse(output);` | `let job = job_pool.submit(SemanticParseJob::new(output)); // frame N` → `if let Some(result) = job_pool.try_poll(job) { ... } // frame N+k` |
| `let intent = nlp::interpret(text);` | submit NLP job → poll next frame |
| `let decision = brain::route(cmd);` | submit brain job → poll next frame |
| `let memory = memory::query(key);` | submit memory job → poll next frame |
| `mcp.call(method, args)` | submit MCP job → poll next frame |

Render loop never blocks on any of these. Completion events published via WU-15 typed bus. Checkpoints enforced in WU-5 review (§6 Stage 5).

---

## 9. Success Criteria

Phase 1 is DONE when all of the following are true:

**Adapter framework:**
1. `cargo test --workspace` passes with 0 failures
2. `cargo clippy --workspace` passes with 0 warnings
3. Terminal renders identically through the adapter path (screenshot regression < 5% drift)
4. Typing in terminal works with no perceptible latency increase
5. Split horizontal / split vertical create new TerminalAdapters
6. Close pane removes adapter, rebalances layout, refocuses
7. Brain receives `AiEvent::CommandComplete` from adapter's bus emissions
8. MCP commands (`screenshot`, `send_key`, `read_output`, `split_pane`) all work through coordinator
9. Sysmon, agent panes, console, boot sequence are unaffected
10. Zero `unwrap()` in new code (outside `#[cfg(test)]`)
11. All new code is `deny(warnings)` clean

**Engine foundations (added 2026-04-24 — all 11 patterns from GEA audit):**
12. `Clock` type lives in a dedicated module; `App` holds `real_clock`, `session_clock`, `fx_clock`; all adapter `update(dt)` calls receive their subsystem's clock, not the raw frame dt
13. Each adapter declares a `Cadence` (target Hz); coordinator skips `update()` calls when the adapter isn't due
14. `DtClamp` applied in main loop; artificially induced 500ms frame stall does NOT crash physics/FX/animation
15. `Subsystems::start_up()` boots in explicit, declared order; `shut_down()` unwinds in reverse; full boot→shut_down cycle leaves zero leaked resources in a sanitizer build
16. `DebugDrawManager` is called at least once from a real code path (agent pane bounds, scene node axes, or layout box overlay); toggle via `debug.draw on|off` console command
17. Console overlay routes input through NLP → brain path; hardcoded command table is <10 commands (trivial cases only)
18. `PhantomLogger` is installed; `log.channel <name> on|off` and `log.verbose <n>` work at runtime; panic produces a crash report file
19. `phantom-profile` feature flag builds; Tracy GUI shows at least 8 named spans (tick, render, coord.update, text.prepare, shader.frame, brain.tick, supervisor.tick, frame_mark)
20. `JobPool` replaces per-agent threads; running 10 concurrent agents uses ≤4 OS threads; worker panic does not kill pool (caught, logged, worker respawns)
21. `ResourceManager` is the only loader in the workspace; shaders, fonts, themes, videos, WASM plugins, system prompts all load through it; loading the same path twice returns the same `Arc`; dropping the last handle decrements ref count
22. All `BusMessage`s carry typed `Event` enum (no `serde_json::Value` payloads in main paths); `frame` field populated; `Event::topic()` matches published topic
23. Semantic parse, NLP interpret, brain decision, memory query, and MCP call sites no longer block the render loop — all converted to submit-job-poll-next-frame (WU-13 via WU-8); render frame time P99 unchanged or better

---

## 10. What Phase 1 Does NOT Include

Explicitly out of scope (deferred to Phase 2+). No Phase 1.5 — all engine foundations ship in Phase 1.

**Phase 2:**
- VideoAdapter, AgentAdapter, MonitorAdapter (now trivial to build on complete adapter + job queue + resource manager + typed bus foundation)

**Phase 3:**
- Floating panes
- Spatial negotiation / SpatialPreference in layout
- Pop-out windows (3b)

**Phase 4:**
- Bus wiring between adapters (cross-adapter routing rules)
- User-created pipes

**Phase 5:**
- AI command routing to adapters
- Dynamic MCP tool registration
- WASM adapter runtime

---

## 11. Why Engine Foundations Are In Phase 1 (Not Deferred)

The *Game Engine Architecture* audit (2026-04-24) surfaced eleven patterns that Phantom will need regardless of feature roadmap, because every adapter added in Phase 2+ assumes them:

| Pattern | What happens if deferred |
|---------|--------------------------|
| Split render loop from update loop + per-adapter cadences (WU-6, WU-1) | Every Phase 2 adapter (Video at 24fps, Agent at 0.5Hz, Monitor at 1Hz) either over-ticks (CPU waste, battery drain) or is retrofitted later — all adapter `update()` signatures change, cascading through every test |
| Clock type (WU-6) | Video playback, glitch FX timing, slo-mo debugging, agent replay all roll their own clocks → four incompatible time domains |
| dt clamp (WU-6) | First debugger session after Phase 2 ships causes a 30-second spike → animation explodes, agent state desyncs, bug report "Phantom crashes when I breakpoint" |
| Explicit start_up/shut_down (WU-7) | Adding VideoAdapter or AgentAdapter to the wall of `::new()` calls in `App::new()` will put them in an order that compiles but mis-initializes (brain starts before supervisor → silently wrong) |
| Job queue (WU-8) | Phase 2's AgentAdapter inherits per-agent thread model; scaling past 5 concurrent agents causes scheduler thrashing; retrofitting requires changing every agent call site in supervisor + brain + MCP + NLP |
| DebugDrawManager (WU-9) | Layout bugs in floating panes (Phase 3) require printf-logging pixel coords → 10x slower development of the hardest UI feature |
| Console evaluator (WU-10) | Every new command gets hardcoded into the console switch → drift between "what the console can do" and "what NLP can do" grows every release |
| Channel logging + crash reports (WU-11) | Phase 2 bugs in agent orchestration produce a 10,000-line log wall → impossible to triage; crash reports from users contain nothing actionable |
| Tracy profiler (WU-12) | First perf regression after Phase 2 lands is debugged by adding `println!` with `Instant::now()` → hours wasted; no historical comparison possible |
| ResourceManager (WU-14) | VideoAdapter and WasmPlugin registry (Phase 2, Phase 4) reinvent their own loader; switching themes reloads shaders from disk; opening a 4K video blocks the render thread |
| Typed event bus (WU-15) | Every new topic in Phase 2+ adds a stringly-typed JSON contract that breaks silently; schema drift between publisher and subscriber surfaces as a mysterious missing-field error at runtime |
| Async result pattern (WU-13 in WU-5) | Brain thinking or agent polling blocks render → visible jank; first time users feel the terminal "freeze" they blame Phantom, not the LLM |

The economic argument: WU-6 through WU-16 total ~2,500 lines of new code (plus ~400 lines of conversion work inside WU-5). The cost of retrofitting these after Phase 2+ ships is 5–10x that, because every adapter, every subsystem, every test touches them. Do it once, now, while there's one adapter to migrate.

---

## 12. Updated Scope Summary (2026-04-24)

| Category | Original Phase 1 | Expanded Phase 1 (full fold-in) |
|----------|------------------|---------------------------------|
| Work units | 6 (WU-0 through WU-5) | 15 (WU-0 through WU-15, with WU-13 absorbed into WU-5) |
| New code | ~700 lines | ~4,300 lines |
| Migration | ~200 lines | ~600 lines (includes async conversion in WU-5) |
| Success criteria | 11 | 23 |
| New crates | 0 | up to 3 (phantom-time, phantom-jobs, phantom-resources — each optional; can be modules in existing crates) |
| Wave 1 parallelism | 3 agents | 10 agents (recommend 3 batches) |
| Pre-req gates | 1 (WU-0) | 3 (WU-0, WU-6, WU-15) |
| Phase 1.5 deferrals | — | None. Everything in Phase 1. |

## 13. Risks of the Expanded Scope

**Risk: Phase 1 grows too large to ship cohesively**
**Mitigation**: The decomposition is deliberately parallelizable. Wave 1 has 10 agents across disjoint files. Each work unit is independently testable. If any one WU slips, it does not block the others — only WU-5 (integration) serializes them. If extreme schedule pressure appears, WU-12 (profiler) is the only fully-optional item (feature-flagged, zero runtime cost when off). Every other WU has a Phase 2 caller that depends on it.

**Risk: Contract churn as foundations interact with coordinator design**
**Mitigation**: Three pre-requisite gates (WU-0 trait split, WU-6 Clock+Cadence, WU-15 typed event bus) must merge before Wave 1 starts. Once merged, the contracts are frozen. No speculative building on unmerged contracts.

**Risk: More agents means more merge conflicts**
**Mitigation**: File-claim matrix in §5 is zero-overlap across all 10 Wave 1 agents. Worktree isolation (per ARD-003 findings on Claude Code v2.1.50 worktrees) eliminates physical conflict. The only sequential point is WU-5 (integration), which is 1 agent by design.

**Risk: WU-5 integration agent must hold context for all of Wave 1 at once**
**Mitigation**: WU-5 has a detailed checklist (§8). Each integration point is a discrete conversion pattern (replace X with Y) with explicit before/after code. Review gate §6 Stage 5 catches regressions. The integration PR may land in sub-PRs if size becomes unwieldy (e.g., "integration-coordinator", "integration-jobs", "integration-resources") — but must merge together.

**Risk: Running 3 new crates simultaneously increases Cargo resolver churn**
**Mitigation**: Each new crate (phantom-time, phantom-jobs, phantom-resources) has ≤3 dependencies — no framework-level deps. Alternative: land each as a module inside an existing crate (phantom-scene, phantom-supervisor, phantom-app) to avoid new crate overhead. WU authors have this flexibility per §8a specs.
