//! Sec.8 — Top-of-screen notification banner.
//!
//! A thin, single-line banner that the renderer draws at the top of the
//! window when [`NotificationBanner::set_message`] has been called this
//! frame. It is the visible end of the [`crate::tokens::Tokens`] +
//! `phantom_app::notifications::NotificationCenter` pipeline:
//!
//! ```text
//!   Layer-2 dispatch gate
//!         │  EventKind::CapabilityDenied
//!         ▼
//!   denied_event_sink (Arc<Mutex<…>>)
//!         │  drained in update.rs
//!         ▼
//!   NotificationCenter::record_denial
//!         │  threshold reached
//!         ▼
//!   NotificationCenter::current_banner ──► NotificationBanner ──► chrome quads/text
//! ```
//!
//! The banner is stateless across frames: the App reads `current_banner`
//! every frame and pushes the message+severity into the widget right
//! before `render_quads` / `render_text` runs. When the center returns
//! `None`, [`NotificationBanner::clear`] hides the banner — `render_quads`
//! emits an empty `Vec` so the renderer's append-pattern is a no-op.
//!
//! Colors are sourced from [`crate::tokens::Tokens`]: `status_info` for
//! [`Severity::Info`], `status_warn` for `Warn`, `status_danger` for
//! `Danger`. No raw RGBA constants live in this file — themes can recolor
//! the entire banner just by mutating `ColorRoles`.

use crate::layout::Rect;
use crate::render_ctx::RenderCtx;
use crate::tokens::Tokens;
use crate::widgets::{TextSegment, Widget};
use phantom_renderer::quads::QuadInstance;

/// Banner severity, mirrored from
/// `phantom_app::notifications::Severity` so the UI crate doesn't need to
/// depend on `phantom-app`. The renderer maps these onto `Tokens`:
/// `Info → status_info`, `Warn → status_warn`, `Danger → status_danger`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerSeverity {
    Info,
    Warn,
    Danger,
}

/// Default banner height in physical pixels, matching the spec's "~32px".
///
/// Kept as a `pub const` so the App can subtract it from the chrome's
/// available area when the banner is active without re-reading widget
/// internals.
pub const NOTIFICATION_BANNER_HEIGHT: f32 = 32.0;

/// Top-of-window banner. Stateless across frames — the App calls
/// [`NotificationBanner::set_message`] when
/// `NotificationCenter::current_banner` is `Some` and
/// [`NotificationBanner::clear`] otherwise.
#[derive(Debug, Clone, Default)]
pub struct NotificationBanner {
    /// `None` when the banner is hidden (no active notification this frame).
    /// `Some` when the renderer should emit chrome for this banner.
    state: Option<BannerState>,
    /// Live render context for spacing/measurement. Defaults to
    /// `RenderCtx::fallback()` until the App threads the real one in.
    ctx: RenderCtx,
}

#[derive(Debug, Clone)]
struct BannerState {
    message: String,
    severity: BannerSeverity,
}

impl NotificationBanner {
    /// Construct an empty banner (hidden until `set_message` is called).
    pub fn new() -> Self {
        Self {
            state: None,
            ctx: RenderCtx::fallback(),
        }
    }

    /// Update the live render context so spacing reflects the current font.
    pub fn set_render_ctx(&mut self, ctx: RenderCtx) {
        self.ctx = ctx;
    }

    /// Show `message` at `severity` until [`Self::clear`] is called.
    pub fn set_message(&mut self, message: String, severity: BannerSeverity) {
        self.state = Some(BannerState { message, severity });
    }

    /// Hide the banner.
    pub fn clear(&mut self) {
        self.state = None;
    }

    /// `true` if there is an active banner waiting to render.
    pub fn is_visible(&self) -> bool {
        self.state.is_some()
    }

    /// Pixel height the banner occupies when visible. Returns `0.0` when
    /// hidden so the App can use this in layout math without a branch.
    pub fn height(&self) -> f32 {
        if self.is_visible() {
            NOTIFICATION_BANNER_HEIGHT
        } else {
            0.0
        }
    }

    /// Resolve severity → token color via the live `Tokens` table. Centralized
    /// so a theme swap recolors the banner without touching this widget.
    fn fg_for(&self, sev: BannerSeverity) -> [f32; 4] {
        let t = Tokens::phosphor(self.ctx);
        match sev {
            BannerSeverity::Info => t.colors.status_info,
            BannerSeverity::Warn => t.colors.status_warn,
            BannerSeverity::Danger => t.colors.status_danger,
        }
    }
}

impl Widget for NotificationBanner {
    /// Emits a single full-width background quad in `surface_recessed` plus
    /// a 2px accent stripe along the bottom in the severity color, so the
    /// banner reads as "top-pinned" without obscuring the underlying chrome.
    /// Hidden state → empty `Vec`.
    fn render_quads(&self, rect: &Rect) -> Vec<QuadInstance> {
        let Some(ref state) = self.state else {
            return Vec::new();
        };
        let t = Tokens::phosphor(self.ctx);
        let accent = self.fg_for(state.severity);
        let stripe_h = t.frame(); // 2.0 px

        vec![
            // Banner background (recessed surface so primary chrome can sit
            // on top without color clash).
            QuadInstance {
                pos: [rect.x, rect.y],
                size: [rect.width, rect.height],
                color: t.colors.surface_recessed,
                border_radius: 0.0,
            },
            // Severity accent stripe along the bottom edge.
            QuadInstance {
                pos: [rect.x, rect.y + rect.height - stripe_h],
                size: [rect.width, stripe_h],
                color: accent,
                border_radius: 0.0,
            },
        ]
    }

    /// Single centered-vertically text segment in the severity color, with
    /// `space_3()` left padding so the message doesn't crowd the edge under
    /// CRT barrel distortion.
    fn render_text(&self, rect: &Rect) -> Vec<TextSegment> {
        let Some(ref state) = self.state else {
            return Vec::new();
        };
        let t = Tokens::phosphor(self.ctx);
        let pad_x = t.space_3();
        // Center the text vertically. Baseline-ish: top-of-line sits half
        // the leading above the rect midline, matching how `StatusBar`
        // computes its `text_y`.
        let text_y = rect.y + (rect.height * 0.5) - (self.ctx.cell_h() * 0.5);

        vec![TextSegment {
            text: state.message.clone(),
            x: rect.x + pad_x,
            y: text_y,
            color: self.fg_for(state.severity),
        }]
    }
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            width: 1920.0,
            height: NOTIFICATION_BANNER_HEIGHT,
        }
    }

    #[test]
    fn hidden_renders_nothing() {
        let banner = NotificationBanner::new();
        assert!(!banner.is_visible());
        assert_eq!(banner.height(), 0.0);
        assert!(banner.render_quads(&rect()).is_empty());
        assert!(banner.render_text(&rect()).is_empty());
    }

    #[test]
    fn visible_emits_background_and_stripe() {
        let mut banner = NotificationBanner::new();
        banner.set_message("hello".into(), BannerSeverity::Danger);
        assert!(banner.is_visible());
        assert_eq!(banner.height(), NOTIFICATION_BANNER_HEIGHT);

        let quads = banner.render_quads(&rect());
        assert_eq!(quads.len(), 2, "background + accent stripe");
        // Stripe must sit at the bottom edge.
        let stripe = &quads[1];
        assert!(stripe.pos[1] > 0.0, "stripe is not at the top");
        assert!(stripe.pos[1] + stripe.size[1] <= NOTIFICATION_BANNER_HEIGHT + 0.01);
    }

    #[test]
    fn severity_drives_accent_color() {
        let ctx = RenderCtx::fallback();
        let tokens = Tokens::phosphor(ctx);

        let mut banner = NotificationBanner::new();

        banner.set_message("info".into(), BannerSeverity::Info);
        let info_quads = banner.render_quads(&rect());
        assert_eq!(info_quads[1].color, tokens.colors.status_info);

        banner.set_message("warn".into(), BannerSeverity::Warn);
        let warn_quads = banner.render_quads(&rect());
        assert_eq!(warn_quads[1].color, tokens.colors.status_warn);

        banner.set_message("danger".into(), BannerSeverity::Danger);
        let danger_quads = banner.render_quads(&rect());
        assert_eq!(danger_quads[1].color, tokens.colors.status_danger);
    }

    #[test]
    fn text_color_matches_severity_token() {
        let ctx = RenderCtx::fallback();
        let tokens = Tokens::phosphor(ctx);

        let mut banner = NotificationBanner::new();
        banner.set_message("danger msg".into(), BannerSeverity::Danger);

        let texts = banner.render_text(&rect());
        assert_eq!(texts.len(), 1);
        assert_eq!(texts[0].text, "danger msg");
        assert_eq!(texts[0].color, tokens.colors.status_danger);
    }

    #[test]
    fn clear_hides_banner() {
        let mut banner = NotificationBanner::new();
        banner.set_message("active".into(), BannerSeverity::Warn);
        assert!(banner.is_visible());

        banner.clear();
        assert!(!banner.is_visible());
        assert!(banner.render_quads(&rect()).is_empty());
    }
}
