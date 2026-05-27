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
    /// Focus ring / keyboard-navigation outline color.
    ///
    /// Used by [`FocusRing`](crate::widgets::focus_ring::FocusRing) as the
    /// 2 px outline drawn around the focused pane or widget.
    pub accent_focus: [f32; 4],
}

impl ColorRoles {
    /// The default Phosphor mapping. Themes may override.
    #[must_use] 
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
            // Bright cyan-green focus outline — high contrast on the phosphor
            // background without clashing with the green text palette.
            accent_focus: [0.20, 0.90, 1.00, 1.00],
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
    #[must_use] 
    pub fn new(colors: ColorRoles, ctx: RenderCtx) -> Self {
        Self { colors, ctx }
    }

    #[must_use]
    pub fn phosphor(ctx: RenderCtx) -> Self {
        Self {
            colors: ColorRoles::phosphor(),
            ctx,
        }
    }

    /// Build a `Tokens` snapshot derived from a [`crate::themes::Theme`].
    ///
    /// The phosphor `ColorRoles` are the canonical structure; this fn
    /// overlays the theme's terminal palette onto the role table so a
    /// theme switch visibly recolors token-driven chrome (AppHead etc).
    /// Mappings:
    /// - `surface_base`/`surface_recessed` ← theme background (darker
    ///   variant for recessed)
    /// - `text_primary`/`text_secondary`/`text_dim` ← theme foreground at
    ///   descending alphas
    /// - `text_accent` ← theme cursor color (typically the theme's most
    ///   saturated highlight)
    /// - `chrome_divider`/`chrome_frame_dim` ← theme ui_colors.border
    /// - `status_ok`/`status_warn`/`status_danger`/`status_info` keep
    ///   their phosphor defaults so danger always reads as red etc.
    #[must_use]
    pub fn for_theme(theme: &crate::themes::Theme, ctx: RenderCtx) -> Self {
        let mut roles = ColorRoles::phosphor();
        let bg = theme.colors.background;
        roles.surface_base = bg;
        // surface_recessed: subtly darker tint of bg.
        roles.surface_recessed = [bg[0] * 1.3, bg[1] * 1.3, bg[2] * 1.3, 1.0];
        // surface_raised: lighter than recessed.
        roles.surface_raised = [bg[0] * 1.8, bg[1] * 1.8, bg[2] * 1.8, 1.0];

        let fg = theme.colors.foreground;
        roles.text_primary = fg;
        roles.text_secondary = [fg[0], fg[1], fg[2], 0.78];
        roles.text_dim = [fg[0], fg[1], fg[2], 0.55];
        roles.text_accent = theme.colors.cursor;

        let border = theme.ui_colors.border;
        roles.chrome_divider = border;
        roles.chrome_frame_dim = border;
        roles.chrome_frame = border;
        roles.chrome_frame_active = theme.colors.cursor;

        roles.selection_bg = theme.colors.selection;
        roles.accent_focus = theme.colors.cursor;

        Self::new(roles, ctx)
    }

    /// Look up a built-in theme by name (case-insensitive) and build the
    /// corresponding `Tokens` snapshot. Returns `None` when the name does
    /// not match any registered theme.
    ///
    /// Adapters call this from their `accept_command "set_theme_name"`
    /// handler to refresh their token palette without holding a shared
    /// `Arc<RwLock<Tokens>>`.
    #[must_use]
    pub fn for_theme_name(name: &str, ctx: RenderCtx) -> Option<Self> {
        crate::themes::builtin_by_name(name).map(|theme| Self::for_theme(&theme, ctx))
    }

    /// Return a copy with `ctx.elapsed_secs` replaced.
    ///
    /// Adapters that hold a long-lived `Tokens` snapshot use this each frame
    /// to thread the App's monotonic clock into chrome animations (the
    /// `AppHead` live-dot pulse, etc.) without rebuilding the whole palette.
    #[must_use]
    pub fn with_elapsed(self, elapsed_secs: f32) -> Self {
        Self {
            colors: self.colors,
            ctx: RenderCtx {
                cell_size: self.ctx.cell_size,
                dpi_scale: self.ctx.dpi_scale,
                elapsed_secs,
            },
        }
    }

    // -- Spacing scale (4px-base, scales with the cell on dense fonts) --
    #[must_use] 
    pub fn space_0(&self) -> f32 {
        0.0
    }
    #[must_use] 
    pub fn space_1(&self) -> f32 {
        4.0
    }
    #[must_use] 
    pub fn space_2(&self) -> f32 {
        8.0
    }
    #[must_use] 
    pub fn space_3(&self) -> f32 {
        12.0
    }
    #[must_use] 
    pub fn space_4(&self) -> f32 {
        16.0
    }
    #[must_use] 
    pub fn space_5(&self) -> f32 {
        24.0
    }
    #[must_use] 
    pub fn space_6(&self) -> f32 {
        32.0
    }

    // -- Radii --
    #[must_use] 
    pub fn radius_sm(&self) -> f32 {
        2.0
    }
    #[must_use] 
    pub fn radius_md(&self) -> f32 {
        4.0
    }
    #[must_use] 
    pub fn radius_lg(&self) -> f32 {
        6.0
    }

    // -- Line widths --
    #[must_use] 
    pub fn hair(&self) -> f32 {
        1.0
    }
    #[must_use] 
    pub fn frame(&self) -> f32 {
        2.0
    }

    // -- Convenience accessors that flow through the ctx --
    #[must_use] 
    pub fn cell_w(&self) -> f32 {
        self.ctx.cell_w()
    }
    #[must_use] 
    pub fn cell_h(&self) -> f32 {
        self.ctx.cell_h()
    }
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
        let scale = [
            t.space_0(),
            t.space_1(),
            t.space_2(),
            t.space_3(),
            t.space_4(),
            t.space_5(),
            t.space_6(),
        ];
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
        for &c in &[
            r.surface_base,
            r.surface_recessed,
            r.text_primary,
            r.chrome_frame,
        ] {
            assert!(c[3] > 0.0, "alpha must be positive: {c:?}");
        }
    }

    #[test]
    fn selection_bg_token_exists_and_semi_transparent() {
        let r = ColorRoles::phosphor();
        // Must exist and be semi-transparent (alpha in 0..1 exclusive).
        let a = r.selection_bg[3];
        assert!(
            a > 0.0 && a < 1.0,
            "selection_bg alpha must be semi-transparent, got {a}"
        );
    }

    #[test]
    fn accent_focus_token_is_opaque_and_bright() {
        let r = ColorRoles::phosphor();
        assert_eq!(
            r.accent_focus[3], 1.0,
            "accent_focus alpha must be 1.0 (opaque)"
        );
        assert!(
            r.accent_focus[2] > 0.5,
            "accent_focus should have elevated blue channel"
        );
    }
}
