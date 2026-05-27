//! Notifications adapter — rolling list of bus events (denials, suggestions,
//! loop status). Subscribes to a curated set of topics so the user has a
//! single place to scan recent system activity.

use std::collections::VecDeque;

use serde_json::json;

use phantom_adapter::adapter::{QuadData, Rect, RenderOutput, TextData};
use phantom_adapter::spatial::{InternalLayout, SpatialPreference};
use phantom_adapter::{
    AppCore, BusMessage, BusParticipant, Commandable, InputHandler, Lifecycled, Permissioned,
    Renderable, TopicDeclaration,
};
use phantom_protocol::Event;
use phantom_ui::tokens::Tokens;
use phantom_ui::widgets::{AppHead, AppHeadDot};
use phantom_ui::RenderCtx;

/// Visible row count cap. Adapter keeps `MAX_HISTORY` entries internally.
const VISIBLE_ROWS: usize = 12;
/// Maximum entries kept in the ring buffer.
pub const MAX_HISTORY: usize = 100;

/// Severity tier for a notification — drives the left-edge accent bar color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warn,
    Danger,
}

impl Severity {
    /// Resolve the accent bar color for this severity from the active token palette.
    fn color(self, t: &Tokens) -> [f32; 4] {
        match self {
            Self::Info => t.colors.status_info,
            Self::Warn => t.colors.status_warn,
            Self::Danger => t.colors.status_danger,
        }
    }
}

/// One notification row.
#[derive(Debug, Clone)]
pub struct Notification {
    pub source: String,
    pub message: String,
    pub severity: Severity,
}

impl Notification {
    /// Convenience constructor.
    #[must_use]
    pub fn new(source: impl Into<String>, message: impl Into<String>, severity: Severity) -> Self {
        Self {
            source: source.into(),
            message: message.into(),
            severity,
        }
    }
}

/// Notifications pane.
///
/// **Sink-only**: this adapter never emits onto the bus. `BusParticipant`
/// uses the default empty `publishes()` and `drain_outbox()`. Future work
/// that needs "click notification → focus source pane" should add an
/// outbox; today, the App handles such routing externally.
pub struct NotificationsAdapter {
    history: VecDeque<Notification>,
    tokens: Tokens,
    app_id: u32,
    subscribes: Vec<String>,
}

impl NotificationsAdapter {
    /// Build with no history.
    #[must_use]
    pub fn new() -> Self {
        Self {
            history: VecDeque::with_capacity(MAX_HISTORY),
            tokens: Tokens::phosphor(RenderCtx::fallback()),
            app_id: 0,
            // Default subscription set: denials, brain suggestions, system
            // warns. `loop.tick` is intentionally excluded — it fires at
            // the overseer heartbeat and would churn the visible 12 rows.
            // Subscribe to it explicitly via `set_subscriptions` when the
            // host actually wants tick visibility.
            subscribes: vec![
                "agent.denied".into(),
                "brain.suggestion".into(),
                "system.warn".into(),
            ],
        }
    }

    /// Update the live color palette. The host App calls this on theme switch.
    pub fn set_tokens(&mut self, tokens: Tokens) {
        self.tokens = tokens;
    }

    /// Replace the subscription topic list (e.g. to add `loop.tick` for
    /// diagnostic dashboards).
    pub fn set_subscriptions(&mut self, topics: Vec<String>) {
        self.subscribes = topics;
    }

    /// Push a new notification, dropping the oldest if at capacity.
    pub fn push(&mut self, n: Notification) {
        if self.history.len() == MAX_HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(n);
    }

    /// Returns newest-first iterator over the ring.
    pub fn iter_newest_first(&self) -> impl Iterator<Item = &Notification> {
        self.history.iter().rev()
    }

    /// Notification count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.history.len()
    }

    /// `true` when no notifications have been received.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.history.is_empty()
    }
}

impl Default for NotificationsAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl AppCore for NotificationsAdapter {
    fn app_type(&self) -> &str {
        "notifications"
    }

    fn is_alive(&self) -> bool {
        true
    }

    fn update(&mut self, _dt: f32) {}

    fn get_state(&self) -> serde_json::Value {
        json!({
            "type": "notifications",
            "count": self.history.len(),
            "subscriptions": self.subscribes,
        })
    }

    fn title(&self) -> &str {
        "Notifications"
    }
}

impl Renderable for NotificationsAdapter {
    fn render(&self, rect: &Rect) -> RenderOutput {
        let mut quads: Vec<QuadData> = Vec::new();
        let mut text_segments: Vec<TextData> = Vec::new();
        let t = self.tokens;

        let meta = if self.history.is_empty() {
            "0 new".to_string()
        } else {
            format!("{} new", self.history.len())
        };
        let dot = if self.history.is_empty() { AppHeadDot::None } else { AppHeadDot::Warn };
        let head = AppHead::new("NOTIFICATIONS", "")
            .with_icon("▲")
            .with_meta(meta)
            .with_dot(dot)
            .with_tokens(t)
            .focused(rect.focused);
        head.render_into_adapter(rect, &mut quads, &mut text_segments);

        let body = head.body_rect_adapter(rect);
        let cell_w = if rect.cell_size.0 > 0.0 { rect.cell_size.0 } else { 8.0 };
        let cell_h = if rect.cell_size.1 > 0.0 { rect.cell_size.1 } else { 16.0 };

        let mut y = body.y + cell_h * 0.5;
        let pad_x = cell_w;
        let source_color = t.colors.text_secondary;
        let message_color = t.colors.text_primary;

        if self.history.is_empty() {
            text_segments.push(TextData {
                text: "  (no notifications)".to_string(),
                x: body.x + pad_x,
                y,
                color: source_color,
            });
        }

        for n in self.iter_newest_first().take(VISIBLE_ROWS) {
            if y + cell_h * 2.0 > body.y + body.height {
                break;
            }

            // Left accent bar — severity color.
            quads.push(QuadData {
                x: body.x + pad_x,
                y,
                w: 2.0,
                h: cell_h * 1.6,
                color: n.severity.color(&t),
            });

            // Source line.
            text_segments.push(TextData {
                text: n.source.clone(),
                x: body.x + pad_x + 8.0,
                y,
                color: source_color,
            });
            y += cell_h;

            // Message line.
            text_segments.push(TextData {
                text: n.message.clone(),
                x: body.x + pad_x + 8.0,
                y,
                color: message_color,
            });
            y += cell_h * 0.9;
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
            min_size: (32, 8),
            preferred_size: (50, 20),
            max_size: Some((90, 40)),
            aspect_ratio: None,
            internal_panes: 1,
            internal_layout: InternalLayout::Single,
            priority: 2.0,
        })
    }
}

impl InputHandler for NotificationsAdapter {
    fn handle_input(&mut self, _key: &str) -> bool {
        false
    }

    fn accepts_input(&self) -> bool {
        false
    }
}

impl Commandable for NotificationsAdapter {
    fn accept_command(&mut self, cmd: &str, args: &serde_json::Value) -> anyhow::Result<String> {
        match cmd {
            "push" => {
                let source = args.get("source").and_then(|v| v.as_str()).unwrap_or("system");
                let message = args
                    .get("message")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow::anyhow!("missing field: message"))?;
                let sev = match args.get("severity").and_then(|v| v.as_str()) {
                    Some("info") => Severity::Info,
                    Some("danger") => Severity::Danger,
                    _ => Severity::Warn,
                };
                self.push(Notification::new(source, message, sev));
                Ok(json!({ "status": "ok", "count": self.history.len() }).to_string())
            }
            "clear" => {
                self.history.clear();
                Ok(json!({ "status": "ok" }).to_string())
            }
            "snapshot" => {
                let rows: Vec<serde_json::Value> = self
                    .iter_newest_first()
                    .map(|n| {
                        json!({
                            "source": n.source,
                            "message": n.message,
                            "severity": match n.severity {
                                Severity::Info => "info",
                                Severity::Warn => "warn",
                                Severity::Danger => "danger",
                            },
                        })
                    })
                    .collect();
                Ok(serde_json::Value::Array(rows).to_string())
            }
            other => Err(anyhow::anyhow!("unknown command: {other}")),
        }
    }
}

impl BusParticipant for NotificationsAdapter {
    fn subscribes_to(&self) -> Vec<String> {
        self.subscribes.clone()
    }

    fn publishes(&self) -> Vec<TopicDeclaration> {
        Vec::new()
    }

    fn on_message(&mut self, msg: &BusMessage) {
        // Best-effort: derive a notification from typed events the bus
        // currently emits. Unmatched variants are silently ignored.
        match &msg.event {
            Event::AgentError { agent_id, error } => {
                self.push(Notification::new(
                    format!("agent {agent_id}"),
                    format!("error: {error}"),
                    Severity::Danger,
                ));
            }
            Event::AgentSpawned { agent_id, task } => {
                self.push(Notification::new(
                    format!("agent {agent_id}"),
                    format!("spawned · {task}"),
                    Severity::Info,
                ));
            }
            Event::BrainDecision { action, confidence } => {
                self.push(Notification::new(
                    "brain",
                    format!("{action} ({:.0}%)", confidence * 100.0),
                    Severity::Info,
                ));
            }
            Event::Custom { kind, data } => {
                let sev = if kind.contains("denied") || kind.contains("error") {
                    Severity::Danger
                } else if kind.contains("warn") {
                    Severity::Warn
                } else {
                    Severity::Info
                };
                self.push(Notification::new(kind.clone(), data.clone(), sev));
            }
            _ => {}
        }
    }
}

impl Lifecycled for NotificationsAdapter {
    fn set_app_id(&mut self, id: u32) {
        self.app_id = id;
    }
}

impl Permissioned for NotificationsAdapter {}

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
    fn app_type_is_notifications() {
        let a = NotificationsAdapter::new();
        assert_eq!(a.app_type(), "notifications");
    }

    #[test]
    fn push_and_snapshot_returns_newest_first() {
        let mut a = NotificationsAdapter::new();
        a.push(Notification::new("a", "first", Severity::Info));
        a.push(Notification::new("b", "second", Severity::Warn));
        let resp = a.accept_command("snapshot", &json!({})).unwrap();
        let v: serde_json::Value = serde_json::from_str(&resp).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(arr[0]["message"], "second");
        assert_eq!(arr[1]["message"], "first");
    }

    #[test]
    fn empty_state_renders_hint() {
        let a = NotificationsAdapter::new();
        let out = a.render(&rect());
        assert!(out.text_segments.iter().any(|t| t.text.contains("no notifications")));
    }

    #[test]
    fn renders_accent_bar_per_row() {
        let mut a = NotificationsAdapter::new();
        a.push(Notification::new("brain", "x", Severity::Info));
        a.push(Notification::new("defender", "y", Severity::Danger));
        let out = a.render(&rect());
        // 2 header quads + 2 accent bars
        assert!(out.quads.len() >= 4);
    }

    #[test]
    fn capacity_caps_history() {
        let mut a = NotificationsAdapter::new();
        for i in 0..(MAX_HISTORY + 10) {
            a.push(Notification::new("src", format!("m{i}"), Severity::Info));
        }
        assert_eq!(a.len(), MAX_HISTORY);
    }

    #[test]
    fn clear_command_drains_history() {
        let mut a = NotificationsAdapter::new();
        a.push(Notification::new("a", "x", Severity::Info));
        a.accept_command("clear", &json!({})).unwrap();
        assert!(a.is_empty());
    }

    #[test]
    fn subscribes_to_default_topics() {
        let a = NotificationsAdapter::new();
        let topics = a.subscribes_to();
        assert!(topics.contains(&"agent.denied".to_string()));
        assert!(topics.contains(&"brain.suggestion".to_string()));
    }

    #[test]
    fn default_subscriptions_exclude_loop_tick() {
        // loop.tick is high-frequency overseer heartbeat; subscribing by
        // default would spam the visible rows. Hosts that want tick
        // visibility opt in via set_subscriptions.
        let a = NotificationsAdapter::new();
        assert!(!a.subscribes_to().contains(&"loop.tick".to_string()));
    }

    #[test]
    fn set_subscriptions_replaces_topics() {
        let mut a = NotificationsAdapter::new();
        a.set_subscriptions(vec!["loop.tick".into()]);
        assert_eq!(a.subscribes_to(), vec!["loop.tick".to_string()]);
    }

    #[test]
    fn set_app_id_stores_id() {
        let mut a = NotificationsAdapter::new();
        a.set_app_id(42);
        assert_eq!(a.app_id, 42);
    }

    #[test]
    fn theme_swap_propagates_to_accent_bar() {
        use phantom_ui::tokens::ColorRoles;
        let mut a = NotificationsAdapter::new();
        a.push(Notification::new("brain", "x", Severity::Danger));
        let out_p = a.render(&rect());

        let mut roles = ColorRoles::phosphor();
        roles.status_danger = [0.0, 0.0, 1.0, 1.0];
        a.set_tokens(Tokens::new(roles, RenderCtx::fallback()));
        let out_b = a.render(&rect());

        // The accent bar is the 2-px-wide quad — find it in each render.
        let bar_p = out_p
            .quads
            .iter()
            .find(|q| (q.w - 2.0).abs() < 0.01)
            .expect("phosphor accent bar must render");
        let bar_b = out_b
            .quads
            .iter()
            .find(|q| (q.w - 2.0).abs() < 0.01)
            .expect("blue accent bar must render");
        assert_ne!(bar_p.color, bar_b.color);
        assert!(bar_b.color[2] > 0.9);
    }
}
