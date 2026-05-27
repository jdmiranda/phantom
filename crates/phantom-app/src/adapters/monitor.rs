//! Monitor adapter — wraps `SysmonHandle` as an `AppAdapter`.
//!
//! Bridges the system resource monitor into the unified app model so
//! that sysmon stats flow through the event bus and are renderable as
//! a first-class pane. When opened via `Cmd+Shift+M`, the App calls
//! `refresh_monitor_pane` once per second to push fresh metrics via the
//! `set_metrics` command.

use serde_json::json;

use phantom_adapter::adapter::{QuadData, Rect, RenderOutput, TextData};
use phantom_adapter::spatial::{InternalLayout, SpatialPreference};
use phantom_adapter::{
    AppCore, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned, Renderable,
};
use phantom_ui::tokens::Tokens;
use phantom_ui::widgets::AppHead;
use phantom_ui::RenderCtx;

use crate::sysmon::SysmonHandle;

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

/// Cached metrics rendered on the monitor pane. Refreshed by the App
/// (`refresh_monitor_pane`) via the `set_metrics` command or by the
/// background sysmon thread.
#[derive(Debug, Clone, Default)]
struct MonitorMetrics {
    cpu_usage: f32,
    load_avg_1m: f32,
    mem_usage: f32,
    mem_used_mb: u64,
    mem_total_mb: u64,
    disk_usage: f32,
    disk_used_gb: f32,
    disk_total_gb: f32,
    disk_read_kbs: f32,
    disk_write_kbs: f32,
    net_rx_kbs: f32,
    net_tx_kbs: f32,
    battery_pct: Option<f32>,
    battery_charging: bool,
    cpu_temp_c: Option<f32>,
    gpu_temp_c: Option<f32>,
    gpu_usage: Option<f32>,
    net_connections: u32,
    has_data: bool,
}

/// System resource monitor wrapped in the `AppAdapter` interface.
///
/// Renders a live system-stats pane when spawned via `toggle_monitor_pane`.
/// Polls the background sysmon thread each frame; the App also calls
/// `set_metrics` once a second to keep the visible view fresh.
pub struct MonitorAdapter {
    handle: SysmonHandle,
    app_id: u32,
    outbox: Vec<phantom_adapter::BusMessage>,
    active: bool,
    /// Latest metrics for rendering. Populated by either the internal
    /// sysmon poll or the `set_metrics` command from the App.
    metrics: MonitorMetrics,
    tokens: Tokens,
}

impl MonitorAdapter {
    /// Wrap an already-spawned sysmon handle in the adapter.
    #[allow(dead_code)]
    pub(crate) fn new(handle: SysmonHandle) -> Self {
        Self {
            handle,
            app_id: 0,
            outbox: Vec::new(),
            active: false,
            metrics: MonitorMetrics::default(),
            tokens: Tokens::phosphor(RenderCtx::fallback()),
        }
    }

    /// Update the live color palette. The host App calls this on theme switch.
    #[allow(dead_code)]
    pub fn set_tokens(&mut self, tokens: Tokens) {
        self.tokens = tokens;
    }
}

// ---------------------------------------------------------------------------
// Sub-trait implementations (ISP)
// ---------------------------------------------------------------------------

impl AppCore for MonitorAdapter {
    fn app_type(&self) -> &str {
        "monitor"
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn update(&mut self, _dt: f32) {
        if !self.active {
            return;
        }

        let changed = self.handle.poll_changed();

        // Emit a bus event when new stats arrive from the sysmon thread,
        // and refresh the cached metrics used by `render`.
        if changed
            && let Some(ref stats) = self.handle.latest {
                self.metrics = MonitorMetrics {
                    cpu_usage: stats.cpu_usage,
                    load_avg_1m: stats.load_avg_1m,
                    mem_usage: stats.mem_usage,
                    mem_used_mb: stats.mem_used_mb,
                    mem_total_mb: stats.mem_total_mb,
                    disk_usage: stats.disk_usage,
                    disk_used_gb: stats.disk_used_gb,
                    disk_total_gb: stats.disk_total_gb,
                    disk_read_kbs: stats.disk_read_kbs,
                    disk_write_kbs: stats.disk_write_kbs,
                    net_rx_kbs: stats.net_rx_kbs,
                    net_tx_kbs: stats.net_tx_kbs,
                    battery_pct: stats.battery_pct,
                    battery_charging: stats.battery_charging,
                    cpu_temp_c: stats.cpu_temp_c,
                    gpu_temp_c: stats.gpu_temp_c,
                    gpu_usage: stats.gpu_usage,
                    net_connections: stats.net_connections,
                    has_data: true,
                };

                let data = json!({
                    "cpu_usage": stats.cpu_usage,
                    "load_avg_1m": stats.load_avg_1m,
                    "mem_usage": stats.mem_usage,
                    "mem_used_mb": stats.mem_used_mb,
                    "mem_total_mb": stats.mem_total_mb,
                    "disk_usage": stats.disk_usage,
                    "disk_used_gb": stats.disk_used_gb,
                    "disk_total_gb": stats.disk_total_gb,
                    "disk_read_kbs": stats.disk_read_kbs,
                    "disk_write_kbs": stats.disk_write_kbs,
                    "net_rx_kbs": stats.net_rx_kbs,
                    "net_tx_kbs": stats.net_tx_kbs,
                    "battery_pct": stats.battery_pct,
                    "battery_charging": stats.battery_charging,
                    "battery_time_remaining": stats.battery_time_remaining,
                    "cpu_temp_c": stats.cpu_temp_c,
                    "gpu_temp_c": stats.gpu_temp_c,
                    "gpu_usage": stats.gpu_usage,
                    "net_connections": stats.net_connections,
                });

                self.outbox.push(phantom_adapter::BusMessage {
                    topic_id: 0,
                    sender: self.app_id,
                    event: phantom_protocol::Event::Custom {
                        kind: "sysmon.stats".into(),
                        data: data.to_string(),
                    },
                    frame: 0,
                    timestamp: 0,
                });
            }
    }

    fn get_state(&self) -> serde_json::Value {
        json!({
            "type": "monitor",
            "active": self.active,
            "has_stats": self.metrics.has_data,
        })
    }

    fn title(&self) -> &str {
        "monitor"
    }
}

impl Renderable for MonitorAdapter {
    fn render(&self, rect: &Rect) -> RenderOutput {
        let t = self.tokens;
        let mut quads: Vec<QuadData> = Vec::new();
        let mut text_segments: Vec<TextData> = Vec::new();

        let head = AppHead::new("MONITOR", "system metrics")
            .with_icon("◇")
            .with_meta(if self.metrics.has_data {
                format!("cpu {:>3.0}%  mem {:>3.0}%", self.metrics.cpu_usage * 100.0, self.metrics.mem_usage * 100.0)
            } else {
                "waiting for samples".to_string()
            })
            .with_tokens(t);
        head.render_into_adapter(rect, &mut quads, &mut text_segments);

        let body = head.body_rect_adapter(rect);
        let cell_w = if rect.cell_size.0 > 0.0 { rect.cell_size.0 } else { 8.0 };
        let cell_h = if rect.cell_size.1 > 0.0 { rect.cell_size.1 } else { 16.0 };

        let m = &self.metrics;
        let lines: Vec<String> = if !m.has_data {
            vec!["awaiting first sample…".to_string()]
        } else {
            let battery_str = match m.battery_pct {
                Some(pct) => format!("{:>3.0}%{}", pct, if m.battery_charging { " ⚡" } else { "" }),
                None => "—".to_string(),
            };
            let cpu_temp_str = m.cpu_temp_c.map(|t| format!("{:>4.1}°C", t)).unwrap_or_else(|| "—".into());
            let gpu_temp_str = m.gpu_temp_c.map(|t| format!("{:>4.1}°C", t)).unwrap_or_else(|| "—".into());
            let gpu_str = m.gpu_usage.map(|u| format!("{:>3.0}%", u * 100.0)).unwrap_or_else(|| "—".into());
            vec![
                format!("CPU       {:>5.1}%   (load 1m {:>4.2})", m.cpu_usage * 100.0, m.load_avg_1m),
                format!(
                    "MEM       {:>5.1}%   ({} / {} MiB)",
                    m.mem_usage * 100.0, m.mem_used_mb, m.mem_total_mb
                ),
                format!(
                    "DISK      {:>5.1}%   ({:.1} / {:.1} GiB)",
                    m.disk_usage * 100.0, m.disk_used_gb, m.disk_total_gb
                ),
                format!("DISK I/O  R {:>6.0} kB/s   W {:>6.0} kB/s", m.disk_read_kbs, m.disk_write_kbs),
                format!("NET       ↓ {:>6.0} kB/s   ↑ {:>6.0} kB/s", m.net_rx_kbs, m.net_tx_kbs),
                format!("NET conns {:>5}", m.net_connections),
                format!("BATTERY   {}", battery_str),
                format!("CPU temp  {}   GPU {}   GPU temp {}", cpu_temp_str, gpu_str, gpu_temp_str),
            ]
        };

        let mut y = body.y + cell_h * 0.5;
        for line in &lines {
            if y + cell_h > body.y + body.height {
                break;
            }
            text_segments.push(TextData {
                text: line.clone(),
                x: body.x + cell_w,
                y,
                color: t.colors.text_primary,
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
            min_size: (30, 8),
            preferred_size: (50, 14),
            max_size: Some((80, 24)),
            aspect_ratio: None,
            internal_panes: 1,
            internal_layout: InternalLayout::Single,
            priority: 2.0,
        })
    }
}

impl InputHandler for MonitorAdapter {
    fn handle_input(&mut self, _key: &str) -> bool {
        false
    }

    fn accepts_input(&self) -> bool {
        false
    }
}

impl Commandable for MonitorAdapter {
    fn accept_command(&mut self, cmd: &str, args: &serde_json::Value) -> anyhow::Result<String> {
        match cmd {
            "activate" => {
                self.active = true;
                self.handle.set_active(true);
                Ok("activated".into())
            }
            "deactivate" => {
                self.active = false;
                self.handle.set_active(false);
                Ok("deactivated".into())
            }
            "set_metrics" => {
                // Refresh the rendered metrics from a JSON payload pushed by
                // the App. Missing keys keep the previous value, so partial
                // payloads are safe.
                let get_f32 = |k: &str| args.get(k).and_then(serde_json::Value::as_f64).map(|v| v as f32);
                let get_u64 = |k: &str| args.get(k).and_then(serde_json::Value::as_u64);
                let get_opt_f32 = |k: &str| args.get(k).and_then(serde_json::Value::as_f64).map(|v| v as f32);

                if let Some(v) = get_f32("cpu_usage") { self.metrics.cpu_usage = v; }
                if let Some(v) = get_f32("load_avg_1m") { self.metrics.load_avg_1m = v; }
                if let Some(v) = get_f32("mem_usage") { self.metrics.mem_usage = v; }
                if let Some(v) = get_u64("mem_used_mb") { self.metrics.mem_used_mb = v; }
                if let Some(v) = get_u64("mem_total_mb") { self.metrics.mem_total_mb = v; }
                if let Some(v) = get_f32("disk_usage") { self.metrics.disk_usage = v; }
                if let Some(v) = get_f32("disk_used_gb") { self.metrics.disk_used_gb = v; }
                if let Some(v) = get_f32("disk_total_gb") { self.metrics.disk_total_gb = v; }
                if let Some(v) = get_f32("disk_read_kbs") { self.metrics.disk_read_kbs = v; }
                if let Some(v) = get_f32("disk_write_kbs") { self.metrics.disk_write_kbs = v; }
                if let Some(v) = get_f32("net_rx_kbs") { self.metrics.net_rx_kbs = v; }
                if let Some(v) = get_f32("net_tx_kbs") { self.metrics.net_tx_kbs = v; }
                if args.get("battery_pct").is_some() {
                    self.metrics.battery_pct = get_opt_f32("battery_pct");
                }
                if let Some(v) = args.get("battery_charging").and_then(serde_json::Value::as_bool) {
                    self.metrics.battery_charging = v;
                }
                if args.get("cpu_temp_c").is_some() {
                    self.metrics.cpu_temp_c = get_opt_f32("cpu_temp_c");
                }
                if args.get("gpu_temp_c").is_some() {
                    self.metrics.gpu_temp_c = get_opt_f32("gpu_temp_c");
                }
                if args.get("gpu_usage").is_some() {
                    self.metrics.gpu_usage = get_opt_f32("gpu_usage");
                }
                if let Some(v) = args.get("net_connections").and_then(serde_json::Value::as_u64) {
                    self.metrics.net_connections = v as u32;
                }
                self.metrics.has_data = true;
                Ok("metrics updated".into())
            }
            "stats" => {
                let data = match self.handle.latest {
                    Some(ref stats) => json!({
                        "cpu_usage": stats.cpu_usage,
                        "load_avg_1m": stats.load_avg_1m,
                        "mem_usage": stats.mem_usage,
                        "mem_used_mb": stats.mem_used_mb,
                        "mem_total_mb": stats.mem_total_mb,
                        "disk_usage": stats.disk_usage,
                        "disk_used_gb": stats.disk_used_gb,
                        "disk_total_gb": stats.disk_total_gb,
                        "disk_read_kbs": stats.disk_read_kbs,
                        "disk_write_kbs": stats.disk_write_kbs,
                        "net_rx_kbs": stats.net_rx_kbs,
                        "net_tx_kbs": stats.net_tx_kbs,
                        "battery_pct": stats.battery_pct,
                        "battery_charging": stats.battery_charging,
                        "battery_time_remaining": stats.battery_time_remaining,
                        "cpu_temp_c": stats.cpu_temp_c,
                        "gpu_temp_c": stats.gpu_temp_c,
                        "gpu_usage": stats.gpu_usage,
                        "net_connections": stats.net_connections,
                    }),
                    None => json!(null),
                };
                Ok(data.to_string())
            }
            other => Err(anyhow::anyhow!("unknown command: {other}")),
        }
    }
}

impl BusParticipant for MonitorAdapter {
    fn drain_outbox(&mut self) -> Vec<phantom_adapter::BusMessage> {
        std::mem::take(&mut self.outbox)
    }
}

impl Lifecycled for MonitorAdapter {
    fn set_app_id(&mut self, id: u32) {
        self.app_id = id;
    }
}

impl Permissioned for MonitorAdapter {}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    use crate::sysmon::{SysmonHandle, SystemStats};

    fn fake_handle() -> (mpsc::Sender<SystemStats>, SysmonHandle) {
        let (tx, rx) = mpsc::channel();
        let handle = SysmonHandle::for_test(rx);
        (tx, handle)
    }

    fn fake_stats() -> SystemStats {
        SystemStats {
            cpu_usage: 0.25,
            load_avg_1m: 2.5,
            mem_usage: 0.60,
            mem_used_mb: 12288,
            mem_total_mb: 20480,
            disk_usage: 0.45,
            disk_used_gb: 225.0,
            disk_total_gb: 500.0,
            disk_read_kbs: 5120.0,
            disk_write_kbs: 2048.0,
            net_rx_kbs: 1500.0,
            net_tx_kbs: 300.0,
            battery_pct: Some(72.0),
            battery_charging: true,
            battery_time_remaining: Some("1:30".into()),
            cpu_temp_c: Some(55.0),
            gpu_temp_c: Some(48.0),
            gpu_usage: Some(0.15),
            net_connections: 42,
        }
    }

    #[test]
    fn app_type_returns_monitor() {
        let (_tx, handle) = fake_handle();
        let adapter = MonitorAdapter::new(handle);
        assert_eq!(adapter.app_type(), "monitor");
    }

    #[test]
    fn is_always_alive() {
        let (_tx, handle) = fake_handle();
        let adapter = MonitorAdapter::new(handle);
        assert!(adapter.is_alive());
    }

    #[test]
    fn is_visual_so_it_can_take_a_pane_slot() {
        let (_tx, handle) = fake_handle();
        let adapter = MonitorAdapter::new(handle);
        assert!(adapter.is_visual());
    }

    #[test]
    fn does_not_accept_input() {
        let (_tx, handle) = fake_handle();
        let adapter = MonitorAdapter::new(handle);
        assert!(!adapter.accepts_input());
    }

    #[test]
    fn activate_deactivate_commands() {
        let (_tx, handle) = fake_handle();
        let mut adapter = MonitorAdapter::new(handle);
        let result = adapter.accept_command("activate", &json!({})).unwrap();
        assert_eq!(result, "activated");
        assert!(adapter.active);

        let result = adapter.accept_command("deactivate", &json!({})).unwrap();
        assert_eq!(result, "deactivated");
        assert!(!adapter.active);
    }

    #[test]
    fn stats_command_returns_null_when_no_stats() {
        let (_tx, handle) = fake_handle();
        let mut adapter = MonitorAdapter::new(handle);
        let result = adapter.accept_command("stats", &json!({})).unwrap();
        assert_eq!(result, "null");
    }

    #[test]
    fn stats_command_returns_json_when_stats_present() {
        let (tx, handle) = fake_handle();
        let mut adapter = MonitorAdapter::new(handle);
        tx.send(fake_stats()).unwrap();
        adapter.handle.poll();

        let result = adapter.accept_command("stats", &json!({})).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["cpu_usage"], 0.25);
        assert_eq!(parsed["net_connections"], 42);
    }

    #[test]
    fn unknown_command_returns_error() {
        let (_tx, handle) = fake_handle();
        let mut adapter = MonitorAdapter::new(handle);
        let result = adapter.accept_command("bogus", &json!({}));
        assert!(result.is_err());
    }

    #[test]
    fn set_metrics_command_populates_render_state() {
        let (_tx, handle) = fake_handle();
        let mut adapter = MonitorAdapter::new(handle);
        assert!(!adapter.metrics.has_data);
        let payload = json!({
            "cpu_usage": 0.42,
            "mem_usage": 0.65,
            "mem_used_mb": 8192,
            "mem_total_mb": 16384,
            "net_connections": 17,
        });
        let result = adapter.accept_command("set_metrics", &payload).unwrap();
        assert_eq!(result, "metrics updated");
        assert!(adapter.metrics.has_data);
        assert!((adapter.metrics.cpu_usage - 0.42).abs() < 1e-6);
        assert!((adapter.metrics.mem_usage - 0.65).abs() < 1e-6);
        assert_eq!(adapter.metrics.mem_used_mb, 8192);
        assert_eq!(adapter.metrics.net_connections, 17);
    }

    #[test]
    fn render_emits_text_when_metrics_present() {
        let (_tx, handle) = fake_handle();
        let mut adapter = MonitorAdapter::new(handle);
        let payload = json!({"cpu_usage": 0.5, "mem_usage": 0.3});
        adapter.accept_command("set_metrics", &payload).unwrap();
        let rect = Rect {
            x: 0.0,
            y: 0.0,
            width: 600.0,
            height: 400.0,
            cell_size: (8.0, 16.0),
            focused: false,
            elapsed_secs: 0.0,
        };
        let out = adapter.render(&rect);
        // Should produce at least the head + some body lines.
        assert!(!out.text_segments.is_empty());
        let found_cpu = out
            .text_segments
            .iter()
            .any(|t| t.text.starts_with("CPU "));
        assert!(found_cpu, "rendered text must include a CPU row");
    }

    #[test]
    fn update_emits_bus_event_when_active_and_stats_arrive() {
        let (tx, handle) = fake_handle();
        let mut adapter = MonitorAdapter::new(handle);
        adapter.active = true;
        tx.send(fake_stats()).unwrap();

        adapter.update(0.016);

        let msgs = adapter.drain_outbox();
        assert_eq!(msgs.len(), 1);
        assert!(matches!(
            &msgs[0].event,
            phantom_protocol::Event::Custom { kind, .. } if kind == "sysmon.stats"
        ));
    }

    #[test]
    fn update_does_not_emit_when_inactive() {
        let (tx, handle) = fake_handle();
        let mut adapter = MonitorAdapter::new(handle);
        tx.send(fake_stats()).unwrap();

        adapter.update(0.016);

        let msgs = adapter.drain_outbox();
        assert!(msgs.is_empty());
    }

    #[test]
    fn set_app_id_stores_id() {
        let (_tx, handle) = fake_handle();
        let mut adapter = MonitorAdapter::new(handle);
        adapter.set_app_id(42);
        assert_eq!(adapter.app_id, 42);
    }

    #[test]
    fn permissions_are_empty() {
        let (_tx, handle) = fake_handle();
        let adapter = MonitorAdapter::new(handle);
        assert!(adapter.permissions().is_empty());
    }

    #[test]
    fn get_state_reflects_active_flag() {
        let (_tx, handle) = fake_handle();
        let mut adapter = MonitorAdapter::new(handle);
        let state = adapter.get_state();
        assert_eq!(state["active"], false);

        adapter.active = true;
        let state = adapter.get_state();
        assert_eq!(state["active"], true);
    }

    #[test]
    fn send_assert() {
        fn _check<T: Send>() {}
        _check::<MonitorAdapter>();
    }
}
