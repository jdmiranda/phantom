//! Voice/STT adapter — render the live audio level + partial transcript.
//!
//! The adapter is "view-only" — it accepts level samples and transcript
//! deltas via commands. A real STT backend (phantom-stt, currently 🔧)
//! drives those commands when streaming Whisper or Deepgram lands.

use std::collections::VecDeque;

use serde_json::json;

use phantom_adapter::adapter::{QuadData, Rect, RenderOutput, TextData};
use phantom_adapter::spatial::{InternalLayout, SpatialPreference};
use phantom_adapter::{
    AppCore, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned, Renderable,
};
use phantom_ui::widgets::{AppHead, AppHeadDot};

/// How many recent audio-level samples drive the visualiser bars.
pub const LEVEL_BUFFER: usize = 32;

/// Voice / STT pane.
pub struct VoiceSttAdapter {
    levels: VecDeque<f32>,
    transcript_committed: String,
    transcript_partial: String,
    listening: bool,
    backend: String,
    app_id: u32,
}

impl VoiceSttAdapter {
    /// Build empty.
    #[must_use]
    pub fn new() -> Self {
        Self {
            levels: VecDeque::with_capacity(LEVEL_BUFFER),
            transcript_committed: String::new(),
            transcript_partial: String::new(),
            listening: false,
            backend: "mock".into(),
            app_id: 0,
        }
    }

    /// Append a level sample in `0.0..=1.0`.
    pub fn push_level(&mut self, level: f32) {
        if self.levels.len() == LEVEL_BUFFER {
            self.levels.pop_front();
        }
        self.levels.push_back(level.clamp(0.0, 1.0));
    }

    /// Replace the partial transcript (re-typed each delta).
    pub fn set_partial(&mut self, s: impl Into<String>) {
        self.transcript_partial = s.into();
    }

    /// Commit the partial transcript into the committed buffer with a leading space.
    pub fn commit_partial(&mut self) {
        if self.transcript_partial.is_empty() {
            return;
        }
        if !self.transcript_committed.is_empty() {
            self.transcript_committed.push(' ');
        }
        self.transcript_committed.push_str(&self.transcript_partial);
        self.transcript_partial.clear();
    }

    /// True when actively recording.
    #[must_use]
    pub fn is_listening(&self) -> bool {
        self.listening
    }

    /// True when no audio frames or transcript text are present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.levels.is_empty()
            && self.transcript_committed.is_empty()
            && self.transcript_partial.is_empty()
    }
}

impl Default for VoiceSttAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AppCore for VoiceSttAdapter {
    fn app_type(&self) -> &str {
        "voice-stt"
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn update(&mut self, _dt: f32) {}

    fn get_state(&self) -> serde_json::Value {
        json!({
            "type": "voice-stt",
            "listening": self.listening,
            "backend": self.backend,
            "committed_chars": self.transcript_committed.len(),
            "partial_chars": self.transcript_partial.len(),
        })
    }

    fn title(&self) -> &str {
        "Voice"
    }
}

impl Renderable for VoiceSttAdapter {
    fn render(&self, rect: &Rect) -> RenderOutput {
        let mut quads: Vec<QuadData> = Vec::new();
        let mut text_segments: Vec<TextData> = Vec::new();

        let meta = if self.listening { "listening".to_string() } else { "idle".to_string() };
        let dot = if self.listening { AppHeadDot::Live } else { AppHeadDot::Info };
        let head = AppHead::new("VOICE", format!("stt · {}", self.backend))
            .with_icon("◉")
            .with_meta(meta)
            .with_dot(dot);
        head.render_into_adapter(rect, &mut quads, &mut text_segments);

        let body = head.body_rect_adapter(rect);
        let cell_w = if rect.cell_size.0 > 0.0 { rect.cell_size.0 } else { 8.0 };
        let cell_h = if rect.cell_size.1 > 0.0 { rect.cell_size.1 } else { 16.0 };

        // Level bars across the top of the body.
        let bar_strip_h = cell_h * 2.0;
        let bar_strip_y = body.y + cell_h * 0.5;
        let bar_width = 3.0;
        let bar_gap = 2.0;
        let total_bars = LEVEL_BUFFER as f32;
        let strip_x_start = body.x + cell_w;
        let strip_x_max = body.x + body.width - cell_w;
        let max_x_room = strip_x_max - strip_x_start;
        let drawable_bars = ((max_x_room + bar_gap) / (bar_width + bar_gap)).floor().min(total_bars) as usize;

        for (idx, &lvl) in self
            .levels
            .iter()
            .rev()
            .take(drawable_bars)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .enumerate()
        {
            let h = (bar_strip_h * lvl).max(2.0);
            let x = strip_x_start + (idx as f32) * (bar_width + bar_gap);
            let y = bar_strip_y + bar_strip_h - h;
            quads.push(QuadData {
                x,
                y,
                w: bar_width,
                h,
                color: [0.30, 1.00, 0.55, 0.95],
            });
        }

        // Transcript below the bars.
        let transcript_y = body.y + bar_strip_h + cell_h * 1.5;
        let committed_color = [0.85, 1.00, 0.95, 1.00];
        let partial_color = [0.50, 0.65, 0.55, 0.85];

        if !self.transcript_committed.is_empty() {
            text_segments.push(TextData {
                text: self.transcript_committed.clone(),
                x: body.x + cell_w,
                y: transcript_y,
                color: committed_color,
            });
        }
        if !self.transcript_partial.is_empty() {
            let y = transcript_y + cell_h;
            text_segments.push(TextData {
                text: format!("… {}", self.transcript_partial),
                x: body.x + cell_w,
                y,
                color: partial_color,
            });
        }

        if self.is_empty() && !self.listening {
            text_segments.push(TextData {
                text: "  press to dictate".to_string(),
                x: body.x + cell_w,
                y: transcript_y,
                color: partial_color,
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
            min_size: (40, 8),
            preferred_size: (60, 14),
            max_size: Some((120, 30)),
            aspect_ratio: None,
            internal_panes: 1,
            internal_layout: InternalLayout::Single,
            priority: 2.0,
        })
    }
}

impl InputHandler for VoiceSttAdapter {
    fn handle_input(&mut self, _key: &str) -> bool {
        false
    }

    fn accepts_input(&self) -> bool {
        false
    }
}

impl Commandable for VoiceSttAdapter {
    fn accept_command(&mut self, cmd: &str, args: &serde_json::Value) -> anyhow::Result<String> {
        match cmd {
            "start" => {
                self.listening = true;
                Ok(json!({ "status": "ok" }).to_string())
            }
            "stop" => {
                self.listening = false;
                Ok(json!({ "status": "ok" }).to_string())
            }
            "push_level" => {
                let v = args
                    .get("value")
                    .and_then(|v| v.as_f64())
                    .ok_or_else(|| anyhow::anyhow!("missing field: value"))?;
                self.push_level(v as f32);
                Ok(json!({ "status": "ok" }).to_string())
            }
            "set_partial" => {
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("missing field: text"))?;
                self.set_partial(text);
                Ok(json!({ "status": "ok" }).to_string())
            }
            "commit_partial" => {
                self.commit_partial();
                Ok(json!({ "status": "ok" }).to_string())
            }
            "set_backend" => {
                let name = args
                    .get("name")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("missing field: name"))?;
                self.backend = name.to_string();
                Ok(json!({ "status": "ok" }).to_string())
            }
            "clear" => {
                self.levels.clear();
                self.transcript_committed.clear();
                self.transcript_partial.clear();
                Ok(json!({ "status": "ok" }).to_string())
            }
            "snapshot" => Ok(self.get_state().to_string()),
            other => Err(anyhow::anyhow!("unknown command: {other}")),
        }
    }
}

impl BusParticipant for VoiceSttAdapter {}

impl Lifecycled for VoiceSttAdapter {
    fn set_app_id(&mut self, id: u32) {
        self.app_id = id;
    }
}

impl Permissioned for VoiceSttAdapter {}

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
    fn app_type_is_voice_stt() {
        assert_eq!(VoiceSttAdapter::new().app_type(), "voice-stt");
    }

    #[test]
    fn start_stop_command_toggles_listening() {
        let mut a = VoiceSttAdapter::new();
        a.accept_command("start", &json!({})).unwrap();
        assert!(a.is_listening());
        a.accept_command("stop", &json!({})).unwrap();
        assert!(!a.is_listening());
    }

    #[test]
    fn push_level_caps_buffer_size() {
        let mut a = VoiceSttAdapter::new();
        for _ in 0..(LEVEL_BUFFER + 5) {
            a.push_level(0.5);
        }
        assert!(a.levels.len() <= LEVEL_BUFFER);
    }

    #[test]
    fn commit_partial_appends_with_space() {
        let mut a = VoiceSttAdapter::new();
        a.set_partial("hello");
        a.commit_partial();
        a.set_partial("world");
        a.commit_partial();
        assert_eq!(a.transcript_committed, "hello world");
    }

    #[test]
    fn empty_renders_press_to_dictate_hint() {
        let a = VoiceSttAdapter::new();
        let out = a.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text.contains("press to dictate")));
    }

    #[test]
    fn renders_app_head_with_listening_meta() {
        let mut a = VoiceSttAdapter::new();
        a.accept_command("start", &json!({})).unwrap();
        let out = a.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text == "listening"));
    }
}
