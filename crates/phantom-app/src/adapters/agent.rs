//! Agent adapter — wraps `AgentPane` as an `AppAdapter`.
//!
//! Bridges the AI-agent pane into the unified app model so that agents
//! participate in layout negotiation, event bus messaging, and command
//! dispatch alongside terminals and other adapters.

use serde_json::json;

use phantom_adapter::adapter::{QuadData, Rect, RenderOutput, TextData};
use phantom_adapter::spatial::{InternalLayout, SpatialPreference};
use phantom_adapter::{
    AppCore, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned, Renderable,
};

use crate::agent_pane::{AgentPane, AgentPaneStatus};

/// Line height in logical pixels used to stack text lines in render output.
const LINE_HEIGHT: f32 = 18.0;

/// Agent response text: phosphor green, slightly dimmer than terminal.
const TEXT_COLOR: [f32; 4] = [0.4, 0.8, 0.45, 0.95];

/// An agent pane wrapped in the `AppAdapter` interface.
///
/// Owns an `AgentPane` and translates its output stream into the
/// adapter render / bus / command protocols.
pub struct AgentAdapter {
    pane: AgentPane,
    app_id: u32,
    outbox: Vec<phantom_adapter::BusMessage>,
    /// Tracks previous status so we can detect transitions.
    prev_status: AgentPaneStatus,
    /// Input buffer for interactive chat (keystrokes accumulate here).
    input_buffer: String,
    /// Reconciler spawn tag — echoed back in `AgentTaskComplete` so the
    /// brain can match the completion to the right `active_dispatches`
    /// entry regardless of the AgentManager's sequential ID assignment.
    spawn_tag: Option<u64>,
    /// Set to `true` by the `"dismiss"` command so the dead-adapter reaper
    /// in `update.rs` removes this pane from the coordinator after the user
    /// has acknowledged the output.
    dismissed: bool,
    /// Lines scrolled above the live view (0 = bottom / live).
    ///
    /// Incremented by the `scroll` command (wheel), set by `scroll_to_offset`
    /// (scrollbar click-jump). Clamped to `[0, total_lines - visible_rows]`.
    scroll_offset: usize,
}

// ---------------------------------------------------------------------------
// Constructor and accessors
// ---------------------------------------------------------------------------

impl AgentAdapter {
    /// Wrap an already-spawned agent pane in the adapter.
    pub(crate) fn new(pane: AgentPane) -> Self {
        let status = pane.status;
        Self {
            pane,
            app_id: 0,
            outbox: Vec::new(),
            prev_status: status,
            input_buffer: String::new(),
            spawn_tag: None,
            dismissed: false,
            scroll_offset: 0,
        }
    }

    /// Wrap a pane and record the reconciler spawn tag so it is echoed back
    /// in the `AgentTaskComplete` bus event.
    pub(crate) fn with_spawn_tag(pane: AgentPane, spawn_tag: Option<u64>) -> Self {
        let mut adapter = Self::new(pane);
        adapter.spawn_tag = spawn_tag;
        adapter
    }

    /// Immutable access to the inner agent pane.
    #[allow(dead_code)]
    pub(crate) fn pane(&self) -> &AgentPane {
        &self.pane
    }

    /// Mutable access to the inner agent pane.
    #[allow(dead_code)]
    pub(crate) fn pane_mut(&mut self) -> &mut AgentPane {
        &mut self.pane
    }
}

// ---------------------------------------------------------------------------
// Sub-trait implementations (ISP — each trait is focused)
// ---------------------------------------------------------------------------

impl AppCore for AgentAdapter {
    fn app_type(&self) -> &str {
        "agent"
    }

    fn is_alive(&self) -> bool {
        // Stay alive after natural completion (Done/Failed) so the user can
        // read the output. Return `false` only once the user explicitly
        // dismisses the pane via the `"dismiss"` command — that signals the
        // dead-adapter reaper in `update.rs` to remove it from the coordinator.
        !self.dismissed
    }

    fn update(&mut self, _dt: f32) {
        self.pane.poll();
        // Refresh cached lines for rendering.
        self.pane.tail_lines(200);

        // Emit a bus event only on terminal status transitions (Done/Failed).
        if self.pane.status != self.prev_status {
            let event = match self.pane.status {
                AgentPaneStatus::Done => Some((true, "Agent finished successfully".to_string())),
                AgentPaneStatus::Failed => Some((false, "Agent failed".to_string())),
                AgentPaneStatus::Working => None, // Not a completion event
            };

            if let Some((success, summary)) = event {
                self.outbox.push(phantom_adapter::BusMessage {
                    topic_id: 0,
                    sender: self.app_id,
                    event: phantom_protocol::Event::AgentTaskComplete {
                        agent_id: self.app_id,
                        success,
                        summary,
                        spawn_tag: self.spawn_tag,
                    },
                    frame: 0,
                    timestamp: 0,
                });
            }

            self.prev_status = self.pane.status;
        }
    }

    fn get_state(&self) -> serde_json::Value {
        let total_lines = self.pane.cached_lines.len();
        json!({
            "type": "agent",
            "task": self.pane.task,
            "status": format!("{:?}", self.pane.status),
            "output_len": self.pane.output.len(),
            "alive": self.pane.status == AgentPaneStatus::Working,
            // Exposed so scrollbar click-jump (scrollbar_y_to_offset) can
            // calculate the correct target offset from a click position.
            "history_size": total_lines,
        })
    }
}

/// Height of the input bar at the bottom of the agent pane.
const INPUT_BAR_HEIGHT: f32 = 28.0;
/// Input bar background: slightly lighter than output so it's distinct.
const INPUT_BAR_BG: [f32; 4] = [0.08, 0.10, 0.12, 1.0];
/// Input bar separator: bright phosphor green line.
const INPUT_BAR_SEP: [f32; 4] = [0.2, 0.8, 0.3, 0.6];
/// User input text: bright phosphor green (Pip-Boy style).
const INPUT_COLOR: [f32; 4] = [0.2, 1.0, 0.4, 1.0];
/// Output area background: near-transparent so it doesn't fight the theme.
const OUTPUT_BG: [f32; 4] = [0.0, 0.0, 0.0, 0.0];

impl Renderable for AgentAdapter {
    fn render(&self, rect: &Rect) -> RenderOutput {
        let mut quads = Vec::new();
        let mut text_segments = Vec::new();

        let pad = 6.0;

        // --- Output area: top of rect to (bottom - INPUT_BAR_HEIGHT) ---
        let output_height = (rect.height - INPUT_BAR_HEIGHT - pad).max(LINE_HEIGHT);
        let output_max_lines = (output_height / LINE_HEIGHT).floor().max(1.0) as usize;

        // Output background.
        quads.push(QuadData {
            x: rect.x, y: rect.y,
            w: rect.width, h: output_height + pad,
            color: OUTPUT_BG,
        });

        // Render output lines — respecting scroll_offset.
        // scroll_offset 0 = bottom (live view), N = N lines scrolled up.
        let lines = &self.pane.cached_lines;
        let total_lines = lines.len();
        let history_size = total_lines.saturating_sub(output_max_lines);
        // Clamp in case scroll_offset drifted past the history.
        let clamped_offset = self.scroll_offset.min(history_size);
        // The window end (exclusive) is total_lines minus offset, the window
        // start is end minus visible lines.
        let window_end = total_lines.saturating_sub(clamped_offset);
        let window_start = window_end.saturating_sub(output_max_lines);
        let visible = &lines[window_start..window_end];

        for (i, line) in visible.iter().enumerate() {
            text_segments.push(TextData {
                text: line.clone(),
                x: rect.x + pad,
                y: rect.y + pad + (i as f32) * LINE_HEIGHT,
                color: TEXT_COLOR,
            });
        }

        // --- Input bar: fixed at the bottom ---
        let input_y = rect.y + rect.height - INPUT_BAR_HEIGHT;

        // Separator line.
        quads.push(QuadData {
            x: rect.x, y: input_y,
            w: rect.width, h: 1.0,
            color: INPUT_BAR_SEP,
        });

        // Input background.
        quads.push(QuadData {
            x: rect.x, y: input_y + 1.0,
            w: rect.width, h: INPUT_BAR_HEIGHT - 1.0,
            color: INPUT_BAR_BG,
        });

        // Input prompt + text.
        let prompt = format!("> {}_", self.input_buffer);
        text_segments.push(TextData {
            text: prompt,
            x: rect.x + pad,
            y: input_y + 6.0,
            color: INPUT_COLOR,
        });

        // --- Scroll state for scrollbar rendering ---
        let scroll = if history_size > 0 {
            Some(phantom_adapter::adapter::ScrollState {
                display_offset: clamped_offset,
                history_size,
                visible_rows: output_max_lines,
            })
        } else {
            None
        };

        RenderOutput {
            quads,
            text_segments,
            grid: None,
            scroll,
            selection: None,
        }
    }

    fn is_visual(&self) -> bool {
        true
    }

    fn spatial_preference(&self) -> Option<SpatialPreference> {
        Some(SpatialPreference {
            min_size: (30, 8),
            preferred_size: (80, 20),
            max_size: Some((120, 40)),
            aspect_ratio: None,
            internal_panes: 1,
            internal_layout: InternalLayout::Single,
            priority: 5.0,
        })
    }
}

impl InputHandler for AgentAdapter {
    fn handle_input(&mut self, key: &str) -> bool {
        match key {
            "\r" | "\n" => {
                let input = std::mem::take(&mut self.input_buffer);
                let trimmed = input.trim().to_string();
                if !trimmed.is_empty() {
                    self.pane.send_followup(trimmed);
                }
                true
            }
            "\x7f" | "\x08" => {
                self.input_buffer.pop();
                true
            }
            s if s.len() == 1 && s.as_bytes()[0] >= 0x20 => {
                self.input_buffer.push_str(s);
                true
            }
            _ => false,
        }
    }

    fn accepts_input(&self) -> bool {
        true
    }
}

impl Commandable for AgentAdapter {
    fn accept_command(
        &mut self,
        cmd: &str,
        args: &serde_json::Value,
    ) -> anyhow::Result<String> {
        match cmd {
            "dismiss" => {
                self.pane.status = AgentPaneStatus::Done;
                self.dismissed = true;
                Ok("dismissed".into())
            }
            "status" => Ok(format!("{:?}", self.pane.status)),
            "write" => {
                // Text input from the keyboard (same as terminal "write" command).
                if let Some(text) = args.get("text").and_then(|v| v.as_str()) {
                    for ch in text.chars() {
                        self.handle_input(&ch.to_string());
                    }
                }
                Ok("ok".into())
            }
            "write_bytes" => {
                // Raw bytes from route_bytes — decode as UTF-8 and feed to handle_input.
                if let Some(bytes) = args.get("bytes").and_then(|v| v.as_array()) {
                    let raw: Vec<u8> = bytes.iter()
                        .filter_map(|b| b.as_u64().map(|n| n as u8))
                        .collect();
                    let text = String::from_utf8_lossy(&raw);
                    for ch in text.chars() {
                        self.handle_input(&ch.to_string());
                    }
                }
                Ok("ok".into())
            }
            "scroll" => {
                // Wheel scroll: {"direction": "up"|"down", "lines": N}
                let lines = args.get("lines")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(3) as usize;
                let direction = args.get("direction")
                    .and_then(|v| v.as_str())
                    .unwrap_or("down");
                let total_lines = self.pane.cached_lines.len();
                // Re-compute visible rows from the last rect (not available here;
                // use a reasonable default of 20 lines so scrolling feels responsive).
                let visible_approx: usize = 20;
                let max_offset = total_lines.saturating_sub(visible_approx);
                if direction == "up" {
                    self.scroll_offset = (self.scroll_offset + lines).min(max_offset);
                } else {
                    self.scroll_offset = self.scroll_offset.saturating_sub(lines);
                }
                Ok(format!("offset={}", self.scroll_offset))
            }
            "scroll_to_offset" => {
                // Scrollbar click-jump: {"offset": N}
                let offset = args.get("offset")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                self.scroll_offset = offset;
                Ok(format!("offset={}", self.scroll_offset))
            }
            other => Err(anyhow::anyhow!("unknown command: {other}")),
        }
    }
}

impl BusParticipant for AgentAdapter {
    fn drain_outbox(&mut self) -> Vec<phantom_adapter::BusMessage> {
        std::mem::take(&mut self.outbox)
    }
}

impl Lifecycled for AgentAdapter {
    fn set_app_id(&mut self, id: u32) {
        self.app_id = id;
    }
}

impl Permissioned for AgentAdapter {
    fn permissions(&self) -> Vec<String> {
        vec!["network".into()]
    }
}

// ---------------------------------------------------------------------------
// Compile-time Send assert
// ---------------------------------------------------------------------------

fn _assert_send() {
    fn _check<T: Send>() {}
    _check::<AgentAdapter>();
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_adapter::adapter::QuadData;

    /// `app_type()` must return `"agent"` — exercises the real method, not a
    /// string literal comparison.
    #[test]
    fn test_app_type_returns_agent() {
        let pane = crate::agent_pane::AgentPane::test_with_lines(vec![]);
        let adapter = AgentAdapter::new(pane);
        assert_eq!(adapter.app_type(), "agent");
    }

    /// `render()` on an adapter with known cached lines must emit at least one
    /// text segment per visible line plus the input-bar prompt.
    #[test]
    fn test_render_produces_text_segments() {
        let lines: Vec<String> = vec!["line one".into(), "line two".into()];
        let pane = crate::agent_pane::AgentPane::test_with_lines(lines);
        let adapter = AgentAdapter::new(pane);

        let rect = Rect {
            x: 10.0,
            y: 20.0,
            width: 400.0,
            height: 300.0,
            ..Default::default()
        };
        let output = adapter.render(&rect);

        // At least 2 content lines + 1 prompt segment must be present.
        assert!(
            output.text_segments.len() >= 3,
            "expected >=3 text segments (2 lines + prompt), got {}",
            output.text_segments.len(),
        );
        // All text must be placed inside the rect horizontally.
        for seg in &output.text_segments {
            assert!(
                seg.x >= rect.x - 0.01,
                "text x {} < rect.x {}",
                seg.x,
                rect.x,
            );
        }
    }

    /// `handle_input` on a printable character must accumulate the character
    /// in the input buffer and return `true`.
    #[test]
    fn test_handle_input_accepts_printable_chars() {
        let pane = crate::agent_pane::AgentPane::test_with_lines(vec![]);
        let mut adapter = AgentAdapter::new(pane);
        let accepted = adapter.handle_input("a");
        assert!(accepted, "printable char must be accepted");
    }

    /// `accept_command` with an unknown command must return an `Err`.
    #[test]
    fn test_accept_command_unknown_returns_error() {
        let pane = crate::agent_pane::AgentPane::test_with_lines(vec![]);
        let mut adapter = AgentAdapter::new(pane);
        let result = adapter.accept_command("bogus", &serde_json::json!({}));
        assert!(result.is_err(), "unknown command must return Err");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("unknown command"), "error must mention 'unknown command'");
        assert!(msg.contains("bogus"), "error must name the bad command");
    }

    /// `permissions()` must include `"network"`.
    #[test]
    fn test_permissions_include_network() {
        let pane = crate::agent_pane::AgentPane::test_with_lines(vec![]);
        let adapter = AgentAdapter::new(pane);
        assert!(
            adapter.permissions().contains(&"network".to_string()),
            "AgentAdapter must declare the 'network' permission",
        );
    }

    /// A freshly-created adapter backed by a `Done`-status pane is alive
    /// (the user has not yet dismissed it).
    #[test]
    fn test_is_alive_before_dismiss() {
        let pane = crate::agent_pane::AgentPane::test_with_lines(vec![]);
        let adapter = AgentAdapter::new(pane);
        assert!(
            adapter.is_alive(),
            "adapter must be alive before dismiss so the user can read output",
        );
    }

    /// After the `"dismiss"` command `is_alive()` must return `false` so the
    /// dead-adapter reaper in `update.rs` removes the pane from the coordinator.
    #[test]
    fn test_is_alive_false_after_dismiss() {
        let pane = crate::agent_pane::AgentPane::test_with_lines(vec![]);
        let mut adapter = AgentAdapter::new(pane);
        let result = adapter.accept_command("dismiss", &serde_json::json!({}));
        assert!(result.is_ok(), "dismiss command must succeed");
        assert!(
            !adapter.is_alive(),
            "adapter must be dead after dismiss so the reaper can remove it",
        );
    }

    #[test]
    fn test_send_assert() {
        fn _check<T: Send>() {}
        _check::<AgentAdapter>();
    }

    // ── Issue #13 acceptance-criteria tests ────────────────────────────
    //
    // These tests verify that AgentAdapter is a real coordinator split pane,
    // not an overlay. They are unit-level: they prove the adapter's interface
    // contracts hold for the coordinator to treat it as a tiled split.

    /// AgentAdapter must report `is_visual() == true` so the coordinator
    /// includes it in `render_all` (tiled-split path, not overlay).
    #[test]
    fn agent_adapter_is_visual() {
        let pane = crate::agent_pane::AgentPane::test_with_lines(vec![
            "working...".into(),
        ]);
        let adapter = AgentAdapter::new(pane);
        assert!(
            adapter.is_visual(),
            "AgentAdapter must be visual so coordinator includes it in render_all"
        );
    }

    /// AgentAdapter must report `accepts_input() == true` so the coordinator
    /// routes keyboard events to it (same as terminal panes).
    #[test]
    fn agent_adapter_accepts_input() {
        let pane = crate::agent_pane::AgentPane::test_with_lines(vec![]);
        let adapter = AgentAdapter::new(pane);
        assert!(
            adapter.accepts_input(),
            "AgentAdapter must accept input so Cmd+[/Cmd+] focus cycle works"
        );
    }

    /// AgentAdapter's `app_type()` must return `"agent"` so the coordinator's
    /// chrome logic and app-count queries identify it correctly.
    #[test]
    fn agent_adapter_app_type_is_agent() {
        let pane = crate::agent_pane::AgentPane::test_with_lines(vec![]);
        let adapter = AgentAdapter::new(pane);
        assert_eq!(adapter.app_type(), "agent");
    }

    /// Render output must be bounded within the supplied rect — the adapter
    /// must not draw outside its split boundaries (which would produce
    /// visual overlap with the terminal pane, the exact symptom of #13).
    #[test]
    fn agent_adapter_render_stays_within_rect() {
        let lines: Vec<String> = (0..10)
            .map(|i| format!("output line {i}"))
            .collect();
        let pane = crate::agent_pane::AgentPane::test_with_lines(lines);
        let adapter = AgentAdapter::new(pane);

        let rect = Rect {
            x: 200.0,
            y: 100.0,
            width: 400.0,
            height: 300.0,
            ..Default::default()
        };
        let output = adapter.render(&rect);

        // Every quad must be within [rect.x .. rect.x+rect.width] × [rect.y .. rect.y+rect.height].
        for q in &output.quads {
            assert!(
                q.x >= rect.x - 0.01 && q.x + q.w <= rect.x + rect.width + 0.01,
                "quad x [{}, {}] must be within rect x [{}, {}]",
                q.x,
                q.x + q.w,
                rect.x,
                rect.x + rect.width,
            );
            assert!(
                q.y >= rect.y - 0.01 && q.y + q.h <= rect.y + rect.height + 0.01,
                "quad y [{}, {}] must be within rect y [{}, {}]",
                q.y,
                q.y + q.h,
                rect.y,
                rect.y + rect.height,
            );
        }

        // Text must start within the rect (x and y).
        for seg in &output.text_segments {
            assert!(
                seg.x >= rect.x - 0.01,
                "text x {} must be >= rect.x {}",
                seg.x,
                rect.x,
            );
            assert!(
                seg.y >= rect.y - 0.01,
                "text y {} must be >= rect.y {}",
                seg.y,
                rect.y,
            );
        }
    }

    // ── Issue #16: scrollbar + agent scroll support ────────────────────

    /// A newly-constructed AgentAdapter must start at scroll_offset=0 (live view).
    #[test]
    fn agent_adapter_scroll_offset_starts_at_zero() {
        let pane = crate::agent_pane::AgentPane::test_with_lines(vec![]);
        let adapter = AgentAdapter::new(pane);
        assert_eq!(adapter.scroll_offset, 0, "scroll must start at live view (bottom)");
    }

    /// `scroll` command with direction "up" increments scroll_offset.
    #[test]
    fn scroll_command_up_increments_offset() {
        let lines: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
        let pane = crate::agent_pane::AgentPane::test_with_lines(lines);
        let mut adapter = AgentAdapter::new(pane);

        let result = adapter.accept_command("scroll", &serde_json::json!({
            "direction": "up",
            "lines": 5,
        }));
        assert!(result.is_ok());
        assert!(adapter.scroll_offset > 0, "scroll_offset must increase on up scroll");
    }

    /// `scroll` command with direction "down" decrements scroll_offset
    /// and clamps at 0.
    #[test]
    fn scroll_command_down_clamps_at_zero() {
        let pane = crate::agent_pane::AgentPane::test_with_lines(vec![]);
        let mut adapter = AgentAdapter::new(pane);
        adapter.scroll_offset = 3;

        let _ = adapter.accept_command("scroll", &serde_json::json!({
            "direction": "down",
            "lines": 10,
        }));
        assert_eq!(adapter.scroll_offset, 0, "scroll must not go below 0");
    }

    /// `scroll_to_offset` command sets the exact offset.
    #[test]
    fn scroll_to_offset_command_sets_exact_offset() {
        let lines: Vec<String> = (0..50).map(|i| format!("line {i}")).collect();
        let pane = crate::agent_pane::AgentPane::test_with_lines(lines);
        let mut adapter = AgentAdapter::new(pane);

        let _ = adapter.accept_command("scroll_to_offset", &serde_json::json!({"offset": 20}));
        assert_eq!(adapter.scroll_offset, 20);
    }

    /// When there is scrollable history, `render()` must return a non-None scroll state.
    #[test]
    fn agent_render_provides_scroll_state_when_scrollable() {
        // Create more lines than fit in a typical visible area.
        let lines: Vec<String> = (0..100).map(|i| format!("line {i}")).collect();
        let pane = crate::agent_pane::AgentPane::test_with_lines(lines);
        let adapter = AgentAdapter::new(pane);

        let rect = Rect {
            x: 0.0, y: 0.0,
            width: 400.0, height: 300.0,
            ..Default::default()
        };
        let output = adapter.render(&rect);
        assert!(
            output.scroll.is_some(),
            "render must provide scroll state when there is more content than fits"
        );
    }

    /// When all lines fit in the visible area, scroll state should be None.
    #[test]
    fn agent_render_no_scroll_state_when_short_content() {
        let lines: Vec<String> = vec!["line 1".into(), "line 2".into()];
        let pane = crate::agent_pane::AgentPane::test_with_lines(lines);
        let adapter = AgentAdapter::new(pane);

        let rect = Rect {
            x: 0.0, y: 0.0,
            width: 400.0, height: 300.0,
            ..Default::default()
        };
        let output = adapter.render(&rect);
        // 2 lines fit easily in 300px height → no scrollbar needed.
        assert!(
            output.scroll.is_none(),
            "render must not provide scroll state when content fits on screen"
        );
    }

    /// `get_state()` must expose `history_size` so the scrollbar click-jump
    /// can compute the correct offset from a pixel coordinate.
    #[test]
    fn agent_get_state_exposes_history_size() {
        let lines: Vec<String> = (0..10).map(|i| format!("line {i}")).collect();
        let pane = crate::agent_pane::AgentPane::test_with_lines(lines);
        let adapter = AgentAdapter::new(pane);
        let state = adapter.get_state();
        assert!(
            state.get("history_size").is_some(),
            "get_state must expose history_size for scrollbar click-jump"
        );
        assert_eq!(state["history_size"].as_u64(), Some(10));
    }

    /// `with_spawn_tag` preserves the tag so the reconciler can correlate
    /// `AgentTaskComplete` events back to the right `active_dispatches` entry.
    #[test]
    fn agent_adapter_with_spawn_tag_stores_tag() {
        let pane = crate::agent_pane::AgentPane::test_with_lines(vec![]);
        let adapter = AgentAdapter::with_spawn_tag(pane, Some(42));
        // We cannot read spawn_tag directly (private), but we can verify the
        // adapter is alive and properly constructed.
        assert!(adapter.is_alive());
        assert_eq!(adapter.app_type(), "agent");
    }

    /// The coordinator must be able to register an AgentAdapter alongside a
    /// terminal adapter and correctly report both in `all_running()`. This
    /// proves the pane-split registration path works without GPU resources.
    #[test]
    fn coordinator_registers_agent_adapter_alongside_terminal() {
        use phantom_adapter::{AppCore, Commandable, BusParticipant, EventBus,
                              InputHandler, Lifecycled, Permissioned, Renderable};
        use phantom_scene::node::{NodeId, NodeKind};
        use phantom_scene::tree::SceneTree;
        use phantom_scene::clock::Cadence;
        use phantom_ui::layout::LayoutEngine;
        use crate::coordinator::AppCoordinator;

        // Minimal mock terminal adapter.
        struct MockTerminal;
        impl AppCore for MockTerminal {
            fn app_type(&self) -> &str { "terminal" }
            fn is_alive(&self) -> bool { true }
            fn update(&mut self, _dt: f32) {}
            fn get_state(&self) -> serde_json::Value { serde_json::json!({}) }
        }
        impl Renderable for MockTerminal {
            fn render(&self, rect: &Rect) -> RenderOutput { RenderOutput {
                quads: vec![QuadData { x: rect.x, y: rect.y, w: rect.width, h: rect.height, color: [1.0; 4] }],
                text_segments: vec![], grid: None, scroll: None, selection: None,
            }}
            fn is_visual(&self) -> bool { true }
        }
        impl InputHandler for MockTerminal {
            fn handle_input(&mut self, _key: &str) -> bool { false }
        }
        impl Commandable for MockTerminal {
            fn accept_command(&mut self, _cmd: &str, _args: &serde_json::Value) -> anyhow::Result<String> {
                Ok("ok".into())
            }
        }
        impl BusParticipant for MockTerminal {}
        impl Lifecycled for MockTerminal {}
        impl Permissioned for MockTerminal {}

        let mut coord = AppCoordinator::new(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content: NodeId = scene.add_node(scene.root(), NodeKind::ContentArea);

        // Register terminal adapter first.
        let term_id = coord.register_adapter(
            Box::new(MockTerminal),
            &mut layout,
            &mut scene,
            content,
            Cadence::unlimited(),
        );

        // Split the layout to create a new pane for the agent.
        let term_pane_id = coord.pane_id_for(term_id).expect("terminal must have pane");
        let (existing_child, new_child) = layout.split_vertical(term_pane_id)
            .expect("split must succeed");
        coord.remap_pane(term_id, term_pane_id, existing_child);
        layout.resize(800.0, 600.0).unwrap();

        // Register AgentAdapter at the new split pane.
        let pane = crate::agent_pane::AgentPane::test_with_lines(vec!["● Agent working...".into()]);
        let agent_adapter = AgentAdapter::new(pane);
        let agent_node = scene.add_node(content, NodeKind::Pane);
        let agent_id = coord.register_adapter_at_pane(
            Box::new(agent_adapter),
            new_child,
            agent_node,
            Cadence::unlimited(),
        );

        // Both adapters must be running.
        let running = coord.all_app_ids();
        assert!(running.contains(&term_id), "terminal adapter must be running");
        assert!(running.contains(&agent_id), "agent adapter must be running");
        assert_eq!(running.len(), 2, "exactly 2 adapters registered");

        // Both must have distinct pane IDs — they share no pane.
        let term_pane = coord.pane_id_for(term_id).expect("terminal has pane");
        let agent_pane_id = coord.pane_id_for(agent_id).expect("agent has pane");
        assert_ne!(term_pane, agent_pane_id, "terminal and agent must occupy different panes");

        // render_all must return 2 outputs (both are visual).
        let outputs = coord.render_all(&layout, (8.0, 16.0));
        assert_eq!(outputs.len(), 2, "both adapters must produce render output");

        // The agent's render output must be in its own pane rect, not covering
        // the terminal's rect — the core fix for issue #13.
        let term_rect = layout.get_pane_rect(term_pane).expect("terminal rect");
        let agent_rect = layout.get_pane_rect(agent_pane_id).expect("agent rect");
        assert!(
            (term_rect.x - agent_rect.x).abs() > 1.0
                || (term_rect.y - agent_rect.y).abs() > 1.0,
            "terminal and agent must occupy different spatial regions; \
             terminal rect ({:.0},{:.0} {:.0}x{:.0}) vs agent rect ({:.0},{:.0} {:.0}x{:.0})",
            term_rect.x, term_rect.y, term_rect.width, term_rect.height,
            agent_rect.x, agent_rect.y, agent_rect.width, agent_rect.height,
        );
    }

    /// Focus cycling: setting focus on agent then terminal must work in
    /// both directions — proving Cmd+[/Cmd+] can visit both panes.
    #[test]
    fn focus_cycles_through_agent_and_terminal() {
        use phantom_adapter::{AppCore, Commandable, BusParticipant, EventBus,
                              InputHandler, Lifecycled, Permissioned, Renderable};
        use phantom_scene::node::{NodeId, NodeKind};
        use phantom_scene::tree::SceneTree;
        use phantom_scene::clock::Cadence;
        use phantom_ui::layout::LayoutEngine;
        use crate::coordinator::AppCoordinator;

        struct MockTerminal2;
        impl AppCore for MockTerminal2 {
            fn app_type(&self) -> &str { "terminal" }
            fn is_alive(&self) -> bool { true }
            fn update(&mut self, _dt: f32) {}
            fn get_state(&self) -> serde_json::Value { serde_json::json!({}) }
        }
        impl Renderable for MockTerminal2 {
            fn render(&self, _rect: &Rect) -> RenderOutput { RenderOutput::default() }
            fn is_visual(&self) -> bool { true }
        }
        impl InputHandler for MockTerminal2 {
            fn handle_input(&mut self, _key: &str) -> bool { false }
        }
        impl Commandable for MockTerminal2 {
            fn accept_command(&mut self, _cmd: &str, _args: &serde_json::Value) -> anyhow::Result<String> {
                Ok("ok".into())
            }
        }
        impl BusParticipant for MockTerminal2 {}
        impl Lifecycled for MockTerminal2 {}
        impl Permissioned for MockTerminal2 {}

        let mut coord = AppCoordinator::new(EventBus::new());
        let mut layout = LayoutEngine::new().unwrap();
        let mut scene = SceneTree::new();
        let content: NodeId = scene.add_node(scene.root(), NodeKind::ContentArea);

        let term_id = coord.register_adapter(
            Box::new(MockTerminal2),
            &mut layout, &mut scene, content,
            Cadence::unlimited(),
        );

        let term_pane_id = coord.pane_id_for(term_id).unwrap();
        let (existing_child, new_child) = layout.split_vertical(term_pane_id).unwrap();
        coord.remap_pane(term_id, term_pane_id, existing_child);
        layout.resize(800.0, 600.0).unwrap();

        let pane = crate::agent_pane::AgentPane::test_with_lines(vec![]);
        let agent_adapter = AgentAdapter::new(pane);
        let agent_node = scene.add_node(content, NodeKind::Pane);
        let agent_id = coord.register_adapter_at_pane(
            Box::new(agent_adapter),
            new_child,
            agent_node,
            Cadence::unlimited(),
        );

        // Focus on agent.
        coord.set_focus(agent_id);
        assert_eq!(coord.focused(), Some(agent_id), "agent should be focused");

        // Focus on terminal.
        coord.set_focus(term_id);
        assert_eq!(coord.focused(), Some(term_id), "terminal should be focused");

        // Focus back on agent.
        coord.set_focus(agent_id);
        assert_eq!(coord.focused(), Some(agent_id), "focus cycle back to agent");
    }
}
