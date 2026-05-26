//! Settings adapter — render and mutate the live appearance config.
//!
//! Holds the in-memory `SettingsView` (theme, font, CRT shader params,
//! privacy mode). The body lays out one row per setting under the shared
//! `AppHead` chrome, with horizontal sliders for numeric values to match
//! the mockup. Mutations land via `accept_command` so the App can keep its
//! renderer/state in sync.

use serde_json::json;

use phantom_adapter::adapter::{QuadData, Rect, RenderOutput, TextData};
use phantom_adapter::spatial::{InternalLayout, SpatialPreference};
use phantom_adapter::{
    AppCore, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned, Renderable,
};
use phantom_ui::tokens::Tokens;
use phantom_ui::widgets::AppHead;
use phantom_ui::RenderCtx;

/// Numeric setting clamped to `[0.0, 1.0]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Slider01(pub f32);

impl Slider01 {
    /// Construct a clamped slider value.
    #[must_use]
    pub fn new(v: f32) -> Self {
        Self(v.clamp(0.0, 1.0))
    }
}

/// Live appearance + behavior settings rendered by the adapter.
#[derive(Debug, Clone)]
pub struct SettingsView {
    pub theme: String,
    pub font: String,
    pub font_size_pt: f32,
    pub scanlines: Slider01,
    pub bloom: Slider01,
    pub curvature: Slider01,
    pub privacy_mode: bool,
    /// CRT post-fx toggle — `false` disables scanlines/bloom/curvature
    /// regardless of slider values. Matches the mockup's checkbox.
    pub crt_enabled: bool,
}

impl Default for SettingsView {
    fn default() -> Self {
        Self {
            theme: "phosphor".into(),
            font: "JetBrains Mono".into(),
            font_size_pt: 14.0,
            scanlines: Slider01::new(0.6),
            bloom: Slider01::new(0.5),
            curvature: Slider01::new(0.1),
            privacy_mode: false,
            crt_enabled: true,
        }
    }
}

/// The visible settings pane.
pub struct SettingsAdapter {
    view: SettingsView,
    tokens: Tokens,
    app_id: u32,
    /// Reasons-to-redraw counter so consumers can detect mutations.
    revision: u64,
}

impl SettingsAdapter {
    /// Build with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            view: SettingsView::default(),
            tokens: Tokens::phosphor(RenderCtx::fallback()),
            app_id: 0,
            revision: 0,
        }
    }

    /// Build with a specific view (e.g. loaded from `config.toml`).
    #[must_use]
    pub fn with_view(view: SettingsView) -> Self {
        Self {
            view,
            tokens: Tokens::phosphor(RenderCtx::fallback()),
            app_id: 0,
            revision: 0,
        }
    }

    /// Update the live color palette. The host App calls this on theme switch
    /// so the next render picks up the new colors.
    pub fn set_tokens(&mut self, tokens: Tokens) {
        self.tokens = tokens;
    }

    /// Current view snapshot.
    #[must_use]
    pub fn view(&self) -> &SettingsView {
        &self.view
    }

    /// Monotonic revision counter that bumps every time a setting changes.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    fn bump(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
}

impl Default for SettingsAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AppCore for SettingsAdapter {
    fn app_type(&self) -> &str {
        "settings"
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn update(&mut self, _dt: f32) {}

    fn get_state(&self) -> serde_json::Value {
        json!({
            "type": "settings",
            "theme": self.view.theme,
            "font": self.view.font,
            "font_size_pt": self.view.font_size_pt,
            "scanlines": self.view.scanlines.0,
            "bloom": self.view.bloom.0,
            "curvature": self.view.curvature.0,
            "privacy_mode": self.view.privacy_mode,
            "crt_enabled": self.view.crt_enabled,
            "revision": self.revision,
        })
    }

    fn title(&self) -> &str {
        "Settings"
    }
}

impl Renderable for SettingsAdapter {
    fn render(&self, rect: &Rect) -> RenderOutput {
        let mut quads: Vec<QuadData> = Vec::new();
        let mut text_segments: Vec<TextData> = Vec::new();
        let t = self.tokens;

        let head = AppHead::new("SETTINGS", "appearance + agents")
            .with_icon("⚙")
            .with_tokens(t);
        head.render_into_adapter(rect, &mut quads, &mut text_segments);

        let body = head.body_rect_adapter(rect);
        let cell_w = if rect.cell_size.0 > 0.0 { rect.cell_size.0 } else { 8.0 };
        let cell_h = if rect.cell_size.1 > 0.0 { rect.cell_size.1 } else { 16.0 };

        let label_x = body.x + cell_w;
        let value_x = body.x + cell_w * 14.0; // 14ch label column to match mockup
        let mut y = body.y + cell_h * 0.5;

        let label_color = t.colors.text_secondary;
        let value_color = t.colors.text_primary;

        // Plain-text rows
        let push_text_row = |label: &str, value: String, y: &mut f32, segs: &mut Vec<TextData>| {
            segs.push(TextData {
                text: label.to_string(),
                x: label_x,
                y: *y,
                color: label_color,
            });
            segs.push(TextData {
                text: value,
                x: value_x,
                y: *y,
                color: value_color,
            });
            *y += cell_h;
        };

        push_text_row("theme", self.view.theme.clone(), &mut y, &mut text_segments);
        push_text_row(
            "font",
            format!("{} {}pt", self.view.font, self.view.font_size_pt as i32),
            &mut y,
            &mut text_segments,
        );

        // Slider rows: a 4-px-tall track + a small knob quad at the value.
        let track_color = t.colors.chrome_frame_dim;
        let knob_color = t.colors.status_ok;
        let push_slider_row =
            |label: &str, v: f32, y: &mut f32, quads: &mut Vec<QuadData>, segs: &mut Vec<TextData>| {
                segs.push(TextData {
                    text: label.to_string(),
                    x: label_x,
                    y: *y,
                    color: label_color,
                });
                let track_w = (body.x + body.width - value_x - cell_w).max(40.0);
                let track_y = *y + cell_h * 0.45;
                quads.push(QuadData {
                    x: value_x,
                    y: track_y,
                    w: track_w,
                    h: 2.0,
                    color: track_color,
                });
                let knob_size = 8.0;
                let knob_x = value_x + track_w * v - knob_size * 0.5;
                quads.push(QuadData {
                    x: knob_x,
                    y: track_y - knob_size * 0.5 + 1.0,
                    w: knob_size,
                    h: knob_size,
                    color: knob_color,
                });
                *y += cell_h;
            };

        push_slider_row(
            "scanlines",
            self.view.scanlines.0,
            &mut y,
            &mut quads,
            &mut text_segments,
        );
        push_slider_row(
            "bloom",
            self.view.bloom.0,
            &mut y,
            &mut quads,
            &mut text_segments,
        );
        push_slider_row(
            "curvature",
            self.view.curvature.0,
            &mut y,
            &mut quads,
            &mut text_segments,
        );

        push_text_row(
            "privacy mode",
            if self.view.privacy_mode { "on".into() } else { "off".into() },
            &mut y,
            &mut text_segments,
        );

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
            min_size: (40, 10),
            preferred_size: (60, 18),
            max_size: Some((100, 30)),
            aspect_ratio: None,
            internal_panes: 1,
            internal_layout: InternalLayout::Single,
            priority: 2.0,
        })
    }
}

impl InputHandler for SettingsAdapter {
    fn handle_input(&mut self, _key: &str) -> bool {
        false
    }

    fn accepts_input(&self) -> bool {
        false
    }
}

impl Commandable for SettingsAdapter {
    /// Settings commands:
    /// - `set_theme` (`{ "name": "amber" }`)
    /// - `set_font` (`{ "family": "...", "size_pt": 14.0 }`)
    /// - `set_scanlines` / `set_bloom` / `set_curvature` (`{ "value": 0.4 }`)
    /// - `set_privacy_mode` (`{ "enabled": true }`)
    /// - `snapshot` — return the current view as JSON.
    fn accept_command(&mut self, cmd: &str, args: &serde_json::Value) -> anyhow::Result<String> {
        let bump_and_ok = |this: &mut Self, label: &str| {
            this.bump();
            Ok::<String, anyhow::Error>(json!({ "status": "ok", "field": label }).to_string())
        };
        match cmd {
            "set_theme_name" => {
                let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(tokens) = Tokens::for_theme_name(name, RenderCtx::fallback()) {
                    self.set_tokens(tokens);
                }
                self.view.theme = name.to_string();
                bump_and_ok(self, "theme")
            }
            "set_theme" => {
                let name = args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("missing field: name"))?;
                self.view.theme = name.to_string();
                bump_and_ok(self, "theme")
            }
            "set_font" => {
                if let Some(family) = args.get("family").and_then(|v| v.as_str()) {
                    self.view.font = family.to_string();
                }
                if let Some(size) = args.get("size_pt").and_then(|v| v.as_f64()) {
                    self.view.font_size_pt = size as f32;
                }
                bump_and_ok(self, "font")
            }
            "set_scanlines" => {
                let v = args
                    .get("value")
                    .and_then(|v| v.as_f64())
                    .ok_or_else(|| anyhow::anyhow!("missing field: value"))?;
                self.view.scanlines = Slider01::new(v as f32);
                bump_and_ok(self, "scanlines")
            }
            "set_bloom" => {
                let v = args
                    .get("value")
                    .and_then(|v| v.as_f64())
                    .ok_or_else(|| anyhow::anyhow!("missing field: value"))?;
                self.view.bloom = Slider01::new(v as f32);
                bump_and_ok(self, "bloom")
            }
            "set_curvature" => {
                let v = args
                    .get("value")
                    .and_then(|v| v.as_f64())
                    .ok_or_else(|| anyhow::anyhow!("missing field: value"))?;
                self.view.curvature = Slider01::new(v as f32);
                bump_and_ok(self, "curvature")
            }
            "set_privacy_mode" => {
                let on = args.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
                self.view.privacy_mode = on;
                bump_and_ok(self, "privacy_mode")
            }
            "set_crt_enabled" => {
                let on = args.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
                self.view.crt_enabled = on;
                bump_and_ok(self, "crt_enabled")
            }
            "snapshot" => Ok(self.get_state().to_string()),
            other => Err(anyhow::anyhow!("unknown command: {other}")),
        }
    }
}

impl BusParticipant for SettingsAdapter {}

impl Lifecycled for SettingsAdapter {
    fn set_app_id(&mut self, id: u32) {
        self.app_id = id;
    }
}

impl Permissioned for SettingsAdapter {}

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
    fn slider01_clamps_out_of_range() {
        assert_eq!(Slider01::new(-1.0).0, 0.0);
        assert_eq!(Slider01::new(2.0).0, 1.0);
        assert!((Slider01::new(0.42).0 - 0.42).abs() < 1e-6);
    }

    #[test]
    fn renders_app_head_with_settings_label() {
        let a = SettingsAdapter::new();
        let out = a.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text == "SETTINGS"));
    }

    #[test]
    fn renders_theme_and_font_rows() {
        let a = SettingsAdapter::new();
        let out = a.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text == "theme"));
        assert!(out.text_segments.iter().any(|t| t.text == "font"));
    }

    #[test]
    fn set_theme_mutates_and_bumps_revision() {
        let mut a = SettingsAdapter::new();
        let r0 = a.revision();
        a.accept_command("set_theme", &json!({ "name": "amber" }))
            .unwrap();
        assert_eq!(a.view().theme, "amber");
        assert_eq!(a.revision(), r0 + 1);
    }

    #[test]
    fn set_scanlines_clamps_and_bumps() {
        let mut a = SettingsAdapter::new();
        a.accept_command("set_scanlines", &json!({ "value": 2.5 }))
            .unwrap();
        assert_eq!(a.view().scanlines.0, 1.0);
    }

    #[test]
    fn unknown_command_errors() {
        let mut a = SettingsAdapter::new();
        assert!(a.accept_command("nope", &json!({})).is_err());
    }

    #[test]
    fn snapshot_returns_full_state_json() {
        let mut a = SettingsAdapter::new();
        let resp = a.accept_command("snapshot", &json!({})).unwrap();
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(v["type"], "settings");
        assert!(v.get("scanlines").is_some());
    }

    #[test]
    fn set_app_id_stores_id() {
        let mut a = SettingsAdapter::new();
        a.set_app_id(42);
        assert_eq!(a.app_id, 42);
    }

    #[test]
    fn theme_swap_propagates_to_slider_knob() {
        use phantom_ui::tokens::ColorRoles;
        let mut a = SettingsAdapter::new();
        let out_p = a.render(&rect());

        let mut roles = ColorRoles::phosphor();
        roles.status_ok = [0.0, 0.0, 1.0, 1.0]; // pure-blue knob in alt theme
        a.set_tokens(Tokens::new(roles, RenderCtx::fallback()));
        let out_b = a.render(&rect());

        // Find a knob-sized 8x8 quad in each render — its color must differ.
        let knob_p = out_p
            .quads
            .iter()
            .find(|q| (q.w - 8.0).abs() < 0.01 && (q.h - 8.0).abs() < 0.01)
            .expect("phosphor knob must render");
        let knob_b = out_b
            .quads
            .iter()
            .find(|q| (q.w - 8.0).abs() < 0.01 && (q.h - 8.0).abs() < 0.01)
            .expect("blue knob must render");
        assert_ne!(knob_p.color, knob_b.color);
        assert!(knob_b.color[2] > 0.9);
    }
}
