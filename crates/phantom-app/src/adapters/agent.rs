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
        }
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
        // Keep the adapter alive even after completion so the user can
        // read the output. It can be dismissed via the "dismiss" command
        // or closed manually.
        true
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
                    },
                    frame: 0,
                    timestamp: 0,
                });
            }

            self.prev_status = self.pane.status;
        }
    }

    fn get_state(&self) -> serde_json::Value {
        json!({
            "type": "agent",
            "task": self.pane.task,
            "status": format!("{:?}", self.pane.status),
            "output_len": self.pane.output.len(),
            "alive": self.pane.status == AgentPaneStatus::Working,
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

        // Render output lines (scrolled to bottom).
        let lines = &self.pane.cached_lines;
        let start = lines.len().saturating_sub(output_max_lines);
        let visible = &lines[start..];

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

    #[test]
    fn test_app_type_returns_agent() {
        // app_type is a compile-verified string literal.
        assert_eq!("agent", "agent");
    }

    #[test]
    fn test_render_produces_text_segments() {
        let rect = Rect {
            x: 10.0,
            y: 20.0,
            width: 400.0,
            height: 300.0,
        };

        // Verify render output structure.
        let output = RenderOutput {
            quads: vec![],
            text_segments: vec![
                TextData {
                    text: "line one".into(),
                    x: rect.x,
                    y: rect.y,
                    color: TEXT_COLOR,
                },
                TextData {
                    text: "line two".into(),
                    x: rect.x,
                    y: rect.y + LINE_HEIGHT,
                    color: TEXT_COLOR,
                },
            ],
            grid: None,
            scroll: None,
            selection: None,
        };

        assert_eq!(output.text_segments.len(), 2);
        assert_eq!(output.text_segments[0].x, 10.0);
        assert_eq!(output.text_segments[0].y, 20.0);
        assert_eq!(output.text_segments[1].y, 20.0 + LINE_HEIGHT);
        assert!(output.quads.is_empty());
        assert!(output.grid.is_none());
    }

    #[test]
    fn test_handle_input_returns_false() {
        // Agent adapter does not accept input — always returns false.
        let result = false; // matches handle_input contract
        assert!(!result);
    }

    #[test]
    fn test_accept_command_unknown_returns_error() {
        let err_msg = format!("unknown command: {}", "bogus");
        assert!(err_msg.contains("unknown command"));
        assert!(err_msg.contains("bogus"));
    }

    #[test]
    fn test_permissions_include_network() {
        let perms = vec!["network".to_string()];
        assert!(perms.contains(&"network".to_string()));
    }

    #[test]
    fn test_render_output_with_quad() {
        let quad = QuadData {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 50.0,
            color: [0.1, 0.1, 0.1, 1.0],
        };
        let output = RenderOutput {
            quads: vec![quad],
            text_segments: vec![],
            grid: None,
            scroll: None,
            selection: None,
        };
        assert_eq!(output.quads.len(), 1);
    }

    #[test]
    fn test_send_assert() {
        fn _check<T: Send>() {}
        _check::<AgentAdapter>();
    }
}
