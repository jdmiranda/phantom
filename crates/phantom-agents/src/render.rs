//! Agent pane rendering primitives.
//!
//! Each agent gets its own terminal pane with visual treatment that reflects
//! its lifecycle state: animated borders while working, green on success, red
//! on failure. This module provides the style definitions, animation math, and
//! text formatting needed by the renderer to draw agent panes.

use crate::agent::{Agent, AgentStatus, AgentTask};

// ---------------------------------------------------------------------------
// AgentPaneStyle
// ---------------------------------------------------------------------------

/// Visual style parameters for an agent pane.
///
/// Colors are stored as `[r, g, b, a]` in linear sRGB, range 0.0..1.0.
#[derive(Debug, Clone)]
pub struct AgentPaneStyle {
    /// Border color (may be animated at runtime).
    pub border_color: [f32; 4],
    /// Background color of the status header bar.
    pub header_bg: [f32; 4],
    /// Foreground (text) color of the status header bar.
    pub header_fg: [f32; 4],
    /// Color used for the status badge text.
    pub status_color: [f32; 4],
    /// Border pulse animation speed in radians per second.
    /// Set to `0.0` to disable pulsing.
    pub pulse_speed: f32,
}

impl AgentPaneStyle {
    /// Select the appropriate style for an agent's current status.
    pub fn for_status(status: AgentStatus) -> Self {
        match status {
            AgentStatus::Working | AgentStatus::WaitingForTool => Self::working(),
            AgentStatus::Queued => Self::queued(),
            AgentStatus::Planning => Self::planning(),
            AgentStatus::AwaitingApproval => Self::awaiting_approval(),
            AgentStatus::Done => Self::done(),
            AgentStatus::Failed | AgentStatus::Flatline => Self::failed(),
        }
    }

    /// Slow amber pulse — the agent is building its execution plan.
    fn planning() -> Self {
        Self {
            border_color: [0.95, 0.65, 0.0, 1.0],     // amber
            header_bg: [0.12, 0.09, 0.02, 1.0],        // dark amber-brown
            header_fg: [1.0, 0.85, 0.45, 1.0],         // light amber
            status_color: [0.95, 0.65, 0.0, 1.0],      // amber
            pulse_speed: 1.5,                           // slow gentle pulse
        }
    }

    /// Steady amber — plan is ready, waiting for user approval badge.
    fn awaiting_approval() -> Self {
        Self {
            border_color: [0.95, 0.75, 0.1, 1.0],     // bright amber
            header_bg: [0.14, 0.10, 0.02, 1.0],        // dark amber-brown
            header_fg: [1.0, 0.90, 0.55, 1.0],         // light amber-yellow
            status_color: [0.95, 0.75, 0.1, 1.0],      // bright amber
            pulse_speed: 0.0,                           // no pulse — static badge
        }
    }

    /// Animated cyan border with a steady pulse — the agent is thinking.
    fn working() -> Self {
        Self {
            border_color: [0.0, 0.85, 0.95, 1.0],   // cyan
            header_bg: [0.05, 0.12, 0.15, 1.0],       // dark teal
            header_fg: [0.7, 0.95, 1.0, 1.0],         // light cyan
            status_color: [0.0, 0.85, 0.95, 1.0],     // cyan
            pulse_speed: 3.0,                          // ~0.5 Hz visible pulse
        }
    }

    /// Dim gray, no animation — waiting in the queue.
    fn queued() -> Self {
        Self {
            border_color: [0.35, 0.35, 0.35, 1.0],    // gray
            header_bg: [0.08, 0.08, 0.08, 1.0],       // near-black
            header_fg: [0.55, 0.55, 0.55, 1.0],       // mid-gray
            status_color: [0.45, 0.45, 0.45, 1.0],    // gray
            pulse_speed: 0.0,
        }
    }

    /// Solid green — finished successfully.
    fn done() -> Self {
        Self {
            border_color: [0.2, 0.9, 0.3, 1.0],       // green
            header_bg: [0.05, 0.15, 0.06, 1.0],       // dark green
            header_fg: [0.6, 1.0, 0.65, 1.0],         // light green
            status_color: [0.2, 0.9, 0.3, 1.0],       // green
            pulse_speed: 0.0,
        }
    }

    /// Solid red — the agent errored out.
    fn failed() -> Self {
        Self {
            border_color: [0.95, 0.15, 0.15, 1.0],    // red
            header_bg: [0.18, 0.04, 0.04, 1.0],       // dark red
            header_fg: [1.0, 0.5, 0.5, 1.0],          // light red
            status_color: [0.95, 0.15, 0.15, 1.0],    // red
            pulse_speed: 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Animation helpers
// ---------------------------------------------------------------------------

/// Compute the animated border color for the current frame.
///
/// Applies a sine-wave brightness pulse controlled by
/// [`AgentPaneStyle::pulse_speed`]. When `pulse_speed` is `0.0` the color
/// is returned unchanged.
///
/// `elapsed` is the time in seconds since the application started (or any
/// monotonically-increasing clock value).
pub fn animated_border_color(style: &AgentPaneStyle, elapsed: f32) -> [f32; 4] {
    if style.pulse_speed == 0.0 {
        return style.border_color;
    }
    let pulse = (elapsed * style.pulse_speed).sin() * 0.15 + 0.85;
    [
        style.border_color[0] * pulse,
        style.border_color[1] * pulse,
        style.border_color[2] * pulse,
        style.border_color[3], // alpha stays constant
    ]
}

// ---------------------------------------------------------------------------
// Header / text formatting
// ---------------------------------------------------------------------------

/// Format the header line rendered above an agent pane.
///
/// Example output:
/// ```text
/// ■ AGENT #3 — Fix: build error [WORKING 4.2s]
/// ```
pub fn agent_header(agent: &Agent) -> String {
    let task_desc = match &agent.task {
        AgentTask::FixError { error_summary, .. } => {
            format!("Fix: {}", truncate(error_summary, 40))
        }
        AgentTask::RunCommand { command } => {
            format!("Run: {}", truncate(command, 40))
        }
        AgentTask::ReviewCode { .. } => "Code Review".to_string(),
        AgentTask::FreeForm { prompt } => truncate(prompt, 50),
        AgentTask::WatchAndNotify { description } => {
            format!("Watch: {}", truncate(description, 40))
        }
    };

    let status = match agent.status {
        AgentStatus::Queued => "QUEUED".to_string(),
        AgentStatus::Planning => format!("PLANNING {:.1}s", agent.elapsed().as_secs_f32()),
        AgentStatus::AwaitingApproval => "PENDING APPROVAL".to_string(),
        AgentStatus::Working => format!("WORKING {:.1}s", agent.elapsed().as_secs_f32()),
        AgentStatus::WaitingForTool => "TOOL CALL".to_string(),
        AgentStatus::Done => format!("DONE {:.1}s", agent.elapsed().as_secs_f32()),
        AgentStatus::Failed => "FAILED".to_string(),
        AgentStatus::Flatline => "FLATLINE".to_string(),
    };

    format!("\u{25a0} AGENT #{} \u{2014} {} [{}]", agent.id, task_desc, status)
}

/// Return the tail of the agent's output log as displayable text lines.
///
/// If the log has more than `max_lines` entries, only the most recent
/// `max_lines` are returned (the pane scrolls to the bottom).
pub fn agent_output_lines(agent: &Agent, max_lines: usize) -> Vec<String> {
    let log = &agent.output_log;
    if log.len() <= max_lines {
        log.clone()
    } else {
        log[log.len() - max_lines..].to_vec()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Truncate a string to at most `max` characters, appending "..." when cut.
fn truncate(s: &str, max: usize) -> String {
    if s.len() > max {
        format!("{}...", &s[..max])
    } else {
        s.to_owned()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, AgentStatus, AgentTask};
    use std::f32::consts::PI;

    // -- AgentPaneStyle tests -----------------------------------------------

    #[test]
    fn style_working_has_nonzero_pulse() {
        let style = AgentPaneStyle::for_status(AgentStatus::Working);
        assert!(style.pulse_speed > 0.0, "working style must animate");
    }

    #[test]
    fn style_waiting_for_tool_uses_working_style() {
        let working = AgentPaneStyle::for_status(AgentStatus::Working);
        let waiting = AgentPaneStyle::for_status(AgentStatus::WaitingForTool);
        assert_eq!(working.pulse_speed, waiting.pulse_speed);
        assert_eq!(working.border_color, waiting.border_color);
    }

    #[test]
    fn style_queued_has_no_pulse() {
        let style = AgentPaneStyle::for_status(AgentStatus::Queued);
        assert_eq!(style.pulse_speed, 0.0);
    }

    #[test]
    fn style_done_is_green() {
        let style = AgentPaneStyle::for_status(AgentStatus::Done);
        assert_eq!(style.pulse_speed, 0.0);
        // Green channel should dominate the border.
        assert!(
            style.border_color[1] > style.border_color[0],
            "done border should be greener than red"
        );
        assert!(
            style.border_color[1] > style.border_color[2],
            "done border should be greener than blue"
        );
    }

    #[test]
    fn style_failed_is_red() {
        let style = AgentPaneStyle::for_status(AgentStatus::Failed);
        assert_eq!(style.pulse_speed, 0.0);
        // Red channel should dominate.
        assert!(
            style.border_color[0] > style.border_color[1],
            "failed border should be redder than green"
        );
        assert!(
            style.border_color[0] > style.border_color[2],
            "failed border should be redder than blue"
        );
    }

    // -- animated_border_color tests ----------------------------------------

    #[test]
    fn animated_border_no_pulse_returns_original() {
        let style = AgentPaneStyle::for_status(AgentStatus::Queued);
        let color = animated_border_color(&style, 42.0);
        assert_eq!(color, style.border_color);
    }

    #[test]
    fn animated_border_pulse_varies_with_time() {
        let style = AgentPaneStyle::for_status(AgentStatus::Working);
        let c0 = animated_border_color(&style, 0.0);
        // At pi/(2*speed) the sine peaks at 1.0.
        let c_peak = animated_border_color(&style, PI / (2.0 * style.pulse_speed));
        // Colors should differ because the sine values differ.
        assert_ne!(c0, c_peak);
    }

    #[test]
    fn animated_border_alpha_stays_constant() {
        let style = AgentPaneStyle::for_status(AgentStatus::Working);
        for t in [0.0, 0.5, 1.0, 2.5, 10.0] {
            let color = animated_border_color(&style, t);
            assert_eq!(
                color[3], style.border_color[3],
                "alpha must not change at t={t}"
            );
        }
    }

    // -- agent_header tests -------------------------------------------------

    #[test]
    fn header_contains_agent_id_and_task() {
        let agent = Agent::new(
            7,
            AgentTask::FixError {
                error_summary: "type mismatch".into(),
                file: Some("lib.rs".into()),
                context: "cargo check".into(),
            },
        );
        let hdr = agent_header(&agent);
        assert!(hdr.contains("AGENT #7"), "header must show agent id");
        assert!(hdr.contains("Fix:"), "header must show task type");
        assert!(hdr.contains("type mismatch"), "header must show error summary");
        assert!(hdr.contains("QUEUED"), "new agent is queued");
    }

    #[test]
    fn header_run_command() {
        let agent = Agent::new(
            1,
            AgentTask::RunCommand {
                command: "cargo test --release".into(),
            },
        );
        let hdr = agent_header(&agent);
        assert!(hdr.contains("Run: cargo test --release"));
    }

    #[test]
    fn header_review_code() {
        let agent = Agent::new(
            2,
            AgentTask::ReviewCode {
                files: vec!["a.rs".into()],
                context: "pr review".into(),
            },
        );
        let hdr = agent_header(&agent);
        assert!(hdr.contains("Code Review"));
    }

    #[test]
    fn header_freeform_truncates_long_prompt() {
        let long = "x".repeat(100);
        let agent = Agent::new(1, AgentTask::FreeForm { prompt: long });
        let hdr = agent_header(&agent);
        assert!(hdr.contains("..."), "long prompt should be truncated");
    }

    #[test]
    fn header_watch() {
        let agent = Agent::new(
            5,
            AgentTask::WatchAndNotify {
                description: "CI status".into(),
            },
        );
        let hdr = agent_header(&agent);
        assert!(hdr.contains("Watch: CI status"));
    }

    #[test]
    fn header_working_status_shows_elapsed() {
        let mut agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test".into(),
            },
        );
        agent.status = AgentStatus::Working;
        let hdr = agent_header(&agent);
        assert!(hdr.contains("WORKING"), "should show WORKING status");
        assert!(hdr.contains("s"), "should show elapsed seconds");
    }

    #[test]
    fn header_failed_status() {
        let mut agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test".into(),
            },
        );
        agent.complete(false);
        let hdr = agent_header(&agent);
        assert!(hdr.contains("FAILED"));
    }

    #[test]
    fn header_tool_call_status() {
        let mut agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test".into(),
            },
        );
        agent.status = AgentStatus::WaitingForTool;
        let hdr = agent_header(&agent);
        assert!(hdr.contains("TOOL CALL"));
    }

    // -- agent_output_lines tests -------------------------------------------

    #[test]
    fn output_lines_returns_all_when_under_limit() {
        let mut agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test".into(),
            },
        );
        agent.log("line 1");
        agent.log("line 2");
        let lines = agent_output_lines(&agent, 10);
        assert_eq!(lines, vec!["line 1", "line 2"]);
    }

    #[test]
    fn output_lines_truncates_to_tail() {
        let mut agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test".into(),
            },
        );
        for i in 0..20 {
            agent.log(&format!("line {i}"));
        }
        let lines = agent_output_lines(&agent, 5);
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0], "line 15");
        assert_eq!(lines[4], "line 19");
    }

    #[test]
    fn output_lines_empty_log() {
        let agent = Agent::new(
            1,
            AgentTask::FreeForm {
                prompt: "test".into(),
            },
        );
        let lines = agent_output_lines(&agent, 10);
        assert!(lines.is_empty());
    }

    // -- truncate tests -----------------------------------------------------

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact_length_unchanged() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_long_string_adds_ellipsis() {
        let result = truncate("hello world", 5);
        assert_eq!(result, "hello...");
    }

    // -- FSM #34: Planning + AwaitingApproval styles --------------------------

    #[test]
    fn style_planning_has_nonzero_pulse() {
        let style = AgentPaneStyle::for_status(AgentStatus::Planning);
        assert!(style.pulse_speed > 0.0, "planning style must animate (slow amber pulse)");
    }

    #[test]
    fn style_planning_is_amber() {
        let style = AgentPaneStyle::for_status(AgentStatus::Planning);
        // Amber = high red, high green, low blue.
        assert!(
            style.border_color[0] > style.border_color[2],
            "planning border red channel must exceed blue (amber)"
        );
        assert!(
            style.border_color[1] > style.border_color[2],
            "planning border green channel must exceed blue (amber)"
        );
    }

    #[test]
    fn style_awaiting_approval_has_no_pulse() {
        // AwaitingApproval shows a static badge — no animation.
        let style = AgentPaneStyle::for_status(AgentStatus::AwaitingApproval);
        assert_eq!(style.pulse_speed, 0.0, "awaiting_approval style must not animate");
    }

    #[test]
    fn style_awaiting_approval_is_amber_family() {
        let style = AgentPaneStyle::for_status(AgentStatus::AwaitingApproval);
        // AwaitingApproval uses bright amber (same color family as Planning).
        assert!(
            style.border_color[0] > style.border_color[2],
            "awaiting_approval border red must exceed blue"
        );
        assert!(
            style.border_color[1] > style.border_color[2],
            "awaiting_approval border green must exceed blue"
        );
    }

    #[test]
    fn style_awaiting_approval_brighter_than_planning() {
        // The approval badge is intentionally brighter than the planning pulse.
        let planning = AgentPaneStyle::for_status(AgentStatus::Planning);
        let awaiting = AgentPaneStyle::for_status(AgentStatus::AwaitingApproval);
        let planning_lum = planning.border_color[0] + planning.border_color[1];
        let awaiting_lum = awaiting.border_color[0] + awaiting.border_color[1];
        assert!(
            awaiting_lum >= planning_lum,
            "awaiting_approval border must be at least as bright as planning"
        );
    }

    #[test]
    fn header_planning_status_shows_elapsed() {
        let mut agent = Agent::new(1, AgentTask::FreeForm { prompt: "test".into() });
        agent.begin_planning();
        let hdr = agent_header(&agent);
        assert!(hdr.contains("PLANNING"), "header must show PLANNING status");
        assert!(hdr.contains('s'), "header must show elapsed seconds");
    }

    #[test]
    fn header_awaiting_approval_status() {
        let mut agent = Agent::new(1, AgentTask::FreeForm { prompt: "test".into() });
        agent.begin_planning();
        agent.submit_plan_for_approval();
        let hdr = agent_header(&agent);
        assert!(hdr.contains("PENDING APPROVAL"), "header must show PENDING APPROVAL status");
    }
}
