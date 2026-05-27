//! Voice/STT adapter — render the live audio level + partial transcript.
//!
//! The adapter is "view-only" — it accepts level samples and transcript
//! deltas via commands. A real STT backend (phantom-stt, currently 🔧)
//! drives those commands when streaming Whisper or Deepgram lands.
//!
//! # Backend readiness
//!
//! `phantom_app::stt::SttPipeline` is built at `App::new` when
//! `OPENAI_API_KEY` is present, so the backend tasks are alive — but
//! mic capture is **not** wired (see `SttPipeline::push_chunk`'s
//! `#[allow(dead_code)]` and issues #56 / #68). Until that lands, this
//! adapter does NOT synthesise fake level bars: it shows the mic
//! armed/disarmed indicator and the empty visualiser strip honestly,
//! so the operator can see at a glance that nothing is listening.

use std::collections::VecDeque;

use serde_json::json;

use phantom_adapter::adapter::{QuadData, Rect, RenderOutput, TextData};
use phantom_adapter::spatial::{InternalLayout, SpatialPreference};
use phantom_adapter::{
    AppCore, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned, Renderable,
};
use phantom_ui::tokens::Tokens;
use phantom_ui::widgets::{AppHead, AppHeadDot};
use phantom_ui::RenderCtx;

/// How many recent audio-level samples drive the visualiser bars.
pub const LEVEL_BUFFER: usize = 32;

/// Voice / STT pane.
pub struct VoiceSttAdapter {
    levels: VecDeque<f32>,
    transcript_committed: String,
    transcript_partial: String,
    listening: bool,
    /// Whether the operator has armed the mic in the OS-level sense
    /// (e.g. the `SttPipeline` exists and is accepting audio). Distinct
    /// from `listening`, which tracks the active recording state. When
    /// `mic_armed = false`, level bars are suppressed so the pane never
    /// shows synthetic activity while the mic is genuinely off.
    mic_armed: bool,
    backend: String,
    tokens: Tokens,
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
            mic_armed: false,
            backend: "mock".into(),
            tokens: Tokens::phosphor(RenderCtx::fallback()),
            app_id: 0,
        }
    }

    /// Update the live color palette. The host App calls this on theme switch.
    pub fn set_tokens(&mut self, tokens: Tokens) {
        self.tokens = tokens;
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

    /// True when the mic-armed flag is set. Used by the host App to
    /// reflect `SttPipeline::is_some()` honestly to the operator.
    #[must_use]
    pub fn is_mic_armed(&self) -> bool {
        self.mic_armed
    }

    /// Reflect whether the upstream `SttPipeline` is built and ready
    /// to receive audio. The host App passes
    /// `app.stt.is_some()` here at adapter spawn (and whenever the
    /// pipeline is recycled) so the pane's head meta and visualiser
    /// suppression are tied to the live backend instead of a guess.
    pub fn set_mic_armed(&mut self, armed: bool) {
        self.mic_armed = armed;
        if !armed {
            // Clear stale levels so the empty visualiser doesn't show
            // residual energy from a previous armed session.
            self.levels.clear();
        }
    }

    /// Override the backend label shown in the head subtitle. The host App
    /// passes a label like `"openai"` or `"mock"` based on whether
    /// `SttPipeline::build()` succeeded.
    pub fn set_backend(&mut self, backend: impl Into<String>) {
        self.backend = backend.into();
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
            "mic_armed": self.mic_armed,
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
        let t = self.tokens;

        // Head meta surfaces three honest states:
        //   * actively listening
        //   * mic armed (pipeline up, no audio yet)
        //   * mic disarmed (no `SttPipeline` — e.g. no API key)
        let (meta, dot) = if self.listening {
            ("listening".to_string(), AppHeadDot::Live)
        } else if self.mic_armed {
            ("armed".to_string(), AppHeadDot::Info)
        } else {
            ("disarmed".to_string(), AppHeadDot::Info)
        };
        let head = AppHead::new("VOICE", format!("stt · {}", self.backend))
            .with_icon("◉")
            .with_meta(meta)
            .with_dot(dot)
            .with_tokens(t)
            .focused(rect.focused);
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
        let drawable_bars = ((max_x_room + bar_gap) / (bar_width + bar_gap))
            .floor()
            .max(0.0)
            .min(total_bars) as usize;

        let total = self.levels.len();
        let skip = total.saturating_sub(drawable_bars);
        for (idx, &lvl) in self.levels.iter().skip(skip).enumerate() {
            let h = (bar_strip_h * lvl).max(2.0);
            let x = strip_x_start + (idx as f32) * (bar_width + bar_gap);
            let y = bar_strip_y + bar_strip_h - h;
            quads.push(QuadData {
                x,
                y,
                w: bar_width,
                h,
                color: t.colors.status_ok,
            });
        }

        // Transcript below the bars.
        let transcript_y = body.y + bar_strip_h + cell_h * 1.5;
        let committed_color = t.colors.text_accent;
        let partial_color = t.colors.text_dim;

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
            // Honest empty-state copy: which of the three "nothing happening"
            // modes are we in? Each gets its own hint so the operator never
            // sees a generic "press to dictate" when the mic literally cannot
            // capture audio yet.
            let hint = if !self.mic_armed {
                "  (mic disarmed — stt backend pending mic capture)".to_string()
            } else {
                "  (mic armed — no audio yet)".to_string()
            };
            text_segments.push(TextData {
                text: hint,
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
            "set_theme_name" => {
                let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(tokens) = Tokens::for_theme_name(name, RenderCtx::fallback()) {
                    self.set_tokens(tokens);
                }
                Ok(json!({ "status": "ok" }).to_string())
            }
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
            "set_mic_armed" => {
                let v = args
                    .get("armed")
                    .and_then(|v| v.as_bool())
                    .ok_or_else(|| anyhow::anyhow!("missing field: armed"))?;
                self.set_mic_armed(v);
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
            ..Default::default()
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
    fn empty_renders_mic_disarmed_hint() {
        // Fresh adapter has no SttPipeline arm yet — surface the disarmed
        // honest state, not synthetic activity.
        let a = VoiceSttAdapter::new();
        let out = a.render(&rect());
        assert!(
            out.text_segments
                .iter()
                .any(|t| t.text.contains("mic disarmed")),
            "fresh empty state must surface mic-disarmed hint"
        );
    }

    #[test]
    fn empty_armed_renders_no_audio_yet_hint() {
        // Armed but no levels yet: pipeline exists, mic capture isn't
        // delivering audio. Distinct from disarmed.
        let mut a = VoiceSttAdapter::new();
        a.set_mic_armed(true);
        let out = a.render(&rect());
        assert!(
            out.text_segments
                .iter()
                .any(|t| t.text.contains("no audio yet")),
            "armed empty state must surface the no-audio-yet hint"
        );
    }

    #[test]
    fn set_mic_armed_round_trips() {
        let mut a = VoiceSttAdapter::new();
        assert!(!a.is_mic_armed());
        a.set_mic_armed(true);
        assert!(a.is_mic_armed());
        a.set_mic_armed(false);
        assert!(!a.is_mic_armed());
    }

    #[test]
    fn disarming_clears_stale_levels() {
        // Drop levels when the mic is disarmed so the empty visualiser
        // never carries residual energy from a previous armed session.
        let mut a = VoiceSttAdapter::new();
        a.set_mic_armed(true);
        a.push_level(0.5);
        a.push_level(0.6);
        assert!(!a.levels.is_empty());
        a.set_mic_armed(false);
        assert!(a.levels.is_empty(), "disarming must clear stale levels");
    }

    #[test]
    fn head_meta_reflects_armed_disarmed_listening() {
        // Each of the three honest states must produce its own head meta
        // text so the operator can read the mic state at a glance.
        let mut a = VoiceSttAdapter::new();
        let out = a.render(&rect());
        assert!(
            out.text_segments.iter().any(|t| t.text == "disarmed"),
            "disarmed mic must show 'disarmed' in head meta"
        );

        a.set_mic_armed(true);
        let out = a.render(&rect());
        assert!(
            out.text_segments.iter().any(|t| t.text == "armed"),
            "armed mic must show 'armed' in head meta"
        );

        a.accept_command("start", &json!({})).unwrap();
        let out = a.render(&rect());
        assert!(
            out.text_segments.iter().any(|t| t.text == "listening"),
            "listening mic must show 'listening' in head meta"
        );
    }

    #[test]
    fn renders_app_head_with_listening_meta() {
        let mut a = VoiceSttAdapter::new();
        a.accept_command("start", &json!({})).unwrap();
        let out = a.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text == "listening"));
    }

    #[test]
    fn set_backend_updates_label() {
        let mut a = VoiceSttAdapter::new();
        a.set_backend("openai");
        let out = a.render(&rect());
        assert!(
            out.text_segments
                .iter()
                .any(|t| t.text.contains("openai")),
            "backend label override must appear in render"
        );
    }

    #[test]
    fn set_app_id_stores_id() {
        let mut a = VoiceSttAdapter::new();
        a.set_app_id(42);
        assert_eq!(a.app_id, 42);
    }

    #[test]
    fn drawable_bars_does_not_panic_on_narrow_rect() {
        let mut a = VoiceSttAdapter::new();
        a.push_level(0.5);
        let narrow = Rect { x: 0.0, y: 0.0, width: 30.0, height: 100.0, cell_size: (8.0, 16.0), ..Default::default() };
        // Should not panic regardless of how narrow the rect is.
        let _ = a.render(&narrow);
    }

    #[test]
    fn theme_swap_propagates_to_level_bar() {
        use phantom_ui::tokens::{ColorRoles, Tokens};
        let mut a = VoiceSttAdapter::new();
        a.push_level(0.8);
        let out_p = a.render(&rect());
        let bar_p = out_p
            .quads
            .iter()
            .find(|q| (q.w - 3.0).abs() < 0.01)
            .expect("level bar must render");

        let mut roles = ColorRoles::phosphor();
        roles.status_ok = [0.0, 0.0, 1.0, 1.0];
        a.set_tokens(Tokens::new(roles, RenderCtx::fallback()));
        let out_b = a.render(&rect());
        let bar_b = out_b
            .quads
            .iter()
            .find(|q| (q.w - 3.0).abs() < 0.01)
            .expect("level bar must render");

        assert_ne!(bar_p.color, bar_b.color);
        assert!(bar_b.color[2] > 0.9);
    }
}
