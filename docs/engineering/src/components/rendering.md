# Rendering

[← back to components index](README.md)

> GPU pipeline + Taffy layout + design tokens.

## Status

<span class="chip ok">shipping</span> — all three crates ship and are
exercised every frame.

## What it does

The rendering ring takes the per-frame output of the adapter swarm (quads,
text, grids) and turns it into pixels on the GPU. Owns the WGSL shader
pipeline, the glyph atlas, the Taffy layout solver, and the design
tokens that style every chrome surface.

## Crates

### `phantom-renderer` <span class="chip ok">shipping</span>

The GPU pipeline. ~5k LOC.

- `GpuContext` — wgpu device + queue + surface for the winit window.
- `GlyphAtlas` + `ColorGlyphAtlas` — etagere bin-packed glyph cache (text
  + emoji).
- `TextRenderer`, `QuadRenderer`, `GridRenderer` — three pipelines.
- `PostFxPipeline` — CRT post-processing (scanlines, bloom, chromatic
  aberration, curvature, vignette, noise — all opt-in shader params).
- `VideoRenderer` — frame upload pipeline for [phantom-vision](extensibility.md)
  + the video adapter.

### `phantom-ui` <span class="chip ok">shipping</span>

Taffy layout + design tokens + widgets.

- `LayoutEngine` — wraps `taffy::TaffyTree` with phantom-specific chrome
  (tab bar / content / status bar tri-panel) and a pane API (`add_pane`,
  `split_horizontal`, `split_vertical`, `set_flex_grow`,
  `set_pane_size_constraints`, `get_pane_rect`).
- `LayoutArbiter` — greedy 3-phase allocator (see Flow 1's
  [arbiter-leftover gap](../gaps.md#gap-arbiter-leftover)).
- `Tokens` — semantic color roles. Themed via the `Theme` struct
  (phosphor / amber / ice / blood + the engineering-docs additions
  vapor + cyber).
- Widget primitives: `MessageBlock`, `Scrollbar`, `FocusRing`,
  `NotificationBanner`, `SearchBar`, `KeybindHelp`, etc.
- `KeybindRegistry` — action ↔ key binding table.
- `CursorBlink` — clock-driven cursor blink suppressor so rapid-redraw
  TUIs don't strobe.

### `phantom-scene` <span class="chip ok">shipping</span>

Retained scene graph.

- `SceneTree` — node hierarchy with z-order + dirty-bit tracking.
- `NodeKind` — pane, overlay, decoration, etc.
- `Clock` + `Cadence` — frame-rate-adjusted tick scheduler so adapters
  with different update rates (terminal at 60Hz, monitor at 1Hz) coexist.
- `DtClamp` — caps frame dt to a maximum so animations don't explode
  after a debugger pause or OS suspend.

## Owns

- GPU device + queue
- Glyph atlas + color glyph atlas
- WGSL shader pipelines (`text.wgsl`, `text_color.wgsl`, `crt.wgsl`, etc.)
- Taffy layout tree
- Arbiter allocation state
- Scene graph nodes + dirty bits
- Frame clock
- Design tokens + theme palettes

## Reads from

| Source | What |
|---|---|
| Adapters (via `Coordinator::render_all`) | per-adapter `RenderOutput` (quads + text + grid + scroll + selection) |
| Theme config | active palette + shader params |
| Cell metrics (live font shaping) | cols/rows per pane |

## Writes to / publishes

| Target | What |
|---|---|
| GPU surface | pixels |
| Coordinator | computed pane rects (back-pressure if a pane requests resize) |
| Bus | nothing directly; rendering is one-way |

## Decisions honoured

- [ADR-001 · Architecture decisions](../decisions/001-architecture.md) — GPU-
  first rendering, wgpu cross-platform backend.
- [ADR-005 · Keystroke glitch FX](../decisions/005-keystroke-fx.md) — the
  per-keystroke shader overlay lives in the renderer's PostFx pass.

## Open gaps

- [gap-arbiter-leftover](../gaps.md#gap-arbiter-leftover) — Phase 3
  redistribution misses unbounded adapters; lives in `phantom-ui`.

## Source files

| Concept | File |
|---|---|
| GPU context | [`crates/phantom-renderer/src/gpu.rs`](../../../../crates/phantom-renderer/src/gpu.rs) |
| Layout engine | [`crates/phantom-ui/src/layout.rs`](../../../../crates/phantom-ui/src/layout.rs) |
| Arbiter | [`crates/phantom-ui/src/arbiter.rs`](../../../../crates/phantom-ui/src/arbiter.rs) |
| Tokens | [`crates/phantom-ui/src/tokens.rs`](../../../../crates/phantom-ui/src/tokens.rs) |
| Themes | [`crates/phantom-ui/src/themes.rs`](../../../../crates/phantom-ui/src/themes.rs) |
| Scene graph | [`crates/phantom-scene/src/lib.rs`](../../../../crates/phantom-scene/src/lib.rs) |
