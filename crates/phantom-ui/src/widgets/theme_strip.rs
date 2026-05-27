//! Top-of-window theme picker bar — mirrors the mockup's `.controls` row.
//!
//! Renders a horizontal strip pinned to the very top of the window. The strip
//! shows one swatch per built-in theme (matching
//! [`themes::BUILTIN_NAMES`](crate::themes::BUILTIN_NAMES)), the active
//! theme's name, and a `[ ] scanlines (CRT)` toggle on the right edge.
//!
//! ```text
//! ┌────────────────────────────────────────────────────────────────┐
//! │  ● ○ ○ ○ ○ ○   phosphor                       [x] CRT          │
//! └────────────────────────────────────────────────────────────────┘
//! ```
//!
//! The widget is pure render — clicks are routed by `ThemeStrip::hit_test`,
//! which the App's mouse handler calls to translate a pixel position into a
//! [`ThemeStripAction`] (theme select / CRT toggle / no-op).

use crate::layout::Rect;
use crate::tokens::Tokens;
use crate::widgets::{TextSegment, Widget};
use phantom_renderer::quads::QuadInstance;

/// Strip height in logical pixels — fixed to match the mockup's 48px controls
/// row scaled to a denser terminal-chrome density.
pub const THEME_STRIP_HEIGHT: f32 = 28.0;

/// Action emitted by [`ThemeStrip::hit_test`] when the user clicks the bar.
#[derive(Debug, Clone, PartialEq)]
pub enum ThemeStripAction {
    /// Switch to the theme with the given lowercase name (e.g. `"amber"`).
    SetTheme(String),
    /// Flip the CRT (scanlines / bloom / curvature) on or off.
    ToggleCrt,
    /// Click landed outside any actionable region — caller should fall
    /// through to other hit-testers.
    None,
}

/// A single theme swatch — a small circle filled with the theme's signature
/// hue. The strip lays these out left-to-right with a fixed gap.
#[derive(Debug, Clone)]
struct Swatch {
    /// Lowercase theme name (matches `themes::builtin_by_name`).
    name: String,
    /// Fill color (signature hue per theme).
    color: [f32; 4],
}

impl Swatch {
    fn new(name: &str, color: [f32; 4]) -> Self {
        Self {
            name: name.to_owned(),
            color,
        }
    }
}

/// Top-of-window theme picker + CRT toggle bar.
///
/// Construct once and re-bind per frame with [`with_tokens`](Self::with_tokens)
/// + [`with_active`](Self::with_active) + [`with_crt`](Self::with_crt).
///
/// Hit testing is decoupled from rendering: the App calls
/// [`hit_test`](Self::hit_test) on mouse press to map a pixel position to a
/// [`ThemeStripAction`].
#[derive(Debug, Clone)]
pub struct ThemeStrip {
    swatches: Vec<Swatch>,
    /// Lowercase name of the currently active theme. Drives the outline ring
    /// on the matching swatch.
    active: String,
    /// Whether CRT post-fx is enabled. Drives the checkbox glyph.
    crt_on: bool,
    /// Live token snapshot — theme switches recolor the strip's chrome.
    tokens: Tokens,
}

impl ThemeStrip {
    /// Build a fresh strip with the seven canonical swatches.
    ///
    /// The colors are hand-picked to match the mockup's CSS:
    /// `.controls .sw[data-set="phosphor"] { background: #33ff00 }` etc.
    /// Each theme's signature hue is encoded directly so the strip stays
    /// readable on any background.
    #[must_use]
    pub fn new() -> Self {
        Self {
            swatches: vec![
                Swatch::new("phosphor", [0.20, 1.00, 0.00, 1.0]),
                Swatch::new("amber", [1.00, 0.69, 0.00, 1.0]),
                Swatch::new("ice", [0.40, 0.87, 1.00, 1.0]),
                Swatch::new("blood", [1.00, 0.20, 0.27, 1.0]),
                Swatch::new("vapor", [1.00, 0.27, 0.87, 1.0]),
                Swatch::new("cyber", [1.00, 0.00, 0.48, 1.0]),
            ],
            active: "phosphor".into(),
            crt_on: false,
            tokens: Tokens::phosphor(crate::render_ctx::RenderCtx::fallback()),
        }
    }

    /// Builder: set the live tokens snapshot.
    #[must_use]
    pub fn with_tokens(mut self, tokens: Tokens) -> Self {
        self.tokens = tokens;
        self
    }

    /// Builder: set the active theme name (lowercase).
    #[must_use]
    pub fn with_active(mut self, name: impl Into<String>) -> Self {
        self.active = name.into().to_ascii_lowercase();
        self
    }

    /// Setter: update the active theme without rebuilding.
    pub fn set_active(&mut self, name: &str) {
        self.active = name.to_ascii_lowercase();
    }

    /// Builder: set the CRT toggle state.
    #[must_use]
    pub fn with_crt(mut self, on: bool) -> Self {
        self.crt_on = on;
        self
    }

    /// Setter: update the CRT toggle without rebuilding.
    pub fn set_crt(&mut self, on: bool) {
        self.crt_on = on;
    }

    /// Whether the CRT toggle is currently on.
    #[must_use]
    pub fn crt_on(&self) -> bool {
        self.crt_on
    }

    /// Number of swatches rendered.
    #[must_use]
    pub fn swatch_count(&self) -> usize {
        self.swatches.len()
    }

    /// Currently active theme name.
    #[must_use]
    pub fn active(&self) -> &str {
        &self.active
    }

    // --- Internal layout math ---

    fn swatch_radius(&self) -> f32 {
        9.0
    }

    fn swatch_gap(&self) -> f32 {
        14.0
    }

    fn left_pad(&self) -> f32 {
        16.0
    }

    /// Pixel center of swatch `i` (0-based).
    fn swatch_center_x(&self, rect_x: f32, i: usize) -> f32 {
        let r = self.swatch_radius();
        let g = self.swatch_gap();
        rect_x + self.left_pad() + r + (i as f32) * (r * 2.0 + g)
    }

    /// Right-edge pixel x of the rightmost swatch.
    fn swatches_right_edge(&self, rect_x: f32) -> f32 {
        if self.swatches.is_empty() {
            return rect_x + self.left_pad();
        }
        let last = self.swatches.len() - 1;
        self.swatch_center_x(rect_x, last) + self.swatch_radius()
    }

    /// Pixel rect of the CRT toggle area (square + label) anchored to right edge.
    fn crt_toggle_rect(&self, rect: &Rect) -> Rect {
        // Reserve ~140px for "[x] scanlines (CRT)" on the right.
        let width: f32 = 156.0;
        let right_pad: f32 = 16.0;
        Rect {
            x: rect.x + rect.width - width - right_pad,
            y: rect.y,
            width,
            height: rect.height,
        }
    }

    /// Hit-test a pixel position. Returns the corresponding action.
    ///
    /// Used by the App's mouse handler. Pixel coordinates must be in the same
    /// space as the rect passed to [`render_quads`].
    #[must_use]
    pub fn hit_test(&self, rect: &Rect, x: f32, y: f32) -> ThemeStripAction {
        if y < rect.y || y > rect.y + rect.height {
            return ThemeStripAction::None;
        }
        if x < rect.x || x > rect.x + rect.width {
            return ThemeStripAction::None;
        }

        // CRT toggle has higher priority since it sits on the right edge and
        // could overlap the rightmost swatch on narrow windows.
        let crt = self.crt_toggle_rect(rect);
        if x >= crt.x && x <= crt.x + crt.width && y >= crt.y && y <= crt.y + crt.height {
            return ThemeStripAction::ToggleCrt;
        }

        // Swatches — generous tap target: full radius + half-gap.
        let r = self.swatch_radius();
        let g = self.swatch_gap();
        for (i, sw) in self.swatches.iter().enumerate() {
            let cx = self.swatch_center_x(rect.x, i);
            let cy = rect.y + rect.height * 0.5;
            let dx = (x - cx).abs();
            let dy = (y - cy).abs();
            if dx <= r + g * 0.5 && dy <= r + 2.0 {
                return ThemeStripAction::SetTheme(sw.name.clone());
            }
        }

        ThemeStripAction::None
    }
}

impl Default for ThemeStrip {
    fn default() -> Self {
        Self::new()
    }
}

impl Widget for ThemeStrip {
    fn render_quads(&self, rect: &Rect) -> Vec<QuadInstance> {
        let t = self.tokens;
        let mut quads = Vec::with_capacity(2 + self.swatches.len() * 2);

        // -- Strip background ------------------------------------------------
        quads.push(QuadInstance {
            pos: [rect.x, rect.y],
            size: [rect.width, rect.height],
            color: t.colors.surface_raised,
            border_radius: 0.0,
        ..Default::default()
            });
        // Bottom hairline divider so the strip reads as chrome, not a pane.
        let hair = t.hair().max(1.0);
        quads.push(QuadInstance {
            pos: [rect.x, rect.y + rect.height - hair],
            size: [rect.width, hair],
            color: t.colors.chrome_divider,
            border_radius: 0.0,
        ..Default::default()
            });

        // -- Swatches --------------------------------------------------------
        let r = self.swatch_radius();
        for (i, sw) in self.swatches.iter().enumerate() {
            let cx = self.swatch_center_x(rect.x, i);
            let cy = rect.y + rect.height * 0.5;
            let is_active = sw.name == self.active;

            // Outer ring (only when active) — accent color, slightly larger
            // than the swatch radius so it reads as a halo not a stroke.
            if is_active {
                let ring_r = r + 3.0;
                quads.push(QuadInstance {
                    pos: [cx - ring_r, cy - ring_r],
                    size: [ring_r * 2.0, ring_r * 2.0],
                    color: t.colors.chrome_frame_active,
                    border_radius: ring_r,
                ..Default::default()
            });
            }

            // Filled swatch.
            quads.push(QuadInstance {
                pos: [cx - r, cy - r],
                size: [r * 2.0, r * 2.0],
                color: sw.color,
                border_radius: r,
            ..Default::default()
            });
        }

        // -- CRT toggle checkbox --------------------------------------------
        // Box on the left of the label. Filled when CRT is on.
        let crt_rect = self.crt_toggle_rect(rect);
        let box_size = 12.0;
        let box_x = crt_rect.x;
        let box_y = crt_rect.y + (crt_rect.height - box_size) * 0.5;
        // Outline (always shown).
        let stroke = 1.5_f32;
        quads.push(QuadInstance {
            pos: [box_x, box_y],
            size: [box_size, stroke],
            color: t.colors.chrome_frame,
            border_radius: 0.0,
        ..Default::default()
            });
        quads.push(QuadInstance {
            pos: [box_x, box_y + box_size - stroke],
            size: [box_size, stroke],
            color: t.colors.chrome_frame,
            border_radius: 0.0,
        ..Default::default()
            });
        quads.push(QuadInstance {
            pos: [box_x, box_y],
            size: [stroke, box_size],
            color: t.colors.chrome_frame,
            border_radius: 0.0,
        ..Default::default()
            });
        quads.push(QuadInstance {
            pos: [box_x + box_size - stroke, box_y],
            size: [stroke, box_size],
            color: t.colors.chrome_frame,
            border_radius: 0.0,
        ..Default::default()
            });
        // Inner fill (only when on) — slightly inset so the outline stays visible.
        if self.crt_on {
            let inset = stroke + 1.5;
            quads.push(QuadInstance {
                pos: [box_x + inset, box_y + inset],
                size: [box_size - inset * 2.0, box_size - inset * 2.0],
                color: t.colors.chrome_frame_active,
                border_radius: 0.0,
            ..Default::default()
            });
        }

        quads
    }

    fn render_text(&self, rect: &Rect) -> Vec<TextSegment> {
        let t = self.tokens;
        let cell_w = t.cell_w();
        let cell_h = t.cell_h();
        let text_y = rect.y + (rect.height - cell_h) * 0.5;
        let mut segs = Vec::with_capacity(2);

        // -- Active theme name, immediately after the swatches --------------
        let name_x = self.swatches_right_edge(rect.x) + 18.0;
        segs.push(TextSegment {
            text: self.active.clone(),
            x: name_x,
            y: text_y,
            color: t.colors.text_accent,
        });

        // -- CRT label ------------------------------------------------------
        let crt_rect = self.crt_toggle_rect(rect);
        let label_x = crt_rect.x + 18.0; // past the 12px checkbox + 6px gap
        let label = if self.crt_on {
            "CRT on"
        } else {
            "CRT off"
        };
        // Right-align the label inside the toggle rect so it doesn't drift
        // when window width changes.
        let label_width = label.chars().count() as f32 * cell_w;
        let label_right_x = (crt_rect.x + crt_rect.width - label_width).max(label_x);
        segs.push(TextSegment {
            text: label.to_string(),
            x: label_right_x,
            y: text_y,
            color: if self.crt_on {
                t.colors.text_accent
            } else {
                t.colors.text_dim
            },
        });

        segs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_ctx::RenderCtx;

    fn strip_rect() -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            width: 1280.0,
            height: THEME_STRIP_HEIGHT,
        }
    }

    #[test]
    fn strip_has_six_swatches() {
        let s = ThemeStrip::new();
        assert_eq!(s.swatch_count(), 6);
    }

    #[test]
    fn hit_test_returns_set_theme_for_swatch_center() {
        let s = ThemeStrip::new();
        let r = strip_rect();
        let cx = s.swatch_center_x(r.x, 2); // ice
        let cy = r.y + r.height * 0.5;
        assert_eq!(
            s.hit_test(&r, cx, cy),
            ThemeStripAction::SetTheme("ice".into())
        );
    }

    #[test]
    fn hit_test_returns_toggle_for_crt_box() {
        let s = ThemeStrip::new();
        let r = strip_rect();
        let crt = s.crt_toggle_rect(&r);
        let cx = crt.x + 6.0;
        let cy = crt.y + crt.height * 0.5;
        assert_eq!(s.hit_test(&r, cx, cy), ThemeStripAction::ToggleCrt);
    }

    #[test]
    fn hit_test_outside_returns_none() {
        let s = ThemeStrip::new();
        let r = strip_rect();
        assert_eq!(s.hit_test(&r, r.x - 10.0, r.y + 4.0), ThemeStripAction::None);
        assert_eq!(
            s.hit_test(&r, r.x + 200.0, r.y + r.height + 10.0),
            ThemeStripAction::None
        );
    }

    #[test]
    fn active_ring_quad_appears_in_render() {
        let tokens = Tokens::phosphor(RenderCtx::fallback());
        let s = ThemeStrip::new()
            .with_tokens(tokens)
            .with_active("amber");
        let quads = s.render_quads(&strip_rect());
        // Expect at least bg + divider + active ring + 6 swatches = 9
        assert!(quads.len() >= 9);
        // Ring color matches accent.
        let ring = quads
            .iter()
            .find(|q| q.color == tokens.colors.chrome_frame_active);
        assert!(ring.is_some(), "active theme must render an accent ring quad");
    }

    #[test]
    fn crt_label_changes_when_toggled() {
        let s_on = ThemeStrip::new().with_crt(true);
        let s_off = ThemeStrip::new().with_crt(false);
        let txt_on = s_on.render_text(&strip_rect());
        let txt_off = s_off.render_text(&strip_rect());
        let on_label = txt_on
            .iter()
            .find(|t| t.text.contains("CRT"))
            .expect("CRT label must be present");
        let off_label = txt_off
            .iter()
            .find(|t| t.text.contains("CRT"))
            .expect("CRT label must be present");
        assert_ne!(on_label.text, off_label.text);
    }

    #[test]
    fn active_setter_normalizes_case() {
        let mut s = ThemeStrip::new();
        s.set_active("AMBER");
        assert_eq!(s.active(), "amber");
    }
}
