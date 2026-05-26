//! Shared app-header chrome — the strip at the top of every Phantom adapter.
//!
//! Matches the mockup layout: `icon · NAME · TITLE … META`.
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────┐
//! │ ◆  AGENT · claude-opus-4-7 · conversational         ● live  │
//! └──────────────────────────────────────────────────────────────┘
//!   ico  name   title                                    meta
//! ```
//!
//! All colors flow through `Tokens` so a theme swap recolors every adapter
//! header without code changes. Adapters that previously hand-rolled their
//! own header bar should construct an `AppHead` (passing the live tokens
//! snapshot via `with_tokens`) and emit its quads/text alongside their
//! body content.

use crate::layout::Rect;
use crate::render_ctx::RenderCtx;
use crate::tokens::Tokens;
use crate::widgets::{TextSegment, Widget};
use phantom_renderer::quads::QuadInstance;

/// Header bar height, in cell-height units.
///
/// 1.6× the body cell height matches the chrome density used by Inspector
/// and the mockup's `.app-head` height.
pub const APP_HEAD_HEIGHT_CELLS: f32 = 1.6;

/// Status dot color states for the right-side meta region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppHeadDot {
    /// No dot rendered.
    None,
    /// Green ok dot — `status_ok`.
    Ok,
    /// Yellow warn dot — `status_warn`.
    Warn,
    /// Red danger dot — `status_danger`.
    Danger,
    /// Blue info dot — `status_info`.
    Info,
    /// Neutral accent dot — `text_accent`.
    Live,
}

/// The mockup's shared header bar for every adapter pane.
///
/// Construct one per render call. The `tokens` field carries the live color
/// palette — adapters snapshot their theme tokens once per `render()` and
/// pass them in via [`AppHead::with_tokens`]. The default falls back to the
/// phosphor palette for test/stand-alone use.
#[derive(Debug, Clone)]
pub struct AppHead {
    /// Single-glyph icon (e.g. `◆`, `▶`, `⚙`). Empty → no icon segment.
    pub icon: String,
    /// Uppercase app name (e.g. `AGENT`, `TERMINAL`).
    pub name: String,
    /// Free-form title (e.g. `zsh · ~/badass-cli`). Empty → no title segment.
    pub title: String,
    /// Right-aligned meta string (e.g. `live`, `phase 4/7`, `203×58`).
    pub meta: String,
    /// Optional status dot rendered just before `meta` text.
    pub dot: AppHeadDot,
    /// Whether this pane is currently focused — affects header tint.
    pub focused: bool,
    /// Live render context for cell metrics.
    pub ctx: RenderCtx,
    /// Live color palette snapshot — theme switches propagate through this.
    pub tokens: Tokens,
}

impl AppHead {
    /// Build a header with name + title and no dot.
    #[must_use]
    pub fn new(name: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            icon: String::new(),
            name: name.into(),
            title: title.into(),
            meta: String::new(),
            dot: AppHeadDot::None,
            focused: false,
            ctx: RenderCtx::fallback(),
            tokens: Tokens::phosphor(RenderCtx::fallback()),
        }
    }

    /// Builder: set the leading icon glyph.
    #[must_use]
    pub fn with_icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = icon.into();
        self
    }

    /// Builder: set the right-aligned meta string.
    #[must_use]
    pub fn with_meta(mut self, meta: impl Into<String>) -> Self {
        self.meta = meta.into();
        self
    }

    /// Builder: set the status dot.
    #[must_use]
    pub fn with_dot(mut self, dot: AppHeadDot) -> Self {
        self.dot = dot;
        self
    }

    /// Builder: mark the pane focused (brighter chrome).
    #[must_use]
    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    /// Builder: bind a live `RenderCtx` so spacing scales with font size.
    #[must_use]
    pub fn with_ctx(mut self, ctx: RenderCtx) -> Self {
        self.ctx = ctx;
        self
    }

    /// Builder: bind a live tokens snapshot so theme switches recolor the
    /// header without rebuilding the adapter. Defaults to phosphor.
    #[must_use]
    pub fn with_tokens(mut self, tokens: Tokens) -> Self {
        self.tokens = tokens;
        self
    }

    /// Pixel height of the header bar at the current cell metrics.
    #[must_use]
    pub fn height(&self) -> f32 {
        self.ctx.cell_h() * APP_HEAD_HEIGHT_CELLS
    }

    /// The body rect — everything inside `outer` *below* the header strip.
    /// Adapters use this to position their body content.
    #[must_use]
    pub fn body_rect(&self, outer: &Rect) -> Rect {
        let h = self.height();
        Rect {
            x: outer.x,
            y: outer.y + h,
            width: outer.width,
            height: (outer.height - h).max(0.0),
        }
    }

    /// Resolve the dot color from the active token palette.
    fn dot_color(&self) -> Option<[f32; 4]> {
        match self.dot {
            AppHeadDot::None => None,
            AppHeadDot::Ok => Some(self.tokens.colors.status_ok),
            AppHeadDot::Warn => Some(self.tokens.colors.status_warn),
            AppHeadDot::Danger => Some(self.tokens.colors.status_danger),
            AppHeadDot::Info => Some(self.tokens.colors.status_info),
            AppHeadDot::Live => Some(self.tokens.colors.text_accent),
        }
    }
}

impl Default for AppHead {
    fn default() -> Self {
        Self::new("", "")
    }
}

impl AppHead {
    /// Render the header directly into adapter-side primitive vectors.
    ///
    /// Most `AppAdapter::render()` implementations build `Vec<QuadData>` +
    /// `Vec<TextData>` rather than the renderer's `QuadInstance` / widget
    /// `TextSegment` types. This helper is the bridge — it appends the
    /// header's quads and text segments to the adapter's output vectors.
    ///
    /// Accepts the adapter-side `Rect` (with `cell_size` metadata), which
    /// is the shape every `Renderable::render` receives.
    pub fn render_into_adapter(
        &self,
        rect: &phantom_adapter::adapter::Rect,
        quads: &mut Vec<phantom_adapter::adapter::QuadData>,
        text_segments: &mut Vec<phantom_adapter::adapter::TextData>,
    ) {
        let inner = adapter_rect_to_ui(rect);
        for q in self.render_quads(&inner) {
            quads.push(phantom_adapter::adapter::QuadData {
                x: q.pos[0],
                y: q.pos[1],
                w: q.size[0],
                h: q.size[1],
                color: q.color,
            });
        }
        for s in self.render_text(&inner) {
            text_segments.push(phantom_adapter::adapter::TextData {
                text: s.text,
                x: s.x,
                y: s.y,
                color: s.color,
            });
        }
    }

    /// Adapter-side `body_rect` — same semantics as [`Self::body_rect`] but
    /// returns the [`phantom_adapter::adapter::Rect`] type so adapters can
    /// route it straight into their body-layout code without conversion.
    #[must_use]
    pub fn body_rect_adapter(
        &self,
        outer: &phantom_adapter::adapter::Rect,
    ) -> phantom_adapter::adapter::Rect {
        let h = self.height();
        phantom_adapter::adapter::Rect {
            x: outer.x,
            y: outer.y + h,
            width: outer.width,
            height: (outer.height - h).max(0.0),
            cell_size: outer.cell_size,
        }
    }
}

fn adapter_rect_to_ui(r: &phantom_adapter::adapter::Rect) -> Rect {
    Rect {
        x: r.x,
        y: r.y,
        width: r.width,
        height: r.height,
    }
}

impl Widget for AppHead {
    fn render_quads(&self, rect: &Rect) -> Vec<QuadInstance> {
        let t = self.tokens;
        let h = self.height();
        let mut quads = Vec::with_capacity(3);

        // Header band background — slightly raised when focused, recessed otherwise.
        let bg = if self.focused {
            t.colors.surface_raised
        } else {
            t.colors.surface_recessed
        };
        quads.push(QuadInstance {
            pos: [rect.x, rect.y],
            size: [rect.width, h],
            color: bg,
            border_radius: 0.0,
        });

        // Bottom divider hairline — separates header from body.
        let hair_h = t.hair().max(1.0);
        quads.push(QuadInstance {
            pos: [rect.x, rect.y + h - hair_h],
            size: [rect.width, hair_h],
            color: t.colors.chrome_divider,
            border_radius: 0.0,
        });

        // Status dot — small circle just before the meta text.
        // Both the dot and the meta text use `space_3()` as their right
        // padding, so the dot sits flush with the meta's right edge.
        if let Some(color) = self.dot_color() && !self.meta.is_empty() {
            let pad_x = t.space_3();
            let cell_w = self.ctx.cell_w();
            let dot_size = (h * 0.28).max(4.0);
            let meta_width = self.meta.chars().count() as f32 * cell_w;
            let dot_x = rect.x + rect.width - pad_x - meta_width - dot_size - cell_w * 0.5;
            let dot_y = rect.y + (h - dot_size) * 0.5;
            quads.push(QuadInstance {
                pos: [dot_x, dot_y],
                size: [dot_size, dot_size],
                color,
                border_radius: dot_size * 0.5,
            });
        }

        quads
    }

    fn render_text(&self, rect: &Rect) -> Vec<TextSegment> {
        let t = self.tokens;
        let cell_w = self.ctx.cell_w();
        let cell_h = self.ctx.cell_h();
        let h = self.height();
        // Vertically center text inside the header band.
        let baseline_y = rect.y + (h - cell_h) * 0.5;
        let pad_x = t.space_3();
        let mut segs = Vec::with_capacity(4);
        let mut cursor = rect.x + pad_x;

        // Icon
        if !self.icon.is_empty() {
            segs.push(TextSegment {
                text: self.icon.clone(),
                x: cursor,
                y: baseline_y,
                color: t.colors.text_accent,
            });
            cursor += self.icon.chars().count() as f32 * cell_w + cell_w * 0.5;
        }

        // Name (uppercase, bright)
        if !self.name.is_empty() {
            segs.push(TextSegment {
                text: self.name.to_ascii_uppercase(),
                x: cursor,
                y: baseline_y,
                color: t.colors.text_accent,
            });
            cursor += self.name.chars().count() as f32 * cell_w;
        }

        // Separator + title
        if !self.title.is_empty() {
            let sep = " · ";
            segs.push(TextSegment {
                text: sep.to_string(),
                x: cursor,
                y: baseline_y,
                color: t.colors.text_dim,
            });
            cursor += sep.chars().count() as f32 * cell_w;
            segs.push(TextSegment {
                text: self.title.clone(),
                x: cursor,
                y: baseline_y,
                color: t.colors.text_secondary,
            });
        }

        // Meta — right-aligned. Best-effort given monospace assumption.
        if !self.meta.is_empty() {
            let meta_width = self.meta.chars().count() as f32 * cell_w;
            let meta_x = rect.x + rect.width - pad_x - meta_width;
            segs.push(TextSegment {
                text: self.meta.clone(),
                x: meta_x.max(cursor + cell_w),
                y: baseline_y,
                color: t.colors.text_dim,
            });
        }

        segs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::{ColorRoles, Tokens};

    fn rect() -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            width: 600.0,
            height: 200.0,
        }
    }

    #[test]
    fn height_scales_with_cell_h() {
        let head = AppHead::new("AGENT", "").with_ctx(RenderCtx::fallback());
        let h = head.height();
        assert!(h > 0.0);
        assert!((h - head.ctx.cell_h() * APP_HEAD_HEIGHT_CELLS).abs() < 0.001);
    }

    #[test]
    fn body_rect_is_outer_minus_header() {
        let head = AppHead::new("AGENT", "");
        let outer = rect();
        let body = head.body_rect(&outer);
        assert_eq!(body.x, outer.x);
        assert!(body.y > outer.y);
        assert!(body.height < outer.height);
        assert!((body.y - outer.y - head.height()).abs() < 0.001);
    }

    #[test]
    fn empty_head_emits_only_background_and_divider() {
        let head = AppHead::default();
        let quads = head.render_quads(&rect());
        assert_eq!(quads.len(), 2); // bg + divider
        let texts = head.render_text(&rect());
        assert!(texts.is_empty());
    }

    #[test]
    fn name_is_uppercased() {
        let head = AppHead::new("agent", "");
        let texts = head.render_text(&rect());
        assert!(texts.iter().any(|s| s.text == "AGENT"));
    }

    #[test]
    fn icon_and_name_and_title_all_emit_segments() {
        let head = AppHead::new("agent", "claude · conversational").with_icon("◆");
        let texts = head.render_text(&rect());
        let joined: String = texts.iter().map(|s| s.text.as_str()).collect();
        assert!(joined.contains("◆"));
        assert!(joined.contains("AGENT"));
        assert!(joined.contains("claude"));
    }

    #[test]
    fn meta_renders_right_aligned() {
        let head = AppHead::new("term", "zsh").with_meta("203x58");
        let texts = head.render_text(&rect());
        let meta = texts
            .iter()
            .find(|s| s.text == "203x58")
            .expect("meta segment must be present");
        let cell_w = head.ctx.cell_w();
        let right_edge = meta.x + meta.text.chars().count() as f32 * cell_w;
        assert!(
            right_edge > rect().width * 0.7,
            "meta should be right-aligned, right_edge={right_edge}"
        );
    }

    #[test]
    fn dot_emits_extra_quad_when_meta_present() {
        let with_dot = AppHead::new("agent", "x").with_meta("live").with_dot(AppHeadDot::Live);
        let without_dot = AppHead::new("agent", "x").with_meta("live");
        assert!(with_dot.render_quads(&rect()).len() > without_dot.render_quads(&rect()).len());
    }

    #[test]
    fn dot_skipped_when_meta_empty() {
        let head = AppHead::new("agent", "x").with_dot(AppHeadDot::Live);
        // Background + divider only, no dot because meta is empty.
        assert_eq!(head.render_quads(&rect()).len(), 2);
    }

    #[test]
    fn focused_header_has_different_bg() {
        let unfocused = AppHead::new("agent", "");
        let focused = AppHead::new("agent", "").focused(true);
        let bg_u = unfocused.render_quads(&rect())[0].color;
        let bg_f = focused.render_quads(&rect())[0].color;
        assert_ne!(bg_u, bg_f);
    }

    /// Theme propagation: swapping the tokens snapshot must change the
    /// header bg color in the next render. This is the contract behind
    /// "switch themes and every adapter redraws."
    #[test]
    fn theme_swap_propagates_to_header_bg() {
        let phosphor_tokens = Tokens::phosphor(RenderCtx::fallback());

        // Build a contrasting palette — pure-blue recessed surface.
        let mut blue_roles = ColorRoles::phosphor();
        blue_roles.surface_recessed = [0.0, 0.0, 1.0, 1.0];
        let blue_tokens = Tokens::new(blue_roles, RenderCtx::fallback());

        let head_p = AppHead::new("agent", "").with_tokens(phosphor_tokens);
        let head_b = AppHead::new("agent", "").with_tokens(blue_tokens);

        let bg_p = head_p.render_quads(&rect())[0].color;
        let bg_b = head_b.render_quads(&rect())[0].color;

        assert_ne!(bg_p, bg_b);
        assert!(
            bg_b[2] > 0.9,
            "blue theme: header bg blue channel must dominate, got {bg_b:?}"
        );
    }

    /// Dot color must come from the active token palette.
    #[test]
    fn dot_color_follows_status_token() {
        let mut roles = ColorRoles::phosphor();
        roles.status_warn = [1.0, 0.0, 0.0, 1.0]; // override to pure red
        let tokens = Tokens::new(roles, RenderCtx::fallback());
        let head = AppHead::new("agent", "")
            .with_meta("hot")
            .with_dot(AppHeadDot::Warn)
            .with_tokens(tokens);
        let quads = head.render_quads(&rect());
        let dot = quads.last().expect("dot must be the last quad");
        assert_eq!(dot.color, [1.0, 0.0, 0.0, 1.0]);
    }
}
