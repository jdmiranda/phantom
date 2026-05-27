//! Diff adapter — render git-style unified diff hunks with +/- coloring.
//!
//! Accepts a `DiffView` (file path + parsed lines) and renders one line per
//! row with severity coloring matching the mockup: `+` green, `-` red, ` `
//! dim. The file header row is the surface_floating chip from the mockup.

use serde_json::json;

use phantom_adapter::adapter::{QuadData, Rect, RenderOutput, TextData};
use phantom_adapter::spatial::{InternalLayout, SpatialPreference};
use phantom_adapter::{
    AppCore, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned, Renderable,
};
use phantom_ui::tokens::Tokens;
use phantom_ui::widgets::AppHead;
use phantom_ui::RenderCtx;

/// A single line in a unified diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    Add(String),
    Del(String),
    Ctx(String),
    /// `@@ hunk header @@` style separator.
    Hunk(String),
}

impl DiffLine {
    /// Parse a single text line into a `DiffLine`. Mirrors `git diff` markers.
    #[must_use]
    pub fn parse(line: &str) -> Self {
        if let Some(rest) = line.strip_prefix("+++").or_else(|| line.strip_prefix("---")) {
            // File header lines collapse into hunk markers; the file path
            // travels separately in DiffView so we just dim these.
            Self::Hunk(rest.trim().to_string())
        } else if line.starts_with("@@") {
            Self::Hunk(line.to_string())
        } else if let Some(rest) = line.strip_prefix('+') {
            Self::Add(rest.to_string())
        } else if let Some(rest) = line.strip_prefix('-') {
            Self::Del(rest.to_string())
        } else {
            Self::Ctx(line.trim_start_matches(' ').to_string())
        }
    }
}

/// Parsed diff view rendered by the adapter.
#[derive(Debug, Clone, Default)]
pub struct DiffView {
    pub file: String,
    pub plus: usize,
    pub minus: usize,
    pub lines: Vec<DiffLine>,
}

impl DiffView {
    /// Parse a unified diff text into a view.
    #[must_use]
    pub fn parse(file: impl Into<String>, body: &str) -> Self {
        let mut lines = Vec::new();
        let mut plus = 0usize;
        let mut minus = 0usize;
        for raw in body.lines() {
            let line = DiffLine::parse(raw);
            match &line {
                DiffLine::Add(_) => plus += 1,
                DiffLine::Del(_) => minus += 1,
                _ => {}
            }
            lines.push(line);
        }
        Self {
            file: file.into(),
            plus,
            minus,
            lines,
        }
    }
}

/// The diff pane.
pub struct DiffAdapter {
    view: DiffView,
    tokens: Tokens,
    app_id: u32,
}

impl DiffAdapter {
    /// Build with an explicit view.
    #[must_use]
    pub fn new(view: DiffView) -> Self {
        Self {
            view,
            tokens: Tokens::phosphor(RenderCtx::fallback()),
            app_id: 0,
        }
    }

    /// Build with no diff loaded.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(DiffView::default())
    }

    /// Replace the loaded view.
    pub fn set_view(&mut self, view: DiffView) {
        self.view = view;
    }

    /// Update the live color palette. The host App calls this on theme switch.
    pub fn set_tokens(&mut self, tokens: Tokens) {
        self.tokens = tokens;
    }
}

impl Default for DiffAdapter {
    fn default() -> Self {
        Self::empty()
    }
}

impl AppCore for DiffAdapter {
    fn app_type(&self) -> &str {
        "diff"
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn update(&mut self, _dt: f32) {}

    fn get_state(&self) -> serde_json::Value {
        json!({
            "type": "diff",
            "file": self.view.file,
            "plus": self.view.plus,
            "minus": self.view.minus,
            "lines": self.view.lines.len(),
        })
    }

    fn title(&self) -> &str {
        "Diff"
    }
}

impl Renderable for DiffAdapter {
    fn render(&self, rect: &Rect) -> RenderOutput {
        let mut quads: Vec<QuadData> = Vec::new();
        let mut text_segments: Vec<TextData> = Vec::new();
        let t = self.tokens;

        let title = if self.view.file.is_empty() {
            "no diff loaded".to_string()
        } else {
            self.view.file.clone()
        };
        let meta = format!("+{} -{}", self.view.plus, self.view.minus);
        let head = AppHead::new("DIFF", title)
            .with_icon("±")
            .with_meta(meta)
            .with_tokens(t)
            .focused(rect.focused);
        head.render_into_adapter(rect, &mut quads, &mut text_segments);

        let body = head.body_rect_adapter(rect);
        let cell_w = if rect.cell_size.0 > 0.0 { rect.cell_size.0 } else { 8.0 };
        let cell_h = if rect.cell_size.1 > 0.0 { rect.cell_size.1 } else { 16.0 };
        let mut y = body.y + cell_h * 0.3;

        let add_color = t.colors.status_ok;
        let del_color = t.colors.status_danger;
        let ctx_color = t.colors.text_dim;
        let hunk_color = t.colors.status_info;

        // Derive light bg fills by mixing the status colors with the body
        // background — a constant 8% alpha keeps the row tint subtle.
        let add_bg = with_alpha(t.colors.status_ok, 0.08);
        let del_bg = with_alpha(t.colors.status_danger, 0.08);

        for line in &self.view.lines {
            if y + cell_h > body.y + body.height {
                break;
            }
            let (prefix, color, bg) = match line {
                DiffLine::Add(_) => ("+", add_color, Some(add_bg)),
                DiffLine::Del(_) => ("-", del_color, Some(del_bg)),
                DiffLine::Ctx(_) => (" ", ctx_color, None),
                DiffLine::Hunk(_) => ("@", hunk_color, None),
            };
            if let Some(bg) = bg {
                quads.push(QuadData {
                    x: body.x,
                    y,
                    w: body.width,
                    h: cell_h,
                    color: bg,
                });
            }
            let text = match line {
                DiffLine::Add(s) | DiffLine::Del(s) | DiffLine::Ctx(s) | DiffLine::Hunk(s) => {
                    format!("{prefix} {s}")
                }
            };
            text_segments.push(TextData {
                text,
                x: body.x + cell_w,
                y,
                color,
            });
            y += cell_h;
        }

        if self.view.lines.is_empty() {
            text_segments.push(TextData {
                text: "  (no diff loaded)".to_string(),
                x: body.x + cell_w,
                y,
                color: ctx_color,
            });
        }

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
            preferred_size: (80, 24),
            max_size: None,
            aspect_ratio: None,
            internal_panes: 1,
            internal_layout: InternalLayout::Single,
            priority: 3.0,
        })
    }
}

impl InputHandler for DiffAdapter {
    fn handle_input(&mut self, _key: &str) -> bool {
        false
    }

    fn accepts_input(&self) -> bool {
        false
    }
}

impl Commandable for DiffAdapter {
    fn accept_command(&mut self, cmd: &str, args: &serde_json::Value) -> anyhow::Result<String> {
        match cmd {
            "set_theme_name" => {
                let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(tokens) = Tokens::for_theme_name(name, RenderCtx::fallback()) {
                    self.set_tokens(tokens);
                }
                Ok(json!({ "status": "ok" }).to_string())
            }
            "load" => {
                let file = args.get("file").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let body = args
                    .get("body")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("missing field: body"))?;
                self.view = DiffView::parse(file, body);
                Ok(json!({ "status": "ok", "plus": self.view.plus, "minus": self.view.minus }).to_string())
            }
            "clear" => {
                self.view = DiffView::default();
                Ok(json!({ "status": "ok" }).to_string())
            }
            "snapshot" => Ok(self.get_state().to_string()),
            other => Err(anyhow::anyhow!("unknown command: {other}")),
        }
    }
}

impl BusParticipant for DiffAdapter {}

impl Lifecycled for DiffAdapter {
    fn set_app_id(&mut self, id: u32) {
        self.app_id = id;
    }
}

impl Permissioned for DiffAdapter {}

fn with_alpha(c: [f32; 4], alpha: f32) -> [f32; 4] {
    [c[0], c[1], c[2], alpha]
}

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
            ..Default::default()
        }
    }

    const SAMPLE: &str = "\
@@ -1,4 +1,4 @@
 fn spatial_preference(&self) {
-    preferred_size: (80, 20),
-    max_size: Some((120, 40)),
+    preferred_size: (500, 200),
+    max_size: None,
 }
";

    #[test]
    fn parse_counts_plus_and_minus() {
        let v = DiffView::parse("agent.rs", SAMPLE);
        assert_eq!(v.plus, 2);
        assert_eq!(v.minus, 2);
    }

    #[test]
    fn diff_line_parse_recognises_markers() {
        assert!(matches!(DiffLine::parse("+ new"), DiffLine::Add(_)));
        assert!(matches!(DiffLine::parse("- old"), DiffLine::Del(_)));
        assert!(matches!(DiffLine::parse(" ctx"), DiffLine::Ctx(_)));
        assert!(matches!(DiffLine::parse("@@ x @@"), DiffLine::Hunk(_)));
    }

    #[test]
    fn renders_app_head_with_plus_minus_meta() {
        let a = DiffAdapter::new(DiffView::parse("agent.rs", SAMPLE));
        let out = a.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text == "+2 -2"));
        assert!(out.text_segments.iter().any(|t| t.text == "DIFF"));
    }

    #[test]
    fn empty_renders_hint() {
        let a = DiffAdapter::empty();
        let out = a.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text.contains("no diff")));
    }

    #[test]
    fn load_command_replaces_view() {
        let mut a = DiffAdapter::empty();
        a.accept_command(
            "load",
            &json!({ "file": "x.rs", "body": "+ foo\n- bar\n" }),
        )
        .unwrap();
        assert_eq!(a.view.plus, 1);
        assert_eq!(a.view.minus, 1);
    }

    #[test]
    fn set_app_id_stores_id() {
        let mut a = DiffAdapter::empty();
        a.set_app_id(42);
        assert_eq!(a.app_id, 42);
    }

    #[test]
    fn theme_swap_propagates_to_add_line_color() {
        use phantom_ui::tokens::ColorRoles;
        let mut a = DiffAdapter::new(DiffView::parse("x.rs", "+ added"));
        let out_p = a.render(&rect());
        let add_p = out_p
            .text_segments
            .iter()
            .find(|t| t.text.contains("added"))
            .map(|t| t.color)
            .expect("add row must render");

        let mut roles = ColorRoles::phosphor();
        roles.status_ok = [0.0, 0.0, 1.0, 1.0];
        a.set_tokens(Tokens::new(roles, RenderCtx::fallback()));
        let out_b = a.render(&rect());
        let add_b = out_b
            .text_segments
            .iter()
            .find(|t| t.text.contains("added"))
            .map(|t| t.color)
            .expect("add row must render");

        assert_ne!(add_p, add_b);
        assert!(add_b[2] > 0.9);
    }
}
