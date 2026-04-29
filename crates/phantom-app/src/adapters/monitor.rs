//! Monitor adapter — wraps `SysmonHandle` as an `AppAdapter`.
//!
//! Bridges the system resource monitor into the unified app model so
//! that sysmon stats flow through the event bus. Rendering stays in
//! `render_overlay.rs`; this adapter is headless.

use serde_json::json;

use phantom_adapter::adapter::{Rect, RenderOutput};
use phantom_adapter::spatial::{InternalLayout, SpatialPreference};
use phantom_adapter::{
    AppCore, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned, Renderable,
};

use crate::sysmon::SysmonHandle;

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

/// System resource monitor wrapped in the `AppAdapter` interface.
///
/// Polls the background sysmon thread each frame and, when new stats
/// arrive, pushes a `Custom` bus event with the JSON-encoded snapshot.
pub struct MonitorAdapter {
    handle: SysmonHandle,
    app_id: u32,
    outbox: Vec<phantom_adapter::BusMessage>,
    active: bool,
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
        }
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

        // Emit a bus event when new stats arrive from the sysmon thread.
        if changed {
            if let Some(ref stats) = self.handle.latest {
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
    }

    fn get_state(&self) -> serde_json::Value {
        json!({
            "type": "monitor",
            "active": self.active,
            "has_stats": self.handle.latest.is_some(),
        })
    }
}

impl Renderable for MonitorAdapter {
    fn render(&self, _rect: &Rect) -> RenderOutput {
        RenderOutput::default()
    }

    fn is_visual(&self) -> bool {
        false
    }

    fn spatial_preference(&self) -> Option<SpatialPreference> {
        if self.is_visual() {
            Some(SpatialPreference {
                min_size: (20, 6),
                preferred_size: (40, 12),
                max_size: Some((60, 20)),
                aspect_ratio: None,
                internal_panes: 1,
                internal_layout: InternalLayout::Single,
                priority: 2.0,
            })
        } else {
            None
        }
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
    fn accept_command(&mut self, cmd: &str, _args: &serde_json::Value) -> anyhow::Result<String> {
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
    fn is_not_visual() {
        let (_tx, handle) = fake_handle();
        let adapter = MonitorAdapter::new(handle);
        assert!(!adapter.is_visual());
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
