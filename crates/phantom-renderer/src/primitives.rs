// High-level rendering primitives built on top of the existing QuadRenderer
// pipeline. The three primitives unblock adapter rewrites that target the
// `docs/mockups/system.css` design intent — rounded card-style backgrounds,
// gradient terminal bodies, and cursor / focus-ring halos.
//
// Design rationale
// ----------------
// The existing fragment shader in `quads.rs` already implements an
// anti-aliased SDF rounded rectangle keyed by `QuadInstance::border_radius`.
// Rather than introduce new pipelines, bind groups, and WGSL modules, the
// primitives in this file compose `QuadInstance`s on the CPU. That keeps the
// renderer surface area small and avoids touching the byte-stable
// `QuadInstance` layout (its 36-byte size is asserted by
// `tests/clip_rect.rs::quad_instance_size_is_unchanged_from_baseline`).
//
// * `rounded_rect` — one `QuadInstance` with `border_radius` set.
// * `glow`         — a fan of concentric `QuadInstance`s with decreasing
//                    alpha, expanding outward by `radius`. The outer-halo
//                    SDF fade in the existing shader provides the falloff.
// * `gradient`     — a vertical stack of thin `QuadInstance` stripes tinted
//                    by linear interpolation between two stop colors.
//
// Companion WGSL files live in `shaders/rounded_rect.wgsl`,
// `shaders/glow.wgsl`, and `shaders/gradient.wgsl`. They ship as design
// documentation for the eventual dedicated pipelines and are not loaded by
// the current implementation.

use crate::quads::QuadInstance;

/// Axis-aligned rectangle in pixel coordinates, top-left origin.
///
/// The coordinate system matches `QuadInstance::pos` / `QuadInstance::size`,
/// so a primitive that produces `QuadInstance`s from a `Rect` does not need
/// to do any coordinate-space remapping.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    /// Construct a rect from explicit pixel dimensions.
    #[must_use]
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
}

/// Number of concentric halo layers emitted by `draw_glow`.
///
/// More layers means smoother falloff at the cost of more quads. Eight is a
/// reasonable balance — the existing SDF anti-alias on each layer already
/// hides individual layer boundaries at typical UI sizes.
const GLOW_LAYERS: u32 = 8;

/// Number of horizontal stripes emitted by `draw_gradient_rect`.
///
/// Sixteen is enough to make a vertical gradient read as continuous across a
/// terminal-body-sized region (~600 px tall). The CPU cost is negligible
/// because the existing instanced pipeline batches all stripes into the
/// same draw call.
const GRADIENT_STRIPES: u32 = 16;

/// Accumulates primitive draw calls into a flat `Vec<QuadInstance>` ready to
/// hand to `QuadRenderer::prepare`.
///
/// The batch is single-pane scoped: clear it between frames (or between
/// passes) with `clear`. The output order matches the call order, so callers
/// that need painter-style layering should call primitives back-to-front.
#[derive(Default)]
pub struct PrimitivesBatch {
    quads: Vec<QuadInstance>,
}

impl PrimitivesBatch {
    /// Construct an empty batch.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drain all queued quads, leaving the batch empty.
    pub fn clear(&mut self) {
        self.quads.clear();
    }

    /// Borrow the accumulated quads — call sites pass this slice to
    /// `QuadRenderer::prepare`.
    #[must_use]
    pub fn quads(&self) -> &[QuadInstance] {
        &self.quads
    }

    /// Number of quads currently in the batch.
    #[must_use]
    pub fn len(&self) -> usize {
        self.quads.len()
    }

    /// True when the batch holds no quads.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.quads.is_empty()
    }

    // -----------------------------------------------------------------
    // Primitive #1 — rounded_rect
    // -----------------------------------------------------------------

    /// Append a solid-fill rectangle with anti-aliased rounded corners.
    ///
    /// Implemented as a single `QuadInstance` with `border_radius`. The
    /// existing SDF in `quads.rs` handles edge anti-aliasing.
    ///
    /// A `radius` of zero produces a sharp-cornered rectangle, equivalent to
    /// emitting a `QuadInstance` directly.
    pub fn draw_rounded_rect(&mut self, rect: Rect, radius: f32, color: [f32; 4]) {
        let r = radius.max(0.0).min(rect.w * 0.5).min(rect.h * 0.5);
        self.quads.push(QuadInstance {
            pos: [rect.x, rect.y],
            size: [rect.w, rect.h],
            color,
            border_radius: r,
        });
    }

    // -----------------------------------------------------------------
    // Primitive #2 — glow
    // -----------------------------------------------------------------

    /// Append a soft halo around `rect`.
    ///
    /// The halo extends outward by `radius` pixels on every side, with alpha
    /// falling off quadratically toward the outer edge. The implementation
    /// stacks `GLOW_LAYERS` concentric rounded rectangles at decreasing alpha;
    /// the existing SDF anti-alias on each layer smooths the falloff so the
    /// halo reads as a continuous glow rather than discrete bands.
    ///
    /// `color` is the inner (strongest) halo color. The alpha channel scales
    /// each layer — pass a low alpha (e.g. `0.4`) for a subtle ambient glow,
    /// higher (`0.8`+) for a bright cursor halo.
    pub fn draw_glow(&mut self, rect: Rect, color: [f32; 4], radius: f32) {
        if radius <= 0.0 {
            // No halo requested; emit a single sharp-edged base layer so the
            // call is still a no-op-safe primitive.
            self.quads.push(QuadInstance {
                pos: [rect.x, rect.y],
                size: [rect.w, rect.h],
                color,
                border_radius: 0.0,
            });
            return;
        }

        let layers = GLOW_LAYERS as f32;
        for i in 0..GLOW_LAYERS {
            // `t` ramps from 0.0 (innermost layer matches `rect`) to 1.0
            // (outermost layer expanded by `radius` on each side).
            let t = (i as f32) / (layers - 1.0).max(1.0);
            let expand = radius * t;

            // Quadratic falloff — gives a softer, more gaussian-looking glow
            // than a linear ramp without needing a real blur pass.
            let falloff = 1.0 - t * t;
            let layer_alpha = color[3] * falloff / layers;

            self.quads.push(QuadInstance {
                pos: [rect.x - expand, rect.y - expand],
                size: [rect.w + 2.0 * expand, rect.h + 2.0 * expand],
                color: [color[0], color[1], color[2], layer_alpha],
                // Outer layers get progressively rounder so the glow reads
                // as a smooth halo rather than a stacked rectangle.
                border_radius: expand + (rect.w.min(rect.h) * 0.25),
            });
        }
    }

    // -----------------------------------------------------------------
    // Primitive #3 — gradient_fill
    // -----------------------------------------------------------------

    /// Append a vertical two-stop linear gradient inside `rect`.
    ///
    /// The gradient interpolates linearly in RGBA between `top` (at the top
    /// edge) and `bottom` (at the bottom edge). The implementation emits
    /// `GRADIENT_STRIPES` thin horizontal slices, each tinted by the
    /// midpoint color of its band. At typical UI heights the stripes blend
    /// visually into a continuous gradient.
    pub fn draw_gradient_rect(&mut self, rect: Rect, top: [f32; 4], bottom: [f32; 4]) {
        if rect.h <= 0.0 || rect.w <= 0.0 {
            return;
        }

        let stripes = GRADIENT_STRIPES;
        let stripe_h = rect.h / (stripes as f32);

        for i in 0..stripes {
            // Sample color at the midpoint of this stripe for a smoother
            // visual ramp than sampling at the top edge.
            let t = ((i as f32) + 0.5) / (stripes as f32);
            let color = lerp_rgba(top, bottom, t);

            self.quads.push(QuadInstance {
                pos: [rect.x, rect.y + (i as f32) * stripe_h],
                // Add a tiny overlap so anti-aliased seams between stripes do
                // not show as faint horizontal lines on fractional-DPI
                // displays. The extra fraction is well below a pixel.
                size: [rect.w, stripe_h + 0.5],
                color,
                border_radius: 0.0,
            });
        }
    }
}

/// Linear interpolation between two RGBA colors. `t` is clamped to `[0, 1]`.
fn lerp_rgba(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    let t = t.clamp(0.0, 1.0);
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
        a[3] + (b[3] - a[3]) * t,
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> Rect {
        Rect::new(10.0, 20.0, 100.0, 50.0)
    }

    #[test]
    fn batch_starts_empty() {
        let b = PrimitivesBatch::new();
        assert!(b.is_empty());
        assert_eq!(b.len(), 0);
        assert!(b.quads().is_empty());
    }

    #[test]
    fn rounded_rect_emits_single_quad() {
        let mut b = PrimitivesBatch::new();
        b.draw_rounded_rect(rect(), 12.0, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(b.len(), 1);
        let q = &b.quads()[0];
        assert_eq!(q.pos, [10.0, 20.0]);
        assert_eq!(q.size, [100.0, 50.0]);
        assert_eq!(q.color, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(q.border_radius, 12.0);
    }

    #[test]
    fn rounded_rect_clamps_radius_to_half_min_dim() {
        let mut b = PrimitivesBatch::new();
        // Rect is 100x50 — radius cannot exceed 25 (half of 50).
        b.draw_rounded_rect(rect(), 9999.0, [1.0, 1.0, 1.0, 1.0]);
        assert_eq!(b.quads()[0].border_radius, 25.0);
    }

    #[test]
    fn rounded_rect_negative_radius_is_clamped_to_zero() {
        let mut b = PrimitivesBatch::new();
        b.draw_rounded_rect(rect(), -10.0, [1.0; 4]);
        assert_eq!(b.quads()[0].border_radius, 0.0);
    }

    #[test]
    fn glow_emits_expected_layer_count() {
        let mut b = PrimitivesBatch::new();
        b.draw_glow(rect(), [0.5, 0.8, 1.0, 0.6], 20.0);
        assert_eq!(b.len(), GLOW_LAYERS as usize);
    }

    #[test]
    fn glow_layers_expand_outward_monotonically() {
        let mut b = PrimitivesBatch::new();
        b.draw_glow(rect(), [1.0, 1.0, 1.0, 1.0], 16.0);
        let quads = b.quads();
        let mut prev_size = quads[0].size[0];
        for q in &quads[1..] {
            assert!(
                q.size[0] >= prev_size,
                "glow layers must grow outward — got size {} after {}",
                q.size[0],
                prev_size,
            );
            prev_size = q.size[0];
        }
    }

    #[test]
    fn glow_outermost_alpha_is_lowest() {
        let mut b = PrimitivesBatch::new();
        b.draw_glow(rect(), [1.0, 1.0, 1.0, 1.0], 20.0);
        let quads = b.quads();
        let inner_alpha = quads[0].color[3];
        let outer_alpha = quads[quads.len() - 1].color[3];
        // Quadratic falloff drives the outermost layer to alpha 0; the
        // innermost layer keeps the full per-layer share of source alpha.
        assert!(
            inner_alpha > outer_alpha,
            "inner alpha {inner_alpha} must exceed outer alpha {outer_alpha}",
        );
    }

    #[test]
    fn glow_zero_radius_emits_single_layer() {
        let mut b = PrimitivesBatch::new();
        b.draw_glow(rect(), [1.0; 4], 0.0);
        assert_eq!(b.len(), 1);
        let q = &b.quads()[0];
        assert_eq!(q.pos, [10.0, 20.0]);
        assert_eq!(q.size, [100.0, 50.0]);
    }

    #[test]
    fn gradient_emits_expected_stripe_count() {
        let mut b = PrimitivesBatch::new();
        b.draw_gradient_rect(rect(), [1.0, 0.0, 0.0, 1.0], [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(b.len(), GRADIENT_STRIPES as usize);
    }

    #[test]
    fn gradient_top_stripe_is_near_top_color() {
        let mut b = PrimitivesBatch::new();
        b.draw_gradient_rect(rect(), [1.0, 0.0, 0.0, 1.0], [0.0, 0.0, 1.0, 1.0]);
        let first = b.quads()[0].color;
        // First stripe samples at t = 0.5 / N, so it is close to top.
        assert!(first[0] > 0.9, "first stripe red channel {} should be near 1", first[0]);
        assert!(first[2] < 0.1, "first stripe blue channel {} should be near 0", first[2]);
    }

    #[test]
    fn gradient_bottom_stripe_is_near_bottom_color() {
        let mut b = PrimitivesBatch::new();
        b.draw_gradient_rect(rect(), [1.0, 0.0, 0.0, 1.0], [0.0, 0.0, 1.0, 1.0]);
        let last = b.quads()[b.len() - 1].color;
        assert!(last[0] < 0.1, "last stripe red channel {} should be near 0", last[0]);
        assert!(last[2] > 0.9, "last stripe blue channel {} should be near 1", last[2]);
    }

    #[test]
    fn gradient_stripes_tile_full_height() {
        let mut b = PrimitivesBatch::new();
        let r = Rect::new(0.0, 0.0, 100.0, 100.0);
        b.draw_gradient_rect(r, [1.0; 4], [0.0; 4]);
        let quads = b.quads();
        // First stripe starts at y = 0.
        assert!((quads[0].pos[1] - 0.0).abs() < 1e-4);
        // Last stripe's bottom edge reaches the rect's bottom (within the
        // intentional half-pixel anti-seam overlap).
        let last = &quads[quads.len() - 1];
        let bottom_edge = last.pos[1] + last.size[1];
        assert!(
            bottom_edge >= 100.0,
            "gradient bottom edge {bottom_edge} must reach rect bottom 100.0",
        );
    }

    #[test]
    fn gradient_zero_height_emits_nothing() {
        let mut b = PrimitivesBatch::new();
        b.draw_gradient_rect(Rect::new(0.0, 0.0, 100.0, 0.0), [1.0; 4], [0.0; 4]);
        assert!(b.is_empty());
    }

    #[test]
    fn lerp_rgba_endpoints() {
        let a = [1.0, 0.0, 0.0, 1.0];
        let b = [0.0, 1.0, 0.0, 0.5];
        assert_eq!(lerp_rgba(a, b, 0.0), a);
        assert_eq!(lerp_rgba(a, b, 1.0), b);
        let mid = lerp_rgba(a, b, 0.5);
        assert!((mid[0] - 0.5).abs() < 1e-4);
        assert!((mid[1] - 0.5).abs() < 1e-4);
        assert!((mid[3] - 0.75).abs() < 1e-4);
    }

    #[test]
    fn lerp_rgba_clamps_t() {
        let a = [0.0; 4];
        let b = [1.0; 4];
        assert_eq!(lerp_rgba(a, b, -1.0), a);
        assert_eq!(lerp_rgba(a, b, 2.0), b);
    }

    #[test]
    fn clear_resets_batch() {
        let mut b = PrimitivesBatch::new();
        b.draw_rounded_rect(rect(), 4.0, [1.0; 4]);
        b.draw_glow(rect(), [1.0; 4], 8.0);
        b.draw_gradient_rect(rect(), [1.0; 4], [0.0; 4]);
        assert!(!b.is_empty());
        b.clear();
        assert!(b.is_empty());
    }

    #[test]
    fn three_primitives_can_be_combined_in_one_batch() {
        // Compile / shape test: all three primitives can be queued together
        // in painter order without panic, and the QuadInstance output is
        // contiguous and ready to hand to QuadRenderer::prepare.
        let mut b = PrimitivesBatch::new();
        b.draw_gradient_rect(rect(), [0.1, 0.1, 0.1, 1.0], [0.3, 0.3, 0.3, 1.0]);
        b.draw_glow(rect(), [0.4, 0.8, 1.0, 0.7], 12.0);
        b.draw_rounded_rect(rect(), 8.0, [1.0, 1.0, 1.0, 1.0]);
        let expected = (GRADIENT_STRIPES + GLOW_LAYERS + 1) as usize;
        assert_eq!(b.len(), expected);
        // Sanity: no NaN colors leak through.
        for q in b.quads() {
            for c in q.color {
                assert!(c.is_finite(), "quad color must be finite — got {c}");
            }
        }
    }
}
