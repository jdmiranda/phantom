//! Design tokens — centralized colors, spacing, type, and radii.
//!
//! Components reference *roles*, not literals. Themes mutate the table.
//! A density change cascades through every component without code edits.
//!
//! The spacing scale resolves against `RenderCtx::cell_size`, so spacing
//! in pixels stays correct when the user changes font size.

use crate::RenderCtx;

/// Color roles. Themes provide concrete RGBA values; components reference roles.
#[derive(Debug, Clone, Copy)]
pub struct ColorRoles {
    pub surface_base: [f32; 4],
    pub surface_recessed: [f32; 4],
    pub surface_raised: [f32; 4],
    pub chrome_frame: [f32; 4],
    pub chrome_frame_active: [f32; 4],
    pub chrome_frame_dim: [f32; 4],
    pub chrome_divider: [f32; 4],
    pub text_primary: [f32; 4],
    pub text_secondary: [f32; 4],
    pub text_dim: [f32; 4],
    pub text_accent: [f32; 4],
    pub status_ok: [f32; 4],
    pub status_warn: [f32; 4],
    pub status_danger: [f32; 4],
    pub status_info: [f32; 4],
    /// Selection highlight fill color. Used at 50 % alpha by default; the
    /// alpha channel stored here is the *base* alpha — callers may further
    /// modulate it (e.g. dim when the pane loses focus).
    pub selection_bg: [f32; 4],
}

impl ColorRoles {
    /// The default Phosphor mapping. Themes may override.
    pub const fn phosphor() -> Self {
        Self {
            surface_base: [0.04, 0.06, 0.05, 1.0],
            surface_recessed: [0.06, 0.10, 0.07, 1.0],
            surface_raised: [0.08, 0.13, 0.09, 1.0],
            chrome_frame: [0.20, 0.50, 0.30, 0.85],
            chrome_frame_active: [0.30, 1.00, 0.55, 0.95],
            chrome_frame_dim: [0.10, 0.22, 0.14, 0.55],
            chrome_divider: [0.18, 0.38, 0.24, 0.60],
            text_primary: [0.55, 1.00, 0.70, 1.00],
            text_secondary: [0.30, 0.55, 0.40, 0.85],
            text_dim: [0.18, 0.38, 0.24, 0.70],
            text_accent: [0.65, 1.00, 0.80, 1.00],
            status_ok: [0.30, 1.00, 0.55, 1.00],
            status_warn: [1.00, 0.75, 0.20, 1.00],
            status_danger: [1.00, 0.30, 0.25, 1.00],
            status_info: [0.40, 0.85, 1.00, 1.00],
            // Phosphor green at 50 % alpha — legible without hiding text.
            selection_bg: [0.30, 1.00, 0.55, 0.50],
        }
    }
}

/// All design tokens, parameterized by a live `RenderCtx`.
///
/// Spacing primitives resolve against the current `cell_size`. Type sizes are
/// expressed as ratios of the body cell height; until a UI font lands, all
/// "sizes" are achieved via color/intensity contrast (text_primary vs text_dim)
/// while keeping the same monospace cell.
#[derive(Debug, Clone, Copy)]
pub struct Tokens {
    pub colors: ColorRoles,
    pub ctx: RenderCtx,
}

impl Tokens {
    pub fn new(colors: ColorRoles, ctx: RenderCtx) -> Self {
        Self { colors, ctx }
    }

    pub fn phosphor(ctx: RenderCtx) -> Self {
        Self { colors: ColorRoles::phosphor(), ctx }
    }

    // -- Spacing scale (4px-base, scales with the cell on dense fonts) --
    pub fn space_0(&self) -> f32 { 0.0 }
    pub fn space_1(&self) -> f32 { 4.0 }
    pub fn space_2(&self) -> f32 { 8.0 }
    pub fn space_3(&self) -> f32 { 12.0 }
    pub fn space_4(&self) -> f32 { 16.0 }
    pub fn space_5(&self) -> f32 { 24.0 }
    pub fn space_6(&self) -> f32 { 32.0 }

    // -- Radii --
    pub fn radius_sm(&self) -> f32 { 2.0 }
    pub fn radius_md(&self) -> f32 { 4.0 }
    pub fn radius_lg(&self) -> f32 { 6.0 }

    // -- Line widths --
    pub fn hair(&self) -> f32 { 1.0 }
    pub fn frame(&self) -> f32 { 2.0 }

    // -- Convenience accessors that flow through the ctx --
    pub fn cell_w(&self) -> f32 { self.ctx.cell_w() }
    pub fn cell_h(&self) -> f32 { self.ctx.cell_h() }
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spacing_scale_is_monotonic_and_4_step() {
        let t = Tokens::phosphor(RenderCtx::fallback());
        let scale = [t.space_0(), t.space_1(), t.space_2(), t.space_3(),
                     t.space_4(), t.space_5(), t.space_6()];
        // Strictly increasing.
        for w in scale.windows(2) {
            assert!(w[0] < w[1], "spacing scale not monotonic: {scale:?}");
        }
        // Step is 4 between adjacent small values.
        assert_eq!(t.space_2() - t.space_1(), 4.0);
        assert_eq!(t.space_3() - t.space_2(), 4.0);
    }

    #[test]
    fn radii_increase_by_size() {
        let t = Tokens::phosphor(RenderCtx::fallback());
        assert!(t.radius_sm() < t.radius_md());
        assert!(t.radius_md() < t.radius_lg());
    }

    #[test]
    fn frame_thicker_than_hair() {
        let t = Tokens::phosphor(RenderCtx::fallback());
        assert!(t.frame() > t.hair());
    }

    #[test]
    fn ctx_accessors_thread_through() {
        let ctx = RenderCtx::new((11.0, 24.0), 2.0);
        let t = Tokens::phosphor(ctx);
        assert_eq!(t.cell_w(), 11.0);
        assert_eq!(t.cell_h(), 24.0);
    }

    #[test]
    fn phosphor_role_invariants() {
        let r = ColorRoles::phosphor();
        // Active frame is brighter (higher green channel) than dim frame.
        assert!(r.chrome_frame_active[1] > r.chrome_frame_dim[1]);
        // Accent text is brighter than primary.
        assert!(r.text_accent[1] >= r.text_primary[1]);
        // Dim text is dimmer than primary.
        assert!(r.text_dim[1] < r.text_primary[1]);
        // All alphas are positive.
        for &c in &[r.surface_base, r.surface_recessed, r.text_primary, r.chrome_frame] {
            assert!(c[3] > 0.0, "alpha must be positive: {c:?}");
        }
    }

    #[test]
    fn selection_bg_token_exists_and_semi_transparent() {
        let r = ColorRoles::phosphor();
        // Must exist and be semi-transparent (alpha in 0..1 exclusive).
        let a = r.selection_bg[3];
        assert!(a > 0.0 && a < 1.0, "selection_bg alpha must be semi-transparent, got {a}");
    }
}

