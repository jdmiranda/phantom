//! Command execution and supervisor message handling for Phantom.
//!
//! Commands entered via the Quake console are parsed here. Output is pushed
//! back into the console scrollback so users can see results inline.

use log::{debug, info, warn};

use phantom_agents::AgentTask;
use phantom_protocol::{AppMessage, SupervisorCommand};
use phantom_nlp::NlpInterpreter;
use phantom_nlp::interpreter::ResolvedAction;
use phantom_ui::themes;

use crate::app::{App, AppState};
use crate::boot::BootSequence;
use crate::config::PhantomConfig;
use crate::console_eval::{self, EvalResult};

impl App {
    /// Send a command to a coordinator-managed adapter by app ID.
    ///
    /// Returns `Ok(response)` if the adapter accepted the command, or an error
    /// if the adapter doesn't exist, doesn't accept commands, or rejected it.
    /// This is the primary API for programmatic adapter control (MCP, AI brain,
    /// inter-adapter communication).
    #[allow(dead_code)] // Called by MCP/brain integration in WU-5H
    pub(crate) fn send_adapter_command(
        &mut self,
        app_id: phantom_adapter::AppId,
        cmd: &str,
        args: &serde_json::Value,
    ) -> anyhow::Result<String> {
        self.coordinator.send_command(app_id, cmd, args)
    }
}

impl App {
    /// Parse and execute a user command string entered via the console.
    pub(crate) fn execute_user_command(&mut self, input: &str) {
        // Fast path: try the console evaluator first (builtin commands).
        match console_eval::evaluate(input) {
            EvalResult::Ok(Some(msg)) => {
                if msg == "__quit__" {
                    info!("Quit requested via console");
                    self.quit_requested = true;
                } else {
                    self.console.output(msg);
                }
                return;
            }
            EvalResult::Ok(None) => return, // empty input
            EvalResult::Err(err) => {
                self.console.error(err);
                return;
            }
            EvalResult::Pending(routing_msg) => {
                // Fall through to legacy command handling below.
                // Only route to brain as Interrupt if no command matches.
                debug!("{routing_msg}");
            }
            EvalResult::Unknown { input: _, suggestions } => {
                if !suggestions.is_empty() {
                    self.console.output(format!("Did you mean: {}?", suggestions.join(", ")));
                }
                // Fall through to legacy handling.
            }
        }

        let parts: Vec<&str> = input.trim().splitn(3, ' ').collect();
        if parts.is_empty() {
            return;
        }

        match parts[0] {
            "set" => {
                if parts.len() >= 3 {
                    let key = parts[1].to_string();
                    let value = parts[2].to_string();
                    self.apply_set(&key, &value);
                    self.console.output(format!("set {key} = {value}"));
                    if let Some(ref mut sv) = self.supervisor {
                        sv.send(&AppMessage::Log(format!("set {key}={value}")));
                    }
                } else {
                    self.console.error("Usage: set <key> <value>");
                }
            }
            "theme" => {
                if parts.len() >= 2 {
                    let name = parts[1];
                    if themes::builtin_by_name(name).is_some() {
                        self.apply_theme(name);
                        self.console.output(format!("Theme switched to: {name}"));
                        if let Some(ref mut sv) = self.supervisor {
                            sv.send(&AppMessage::Log(format!("theme {name}")));
                        }
                    } else {
                        self.console.error(format!("Unknown theme: {name}"));
                    }
                } else {
                    self.console.error("Usage: theme <name>");
                }
            }
            "reload" => {
                self.apply_reload();
                self.console.system("Config reloaded from disk");
            }
            "quit" | "exit" => {
                self.console.system("Shutting down...");
                self.quit_requested = true;
            }
            "boot" => {
                self.console.system("Replaying boot sequence");
                self.console.open = false;
                let w = self.gpu.surface_config.width;
                let h = self.gpu.surface_config.height;
                let bc = (w as f32 / self.cell_size.0).floor().max(40.0) as usize;
                let br = (h as f32 / self.cell_size.1).floor().max(10.0) as usize;
                self.boot = BootSequence::with_size(bc, br);
                self.state = AppState::Boot;
            }
            "debug" => {
                self.debug_hud = !self.debug_hud;
                let state = if self.debug_hud { "ON" } else { "OFF" };
                self.console.output(format!("Debug HUD: {state}"));
            }
            "plain" => {
                self.theme.shader_params.scanline_intensity = 0.0;
                self.theme.shader_params.bloom_intensity = 0.0;
                self.theme.shader_params.chromatic_aberration = 0.0;
                self.theme.shader_params.curvature = 0.0;
                self.theme.shader_params.vignette_intensity = 0.0;
                self.theme.shader_params.noise_intensity = 0.0;
                self.console.output("All CRT effects disabled");
            }
            cmd if cmd == "agent" || cmd.starts_with("agent ") => {
                let prompt = if cmd == "agent" {
                    // No prompt — open interactive agent pane.
                    "You are an interactive AI assistant in the Phantom terminal. \
                     The user opened an agent pane to chat with you. Help them with \
                     whatever they need. You have tools to read files, edit code, \
                     run commands, and search the project.".to_string()
                } else {
                    input[6..].trim().to_string()
                };
                if self.spawn_agent_pane(AgentTask::FreeForm { prompt: prompt.clone() }) {
                    self.console.system("Agent pane opened.");
                    self.console.open = false; // Close console so the pane is visible.
                } else {
                    self.console.error("Cannot spawn agent: ANTHROPIC_API_KEY not set");
                    self.console.error("Set it with: export ANTHROPIC_API_KEY=sk-...");
                }
            }
            "sysmon" | "monitor" | "stats" => {
                self.sysmon_visible = !self.sysmon_visible;
                self.sysmon.set_active(self.sysmon_visible);
                let state = if self.sysmon_visible { "ON" } else { "OFF" };
                self.console.output(format!("System monitor: {state}"));
            }
            "appmon" | "perf" => {
                self.appmon_visible = !self.appmon_visible;
                let state = if self.appmon_visible { "ON" } else { "OFF" };
                self.console.output(format!("App monitor: {state}"));
            }
            "plugins" | "plugin" => {
                let list = self.plugin_registry.list();
                if list.is_empty() {
                    self.console.output("No plugins loaded");
                } else {
                    for p in &list {
                        let status = if p.enabled { "on" } else { "off" };
                        self.console.output(format!(
                            "{} v{} [{status}] — {}", p.name, p.version, p.description
                        ));
                    }
                }
            }
            "video" => {
                let path_str = if parts.len() >= 2 {
                    input.trim().strip_prefix("video").unwrap().trim().to_string()
                } else {
                    // No path given — open native macOS file picker.
                    self.console.system("Opening file picker...");
                    self.console.open = false; // hide console so dialog is visible
                    match crate::video::pick_video_file() {
                        Some(p) => p,
                        None => {
                            self.console.open = true;
                            self.console.system("Cancelled");
                            return;
                        }
                    }
                };
                let path = std::path::Path::new(&path_str);
                let w = self.gpu.surface_config.width;
                let h = self.gpu.surface_config.height;
                if let Some(playback) = crate::video::VideoPlayback::start(path, w, h) {
                    self.console.system(format!(
                        "Playing: {} ({}x{} @ {}fps)",
                        path.file_name().unwrap_or_default().to_string_lossy(),
                        playback.width, playback.height, playback.fps as u32,
                    ));
                    self.video_playback = Some(playback);
                } else {
                    self.console.error("Failed to start video. Is ffmpeg installed?");
                }
            }
            cmd if cmd.starts_with("goal ") => {
                let objective = cmd.strip_prefix("goal ").unwrap().trim().to_string();
                if objective.is_empty() {
                    self.console.error("Usage: goal <objective>");
                } else {
                    self.console.system(format!("GOAL: {objective}"));
                    self.console.output("Spawning autonomous agent...");
                    // Spawn an agent directly — the agent has tools, context,
                    // and the codebase map. Don't route through the brain's
                    // chat client.
                    self.pending_brain_actions.push(
                        phantom_brain::events::AiAction::SpawnAgent {
                            task: phantom_agents::AgentTask::FreeForm { prompt: objective },
                            spawn_tag: None,
                        }
                    );
                }
            }
            "goals" => {
                self.console.system("Queued goals for the brain:");
                self.console.output("Paste these one at a time to set autonomous work:");
                self.console.output("");
                self.console.output("  goal wire proactive.rs into the brain OODA loop — replace the hardcoded quiet_threshold with ProactiveBrain.should_act()");
                self.console.output("  goal wire curves.rs into scoring — replace hardcoded fix_score 0.9 and explain_score 0.7 with configurable ResponseCurve evaluations");
                self.console.output("  goal wire skill_store.rs into agent prompts — when spawning an agent, query SkillStore for relevant skills and inject into the system prompt");
                self.console.output("  goal wire curriculum.rs into the brain idle handler — when UserIdle > 60s and no goal active, call CurriculumGenerator to propose a task");
                self.console.output("  goal wire orchestrator.rs TaskLedger into GoalPursuit — when a goal has multiple tasks, use the ledger to track progress and re-plan on failure");
                self.console.output("  goal add CLAUDE.md to the project root with build instructions, architecture overview, and deny(warnings) requirement");
            }
            "suggestions" => {
                if self.suggestion_history.is_empty() {
                    self.console.output("No suggestion history.");
                } else {
                    self.console.output(format!("{} recent suggestions:", self.suggestion_history.len()));
                    for (i, s) in self.suggestion_history.iter().enumerate() {
                        let age = std::time::Instant::now().duration_since(s.shown_at).as_secs();
                        self.console.output(format!("  {}. [{}s ago] {}", i + 1, age, s.text));
                    }
                }
            }
            "selftest" => {
                self.console.system("SELFTEST: brain exercising its own features...");
                self.selftest = Some(crate::selftest::SelfTestRunner::new(false));
            }
            "selfheal" => {
                self.console.system("SELFHEAL: test → diagnose → fix → verify → commit → push");
                self.selftest = Some(crate::selftest::SelfTestRunner::new(true));
            }
            "clear" => {
                self.console.history.clear();
                self.console.scroll_offset = 0;
            }
            "help" => {
                self.console.system("Available commands:");
                self.console.output("  set <key> <value>   Tune shader params (curvature, scanlines, bloom, aberration, vignette, noise)");
                self.console.output("  theme <name>        Switch theme");
                self.console.output("  agent <prompt>      Spawn AI agent");
                self.console.output("  sysmon              Toggle system monitor");
                self.console.output("  appmon              Toggle app diagnostics");
                self.console.output("  plugins             List plugins");
                self.console.output("  plain               Disable all CRT effects");
                self.console.output("  debug               Toggle shader debug HUD");
                self.console.output("  reload              Reload config from disk");
                self.console.output("  boot                Replay boot sequence");
                self.console.output("  video <path>        Play video through CRT shader");
                self.console.output("  suggestions         List dismissed/expired suggestion history");
                self.console.output("  selftest            Brain exercises its own features");
                self.console.output("  selfheal            selftest + auto-fix + commit + push");
                self.console.output("  clear               Clear console history");
                self.console.output("  quit                Exit Phantom");
            }
            other => {
                // NLP fallback: try interpreting as natural language.
                if let Some(ref ctx) = self.context {
                    match NlpInterpreter::interpret(input, ctx) {
                        ResolvedAction::RunCommand(cmd) => {
                            self.console.system(format!("Running: {cmd}"));
                            let cmd_text = format!("{cmd}\n");
                            let _ = self.coordinator.send_command_to_focused(
                                "write",
                                &serde_json::json!({"text": cmd_text}),
                            );
                        }
                        ResolvedAction::SpawnAgent(desc) => {
                            self.console.system(format!("Spawning agent: {desc}"));
                            self.pending_brain_actions.push(
                                phantom_brain::events::AiAction::SpawnAgent {
                                    task: phantom_agents::AgentTask::FreeForm { prompt: desc },
                                    spawn_tag: None,
                                }
                            );
                        }
                        ResolvedAction::ShowInfo(info_text) => {
                            self.console.output(info_text);
                        }
                        ResolvedAction::Ambiguous { input: _, options } => {
                            self.console.output(format!("Did you mean: {}", options.join(", ")));
                        }
                        ResolvedAction::PassThrough => {
                            // No command matched, no NLP match — spawn an agent.
                            // The agent has tools, context, and the codebase map.
                            // Don't use the brain's dumb chat client.
                            self.console.system(format!("Spawning agent: {other}"));
                            self.pending_brain_actions.push(
                                phantom_brain::events::AiAction::SpawnAgent {
                                    task: phantom_agents::AgentTask::FreeForm {
                                        prompt: input.trim().to_string(),
                                    },
                                    spawn_tag: None,
                                }
                            );
                        }
                    }
                } else {
                    // No context — spawn agent directly.
                    self.console.system(format!("Spawning agent: {other}"));
                    self.pending_brain_actions.push(
                        phantom_brain::events::AiAction::SpawnAgent {
                            task: phantom_agents::AgentTask::FreeForm {
                                prompt: input.trim().to_string(),
                            },
                            spawn_tag: None,
                        }
                    );
                }
            }
        }
    }

    /// Handle a command received from the supervisor process.
    pub(crate) fn handle_supervisor_command(&mut self, cmd: SupervisorCommand) {
        debug!("Supervisor command: {cmd:?}");
        match cmd {
            SupervisorCommand::Set { key, value } => {
                self.apply_set(&key, &value);
            }
            SupervisorCommand::Theme(name) => {
                self.apply_theme(&name);
            }
            SupervisorCommand::Reload => {
                self.apply_reload();
            }
            SupervisorCommand::Shutdown => {
                info!("Shutdown requested by supervisor");
                self.quit_requested = true;
            }
            SupervisorCommand::Ping => {
                if let Some(ref mut sv) = self.supervisor {
                    sv.send(&AppMessage::Pong);
                }
            }
        }
    }

    /// Live-update a shader parameter by key/value.
    pub(crate) fn apply_set(&mut self, key: &str, value: &str) {
        if let Ok(v) = value.parse::<f32>() {
            match key {
                "curvature" => self.theme.shader_params.curvature = v,
                "scanlines" | "scanline_intensity" => {
                    self.theme.shader_params.scanline_intensity = v;
                }
                "bloom" | "bloom_intensity" => {
                    self.theme.shader_params.bloom_intensity = v;
                }
                "aberration" | "chromatic_aberration" => {
                    self.theme.shader_params.chromatic_aberration = v;
                }
                "vignette" | "vignette_intensity" => {
                    self.theme.shader_params.vignette_intensity = v;
                }
                "noise" | "noise_intensity" => {
                    self.theme.shader_params.noise_intensity = v;
                }
                "font_size" => {
                    debug!("font_size change requires renderer recreation (not yet implemented)");
                    self.console.error("font_size requires restart (not yet hot-swappable)");
                }
                _ => {
                    self.console.error(format!("Unknown config key: {key}"));
                }
            }
        } else {
            self.console.error(format!("Invalid value for {key}: {value} (expected number)"));
        }
    }

    /// Hot-swap the active theme by name.
    pub(crate) fn apply_theme(&mut self, name: &str) {
        if let Some(new_theme) = themes::builtin_by_name(name) {
            info!("Theme switched to: {name}");
            self.theme = new_theme;
        } else {
            warn!("Unknown theme: {name}");
        }
    }

    /// Re-read the config file from disk and apply it.
    pub(crate) fn apply_reload(&mut self) {
        info!("Reloading config from disk");
        let config = PhantomConfig::load();
        self.theme = config.resolve_theme();
    }
}
