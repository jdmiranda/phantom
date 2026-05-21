# Substrate

[← back to components index](README.md)

> The plumbing — the binary, IPC, the AppAdapter trait.

## Status

<span class="chip ok">shipping</span> — all five crates compile, ship, and are
exercised on every cold launch.

## What it does

The substrate is the layer everything else runs on. It owns:

- **`phantom`** — the top-level binary. Bootstraps winit, GPU init, calls
  `App::with_config_scaled`, runs the event loop, handles panic recovery.
- **`phantom-app`** — the orchestrator. Holds `AppCoordinator`, `LayoutEngine`,
  scene graph, theme, all running adapters. The single big struct that
  owns everything else.
- **`phantom-supervisor`** — the Erlang-style heartbeat watcher. Lives in
  its own process; spawns `phantom`, watches heartbeats, restarts on
  timeout. See [Flow 1 · Cold launch](../flows/01-cold-launch.md) (cold launch goes
  through the supervisor handshake) and `docs/research/supervisor-architecture.md`.
- **`phantom-protocol`** — the wire types shared between phantom + supervisor
  + the future federation. Pub-sub `Event` enum lives here. Also the typed
  bus topics.
- **`phantom-adapter`** — the `AppAdapter` trait family (`AppCore`,
  `Renderable`, `InputHandler`, `Commandable`, `BusParticipant`,
  `Lifecycled`, `Permissioned`). Plus `SpatialPreference`, `BusMessage`,
  `RenderOutput`, the typed shape every adapter speaks.

## Crates

### `phantom` <span class="chip ok">shipping</span>

The top-level binary. ~1500 LOC across `src/main.rs`, `src/loop_cli.rs`,
`src/auth_cli.rs`, `src/builder_cli.rs`, `src/fleet_cli.rs`,
`src/headless.rs`, `src/path_resolver.rs`. Owns the winit
`ApplicationHandler` and dispatches subcommands (`phantom`, `phantom loop
run`, `phantom auth`, `phantom builder`, `phantom fleet`).

### `phantom-app` <span class="chip ok">shipping</span>

The orchestrator. The largest crate in the workspace (~25k LOC). Holds:

- `AppCoordinator` — adapter registry + pane/scene mapping, runs the
  arbiter, drains bus outboxes.
- `App::update` — the per-frame tick: input → adapter updates →
  brain ticks → spawn drain → arbiter → render.
- `App::with_config_scaled` — the cold-launch boot constructor (Flow 1).

### `phantom-supervisor` <span class="chip ok">shipping</span>

A separate binary at `crates/phantom-supervisor/src/main.rs`. Forks
`phantom`, watches heartbeat at `/tmp/phantom-{pid}.sock`, SIGTERMs +
respawns on timeout. The user almost never interacts with it directly;
`scripts/phantom-loop-forever.sh` and `./run.sh` invoke it.

### `phantom-protocol` <span class="chip ok">shipping</span>

The wire types. Two files: `src/lib.rs` + `src/events.rs`. Defines:

- `Event` enum — 22+ variants for every cross-component message
  (`TerminalOutput`, `AgentSpawned`, `AgentTaskComplete`, `FastPathTaken`,
  `Custom`, etc.).
- `EventTopic` — routing category for the bus.
- Supervisor IPC envelopes.

### `phantom-adapter` <span class="chip ok">shipping</span>

The `AppAdapter` trait family. Split into focused sub-traits per the
Interface Segregation Principle:

- `AppCore` — required by all (app_type, is_alive, update, get_state).
- `Renderable` — visual adapters (render, spatial_preference,
  on_resize_propose).
- `InputHandler` — keyboard-receiving adapters.
- `Commandable` — adapters that accept commands from the AI.
- `BusParticipant` — pub-sub: publishes / subscribes_to / on_message /
  drain_outbox.
- `Lifecycled` — on_init / on_state_change / set_app_id / set_adapter_id.
- `Permissioned` — WASM sandbox boundary (future).

`SpatialPreference { min_size, preferred_size, max_size, aspect_ratio,
priority, internal_panes, internal_layout }` — the negotiation input the
arbiter consumes. Every adapter that wants visual real estate declares
this.

## Owns

- `AppCoordinator` — `pane_map`, `app_pane_map`, `scene_map`, `registry`,
  `cadences`, `lineage`, `dirty_adapters`, `floating`.
- `LayoutEngine` — wraps `taffy::TaffyTree`. Owns the chrome (tab_bar,
  content, status_bar) + the pane tree.
- `EventBus` — owns topic registry, subscriber lists.
- `AppAdapter` trait family — the trait surface every other component
  consumes.
- `Event` enum — single source for cross-component message shapes.

## Reads from

| Source | What |
|---|---|
| GPU / winit | window events, redraw requests |
| Supervisor socket | shutdown / restart commands |
| MCP listener socket | external tool / command dispatch |
| `~/.config/phantom/config.toml` | user config (theme, font, agent keys) |
| Adapter outboxes | per-frame drain into the bus |

## Writes to / publishes

| Target | What |
|---|---|
| Adapters (via `Coordinator`) | render rects, input events, commands |
| Bus subscribers | every `Event` topic |
| Supervisor | heartbeats every ~1s |
| `~/.local/share/phantom/history/` | session sidecar files (via persisters wired by App) |
| Filesystem | debug logs at `~/.config/phantom/phantom.log` |

## Decisions honoured

- [ADR-001 · Architecture decisions](../decisions/001-architecture.md) — the
  two-process model, the AppAdapter trait, the supervisor's
  responsibilities.
- [ADR-003 · App lifecycle + pub-sub](../decisions/003-pubsub.md) — the
  AppAdapter sub-trait split, the BusParticipant protocol.

## Open gaps

- [gap-capability-class-propagation](../gaps.md#gap-capability-class-propagation)
  (cross-cutting; affects `phantom-protocol`'s potential role as the
  canonical home of `CapabilityClass`).

## Source files

| Concept | File |
|---|---|
| Binary entry | [`crates/phantom/src/main.rs`](../../../../crates/phantom/src/main.rs) |
| Loop CLI | [`crates/phantom/src/loop_cli.rs`](../../../../crates/phantom/src/loop_cli.rs) |
| App orchestrator | [`crates/phantom-app/src/app.rs`](../../../../crates/phantom-app/src/app.rs) |
| Per-frame tick | [`crates/phantom-app/src/update.rs`](../../../../crates/phantom-app/src/update.rs) |
| Coordinator | [`crates/phantom-app/src/coordinator.rs`](../../../../crates/phantom-app/src/coordinator.rs) |
| Supervisor | [`crates/phantom-supervisor/src/main.rs`](../../../../crates/phantom-supervisor/src/main.rs) |
| Event enum | [`crates/phantom-protocol/src/events.rs`](../../../../crates/phantom-protocol/src/events.rs) |
| AppAdapter trait | [`crates/phantom-adapter/src/lib.rs`](../../../../crates/phantom-adapter/src/lib.rs) |
