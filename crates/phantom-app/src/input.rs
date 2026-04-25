//! Keyboard input handling and winit key conversion for Phantom.
//!
//! Contains `handle_key`, `handle_key_with_mods`, `dispatch_action`,
//! command mode / debug HUD key handlers, and all winit → Phantom key
//! conversion functions.

use std::time::Instant;

use log::{debug, info};
use winit::keyboard::{Key, NamedKey};

use phantom_terminal::input::{self, KeyEvent, PhantomKey, PhantomModifiers};
use phantom_ui::keybinds::{Action, KeyCombo};
use phantom_ui::keybinds::Key as UiKey;

use crate::app::{App, AppState};

// ---------------------------------------------------------------------------
// Key conversion (free functions)
// ---------------------------------------------------------------------------

/// Improved key combo extraction that incorporates externally tracked modifiers.
pub fn winit_key_to_combo_with_mods(
    event: &winit::event::KeyEvent,
    modifiers: winit::event::Modifiers,
) -> Option<KeyCombo> {
    let ui_key = winit_logical_to_ui_key(&event.logical_key)?;

    let state = modifiers.state();
    Some(KeyCombo {
        key: ui_key,
        ctrl: state.control_key(),
        alt: state.alt_key(),
        shift: state.shift_key(),
        logo: state.super_key(),
    })
}

/// Map a winit logical key to our UI key enum.
fn winit_logical_to_ui_key(key: &Key) -> Option<UiKey> {
    match key {
        Key::Character(s) => {
            let ch = s.chars().next()?;
            let ch_lower = ch.to_ascii_lowercase();
            Some(UiKey::Char(ch_lower))
        }
        Key::Named(named) => match named {
            NamedKey::Enter => Some(UiKey::Enter),
            NamedKey::Tab => Some(UiKey::Tab),
            NamedKey::Space => Some(UiKey::Space),
            NamedKey::Backspace => Some(UiKey::Backspace),
            NamedKey::Delete => Some(UiKey::Delete),
            NamedKey::Insert => Some(UiKey::Insert),
            NamedKey::Escape => Some(UiKey::Escape),
            NamedKey::ArrowUp => Some(UiKey::Up),
            NamedKey::ArrowDown => Some(UiKey::Down),
            NamedKey::ArrowLeft => Some(UiKey::Left),
            NamedKey::ArrowRight => Some(UiKey::Right),
            NamedKey::Home => Some(UiKey::Home),
            NamedKey::End => Some(UiKey::End),
            NamedKey::PageUp => Some(UiKey::PageUp),
            NamedKey::PageDown => Some(UiKey::PageDown),
            NamedKey::F1 => Some(UiKey::F(1)),
            NamedKey::F2 => Some(UiKey::F(2)),
            NamedKey::F3 => Some(UiKey::F(3)),
            NamedKey::F4 => Some(UiKey::F(4)),
            NamedKey::F5 => Some(UiKey::F(5)),
            NamedKey::F6 => Some(UiKey::F(6)),
            NamedKey::F7 => Some(UiKey::F(7)),
            NamedKey::F8 => Some(UiKey::F(8)),
            NamedKey::F9 => Some(UiKey::F(9)),
            NamedKey::F10 => Some(UiKey::F(10)),
            NamedKey::F11 => Some(UiKey::F(11)),
            NamedKey::F12 => Some(UiKey::F(12)),
            _ => None,
        },
        _ => None,
    }
}

/// Convert a winit `KeyEvent` to a terminal `KeyEvent` for PTY encoding.
fn winit_key_to_terminal(event: &winit::event::KeyEvent) -> Option<KeyEvent> {
    let phantom_key = match &event.logical_key {
        Key::Character(s) => {
            let ch = s.chars().next()?;
            PhantomKey::Char(ch)
        }
        Key::Named(named) => match named {
            NamedKey::Enter => PhantomKey::Enter,
            NamedKey::Backspace => PhantomKey::Backspace,
            NamedKey::Tab => PhantomKey::Tab,
            NamedKey::Escape => PhantomKey::Escape,
            NamedKey::ArrowUp => PhantomKey::Up,
            NamedKey::ArrowDown => PhantomKey::Down,
            NamedKey::ArrowLeft => PhantomKey::Left,
            NamedKey::ArrowRight => PhantomKey::Right,
            NamedKey::Home => PhantomKey::Home,
            NamedKey::End => PhantomKey::End,
            NamedKey::PageUp => PhantomKey::PageUp,
            NamedKey::PageDown => PhantomKey::PageDown,
            NamedKey::Delete => PhantomKey::Delete,
            NamedKey::Insert => PhantomKey::Insert,
            NamedKey::F1 => PhantomKey::F(1),
            NamedKey::F2 => PhantomKey::F(2),
            NamedKey::F3 => PhantomKey::F(3),
            NamedKey::F4 => PhantomKey::F(4),
            NamedKey::F5 => PhantomKey::F(5),
            NamedKey::F6 => PhantomKey::F(6),
            NamedKey::F7 => PhantomKey::F(7),
            NamedKey::F8 => PhantomKey::F(8),
            NamedKey::F9 => PhantomKey::F(9),
            NamedKey::F10 => PhantomKey::F(10),
            NamedKey::F11 => PhantomKey::F(11),
            NamedKey::F12 => PhantomKey::F(12),
            NamedKey::Space => PhantomKey::Char(' '),
            _ => return None,
        },
        _ => return None,
    };

    let mods = PhantomModifiers::NONE;
    Some(KeyEvent { key: phantom_key, mods })
}

/// Get the current wall clock time as a `HH:MM` string.
pub(crate) fn chrono_time_string() -> String {
    // Use libc localtime to get the correct local timezone.
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as libc::time_t;

    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&timestamp, &mut tm) };

    format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
}

// ---------------------------------------------------------------------------
// Input methods on App
// ---------------------------------------------------------------------------

impl App {
    /// Handle modifier state changes from winit.
    pub fn handle_modifiers(&mut self, modifiers: winit::event::Modifiers) {
        let _ = modifiers;
    }

    /// Dispatch an application-level action from the keybind registry.
    pub(crate) fn dispatch_action(&mut self, action: Action) {
        match action {
            Action::Quit => {
                info!("Quit requested via keybind");
                self.quit_requested = true;
            }
            Action::Copy => {
                debug!("Action: Copy (not yet implemented)");
            }
            Action::Paste => {
                debug!("Action: Paste (not yet implemented)");
            }
            Action::NewTab => {
                debug!("Action: NewTab (not yet implemented)");
            }
            Action::CloseTab => {
                debug!("Action: CloseTab (not yet implemented)");
            }
            Action::SplitHorizontal => {
                self.split_focused_pane(true);
            }
            Action::SplitVertical => {
                self.split_focused_pane(false);
            }
            Action::FocusNext => {
                let ids = self.coordinator.all_app_ids();
                if !ids.is_empty() {
                    let current = self.coordinator.focused().unwrap_or(0);
                    let idx = ids.iter().position(|&id| id == current).unwrap_or(0);
                    let next = ids[(idx + 1) % ids.len()];
                    self.coordinator.set_focus(next);
                    debug!("Focus next: adapter {next}");
                }
            }
            Action::FocusPrev => {
                let ids = self.coordinator.all_app_ids();
                if !ids.is_empty() {
                    let current = self.coordinator.focused().unwrap_or(0);
                    let idx = ids.iter().position(|&id| id == current).unwrap_or(0);
                    let prev = ids[(idx + ids.len() - 1) % ids.len()];
                    self.coordinator.set_focus(prev);
                    debug!("Focus prev: adapter {prev}");
                }
            }
            Action::CloseFocused => {
                self.close_focused_pane();
            }
            Action::ZoomIn => {
                let new_size = self.text_renderer.font_size() + 2.0;
                info!("Zoom in: {new_size}pt");
                self.text_renderer.set_font_size(new_size);
                self.cell_size = self.text_renderer.measure_cell();
                self.atlas.clear();
            }
            Action::ZoomOut => {
                let new_size = (self.text_renderer.font_size() - 2.0).max(8.0);
                info!("Zoom out: {new_size}pt");
                self.text_renderer.set_font_size(new_size);
                self.cell_size = self.text_renderer.measure_cell();
                self.atlas.clear();
            }
            Action::ToggleFullscreen => {
                if self.fullscreen_pane.is_some() {
                    info!("Exiting fullscreen mode");
                    self.fullscreen_pane = None;
                } else if let Some(focused) = self.coordinator.focused() {
                    info!("Entering fullscreen mode: adapter {focused}");
                    self.fullscreen_pane = Some(focused);
                }
            }
            Action::ScrollPageUp => {
                let _ = self.coordinator.send_command_to_focused("scroll", &serde_json::json!({"direction": "page_up"}));
            }
            Action::ScrollPageDown => {
                let _ = self.coordinator.send_command_to_focused("scroll", &serde_json::json!({"direction": "page_down"}));
            }
            Action::ScrollToTop => {
                let _ = self.coordinator.send_command_to_focused("scroll", &serde_json::json!({"direction": "top"}));
            }
            Action::ScrollToBottom => {
                let _ = self.coordinator.send_command_to_focused("scroll", &serde_json::json!({"direction": "bottom"}));
            }
            _ => {
                debug!("Action: {action} (not yet implemented)");
            }
        }
    }

    /// Handle keys when the Quake console is open.
    pub(crate) fn handle_console_key(&mut self, event: &winit::event::KeyEvent) {
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                debug!("Console closed via Escape");
                self.console.open = false;
            }
            Key::Named(NamedKey::Enter) => {
                if let Some(cmd) = self.console.submit() {
                    self.execute_user_command(&cmd);
                }
            }
            Key::Named(NamedKey::Tab) => {
                self.console.tab_complete();
            }
            Key::Named(NamedKey::Backspace) => {
                self.console.clear_tab();
                self.console.input.pop();
            }
            Key::Named(NamedKey::Space) => {
                self.console.clear_tab();
                self.console.input.push(' ');
            }
            Key::Named(NamedKey::ArrowUp) => {
                self.console.history_up();
            }
            Key::Named(NamedKey::ArrowDown) => {
                self.console.history_down();
            }
            Key::Named(NamedKey::PageUp) => {
                self.console.scroll_up(10);
            }
            Key::Named(NamedKey::PageDown) => {
                self.console.scroll_down(10);
            }
            Key::Character(s) => {
                if s.as_str() != "`" {
                    self.console.clear_tab();
                    self.console.input.push_str(s.as_str());
                }
            }
            _ => {}
        }
    }

    /// Handle keys when the debug shader HUD is open.
    pub(crate) fn handle_debug_hud_key(&mut self, event: &winit::event::KeyEvent) {
        const PARAM_COUNT: usize = 6;
        const STEP: f32 = 0.01;

        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                self.debug_hud = false;
            }
            Key::Named(NamedKey::Tab) => {
                self.debug_hud_selected = (self.debug_hud_selected + 1) % PARAM_COUNT;
            }
            Key::Named(NamedKey::ArrowUp) => {
                self.adjust_debug_param(STEP);
            }
            Key::Named(NamedKey::ArrowDown) => {
                self.adjust_debug_param(-STEP);
            }
            Key::Named(NamedKey::ArrowRight) => {
                self.adjust_debug_param(STEP * 5.0);
            }
            Key::Named(NamedKey::ArrowLeft) => {
                self.adjust_debug_param(-STEP * 5.0);
            }
            _ => {}
        }
    }

    /// Adjust the currently selected debug HUD shader parameter.
    pub(crate) fn adjust_debug_param(&mut self, delta: f32) {
        let sp = &mut self.theme.shader_params;
        let val = match self.debug_hud_selected {
            0 => &mut sp.scanline_intensity,
            1 => &mut sp.bloom_intensity,
            2 => &mut sp.chromatic_aberration,
            3 => &mut sp.curvature,
            4 => &mut sp.vignette_intensity,
            5 => &mut sp.noise_intensity,
            _ => return,
        };
        *val = (*val + delta).clamp(0.0, 1.0);
    }

    /// Handle a keyboard event with externally tracked modifier state.
    pub fn handle_key_with_mods(
        &mut self,
        event: winit::event::KeyEvent,
        modifiers: winit::event::Modifiers,
    ) {
        if !event.state.is_pressed() {
            return;
        }

        self.last_input_time = Instant::now();

        // Escape exits fullscreen mode.
        if self.fullscreen_pane.is_some()
            && matches!(&event.logical_key, Key::Named(NamedKey::Escape))
        {
            info!("Exiting fullscreen via Escape");
            self.fullscreen_pane = None;
            return;
        }

        if self.suggestion.is_some() {
            self.suggestion = None;
        }

        // Settings panel captures all keys when open.
        if self.settings_panel.open {
            self.handle_settings_key(&event);
            return;
        }

        if self.debug_hud {
            self.handle_debug_hud_key(&event);
            return;
        }

        // Escape kills video playback from anywhere.
        if self.video_playback.is_some()
            && matches!(&event.logical_key, Key::Named(NamedKey::Escape))
        {
            if let Some(ref mut vp) = self.video_playback {
                vp.stop();
            }
            self.video_playback = None;
            self.video_renderer.clear();
            self.console.system("Video stopped");
            return;
        }

        if self.console.open {
            // Backtick while console is open = close it (toggle).
            if !modifiers.state().control_key()
                && !modifiers.state().alt_key()
                && !modifiers.state().super_key()
                && matches!(&event.logical_key, Key::Character(s) if s.as_str() == "`")
            {
                self.console.toggle();
                return;
            }
            self.handle_console_key(&event);
            return;
        }

        // Ctrl+, opens settings panel.
        if modifiers.state().control_key()
            && matches!(&event.logical_key, Key::Character(s) if s.as_str() == ",")
        {
            self.settings_panel.toggle();
            return;
        }

        if !modifiers.state().control_key()
            && !modifiers.state().alt_key()
            && !modifiers.state().super_key()
        {
            if matches!(&event.logical_key, Key::Character(s) if s.as_str() == "`") {
                self.console.toggle();
                debug!("Console toggled open");
                return;
            }
        }

        if let Some(combo) = winit_key_to_combo_with_mods(&event, modifiers) {
            if let Some(action) = self.keybinds.lookup(&combo) {
                // Alt-screen guard: don't consume scroll keybinds in vim/htop/less.
                // Let them fall through to the PTY so the program receives them.
                let is_scroll = matches!(action,
                    Action::ScrollPageUp | Action::ScrollPageDown |
                    Action::ScrollToTop | Action::ScrollToBottom
                );
                if is_scroll {
                    // Check if focused adapter is in alt-screen mode (vim/htop/less).
                    // If so, let the keypress fall through to the PTY.
                    let in_alt = self.coordinator.focused()
                        .and_then(|id| self.coordinator.get_state(id))
                        .and_then(|state| state.get("alt_screen").and_then(|v| v.as_bool()))
                        .unwrap_or(false);
                    if !in_alt {
                        self.dispatch_action(*action);
                        return;
                    }
                    // alt screen: fall through to PTY encoding
                } else {
                    self.dispatch_action(*action);
                    return;
                }
            }
        }

        if self.state == AppState::Boot {
            self.boot.skip();
            return;
        }

        if let Some(mut terminal_event) = winit_key_to_terminal(&event) {
            let state = modifiers.state();
            terminal_event.mods = PhantomModifiers {
                ctrl: state.control_key(),
                alt: state.alt_key(),
                shift: state.shift_key(),
                logo: state.super_key(),
            };

            // Copy selection to clipboard: Cmd+C (macOS) or Ctrl+Shift+C.
            if terminal_event.key == PhantomKey::Char('c')
                && (terminal_event.mods.logo || (terminal_event.mods.ctrl && terminal_event.mods.shift))
            {
                if let Some(focused) = self.coordinator.focused() {
                    if let Ok(text) = self.coordinator.send_command(focused, "select_copy", &serde_json::json!({})) {
                        if !text.is_empty() {
                            #[cfg(target_os = "macos")]
                            {
                                // Use pbcopy for clipboard on macOS.
                                use std::io::Write;
                                if let Ok(mut child) = std::process::Command::new("pbcopy")
                                    .stdin(std::process::Stdio::piped())
                                    .spawn()
                                {
                                    if let Some(ref mut stdin) = child.stdin {
                                        let _ = stdin.write_all(text.as_bytes());
                                    }
                                    let _ = child.wait();
                                }
                            }
                            debug!("Copied {} chars to clipboard", text.len());
                            return;
                        }
                    }
                }
            }

            // Clear selection on any other keypress.
            if let Some(focused) = self.coordinator.focused() {
                let _ = self.coordinator.send_command(focused, "select_clear", &serde_json::json!({}));
            }

            // Encode key event to raw PTY bytes.
            let bytes = input::encode_key(&terminal_event);
            if bytes.is_empty() {
                return;
            }

            // Route through coordinator (adapter-managed terminals).
            self.coordinator.route_bytes(&bytes);
        }
    }

    /// Handle keys when the settings panel is open.
    pub(crate) fn handle_settings_key(&mut self, event: &winit::event::KeyEvent) {
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => self.settings_panel.open = false,
            Key::Named(NamedKey::ArrowUp) => self.settings_panel.prev_item(),
            Key::Named(NamedKey::ArrowDown) => self.settings_panel.next_item(),
            Key::Named(NamedKey::ArrowRight) => self.settings_panel.adjust(1.0),
            Key::Named(NamedKey::ArrowLeft) => self.settings_panel.adjust(-1.0),
            Key::Named(NamedKey::Tab) => self.settings_panel.next_section(),
            _ => {}
        }
        // Apply CRT changes live.
        let snap = self.settings_panel.to_snapshot();
        self.theme.shader_params.scanline_intensity = snap.scanline_intensity;
        self.theme.shader_params.bloom_intensity = snap.bloom_intensity;
        self.theme.shader_params.chromatic_aberration = snap.chromatic_aberration;
        self.theme.shader_params.curvature = snap.curvature;
        self.theme.shader_params.vignette_intensity = snap.vignette_intensity;
        self.theme.shader_params.noise_intensity = snap.noise_intensity;
    }
}
