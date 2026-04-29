//! `App`-side agent pane spawn integration.
//!
//! Contains the `impl App` methods that create and register agent panes
//! inside the GUI coordinator, and the `drain_blocked_events` helper used
//! by `update.rs`.

use log::{info, warn};

use phantom_agents::AgentSpawnOpts;
use phantom_agents::agent::AgentTask;
use phantom_agents::spawn_rules::SubstrateEvent;

use crate::app::App;

use super::{AgentPane, DEFAULT_AGENT_PANE_ROLE, resolve_api_config};

impl App {
    /// Spawn a new agent pane as a first-class coordinator adapter.
    ///
    /// Backwards-compatible wrapper over [`App::spawn_agent_pane_with_opts`].
    /// Existing callers passing a bare [`AgentTask`] keep working byte-for-byte
    /// (no chat model override → default Claude path).
    pub(crate) fn spawn_agent_pane(&mut self, task: AgentTask) -> bool {
        self.spawn_agent_pane_with_opts(AgentSpawnOpts::new(task))
    }

    /// Spawn a new agent pane with explicit spawn options.
    ///
    /// Splits the focused pane vertically, creates the agent (using the
    /// requested [`ChatModel`] if any), wraps it in an `AgentAdapter`, and
    /// registers it in the new split pane.
    pub(crate) fn spawn_agent_pane_with_opts(
        &mut self,
        opts: AgentSpawnOpts,
    ) -> bool {
        // Extract metadata before opts is moved into spawn_with_opts.
        let spawn_tag = opts.spawn_tag;
        // role/label carry Composer spawn_subagent metadata (#224).
        let spawn_role = opts.role().unwrap_or(DEFAULT_AGENT_PANE_ROLE);
        let spawn_label = opts.label().unwrap_or("agent-pane").to_string();
        let resolved_model = opts.resolve_model();
        let Some(claude_config) = resolve_api_config(&resolved_model) else {
            warn!("Cannot spawn agent: no API key configured for model {:?}", resolved_model);
            return false;
        };

        // Split the focused pane to make room for the agent.
        let Some(focused_app_id) = self.coordinator.focused() else {
            warn!("Cannot spawn agent: no focused adapter");
            return false;
        };
        let Some(current_pane_id) = self.coordinator.pane_id_for(focused_app_id) else {
            warn!("Cannot spawn agent: focused adapter has no layout pane");
            return false;
        };

        let split_result = self.layout.split_vertical(current_pane_id);
        let (existing_child, new_child) = match split_result {
            Ok(ids) => ids,
            Err(e) => {
                warn!("Agent split failed: {e}");
                return false;
            }
        };

        // Equal split: terminal 50%, agent 50%.
        let _ = self.layout.set_flex_grow(existing_child, 1.0);
        let _ = self.layout.set_flex_grow(new_child, 1.0);

        // Resize layout after split.
        let width = self.gpu.surface_config.width;
        let height = self.gpu.surface_config.height;
        let _ = self.layout.resize(width as f32, height as f32);

        // Remap the existing terminal's PaneId.
        self.coordinator
            .remap_pane(focused_app_id, current_pane_id, existing_child);

        // Resize the existing terminal to fit its new (smaller) pane.
        if let Ok(rect) = self.layout.get_pane_rect(existing_child) {
            let (cols, rows) = crate::pane::pane_cols_rows(self.cell_size, rect);
            let _ = self.coordinator.send_command(
                focused_app_id,
                "resize",
                &serde_json::json!({"cols": cols, "rows": rows}),
            );
        }

        // Create the agent and register in the new split pane.
        //
        // Hand the App's canonical `BlockedEventSink` to the pane so that
        // when the agent's consecutive tool-call failure streak crosses
        // [`TOOL_BLOCK_THRESHOLD`], an `EventKind::AgentBlocked` substrate
        // event lands in `App.blocked_event_sink`. The drain step in
        // `update.rs::update` picks those up each frame and forwards them
        // into the substrate runtime
        // where the Fixer spawn rule consumes them and queues a Fixer
        // `SpawnAction` — closing the producer→consumer loop.
        let mut agent_pane = AgentPane::spawn_with_opts(
            opts,
            &claude_config,
            Some(self.blocked_event_sink.clone()),
            None,
        );

        // Wire the substrate handles so chat-tool / composer-tool dispatch
        // routes through the live runtime. The pane gets clones of the
        // runtime's `Arc<Mutex<…>>` registry + event log and a clone of the
        // App's `pending_spawn_subagent` queue. The `AgentRef` is stamped
        // with a fresh id (currently 0 — the agent module's `AgentId` is a
        // `u64` per-session counter, unified with QuarantineRegistry, fixes #273)
        // and the default
        // [`DEFAULT_AGENT_PANE_ROLE`]; when the next phase wires Composer
        // panes through this path the role override will live on
        // [`AgentSpawnOpts`].
        // Use spawn_role/spawn_label from opts (#224).
        let self_ref = phantom_agents::role::AgentRef::new(
            0,
            spawn_role,
            spawn_label,
            phantom_agents::role::SpawnSource::User,
        );
        agent_pane.set_substrate_handles(
            self.runtime.registry_handle(),
            self.runtime.event_log_handle(),
            self.pending_spawn_subagent.clone(),
            self_ref,
            spawn_role,
            self.quarantine_registry.clone(),
        );

        // Wire the snapshot sink so the pane pushes an AgentSnapshot into the
        // App-owned queue when it reaches Done or Failed.  The App drains this
        // at shutdown and persists the snapshots via AgentStatePersister.
        agent_pane.set_snapshot_sink(self.agent_snapshot_queue.clone());

        // Issue #235: inject the ticket dispatcher for Dispatcher-role panes.
        // `agent_pane.role` was just set by `set_substrate_handles` above.
        // For the current default (Conversational) this is a no-op. When a
        // future spawn path sets role = Dispatcher this branch fires and the
        // pane gains live access to the GH ticket queue.
        if agent_pane.role == phantom_agents::role::AgentRole::Dispatcher {
            if let Some(ref td) = self.ticket_dispatcher {
                agent_pane.set_ticket_dispatcher(std::sync::Arc::clone(td));
            } else {
                warn!(
                    "Spawning a Dispatcher-role agent pane but ticket_dispatcher is None \
                     (GITHUB_TOKEN / GH_REPO not set); ticket tools will fail gracefully"
                );
            }
        }

        // Wire the history capture sidecar so this agent's tool calls and
        // output are recorded in the session's agents.jsonl sidecar.
        if let Some(ref capture) = self.agent_capture {
            agent_pane.set_agent_capture(capture.clone(), self.session_uuid);
        }

        let adapter = crate::adapters::agent::AgentAdapter::with_spawn_tag(agent_pane, spawn_tag);

        let scene_node = self
            .scene
            .add_node(self.scene_content_node, phantom_scene::node::NodeKind::Pane);

        let app_id = self.coordinator.register_adapter_at_pane(
            Box::new(adapter),
            new_child,
            scene_node,
            phantom_scene::clock::Cadence::unlimited(),
            &mut self.layout,
        );

        // Focus the new agent pane.
        self.coordinator.set_focus(app_id);

        // Notify the substrate runtime that an agent pane was opened. The
        // seed `pane.opened.agent` rule will fire on the next tick, queueing
        // a `SpawnIfNotRunning(Watcher, "agent-pane-watch")` action and
        // appending the event to `events.jsonl`. Phase 2 will turn that
        // queued action into an actual supervised Watcher task.
        self.runtime
            .push_event(phantom_agents::spawn_rules::SubstrateEvent {
                kind: phantom_agents::spawn_rules::EventKind::PaneOpened {
                    app_type: "agent".to_string(),
                },
                payload: serde_json::json!({
                    "app_id": app_id,
                    "pane_id": format!("{:?}", new_child),
                }),
                source: phantom_agents::spawn_rules::EventSource::User,
            });

        info!("Agent adapter registered (AppId {app_id}) in split pane");
        true
    }

    /// Drain the App's `BlockedEventSink` and return the queued
    /// `EventKind::AgentBlocked` substrate events.
    ///
    /// Producers (`AgentPane::execute_pending_tools` via
    /// [`AgentPane::maybe_emit_blocked_event`]) push events synchronously when
    /// an agent's consecutive tool-call failure streak crosses
    /// [`TOOL_BLOCK_THRESHOLD`]. The consumer side (`update.rs::update`) calls
    /// this each frame and forwards every drained event into
    /// [`crate::runtime::AgentRuntime::push_event`], where the registered
    /// `fixer_spawn_rule` matches and queues a Fixer `SpawnAction`.
    ///
    /// Returns an empty `Vec` if the sink is empty or the mutex is poisoned —
    /// observability is best-effort, never fatal.
    pub(crate) fn drain_blocked_events(
        &mut self,
    ) -> Vec<SubstrateEvent> {
        match self.blocked_event_sink.lock() {
            Ok(mut q) => std::mem::take(&mut *q),
            Err(_) => {
                warn!("blocked_event_sink mutex poisoned; dropping queued events");
                Vec::new()
            }
        }
    }
}
