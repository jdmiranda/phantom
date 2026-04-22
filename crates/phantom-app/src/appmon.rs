//! App-internal diagnostics monitor — Phantom's own health metrics.
//!
//! Renders boot-style bars for frame time, pane count, buffer usage,
//! event bus depth, scene graph size, etc. Zero-cost: reads fields
//! already on the App struct, no syscalls, no allocations per frame.

use crate::app::App;

/// A snapshot of app-internal metrics, collected once per frame.
pub(crate) struct AppMetrics {
    pub fps: f32,
    pub frame_time_ms: f32,
    pub pane_count: usize,
    pub agent_count: usize,
    pub agent_working: usize,
    pub scene_nodes: usize,
    pub bus_queue_depth: usize,
    pub memory_entries: usize,
    pub pty_buf_bytes: usize,
    pub uptime_secs: u64,
    pub brain_active: bool,
    pub plugin_count: usize,
}

impl App {
    /// Collect current app metrics. Cheap — just field reads.
    pub(crate) fn collect_metrics(&self) -> AppMetrics {
        let now = std::time::Instant::now();
        let frame_time_ms = now.duration_since(self.last_frame).as_secs_f32() * 1000.0;
        let fps = if frame_time_ms > 0.0 { 1000.0 / frame_time_ms } else { 0.0 };

        let agent_working = self.agent_panes.iter()
            .filter(|p| p.status == crate::agent_pane::AgentPaneStatus::Working)
            .count();

        let pty_buf_bytes: usize = self.panes.iter()
            .map(|p| p.output_buf.len())
            .sum();

        let memory_entries = self.memory.as_ref()
            .map(|m| m.count())
            .unwrap_or(0);

        AppMetrics {
            fps,
            frame_time_ms,
            pane_count: self.panes.len(),
            agent_count: self.agent_panes.len(),
            agent_working,
            scene_nodes: self.scene.node_count(),
            bus_queue_depth: self.event_bus.queue_len(),
            memory_entries,
            pty_buf_bytes,
            uptime_secs: now.duration_since(self.start_time).as_secs(),
            brain_active: self.brain.is_some(),
            plugin_count: self.plugin_registry.len(),
        }
    }
}

/// Build display lines for the app monitor panel.
pub(crate) fn build_appmon_lines(m: &AppMetrics) -> Vec<(String, [f32; 4])> {
    let green = [0.2, 1.0, 0.5, 1.0];
    let yellow = [0.9, 0.9, 0.2, 1.0];
    let red = [1.0, 0.35, 0.2, 1.0];
    let cyan = [0.0, 0.8, 0.9, 1.0];
    let dim = [0.4, 0.7, 0.5, 0.7];

    // FPS bar: 60fps = full, color by threshold.
    let fps_frac = (m.fps / 60.0).clamp(0.0, 1.0);
    let fps_color = if m.fps >= 50.0 { green } else if m.fps >= 30.0 { yellow } else { red };

    // Frame time bar: 16ms = target, 33ms = yellow, 50ms+ = red.
    let ft_frac = (1.0 - (m.frame_time_ms / 50.0).clamp(0.0, 1.0)).max(0.0);
    let ft_color = if m.frame_time_ms <= 17.0 { green } else if m.frame_time_ms <= 33.0 { yellow } else { red };

    // PTY buffer: 8192 max, show usage.
    let pty_max = 8192.0 * m.pane_count.max(1) as f32;
    let pty_frac = (m.pty_buf_bytes as f32 / pty_max).clamp(0.0, 1.0);

    // Bus queue: 256 max.
    let bus_frac = (m.bus_queue_depth as f32 / 256.0).clamp(0.0, 1.0);
    let bus_color = if bus_frac < 0.5 { green } else if bus_frac < 0.8 { yellow } else { red };

    let bar = |frac: f32| -> String {
        let w = 20;
        let filled = (frac * w as f32).round() as usize;
        let empty = w - filled;
        format!("{}{}", "\u{2588}".repeat(filled), "\u{2591}".repeat(empty))
    };

    let brain_status = if m.brain_active { "ONLINE" } else { "OFFLINE" };
    let uptime = format_uptime(m.uptime_secs);

    vec![
        (format!("▮ {:<12} {} {:.0} fps", "FRAMERATE", bar(fps_frac), m.fps), fps_color),
        (format!("▮ {:<12} {} {:.1}ms", "FRAME TIME", bar(ft_frac), m.frame_time_ms), ft_color),
        (format!("▮ {:<12} {} {}/{}", "PTY BUFFER", bar(pty_frac), m.pty_buf_bytes, pty_max as usize), dim),
        (format!("▮ {:<12} {} {}/256", "EVENT BUS", bar(bus_frac), m.bus_queue_depth), bus_color),
        (format!(
            "▮ {:<12} panes:{} agents:{}/{} scene:{} mem:{} plugins:{}",
            "SUBSYSTEMS",
            m.pane_count,
            m.agent_working,
            m.agent_count,
            m.scene_nodes,
            m.memory_entries,
            m.plugin_count,
        ), cyan),
        (format!("▮ {:<12} brain:{} uptime:{}", "STATUS", brain_status, uptime), green),
    ]
}

fn format_uptime(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_metrics() -> AppMetrics {
        AppMetrics {
            fps: 60.0,
            frame_time_ms: 16.5,
            pane_count: 2,
            agent_count: 1,
            agent_working: 1,
            scene_nodes: 8,
            bus_queue_depth: 12,
            memory_entries: 5,
            pty_buf_bytes: 4096,
            uptime_secs: 3661,
            brain_active: true,
            plugin_count: 0,
        }
    }

    #[test]
    fn build_appmon_lines_produces_six_lines() {
        let lines = build_appmon_lines(&sample_metrics());
        assert_eq!(lines.len(), 6);
    }

    #[test]
    fn framerate_line_shows_fps() {
        let lines = build_appmon_lines(&sample_metrics());
        assert!(lines[0].0.contains("60 fps"));
        assert!(lines[0].0.contains("FRAMERATE"));
    }

    #[test]
    fn frame_time_line_shows_ms() {
        let lines = build_appmon_lines(&sample_metrics());
        assert!(lines[1].0.contains("16.5ms"));
    }

    #[test]
    fn subsystems_line_shows_counts() {
        let lines = build_appmon_lines(&sample_metrics());
        let sub = &lines[4].0;
        assert!(sub.contains("panes:2"));
        assert!(sub.contains("agents:1/1"));
        assert!(sub.contains("scene:8"));
        assert!(sub.contains("mem:5"));
    }

    #[test]
    fn status_line_shows_brain_and_uptime() {
        let lines = build_appmon_lines(&sample_metrics());
        assert!(lines[5].0.contains("ONLINE"));
        assert!(lines[5].0.contains("1h01m01s"));
    }

    #[test]
    fn format_uptime_formats() {
        assert_eq!(format_uptime(5), "5s");
        assert_eq!(format_uptime(65), "1m05s");
        assert_eq!(format_uptime(3661), "1h01m01s");
        assert_eq!(format_uptime(0), "0s");
    }

    #[test]
    fn low_fps_colors_red() {
        let mut m = sample_metrics();
        m.fps = 15.0;
        m.frame_time_ms = 66.0;
        let lines = build_appmon_lines(&m);
        // Red = [1.0, 0.35, 0.2, 1.0]
        assert_eq!(lines[0].1[0], 1.0);
        assert!(lines[0].1[1] < 0.5);
    }

    #[test]
    fn bus_overflow_colors_red() {
        let mut m = sample_metrics();
        m.bus_queue_depth = 230;
        let lines = build_appmon_lines(&m);
        assert_eq!(lines[3].1[0], 1.0); // red
    }
}
