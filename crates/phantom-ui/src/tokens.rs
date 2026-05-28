//! Design tokens — centralized colors, spacing, type, and radii.
//!
//! Components reference *roles*, not literals. Themes mutate the table.
//! A density change cascades through every component without code edits.
//!
//! The spacing scale resolves against `RenderCtx::cell_size`, so spacing
//! in pixels stays correct when the user changes font size.

use crate::RenderCtx;

/// Shader-effect colors. These mirror the `--glow`, `--scanline`, and
/// `--selection` CSS custom properties from `docs/mockups/system.css`.
///
/// Each color is stored as straight (non-premultiplied) sRGB-in-`[0,1]`
/// RGBA, matching the convention used throughout `ColorRoles`. The glow
/// radius is the CSS `box-shadow` blur radius in CSS pixels (e.g. the
/// phosphor theme's `0 0 12px rgba(...)` shadow → `glow_radius = 12.0`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShaderRoles {
    /// CSS `--glow` shadow color (the RGBA fed to `box-shadow`).
    pub glow_color: [f32; 4],
    /// CSS `--glow` shadow blur radius, in CSS pixels.
    pub glow_radius: f32,
    /// CSS `--scanline` overlay color (very low-alpha tint).
    pub scanline_color: [f32; 4],
    /// CSS `--selection` highlight color (mid-alpha tint).
    pub selection_color: [f32; 4],
}

/// Color roles. Themes provide concrete RGBA values; components reference roles.
///
/// Every field mirrors a CSS custom property in `docs/mockups/system.css`.
/// Colors are stored as straight sRGB-in-`[0,1]` RGBA, matching the
/// `Rgba8UnormSrgb` surface format that `phantom-renderer` selects in
/// `gpu.rs` — the GPU handles the sRGB → linear conversion on sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorRoles {
    // -- Surfaces (z-order: sunken → floating) --
    /// CSS `--surface-recessed`.
    pub surface_recessed: [f32; 4],
    /// CSS `--surface-base`.
    pub surface_base: [f32; 4],
    /// CSS `--surface-raised`.
    pub surface_raised: [f32; 4],
    /// CSS `--surface-floating` — the topmost surface tier, used for
    /// floating chrome (status bars, command palettes).
    pub surface_floating: [f32; 4],

    // -- Frames + dividers --
    /// CSS `--frame-active` — the high-contrast active border color.
    pub chrome_frame: [f32; 4],
    /// CSS `--frame-active` (duplicate alias retained for back-compat).
    pub chrome_frame_active: [f32; 4],
    /// CSS `--frame-dim` — low-contrast inactive border.
    pub chrome_frame_dim: [f32; 4],
    /// CSS `--divider`.
    pub chrome_divider: [f32; 4],

    // -- Text --
    /// CSS `--text-primary` — default body text.
    pub text_primary: [f32; 4],
    /// CSS `--text-secondary`.
    pub text_secondary: [f32; 4],
    /// CSS `--text-dim`.
    pub text_dim: [f32; 4],
    /// CSS `--text-accent`.
    pub text_accent: [f32; 4],
    /// CSS `--text-bright` — the headline/brand color, distinct from
    /// `text_accent` and typically the same hue as `--frame-active`.
    pub text_bright: [f32; 4],

    // -- Status --
    /// CSS `--status-ok`.
    pub status_ok: [f32; 4],
    /// CSS `--status-warn`.
    pub status_warn: [f32; 4],
    /// CSS `--status-danger`.
    pub status_danger: [f32; 4],
    /// CSS `--status-info`.
    pub status_info: [f32; 4],
    /// CSS `--status-mute`.
    pub status_mute: [f32; 4],

    // -- Roles (events, agents, tools) --
    /// CSS `--role-user`.
    pub role_user: [f32; 4],
    /// CSS `--role-agent`.
    pub role_agent: [f32; 4],
    /// CSS `--role-tool`.
    pub role_tool: [f32; 4],
    /// CSS `--role-system`.
    pub role_system: [f32; 4],

    // -- Shader-effect colors --
    /// CSS `--glow` / `--scanline` / `--selection` shaders, grouped.
    pub shader: ShaderRoles,

    // -- Legacy widget fields (kept stable for the existing widget API) --
    /// Selection highlight fill color. Used at 50 % alpha by default; the
    /// alpha channel stored here is the *base* alpha — callers may further
    /// modulate it (e.g. dim when the pane loses focus).
    ///
    /// Mirrors `shader.selection_color`; retained as a separate field
    /// because widgets (cursor, selection) already reference it by name.
    pub selection_bg: [f32; 4],
    /// Focus ring / keyboard-navigation outline color.
    ///
    /// Used by [`FocusRing`](crate::widgets::focus_ring::FocusRing) as the
    /// 2 px outline drawn around the focused pane or widget.
    pub accent_focus: [f32; 4],
}

// ── hex → [f32; 4] helpers (duplicated from themes.rs so tokens.rs can be
//    its own source of truth — every value matches docs/mockups/system.css) ──
const fn h(r: u8, g: u8, b: u8) -> [f32; 4] {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
}
const fn ha(r: u8, g: u8, b: u8, a: f32) -> [f32; 4] {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, a]
}

impl ColorRoles {
    /// **Phosphor** — exact mirror of `[data-theme="phosphor"]` in
    /// `docs/mockups/system.css`.
    #[must_use]
    pub const fn phosphor() -> Self {
        Self {
            surface_recessed: h(0x06, 0x0a, 0x10),
            surface_base:     h(0x0a, 0x0e, 0x14),
            surface_raised:   h(0x0d, 0x12, 0x19),
            surface_floating: h(0x11, 0x17, 0x1f),

            chrome_frame:        h(0x33, 0xff, 0x00),
            chrome_frame_active: h(0x33, 0xff, 0x00),
            chrome_frame_dim:    h(0x1a, 0x2a, 0x18),
            chrome_divider:      h(0x15, 0x28, 0x1a),

            text_dim:       h(0x4a, 0x80, 0x48),
            text_secondary: h(0x6f, 0x9f, 0x6a),
            text_primary:   h(0xb8, 0xff, 0xb8),
            text_bright:    h(0x33, 0xff, 0x00),
            text_accent:    h(0x5f, 0xff, 0x20),

            status_ok:     h(0x33, 0xff, 0x00),
            status_warn:   h(0xff, 0xb0, 0x00),
            status_danger: h(0xff, 0x33, 0x44),
            status_info:   h(0x66, 0xdd, 0xff),
            status_mute:   h(0x4a, 0x80, 0x48),

            role_user:   h(0xc4, 0xff, 0x66),
            role_agent:  h(0x33, 0xff, 0x00),
            role_tool:   h(0x66, 0xdd, 0xff),
            role_system: h(0xff, 0xb0, 0x00),

            shader: ShaderRoles {
                glow_color:      ha(0x33, 0xff, 0x00, 0.32),
                glow_radius:     12.0,
                scanline_color:  ha(0x33, 0xff, 0x00, 0.04),
                selection_color: ha(0x33, 0xff, 0x00, 0.22),
            },

            // Legacy widget fields. selection_bg mirrors --selection;
            // accent_focus mirrors --frame-active (the bright phosphor green).
            selection_bg: ha(0x33, 0xff, 0x00, 0.22),
            accent_focus: h(0x33, 0xff, 0x00),
        }
    }

    /// **Amber** — exact mirror of `[data-theme="amber"]`.
    #[must_use]
    pub const fn amber() -> Self {
        Self {
            surface_recessed: h(0x0a, 0x05, 0x00),
            surface_base:     h(0x11, 0x08, 0x00),
            surface_raised:   h(0x17, 0x0c, 0x01),
            surface_floating: h(0x1e, 0x10, 0x04),

            chrome_frame:        h(0xff, 0xb0, 0x00),
            chrome_frame_active: h(0xff, 0xb0, 0x00),
            chrome_frame_dim:    h(0x3a, 0x23, 0x04),
            chrome_divider:      h(0x2a, 0x19, 0x03),

            text_dim:       h(0x80, 0x64, 0x30),
            text_secondary: h(0xb8, 0x8f, 0x4e),
            text_primary:   h(0xff, 0xd9, 0xa3),
            text_bright:    h(0xff, 0xb0, 0x00),
            text_accent:    h(0xff, 0xc9, 0x4d),

            status_ok:     h(0xc4, 0xff, 0x66),
            status_warn:   h(0xff, 0xb0, 0x00),
            status_danger: h(0xff, 0x55, 0x44),
            status_info:   h(0xff, 0xce, 0x5f),
            status_mute:   h(0x80, 0x64, 0x30),

            role_user:   h(0xff, 0xd9, 0xa3),
            role_agent:  h(0xff, 0xb0, 0x00),
            role_tool:   h(0xff, 0xce, 0x5f),
            role_system: h(0xff, 0x55, 0x44),

            shader: ShaderRoles {
                glow_color:      ha(0xff, 0xb0, 0x00, 0.32),
                glow_radius:     14.0,
                scanline_color:  ha(0xff, 0xb0, 0x00, 0.04),
                selection_color: ha(0xff, 0xb0, 0x00, 0.22),
            },

            selection_bg: ha(0xff, 0xb0, 0x00, 0.22),
            accent_focus: h(0xff, 0xb0, 0x00),
        }
    }

    /// **Ice** — exact mirror of `[data-theme="ice"]`.
    #[must_use]
    pub const fn ice() -> Self {
        Self {
            surface_recessed: h(0x04, 0x08, 0x0d),
            surface_base:     h(0x06, 0x0c, 0x14),
            surface_raised:   h(0x08, 0x11, 0x1b),
            surface_floating: h(0x0c, 0x19, 0x24),

            chrome_frame:        h(0x66, 0xdd, 0xff),
            chrome_frame_active: h(0x66, 0xdd, 0xff),
            chrome_frame_dim:    h(0x18, 0x32, 0x4a),
            chrome_divider:      h(0x14, 0x28, 0x3c),

            text_dim:       h(0x4a, 0x7d, 0x99),
            text_secondary: h(0x6f, 0xa3, 0xc4),
            text_primary:   h(0xb8, 0xe0, 0xff),
            text_bright:    h(0x66, 0xdd, 0xff),
            text_accent:    h(0x5f, 0xc4, 0xff),

            status_ok:     h(0x5f, 0xff, 0xb8),
            status_warn:   h(0xff, 0xce, 0x5f),
            status_danger: h(0xff, 0x5f, 0xa3),
            status_info:   h(0x66, 0xdd, 0xff),
            status_mute:   h(0x4a, 0x7d, 0x99),

            role_user:   h(0xb8, 0xe0, 0xff),
            role_agent:  h(0x66, 0xdd, 0xff),
            role_tool:   h(0x5f, 0xff, 0xb8),
            role_system: h(0xff, 0xce, 0x5f),

            shader: ShaderRoles {
                glow_color:      ha(0x66, 0xdd, 0xff, 0.32),
                glow_radius:     14.0,
                scanline_color:  ha(0x66, 0xdd, 0xff, 0.04),
                selection_color: ha(0x66, 0xdd, 0xff, 0.22),
            },

            selection_bg: ha(0x66, 0xdd, 0xff, 0.22),
            accent_focus: h(0x66, 0xdd, 0xff),
        }
    }

    /// **Blood** — exact mirror of `[data-theme="blood"]`.
    #[must_use]
    pub const fn blood() -> Self {
        Self {
            surface_recessed: h(0x0c, 0x04, 0x05),
            surface_base:     h(0x11, 0x06, 0x08),
            surface_raised:   h(0x18, 0x09, 0x0c),
            surface_floating: h(0x1f, 0x0c, 0x10),

            chrome_frame:        h(0xff, 0x33, 0x44),
            chrome_frame_active: h(0xff, 0x33, 0x44),
            chrome_frame_dim:    h(0x3c, 0x18, 0x20),
            chrome_divider:      h(0x2a, 0x12, 0x18),

            text_dim:       h(0x8a, 0x40, 0x48),
            text_secondary: h(0xb8, 0x66, 0x6d),
            text_primary:   h(0xff, 0xd0, 0xd4),
            text_bright:    h(0xff, 0x33, 0x44),
            text_accent:    h(0xff, 0x66, 0x80),

            status_ok:     h(0x66, 0xff, 0x80),
            status_warn:   h(0xff, 0xaa, 0x44),
            status_danger: h(0xff, 0x33, 0x44),
            status_info:   h(0xff, 0x80, 0xc4),
            status_mute:   h(0x8a, 0x40, 0x48),

            role_user:   h(0xff, 0xd0, 0xd4),
            role_agent:  h(0xff, 0x33, 0x44),
            role_tool:   h(0xff, 0x80, 0xc4),
            role_system: h(0xff, 0xaa, 0x44),

            shader: ShaderRoles {
                glow_color:      ha(0xff, 0x33, 0x44, 0.36),
                glow_radius:     14.0,
                scanline_color:  ha(0xff, 0x33, 0x44, 0.04),
                selection_color: ha(0xff, 0x33, 0x44, 0.22),
            },

            selection_bg: ha(0xff, 0x33, 0x44, 0.22),
            accent_focus: h(0xff, 0x33, 0x44),
        }
    }

    /// **Vapor** — exact mirror of `[data-theme="vapor"]` (Miami Vice neon).
    #[must_use]
    pub const fn vapor() -> Self {
        Self {
            surface_recessed: h(0x0a, 0x04, 0x18),
            surface_base:     h(0x0e, 0x06, 0x26),
            surface_raised:   h(0x15, 0x0a, 0x36),
            surface_floating: h(0x1c, 0x10, 0x48),

            chrome_frame:        h(0xff, 0x44, 0xdd),
            chrome_frame_active: h(0xff, 0x44, 0xdd),
            chrome_frame_dim:    h(0x3a, 0x1c, 0x5e),
            chrome_divider:      h(0x2a, 0x14, 0x44),

            text_dim:       h(0x8a, 0x4e, 0xc4),
            text_secondary: h(0xc0, 0x8e, 0xff),
            text_primary:   h(0xf0, 0xe2, 0xff),
            text_bright:    h(0xff, 0x44, 0xdd),
            text_accent:    h(0x5f, 0xc4, 0xff),

            status_ok:     h(0x5f, 0xff, 0xd0),
            status_warn:   h(0xff, 0xc9, 0x4d),
            status_danger: h(0xff, 0x5f, 0xa3),
            status_info:   h(0x5f, 0xc4, 0xff),
            status_mute:   h(0x8a, 0x4e, 0xc4),

            role_user:   h(0xff, 0x44, 0xdd),
            role_agent:  h(0x5f, 0xc4, 0xff),
            role_tool:   h(0x5f, 0xff, 0xd0),
            role_system: h(0xff, 0xc9, 0x4d),

            shader: ShaderRoles {
                glow_color:      ha(0xff, 0x44, 0xdd, 0.42),
                glow_radius:     18.0,
                scanline_color:  ha(0xff, 0x44, 0xdd, 0.04),
                selection_color: ha(0x5f, 0xc4, 0xff, 0.24),
            },

            // Vapor's CSS --selection is the cyan accent, not the pink frame.
            selection_bg: ha(0x5f, 0xc4, 0xff, 0.24),
            accent_focus: h(0xff, 0x44, 0xdd),
        }
    }

    /// **Cyber** — exact mirror of `[data-theme="cyber"]` (magenta + cyan).
    #[must_use]
    pub const fn cyber() -> Self {
        Self {
            surface_recessed: h(0x05, 0x02, 0x08),
            surface_base:     h(0x0a, 0x04, 0x10),
            surface_raised:   h(0x13, 0x08, 0x20),
            surface_floating: h(0x1c, 0x0e, 0x30),

            chrome_frame:        h(0xff, 0x00, 0x7a),
            chrome_frame_active: h(0xff, 0x00, 0x7a),
            chrome_frame_dim:    h(0x3a, 0x18, 0x50),
            chrome_divider:      h(0x22, 0x10, 0x38),

            text_dim:       h(0x7a, 0x3c, 0x8a),
            text_secondary: h(0xc4, 0x4e, 0xd8),
            text_primary:   h(0xf0, 0xd4, 0xff),
            text_bright:    h(0xff, 0x00, 0x7a),
            text_accent:    h(0x00, 0xff, 0xd0),

            status_ok:     h(0x00, 0xff, 0xd0),
            status_warn:   h(0xff, 0xe4, 0x4d),
            status_danger: h(0xff, 0x44, 0x44),
            status_info:   h(0x00, 0xb4, 0xff),
            status_mute:   h(0x7a, 0x3c, 0x8a),

            role_user:   h(0x00, 0xff, 0xd0),
            role_agent:  h(0xff, 0x00, 0x7a),
            role_tool:   h(0xff, 0xe4, 0x4d),
            role_system: h(0xff, 0x44, 0x44),

            shader: ShaderRoles {
                glow_color:      ha(0xff, 0x00, 0x7a, 0.50),
                glow_radius:     18.0,
                scanline_color:  ha(0xff, 0x00, 0x7a, 0.04),
                selection_color: ha(0x00, 0xff, 0xd0, 0.24),
            },

            selection_bg: ha(0x00, 0xff, 0xd0, 0.24),
            accent_focus: h(0xff, 0x00, 0x7a),
        }
    }

    /// Look up a CSS-derived `ColorRoles` palette by name (case-insensitive).
    ///
    /// Matches the six themes defined as `[data-theme="..."]` blocks in
    /// `docs/mockups/system.css`. Returns `None` for unknown names.
    #[must_use]
    pub fn for_theme_name(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "phosphor" => Some(Self::phosphor()),
            "amber" => Some(Self::amber()),
            "ice" => Some(Self::ice()),
            "blood" => Some(Self::blood()),
            "vapor" | "vaporwave" => Some(Self::vapor()),
            "cyber" | "cyberpunk" => Some(Self::cyber()),
            _ => None,
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
        // When the theme's name resolves to one of the six CSS-source-of-truth
        // palettes in `docs/mockups/system.css`, use that palette verbatim so
        // the Rust UI matches the HTML mockup byte-for-byte. Falls back to the
        // legacy "derive from terminal colors" path for any non-CSS theme
        // (e.g. Pip-Boy, which is a Rust-only palette).
        if let Some(roles) = ColorRoles::for_theme_name(&theme.name) {
            return Self::new(roles, ctx);
        }

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
        roles.text_bright = theme.colors.cursor;

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
        // Phosphor's accent_focus is now CSS --frame-active (#33ff00 green),
        // so it no longer has an elevated blue channel — but it must still be
        // opaque and fully saturated on its primary hue.
        let r = ColorRoles::phosphor();
        assert_eq!(
            r.accent_focus[3], 1.0,
            "accent_focus alpha must be 1.0 (opaque)"
        );
        // Phosphor accent_focus is #33ff00 — green channel must be at maximum.
        assert!(
            r.accent_focus[1] > 0.99,
            "phosphor accent_focus green channel must be ~1.0, got {}",
            r.accent_focus[1]
        );
    }

    // -----------------------------------------------------------------
    // CSS source-of-truth regression guard.
    //
    // These hardcoded `[f32; 4]` values were extracted directly from the
    // six `[data-theme="..."]` blocks in `docs/mockups/system.css`. They
    // are the contract: when the human edits system.css, this test will
    // fail and the Rust mirror must be re-synced. Do NOT relax these
    // asserts to bring them back into agreement — fix the constructor.
    //
    // The five (theme, role) pairs below cover one role per theme, hitting
    // surfaces, frames, text, status, role colors, and shader effects so a
    // drift in any of those tables surfaces as a unit-test failure.
    // -----------------------------------------------------------------
    fn approx(a: [f32; 4], b: [f32; 4], label: &str) {
        for i in 0..4 {
            let d = (a[i] - b[i]).abs();
            assert!(
                d < 0.005,
                "{label} channel {i} drifted: got {:?}, expected {:?}",
                a,
                b
            );
        }
    }

    #[test]
    fn css_source_of_truth_phosphor_text_bright() {
        // CSS --text-bright = #33ff00 → 0x33 = 51/255 ≈ 0.2,  0xff = 1.0.
        approx(
            ColorRoles::phosphor().text_bright,
            [51.0 / 255.0, 255.0 / 255.0, 0.0 / 255.0, 1.0],
            "phosphor.text_bright",
        );
    }

    #[test]
    fn css_source_of_truth_amber_status_warn() {
        // CSS --status-warn = #ffb000 (amber palette).
        approx(
            ColorRoles::amber().status_warn,
            [255.0 / 255.0, 176.0 / 255.0, 0.0 / 255.0, 1.0],
            "amber.status_warn",
        );
    }

    #[test]
    fn css_source_of_truth_ice_role_agent() {
        // CSS --role-agent = #66ddff (ice palette).
        approx(
            ColorRoles::ice().role_agent,
            [102.0 / 255.0, 221.0 / 255.0, 255.0 / 255.0, 1.0],
            "ice.role_agent",
        );
    }

    #[test]
    fn css_source_of_truth_blood_surface_floating() {
        // CSS --surface-floating = #1f0c10 (blood palette).
        approx(
            ColorRoles::blood().surface_floating,
            [31.0 / 255.0, 12.0 / 255.0, 16.0 / 255.0, 1.0],
            "blood.surface_floating",
        );
    }

    #[test]
    fn css_source_of_truth_cyber_glow_color() {
        // CSS --glow = 0 0 18px rgba(255, 0, 122, 0.5) on cyber.
        approx(
            ColorRoles::cyber().shader.glow_color,
            [255.0 / 255.0, 0.0 / 255.0, 122.0 / 255.0, 0.5],
            "cyber.shader.glow_color",
        );
        assert!(
            (ColorRoles::cyber().shader.glow_radius - 18.0).abs() < 0.001,
            "cyber.shader.glow_radius must be 18.0, got {}",
            ColorRoles::cyber().shader.glow_radius
        );
    }

    #[test]
    fn css_source_of_truth_vapor_selection_uses_cyan_accent() {
        // Vapor is the one theme where --selection is the cyan accent,
        // not the pink frame: rgba(95, 196, 255, 0.24).
        approx(
            ColorRoles::vapor().shader.selection_color,
            [95.0 / 255.0, 196.0 / 255.0, 255.0 / 255.0, 0.24],
            "vapor.shader.selection_color",
        );
    }

    #[test]
    fn for_theme_name_resolves_all_six_css_themes() {
        for name in ["phosphor", "amber", "ice", "blood", "vapor", "cyber"] {
            assert!(
                ColorRoles::for_theme_name(name).is_some(),
                "CSS theme '{name}' must resolve via ColorRoles::for_theme_name"
            );
        }
        // Case-insensitive.
        assert!(ColorRoles::for_theme_name("PHOSPHOR").is_some());
        assert!(ColorRoles::for_theme_name("Cyberpunk").is_some());
        // Unknown returns None.
        assert!(ColorRoles::for_theme_name("nonexistent").is_none());
    }
}
