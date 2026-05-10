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
        self.spawn_agent_pane_with_opts(AgentSpawnOpts::new(task)).is_some()
    }

    /// Spawn a new agent pane with explicit spawn options.
    ///
    /// Splits the focused pane vertically, creates the agent (using the
    /// requested [`ChatModel`] if any), wraps it in an `AgentAdapter`, and
    /// registers it in the new split pane.
    ///
    /// Returns `Some(agent_id)` on success, `None` when the spawn cannot
    /// proceed (no focused pane, layout error, or missing API key).
    /// The `agent_id` is the stable [`phantom_agents::AgentId`] (`u64`)
    /// assigned by the agent's internal id counter — callers can use this
    /// to correlate subsequent `phantom.get_agent_status` polls (issue #400).
    pub(crate) fn spawn_agent_pane_with_opts(
        &mut self,
        opts: AgentSpawnOpts,
    ) -> Option<u64> {
        // -- MCP discovery barrier (Bug 3 fix) --------------------------------
        //
        // Wait up to 500 ms for the async MCP external-server discovery task to
        // complete so that the spawned agent receives the full external tool
        // list.  If discovery is still running after 500 ms we proceed anyway —
        // the agent will have an empty external tool list but can still function
        // (it degrades gracefully, e.g. only built-in tools are available).
        //
        // This is a spin-wait on the background thread; the winit event loop
        // will be briefly blocked only on the very first agent spawn immediately
        // after boot.  Subsequent spawns see `mcp_discovery_complete = true`
        // instantly and take the fast path.
        {
            use std::sync::atomic::Ordering;
            const DISCOVERY_TIMEOUT_MS: u64 = 500;
            const POLL_INTERVAL_MS: u64 = 10;
            let deadline = std::time::Instant::now()
                + std::time::Duration::from_millis(DISCOVERY_TIMEOUT_MS);

            while !self.mcp_discovery_complete.load(Ordering::Acquire) {
                if std::time::Instant::now() >= deadline {
                    warn!(
                        "agent spawn: MCP discovery not complete after {DISCOVERY_TIMEOUT_MS} ms, \
                         proceeding with empty external tool list"
                    );
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(POLL_INTERVAL_MS));
            }
        }

        // Extract metadata before opts is moved into spawn_with_opts.
        let spawn_tag = opts.spawn_tag;
        // role/label carry Composer spawn_subagent metadata (#224).
        let spawn_role = opts.role().unwrap_or(DEFAULT_AGENT_PANE_ROLE);
        let spawn_label = opts.label().unwrap_or("agent-pane").to_string();
        let resolved_model = opts.resolve_model();
        let Some(claude_config) = resolve_api_config(&resolved_model) else {
            warn!("Cannot spawn agent: no API key configured for model {:?}", resolved_model);
            return None;
        };

        // Split the focused pane to make room for the agent.
        let Some(focused_app_id) = self.coordinator.focused() else {
            warn!("Cannot spawn agent: no focused adapter");
            return None;
        };
        let Some(current_pane_id) = self.coordinator.pane_id_for(focused_app_id) else {
            warn!("Cannot spawn agent: focused adapter has no layout pane");
            return None;
        };

        let split_result = self.layout.split_vertical(current_pane_id);
        let (existing_child, new_child) = match split_result {
            Ok(ids) => ids,
            Err(e) => {
                warn!("Agent split failed: {e}");
                return None;
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
            self.privacy_mode,
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

        // Wire the MCP tool registry so this pane can fall back to external
        // MCP servers for tool names not in the built-in surface. The same
        // Arc is shared across all panes spawned in this session.
        agent_pane.set_mcp_registry(std::sync::Arc::clone(&self.mcp_registry));

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

        // Issue #437: wire a real DagExplorerContext for Cartographer-role panes.
        // A fresh DagStore is created per-pane so each Cartographer session
        // operates on its own isolated task DAG. The graceful error-string
        // fallback is preserved: if the inspector adapter is absent the DAG
        // viewer commands simply will not fire, but the Cartographer's in-memory
        // task-management tools (list, annotate, mark_complete, …) still work.
        if agent_pane.role == phantom_agents::role::AgentRole::Cartographer {
            let dag_ctx = phantom_agents::dag_explorer::DagExplorerContext::empty();
            agent_pane.set_dag_explorer_context(dag_ctx);
        }

        // Wire the history capture sidecar so this agent's tool calls and
        // output are recorded in the session's agents.jsonl sidecar.
        if let Some(ref capture) = self.agent_capture {
            agent_pane.set_agent_capture(capture.clone(), self.session_uuid);
        }

        // Wire TTS: when a pipeline is active, hand the pane a clone of the
        // sender so completed assistant messages are forwarded for speech
        // playback. No-op when TTS is disabled (None).
        if let Some(ref tts) = self.tts
            && let Some(ref tx) = tts.tts_tx
        {
            agent_pane.set_tts_tx(tx.clone());
        }

        // Capture the stable AgentId BEFORE the pane is moved into the adapter.
        // This is the value returned to MCP callers (issue #399).
        let agent_id = agent_pane.agent_id();

        // Thread the substrate-owned QuarantineRegistry into the adapter
        // (issue #649) so it can detect quarantine-coincident failures on
        // the `AgentPane::Failed → AgentTaskComplete` emit and annotate
        // the summary for the brain reconciler.
        let adapter = crate::adapters::agent::AgentAdapter::with_spawn_tag(agent_pane, spawn_tag)
            .with_quarantine_registry(self.quarantine_registry.clone());

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

        info!("Agent adapter registered (AppId {app_id}) in split pane (agent_id={agent_id})");
        Some(agent_id)
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    /// Simulate the discovery barrier logic: verify that when the flag is
    /// initially false and then set to true from a background thread, the
    /// polling loop exits before the 500 ms deadline expires.
    ///
    /// This is a self-contained unit test of the polling logic extracted from
    /// `spawn_agent_pane_with_opts` — it does not require a GPU or full App.
    #[test]
    fn discovery_barrier_prevents_empty_tool_list_on_fast_spawn() {
        let flag = Arc::new(AtomicBool::new(false));
        let flag_clone = Arc::clone(&flag);

        // Simulate the discovery task completing after a short delay.
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            flag_clone.store(true, Ordering::Release);
        });

        // Mirror the barrier logic from spawn_agent_pane_with_opts.
        const DISCOVERY_TIMEOUT_MS: u64 = 500;
        const POLL_INTERVAL_MS: u64 = 10;
        let deadline = Instant::now() + Duration::from_millis(DISCOVERY_TIMEOUT_MS);
        let mut timed_out = false;

        while !flag.load(Ordering::Acquire) {
            if Instant::now() >= deadline {
                timed_out = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
        }

        assert!(
            !timed_out,
            "discovery barrier timed out — agent would get empty tool list on fast spawn"
        );
        assert!(
            flag.load(Ordering::Acquire),
            "flag should be true after the barrier exits without timeout"
        );
    }

    /// When discovery never completes (e.g. all servers are unreachable), the
    /// barrier must give up after 500 ms and not block forever.
    #[test]
    fn discovery_barrier_times_out_if_discovery_never_completes() {
        let flag = Arc::new(AtomicBool::new(false)); // never set to true

        const DISCOVERY_TIMEOUT_MS: u64 = 100; // shorter for test speed
        const POLL_INTERVAL_MS: u64 = 10;
        let deadline = Instant::now() + Duration::from_millis(DISCOVERY_TIMEOUT_MS);
        let mut timed_out = false;

        while !flag.load(Ordering::Acquire) {
            if Instant::now() >= deadline {
                timed_out = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
        }

        assert!(
            timed_out,
            "barrier should have timed out when discovery never completes"
        );
    }
}
