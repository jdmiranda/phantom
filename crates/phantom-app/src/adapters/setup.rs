//! Setup adapter — the "waiting for API key / first-run setup" pane.
//!
//! Renders the centred `PHANTOM` wordmark, a status row with a dot, and a
//! hint line directing the user to the action that unblocks the app
//! (typically `phantom auth login` or setting `ANTHROPIC_API_KEY`).

use serde_json::json;

use phantom_adapter::adapter::{QuadData, Rect, RenderOutput, TextData};
use phantom_adapter::spatial::{InternalLayout, SpatialPreference};
use phantom_adapter::{
    AppCore, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned, Renderable,
};
use phantom_ui::tokens::Tokens;
use phantom_ui::widgets::{AppHead, AppHeadDot};
use phantom_ui::RenderCtx;

/// Current state of the setup pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupStatus {
    /// Blocked waiting for the user to supply something.
    Waiting,
    /// Setup completed; pane should dismiss itself.
    Ready,
    /// An error blocked setup.
    Failed,
}

impl SetupStatus {
    fn dot(self) -> AppHeadDot {
        match self {
            Self::Waiting => AppHeadDot::Warn,
            Self::Ready => AppHeadDot::Ok,
            Self::Failed => AppHeadDot::Danger,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

/// Setup pane.
pub struct SetupAdapter {
    status: SetupStatus,
    status_message: String,
    hint: String,
    tokens: Tokens,
    app_id: u32,
}

impl SetupAdapter {
    /// Build with a default `waiting for API key` state.
    #[must_use]
    pub fn new() -> Self {
        Self {
            status: SetupStatus::Waiting,
            status_message: "agent · waiting for API key".into(),
            hint: "set ANTHROPIC_API_KEY · or run `phantom auth login`".into(),
            tokens: Tokens::phosphor(RenderCtx::fallback()),
            app_id: 0,
        }
    }

    /// Update the live color palette. The host App calls this on theme switch.
    pub fn set_tokens(&mut self, tokens: Tokens) {
        self.tokens = tokens;
    }

    /// Update the visible status line.
    pub fn set_status(&mut self, status: SetupStatus, message: impl Into<String>) {
        self.status = status;
        self.status_message = message.into();
    }

    /// Replace the hint line shown below the status row.
    pub fn set_hint(&mut self, hint: impl Into<String>) {
        self.hint = hint.into();
    }

    /// Current status.
    #[must_use]
    pub fn status(&self) -> SetupStatus {
        self.status
    }
}

impl Default for SetupAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AppCore for SetupAdapter {
    fn app_type(&self) -> &str {
        "setup"
    }

    fn is_alive(&self) -> bool {
        self.status != SetupStatus::Ready
    }

    fn update(&mut self, _dt: f32) {}

    fn get_state(&self) -> serde_json::Value {
        json!({
            "type": "setup",
            "status": self.status.label(),
        })
    }

    fn title(&self) -> &str {
        "Setup"
    }
}

impl Renderable for SetupAdapter {
    fn render(&self, rect: &Rect) -> RenderOutput {
        let mut quads: Vec<QuadData> = Vec::new();
        let mut text_segments: Vec<TextData> = Vec::new();
        let t = self.tokens;

        // Render guard: once setup is Ready the pane is being torn down.
        // Emit chrome-only output so the App's compositor doesn't flash a
        // stale wordmark before despawn lands.
        if self.status == SetupStatus::Ready {
            let head = AppHead::new("SETUP", self.status.label())
                .with_icon("◌")
                .with_meta(self.status.label())
                .with_dot(self.status.dot())
                .with_tokens(t);
            head.render_into_adapter(rect, &mut quads, &mut text_segments);
            return RenderOutput {
                quads,
                text_segments,
                grid: None,
                scroll: None,
                selection: None,
            };
        }

        let head = AppHead::new("SETUP", self.status.label())
            .with_icon("◌")
            .with_meta(self.status.label())
            .with_dot(self.status.dot())
            .with_tokens(t);
        head.render_into_adapter(rect, &mut quads, &mut text_segments);

        let body = head.body_rect_adapter(rect);
        let cell_w = if rect.cell_size.0 > 0.0 { rect.cell_size.0 } else { 8.0 };
        let cell_h = if rect.cell_size.1 > 0.0 { rect.cell_size.1 } else { 16.0 };

        // Centred wordmark.
        let wordmark = "PHANTOM";
        let wordmark_w = wordmark.chars().count() as f32 * cell_w * 2.0;
        let wordmark_x = body.x + (body.width - wordmark_w) * 0.5;
        let wordmark_y = body.y + body.height * 0.32;
        text_segments.push(TextData {
            text: wordmark.to_string(),
            x: wordmark_x,
            y: wordmark_y,
            color: t.colors.text_accent,
        });

        // Status row.
        let status_w = self.status_message.chars().count() as f32 * cell_w;
        let status_x = body.x + (body.width - status_w) * 0.5;
        let status_y = wordmark_y + cell_h * 2.0;
        text_segments.push(TextData {
            text: self.status_message.clone(),
            x: status_x,
            y: status_y,
            color: t.colors.text_primary,
        });

        // Hint row.
        let hint_w = self.hint.chars().count() as f32 * cell_w;
        let hint_x = body.x + (body.width - hint_w) * 0.5;
        let hint_y = status_y + cell_h * 1.5;
        text_segments.push(TextData {
            text: self.hint.clone(),
            x: hint_x,
            y: hint_y,
            color: t.colors.text_dim,
        });

        RenderOutput {
            quads,
            text_segments,
            grid: None,
            scroll: None,
            selection: None,
        }
    }

    fn is_visual(&self) -> bool {
        true
    }

    fn spatial_preference(&self) -> Option<SpatialPreference> {
        Some(SpatialPreference {
            min_size: (50, 14),
            preferred_size: (80, 24),
            max_size: None,
            aspect_ratio: None,
            internal_panes: 1,
            internal_layout: InternalLayout::Single,
            priority: 10.0, // Setup wins layout while present.
        })
    }
}

impl InputHandler for SetupAdapter {
    fn handle_input(&mut self, _key: &str) -> bool {
        false
    }

    fn accepts_input(&self) -> bool {
        false
    }
}

impl Commandable for SetupAdapter {
    fn accept_command(&mut self, cmd: &str, args: &serde_json::Value) -> anyhow::Result<String> {
        match cmd {
            "set_status" => {
                let status = match args.get("status").and_then(|v| v.as_str()) {
                    Some("ready") => SetupStatus::Ready,
                    Some("failed") => SetupStatus::Failed,
                    _ => SetupStatus::Waiting,
                };
                let msg = args
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or(status.label());
                self.set_status(status, msg);
                Ok(json!({ "status": "ok" }).to_string())
            }
            "set_hint" => {
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("missing field: text"))?;
                self.set_hint(text);
                Ok(json!({ "status": "ok" }).to_string())
            }
            "ready" => {
                self.status = SetupStatus::Ready;
                Ok(json!({ "status": "ok" }).to_string())
            }
            "snapshot" => Ok(self.get_state().to_string()),
            other => Err(anyhow::anyhow!("unknown command: {other}")),
        }
    }
}

impl BusParticipant for SetupAdapter {}

impl Lifecycled for SetupAdapter {
    fn set_app_id(&mut self, id: u32) {
        self.app_id = id;
    }
}

impl Permissioned for SetupAdapter {}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 400.0,
            cell_size: (8.0, 16.0),
        }
    }

    #[test]
    fn app_type_is_setup() {
        assert_eq!(SetupAdapter::new().app_type(), "setup");
    }

    #[test]
    fn ready_marks_pane_no_longer_alive() {
        let mut s = SetupAdapter::new();
        assert!(s.is_alive());
        s.accept_command("ready", &json!({})).unwrap();
        assert!(!s.is_alive());
    }

    #[test]
    fn renders_phantom_wordmark() {
        let s = SetupAdapter::new();
        let out = s.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text == "PHANTOM"));
    }

    #[test]
    fn renders_status_message_and_hint() {
        let s = SetupAdapter::new();
        let out = s.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text.contains("waiting for API key")));
        assert!(out.text_segments.iter().any(|t| t.text.contains("ANTHROPIC_API_KEY")));
    }

    #[test]
    fn set_status_command_updates_state() {
        let mut s = SetupAdapter::new();
        s.accept_command(
            "set_status",
            &json!({ "status": "failed", "message": "auth blocked" }),
        )
        .unwrap();
        assert_eq!(s.status(), SetupStatus::Failed);
    }

    #[test]
    fn set_app_id_stores_id() {
        let mut s = SetupAdapter::new();
        s.set_app_id(42);
        assert_eq!(s.app_id, 42);
    }

    #[test]
    fn ready_render_skips_body() {
        let mut s = SetupAdapter::new();
        let before = s.render(&rect()).text_segments.len();
        s.accept_command("ready", &json!({})).unwrap();
        let after = s.render(&rect()).text_segments.len();
        assert!(
            after < before,
            "ready render must skip the body; got {after} vs {before}",
        );
    }

    #[test]
    fn theme_swap_propagates_to_wordmark() {
        use phantom_ui::tokens::ColorRoles;
        let s = SetupAdapter::new();
        let out_p = s.render(&rect());
        let mark_p = out_p
            .text_segments
            .iter()
            .find(|t| t.text == "PHANTOM")
            .map(|t| t.color)
            .expect("wordmark must render");

        let mut roles = ColorRoles::phosphor();
        roles.text_accent = [0.0, 0.0, 1.0, 1.0];
        let mut s2 = SetupAdapter::new();
        s2.set_tokens(Tokens::new(roles, RenderCtx::fallback()));
        let out_b = s2.render(&rect());
        let mark_b = out_b
            .text_segments
            .iter()
            .find(|t| t.text == "PHANTOM")
            .map(|t| t.color)
            .expect("wordmark must render");

        assert_ne!(mark_p, mark_b);
        assert!(mark_b[2] > 0.9);
    }
}
