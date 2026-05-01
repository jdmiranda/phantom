//! [`AppActionHandler`] — the GUI-side [`ActionHandler`] implementation.
//!
//! Bundles all the mutable `App` sub-systems that brain-action dispatch needs
//! into a single struct so [`AiAction::execute`] can be called with one
//! argument rather than the ten-parameter signature that
//! `execute_brain_action` previously required.

use std::time::Instant;

use log::{info, warn};

use phantom_agents::{AgentId, AgentSpawnOpts, AgentTask};
use phantom_agents::agent::PauseReason;
use phantom_agents::dispatch::Disposition;
use phantom_brain::dispatch::ActionHandler;
use phantom_brain::events::{ConnectionState, SuggestionOption};
use phantom_ui::widgets::StatusBar;

use crate::app::SuggestionOverlay;
use crate::console::Console;
use crate::coordinator::AppCoordinator;

// ---------------------------------------------------------------------------
// AppActionHandler
// ---------------------------------------------------------------------------

/// Wraps mutable references to all `App` sub-systems that brain-action
/// dispatch touches, satisfying the [`ActionHandler`] contract for the full
/// GUI path.
pub(crate) struct AppActionHandler<'a> {
    /// Wall-clock instant captured at the start of this frame.
    pub now: Instant,
    /// Current suggestion overlay slot (written by `ShowSuggestion`).
    pub suggestion: &'a mut Option<SuggestionOverlay>,
    /// Persistent per-project memory store.
    pub memory: &'a mut Option<phantom_memory::MemoryStore>,
    /// Transient notification store.
    pub notification_store: &'a mut Option<phantom_memory::notifications::NotificationStore>,
    /// Console for inline replies.
    pub console: &'a mut Console,
    /// Coordinator for command dispatch and adapter management.
    pub coordinator: &'a mut AppCoordinator,
    /// Layout engine used when removing adapters.
    pub layout: &'a mut phantom_ui::layout::LayoutEngine,
    /// Scene tree used when removing adapters.
    pub scene: &'a mut phantom_scene::tree::SceneTree,
    /// Accumulates `SpawnAgent` requests deferred to after the action loop
    /// (avoids borrow conflicts on `App`).
    pub tasks_to_spawn: &'a mut Vec<AgentSpawnOpts>,
    /// Status bar widget; updated when offline-mode toggles so the
    /// `[OFFLINE]` chip appears in the bottom-right of the chrome.
    pub status_bar: &'a mut StatusBar,
}

impl ActionHandler for AppActionHandler<'_> {
    fn show_suggestion(&mut self, text: String, options: Vec<SuggestionOption>) {
        info!("[PHANTOM]: {text}");
        *self.suggestion = Some(SuggestionOverlay {
            text,
            options,
            shown_at: self.now,
        });
    }

    fn show_notification(&mut self, msg: String) {
        info!("[PHANTOM]: {msg}");
        if let Some(store) = self.notification_store
            && let Err(e) = store.push(
                phantom_memory::notifications::NotificationKind::PlanReady,
                "Phantom",
                &msg,
                None,
            ) {
                warn!("NotificationStore::push failed: {e}");
            }
    }

    fn update_memory(&mut self, key: String, value: String) {
        if let Some(mem) = self.memory {
            let _ = mem.set(
                &key,
                &value,
                phantom_memory::MemoryCategory::Context,
                phantom_memory::MemorySource::Auto,
            );
        }
    }

    fn spawn_agent(&mut self, task: AgentTask, spawn_tag: Option<u64>, disposition: Disposition) {
        info!(
            "[PHANTOM]: Spawning agent \
             (spawn_tag={spawn_tag:?}, disposition={disposition:?}, \
             auto_approve={})...",
            disposition.auto_approve(),
        );
        let mut opts = AgentSpawnOpts::new(task).with_disposition(disposition);
        opts.spawn_tag = spawn_tag;
        self.tasks_to_spawn.push(opts);
    }

    fn console_reply(&mut self, reply: String) {
        info!("[PHANTOM]: {reply}");
        self.console.output(format!("[phantom] {reply}"));
    }

    fn run_command(&mut self, cmd: String) {
        info!("[PHANTOM]: Running command: {cmd}");
        let cmd_text = if cmd.ends_with('\n') {
            cmd
        } else {
            format!("{cmd}\n")
        };
        let _ = self
            .coordinator
            .send_command_to_focused("write", &serde_json::json!({"text": cmd_text}));
    }

    fn dismiss_adapter(&mut self, app_id: u32) {
        info!("[PHANTOM]: Dismissing adapter {app_id}");
        self.coordinator
            .remove_adapter(app_id, self.layout, self.scene);
    }

    fn agent_flatlined(&mut self, id: AgentId, reason: String) {
        info!("[PHANTOM]: Agent {id} flatlined: {reason}");
    }

    fn suggest(&mut self, action: String, rationale: String, confidence: f32) {
        info!(
            "[PHANTOM]: Proactive suggestion (confidence={confidence:.2}): {action} — {rationale}"
        );
    }

    fn quarantine_agent(&mut self, agent_id: AgentId, denial_count: usize) {
        info!("[PHANTOM]: Quarantining agent {agent_id} after {denial_count} denials");
    }

    fn agent_quarantined(&mut self, agent_id: AgentId, denial_count: usize) {
        info!("[PHANTOM]: Agent {agent_id} quarantined ({denial_count} denials)");
    }

    fn pause_agent(&mut self, agent_id: AgentId, reason: PauseReason) {
        info!("[PHANTOM]: Pausing agent {agent_id} ({reason:?})");
    }

    fn resume_agent(&mut self, agent_id: AgentId) {
        info!("[PHANTOM]: Resuming agent {agent_id}");
    }

    fn update_connection_state(&mut self, state: ConnectionState) {
        info!("[PHANTOM]: Connection state updated: {state:?}");
    }

    fn set_offline_mode(&mut self, enabled: bool) {
        info!("[PHANTOM]: Offline mode {}", if enabled { "ON" } else { "OFF" });
        // The actual router state is managed by the brain thread; this hook
        // mirrors the flag onto the status bar so the `[OFFLINE]` chip
        // renders in the bottom-right chrome (#424).
        self.status_bar.set_offline_mode(enabled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use phantom_adapter::EventBus;
    use phantom_brain::events::AiAction;
    use phantom_ui::layout::{LayoutEngine, Rect};
    use phantom_ui::widgets::Widget;

    fn status_rect() -> Rect {
        Rect { x: 0.0, y: 0.0, width: 1920.0, height: 28.0 }
    }

    fn has_offline_chip(bar: &StatusBar) -> bool {
        bar.render_text(&status_rect())
            .iter()
            .any(|s| s.text.contains("[OFFLINE]"))
    }

    /// `AppActionHandler::set_offline_mode` must mirror the flag onto the
    /// `StatusBar` so the `[OFFLINE]` chip appears/clears in the chrome (#424).
    #[test]
    fn set_offline_mode_drives_status_bar_indicator() {
        let mut suggestion: Option<SuggestionOverlay> = None;
        let mut memory: Option<phantom_memory::MemoryStore> = None;
        let mut notification_store: Option<
            phantom_memory::notifications::NotificationStore,
        > = None;
        let mut console = Console::new();
        let mut coordinator = AppCoordinator::new(EventBus::new());
        let mut layout = LayoutEngine::new().expect("layout");
        let mut scene = phantom_scene::tree::SceneTree::new();
        let mut tasks_to_spawn = Vec::new();
        let mut status_bar = StatusBar::new();

        assert!(!has_offline_chip(&status_bar), "default: no chip");

        AiAction::SetOfflineMode { enabled: true }.execute(&mut AppActionHandler {
            now: Instant::now(),
            suggestion: &mut suggestion,
            memory: &mut memory,
            notification_store: &mut notification_store,
            console: &mut console,
            coordinator: &mut coordinator,
            layout: &mut layout,
            scene: &mut scene,
            tasks_to_spawn: &mut tasks_to_spawn,
            status_bar: &mut status_bar,
        });
        assert!(has_offline_chip(&status_bar), "chip must appear when enabled");

        AiAction::SetOfflineMode { enabled: false }.execute(&mut AppActionHandler {
            now: Instant::now(),
            suggestion: &mut suggestion,
            memory: &mut memory,
            notification_store: &mut notification_store,
            console: &mut console,
            coordinator: &mut coordinator,
            layout: &mut layout,
            scene: &mut scene,
            tasks_to_spawn: &mut tasks_to_spawn,
            status_bar: &mut status_bar,
        });
        assert!(!has_offline_chip(&status_bar), "chip must clear when disabled");
    }
}
