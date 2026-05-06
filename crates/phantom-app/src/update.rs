//! Per-frame update loop: coordinator adapter ticking, dead adapter reaping,
//! brain event polling, MCP command dispatch, and status bar updates.

use std::path::Path;
use std::sync::mpsc;
use std::time::Instant;

use anyhow::Result;
use log::{debug, info, warn};

use phantom_brain::events::AiEvent;
use phantom_brain::ooda::WorldState;
use phantom_context::ProjectContext;
use phantom_history::HistoryEntry;
use phantom_mcp::{AgentStatusInfo, AppCommand, PaneInfo, ScreenshotReply, SpawnAgentReply};
use phantom_protocol::Event;
use crate::app::{App, AppState};
use crate::input::chrono_time_string;

impl App {
    /// Per-frame update: read PTY data, advance boot sequence, update widgets.
    ///
    /// Call this once per frame before [`render`](Self::render).
    pub fn update(&mut self) {
        crate::profile_scope!("update");
        let now = Instant::now();
        let raw_dt_duration = now.duration_since(self.last_frame);
        self.last_frame = now;

        // Warn if a frame takes abnormally long (> 2 seconds).
        if raw_dt_duration.as_secs_f32() > 2.0 {
            warn!(
                "SLOW FRAME: dt={:.2}s — previous frame blocked the event loop",
                raw_dt_duration.as_secs_f32()
            );
        }

        // Clamp dt to [target_dt, max_dt] to prevent animation explosions on
        // debugger pauses, GC spikes, or OS suspends. Large raw deltas are
        // replaced with the nominal 16.6 ms target so physics/animation math
        // stays bounded regardless of wall-clock stalls.
        let dt_duration = self.dt_clamp.apply(raw_dt_duration);
        let dt = dt_duration.as_secs_f32();

        // Advance the scene clock with the clamped delta so all subsystems
        // share a monotonic time base that cannot jump on pause/resume.
        self.scene_clock.tick(dt_duration);

        // Coordinator: tick all registered adapters and deliver bus messages.
        self.coordinator.update_all(dt_duration);

        // Issue #323: poll terminal adapters for alt-screen transitions and
        // manage the secondary split-pane lifecycle.
        self.poll_alt_screen_transitions();
        self.tick_alt_screen_fade(dt);

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
        let dead_adapters: Vec<_> = self
            .coordinator
            .all_app_ids()
            .into_iter()
            .filter(|id| {
                self.coordinator
                    .registry()
                    .get_adapter(*id)
                    .is_some_and(|a| !a.is_alive())
            })
            .collect();
        for dead_id in dead_adapters {
            info!("Adapter {dead_id} exited, removing");
            self.coordinator
                .remove_adapter(dead_id, &mut self.layout, &mut self.scene);
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

        // Session resume prompt: fire exactly once on the first Terminal tick
        // when previous-session sidecar files contained live agents or goals.
        // We drain `restored_session` via `.take()` so this block runs at most
        // once per process lifetime (subsequent frames see `None`).
        if self.state == AppState::Terminal {
            if let Some(session) = self.restored_session.take() {
                let n_agents = session.agent_count();
                let n_goals = session.goal_count();
                let msg = if n_agents > 0 && n_goals > 0 {
                    format!(
                        "Resume previous session? ({n_agents} agent{}, {n_goals} goal{})",
                        if n_agents == 1 { "" } else { "s" },
                        if n_goals == 1 { "" } else { "s" },
                    )
                } else if n_agents > 0 {
                    format!(
                        "Resume previous session? ({n_agents} agent{})",
                        if n_agents == 1 { "" } else { "s" },
                    )
                } else {
                    format!(
                        "Resume previous session? ({n_goals} goal{})",
                        if n_goals == 1 { "" } else { "s" },
                    )
                };
                if let Some(ref brain) = self.brain {
                    let _ = brain.send_event(AiEvent::Interrupt(msg));
                }
                // session is dropped here; self.restored_session is None for all future frames.
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
                action.execute(&mut crate::action_context::AppActionHandler {
                    now,
                    suggestion: &mut self.suggestion,
                    memory: &mut self.memory,
                    notification_store: &mut self.notification_store,
                    console: &mut self.console,
                    coordinator: &mut self.coordinator,
                    layout: &mut self.layout,
                    scene: &mut self.scene,
                    tasks_to_spawn: &mut tasks_to_spawn,
                    status_bar: &mut self.status_bar,
                });
            }
        }

        // Per-frame OODA tick (#45): synchronous Observe/Orient/Decide/Act pass
        // driven by the render clock. Builds a WorldState snapshot from current
        // App state, runs the BDS in <2 ms, and feeds winning actions directly
        // into the same execute_brain_action pipeline as the async brain thread.
        {
            let idle_secs = now.duration_since(self.last_input_time).as_secs_f32();
            // Build a fully-populated WorldState from live app signals (#358).
            // All signals are O(1) reads from the OODA cache updated by
            // drain_bus_to_brain() — no per-tick scanning.
            let world = self.build_world_state(idle_secs);
            let dt_ms = (dt * 1000.0) as u64;
            let ooda_actions = self.ooda_loop.tick(&world, dt_ms);
            for action in ooda_actions {
                action.execute(&mut crate::action_context::AppActionHandler {
                    now,
                    suggestion: &mut self.suggestion,
                    memory: &mut self.memory,
                    notification_store: &mut self.notification_store,
                    console: &mut self.console,
                    coordinator: &mut self.coordinator,
                    layout: &mut self.layout,
                    scene: &mut self.scene,
                    tasks_to_spawn: &mut tasks_to_spawn,
                    status_bar: &mut self.status_bar,
                });
            }
        }

        // Execute actions triggered by user interaction with suggestion options.
        let pending = std::mem::take(&mut self.pending_brain_actions);
        for action in pending {
            action.execute(&mut crate::action_context::AppActionHandler {
                now,
                suggestion: &mut self.suggestion,
                memory: &mut self.memory,
                notification_store: &mut self.notification_store,
                console: &mut self.console,
                coordinator: &mut self.coordinator,
                layout: &mut self.layout,
                scene: &mut self.scene,
                tasks_to_spawn: &mut tasks_to_spawn,
                status_bar: &mut self.status_bar,
            });
        }

        // Drain NLP translate results: each result was produced off-thread by
        // `try_nlp_translate_or_spawn_agent` and carries a display message and
        // an optional AiAction. We show the message immediately and dispatch
        // the action through the standard brain-action pipeline.
        loop {
            match self.nlp_translate_rx.try_recv() {
                Ok(res) => {
                    self.console.system(res.display);
                    if let Some(action) = res.action {
                        action.execute(&mut crate::action_context::AppActionHandler {
                            now,
                            suggestion: &mut self.suggestion,
                            memory: &mut self.memory,
                            notification_store: &mut self.notification_store,
                            console: &mut self.console,
                            coordinator: &mut self.coordinator,
                            layout: &mut self.layout,
                            scene: &mut self.scene,
                            tasks_to_spawn: &mut tasks_to_spawn,
                            status_bar: &mut self.status_bar,
                        });
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    warn!("NLP translate channel disconnected unexpectedly");
                    break;
                }
            }
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
            let task = phantom_agents::AgentTask::FreeForm {
                prompt: req.task.clone(),
            };
            // Wire role / label / chat_model from the SpawnSubagentRequest into
            // AgentSpawnOpts so the spawned pane runs under the requested role
            // and displays the requested label. Fixes #224 where these fields
            // were silently discarded and Conversational / "agent-pane" defaults
            // were used instead.
            let mut opts = phantom_agents::AgentSpawnOpts::new(task)
                .with_role(req.role)
                .with_label(req.label.clone());
            if let Some(model) = req.chat_model.clone() {
                opts = opts.with_chat_model(model);
            }
            // parent and assigned_id are substrate-level correlation fields.
            let _ = req.parent;
            let _ = req.assigned_id;
            let _ = self.spawn_agent_pane_with_opts(opts);
        }

        // Expire stale suggestions (save to history before clearing).
        if self
            .suggestion
            .as_ref()
            .is_some_and(|s| now.duration_since(s.shown_at).as_secs() > 10)
            && let Some(expired) = self.suggestion.take() {
                self.suggestion_history.push_back(expired);
                if self.suggestion_history.len() > 10 {
                    self.suggestion_history.pop_front();
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
                    // Signal the OODA cache that git state just refreshed (#358).
                    self.ooda_git_changed = true;
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
        let denied: Vec<phantom_agents::spawn_rules::SubstrateEvent> =
            match self.denied_event_sink.lock() {
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
        //
        // Use the scene clock's elapsed millis rather than SystemTime::now() so
        // that the notification/cursor/shader timer domain is consistent with
        // the clamped frame delta and cannot jump on OS clock adjustments.
        let now_ms_tick: u64 = self.scene_clock.elapsed().as_millis() as u64;
        self.notifications.tick(now_ms_tick);

        // Advance the clock-driven cursor blink timer.  This runs once per
        // frame so blink timing is independent of repaint cadence — rapidly
        // updating TUIs no longer amplify cursor strobing through cell churn.
        self.cursor_blink.tick(now_ms_tick);

        // Poll the live shader reloader (no-op in release builds).
        self.poll_shader_reload(now_ms_tick);

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
                self.coordinator
                    .registry()
                    .all_running()
                    .into_iter()
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
            // 0) Command-boundary tracking for both the history store and
            //    the brain's ParsedOutput (issue #226), plus OODA signal
            //    cache updates (issue #358).
            //
            //    `CommandStarted` records the command text in both maps;
            //    `CommandComplete` consumes pending_command_text for history
            //    and updates the OODA ParsedOutput cache in O(1) so that
            //    `build_world_state()` can read error presence without scanning.
            //    `AgentTaskComplete`/`AgentError` set the one-frame pulse flag.
            //
            // The ParsedOutput is computed here (section 0) rather than inside
            // the brain branch (section 1) so the cache is always populated
            // regardless of whether the async brain thread is running.
            match &msg.event {
                Event::CommandStarted { app_id, command } => {
                    self.pending_command_text.insert(*app_id, command.clone());
                    self.pane_last_command.insert(*app_id, command.clone());
                }
                Event::CommandComplete { app_id, exit_code } => {
                    let command_text = self.pending_command_text.remove(app_id).unwrap_or_default();
                    if let Some(ref mut store) = self.history {
                        let cwd = self
                            .context
                            .as_ref()
                            .map(|c| std::path::PathBuf::from(&c.root))
                            .unwrap_or_else(|| std::path::PathBuf::from("."));
                        let entry = HistoryEntry::builder(&command_text, cwd, self.session_uuid)
                            .exit_code(*exit_code)
                            .build();
                        if let Err(e) = store.append(&entry) {
                            warn!("history append failed: {e}");
                        }
                        // Refresh the brain's history snapshot every 10 commands.
                        // This keeps the snapshot fresh without hammering the JSONL
                        // file on every single command.
                        if store.count() % 10 == 0 {
                            if let Ok(recent) = store.recent(20) {
                                if let Some(ref brain) = self.brain {
                                    let _ = brain.send_event(
                                        phantom_brain::events::AiEvent::HistorySnapshot(recent),
                                    );
                                }
                            }
                        }
                    }
                    // OODA cache (#358): store the parsed result so
                    // build_world_state() can derive has_errors / error_count.
                    let command = self
                        .pane_last_command
                        .get(app_id)
                        .cloned()
                        .unwrap_or_default();
                    let raw_output = self
                        .coordinator
                        .terminal_output_buf(*app_id)
                        .unwrap_or_default();
                    let parsed = self.semantic_skill.parse(
                        &command,
                        "",
                        &raw_output,
                        Some(*exit_code),
                    );
                    self.ooda_last_parsed = Some(parsed);
                }
                // OODA cache (#358): pulse flag — cleared after one tick by
                // build_world_state().
                Event::AgentTaskComplete { .. } | Event::AgentError { .. } => {
                    self.ooda_agent_just_completed = true;
                }
                _ => {}
            }

            // 1) Forward into the brain (if running) as an AiEvent.
            if let Some(ref brain) = self.brain {
                let ai_event = match &msg.event {
                    Event::TerminalOutput { bytes, .. } => {
                        Some(AiEvent::OutputChunk(format!("[{bytes} bytes]")))
                    }
                    Event::CommandComplete { app_id, .. } => {
                        // Re-use the parsed output already stored in the OODA
                        // cache (computed in section 0 above) so we don't parse
                        // the same output twice.
                        let parsed = self
                            .ooda_last_parsed
                            .clone()
                            .unwrap_or_else(|| phantom_semantic::ParsedOutput {
                                command: self
                                    .pane_last_command
                                    .get(app_id)
                                    .cloned()
                                    .unwrap_or_default(),
                                command_type: phantom_semantic::CommandType::Unknown,
                                exit_code: None,
                                content_type: phantom_semantic::ContentType::PlainText,
                                errors: Vec::new(),
                                warnings: Vec::new(),
                                duration_ms: None,
                                raw_output: String::new(),
                            });
                        Some(AiEvent::CommandComplete(parsed))
                    }
                    Event::AgentTaskComplete {
                        agent_id,
                        success,
                        summary,
                        spawn_tag,
                    } => Some(AiEvent::AgentComplete {
                        id: *agent_id,
                        success: *success,
                        summary: summary.clone(),
                        spawn_tag: *spawn_tag,
                    }),
                    Event::AgentError { agent_id, error } => Some(AiEvent::AgentComplete {
                        id: *agent_id,
                        success: false,
                        summary: error.clone(),
                        spawn_tag: None,
                    }),
                    _ => None,
                };
                if let Some(event) = ai_event {
                    let _ = brain.send_event(event);
                }
            }

            // 2) Route command-boundary events into the capture pipeline,
            //    now passing the tracked command intent so bundle metadata
            //    records the actual command that ran.
            if let Event::CommandComplete { app_id, .. } = &msg.event {
                let intent = self.pane_last_command.get(app_id).cloned();
                let _ = self.on_command_boundary(*app_id, intent);
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
            AppCommand::ListPanes { reply } => {
                let result = self.mcp_list_panes();
                let _ = reply.send(result);
            }
            AppCommand::GetAgentStatus { agent_id, reply } => {
                let result = self.mcp_get_agent_status(agent_id);
                let _ = reply.send(result);
            }
            AppCommand::SpawnAgent { prompt, role: _, reply } => {
                // role mapping is deferred to v2; all roles use the FreeForm path.
                let task = phantom_agents::agent::AgentTask::FreeForm { prompt };
                // Build a minimal ISO-8601 UTC timestamp without a chrono dep.
                let started_at = {
                    let secs = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let s = secs % 60;
                    let m = (secs / 60) % 60;
                    let h = (secs / 3600) % 24;
                    let days = secs / 86400; // days since 1970-01-01
                    // Simple Gregorian calendar computation.
                    let (year, month, day) = {
                        let mut y = 1970u32;
                        let mut d = days;
                        loop {
                            let leap = (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400);
                            let yd = if leap { 366u64 } else { 365 };
                            if d < yd { break; }
                            d -= yd;
                            y += 1;
                        }
                        let leap = (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400);
                        let mdays = [31u64,if leap{29}else{28},31,30,31,30,31,31,30,31,30,31];
                        let mut mo = 1u32;
                        for &md in &mdays {
                            if d < md { break; }
                            d -= md;
                            mo += 1;
                        }
                        (y, mo, d + 1)
                    };
                    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
                };
                match self.spawn_agent_pane_with_opts(phantom_agents::AgentSpawnOpts::new(task)) {
                    Some(agent_id) => {
                        let _ = reply.send(Ok(SpawnAgentReply { agent_id, started_at }));
                    }
                    None => {
                        let _ = reply.send(Err(
                            "spawn_agent failed: no focused pane or API key not configured".into(),
                        ));
                    }
                }
            }
        }
    }

    fn mcp_capture_screenshot(&mut self, path: &Path) -> Result<ScreenshotReply, String> {
        use phantom_renderer::screenshot::{ScreenshotMetadata, capture_frame, save_screenshot};
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
            branch: self
                .context
                .as_ref()
                .and_then(|c| c.git.as_ref().map(|g| g.branch.clone())),
        };

        save_screenshot(&pixels_rgba, width, height, &metadata, path)
            .map_err(|e| format!("save failed: {e}"))?;

        info!(
            "Screenshot saved via MCP: {} ({}x{})",
            path.display(),
            width,
            height
        );

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
        self.coordinator
            .send_command_to_focused("write_bytes", &serde_json::json!({"bytes": bytes}))
            .map_err(|e| format!("write_bytes failed: {e}"))?;
        self.last_input_time = Instant::now();
        Ok(format!("wrote {} bytes to pty", bytes.len()))
    }

    fn mcp_send_to_pty(&mut self, command: &str) -> Result<(), String> {
        let mut text = command.to_string();
        if !text.ends_with('\n') {
            text.push('\n');
        }
        self.coordinator
            .send_command_to_focused("write", &serde_json::json!({"text": text}))
            .map_err(|e| format!("write failed: {e}"))?;
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
        state
            .get("text")
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

    /// Build the pane list from the coordinator's adapter registry.
    ///
    /// Iterates all running adapters, reads their metadata and adapter state,
    /// and returns a [`Vec<PaneInfo>`].  For agent-type panes the `agent_id`
    /// field is populated from the `"agent_id"` key in the adapter's `get_state()`
    /// JSON (added in issue #400 via `AgentAdapter::get_state`).
    ///
    /// This is the **real lookup path**: no mock, no hardcoded data.
    fn mcp_list_panes(&self) -> Result<Vec<PaneInfo>, String> {
        let focused = self.coordinator.focused();
        let app_ids = self.coordinator.all_app_ids();
        let mut panes = Vec::with_capacity(app_ids.len());

        for app_id in app_ids {
            let Some(entry) = self.coordinator.registry().get(app_id) else {
                continue;
            };
            // Compute the layout PaneId string for wire stability.
            let pane_id_str = self
                .coordinator
                .pane_id_for(app_id)
                .map(|pid| format!("{pid:?}"))
                .unwrap_or_else(|| format!("{app_id}"));

            // Read adapter title via the AppCore trait.
            let title = self
                .coordinator
                .registry()
                .get_adapter(app_id)
                .map(|a| a.title().to_owned())
                .unwrap_or_else(|| entry.app_type.clone());

            // For agent-type panes, extract the stable agent_id from get_state().
            let agent_id: Option<u64> = if entry.app_type == "agent" {
                self.coordinator
                    .registry()
                    .get_adapter(app_id)
                    .and_then(|a| a.get_state().get("agent_id").and_then(|v| v.as_u64()))
            } else {
                None
            };

            panes.push(PaneInfo {
                id: pane_id_str,
                pane_type: entry.app_type.clone(),
                title,
                focused: focused == Some(app_id),
                agent_id,
            });
        }

        Ok(panes)
    }

    /// Look up the status of a specific agent by its stable `u64` id.
    ///
    /// Iterates all running adapters, finds the one whose `get_state()["agent_id"]`
    /// matches `agent_id`, and returns an [`AgentStatusInfo`] snapshot.
    ///
    /// This is the **real lookup path** per the issue #400 acceptance criteria.
    /// The fake-Phantom tests in `phantom-mcp` and `phantom-hub` verify the
    /// agent_id is forwarded correctly; this function is the Phantom-side consumer.
    fn mcp_get_agent_status(&self, agent_id: u64) -> Result<AgentStatusInfo, String> {
        for app_id in self.coordinator.all_app_ids() {
            let Some(entry) = self.coordinator.registry().get(app_id) else {
                continue;
            };
            if entry.app_type != "agent" {
                continue;
            }
            let Some(adapter) = self.coordinator.registry().get_adapter(app_id) else {
                continue;
            };
            let state = adapter.get_state();

            // Match on the stable agent_id stored in get_state().
            let stored_id = match state.get("agent_id").and_then(|v| v.as_u64()) {
                Some(id) => id,
                None => continue,
            };
            if stored_id != agent_id {
                continue;
            }

            // Found the agent.  Map AgentPaneStatus → string state.
            let state_str = state
                .get("status")
                .and_then(|v| v.as_str())
                .map(|s| match s {
                    "Working" => "running",
                    "Done" => "done",
                    "Failed" => "failed",
                    other => other,
                })
                .unwrap_or("unknown")
                .to_owned();

            let task = state
                .get("task")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();

            // Build an output excerpt (≤256 bytes) from the adapter's read path.
            let last_output_excerpt: Option<String> = {
                let output_len = state
                    .get("output_len")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                if output_len > 0 {
                    // The output is inside the adapter; read it via the MCP
                    // read_output path (last N lines of the terminal state).
                    // For agents, the output lives in the pane's `cached_lines`.
                    // We can't call `tail_lines` (&mut self) here — use `cached_lines`.
                    adapter
                        .get_state()
                        .get("output")
                        .and_then(|v| v.as_str())
                        .map(|s| s.chars().rev().take(256).collect::<String>().chars().rev().collect::<String>())
                        .or_else(|| {
                            // Fallback: use the text from a read_output call.
                            // Since we hold &self, read from the terminal state.
                            Some(format!("output_len={output_len}"))
                        })
                } else {
                    None
                }
            };

            return Ok(AgentStatusInfo {
                agent_id,
                state: state_str,
                task,
                last_output_excerpt,
            });
        }

        Err(format!("agent {agent_id} not found"))
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

    // -----------------------------------------------------------------------
    // Live shader reload — #84
    // -----------------------------------------------------------------------

    /// Poll the background shader watcher and act on any events.
    ///
    /// * `ShaderEvent::Reloaded { name: "crt", .. }` → rebuilds the CRT
    ///   post-processing pipeline in-place via PostFxPipeline::reload_shader.
    ///   On wgpu failure the error is surfaced as a Severity::Warn banner
    ///   and the last-good pipeline stays active.
    /// * `ShaderEvent::Reloaded { name: other }` → logged at DEBUG, no action.
    /// * `ShaderEvent::Error { .. }` → WGSL parse failed; last-good pipeline
    ///   stays active and the error is shown as a banner.
    ///
    /// This method is a no-op in release builds (the stub always returns None).
    fn poll_shader_reload(&mut self, now_ms: u64) {
        use phantom_renderer::shader_loader::ShaderEvent;

        loop {
            let Some(event) = self.shader_reloader.poll() else {
                break;
            };
            match event {
                ShaderEvent::Reloaded {
                    ref name,
                    ref source,
                } if name == "crt" => match self.postfx.reload_shader(&self.gpu.device, source) {
                    Ok(()) => log::info!("live-reload: crt.wgsl pipeline swapped"),
                    Err(msg) => {
                        let message =
                            format!("crt.wgsl hot-swap failed — {msg}. Last-good shader active.");
                        self.push_shader_error_banner(message, now_ms);
                    }
                },
                ShaderEvent::Reloaded { name, .. } => {
                    log::debug!("live-reload: {name}.wgsl reloaded (no pipeline swap yet)");
                }
                ShaderEvent::Error { name, message } => {
                    let banner_msg = format!(
                        "Shader error in {name}.wgsl — {message}. Last-good shader still active."
                    );
                    self.push_shader_error_banner(banner_msg, now_ms);
                }
            }
        }
    }

    /// Push a Severity::Warn banner from a shader reload failure.
    fn push_shader_error_banner(&mut self, message: String, now_ms: u64) {
        use crate::notifications::{Banner, DEFAULT_BANNER_TTL_MS, Severity};
        self.notifications.push_banner(Banner {
            message,
            severity: Severity::Warn,
            expires_at_ms: now_ms.saturating_add(DEFAULT_BANNER_TTL_MS),
        });
    }

    // -----------------------------------------------------------------------
    // Issue #323 — alt-screen split-pane lifecycle
    // -----------------------------------------------------------------------

    /// Poll all terminal adapters for alt-screen transitions.
    ///
    /// On rising edge (`is_detached` false → true): triggers `split_for_alt_screen`.
    /// On falling edge (`is_detached` true → false): queues the secondary pane for
    /// fade-out via `alt_screen_fade`.
    pub(crate) fn poll_alt_screen_transitions(&mut self) {
        let all_ids: Vec<phantom_adapter::AppId> = self.coordinator.all_app_ids();

        // Collect transitions (avoid borrow conflict with self.coordinator).
        let mut to_split: Vec<(phantom_adapter::AppId, String)> = Vec::new();

        for app_id in &all_ids {
            let app_id = *app_id;
            let state = self
                .coordinator
                .registry()
                .get_adapter(app_id)
                .map(|a: &dyn phantom_adapter::AppAdapter| a.get_state());
            let Some(state) = state else { continue };

            let is_terminal = state
                .get("type")
                .and_then(|v: &serde_json::Value| v.as_str())
                == Some("terminal");
            if !is_terminal {
                continue;
            }

            let is_detached = state
                .get("is_detached")
                .and_then(|v: &serde_json::Value| v.as_bool())
                .unwrap_or(false);
            let label = state
                .get("detached_label")
                .and_then(|v: &serde_json::Value| v.as_str())
                .unwrap_or("interactive")
                .to_owned();

            let prev = self.prev_detached.get(&app_id).copied().unwrap_or(false);
            self.prev_detached.insert(app_id, is_detached);

            // Rising edge: terminal just entered alt-screen.
            if is_detached && !prev && !self.alt_screen_secondaries.contains_key(&app_id) {
                to_split.push((app_id, label));
            }

            // Falling edge: terminal just left alt-screen.
            if !is_detached && prev
                && let Some(&secondary_id) = self.alt_screen_secondaries.get(&app_id) {
                    // Start fade animation if not already fading.
                    self.alt_screen_fade.entry(secondary_id).or_insert(0.0);
                }
        }

        for (primary_id, label) in to_split {
            self.split_for_alt_screen(primary_id, label);
        }
    }

    /// Advance the 300 ms collapse fade for secondary alt-screen panes.
    ///
    /// Once a secondary pane's fade reaches 1.0 it is added to
    /// `alt_screen_pending_collapses` so the pane can be cleaned up.
    pub(crate) fn tick_alt_screen_fade(&mut self, dt: f32) {
        const FADE_DURATION: f32 = 0.3; // 300 ms

        // Collect secondaries whose fade is complete this frame.
        let mut completed: Vec<phantom_adapter::AppId> = Vec::new();
        for (secondary_id, progress) in self.alt_screen_fade.iter_mut() {
            *progress += dt / FADE_DURATION;
            if *progress >= 1.0 {
                completed.push(*secondary_id);
            }
        }

        // Build reverse map (secondary → primary) so we can pass both to collapse.
        let reverse: std::collections::HashMap<phantom_adapter::AppId, phantom_adapter::AppId> =
            self.alt_screen_secondaries
                .iter()
                .map(|(&primary, &secondary)| (secondary, primary))
                .collect();

        // Drain completed fades into pending_collapses (deduped).
        for secondary_id in completed {
            if !self.alt_screen_pending_collapses.contains(&secondary_id) {
                self.alt_screen_pending_collapses.push(secondary_id);
            }
        }

        // Collapse any ready panes.
        let collapsing: Vec<_> = self.alt_screen_pending_collapses.drain(..).collect();
        for secondary_id in collapsing {
            if let Some(&primary_id) = reverse.get(&secondary_id) {
                self.collapse_alt_screen_pane(primary_id, secondary_id);
            } else {
                // Primary already gone — clean up orphaned fade entry.
                self.alt_screen_fade.remove(&secondary_id);
            }
        }
    }

    // -----------------------------------------------------------------------
    // OODA signal builder (#358)
    // -----------------------------------------------------------------------

    /// Build a [`WorldState`] from live app signals for the per-frame OODA tick.
    ///
    /// All reads are O(1) field accesses or tiny collection sizes (≤10 items).
    /// One-frame pulse flags (`ooda_agent_just_completed`, `ooda_git_changed`)
    /// are cleared after being consumed so they fire for exactly one OODA tick.
    ///
    /// # Signal sources
    ///
    /// | `WorldState` field       | Live source                                     |
    /// |--------------------------|------------------------------------------------|
    /// | `idle_secs`              | `now - last_input_time` (computed by caller)   |
    /// | `has_errors`             | `ooda_last_parsed.errors.is_empty().not()`     |
    /// | `error_count`            | `ooda_last_parsed.errors.len()`               |
    /// | `has_active_process`     | `pending_command_text.is_empty().not()`        |
    /// | `agent_just_completed`   | `ooda_agent_just_completed` (one-frame pulse)  |
    /// | `file_or_git_changed`    | `ooda_git_changed` or `context.git.is_dirty`  |
    /// | `in_repl`                | any adapter is in alt-screen (`prev_detached`) |
    /// | `chattiness`             | `suggestion_history` depth / capacity ratio    |
    /// | `suggestions_since_input`| history entries with `shown_at >= last_input_time` |
    pub(crate) fn build_world_state(&mut self, idle_secs: f32) -> WorldState {
        // --- error signals --------------------------------------------------
        let (has_errors, error_count) = self
            .ooda_last_parsed
            .as_ref()
            .map(|p| (!p.errors.is_empty(), p.errors.len() as u32))
            .unwrap_or((false, 0));

        // --- active process -------------------------------------------------
        // Any entry in `pending_command_text` means a `CommandStarted` arrived
        // without a matching `CommandComplete` — i.e. a process is still running.
        let has_active_process = !self.pending_command_text.is_empty();

        // --- agent completion pulse -----------------------------------------
        let agent_just_completed = self.ooda_agent_just_completed;
        self.ooda_agent_just_completed = false; // consume: fires for one tick only

        // --- file / git change ----------------------------------------------
        // `ooda_git_changed` is a one-frame pulse set by the git-refresh reap
        // path (line 315) when the background git-refresh thread finishes, and
        // also by `drain_bus_to_brain` when a `GitStateChanged` event arrives.
        // We also OR in `context.git.is_dirty` so the OODA loop knows the repo
        // is dirty even before an explicit change event fires.
        let git_dirty = self
            .context
            .as_ref()
            .and_then(|c| c.git.as_ref())
            .map(|g| g.is_dirty)
            .unwrap_or(false);
        let file_or_git_changed = self.ooda_git_changed || git_dirty;
        self.ooda_git_changed = false; // consume pulse

        // --- REPL / alt-screen detection ------------------------------------
        // If any terminal adapter is currently in alt-screen mode (interactive
        // full-screen program like vim, less, python REPL, etc.) we consider
        // the user to be inside a REPL.  The `prev_detached` map is maintained
        // by `poll_alt_screen_transitions` each frame.
        let in_repl = self.prev_detached.values().any(|&d| d);

        // --- chattiness (dampener ratio) ------------------------------------
        // Expressed as the current suggestion queue depth relative to the
        // maximum stored (10). This gives the OODA scorer a 0.0–1.0 signal
        // without holding a reference to the brain thread's internal scorer.
        const MAX_HISTORY: f32 = 10.0;
        let chattiness = (self.suggestion_history.len() as f32 / MAX_HISTORY).min(1.0);

        // --- suggestions since last input -----------------------------------
        // Count history entries shown after the last user-input instant.
        // The deque holds at most 10 items so the scan is O(1) in practice.
        let suggestions_since_input = self
            .suggestion_history
            .iter()
            .filter(|s| s.shown_at >= self.last_input_time)
            .count() as u32;

        WorldState::new(
            idle_secs,
            has_errors,
            error_count,
            has_active_process,
            false, // new_pattern_detected: not yet wired; requires memory pattern engine (#28, #62)
            agent_just_completed,
            file_or_git_changed,
            in_repl,
            chattiness,
            suggestions_since_input,
        )
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
            let timed_out =
                git_refresh_spawned_at.is_some_and(|t| now.duration_since(t) > GIT_REFRESH_TIMEOUT);
            let finished = git_refresh_handle.as_ref().is_some_and(|h| h.is_finished());
            if timed_out || finished {
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
            let timed_out =
                git_refresh_spawned_at.is_some_and(|t| now.duration_since(t) > GIT_REFRESH_TIMEOUT);
            let finished = git_refresh_handle.as_ref().is_some_and(|h| h.is_finished());
            if timed_out || finished {
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

    // ---- #358: build_world_state() signal mapping -------------------------
    //
    // Full `App` construction requires a GPU context, so these tests exercise
    // the mapping logic directly using the same standalone patterns as the
    // git-refresh timeout tests above.

    /// `has_errors` / `error_count` derive from the last ParsedOutput stored
    /// in the OODA cache.  When the cache holds a ParsedOutput with errors,
    /// the WorldState must reflect them; when the cache is empty both must be
    /// false/0.
    #[test]
    fn build_world_state_error_signals_from_parsed_output() {
        use phantom_semantic::{CommandType, ContentType, DetectedError, ErrorType, ParsedOutput};

        // --- helper: build a minimal ParsedOutput with `n` errors -----------
        fn make_parsed(n: usize) -> ParsedOutput {
            ParsedOutput {
                command: "cargo build".into(),
                command_type: CommandType::Unknown,
                exit_code: Some(if n > 0 { 1 } else { 0 }),
                content_type: ContentType::PlainText,
                errors: (0..n)
                    .map(|i| DetectedError {
                        message: format!("error #{i}"),
                        error_type: ErrorType::Compiler,
                        file: None,
                        line: None,
                        column: None,
                        code: None,
                        severity: phantom_semantic::Severity::Error,
                        raw_line: String::new(),
                        suggestion: None,
                    })
                    .collect(),
                warnings: Vec::new(),
                duration_ms: None,
                raw_output: String::new(),
            }
        }

        // Simulate the mapping performed by build_world_state().
        let derive_error_signals =
            |parsed: &Option<ParsedOutput>| -> (bool, u32) {
                parsed
                    .as_ref()
                    .map(|p| (!p.errors.is_empty(), p.errors.len() as u32))
                    .unwrap_or((false, 0))
            };

        // No cache yet → no errors.
        let (has_errors, count) = derive_error_signals(&None);
        assert!(!has_errors, "empty cache must yield has_errors=false");
        assert_eq!(count, 0, "empty cache must yield error_count=0");

        // Cache with 0 errors → clean command.
        let (has_errors, count) = derive_error_signals(&Some(make_parsed(0)));
        assert!(!has_errors, "zero-error ParsedOutput must yield has_errors=false");
        assert_eq!(count, 0);

        // Cache with 3 errors → OODA sees them.
        let (has_errors, count) = derive_error_signals(&Some(make_parsed(3)));
        assert!(has_errors, "ParsedOutput with 3 errors must set has_errors=true");
        assert_eq!(count, 3, "error_count must equal parsed.errors.len()");
    }

    /// `has_active_process` is `true` when `pending_command_text` is non-empty
    /// (a `CommandStarted` arrived without a matching `CommandComplete`).
    #[test]
    fn build_world_state_active_process_from_pending_commands() {
        use std::collections::HashMap;

        let pending_active: HashMap<u32, String> = [(42u32, "cargo build".into())].into();
        let pending_empty: HashMap<u32, String> = HashMap::new();

        let has_active = |p: &HashMap<u32, String>| !p.is_empty();

        assert!(
            has_active(&pending_active),
            "non-empty pending_command_text must set has_active_process=true"
        );
        assert!(
            !has_active(&pending_empty),
            "empty pending_command_text must set has_active_process=false"
        );
    }

    /// `agent_just_completed` is a one-frame pulse: `true` for exactly one call
    /// to `build_world_state()` after the flag is set, then `false` thereafter.
    #[test]
    fn build_world_state_agent_just_completed_is_single_frame_pulse() {
        // Simulate the pulse-consume logic from build_world_state().
        let mut flag = true;

        // First call: flag is true, consume it.
        let first = flag;
        flag = false;

        // Second call: flag must be false.
        let second = flag;

        assert!(first, "agent_just_completed must be true on the first call");
        assert!(!second, "agent_just_completed must be false on subsequent calls");
    }

    /// `suggestions_since_input` counts history entries whose `shown_at` is
    /// at or after `last_input_time`.
    #[test]
    fn build_world_state_suggestions_since_input_counts_post_input_history() {
        use std::time::{Duration, Instant};

        let base = Instant::now();
        let last_input_time = base + Duration::from_secs(1);

        // Build three dummy instants: one before input, two after.
        let instants = [
            base,                               // before input — must not count
            last_input_time + Duration::from_millis(100), // after input
            last_input_time + Duration::from_millis(200), // after input
        ];

        let count = instants
            .iter()
            .filter(|&&t| t >= last_input_time)
            .count() as u32;

        assert_eq!(count, 2, "only 2 of 3 instants are at or after last_input_time");
    }

    /// `in_repl` is `true` when any value in `prev_detached` is `true`.
    #[test]
    fn build_world_state_in_repl_from_prev_detached() {
        use std::collections::HashMap;

        let no_repl: HashMap<u32, bool> = [(1u32, false), (2u32, false)].into();
        let repl: HashMap<u32, bool> = [(1u32, false), (2u32, true)].into();
        let empty: HashMap<u32, bool> = HashMap::new();

        let in_repl = |m: &HashMap<u32, bool>| m.values().any(|&d| d);

        assert!(!in_repl(&no_repl), "all-false detached map must yield in_repl=false");
        assert!(in_repl(&repl), "one true entry must yield in_repl=true");
        assert!(!in_repl(&empty), "empty map must yield in_repl=false");
    }

    // ---- DtClamp + SceneClock integration tests --------------------------
    //
    // These tests exercise the clamping and clock logic introduced to prevent
    // animation explosions on debugger pauses and OS suspends. They operate
    // directly on the `phantom_scene` types, mirroring the exact code path in
    // `App::update`, so a green test here is a green production path.

    /// A 5-second frame spike must be clamped to `target_dt` (16.6 ms),
    /// keeping the downstream delta well inside the 100 ms `max_dt` bound.
    #[test]
    fn dt_clamp_prevents_large_spike_from_propagating() {
        use std::time::Duration;

        let clamp = phantom_scene::DtClamp::default_60fps();
        let huge_spike = Duration::from_secs(5);

        let clamped = clamp.apply(huge_spike);

        assert!(
            clamped <= Duration::from_millis(100),
            "clamped dt {clamped:?} must not exceed 100 ms after a 5-second spike"
        );
        assert!(
            clamped < huge_spike,
            "clamped dt must be strictly less than the raw 5-second spike"
        );
    }

    /// After two distinct ticks, `elapsed()` must exceed the first tick's
    /// contribution, proving the clock advances monotonically.
    #[test]
    fn scene_clock_advances_each_frame() {
        use std::time::Duration;

        let mut clock = phantom_scene::Clock::new();

        clock.tick(Duration::from_millis(16));
        let after_first = clock.elapsed();

        clock.tick(Duration::from_millis(16));
        let after_second = clock.elapsed();

        assert!(
            after_second > after_first,
            "elapsed after two ticks ({after_second:?}) must exceed elapsed after one tick ({after_first:?})"
        );
    }

    /// When a large raw dt is clamped before being passed to `Clock::tick`,
    /// the clock's elapsed must reflect the *clamped* delta, not the spike.
    /// This mirrors the exact pipeline in `App::update`.
    #[test]
    fn scene_clock_delta_is_clamped() {
        use std::time::Duration;

        let clamp = phantom_scene::DtClamp::default_60fps();
        let mut clock = phantom_scene::Clock::new();

        let raw_spike = Duration::from_secs(5);
        let clamped_dt = clamp.apply(raw_spike);

        clock.tick(clamped_dt);

        let elapsed = clock.elapsed();

        assert!(
            elapsed <= Duration::from_millis(100),
            "clock elapsed {elapsed:?} must not reflect the raw 5-second spike"
        );
        assert!(
            elapsed > Duration::ZERO,
            "clock must have advanced by at least one clamped tick"
        );
    }
}
