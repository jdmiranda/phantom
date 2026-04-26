//! Per-frame update loop: coordinator adapter ticking, dead adapter reaping,
//! brain event polling, MCP command dispatch, and status bar updates.

use std::path::Path;
use std::sync::mpsc;
use std::time::Instant;

use anyhow::Result;
use log::{debug, info, warn};

use phantom_adapter::BusMessage;
use phantom_brain::events::{AiAction, AiEvent};
use phantom_protocol::Event;
use phantom_context::ProjectContext;
use phantom_mcp::{AppCommand, ScreenshotReply};
use crate::app::{App, AppState, SuggestionOverlay};
use crate::input::chrono_time_string;

impl App {
    /// Per-frame update: read PTY data, advance boot sequence, update widgets.
    ///
    /// Call this once per frame before [`render`](Self::render).
    pub fn update(&mut self) {
        crate::profile_scope!("update");
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f32();
        let dt_duration = now.duration_since(self.last_frame);
        self.last_frame = now;

        // Warn if a frame takes abnormally long (> 2 seconds).
        if dt > 2.0 {
            warn!("SLOW FRAME: dt={dt:.2}s — previous frame blocked the event loop");
        }

        // Coordinator: tick all registered adapters and deliver bus messages.
        self.coordinator.update_all(dt_duration);

        // Bridge: drain bus events for the brain observer and forward as AiEvents.
        Self::drain_bus_to_brain(self.coordinator.bus_mut(), &self.brain);

        // Reap dead adapters (PTY exited).
        let dead_adapters: Vec<_> = self.coordinator.all_app_ids()
            .into_iter()
            .filter(|id| {
                self.coordinator.registry()
                    .get_adapter(*id)
                    .map_or(false, |a| !a.is_alive())
            })
            .collect();
        for dead_id in dead_adapters {
            info!("Adapter {dead_id} exited, removing");
            self.coordinator.remove_adapter(dead_id, &mut self.layout, &mut self.scene);
        }

        if self.coordinator.adapter_count() == 0 {
            info!("All adapters exited, quitting");
            self.quit_requested = true;
        }

        // Boot state machine.
        if self.state == AppState::Boot {
            self.boot.update(dt);
            if self.boot.is_done() {
                info!("Boot sequence complete, transitioning to terminal");
                self.state = AppState::Terminal;
            }
        }

        // Supervisor command polling (drain all pending; heartbeats are on a dedicated thread).
        while let Some(cmd) = self.supervisor.as_mut().and_then(|sv| sv.try_recv()) {
            self.handle_supervisor_command(cmd);
        }

        // AI Brain: send idle events + drain actions.
        // Collect agent spawn tasks separately to avoid borrow conflict.
        let mut tasks_to_spawn = Vec::new();
        if let Some(ref brain) = self.brain {
            let idle_secs = now.duration_since(self.last_input_time).as_secs_f32();
            if idle_secs > 5.0 && (idle_secs % 5.0) < dt {
                let _ = brain.send_event(AiEvent::UserIdle { seconds: idle_secs });
            }

            while let Some(action) = brain.try_recv_action() {
                match action {
                    AiAction::ShowSuggestion { text, options } => {
                        info!("[PHANTOM]: {text}");
                        self.suggestion = Some(SuggestionOverlay {
                            text,
                            options,
                            shown_at: now,
                        });
                    }
                    AiAction::ShowNotification(msg) => {
                        info!("[PHANTOM]: {msg}");
                    }
                    AiAction::UpdateMemory { key, value } => {
                        if let Some(ref mut mem) = self.memory {
                            let _ = mem.set(
                                &key,
                                &value,
                                phantom_memory::MemoryCategory::Context,
                                phantom_memory::MemorySource::Auto,
                            );
                        }
                    }
                    AiAction::SpawnAgent(task) => {
                        info!("[PHANTOM]: Spawning agent...");
                        tasks_to_spawn.push(task);
                    }
                    AiAction::ConsoleReply(reply) => {
                        info!("[PHANTOM]: {reply}");
                        self.console.output(format!("[phantom] {reply}"));
                    }
                    AiAction::RunCommand(cmd) => {
                        info!("[PHANTOM]: Running command: {cmd}");
                        let cmd_text = if cmd.ends_with('\n') { cmd } else { format!("{cmd}\n") };
                        let _ = self.coordinator.send_command_to_focused(
                            "write",
                            &serde_json::json!({"text": cmd_text}),
                        );
                    }
                    AiAction::DoNothing => {}
                }
            }
        }

        // Spawn agent panes (deferred from brain action loop to avoid borrow conflict).
        for task in tasks_to_spawn {
            let _ = self.spawn_agent_pane(task);
        }

        // Expire stale suggestions.
        if let Some(ref s) = self.suggestion {
            if now.duration_since(s.shown_at).as_secs() > 10 {
                self.suggestion = None;
            }
        }

        // Refresh git context periodically (off main thread, max once per 30s, one at a time).
        if let Some(ref ctx) = self.context {
            if now.duration_since(self.git_refresh_last).as_secs() >= 30 {
                // Reap completed handle only when the timer fires (not every frame).
                if self.git_refresh_handle.as_ref().is_some_and(|h| h.is_finished()) {
                    self.git_refresh_handle = None;
                }
            }
            if now.duration_since(self.git_refresh_last).as_secs() >= 30
                && self.git_refresh_handle.is_none()
            {
                self.git_refresh_last = now;
                let project_dir = ctx.root.clone();
                let brain_tx = self.brain.as_ref().map(|b| b.event_sender());
                self.git_refresh_handle = std::thread::Builder::new()
                    .name("git-refresh".into())
                    .spawn(move || {
                        let mut fresh = ProjectContext::detect(std::path::Path::new(&project_dir));
                        fresh.refresh_git();
                        if let Some(tx) = brain_tx {
                            let _ = tx.send(AiEvent::GitStateChanged);
                        }
                    })
                    .ok();
            }
            if let Some(ref git) = ctx.git {
                self.status_bar.set_branch(&git.branch);
            }
        }

        // Drain MCP commands.
        loop {
            match self.mcp_cmd_rx.try_recv() {
                Ok(cmd) => self.handle_mcp_command(cmd),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    warn!("MCP command channel disconnected");
                    break;
                }
            }
        }

        // Poll agent panes for streaming output and emit bus events on completion.
        self.poll_agent_panes();
        for pane in &mut self.agent_panes {
            if !pane.event_emitted
                && matches!(pane.status, crate::agent_pane::AgentPaneStatus::Done | crate::agent_pane::AgentPaneStatus::Failed)
            {
                pane.event_emitted = true;
                self.coordinator.bus_mut().emit(BusMessage {
                    topic_id: self.topic_agent_event,
                    sender: 0,
                    event: Event::AgentTaskComplete {
                        agent_id: 0,
                        success: pane.status == crate::agent_pane::AgentPaneStatus::Done,
                        summary: pane.task.clone(),
                    },
                    frame: 0,
                    timestamp: now.duration_since(self.start_time).as_secs(),
                });
            }
        }

        // Drain completed jobs from the worker pool.
        if let Some(ref pool) = self.job_pool {
            for (job_id, result) in pool.drain_completed() {
                match result {
                    crate::jobs::JobResult::Done(msg) => {
                        debug!("Job {:?} completed: {msg}", job_id);
                    }
                    crate::jobs::JobResult::Err(err) => {
                        warn!("Job {:?} failed: {err}", job_id);
                    }
                    crate::jobs::JobResult::Cancelled => {
                        debug!("Job {:?} cancelled", job_id);
                    }
                }
            }
        }

        self.scene.update_world_transforms();

        // Poll system monitor.
        self.sysmon.poll();

        // Advance keystroke glitch animations.
        self.keystroke_fx.tick();

        // Advance console slide animation.
        self.console.animate(dt);

        // Video playback: upload next frame to GPU.
        if let Some(ref mut playback) = self.video_playback {
            playback.poll_finished();
            if let Some(frame) = playback.take_frame() {
                self.video_renderer.upload_frame(
                    &self.gpu.device,
                    &self.gpu.queue,
                    frame.width,
                    frame.height,
                    &frame.data,
                );
            }
            if playback.finished {
                self.console.system("Video finished");
                self.video_playback = None;
                self.video_renderer.clear();
            }
        }

        // Update status bar clock.
        let now_wall = chrono_time_string();
        self.status_bar.set_time(&now_wall);

        // Watchdog: log a heartbeat every ~10 seconds for crash forensics.
        self.watchdog_frame += 1;
        if now.duration_since(self.watchdog_last).as_secs() >= 10 {
            let uptime = now.duration_since(self.start_time).as_secs();
            info!(
                "watchdog: alive frame={} uptime={}s adapters={} agents={}",
                self.watchdog_frame,
                uptime,
                self.coordinator.adapter_count(),
                self.agent_panes.len(),
            );
            self.watchdog_last = now;
        }
    }

    // -----------------------------------------------------------------------
    // Bus → Brain bridge
    // -----------------------------------------------------------------------

    /// Drain bus events for the brain observer (ID 0xFFFF_FFFE) and convert
    /// them to AiEvents. Static method to avoid borrow conflicts.
    fn drain_bus_to_brain(
        bus: &mut phantom_adapter::EventBus,
        brain: &Option<phantom_brain::brain::BrainHandle>,
    ) {
        const BRAIN_OBSERVER_ID: u32 = 0xFFFF_FFFE;
        let msgs = bus.drain_for(BRAIN_OBSERVER_ID);
        if msgs.is_empty() {
            return;
        }
        let Some(brain) = brain else { return };

        for msg in msgs {
            let ai_event = match msg.event {
                Event::TerminalOutput { bytes, .. } => {
                    Some(AiEvent::OutputChunk(format!("[{bytes} bytes]")))
                }
                Event::CommandComplete { exit_code, .. } => {
                    Some(AiEvent::CommandComplete(
                        phantom_semantic::ParsedOutput {
                            command: String::new(),
                            command_type: phantom_semantic::CommandType::Unknown,
                            exit_code: Some(exit_code),
                            content_type: phantom_semantic::ContentType::PlainText,
                            errors: vec![],
                            warnings: vec![],
                            duration_ms: None,
                            raw_output: String::new(),
                        },
                    ))
                }
                Event::AgentTaskComplete { agent_id, success, summary } => {
                    Some(AiEvent::AgentComplete { id: agent_id, success, summary })
                }
                Event::AgentError { agent_id, error } => {
                    Some(AiEvent::AgentComplete {
                        id: agent_id,
                        success: false,
                        summary: error,
                    })
                }
                _ => None,
            };
            if let Some(event) = ai_event {
                let _ = brain.send_event(event);
            }
        }
    }

    // -----------------------------------------------------------------------
    // MCP command handlers
    // -----------------------------------------------------------------------

    fn handle_mcp_command(&mut self, cmd: AppCommand) {
        match cmd {
            AppCommand::Screenshot { path, reply } => {
                let result = self.mcp_capture_screenshot(&path);
                let _ = reply.send(result);
            }
            AppCommand::RunCommand { command, reply } => {
                let result = self.mcp_send_to_pty(&command);
                let _ = reply.send(result);
            }
            AppCommand::SendKey { key, reply } => {
                let result = self.mcp_send_key(&key);
                let _ = reply.send(result);
            }
            AppCommand::ReadTerminalState { reply } => {
                let text = self.mcp_read_terminal_state();
                let _ = reply.send(Ok(text));
            }
            AppCommand::GetContext { reply } => {
                let json = self.mcp_get_context_json();
                let _ = reply.send(Ok(json));
            }
            AppCommand::PhantomCommand { command, reply } => {
                info!("[MCP]: phantom.command: {command}");
                self.execute_user_command(&command);
                let _ = reply.send(Ok(format!("executed: {command}")));
            }
            AppCommand::ReadOutput { lines, reply } => {
                let text = self.mcp_read_output(lines);
                let _ = reply.send(Ok(text));
            }
            AppCommand::SplitPane { direction, reply } => {
                let horizontal = direction == "horizontal";
                self.split_focused_pane(horizontal);
                let _ = reply.send(Ok(format!("split pane {direction}")));
            }
            AppCommand::GetMemory { key, reply } => {
                let result = self.mcp_get_memory(&key);
                let _ = reply.send(result);
            }
            AppCommand::SetMemory { key, value, reply } => {
                let result = self.mcp_set_memory(&key, &value);
                let _ = reply.send(result);
            }
        }
    }

    fn mcp_capture_screenshot(&mut self, path: &Path) -> Result<ScreenshotReply, String> {
        use phantom_renderer::screenshot::{capture_frame, ScreenshotMetadata, save_screenshot};
        use std::time::{SystemTime, UNIX_EPOCH};

        let texture = self.postfx.scene_texture();
        let width = texture.width();
        let height = texture.height();

        let pixels = capture_frame(&self.gpu.device, &self.gpu.queue, texture, width, height)
            .map_err(|e| format!("capture failed: {e}"))?;

        let pixels_rgba = match self.gpu.format {
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb => {
                let mut out = pixels;
                for px in out.chunks_exact_mut(4) {
                    px.swap(0, 2);
                }
                out
            }
            _ => pixels,
        };

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let metadata = ScreenshotMetadata {
            timestamp,
            width,
            height,
            theme: self.theme.name.clone(),
            pane_count: self.coordinator.adapter_count(),
            project: self.context.as_ref().map(|c| c.name.clone()),
            branch: self.context.as_ref().and_then(|c| c.git.as_ref().map(|g| g.branch.clone())),
        };

        save_screenshot(&pixels_rgba, width, height, &metadata, path)
            .map_err(|e| format!("save failed: {e}"))?;

        info!("Screenshot saved via MCP: {} ({}x{})", path.display(), width, height);

        Ok(ScreenshotReply {
            path: path.to_path_buf(),
            width,
            height,
        })
    }

    fn mcp_send_key(&mut self, key: &str) -> Result<String, String> {
        use crate::pane::key_name_to_bytes;

        if self.state == AppState::Boot {
            if self.boot.is_waiting() {
                self.boot.dismiss();
                return Ok("dismissed boot pause".into());
            }
            self.boot.skip();
            return Ok("skipped boot sequence".into());
        }

        let bytes = key_name_to_bytes(key);
        self.coordinator.send_command_to_focused(
            "write_bytes",
            &serde_json::json!({"bytes": bytes}),
        ).map_err(|e| format!("write_bytes failed: {e}"))?;
        self.last_input_time = Instant::now();
        Ok(format!("wrote {} bytes to pty", bytes.len()))
    }

    fn mcp_send_to_pty(&mut self, command: &str) -> Result<(), String> {
        let mut text = command.to_string();
        if !text.ends_with('\n') {
            text.push('\n');
        }
        self.coordinator.send_command_to_focused(
            "write",
            &serde_json::json!({"text": text}),
        ).map_err(|e| format!("write failed: {e}"))?;
        Ok(())
    }

    fn mcp_read_terminal_state(&self) -> String {
        let Some(focused) = self.coordinator.focused() else {
            return String::new();
        };
        let Some(state) = self.coordinator.get_state(focused) else {
            return String::new();
        };
        // The adapter's get_state() returns JSON with a "text" field
        // containing the rendered terminal grid as text.
        state.get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }

    fn mcp_read_output(&self, lines: usize) -> String {
        let full = self.mcp_read_terminal_state();
        if full.is_empty() {
            return String::new();
        }
        let all_lines: Vec<&str> = full.lines().collect();
        let start = all_lines.len().saturating_sub(lines);
        all_lines[start..].join("\n")
    }

    fn mcp_get_memory(&self, key: &str) -> Result<String, String> {
        let Some(ref mem) = self.memory else {
            return Err("memory store not available".into());
        };
        match mem.get(key) {
            Some(entry) => Ok(entry.value.clone()),
            None => Ok(String::new()),
        }
    }

    fn mcp_set_memory(&mut self, key: &str, value: &str) -> Result<String, String> {
        let Some(ref mut mem) = self.memory else {
            return Err("memory store not available".into());
        };
        mem.set(
            key,
            value,
            phantom_memory::MemoryCategory::Context,
            phantom_memory::MemorySource::Agent,
        )
        .map_err(|e| format!("memory write failed: {e}"))?;
        Ok(format!("stored: {key}"))
    }

    fn mcp_get_context_json(&self) -> serde_json::Value {
        use serde_json::json;
        let Some(ctx) = &self.context else {
            return json!({});
        };
        json!({
            "name": ctx.name,
            "root": ctx.root,
            "project_type": format!("{:?}", ctx.project_type),
            "git": ctx.git.as_ref().map(|g| json!({
                "branch": g.branch,
                "dirty": g.is_dirty,
                "ahead": g.ahead,
                "behind": g.behind,
            })),
            "adapter_count": self.coordinator.adapter_count(),
            "focused_adapter": self.coordinator.focused(),
            "theme": self.theme.name,
        })
    }
}
