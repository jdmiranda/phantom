//! Boot adapter — the splash screen rendered during system startup phases.
//!
//! Renders the ASCII PHANTOM banner plus a checklist of boot phases. The
//! App ticks through phases by calling `accept_command "advance"`; the
//! adapter exits via `accept_command "finish"`.

use serde_json::json;

use phantom_adapter::adapter::{QuadData, Rect, RenderOutput, TextData};
use phantom_adapter::spatial::{InternalLayout, SpatialPreference};
use phantom_adapter::{
    AppCore, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned, Renderable,
};
use phantom_ui::widgets::AppHead;

/// ASCII PHANTOM banner — matches the mockup's `<pre>` block.
pub const PHANTOM_BANNER: &str = " ██████╗ ██╗  ██╗ █████╗ ███╗   ██╗████████╗ ██████╗ ███╗   ███╗\n\
 ██╔══██╗██║  ██║██╔══██╗████╗  ██║╚══██╔══╝██╔═══██╗████╗ ████║\n\
 ██████╔╝███████║███████║██╔██╗ ██║   ██║   ██║   ██║██╔████╔██║\n\
 ██╔═══╝ ██╔══██║██╔══██║██║╚██╗██║   ██║   ██║   ██║██║╚██╔╝██║\n\
 ██║     ██║  ██║██║  ██║██║ ╚████║   ██║   ╚██████╔╝██║ ╚═╝ ██║\n\
 ╚═╝     ╚═╝  ╚═╝╚═╝  ╚═╝╚═╝  ╚═══╝   ╚═╝    ╚═════╝ ╚═╝     ╚═╝";

/// State of a boot phase entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootCheckState {
    Pending,
    Running,
    Ok,
    Failed,
}

impl BootCheckState {
    fn marker(self) -> &'static str {
        match self {
            Self::Pending => "[ .. ]",
            Self::Running => "[ ›› ]",
            Self::Ok => "[ OK ]",
            Self::Failed => "[FAIL]",
        }
    }
    fn color(self) -> [f32; 4] {
        match self {
            Self::Pending => [0.30, 0.55, 0.40, 0.85],
            Self::Running => [0.40, 0.85, 1.00, 1.00],
            Self::Ok => [0.30, 1.00, 0.55, 1.00],
            Self::Failed => [1.00, 0.30, 0.25, 1.00],
        }
    }
}

/// One row in the boot checklist.
#[derive(Debug, Clone)]
pub struct BootCheck {
    pub label: String,
    pub state: BootCheckState,
}

impl BootCheck {
    /// Convenience constructor.
    #[must_use]
    pub fn new(label: impl Into<String>, state: BootCheckState) -> Self {
        Self {
            label: label.into(),
            state,
        }
    }
}

/// Boot pane.
pub struct BootAdapter {
    checks: Vec<BootCheck>,
    phase: usize,
    total_phases: usize,
    finished: bool,
    app_id: u32,
}

impl BootAdapter {
    /// Build with the default sequence of boot checks.
    #[must_use]
    pub fn new() -> Self {
        let checks = vec![
            BootCheck::new("gpu · Metal · M3 Max", BootCheckState::Ok),
            BootCheck::new("brain · ollama + claude", BootCheckState::Ok),
            BootCheck::new("supervisor handshake", BootCheckState::Ok),
            BootCheck::new("mcp discovery", BootCheckState::Running),
            BootCheck::new("plugins · scan", BootCheckState::Pending),
            BootCheck::new("memory · open", BootCheckState::Pending),
            BootCheck::new("session · restore", BootCheckState::Pending),
        ];
        let total_phases = checks.len();
        Self {
            checks,
            phase: 4,
            total_phases,
            finished: false,
            app_id: 0,
        }
    }

    /// Number of checks marked OK.
    #[must_use]
    pub fn ok_count(&self) -> usize {
        self.checks
            .iter()
            .filter(|c| c.state == BootCheckState::Ok)
            .count()
    }

    /// True once `finish` has been called.
    #[must_use]
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Bump the running check to OK and mark the next pending one as running.
    pub fn advance(&mut self) {
        let mut bumped = false;
        for c in self.checks.iter_mut() {
            if c.state == BootCheckState::Running {
                c.state = BootCheckState::Ok;
                bumped = true;
                break;
            }
        }
        if !bumped {
            return;
        }
        // Promote the next pending to running.
        for c in self.checks.iter_mut() {
            if c.state == BootCheckState::Pending {
                c.state = BootCheckState::Running;
                break;
            }
        }
        if self.phase < self.total_phases {
            self.phase += 1;
        }
    }
}

impl Default for BootAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AppCore for BootAdapter {
    fn app_type(&self) -> &str {
        "boot"
    }

    fn is_alive(&self) -> bool {
        !self.finished
    }

    fn update(&mut self, _dt: f32) {}

    fn get_state(&self) -> serde_json::Value {
        json!({
            "type": "boot",
            "phase": self.phase,
            "total": self.total_phases,
            "ok": self.ok_count(),
            "finished": self.finished,
        })
    }

    fn title(&self) -> &str {
        "Boot"
    }
}

impl Renderable for BootAdapter {
    fn render(&self, rect: &Rect) -> RenderOutput {
        let mut quads: Vec<QuadData> = Vec::new();
        let mut text_segments: Vec<TextData> = Vec::new();

        let head = AppHead::new("BOOT", "system check")
            .with_icon("⊙")
            .with_meta(format!("phase {} / {}", self.phase, self.total_phases));
        head.render_into_adapter(rect, &mut quads, &mut text_segments);

        let body = head.body_rect_adapter(rect);
        let cell_w = if rect.cell_size.0 > 0.0 { rect.cell_size.0 } else { 8.0 };
        let cell_h = if rect.cell_size.1 > 0.0 { rect.cell_size.1 } else { 16.0 };

        let banner_color = [0.65, 1.00, 0.80, 1.00];
        let mut y = body.y + cell_h * 0.5;
        for line in PHANTOM_BANNER.lines() {
            if y + cell_h > body.y + body.height - cell_h * (self.checks.len() as f32 + 1.0) {
                break;
            }
            text_segments.push(TextData {
                text: line.to_string(),
                x: body.x + cell_w,
                y,
                color: banner_color,
            });
            y += cell_h * 0.7;
        }

        y += cell_h * 0.6;
        for c in &self.checks {
            if y + cell_h > body.y + body.height {
                break;
            }
            text_segments.push(TextData {
                text: c.state.marker().to_string(),
                x: body.x + cell_w,
                y,
                color: c.state.color(),
            });
            text_segments.push(TextData {
                text: c.label.clone(),
                x: body.x + cell_w * 9.0,
                y,
                color: [0.85, 1.00, 0.95, 1.00],
            });
            y += cell_h;
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
            min_size: (60, 16),
            preferred_size: (80, 24),
            max_size: None,
            aspect_ratio: None,
            internal_panes: 1,
            internal_layout: InternalLayout::Single,
            priority: 5.0,
        })
    }
}

impl InputHandler for BootAdapter {
    fn handle_input(&mut self, _key: &str) -> bool {
        false
    }

    fn accepts_input(&self) -> bool {
        false
    }
}

impl Commandable for BootAdapter {
    fn accept_command(&mut self, cmd: &str, _args: &serde_json::Value) -> anyhow::Result<String> {
        match cmd {
            "advance" => {
                self.advance();
                Ok(json!({ "status": "ok", "phase": self.phase }).to_string())
            }
            "finish" => {
                for c in self.checks.iter_mut() {
                    if c.state == BootCheckState::Pending || c.state == BootCheckState::Running {
                        c.state = BootCheckState::Ok;
                    }
                }
                self.finished = true;
                Ok(json!({ "status": "ok" }).to_string())
            }
            "snapshot" => Ok(self.get_state().to_string()),
            other => Err(anyhow::anyhow!("unknown command: {other}")),
        }
    }
}

impl BusParticipant for BootAdapter {}

impl Lifecycled for BootAdapter {
    fn set_app_id(&mut self, id: u32) {
        self.app_id = id;
    }
}

impl Permissioned for BootAdapter {}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect() -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            width: 900.0,
            height: 500.0,
            cell_size: (8.0, 16.0),
        }
    }

    #[test]
    fn app_type_is_boot() {
        assert_eq!(BootAdapter::new().app_type(), "boot");
    }

    #[test]
    fn advance_progresses_running_check() {
        let mut b = BootAdapter::new();
        let before_ok = b.ok_count();
        b.advance();
        assert_eq!(b.ok_count(), before_ok + 1);
    }

    #[test]
    fn finish_marks_all_ok_and_sets_finished_flag() {
        let mut b = BootAdapter::new();
        b.accept_command("finish", &json!({})).unwrap();
        assert!(b.finished());
        assert_eq!(b.ok_count(), b.total_phases);
    }

    #[test]
    fn renders_banner_and_checks() {
        let b = BootAdapter::new();
        let out = b.render(&rect());
        // Banner — first line starts with a bunch of block characters.
        assert!(out.text_segments.iter().any(|t| t.text.contains('█')));
        // Check label — at least one of the default labels must appear.
        assert!(out
            .text_segments
            .iter()
            .any(|t| t.text.contains("supervisor")));
    }
}
