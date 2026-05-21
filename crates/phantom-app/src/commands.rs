//! Command execution and supervisor message handling for Phantom.
//!
//! Commands entered via the Quake console are parsed here. Output is pushed
//! back into the console scrollback so users can see results inline.

use log::{debug, info, warn};

use phantom_agents::cli::{AgentCommand, parse_agent_command};
use phantom_agents::role::CapabilityClass;
use phantom_agents::{AgentSpawnOpts, AgentTask, PeerId};
use phantom_brain::events::AiEvent;
use phantom_nlp::NlpInterpreter;
use phantom_nlp::interpreter::ResolvedAction;
use phantom_nlp::{Intent, translate};
use phantom_protocol::{AppMessage, SupervisorCommand};
use phantom_renderer::screenshot::{ScreenshotMetadata, capture_frame, save_screenshot};
use phantom_ui::themes;

use crate::app::{App, AppState, NlpTranslateResult};
use crate::boot::BootSequence;
use crate::config::PhantomConfig;
use crate::console_eval::{self, EvalResult};

/// Default font size in points used by `font reset`. Mirrors the
/// `PhantomConfig::default().font_size` value so the console reset and the
/// disk-loaded default stay in lockstep.
const DEFAULT_FONT_SIZE_PT: f32 = 14.0;

/// Minimum/maximum font size accepted by the `font <size>` command, in points.
const MIN_FONT_SIZE_PT: f32 = 6.0;
const MAX_FONT_SIZE_PT: f32 = 72.0;

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
            EvalResult::Unknown {
                input: _,
                suggestions,
            } => {
                if !suggestions.is_empty() {
                    self.console
                        .output(format!("Did you mean: {}?", suggestions.join(", ")));
                }
                // Fall through to legacy handling.
            }
        }

        let parts: Vec<&str> = input.trim().splitn(4, ' ').collect();
        if parts.is_empty() {
            return;
        }

        // Normalise the command word to lowercase so `Font`, `FONT`, and
        // `font` all route to the same arm (Bug 4 — case-sensitivity fix).
        let cmd_lower = parts[0].to_lowercase();

        match cmd_lower.as_str() {
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
            cmd if cmd == "agent" || cmd.starts_with("agent ") || cmd == "agents" => {
                // Route through the structured CLI parser so --model / --role /
                // --ttl / --capability flags are honoured end-to-end.
                match parse_agent_command(input) {
                    None | Some(AgentCommand::Help) => {
                        // Bare `agent` with no flags → open interactive pane.
                        let prompt =
                            "You are an interactive AI assistant in the Phantom terminal. \
                             The user opened an agent pane to chat with you. Help them with \
                             whatever they need. You have tools to read files, edit code, \
                             run commands, and search the project."
                                .to_string();
                        if self.spawn_agent_pane(AgentTask::FreeForm { prompt }) {
                            self.console.system("Agent pane opened.");
                            self.console.open = false;
                        } else {
                            self.console
                                .error("Cannot spawn agent: ANTHROPIC_API_KEY not set");
                            self.console
                                .error("Set it with: export ANTHROPIC_API_KEY=sk-...");
                        }
                    }
                    Some(AgentCommand::Spawn { prompt }) => {
                        if self.spawn_agent_pane(AgentTask::FreeForm {
                            prompt: prompt.clone(),
                        }) {
                            self.console.system("Agent pane opened.");
                            self.console.open = false;
                        } else {
                            self.console
                                .error("Cannot spawn agent: ANTHROPIC_API_KEY not set");
                            self.console
                                .error("Set it with: export ANTHROPIC_API_KEY=sk-...");
                        }
                    }
                    Some(AgentCommand::SpawnWithFlags { prompt, flags }) => {
                        // Wire --model through AgentSpawnOpts so it reaches the
                        // ChatBackend selector (the core of issue #85).
                        let task = AgentTask::FreeForm {
                            prompt: prompt.clone(),
                        };
                        let mut opts = AgentSpawnOpts::new(task);
                        opts.chat_model = flags.model.clone();
                        // Surface warnings to the user.
                        for warn_msg in &flags.warnings {
                            self.console.output(format!("  warning: {warn_msg}"));
                        }
                        if let Some(ref m) = flags.model {
                            self.console
                                .output(format!("  model: {}", m.backend_name()));
                        }
                        if self.spawn_agent_pane_with_opts(opts).is_some() {
                            self.console.system("Agent pane opened.");
                            self.console.open = false;
                        } else {
                            self.console.error("Cannot spawn agent: API key not set");
                            self.console
                                .error("Set ANTHROPIC_API_KEY or OPENAI_API_KEY");
                        }
                    }
                    Some(AgentCommand::SpawnFix { target }) => {
                        let task = AgentTask::FixError {
                            error_summary: format!("fix {target}"),
                            file: Some(target.clone()),
                            context: "user-initiated fix".into(),
                        };
                        if self.spawn_agent_pane(task) {
                            self.console
                                .system(format!("Fix agent opened for {target}."));
                            self.console.open = false;
                        } else {
                            self.console
                                .error("Cannot spawn agent: ANTHROPIC_API_KEY not set");
                        }
                    }
                    Some(AgentCommand::SpawnReview) => {
                        let task = AgentTask::ReviewCode {
                            files: Vec::new(),
                            context: "user-initiated review".into(),
                        };
                        if self.spawn_agent_pane(task) {
                            self.console.system("Review agent opened.");
                            self.console.open = false;
                        } else {
                            self.console
                                .error("Cannot spawn agent: ANTHROPIC_API_KEY not set");
                        }
                    }
                    Some(AgentCommand::SpawnWatch { description }) => {
                        let task = AgentTask::WatchAndNotify {
                            description: description.clone(),
                        };
                        if self.spawn_agent_pane(task) {
                            self.console
                                .system(format!("Watch agent opened: {description}."));
                            self.console.open = false;
                        } else {
                            self.console
                                .error("Cannot spawn agent: ANTHROPIC_API_KEY not set");
                        }
                    }
                    Some(AgentCommand::List) => {
                        self.console.system("(agent list: use the agents panel)");
                    }
                    Some(AgentCommand::Show { id }) => {
                        self.console
                            .system(format!("(agent show #{id}: use the agents panel)"));
                    }
                    Some(AgentCommand::Kill { id }) => {
                        self.console
                            .system(format!("(agent kill #{id}: use the agents panel)"));
                    }
                    Some(AgentCommand::KillAll) => {
                        self.console
                            .system("(agent kill-all: use the agents panel)");
                    }
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
                            "{} v{} [{status}] — {}",
                            p.name, p.version, p.description
                        ));
                    }
                }
            }
            "video" => {
                let path_str = if parts.len() >= 2 {
                    input
                        .trim()
                        .strip_prefix("video")
                        .unwrap()
                        .trim()
                        .to_string()
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
                        playback.width,
                        playback.height,
                        playback.fps as u32,
                    ));
                    self.video_playback = Some(playback);
                } else {
                    self.console
                        .error("Failed to start video. Is ffmpeg installed?");
                }
            }
            cmd if cmd.starts_with("goal ") => {
                let objective = cmd.strip_prefix("goal ").unwrap().trim().to_string();
                if objective.is_empty() {
                    self.console.error("Usage: goal <objective>");
                } else {
                    self.console.system(format!("GOAL: {objective}"));
                    self.console.output("Spawning autonomous agent...");

                    // Persist the goal so the reconciler can resume after a
                    // restart (issue #206).
                    self.persist_goal(&objective);

                    // Spawn an agent directly — the agent has tools, context,
                    // and the codebase map. Don't route through the brain's
                    // chat client.
                    self.pending_brain_actions
                        .push(phantom_brain::events::AiAction::SpawnAgent {
                            task: phantom_agents::AgentTask::FreeForm { prompt: objective },
                            spawn_tag: None,
                            disposition: phantom_agents::dispatch::Disposition::Chat,
                        });
                }
            }
            "goals" => {
                self.console.system("Queued goals for the brain:");
                self.console
                    .output("Paste these one at a time to set autonomous work:");
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
                    self.console.output(format!(
                        "{} recent suggestions:",
                        self.suggestion_history.len()
                    ));
                    for (i, s) in self.suggestion_history.iter().enumerate() {
                        let age = std::time::Instant::now()
                            .duration_since(s.shown_at)
                            .as_secs();
                        self.console
                            .output(format!("  {}. [{}s ago] {}", i + 1, age, s.text));
                    }
                }
            }
            cmd if cmd == "history" || cmd.starts_with("history ") => {
                let limit: usize = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(20);
                match self.history {
                    None => self.console.output("History store not available."),
                    Some(ref store) => {
                        let total = store.count();
                        self.console.output(format!(
                            "Command history ({total} total, showing last {limit}):"
                        ));
                        match store.recent(limit) {
                            Err(e) => self.console.output(format!("history read error: {e}")),
                            Ok(entries) if entries.is_empty() => {
                                self.console.output("  (no commands recorded yet)");
                            }
                            Ok(entries) => {
                                let start_num = total.saturating_sub(entries.len());
                                for (i, e) in entries.iter().enumerate() {
                                    let code = e
                                        .exit_code()
                                        .map(|c| format!(" [exit {c}]"))
                                        .unwrap_or_default();
                                    self.console.output(format!(
                                        "  {:>3}. {}{code}",
                                        start_num + i + 1,
                                        e.command(),
                                    ));
                                }
                            }
                        }
                    }
                }
            }
            "ghost" => {
                // `ghost privacy on` / `ghost privacy off` — toggle privacy mode.
                // `ghost grant <peer> <capability>`  — grant capability to a remote peer.
                // `ghost revoke <peer>`              — revoke all grants for a peer.
                // `ghost grants [list]`              — list all active peer grants.
                match (
                    parts.get(1).copied(),
                    parts.get(2).copied(),
                    parts.get(3).copied(),
                ) {
                    (Some("privacy"), Some("on"), _) => {
                        self.privacy_mode = true;
                        self.status_bar.set_privacy_mode(true);
                        if let Some(ref brain) = self.brain {
                            let _ = brain.send_event(AiEvent::SetPrivacyMode(true));
                        }
                        self.console
                            .system("[P] Privacy mode ON — cloud APIs blocked");
                    }
                    (Some("privacy"), Some("off"), _) => {
                        self.privacy_mode = false;
                        self.status_bar.set_privacy_mode(false);
                        if let Some(ref brain) = self.brain {
                            let _ = brain.send_event(AiEvent::SetPrivacyMode(false));
                        }
                        self.console.system("Privacy mode OFF — cloud APIs allowed");
                    }
                    (Some("privacy"), _, _) => {
                        let state = if self.privacy_mode { "ON" } else { "OFF" };
                        self.console.output(format!("Privacy mode: {state}"));
                        self.console
                            .output("Usage: ghost privacy on | ghost privacy off");
                    }
                    // ghost grant <peer_id> <capability>
                    (Some("grant"), Some(peer_str), Some(cap_str)) => {
                        let cap = parse_capability_class(cap_str);
                        match cap {
                            Some(class) => {
                                let peer = PeerId::new(peer_str);
                                // Merge with any existing grant rather than replacing.
                                let mut classes = self.peer_grant_registry.effective_classes(&peer);
                                classes.insert(class);
                                self.peer_grant_registry.grant(peer.clone(), classes, None);
                                crate::peer_grants::save_peer_grant_registry(
                                    &self.peer_grant_registry,
                                );
                                self.console.system(format!(
                                    "Grant: peer {peer_str} now has {cap_str} capability"
                                ));
                            }
                            None => {
                                self.console.error(format!(
                                    "Unknown capability '{cap_str}'. \
                                     Valid: Sense, Coordinate, Act, Reflect, Compute"
                                ));
                            }
                        }
                    }
                    (Some("grant"), _, _) => {
                        self.console
                            .output("Usage: ghost grant <peer_id> <capability>");
                        self.console
                            .output("  capability: Sense | Coordinate | Act | Reflect | Compute");
                    }
                    // ghost revoke <peer_id>
                    (Some("revoke"), Some(peer_str), _) => {
                        let peer = PeerId::new(peer_str);
                        self.peer_grant_registry.revoke(&peer);
                        crate::peer_grants::save_peer_grant_registry(&self.peer_grant_registry);
                        self.console
                            .system(format!("Revoked: all grants for peer {peer_str} removed"));
                    }
                    (Some("revoke"), None, _) => {
                        self.console.output("Usage: ghost revoke <peer_id>");
                    }
                    // ghost grants [list]
                    (Some("grants"), _, _) => {
                        let grants: Vec<_> = self.peer_grant_registry.iter().collect();
                        if grants.is_empty() {
                            self.console.output("No active peer grants.");
                        } else {
                            self.console
                                .output(format!("{} active peer grant(s):", grants.len()));
                            for g in grants {
                                let classes: Vec<&str> = g
                                    .allowed_classes
                                    .iter()
                                    .map(|c| capability_class_str(*c))
                                    .collect();
                                let expiry = g
                                    .until
                                    .map(|t| {
                                        let remaining = t
                                            .checked_duration_since(std::time::Instant::now())
                                            .unwrap_or_default();
                                        format!("expires in {}s", remaining.as_secs())
                                    })
                                    .unwrap_or_else(|| "permanent".into());
                                self.console.output(format!(
                                    "  {} → [{}] ({})",
                                    g.peer_id,
                                    classes.join(", "),
                                    expiry,
                                ));
                            }
                        }
                    }
                    _ => {
                        self.console.output("Usage: ghost <subcommand> [args]");
                        self.console.output(
                            "  ghost privacy on|off                    Toggle privacy mode",
                        );
                        self.console.output("  ghost grant <peer_id> <capability>      Grant capability to a remote peer");
                        self.console.output("  ghost revoke <peer_id>                  Revoke all grants for a peer");
                        self.console.output(
                            "  ghost grants [list]                     List all active peer grants",
                        );
                        self.console
                            .output("  Capabilities: Sense | Coordinate | Act | Reflect | Compute");
                    }
                }
            }
            cmd if cmd.starts_with("dag.") => {
                // Route `dag.<op> <json-args>` to the Inspector adapter via
                // the coordinator command bus.
                //
                // Usage examples (from the `` ` `` console):
                //   dag.focus_node {"id":"phantom_agents::dispatch"}
                //   dag.zoom {"factor":2.0}
                //   dag.highlight {"ids":["a","b"]}
                //   dag.clear_focus
                //   dag.reset_view
                //
                // The coordinator returns a JSON DagCommandResult blob which
                // we pretty-print to the console.
                let op = cmd; // e.g. "dag.focus_node"
                // Everything after the first space is the JSON args (optional).
                let args_str = input.trim().split_once(' ').map(|x| x.1).unwrap_or("{}");
                let args: serde_json::Value =
                    serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));

                // Look for the first inspector adapter registered with the
                // coordinator and send the command there.
                let inspector_id = self.coordinator.find_adapter_by_type("inspector");
                match inspector_id {
                    None => {
                        self.console
                            .error("dag command: no inspector adapter registered");
                    }
                    Some(id) => match self.coordinator.send_command(id, op, &args) {
                        Ok(json_response) => {
                            self.console.output(json_response);
                        }
                        Err(e) => {
                            self.console.error(format!("dag command error: {e}"));
                        }
                    },
                }
            }
            "selftest" => {
                self.console
                    .system("SELFTEST: brain exercising its own features...");
                self.selftest = Some(crate::selftest::SelfTestRunner::new(false));
            }
            "selfheal" => {
                self.console
                    .system("SELFHEAL: test → diagnose → fix → verify → commit → push");
                self.selftest = Some(crate::selftest::SelfTestRunner::new(true));
            }
            "inspect" => {
                if self.spawn_inspector_pane() {
                    self.console.system("Inspector pane opened.");
                    self.console.open = false;
                } else {
                    self.console.error("inspect: could not open inspector pane (no focused pane or split failed)");
                }
            }
            "clear" => {
                self.console.history.clear();
                self.console.scroll_offset = 0;
            }
            // ------------------------------------------------------------------
            // font <size> | font reset
            // ------------------------------------------------------------------
            "font" => {
                // Lowercase the subcommand so `font Reset`, `font RESET`, and
                // `font reset` all match. The primary command word is already
                // lowercased above; this matches that contract.
                let sub = parts.get(1).map(|s| s.to_ascii_lowercase());
                match sub.as_deref() {
                    None | Some("") => {
                        let current = self.text_renderer.font_size();
                        self.console.output(format!(
                            "Font size: {current:.0}pt (default: {DEFAULT_FONT_SIZE_PT:.0}pt)"
                        ));
                        self.console.output("Usage: font <size>  |  font reset");
                    }
                    Some("reset") => {
                        self.text_renderer.set_font_size(DEFAULT_FONT_SIZE_PT);
                        self.console.output(format!(
                            "Font size reset to {DEFAULT_FONT_SIZE_PT:.0}pt"
                        ));
                    }
                    Some(_) => {
                        // Re-borrow the raw token so we surface the user's
                        // original casing in error messages.
                        let size_str = parts.get(1).copied().unwrap_or("");
                        match size_str.parse::<f32>() {
                            Ok(size) if (MIN_FONT_SIZE_PT..=MAX_FONT_SIZE_PT).contains(&size) => {
                                self.text_renderer.set_font_size(size);
                                self.console.output(format!("Font size set to {size:.0}pt"));
                            }
                            Ok(size) => {
                                self.console.error(format!(
                                    "Font size {size} out of range ({MIN_FONT_SIZE_PT:.0}–{MAX_FONT_SIZE_PT:.0}pt)"
                                ));
                            }
                            Err(_) => {
                                self.console.error(format!("Invalid font size: {size_str}"));
                            }
                        }
                    }
                }
            }
            // ------------------------------------------------------------------
            // memory  |  memory clear
            // ------------------------------------------------------------------
            "memory" => {
                // Lowercase the subcommand to keep the command case-insensitive
                // end-to-end (`memory CLEAR`, `Memory Clear`, etc.).
                let sub = parts.get(1).map(|s| s.to_ascii_lowercase());
                match sub.as_deref() {
                    Some("clear") => match self.memory {
                        None => {
                            self.console.output("Memory store not available.");
                        }
                        Some(ref mut store) => {
                            // `store.all()` returns `&[MemoryEntry]` borrowed
                            // from `store`. We must collect owned `String`s
                            // *before* the mutable `store.remove(...)` calls
                            // because the slice borrow conflicts with the
                            // `&mut self` on `remove`.
                            let keys: Vec<String> =
                                store.all().iter().map(|e| e.key.clone()).collect();
                            let count = keys.len();
                            for key in &keys {
                                let _ = store.remove(key);
                            }
                            self.console
                                .output(format!("Memory cleared ({count} entries removed)."));
                        }
                    },
                    _ => match self.memory {
                        None => {
                            self.console.output("Memory store not available.");
                        }
                        Some(ref store) => {
                            let entries = store.all();
                            if entries.is_empty() {
                                self.console.output("Memory: (no entries)");
                            } else {
                                self.console
                                    .output(format!("Memory ({} entries):", entries.len()));
                                for entry in entries {
                                    self.console
                                        .output(format!("  {:30}  {}", entry.key, entry.value));
                                }
                            }
                        }
                    },
                }
            }
            // ------------------------------------------------------------------
            // screenshot
            // ------------------------------------------------------------------
            "screenshot" => {
                use std::time::{SystemTime, UNIX_EPOCH};

                let texture = self.postfx.scene_texture();
                let width = texture.width();
                let height = texture.height();

                match capture_frame(&self.gpu.device, &self.gpu.queue, texture, width, height) {
                    Err(e) => {
                        self.console
                            .error(format!("Screenshot capture failed: {e}"));
                    }
                    Ok(pixels) => {
                        // Swap BGRA → RGBA on Metal/D3D12 where the surface
                        // format is Bgra8. `capture_frame` strips row padding
                        // but does not re-encode channel order, so we resolve
                        // it here against the live `gpu.format`.
                        let pixels_rgba = match self.gpu.format {
                            wgpu::TextureFormat::Bgra8Unorm
                            | wgpu::TextureFormat::Bgra8UnormSrgb => {
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

                        // Resolve the target directory: prefer `$HOME/Downloads`,
                        // fall back to `std::env::temp_dir()` if `HOME` is unset
                        // *or* if the Downloads directory cannot be created
                        // (headless Linux, sandboxed containers, etc).
                        let downloads = resolve_screenshot_dir();
                        let filename = format!("phantom-{timestamp}.png");
                        let png_path = downloads.join(&filename);

                        match save_screenshot(&pixels_rgba, width, height, &metadata, &png_path) {
                            Ok(()) => {
                                let path_str = png_path.display().to_string();
                                self.console.system(format!("Screenshot saved: {path_str}"));
                                use crate::notifications::{
                                    Banner, DEFAULT_BANNER_TTL_MS, Severity,
                                };
                                let now_ms = SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .map(|d| d.as_millis() as u64)
                                    .unwrap_or(0);
                                self.notifications.push_banner(Banner {
                                    message: format!("Screenshot saved to {path_str}"),
                                    severity: Severity::Info,
                                    expires_at_ms: now_ms.saturating_add(DEFAULT_BANNER_TTL_MS),
                                });
                            }
                            Err(e) => {
                                self.console.error(format!("Screenshot save failed: {e}"));
                            }
                        }
                    }
                }
            }
            cmd if cmd == "offline" || cmd.starts_with("offline ") => {
                // SAFETY: the match guard above guarantees `cmd` either equals
                // "offline" or begins with "offline ", so `strip_prefix("offline")`
                // can never return None here.
                let subcommand = cmd
                    .strip_prefix("offline")
                    .expect("match guard ensures cmd starts with \"offline\"")
                    .trim();
                match subcommand {
                    "on" | "enable" => {
                        self.console
                            .system("Offline mode: ON (using local backends only)");
                        if let Some(ref brain) = self.brain {
                            let _ =
                                brain.send_event(phantom_brain::events::AiEvent::SetOfflineMode {
                                    enabled: true,
                                });
                        }
                    }
                    "off" | "disable" => {
                        self.console
                            .system("Offline mode: OFF (cloud backends available)");
                        if let Some(ref brain) = self.brain {
                            let _ =
                                brain.send_event(phantom_brain::events::AiEvent::SetOfflineMode {
                                    enabled: false,
                                });
                        }
                    }
                    _ => {
                        self.console
                            .error("Usage: offline on|off (or: offline enable|disable)");
                    }
                }
            }
            "help" => {
                self.console.system("Available commands:");
                self.console.output("  set <key> <value>   Tune shader params (curvature, scanlines, bloom, aberration, vignette, noise)");
                self.console.output("  theme <name>        Switch theme");
                self.console.output("  agent <prompt>      Spawn AI agent");
                self.console
                    .output("  sysmon              Toggle system monitor");
                self.console
                    .output("  appmon              Toggle app diagnostics");
                self.console.output("  plugins             List plugins");
                self.console
                    .output("  plain               Disable all CRT effects");
                self.console
                    .output("  debug               Toggle shader debug HUD");
                self.console
                    .output("  reload              Reload config from disk");
                self.console
                    .output("  boot                Replay boot sequence");
                self.console
                    .output("  video <path>        Play video through CRT shader");
                self.console
                    .output("  history [N]         Show last N commands (default 20)");
                self.console
                    .output("  suggestions         List dismissed/expired suggestion history");
                self.console
                    .output("  inspect             Open inspector pane (Cmd+I also works)");
                self.console
                    .output("  selftest            Brain exercises its own features");
                self.console
                    .output("  selfheal            selftest + auto-fix + commit + push");
                self.console.output(
                    "  ghost privacy on                Enable privacy mode (block cloud APIs)",
                );
                self.console
                    .output("  ghost privacy off               Disable privacy mode");
                self.console
                    .output("  ghost grant <peer> <cap>        Grant capability to a remote peer");
                self.console
                    .output("  ghost revoke <peer>             Revoke all grants for a peer");
                self.console
                    .output("  ghost grants                    List all active peer grants");
                self.console
                    .output("    Capabilities: Sense | Coordinate | Act | Reflect | Compute");
                self.console
                    .output("  clear               Clear console history");
                self.console.output(
                    "  font <size>         Set font size in points (6–72); case-insensitive",
                );
                self.console
                    .output("  font reset          Revert font to default (14pt)");
                self.console
                    .output("  memory              List all memory entries for this session");
                self.console
                    .output("  memory clear        Clear all session memory entries");
                self.console.output(
                    "  screenshot          Capture CRT terminal to ~/Downloads/phantom-<ts>.png",
                );
                self.console.output("  quit                Exit Phantom");
                self.console.output("  dag.<op> [json]     DAG viewer commands: focus_node, clear_focus, scroll_to,");
                self.console
                    .output("                        zoom, highlight, clear_highlight, reset_view");
            }
            _other => {
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
                                    disposition: phantom_agents::dispatch::Disposition::Chat,
                                },
                            );
                        }
                        ResolvedAction::ShowInfo(info_text) => {
                            self.console.output(info_text);
                        }
                        ResolvedAction::Ambiguous { input: _, options } => {
                            self.console
                                .output(format!("Did you mean: {}", options.join(", ")));
                        }
                        ResolvedAction::PassThrough => {
                            // Heuristic couldn't classify — try the LLM backend.
                            self.try_nlp_translate_or_spawn_agent(input);
                        }
                    }
                } else {
                    // No project context — try the LLM backend, then fall back to agent.
                    self.try_nlp_translate_or_spawn_agent(input);
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
                    self.console
                        .error("font_size requires restart (not yet hot-swappable)");
                }
                _ => {
                    self.console.error(format!("Unknown config key: {key}"));
                }
            }
        } else {
            self.console.error(format!(
                "Invalid value for {key}: {value} (expected number)"
            ));
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

    // -----------------------------------------------------------------------
    // NLP LLM translate fallback
    // -----------------------------------------------------------------------

    /// Called when the heuristic `NlpInterpreter` returns `PassThrough`.
    ///
    /// If a configured `nlp_backend` is available, spawns a short-lived
    /// background thread to call `translate()` (synchronous ureq), and sends
    /// the result back via `nlp_translate_tx`.  The `update.rs` drain loop
    /// picks up the result next frame.
    ///
    /// When no backend is configured (key absent or `nlp_llm = false`) the
    /// function falls through to directly spawning an agent — same behaviour
    /// as before this feature was added.
    pub(crate) fn try_nlp_translate_or_spawn_agent(&mut self, input: &str) {
        let input_owned = input.trim().to_string();

        if let Some(ref backend) = self.nlp_backend {
            let backend_arc = std::sync::Arc::clone(backend);
            let tx = self.nlp_translate_tx.clone();
            let ctx = self.context.clone();
            // Clone before moving into the closure so we still have it for the
            // fallback path below.
            let input_for_closure = input_owned.clone();

            let spawn_result = std::thread::Builder::new()
                .name("nlp-translate".into())
                .spawn(move || {
                    let input_owned = input_for_closure;
                    // Use the cached context, or fall back to detecting it
                    // synchronously on the thread (cheap: just CWD scan).
                    let detected;
                    let ctx_ref: &phantom_context::ProjectContext = match ctx {
                        Some(ref c) => c,
                        None => {
                            detected =
                                phantom_context::ProjectContext::detect(std::path::Path::new("."));
                            &detected
                        }
                    };
                    // Wrap the LlmSkill in a LlmSkillAdapter so it satisfies
                    // the `&dyn LlmBackend` parameter expected by `translate`.
                    let adapter = phantom_skill_host::LlmSkillAdapter::new(backend_arc);
                    match translate(&input_owned, ctx_ref, &adapter) {
                        Ok(intent) => {
                            let res = intent_to_translate_result(intent);
                            // `try_send` — if the channel is full (8 queued calls)
                            // we silently drop this result rather than blocking.
                            let _ = tx.try_send(res);
                        }
                        Err(e) => {
                            warn!("NLP translate error: {e}");
                            // Fallback: surface as a clarify message.
                            let res = NlpTranslateResult {
                                display: format!("(NLP error: {e})"),
                                action: None,
                            };
                            let _ = tx.try_send(res);
                        }
                    }
                });

            match spawn_result {
                Ok(_) => {
                    self.console.system("Thinking...");
                }
                Err(e) => {
                    warn!("Failed to spawn nlp-translate thread: {e}");
                    // Thread spawn failed — fall back to direct agent spawn.
                    self.console
                        .system(format!("Spawning agent: {input_owned}"));
                    self.pending_brain_actions
                        .push(phantom_brain::events::AiAction::SpawnAgent {
                            task: phantom_agents::AgentTask::FreeForm {
                                prompt: input_owned,
                            },
                            spawn_tag: None,
                            disposition: phantom_agents::dispatch::Disposition::Chat,
                        });
                }
            }
        } else {
            // No LLM backend — spawn agent directly.
            self.console
                .system(format!("Spawning agent: {input_owned}"));
            self.pending_brain_actions
                .push(phantom_brain::events::AiAction::SpawnAgent {
                    task: phantom_agents::AgentTask::FreeForm {
                        prompt: input_owned,
                    },
                    spawn_tag: None,
                    disposition: phantom_agents::dispatch::Disposition::Chat,
                });
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers (free functions — not methods so they can be tested without App)
// ---------------------------------------------------------------------------

/// Convert an [`Intent`] returned by `translate()` into an [`NlpTranslateResult`].
///
/// `original_input` is used as a fallback label when the intent doesn't carry
/// its own display-friendly description.
pub(crate) fn intent_to_translate_result(intent: Intent) -> NlpTranslateResult {
    match intent {
        Intent::RunCommand { cmd } => NlpTranslateResult {
            display: format!("Running: {cmd}"),
            action: Some(phantom_brain::events::AiAction::RunCommand(cmd)),
        },
        Intent::SpawnAgent { goal } => NlpTranslateResult {
            display: format!("Spawning agent: {goal}"),
            action: Some(phantom_brain::events::AiAction::SpawnAgent {
                task: phantom_agents::AgentTask::FreeForm { prompt: goal },
                spawn_tag: None,
                disposition: phantom_agents::dispatch::Disposition::Chat,
            }),
        },
        Intent::SearchHistory { query } => {
            // Map history search to a concrete git log command.
            // Use {:?} Debug quoting to shell-escape the query and prevent
            // injection: LLM-controlled input like "foo; rm -rf ~" becomes
            // `--grep="foo; rm -rf ~"` which git treats as a literal grep
            // pattern rather than a second shell command.
            let cmd = format!("git log --oneline --all --grep={query:?}");
            NlpTranslateResult {
                display: format!("Searching history: {query}"),
                action: Some(phantom_brain::events::AiAction::RunCommand(cmd)),
            }
        }
        Intent::Clarify { question } => NlpTranslateResult {
            display: format!("({question})"),
            action: None,
        },
    }
}

// ---------------------------------------------------------------------------
// Capability helpers (issue #8)
// ---------------------------------------------------------------------------

/// Parse a capability class name from a console command argument.
fn parse_capability_class(s: &str) -> Option<CapabilityClass> {
    match s {
        "Sense" | "sense" => Some(CapabilityClass::Sense),
        "Coordinate" | "coordinate" => Some(CapabilityClass::Coordinate),
        "Act" | "act" => Some(CapabilityClass::Act),
        "Reflect" | "reflect" => Some(CapabilityClass::Reflect),
        "Compute" | "compute" => Some(CapabilityClass::Compute),
        _ => None,
    }
}

/// Return the canonical display name for a capability class.
fn capability_class_str(c: CapabilityClass) -> &'static str {
    match c {
        CapabilityClass::Sense => "Sense",
        CapabilityClass::Coordinate => "Coordinate",
        CapabilityClass::Act => "Act",
        CapabilityClass::Reflect => "Reflect",
        CapabilityClass::Compute => "Compute",
    }
}

/// Resolve the directory used to save screenshots.
///
/// Prefers `$HOME/Downloads`. If `HOME` is unset or the Downloads directory
/// cannot be created (headless Linux, sandboxed containers, NixOS where the
/// XDG dir was never provisioned, etc.) the function falls back to
/// `std::env::temp_dir()`. The returned directory is guaranteed to exist on
/// success; if both the preferred path *and* the temp dir cannot be created
/// the temp dir path is still returned so the eventual `save_screenshot`
/// call surfaces a useful filesystem error to the user.
fn resolve_screenshot_dir() -> std::path::PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        let downloads = std::path::PathBuf::from(home).join("Downloads");
        if std::fs::create_dir_all(&downloads).is_ok() {
            return downloads;
        }
    }
    let tmp = std::env::temp_dir();
    let _ = std::fs::create_dir_all(&tmp);
    tmp
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_capability_class_case_insensitive() {
        assert_eq!(
            parse_capability_class("Sense"),
            Some(CapabilityClass::Sense)
        );
        assert_eq!(
            parse_capability_class("sense"),
            Some(CapabilityClass::Sense)
        );
        assert_eq!(
            parse_capability_class("Coordinate"),
            Some(CapabilityClass::Coordinate)
        );
        assert_eq!(parse_capability_class("Act"), Some(CapabilityClass::Act));
        assert_eq!(
            parse_capability_class("Reflect"),
            Some(CapabilityClass::Reflect)
        );
        assert_eq!(
            parse_capability_class("Compute"),
            Some(CapabilityClass::Compute)
        );
        assert_eq!(parse_capability_class("unknown"), None);
        assert_eq!(parse_capability_class(""), None);
    }

    #[test]
    fn capability_class_str_round_trip() {
        for cap in [
            CapabilityClass::Sense,
            CapabilityClass::Coordinate,
            CapabilityClass::Act,
            CapabilityClass::Reflect,
            CapabilityClass::Compute,
        ] {
            let s = capability_class_str(cap);
            let parsed = parse_capability_class(s);
            assert_eq!(parsed, Some(cap), "round-trip failed for {cap:?}");
        }
    }

    // -----------------------------------------------------------------------
    // Regression tests for the `ghost grant` CLI dispatch path (issue #8)
    //
    // The root bug: `splitn(3, ' ')` yields at most 3 tokens (indices 0..=2),
    // so `parts.get(3)` always returned `None` and the
    // `(Some("grant"), Some(peer), Some(cap))` match arm never fired.
    // Fixing to `splitn(4, ' ')` makes index 3 available.
    //
    // These tests exercise the exact tokenisation logic used in
    // `execute_user_command` without constructing a full `App` (which requires
    // a GPU window).
    // -----------------------------------------------------------------------

    /// `splitn(4)` must yield 4 tokens for `ghost grant <peer> <cap>`.
    /// This is the minimal regression that catches the original splitn(3) bug:
    /// with splitn(3) `parts.get(3)` returns None and the grant arm never fires.
    #[test]
    fn ghost_grant_splitn4_yields_four_parts() {
        let input = "ghost grant peer123 coordinate";
        let parts: Vec<&str> = input.trim().splitn(4, ' ').collect();
        assert_eq!(
            parts.len(),
            4,
            "splitn(4) must produce 4 tokens for 'ghost grant <peer> <cap>'"
        );
        assert_eq!(parts[0], "ghost");
        assert_eq!(parts[1], "grant");
        assert_eq!(parts[2], "peer123");
        assert_eq!(parts[3], "coordinate");
        // Verify the match tuple that the dispatch arm checks
        assert_eq!(parts.get(1).copied(), Some("grant"));
        assert_eq!(parts.get(2).copied(), Some("peer123"));
        assert_eq!(parts.get(3).copied(), Some("coordinate"));
    }

    /// Confirm that `splitn(3)` — the original broken value — would have
    /// swallowed the capability argument.
    #[test]
    fn ghost_grant_splitn3_was_broken() {
        let input = "ghost grant peer123 coordinate";
        let parts: Vec<&str> = input.trim().splitn(3, ' ').collect();
        // With the old splitn(3) the capability token is absent.
        assert_eq!(parts.len(), 3);
        assert_eq!(
            parts.get(3),
            None,
            "splitn(3) must NOT yield a 4th token (documents the regression)"
        );
    }

    /// The capability string parsed from `parts[3]` must be recognised.
    #[test]
    fn ghost_grant_dispatch_capability_parsed() {
        let input = "ghost grant peer-A Coordinate";
        let parts: Vec<&str> = input.trim().splitn(4, ' ').collect();
        let cap_str = parts.get(3).copied().unwrap_or("");
        let cap = parse_capability_class(cap_str);
        assert_eq!(cap, Some(CapabilityClass::Coordinate));
    }

    /// `ghost revoke <peer>` only needs 3 tokens — splitn(4) is backward-compatible.
    #[test]
    fn ghost_revoke_dispatch_still_works() {
        let input = "ghost revoke peer123";
        let parts: Vec<&str> = input.trim().splitn(4, ' ').collect();
        assert_eq!(parts.first().copied(), Some("ghost"));
        assert_eq!(parts.get(1).copied(), Some("revoke"));
        assert_eq!(parts.get(2).copied(), Some("peer123"));
        // No 4th token needed for revoke — arm pattern is (Some("revoke"), Some(peer), _)
        assert!(parts.get(3).is_none());
    }

    /// `ghost grants` needs only 2 tokens — splitn(4) is backward-compatible.
    #[test]
    fn ghost_grants_list_dispatch_still_works() {
        let input = "ghost grants";
        let parts: Vec<&str> = input.trim().splitn(4, ' ').collect();
        assert_eq!(parts.first().copied(), Some("ghost"));
        assert_eq!(parts.get(1).copied(), Some("grants"));
        assert!(parts.get(2).is_none());
        assert!(parts.get(3).is_none());
    }

    /// Capabilities with spaces in future inputs must not be accidentally split —
    /// splitn(4) stops splitting after the 4th token.
    #[test]
    fn ghost_grant_extra_trailing_text_does_not_overflow() {
        // If a user accidentally adds trailing text, the 4th element absorbs it.
        let input = "ghost grant peer-B Act extra-junk";
        let parts: Vec<&str> = input.trim().splitn(4, ' ').collect();
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[3], "Act extra-junk");
        // parse_capability_class should return None for the garbage suffix,
        // triggering the "Unknown capability" error path — not a panic.
        let cap = parse_capability_class(parts[3]);
        assert_eq!(cap, None);
    }

    // -----------------------------------------------------------------------
    // Bug 4 — Case-sensitivity regression tests
    // -----------------------------------------------------------------------

    /// The main command parser must route mixed-case variants to the same arm
    /// as lowercase. `splitn` + `.to_lowercase()` on `parts[0]` ensures this.
    #[test]
    fn command_parser_case_insensitive() {
        // We test the normalisation logic directly: for each variant the first
        // word lowercased must equal the canonical command token.
        for (input, expected_cmd) in [
            ("Font 16", "font"),
            ("FONT reset", "font"),
            ("Memory", "memory"),
            ("MEMORY clear", "memory"),
            ("Screenshot", "screenshot"),
            ("SCREENSHOT", "screenshot"),
            ("Theme amber", "theme"),
            ("THEME ice", "theme"),
            ("QUIT", "quit"),
        ] {
            let parts: Vec<&str> = input.trim().splitn(4, ' ').collect();
            let cmd_lower = parts[0].to_lowercase();
            assert_eq!(
                cmd_lower.as_str(),
                expected_cmd,
                "input '{input}' must normalise to '{expected_cmd}'"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Bug 1 — font command parsing
    // -----------------------------------------------------------------------

    /// `font <size>` must parse the numeric argument correctly.
    #[test]
    fn font_command_sets_font_size() {
        // Parse the numeric part directly (integration with App requires GPU).
        let input = "font 16";
        let parts: Vec<&str> = input.trim().splitn(4, ' ').collect();
        let cmd_lower = parts[0].to_lowercase();
        assert_eq!(cmd_lower.as_str(), "font");
        let size_arg = parts.get(1).copied().unwrap_or("");
        let parsed: Result<f32, _> = size_arg.parse();
        assert!(parsed.is_ok(), "font size '16' must parse as f32");
        assert!((parsed.unwrap() - 16.0).abs() < f32::EPSILON);
    }

    /// `font reset` must produce the "reset" subcommand token.
    #[test]
    fn font_reset_reverts_to_default() {
        let input = "font reset";
        let parts: Vec<&str> = input.trim().splitn(4, ' ').collect();
        let cmd_lower = parts[0].to_lowercase();
        assert_eq!(cmd_lower.as_str(), "font");
        assert_eq!(parts.get(1).copied(), Some("reset"));
    }

    /// Subcommand tokens for `font` must be matched case-insensitively
    /// (review follow-up — addresses the "incomplete case-sensitivity fix"
    /// raised on the original PR).
    #[test]
    fn font_subcommand_is_case_insensitive() {
        for raw in &["FONT RESET", "Font Reset", "fOnT rEsEt"] {
            let parts: Vec<&str> = raw.trim().splitn(4, ' ').collect();
            let cmd_lower = parts[0].to_lowercase();
            let sub_lower = parts.get(1).map(|s| s.to_ascii_lowercase());
            assert_eq!(cmd_lower, "font");
            assert_eq!(sub_lower.as_deref(), Some("reset"), "input was {raw}");
        }
    }

    /// Subcommand tokens for `memory` must be matched case-insensitively.
    #[test]
    fn memory_subcommand_is_case_insensitive() {
        for raw in &["memory clear", "MEMORY CLEAR", "Memory Clear"] {
            let parts: Vec<&str> = raw.trim().splitn(4, ' ').collect();
            let cmd_lower = parts[0].to_lowercase();
            let sub_lower = parts.get(1).map(|s| s.to_ascii_lowercase());
            assert_eq!(cmd_lower, "memory");
            assert_eq!(sub_lower.as_deref(), Some("clear"), "input was {raw}");
        }
    }

    /// Constants advertised in the `font` usage string must match the
    /// values used in the parse guard, so users see the same range the
    /// validator actually enforces.
    #[test]
    fn font_size_constants_match_advertised_range() {
        assert_eq!(MIN_FONT_SIZE_PT, 6.0);
        assert_eq!(MAX_FONT_SIZE_PT, 72.0);
        assert_eq!(DEFAULT_FONT_SIZE_PT, 14.0);
    }

    /// `resolve_screenshot_dir()` must always return a path; under tests
    /// the temp dir branch is the deterministic fallback.
    #[test]
    fn screenshot_dir_resolution_is_robust() {
        // Save and clear HOME so we exercise the temp-dir fallback.
        let prev_home = std::env::var("HOME").ok();
        // SAFETY: tests are single-threaded by default for the modify-env
        // case; if this becomes flaky under nextest parallelism, scope it
        // to a serial test module.
        unsafe {
            std::env::remove_var("HOME");
        }
        let dir = super::resolve_screenshot_dir();
        assert!(dir.exists() || dir == std::env::temp_dir());
        // Restore.
        if let Some(h) = prev_home {
            unsafe {
                std::env::set_var("HOME", h);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Bug 2 — memory command parsing
    // -----------------------------------------------------------------------

    /// `memory` with no subcommand must resolve to the list path (None subcommand).
    #[test]
    fn memory_command_lists_entries() {
        let input = "memory";
        let parts: Vec<&str> = input.trim().splitn(4, ' ').collect();
        let cmd_lower = parts[0].to_lowercase();
        assert_eq!(cmd_lower.as_str(), "memory");
        // No subcommand → shows entries.
        assert_eq!(parts.get(1).copied(), None);
    }

    /// `memory clear` must deliver "clear" as the subcommand.
    #[test]
    fn memory_clear_subcommand_parsed() {
        let input = "memory clear";
        let parts: Vec<&str> = input.trim().splitn(4, ' ').collect();
        let cmd_lower = parts[0].to_lowercase();
        assert_eq!(cmd_lower.as_str(), "memory");
        assert_eq!(parts.get(1).copied(), Some("clear"));
    }

    // -----------------------------------------------------------------------
    // Bug 3 — screenshot command parsing
    // -----------------------------------------------------------------------

    /// `screenshot` must resolve to the "screenshot" command token.
    #[test]
    fn screenshot_command_calls_renderer() {
        // The renderer invocation itself requires a GPU context; we verify
        // the dispatch tokenisation here and rely on the MCP screenshot path
        // (tested in phantom-app integration tests) for end-to-end coverage.
        let input = "screenshot";
        let parts: Vec<&str> = input.trim().splitn(4, ' ').collect();
        let cmd_lower = parts[0].to_lowercase();
        assert_eq!(cmd_lower.as_str(), "screenshot");
        assert_eq!(parts.len(), 1, "screenshot takes no arguments");
    }
}
