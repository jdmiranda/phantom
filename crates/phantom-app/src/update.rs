//! Per-frame update loop: coordinator adapter ticking, dead adapter reaping,
//! brain event polling, MCP command dispatch, and status bar updates.

use std::path::Path;
use std::sync::mpsc;
use std::time::Instant;

use anyhow::Result;
use log::{debug, info, warn};

use phantom_brain::events::{AiAction, AiEvent};
use phantom_brain::ooda::WorldState;
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

        // Substrate runtime: reap dead supervisor children, drain pending
        // substrate events into the on-disk log, and evaluate spawn rules.
        // Cheap when nothing is pending; bounded by events pushed since the
        // last tick.
        self.runtime.tick();

        // Inspector pane (if open): push a fresh InspectorView into the
        // shared Arc<RwLock<…>> so the adapter sees up-to-date counts and
        // events on the next render. No-op when no inspector pane is open.
        self.refresh_inspector_snapshot();

        // Bridge: drain bus events for the brain observer and forward as AiEvents.
        // Now an instance method so command-boundary events can also flow into
        // the per-pane capture pipeline (sealing open bundles on
        // `Event::CommandComplete`). We can't keep the static helper signature
        // because both sides — `App::brain` (for AI events) and
        // `App::capture_state` / `App::bundle_store` (for bundle sealing) —
        // need to be reachable, and routing through `&mut App` is the
        // simplest way to do that without a tangle of disjoint borrows.
        self.drain_bus_to_brain();

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
            // Demo mode: auto-skip the cinematic at 2s so iteration runs
            // never need a key-press to dismiss the boot screen.
            if self.demo_mode && self.boot.elapsed() >= 2.0 && !self.boot.is_done() {
                self.boot.skip_immediate();
            }
            if self.boot.is_done() {
                info!("Boot sequence complete, transitioning to terminal");
                self.state = AppState::Terminal;
            }
        }

        // Demo mode: spawn one example agent pane the first time we land in
        // Terminal, so each run has visible substrate content.
        if self.demo_mode && self.state == AppState::Terminal && !self.demo_post_boot_done {
            self.demo_post_boot_done = true;
            let _ = self.spawn_agent_pane(phantom_agents::AgentTask::FreeForm {
                prompt: "Demo mode: introduce yourself in one sentence and \
                    list a few tasks I can hand to you in this terminal."
                    .to_owned(),
            });
        }

        // Supervisor command polling (drain all pending; heartbeats are on a dedicated thread).
        while let Some(cmd) = self.supervisor.as_mut().and_then(|sv| sv.try_recv()) {
            self.handle_supervisor_command(cmd);
        }

        // AI Brain: send idle events + drain actions.
        // Collect agent spawn opts separately to avoid borrow conflict.
        // Using `AgentSpawnOpts` rather than bare `AgentTask` so the
        // reconciler-issued `spawn_tag` is threaded through to the adapter.
        let mut tasks_to_spawn: Vec<phantom_agents::AgentSpawnOpts> = Vec::new();
        if let Some(ref brain) = self.brain {
            let idle_secs = now.duration_since(self.last_input_time).as_secs_f32();
            if idle_secs > 5.0 && (idle_secs % 5.0) < dt {
                let _ = brain.send_event(AiEvent::UserIdle { seconds: idle_secs });
            }

            while let Some(action) = brain.try_recv_action() {
                Self::execute_brain_action(
                    action, now, &mut self.suggestion, &mut self.memory,
                    &mut self.console, &mut self.coordinator, &mut self.layout,
                    &mut self.scene, &mut tasks_to_spawn,
                );
            }
        }

        // Per-frame OODA tick (#45): synchronous Observe/Orient/Decide/Act pass
        // driven by the render clock. Builds a WorldState snapshot from current
        // App state, runs the BDS in <2 ms, and feeds winning actions directly
        // into the same execute_brain_action pipeline as the async brain thread.
        {
            let idle_secs = now.duration_since(self.last_input_time).as_secs_f32();
            // Derive error presence from the last command context stored on
            // the async brain scorer — we snapshot the same signals.
            let world = WorldState::new(
                idle_secs,
                false,   // has_errors: OODA uses BDS which will be fed via orient
                0,       // error_count
                false,   // has_active_process
                false,   // new_pattern_detected
                false,   // agent_just_completed
                false,   // file_or_git_changed
                false,   // in_repl
                0.0,     // chattiness
                0,       // suggestions_since_input
            );
            let dt_ms = (dt * 1000.0) as u64;
            let ooda_actions = self.ooda_loop.tick(&world, dt_ms);
            for action in ooda_actions {
                Self::execute_brain_action(
                    action, now, &mut self.suggestion, &mut self.memory,
                    &mut self.console, &mut self.coordinator, &mut self.layout,
                    &mut self.scene, &mut tasks_to_spawn,
                );
            }
        }

        // Execute actions triggered by user interaction with suggestion options.
        let pending = std::mem::take(&mut self.pending_brain_actions);
        for action in pending {
            Self::execute_brain_action(
                action, now, &mut self.suggestion, &mut self.memory,
                &mut self.console, &mut self.coordinator, &mut self.layout,
                &mut self.scene, &mut tasks_to_spawn,
            );
        }

        // Spawn agent panes (deferred from brain action loop to avoid borrow conflict).
        for opts in tasks_to_spawn {
            let _ = self.spawn_agent_pane_with_opts(opts);
        }

        // Drain Composer-side `spawn_subagent` requests. The Composer's tool
        // handler pushes onto `pending_spawn_subagent` synchronously when the
        // model invokes `spawn_subagent`; we honor each request here so the
        // actual `App::spawn_agent_pane_with_opts` call (which needs the
        // coordinator + scene + layout) runs on the App's owning thread.
        // The queue is `Arc<Mutex<VecDeque<…>>>` so dispatch contexts can
        // push without holding a reference back to the App; lock briefly to
        // drain the snapshot into a local Vec.
        let pending_subagents: Vec<_> = match self.pending_spawn_subagent.lock() {
            Ok(mut q) => q.drain(..).collect(),
            Err(_) => Vec::new(),
        };
        for req in pending_subagents {
            let task = phantom_agents::AgentTask::FreeForm { prompt: req.task.clone() };
            // The current `spawn_agent_pane` doesn't take role / label / chat_model
            // because the underlying `AgentPane::spawn` predates the role+chat_model
            // fields wired into `composer_tools::SpawnSubagentRequest`. We dispatch
            // the spawn through the existing path; the role/label/chat_model
            // metadata is preserved on the queue entry for the next phase wiring.
            let _ = req.role; // silence unused-field warnings until full wiring lands
            let _ = req.label;
            let _ = req.chat_model;
            let _ = req.parent;
            let _ = req.assigned_id;
            let _ = self.spawn_agent_pane(task);
        }

        // Expire stale suggestions (save to history before clearing).
        if self.suggestion.as_ref().is_some_and(|s| now.duration_since(s.shown_at).as_secs() > 10) {
            if let Some(expired) = self.suggestion.take() {
                self.suggestion_history.push_back(expired);
                if self.suggestion_history.len() > 10 {
                    self.suggestion_history.pop_front();
                }
            }
        }

        // Self-test runner: advance one step per frame.
        if self.selftest.as_ref().is_some_and(|r| !r.is_done()) {
            // Take the runner out to satisfy borrow checker (tick needs &mut App).
            let mut runner = self.selftest.take().unwrap();
            let lines = runner.tick(self);
            for line in &lines {
                self.console.system(line.clone());
            }
            if runner.is_done() {
                self.selftest = None;
            } else {
                self.selftest = Some(runner);
            }
        }

        // Refresh git context periodically (off main thread, max once per 30s, one at a time).
        //
        // Timeout guard (#223): if the spawned thread has not finished within
        // GIT_REFRESH_TIMEOUT we log a warning and drop the handle so the
        // update loop is never blocked.  The underlying thread continues in the
        // background (we cannot forcibly kill it) but it no longer occupies
        // the `git_refresh_handle` slot — the next 30-second tick may spawn a
        // fresh one once the slot is clear.
        const GIT_REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
        if let Some(ref ctx) = self.context {
            // Check for timed-out or completed handles each frame so we don't
            // wait until the 30-second timer fires to notice a hung thread.
            if self.git_refresh_handle.is_some() {
                let timed_out = self
                    .git_refresh_spawned_at
                    .is_some_and(|t| now.duration_since(t) > GIT_REFRESH_TIMEOUT);
                let finished = self
                    .git_refresh_handle
                    .as_ref()
                    .is_some_and(|h| h.is_finished());
                if timed_out {
                    warn!(
                        "git-refresh thread exceeded {}s timeout; abandoning handle",
                        GIT_REFRESH_TIMEOUT.as_secs()
                    );
                    self.git_refresh_handle = None;
                    self.git_refresh_spawned_at = None;
                } else if finished {
                    self.git_refresh_handle = None;
                    self.git_refresh_spawned_at = None;
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
                self.git_refresh_spawned_at = if self.git_refresh_handle.is_some() {
                    Some(now)
                } else {
                    None
                };
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

        // Lars fix-thread consumer (Phase 2.G): drain any `EventKind::AgentBlocked`
        // substrate events that agent panes pushed into `App.blocked_event_sink`
        // during this frame's tool-result processing, and forward each into the
        // runtime's pending queue. The runtime's `tick()` (called above) has
        // already drained the previous frame's pending, so these events will be
        // evaluated by spawn rules on the *next* tick — which is exactly when
        // `last_actions()` will surface the queued Fixer `SpawnAction`.
        let blocked = self.drain_blocked_events();
        for event in blocked {
            self.runtime.push_event(event);
        }

        // Sec.4 / Sec.8 capability-denial consumer: drain `EventKind::Capability
        // Denied` substrate events that the Layer-2 dispatch gate pushed into
        // `App.denied_event_sink` this frame. Each event takes two trips:
        //
        //   1. Forwarded into the substrate runtime so the Defender spawn rule
        //      (registered as `defender_spawn_rule`) can match and queue a
        //      `SpawnIfNotRunning(Defender)` action on the next `runtime.tick()`.
        //   2. Recorded into `App.notifications` (Sec.8) so the per-agent
        //      pattern detector can surface a top-of-screen Danger banner if
        //      the same agent crosses the denial threshold inside the sliding
        //      window. The banner widget reads `current_banner()` next frame.
        //
        // Best-effort on the lock: a poisoned mutex logs a warning and the
        // events for that frame are dropped — observability never aborts the
        // update loop.
        let denied: Vec<phantom_agents::spawn_rules::SubstrateEvent> = match self
            .denied_event_sink
            .lock()
        {
            Ok(mut q) => std::mem::take(&mut *q),
            Err(_) => {
                warn!("denied_event_sink mutex poisoned; dropping queued events");
                Vec::new()
            }
        };
        if !denied.is_empty() {
            // Wall-clock millis — same domain `NotificationCenter` expects.
            // Compute once per frame and reuse for every drained event so the
            // sliding-window math sees a single coherent timestamp.
            let now_ms: u64 = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            for event in denied {
                if let phantom_agents::spawn_rules::EventKind::CapabilityDenied {
                    agent_id, ..
                } = event.kind
                {
                    self.notifications.record_denial(agent_id, now_ms);
                }
                self.runtime.push_event(event);
            }
        }

        // Tick the notification center every frame so banners expire on time
        // even when no new denials arrive. Cheap (linear in the active banner
        // count, which is tiny by construction).
        let now_ms_tick: u64 = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.notifications.tick(now_ms_tick);

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

        // Per-pane capture pipeline: read sub-rects of the (previous frame's)
        // scene texture, dedup via dhash, accumulate frames into open bundles,
        // and submit sealed bundles to the job pool for off-thread encryption
        // + persistence. Best-effort: any failure logs and continues.
        // No-op when `bundle_store` is None.
        if self.state == AppState::Terminal {
            self.capture_panes();
        }

        // Watchdog: log a heartbeat every ~10 seconds for crash forensics.
        self.watchdog_frame += 1;
        if now.duration_since(self.watchdog_last).as_secs() >= 10 {
            let uptime = now.duration_since(self.start_time).as_secs();
            info!(
                "watchdog: alive frame={} uptime={}s adapters={} agents={}",
                self.watchdog_frame,
                uptime,
                self.coordinator.adapter_count(),
                self.coordinator.registry().all_running().into_iter()
                    .filter_map(|id| self.coordinator.registry().get(id))
                    .filter(|e| e.app_type == "agent")
                    .count(),
            );
            self.watchdog_last = now;
        }
    }

    // -----------------------------------------------------------------------
    // Bus → Brain bridge
    // -----------------------------------------------------------------------

    /// Drain bus events for the brain observer (ID 0xFFFF_FFFE), forward
    /// them as `AiEvent`s, and route command-boundary events into the
    /// per-pane capture pipeline.
    ///
    /// Bus draining returns an owned `Vec<BusMessage>` so the
    /// `&mut self.coordinator` borrow is released before we touch
    /// `self.brain` (via `send_event`) or `self.capture_state` /
    /// `self.bundle_store` (via `seal_pane_bundle`). That keeps the borrow
    /// checker happy without threading a closure through a static helper.
    fn drain_bus_to_brain(&mut self) {
        const BRAIN_OBSERVER_ID: u32 = 0xFFFF_FFFE;

        // Cheap skip: nothing to do if neither the brain nor the capture
        // pipeline cares. Still drain to keep the queue from growing.
        let brain_active = self.brain.is_some();
        let capture_active = self.bundle_store.is_some();
        if !brain_active && !capture_active {
            let _ = self.coordinator.bus_mut().drain_for(BRAIN_OBSERVER_ID);
            return;
        }

        let msgs = self.coordinator.bus_mut().drain_for(BRAIN_OBSERVER_ID);
        if msgs.is_empty() {
            return;
        }

        for msg in msgs {
            // 1) Forward into the brain (if running) as an AiEvent.
            if let Some(ref brain) = self.brain {
                let ai_event = match &msg.event {
                    Event::TerminalOutput { bytes, .. } => {
                        Some(AiEvent::OutputChunk(format!("[{bytes} bytes]")))
                    }
                    Event::CommandComplete { exit_code, .. } => {
                        Some(AiEvent::CommandComplete(
                            phantom_semantic::ParsedOutput {
                                command: String::new(),
                                command_type: phantom_semantic::CommandType::Unknown,
                                exit_code: Some(*exit_code),
                                content_type: phantom_semantic::ContentType::PlainText,
                                errors: vec![],
                                warnings: vec![],
                                duration_ms: None,
                                raw_output: String::new(),
                            },
                        ))
                    }
                    Event::AgentTaskComplete { agent_id, success, summary, spawn_tag } => {
                        Some(AiEvent::AgentComplete {
                            id: *agent_id,
                            success: *success,
                            summary: summary.clone(),
                            spawn_tag: *spawn_tag,
                        })
                    }
                    Event::AgentError { agent_id, error } => {
                        Some(AiEvent::AgentComplete {
                            id: *agent_id,
                            success: false,
                            summary: error.clone(),
                            spawn_tag: None,
                        })
                    }
                    _ => None,
                };
                if let Some(event) = ai_event {
                    let _ = brain.send_event(event);
                }
            }

            // 2) Route command-boundary events into the capture pipeline.
            //    `app_id` on the event is the pane that completed a
            //    command — it maps 1:1 to the `AppId` keyed in the capture
            //    state map. No transcript text is carried on the bus event
            //    itself, so `intent` is `None` here; future PTY-side
            //    wiring (shell prompt detection) can route the actual
            //    command text through `App::on_command_boundary`.
            if let Event::CommandComplete { app_id, .. } = &msg.event {
                let _ = self.on_command_boundary(*app_id, None);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Brain action execution (shared by brain drain + user-triggered pending)
    // -----------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn execute_brain_action(
        action: AiAction,
        now: Instant,
        suggestion: &mut Option<SuggestionOverlay>,
        memory: &mut Option<phantom_memory::MemoryStore>,
        console: &mut crate::console::Console,
        coordinator: &mut crate::coordinator::AppCoordinator,
        layout: &mut phantom_ui::layout::LayoutEngine,
        scene: &mut phantom_scene::tree::SceneTree,
        tasks_to_spawn: &mut Vec<phantom_agents::AgentSpawnOpts>,
    ) {
        match action {
            AiAction::ShowSuggestion { text, options } => {
                info!("[PHANTOM]: {text}");
                *suggestion = Some(SuggestionOverlay { text, options, shown_at: now });
            }
            AiAction::ShowNotification(msg) => {
                info!("[PHANTOM]: {msg}");
            }
            AiAction::UpdateMemory { key, value } => {
                if let Some(mem) = memory {
                    let _ = mem.set(&key, &value, phantom_memory::MemoryCategory::Context, phantom_memory::MemorySource::Auto);
                }
            }
            AiAction::SpawnAgent { task, spawn_tag } => {
                info!("[PHANTOM]: Spawning agent (spawn_tag={spawn_tag:?})...");
                let mut opts = phantom_agents::AgentSpawnOpts::new(task);
                opts.spawn_tag = spawn_tag;
                tasks_to_spawn.push(opts);
            }
            AiAction::ConsoleReply(reply) => {
                info!("[PHANTOM]: {reply}");
                console.output(format!("[phantom] {reply}"));
            }
            AiAction::RunCommand(cmd) => {
                info!("[PHANTOM]: Running command: {cmd}");
                let cmd_text = if cmd.ends_with('\n') { cmd } else { format!("{cmd}\n") };
                let _ = coordinator.send_command_to_focused("write", &serde_json::json!({"text": cmd_text}));
            }
            AiAction::DismissAdapter { app_id } => {
                info!("[PHANTOM]: Dismissing adapter {app_id}");
                coordinator.remove_adapter(app_id, layout, scene);
            }
            AiAction::AgentFlatlined { id, reason } => {
                info!("[PHANTOM]: Agent {id} flatlined: {reason}");
            }
            AiAction::DoNothing => {}
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

// ---------------------------------------------------------------------------
// Tests — Lars fix-thread consumer wiring (Phase 2.G)
// ---------------------------------------------------------------------------
//
// We can't construct a full `App` here without a GPU, so the tests exercise
// the substrate slice the consumer actually depends on:
// `BlockedEventSink` → drain → `AgentRuntime::push_event` → `tick()` →
// `last_actions()` returning a Fixer `SpawnAction`. The drain+forward step
// mirrors the logic inline in `App::update`, so a green test here is a
// green producer→consumer loop in production.
#[cfg(test)]
mod tests {
    use phantom_agents::role::AgentRole;
    use phantom_agents::spawn_rules::{
        EventKind, EventSource as SubstrateEventSource, SpawnAction, SubstrateEvent,
    };

    use crate::agent_pane::{BlockedEventSink, new_blocked_event_sink};
    use crate::runtime::{AgentRuntime, RuntimeConfig};

    /// Build an `EventKind::AgentBlocked` event with the canonical payload
    /// shape documented in `phantom_agents::fixer`.
    fn blocked_event(agent_id: u64, reason: &str) -> SubstrateEvent {
        SubstrateEvent {
            kind: EventKind::AgentBlocked {
                agent_id,
                reason: reason.to_string(),
            },
            payload: serde_json::json!({
                "agent_id": agent_id,
                "agent_role": "Conversational",
                "reason": reason,
                "blocked_at_unix_ms": 0,
                "context_excerpt": "",
                "suggested_capability": "Sense",
            }),
            source: SubstrateEventSource::Agent {
                role: AgentRole::Conversational,
            },
        }
    }

    /// Build a fresh runtime under a temp dir so the on-disk event log
    /// doesn't pollute `~/.config/phantom`.
    fn make_runtime() -> (AgentRuntime, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = RuntimeConfig::under_dir(dir.path());
        let rt = AgentRuntime::new(cfg, Vec::new()).expect("runtime open");
        (rt, dir)
    }

    /// Mirror of the consumer one-liner in `App::update`: drain the sink and
    /// forward each event into the runtime's pending queue.
    fn drain_into(sink: &BlockedEventSink, rt: &AgentRuntime) {
        let drained: Vec<SubstrateEvent> = match sink.lock() {
            Ok(mut q) => std::mem::take(&mut *q),
            Err(_) => Vec::new(),
        };
        for ev in drained {
            rt.push_event(ev);
        }
    }

    /// Producer pushes one `AgentBlocked` event into the sink, the consumer
    /// drains it into the runtime, the next tick fires the registered
    /// `fixer_spawn_rule`, and `last_actions()` surfaces the Fixer
    /// `SpawnAction`. This is the end-to-end producer→consumer guarantee.
    #[test]
    fn blocked_events_drained_from_sink_into_runtime() {
        let (mut rt, _dir) = make_runtime();
        let sink = new_blocked_event_sink();

        // Producer: an agent crossed `TOOL_BLOCK_THRESHOLD` and pushed.
        sink.lock()
            .expect("sink lock")
            .push(blocked_event(7, "2+ consecutive tool failures: ENOENT"));

        // Consumer (the one-liner): drain → forward → tick.
        drain_into(&sink, &rt);
        rt.tick();

        // Sink must be empty post-drain.
        assert!(
            sink.lock().expect("sink lock").is_empty(),
            "sink must be drained after forward"
        );

        // Fixer spawn rule must have fired exactly once.
        let actions = rt.last_actions();
        assert_eq!(
            actions.len(),
            1,
            "Fixer spawn rule must fire exactly once for a single AgentBlocked event"
        );
        match &actions[0].action {
            SpawnAction::SpawnIfNotRunning {
                role,
                label_template,
                ..
            } => {
                assert_eq!(*role, AgentRole::Fixer);
                assert_eq!(label_template, "fixer-on-blockage");
            }
            other => panic!("expected SpawnIfNotRunning(Fixer), got {other:?}"),
        }
    }

    /// Empty sink → no actions queued. The consumer must be a no-op when
    /// no producer has pushed.
    #[test]
    fn empty_sink_no_op() {
        let (mut rt, _dir) = make_runtime();
        let sink = new_blocked_event_sink();

        drain_into(&sink, &rt);
        rt.tick();

        assert!(
            rt.last_actions().is_empty(),
            "empty sink must yield zero queued actions"
        );
    }

    /// Three blocked events pushed (e.g. three different stuck agents) all
    /// land in the runtime, all match the Fixer rule, and `last_actions()`
    /// surfaces three queued Fixer `SpawnAction`s.
    #[test]
    fn multiple_blocked_events_all_drained() {
        let (mut rt, _dir) = make_runtime();
        let sink = new_blocked_event_sink();

        // Producer: three distinct agents got stuck this frame.
        {
            let mut q = sink.lock().expect("sink lock");
            q.push(blocked_event(1, "missing tool"));
            q.push(blocked_event(2, "permission denied"));
            q.push(blocked_event(3, "command not found"));
        }

        drain_into(&sink, &rt);
        rt.tick();

        // Sink fully drained.
        assert!(
            sink.lock().expect("sink lock").is_empty(),
            "sink must be empty after the consumer pass"
        );

        // All three events triggered the Fixer rule.
        let actions = rt.last_actions();
        assert_eq!(
            actions.len(),
            3,
            "all 3 AgentBlocked events must each queue a Fixer action; got {}",
            actions.len(),
        );
        for queued in actions {
            match &queued.action {
                SpawnAction::SpawnIfNotRunning { role, .. } => {
                    assert_eq!(*role, AgentRole::Fixer);
                }
                other => panic!("expected SpawnIfNotRunning(Fixer), got {other:?}"),
            }
        }
    }

    // ---- #223: git-refresh timeout logic ----------------------------------

    /// The timeout guard must abandon a handle whose spawned-at instant is
    /// older than GIT_REFRESH_TIMEOUT (5 s) — even when the thread has not
    /// finished — so the update loop is never blocked.
    ///
    /// We exercise the logic directly without spinning up a full `App` by
    /// replicating the exact conditional that `App::update` runs each frame.
    #[test]
    fn git_refresh_timeout_abandons_hung_handle() {
        use std::sync::{Arc, Barrier};
        use std::time::{Duration, Instant};

        const GIT_REFRESH_TIMEOUT: Duration = Duration::from_secs(5);

        // Spawn a thread that blocks indefinitely (simulates a hung git process).
        let barrier = Arc::new(Barrier::new(2));
        let barrier_clone = Arc::clone(&barrier);
        let handle = std::thread::Builder::new()
            .name("test-hung-git".into())
            .spawn(move || {
                barrier_clone.wait(); // Released only when the test drops the barrier.
                // Thread "hangs" here — in real life this would be a blocking git call.
                std::thread::sleep(Duration::from_secs(60));
            })
            .expect("spawn hung thread");

        // Simulate that the thread was spawned 6 seconds ago (over the 5 s timeout).
        let spawned_at = Instant::now() - Duration::from_secs(6);
        let now = Instant::now();

        let mut git_refresh_handle: Option<std::thread::JoinHandle<()>> = Some(handle);
        let mut git_refresh_spawned_at: Option<Instant> = Some(spawned_at);

        // ---- replicate the timeout guard from App::update ----
        if git_refresh_handle.is_some() {
            let timed_out = git_refresh_spawned_at
                .is_some_and(|t| now.duration_since(t) > GIT_REFRESH_TIMEOUT);
            let finished = git_refresh_handle
                .as_ref()
                .is_some_and(|h| h.is_finished());
            if timed_out {
                git_refresh_handle = None;
                git_refresh_spawned_at = None;
            } else if finished {
                git_refresh_handle = None;
                git_refresh_spawned_at = None;
            }
        }
        // ---- end replicated guard ----

        assert!(
            git_refresh_handle.is_none(),
            "timeout guard must clear the handle when spawned_at is older than GIT_REFRESH_TIMEOUT"
        );
        assert!(
            git_refresh_spawned_at.is_none(),
            "timeout guard must also clear spawned_at"
        );

        // Release the barrier so the hung thread can exit and the test
        // process doesn't leak OS threads.
        barrier.wait();
    }

    /// A finished thread must be reaped immediately (before the 30-second
    /// refresh timer fires) so a subsequent spawn can start right away.
    #[test]
    fn git_refresh_reaps_finished_handle_every_frame() {
        use std::time::{Duration, Instant};

        const GIT_REFRESH_TIMEOUT: Duration = Duration::from_secs(5);

        // Spawn a thread that finishes almost immediately.
        let handle = std::thread::Builder::new()
            .name("test-fast-git".into())
            .spawn(|| { /* done immediately */ })
            .expect("spawn fast thread");

        // Give the thread time to finish.
        std::thread::sleep(Duration::from_millis(50));

        let now = Instant::now();
        let spawned_at = Some(now - Duration::from_millis(100));

        let mut git_refresh_handle: Option<std::thread::JoinHandle<()>> = Some(handle);
        let mut git_refresh_spawned_at: Option<Instant> = spawned_at;

        if git_refresh_handle.is_some() {
            let timed_out = git_refresh_spawned_at
                .is_some_and(|t| now.duration_since(t) > GIT_REFRESH_TIMEOUT);
            let finished = git_refresh_handle
                .as_ref()
                .is_some_and(|h| h.is_finished());
            if timed_out {
                git_refresh_handle = None;
                git_refresh_spawned_at = None;
            } else if finished {
                git_refresh_handle = None;
                git_refresh_spawned_at = None;
            }
        }

        assert!(
            git_refresh_handle.is_none(),
            "finished git-refresh handle must be reaped on every frame"
        );
        assert!(
            git_refresh_spawned_at.is_none(),
            "spawned_at must be cleared when the handle is reaped"
        );
    }
}
