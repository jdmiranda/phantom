# Phase 1 Execution Plan: AppCoordinator + Terminal Adapter

**Date**: 2026-04-23
**Status**: Draft
**Depends on**: ARD-002, ARD-003, plan-adapter-integration.md
**Estimated scope**: ~700 lines new code, ~200 lines migration

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

Phase 1 decomposes into **1 pre-requisite**, **3 independent work units** that can be built in parallel, plus **1 integration unit** and **1 test unit**.

```
WU-0: Trait Split           (phantom-adapter/src/*.rs — modify existing)
      PRE-REQUISITE: must merge before Wave 1 starts
WU-1: AppCoordinator       (coordinator.rs — new file)
WU-2: TerminalAdapter       (adapters/terminal.rs — new file)
WU-3: RenderOutput Extension (phantom-adapter/src/adapter.rs — modify)
WU-4: Unit Tests            (phantom-adapter tests + phantom-app tests)
WU-5: Integration Wiring    (app.rs, update.rs, render.rs, input.rs — modify)
      DEPENDS ON: WU-1, WU-2, WU-3
```

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

### Dependency Graph

```
WU-0 (Trait Split) ─────► MERGE TO MAIN (gate)
                                │
                    ┌───────────┼───────────┐
                    ▼           ▼           ▼
              WU-1 (Coord)  WU-2 (Term)  WU-3 (Render)
                    │           │           │
                    └───────────┼───────────┘
                                ▼
                          WU-5 (Integration)
                                │
                                ▼
                          WU-4 (Tests)
```

### Parallelism

- **Wave 0** (sequential): WU-0 — trait split, must merge before anything else
- **Wave 1** (parallel): WU-1, WU-2, WU-3 — zero file overlap, can run simultaneously
- **Wave 2** (sequential): WU-5 — depends on all of Wave 1
- **Wave 3** (sequential): WU-4 — depends on WU-5 for integration tests

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

## 9. Success Criteria

Phase 1 is DONE when all of the following are true:

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

---

## 10. What Phase 1 Does NOT Include

Explicitly out of scope (deferred to Phase 2+):

- VideoAdapter, AgentAdapter, MonitorAdapter (Phase 2)
- Floating panes (Phase 3)
- Spatial negotiation / SpatialPreference in layout (Phase 3)
- Bus wiring between adapters (Phase 4)
- User-created pipes (Phase 4)
- Typed event channels replacing serde_json::Value bus payloads (Phase 4)
- Frame number in BusMessage for traceability (Phase 4)
- AI command routing to adapters (Phase 5)
- Dynamic MCP tool registration (Phase 5)
- Pop-out windows (Phase 3b)
- WASM adapter runtime (future)
