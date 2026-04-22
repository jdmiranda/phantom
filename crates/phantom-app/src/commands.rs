//! Command execution and supervisor message handling for Phantom.
//!
//! Handles backtick command mode input (set, theme, debug, boot, quit, NLP
//! fallback) and supervisor protocol commands.

use log::{debug, info, warn};

use phantom_protocol::{AppMessage, SupervisorCommand};
use phantom_nlp::NlpInterpreter;
use phantom_nlp::interpreter::ResolvedAction;
use phantom_ui::themes;

use crate::app::{App, AppState};
use crate::boot::BootSequence;
use crate::config::PhantomConfig;

impl App {
    /// Parse and execute a user command string entered via command mode.
    pub(crate) fn execute_user_command(&mut self, input: &str) {
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
                    if let Some(ref mut sv) = self.supervisor {
                        sv.send(&AppMessage::Log(format!("set {key}={value}")));
                    }
                } else {
                    warn!("Usage: set <key> <value>");
                }
            }
            "theme" => {
                if parts.len() >= 2 {
                    self.apply_theme(parts[1]);
                    if let Some(ref mut sv) = self.supervisor {
                        sv.send(&AppMessage::Log(format!("theme {}", parts[1])));
                    }
                } else {
                    warn!("Usage: theme <name>");
                }
            }
            "reload" => {
                self.apply_reload();
            }
            "quit" | "exit" => {
                info!("Quit requested via command mode");
                self.quit_requested = true;
            }
            "boot" => {
                info!("Replaying boot sequence via command mode");
                let w = self.gpu.surface_config.width;
                let h = self.gpu.surface_config.height;
                let bc = (w as f32 / self.cell_size.0).floor().max(40.0) as usize;
                let br = (h as f32 / self.cell_size.1).floor().max(10.0) as usize;
                self.boot = BootSequence::with_size(bc, br);
                self.state = AppState::Boot;
            }
            "debug" => {
                self.debug_hud = !self.debug_hud;
                info!("Debug HUD: {}", if self.debug_hud { "ON" } else { "OFF" });
            }
            "plain" => {
                self.theme.shader_params.scanline_intensity = 0.0;
                self.theme.shader_params.bloom_intensity = 0.0;
                self.theme.shader_params.chromatic_aberration = 0.0;
                self.theme.shader_params.curvature = 0.0;
                self.theme.shader_params.vignette_intensity = 0.0;
                self.theme.shader_params.noise_intensity = 0.0;
                info!("Plain mode: all CRT effects disabled");
            }
            "help" => {
                info!(
                    "Commands: set <k> <v> | theme <name> | plain | debug | reload | boot | quit"
                );
            }
            other => {
                // NLP fallback: try interpreting as natural language.
                if let Some(ref ctx) = self.context {
                    match NlpInterpreter::interpret(input, ctx) {
                        ResolvedAction::RunCommand(cmd) => {
                            info!("[PHANTOM NLP]: running `{cmd}`");
                            if let Some(pane) = self.panes.get_mut(self.focused_pane) {
                                let cmd_bytes = format!("{cmd}\n");
                                let _ = pane.terminal.pty_write(cmd_bytes.as_bytes());
                            }
                        }
                        ResolvedAction::SpawnAgent(desc) => {
                            info!("[PHANTOM NLP]: agent requested: {desc}");
                        }
                        ResolvedAction::ShowInfo(info_text) => {
                            info!("[PHANTOM]: {info_text}");
                        }
                        ResolvedAction::Ambiguous { input: _, options } => {
                            info!("[PHANTOM]: Did you mean: {}", options.join(", "));
                        }
                        ResolvedAction::PassThrough => {
                            warn!("Unknown command: {other}");
                        }
                    }
                } else {
                    warn!("Unknown command: {other}");
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
                }
                _ => {
                    warn!("Unknown config key: {key}");
                }
            }
        } else {
            warn!("Invalid value for {key}: {value} (expected f32)");
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
