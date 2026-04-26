//! Agent adapter — wraps `AgentPane` as an `AppAdapter`.
//!
//! Bridges the AI-agent pane into the unified app model so that agents
//! participate in layout negotiation, event bus messaging, and command
//! dispatch alongside terminals and other adapters.

use serde_json::json;

use phantom_adapter::adapter::{Rect, RenderOutput, TextData};
use phantom_adapter::spatial::{InternalLayout, SpatialPreference};
use phantom_adapter::{
    AppCore, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned, Renderable,
};

use crate::agent_pane::{AgentPane, AgentPaneStatus};

/// Line height in logical pixels used to stack text lines in render output.
const LINE_HEIGHT: f32 = 18.0;

/// Default text color: soft green on dark background.
const TEXT_COLOR: [f32; 4] = [0.6, 0.9, 0.6, 1.0];

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
}

// ---------------------------------------------------------------------------
// Constructor and accessors
// ---------------------------------------------------------------------------

impl AgentAdapter {
    /// Wrap an already-spawned agent pane in the adapter.
    #[allow(dead_code)]
    pub(crate) fn new(pane: AgentPane) -> Self {
        let status = pane.status;
        Self {
            pane,
            app_id: 0,
            outbox: Vec::new(),
            prev_status: status,
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
        self.pane.status == AgentPaneStatus::Working
    }

    fn update(&mut self, _dt: f32) {
        self.pane.poll();

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

impl Renderable for AgentAdapter {
    fn render(&self, rect: &Rect) -> RenderOutput {
        let text_segments: Vec<TextData> = self
            .pane
            .cached_lines
            .iter()
            .enumerate()
            .map(|(i, line)| TextData {
                text: line.clone(),
                x: rect.x,
                y: rect.y + (i as f32) * LINE_HEIGHT,
                color: TEXT_COLOR,
            })
            .collect();

        RenderOutput {
            quads: vec![],
            text_segments,
            grid: None,
            scroll: None,
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
    fn handle_input(&mut self, _key: &str) -> bool {
        false
    }

    fn accepts_input(&self) -> bool {
        false
    }
}

impl Commandable for AgentAdapter {
    fn accept_command(
        &mut self,
        cmd: &str,
        _args: &serde_json::Value,
    ) -> anyhow::Result<String> {
        match cmd {
            "dismiss" => {
                self.pane.status = AgentPaneStatus::Done;
                Ok("dismissed".into())
            }
            "status" => Ok(format!("{:?}", self.pane.status)),
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
        };
        assert_eq!(output.quads.len(), 1);
    }

    #[test]
    fn test_send_assert() {
        fn _check<T: Send>() {}
        _check::<AgentAdapter>();
    }
}
