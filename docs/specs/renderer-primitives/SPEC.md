# Renderer Primitives — Spec

Three rendering primitives are added to `phantom-renderer` to unblock adapter
rewrites that target the design intent in `docs/mockups/system.css` /
`docs/mockups/apps.html`. Adapter rewrites are out of scope here.

## Primitives

1. **rounded_rect** — solid-fill rectangle with anti-aliased rounded corners.
2. **glow** — soft radial halo around an axis-aligned rectangle, suitable for
   cursor halos, focus rings, and `box-shadow`-style ambient lighting.
3. **gradient_fill** — vertical two-stop linear gradient inside an axis-aligned
   rectangle.

## Public API

All three live in `phantom_renderer::primitives` and produce GPU-ready
`QuadInstance` records compatible with the existing `QuadRenderer` pipeline.
No changes to `QuadInstance` (its 36-byte size is asserted in
`tests/clip_rect.rs::quad_instance_size_is_unchanged_from_baseline`).

```rust
pub struct Rect { pub x: f32, pub y: f32, pub w: f32, pub h: f32 }

pub struct PrimitivesBatch { /* opaque */ }

impl PrimitivesBatch {
    pub fn new() -> Self;
    pub fn draw_rounded_rect(&mut self, rect: Rect, radius: f32, color: [f32; 4]);
    pub fn draw_glow(&mut self, rect: Rect, color: [f32; 4], radius: f32);
    pub fn draw_gradient_rect(&mut self, rect: Rect, top: [f32; 4], bottom: [f32; 4]);
    pub fn quads(&self) -> &[QuadInstance];
    pub fn clear(&mut self);
}
```

## Implementation Strategy

The existing `QuadRenderer` already supports anti-aliased rounded rectangles
via an SDF in the fragment shader (`QuadInstance::border_radius`). The new
primitives layer on top of that single pipeline, avoiding a new shader and
keeping the renderer surface area small:

* `rounded_rect` — emits one `QuadInstance` with `border_radius` set.
* `glow` — emits a small fan of concentric `QuadInstance`s at decreasing
  alpha, expanding outward by `radius`. The existing SDF rounded-rect fade
  on the outer halo provides the falloff so the halo reads as a soft glow.
* `gradient_fill` — emits a vertical stack of thin `QuadInstance` stripes,
  each tinted with a linear interpolation between `top` and `bottom`. A small
  fixed stripe count is enough for visually-smooth gradients at typical UI
  sizes (header bars, terminal bodies, button backgrounds).

This is intentionally a CPU-side composition approach: zero shader churn, no
new pipeline, no new bind group. Future iterations can swap in a dedicated
gradient/glow shader without changing the public API.

## Non-Goals (this PR)

* No adapter rewrites. Adapter changes are tracked separately.
* No multi-stop gradients. Two-stop linear vertical is sufficient for the
  adapter mockup; multi-stop is a follow-up.
* No directional / angled gradients.
* No box-shadow with non-zero offset. The first iteration of `draw_glow` is a
  centered halo. Offset shadows are a follow-up.
* No live-reload of the new shader files; the `shaders/*.wgsl` companions
  ship as design documentation for the eventual dedicated pipelines.
