//! Per-frame update loop: PTY I/O, error scanning, alt-screen detection,
//! brain event polling, MCP command dispatch, and status bar updates.

use std::path::Path;
use std::sync::mpsc;
use std::time::Instant;

use anyhow::Result;
use log::{debug, info, trace, warn};

use phantom_adapter::BusMessage;
use phantom_brain::events::{AiAction, AiEvent};
use phantom_context::ProjectContext;
use phantom_mcp::{AppCommand, ScreenshotReply};
use phantom_terminal::output;

use crate::app::{App, AppState, SuggestionOverlay};
use crate::input::chrono_time_string;
use crate::pane::pane_cols_rows;

impl App {
    /// Per-frame update: read PTY data, advance boot sequence, update widgets.
    ///
    /// Call this once per frame before [`render`](Self::render).
    pub fn update(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f32();
        self.last_frame = now;

        // Read from all panes' PTYs (non-blocking). Collect indices of exited panes.
        let mut exited: Vec<usize> = Vec::new();
        let mut had_output = false;
        for (i, pane) in self.panes.iter_mut().enumerate() {
            match pane.terminal.pty_read() {
                Ok(n) => {
                    if n > 0 {
                        trace!("Pane {i} PTY read: {n} bytes");
                        had_output = true;

                        // Capture raw output for semantic scanning.
                        let raw = &pane.terminal.last_read_buf()[..n];
                        if let Ok(text) = std::str::from_utf8(raw) {
                            pane.output_buf.push_str(text);
                            if pane.output_buf.len() > 8192 {
                                let excess = pane.output_buf.len() - 8192;
                                let drain_to = pane.output_buf.floor_char_boundary(excess);
                                if drain_to > 0 && pane.output_buf.is_char_boundary(drain_to) {
                                    pane.output_buf.drain(..drain_to);
                                }
                            }
                            pane.error_notified = false;
                        }
                    }
                }
                Err(e) => {
                    warn!("Pane {i} PTY read error (shell may have exited): {e}");
                    exited.push(i);
                }
            }
        }

        // Publish terminal output event to bus (throttled to 1/sec, not every frame).
        if had_output {
            let elapsed_secs = now.duration_since(self.start_time).as_secs();
            if self.event_bus.queue_len() < 128 {
                self.event_bus.emit(BusMessage {
                    topic_id: self.topic_terminal_output,
                    sender: 0,
                    payload: serde_json::json!({ "pane_count": self.panes.len() }),
                    timestamp: elapsed_secs,
                });
            }
        }

        // Semantic scan: detect errors in PTY output and notify brain.
        if let Some(ref brain) = self.brain {
            for pane in self.panes.iter_mut() {
                if pane.error_notified || pane.is_detached || pane.output_buf.is_empty() {
                    continue;
                }
                let buf = &pane.output_buf;
                let has_error = buf.contains("error[E")
                    || buf.contains("Error:")
                    || buf.contains("FAILED")
                    || buf.contains("error: ")
                    || buf.contains("npm ERR!")
                    || buf.contains("Traceback (most recent")
                    || buf.contains("SyntaxError")
                    || buf.contains("TypeError")
                    || buf.contains("panic at");

                if has_error {
                    let parsed = phantom_semantic::SemanticParser::parse(
                        "",
                        &pane.output_buf,
                        &pane.output_buf,
                        Some(1),
                    );
                    if !parsed.errors.is_empty() {
                        let _ = brain.send_event(AiEvent::CommandComplete(parsed));
                        pane.error_notified = true;

                        // Publish error event to bus.
                        self.event_bus.emit(BusMessage {
                            topic_id: self.topic_terminal_error,
                            sender: 0,
                            payload: serde_json::json!({ "has_errors": true }),
                            timestamp: now.duration_since(self.start_time).as_secs(),
                        });
                    }
                }

                let trimmed = buf.trim_end();
                if trimmed.ends_with("$ ") || trimmed.ends_with("% ") || trimmed.ends_with("> ") || trimmed.ends_with("# ") {
                    pane.output_buf.clear();
                }
            }
        }

        // Alt-screen detection.
        for pane in self.panes.iter_mut() {
            let is_alt = phantom_terminal::alt_screen::is_alt_screen(pane.terminal.term());

            if is_alt && !pane.was_alt_screen {
                pane.is_detached = true;
                pane.detached_label = phantom_terminal::process::foreground_process_name(
                    pane.terminal.pty_fd(),
                )
                .unwrap_or_else(|| "interactive".to_string());
                info!("Pane detached: process \"{}\"", pane.detached_label);
            }

            if !is_alt && pane.was_alt_screen && pane.is_detached {
                info!("Pane reattached (was \"{}\")", pane.detached_label);
                pane.is_detached = false;
                pane.detached_label.clear();
            }

            pane.was_alt_screen = is_alt;
        }

        // Remove exited panes.
        for &i in exited.iter().rev() {
            let pane = self.panes.remove(i);
            if let Err(e) = self.layout.remove_pane(pane.pane_id) {
                warn!("Failed to remove exited pane from layout: {e}");
            }
            self.scene.remove_node(pane.scene_node);
            if self.focused_pane >= self.panes.len() && !self.panes.is_empty() {
                self.focused_pane = self.panes.len() - 1;
            }
        }

        if !exited.is_empty() {
            let width = self.gpu.surface_config.width;
            let height = self.gpu.surface_config.height;
            let _ = self.layout.resize(width as f32, height as f32);

            for pane in &mut self.panes {
                if let Ok(rect) = self.layout.get_pane_rect(pane.pane_id) {
                    let (cols, rows) = pane_cols_rows(self.cell_size, rect);
                    pane.terminal.resize(cols, rows);
                }
            }
        }

        if self.panes.is_empty() {
            info!("All panes exited, quitting");
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

        // Supervisor heartbeat & command polling.
        if let Some(ref mut sv) = self.supervisor {
            sv.send_heartbeat();
        }
        let cmd = self.supervisor.as_mut().and_then(|sv| sv.try_recv());
        if let Some(cmd) = cmd {
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
                    AiAction::RunCommand(cmd) => {
                        debug!("Brain suggested command: {cmd}");
                    }
                    AiAction::DoNothing => {}
                }
            }
        }

        // Spawn agent panes (deferred from brain action loop to avoid borrow conflict).
        for task in tasks_to_spawn {
            self.spawn_agent_pane(task);
        }

        // Expire stale suggestions.
        if let Some(ref s) = self.suggestion {
            if now.duration_since(s.shown_at).as_secs() > 10 {
                self.suggestion = None;
            }
        }

        // Refresh git context periodically (off main thread).
        if let Some(ref ctx) = self.context {
            let elapsed = now.duration_since(self.start_time).as_secs();
            if elapsed > 0 && elapsed % 30 == 0 && dt > 0.0 {
                let project_dir = ctx.root.clone();
                let brain_tx = self.brain.as_ref().map(|b| b.event_sender());
                std::thread::spawn(move || {
                    let mut fresh = ProjectContext::detect(std::path::Path::new(&project_dir));
                    fresh.refresh_git();
                    if let Some(tx) = brain_tx {
                        let _ = tx.send(AiEvent::GitStateChanged);
                    }
                });
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
        let agent_count_before = self.agent_panes.iter()
            .filter(|p| p.status == crate::agent_pane::AgentPaneStatus::Working)
            .count();
        self.poll_agent_panes();
        let agent_count_after = self.agent_panes.iter()
            .filter(|p| p.status == crate::agent_pane::AgentPaneStatus::Working)
            .count();
        // If any agent just finished this frame, emit bus events.
        if agent_count_after < agent_count_before {
            for pane in &self.agent_panes {
                if matches!(pane.status, crate::agent_pane::AgentPaneStatus::Done | crate::agent_pane::AgentPaneStatus::Failed) {
                    self.event_bus.emit(BusMessage {
                        topic_id: self.topic_agent_event,
                        sender: 0,
                        payload: serde_json::json!({
                            "task": pane.task,
                            "success": pane.status == crate::agent_pane::AgentPaneStatus::Done,
                        }),
                        timestamp: now.duration_since(self.start_time).as_secs(),
                    });
                }
            }
        }

        // Sync scene graph transforms from layout engine.
        for pane in &self.panes {
            if let Ok(rect) = self.layout.get_pane_rect(pane.pane_id) {
                self.scene.set_transform(
                    pane.scene_node,
                    rect.x, rect.y, rect.width, rect.height,
                );
            }
        }
        self.scene.update_world_transforms();

        // Poll system monitor.
        self.sysmon.poll();

        // Advance keystroke glitch animations.
        self.keystroke_fx.tick();

        // Update status bar clock.
        let now_wall = chrono_time_string();
        self.status_bar.set_time(&now_wall);
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
            pane_count: self.panes.len(),
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
        let pane = self.panes.get_mut(self.focused_pane)
            .ok_or_else(|| "no focused pane".to_string())?;
        pane.terminal.pty_write(&bytes)
            .map_err(|e| format!("pty_write failed: {e}"))?;
        self.last_input_time = Instant::now();
        Ok(format!("wrote {} bytes to pty", bytes.len()))
    }

    fn mcp_send_to_pty(&mut self, command: &str) -> Result<(), String> {
        let pane = self.panes.get_mut(self.focused_pane)
            .ok_or_else(|| "no focused pane".to_string())?;

        let mut bytes = command.as_bytes().to_vec();
        if !bytes.ends_with(b"\n") {
            bytes.push(b'\n');
        }
        pane.terminal.pty_write(&bytes)
            .map_err(|e| format!("pty_write failed: {e}"))?;

        Ok(())
    }

    fn mcp_read_terminal_state(&self) -> String {
        let Some(pane) = self.panes.get(self.focused_pane) else {
            return String::new();
        };
        let (cells, cols, rows, _cursor) = output::extract_grid(pane.terminal.term());

        let mut out = String::with_capacity(cells.len() + rows);
        for row in 0..rows {
            for col in 0..cols {
                let idx = row * cols + col;
                if let Some(cell) = cells.get(idx) {
                    out.push(cell.ch);
                }
            }
            out.push('\n');
        }
        while out.ends_with("\n\n") {
            out.pop();
        }
        out
    }

    fn mcp_read_output(&self, lines: usize) -> String {
        if self.panes.get(self.focused_pane).is_none() {
            return String::new();
        }
        let full = self.mcp_read_terminal_state();
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
            "pane_count": self.panes.len(),
            "focused_pane": self.focused_pane,
            "theme": self.theme.name,
        })
    }
}
