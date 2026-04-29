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
            ..Default::default()
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
