# Phantom AppAdapter Integration Plan

## Context

Phantom has a fully designed "everything is an app" framework (`phantom-adapter`) with an `AppAdapter` trait, pub/sub `EventBus`, `AppRegistry`, lifecycle state machine, and Wayland-style spatial negotiation. **None of it is wired in.** Every component (terminals, video, agents, monitors) is hardcoded directly into `App` with bespoke render paths, no inter-component communication, and a fixed layout.

The goal: make the framework real. Every visual component becomes an `AppAdapter`. The bus routes data between them. The layout negotiates space. The brain orchestrates. Panes are resizable, floatable, pipeable, and AI-controllable.

---

## Phase 1: AppAdapter Coordinator + Terminal Adapter

**Goal:** Wire the first real adapter into the system. Prove the pattern works end-to-end with the most critical component (terminal panes).

### 1.1 Create `AppCoordinator` (new file: `crates/phantom-app/src/coordinator.rs`)

Central manager that owns `AppRegistry` + `EventBus` and orchestrates the adapter lifecycle.

```
AppCoordinator {
    registry: AppRegistry,
    bus: EventBus,
    pane_map: HashMap<PaneId, AppId>,   // layout ↔ adapter mapping
    app_pane_map: HashMap<AppId, PaneId>,
    focused: Option<AppId>,
}
```

**Methods:**
- `register_adapter(adapter, layout, scene) -> AppId` — registers adapter, creates topics from `publishes()`, subscribes from `subscribes_to()`, creates pane in layout, creates scene node, transitions to Running
- `remove_adapter(app_id)` — transitions to Exiting→Dead, removes pane, unsubscribes, GCs
- `update_all(dt)` — for each Running adapter: `adapter.update(dt)`, drain bus messages, deliver via `on_message()`
- `render_all(layout) -> Vec<(AppId, Rect, RenderOutput)>` — for each visual adapter: get rect from layout, call `adapter.render(&rect)`
- `route_input(key) -> bool` — forward input to focused adapter via `handle_input()`
- `set_focus(app_id)` — changes which adapter receives input
- `get_state(app_id) -> Value` — proxy to `adapter.get_state()` (for brain)
- `send_command(app_id, cmd, args) -> Result<String>` — proxy to `adapter.accept_command()`

**Files modified:**
- `crates/phantom-app/src/app.rs` — add `coordinator: AppCoordinator` field, remove direct `panes: Vec<Pane>`
- `crates/phantom-app/src/lib.rs` — add `mod coordinator`

### 1.2 Create `TerminalAdapter` (new file: `crates/phantom-app/src/adapters/terminal.rs`)

Wraps `PhantomTerminal` + `Pane` state into an `AppAdapter` implementation.

```rust
struct TerminalAdapter {
    terminal: PhantomTerminal,
    output_buf: String,
    has_new_output: bool,
    error_notified: bool,
    is_detached: bool,
    detached_label: String,
    was_alt_screen: bool,
    // Emission queue: messages to publish after update()
    pending_emissions: Vec<(String, serde_json::Value)>,
}
```

**Trait implementation:**
- `app_type()` → `"terminal"`
- `is_visual()` → `true`
- `spatial_preference()` → `SpatialPreference::simple(40, 10).with_priority(1.0)`
- `render(&self, rect)` → extract grid from `terminal.term()`, build `RenderOutput` with quads + text
- `handle_input(&mut self, key)` → encode key to bytes, `terminal.pty_write()`
- `get_state()` → JSON with cursor position, last output lines, current directory, running process
- `accept_command(cmd, args)` → `"write"` writes to PTY, `"resize"` resizes terminal, `"read_output"` returns buffer
- `update(dt)` → `pty_read()`, update `output_buf`, detect errors, detect alt-screen, queue emissions
- `is_alive()` → PTY still connected
- `publishes()` → `[TopicDeclaration { name: "terminal.output", data_type: TerminalOutput }]`
- `subscribes_to()` → `[]` (terminals don't subscribe to anything by default)
- `on_message()` → no-op for now

**Rendering bridge:** `TerminalAdapter::render()` returns `RenderOutput` with simplified primitives. The main render loop converts these to GPU calls. This is the key abstraction — adapters don't touch wgpu directly.

**Problem:** The current `RenderOutput` only has `QuadData` + `TextData` — these are simplified types that don't carry enough info for the full terminal grid (cell attributes, cursor shape, colors). Two options:
- **Option A:** Extend `RenderOutput` with a `GridData` variant that carries the terminal grid cells
- **Option B:** Adapters produce intermediate data; the render loop has adapter-type-specific code to convert to GPU calls

**Recommendation:** Option A for now — add `grid_cells: Vec<GridCell>` to `RenderOutput`. This keeps the abstraction clean. `GridCell` already exists in `phantom-renderer`.

**Files modified:**
- `crates/phantom-adapter/src/adapter.rs` — extend `RenderOutput` with grid data
- `crates/phantom-app/src/adapters/terminal.rs` — new file
- `crates/phantom-app/src/adapters/mod.rs` — new file
- `crates/phantom-app/src/lib.rs` — add `mod adapters`
- `crates/phantom-app/src/update.rs` — replace pane loop with `coordinator.update_all(dt)`
- `crates/phantom-app/src/render.rs` — replace `render_terminal()` with coordinator-driven render
- `crates/phantom-app/src/input.rs` — replace direct PTY write with `coordinator.route_input()`

### 1.3 Migration Strategy

**Keep both paths alive during migration.** Don't rip out the old code immediately:

1. Add coordinator alongside existing `panes` vec
2. On startup, create `TerminalAdapter` for the initial pane, register with coordinator
3. Route input through coordinator for registered adapters, fall through to old path for unregistered
4. Render registered adapters via coordinator, fall through to old `render_terminal()` for unregistered
5. Once all panes use adapters, delete old path

**Verification:**
- Terminal renders identically (grid cells, cursor, colors)
- PTY I/O works (type commands, see output)
- Split/close panes work through coordinator
- Semantic scanning still detects errors (via adapter's `update()`)
- Brain still receives `AiEvent::CommandComplete` (via adapter's emission queue)

---

## Phase 2: Video + Agent + Monitor Adapters

**Goal:** Convert remaining hardcoded components to AppAdapters. Video is the highest impact — it goes from fullscreen takeover to a resizable pane.

### 2.1 `VideoAdapter` (new file: `crates/phantom-app/src/adapters/video.rs`)

```rust
struct VideoAdapter {
    playback: Option<VideoPlayback>,
    renderer: VideoRenderer,  // owns GPU texture
    state: VideoState,        // Idle, Playing, Paused, Finished
}
```

**Key trait methods:**
- `spatial_preference()` → `SpatialPreference { aspect_ratio: Some(16.0/9.0), preferred_size: (video_w, video_h), priority: 2.0 }`
- `render(rect)` → render video frame into the given rect (not fullscreen!)
- `accept_command("play", {path})` → start playback
- `accept_command("stop", {})` → stop playback
- `accept_command("pick", {})` → open file picker, start playback
- `get_state()` → `{ playing: true, path: "...", fps: 30, progress: 0.5 }`
- `publishes()` → `[TopicDeclaration { name: "video.frame", data_type: Image }]` (future: other adapters can subscribe to video frames)

**Rendering bridge:** VideoAdapter needs to produce GPU-textured quads, not just colored rects. Extend `RenderOutput`:
```rust
pub struct RenderOutput {
    pub quads: Vec<QuadData>,
    pub text_segments: Vec<TextData>,
    pub textures: Vec<TextureQuad>,  // NEW: textured quads with raw RGBA data or GPU texture handle
}
```

**Problem:** Adapters shouldn't own GPU resources directly (they implement `Send`). The GPU texture lives on the render thread. Solution: adapter produces raw RGBA frame data in `RenderOutput`; the render loop uploads it to the GPU texture via `VideoRenderer`. The adapter doesn't touch wgpu.

**Files modified:**
- `crates/phantom-app/src/adapters/video.rs` — new
- `crates/phantom-adapter/src/adapter.rs` — extend RenderOutput
- `crates/phantom-app/src/render.rs` — video renders into adapter's rect, not centered fullscreen
- `crates/phantom-app/src/commands.rs` — `video` command creates VideoAdapter via coordinator

### 2.2 `AgentAdapter` (new file: `crates/phantom-app/src/adapters/agent.rs`)

```rust
struct AgentAdapter {
    pane: AgentPane,
    // Subscribes to terminal.output for context
}
```

**Key trait methods:**
- `spatial_preference()` → `SpatialPreference::simple(60, 10).with_priority(1.5)` (slightly higher than terminal)
- `render(rect)` → render agent output text into rect (currently hardcoded in render_overlay.rs)
- `accept_command("prompt", {text})` → spawn new agent task
- `get_state()` → `{ task: "...", status: "working", output_lines: 42 }`
- `subscribes_to()` → `["terminal.output"]` — agent sees terminal errors
- `on_message(msg)` → if terminal output contains errors, agent can proactively respond
- `publishes()` → `["agent.output"]` — other adapters can see agent responses

### 2.3 `MonitorAdapter` (new file: `crates/phantom-app/src/adapters/monitor.rs`)

```rust
struct MonitorAdapter {
    sysmon: SysmonHandle,
    appmon_metrics: Option<AppMetrics>,
    mode: MonitorMode,  // System, App, Both
}
```

**Key trait methods:**
- `is_visual()` → `true`
- `spatial_preference()` → `SpatialPreference { min_size: (30, 8), preferred_size: (40, 12), priority: 0.5 }` (lower priority, takes leftover space)
- `render(rect)` → render monitor panel into rect
- `publishes()` → `["system.metrics"]` — brain and other adapters can subscribe to CPU/memory/disk events
- `accept_command("toggle_sys", {})` → show/hide system panel
- `accept_command("toggle_app", {})` → show/hide app diagnostics

### 2.4 Verification

- Video plays in a pane, not fullscreen. Can be resized. Aspect ratio preserved.
- Agent output renders in a pane, not hardcoded overlay. Can be split alongside terminal.
- Monitor panels render in a pane. Can be hidden/shown.
- All adapters visible in `AppRegistry`. Brain can query `get_state()` on any.
- MCP can call `accept_command()` on any adapter by ID.

---

## Phase 3: Spatial Negotiation + Floating Panes

**Goal:** Layout engine respects adapter preferences. Panes are resizable and floatable.

### 3.1 Integrate SpatialPreference into LayoutEngine

**File:** `crates/phantom-ui/src/layout.rs`

New method:
```rust
pub fn add_pane_with_preference(&mut self, pref: &SpatialPreference) -> Result<PaneId> {
    let style = Style {
        flex_grow: pref.priority,
        min_size: Size {
            width: Dimension::Length(pref.min_size.0 as f32 * cell_w),
            height: Dimension::Length(pref.min_size.1 as f32 * cell_h),
        },
        aspect_ratio: pref.aspect_ratio,
        ..default_pane_style()
    };
    let node = self.tree.new_leaf(style)?;
    self.tree.add_child(self.content, node)?;
    Ok(PaneId(node))
}
```

Coordinator calls `add_pane_with_preference()` during adapter registration, passing the adapter's `spatial_preference()`.

### 3.2 Floating Panes

Add a `FloatingPane` concept to the layout engine:
```rust
pub struct FloatingPane {
    pub pane_id: PaneId,
    pub rect: Rect,         // absolute pixel position (user-draggable)
    pub pinned: bool,       // always-on-top
    pub z_order: i32,
}
```

Floating panes are NOT in the Taffy tree. They have absolute pixel positions, rendered in the overlay pass (post-CRT, crisp). Users drag to move/resize.

**Adapter opt-in:** New field in `SpatialPreference`:
```rust
pub floating: bool,         // request floating mode
pub dock: Option<DockEdge>, // snap to Top/Bottom/Left/Right edge
```

**Input handling:**
- Mouse drag on floating pane border → resize
- Mouse drag on floating pane title → move
- Double-click title → toggle float/tile
- Keybind (e.g., Ctrl+Shift+F) → float/unfloat focused pane

### 3.3 Pop-out Windows

winit supports multiple windows. A pop-out pane gets its own OS window:
```rust
pub fn pop_out_pane(&mut self, app_id: AppId, event_loop: &ActiveEventLoop) {
    // Create new winit Window
    // Move adapter's scene subtree to new window's render context
    // Adapter still registered in coordinator, bus messages still flow
}
```

This is the most complex feature. Defer to Phase 3b if needed.

### 3.4 Verification

- Video pane opens with correct aspect ratio (16:9), resizable, maintains ratio
- Terminal panes resize when dragged
- Sysmon floats in corner, pinned on top
- Agent pane can be floated over the terminal
- Console command: `float` / `tile` toggles focused pane

---

## Phase 4: Bus Wiring + Piping

**Goal:** Adapters publish real data. Other adapters subscribe and react. Users can create pipes between adapters.

### 4.1 Wire Real Bus Traffic

Currently bus has 3 topics and 0 subscribers. After Phase 1-2:
- `terminal.output` — published by TerminalAdapter on new PTY data (payload: last N lines)
- `terminal.error` — published when semantic scan detects errors
- `agent.output` — published by AgentAdapter on new text deltas
- `agent.event` — published on agent status change (done/failed)
- `video.frame` — published by VideoAdapter (metadata, not raw pixels)
- `system.metrics` — published by MonitorAdapter every 2 seconds

### 4.2 Auto-Subscription

`AppCoordinator::register_adapter()` reads `subscribes_to()` and auto-subscribes:
- AgentAdapter subscribes to `terminal.output` → sees errors → can proactively fix
- Brain subscribes to ALL topics → unified awareness
- Console subscribes to ALL topics → shows live feed

### 4.3 User-Created Pipes

Console command: `pipe <source_adapter> <target_adapter>`

Implementation:
```rust
// In AppCoordinator:
pub fn create_pipe(&mut self, source: AppId, target: AppId) {
    // Find source's publish topics
    // Subscribe target to those topics
    // On next bus drain, target receives source's messages via on_message()
}
```

Visual indicator: glowing line between piped panes in the scene graph.

### 4.4 Verification

- `pipe terminal.0 agent.0` — agent receives terminal output
- `pipe system.metrics agent.0` — agent monitors system health
- Console shows live bus traffic
- Brain receives all events, can correlate across adapters

---

## Phase 5: AI-First Command Routing

**Goal:** Brain becomes the orchestrator. Natural language commands route to the right adapter.

### 5.1 Brain Adapter Discovery

Brain queries `AppCoordinator` for all running adapters:
```rust
let adapters = coordinator.registry.all_running();
for id in adapters {
    let state = coordinator.get_state(id);
    brain.observe(AdapterObservation { adapter_id: id, state });
}
```

### 5.2 New AiAction: CommandAdapter

```rust
AiAction::CommandAdapter {
    adapter_id: AppId,
    command: String,
    args: serde_json::Value,
}
```

Brain can now issue commands to any adapter: "play that video", "spawn an agent to fix that error", "show system metrics".

### 5.3 New AiAction: CreateAdapter / DestroyAdapter

```rust
AiAction::CreateAdapter { adapter_type: String, config: Value }
AiAction::DestroyAdapter { adapter_id: AppId }
AiAction::CreatePipe { source: AppId, target: AppId }
```

Brain can dynamically create/destroy adapters and create pipes between them.

### 5.4 MCP Dynamic Tool Registration

MCP server queries coordinator for all adapters and their `accept_command()` capabilities:
```
phantom.adapter.terminal.0.write  { text: "cargo build" }
phantom.adapter.video.0.play      { path: "/tmp/video.mp4" }
phantom.adapter.agent.0.prompt    { text: "fix the build" }
phantom.adapter.monitor.0.toggle  {}
```

Tools are registered dynamically — no more hardcoded dispatch table.

### 5.5 Verification

- Brain observes terminal error → automatically spawns agent to fix it
- Brain creates pipe: terminal → agent → terminal (agent fixes errors, writes fix to terminal)
- MCP lists all adapter tools dynamically
- Natural language in console routes to correct adapter: "play a video" → brain → VideoAdapter

---

## Critical Files

### New Files
| File | Purpose |
|------|---------|
| `crates/phantom-app/src/coordinator.rs` | AppCoordinator — registry + bus + pane mapping |
| `crates/phantom-app/src/adapters/mod.rs` | Adapter module |
| `crates/phantom-app/src/adapters/terminal.rs` | TerminalAdapter |
| `crates/phantom-app/src/adapters/video.rs` | VideoAdapter |
| `crates/phantom-app/src/adapters/agent.rs` | AgentAdapter |
| `crates/phantom-app/src/adapters/monitor.rs` | MonitorAdapter |

### Modified Files
| File | Change |
|------|--------|
| `crates/phantom-adapter/src/adapter.rs` | Extend RenderOutput with grid + texture data |
| `crates/phantom-adapter/src/spatial.rs` | Add `floating`, `dock` fields |
| `crates/phantom-app/src/app.rs` | Add coordinator, migrate pane ownership |
| `crates/phantom-app/src/update.rs` | Replace per-component loops with `coordinator.update_all()` |
| `crates/phantom-app/src/render.rs` | Replace hardcoded render paths with coordinator-driven dispatch |
| `crates/phantom-app/src/render_overlay.rs` | Adapters render themselves; remove hardcoded agent/monitor panels |
| `crates/phantom-app/src/input.rs` | Route input through coordinator focus |
| `crates/phantom-app/src/commands.rs` | Commands create/destroy adapters via coordinator |
| `crates/phantom-ui/src/layout.rs` | Add `add_pane_with_preference()`, floating pane support |
| `crates/phantom-brain/src/events.rs` | Add AdapterObservation, CommandAdapter actions |
| `crates/phantom-mcp/src/listener.rs` | Dynamic tool registration from adapter manifests |

---

## Execution Order

1. **Phase 1.1** — `AppCoordinator` + pane mapping (foundation, ~200 lines)
2. **Phase 1.2** — `TerminalAdapter` (biggest adapter, ~400 lines)
3. **Phase 1.3** — Wire into update/render loops, keep old path as fallback
4. **Phase 2.1** — `VideoAdapter` (video in a pane, not fullscreen, ~200 lines)
5. **Phase 2.2** — `AgentAdapter` (~150 lines)
6. **Phase 2.3** — `MonitorAdapter` (~150 lines)
7. **Phase 3.1** — SpatialPreference in layout engine (~100 lines)
8. **Phase 3.2** — Floating panes (~200 lines)
9. **Phase 4.1-4.3** — Bus wiring + piping (~200 lines)
10. **Phase 5.1-5.4** — AI routing + dynamic MCP (~300 lines)

Total: ~1900 lines of new code, ~500 lines of migration/deletion.

---

## Verification Plan

After each phase, test via MCP:

```bash
# Phase 1: Terminal adapter works
phantom.command "split horizontal"  # creates new TerminalAdapter
phantom.send_key "l" "s" "Enter"    # type in focused pane
phantom.read_output                  # verify output through adapter

# Phase 2: Video in a pane
phantom.command "video"             # opens picker, creates VideoAdapter in a pane
phantom.screenshot                   # verify video renders in pane rect, not fullscreen

# Phase 3: Floating
phantom.command "float"             # floats focused pane
phantom.screenshot                   # verify floating pane overlay

# Phase 4: Piping
phantom.command "pipe terminal.0 agent.0"  # create pipe
# trigger error in terminal, verify agent receives it

# Phase 5: AI routing
phantom.command "agent fix the build error"  # brain routes to correct adapters
```

Also: `cargo test -p phantom-app -p phantom-adapter` after every phase.
