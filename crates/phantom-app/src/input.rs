//! Keyboard input handling and winit key conversion for Phantom.
//!
//! Contains `handle_key`, `handle_key_with_mods`, `dispatch_action`,
//! command mode / debug HUD key handlers, and all winit → Phantom key
//! conversion functions.

use std::time::Instant;

use log::{debug, info, warn};
use winit::keyboard::{Key, NamedKey};

use phantom_terminal::input::{self, KeyEvent, PhantomKey, PhantomModifiers};
use phantom_ui::keybinds::{Action, KeyCombo};
use phantom_ui::keybinds::Key as UiKey;

use crate::app::{App, AppState};

// ---------------------------------------------------------------------------
// Key conversion (free functions)
// ---------------------------------------------------------------------------

/// Convert a winit `KeyEvent` to a `KeyCombo` for keybind registry lookup.
fn winit_key_to_combo(event: &winit::event::KeyEvent) -> Option<KeyCombo> {
    let ui_key = winit_logical_to_ui_key(&event.logical_key)?;
    Some(KeyCombo::bare(ui_key))
}

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
    /// Handle a winit keyboard event.
    pub fn handle_key(&mut self, event: winit::event::KeyEvent) {
        if !event.state.is_pressed() {
            return;
        }

        self.last_input_time = Instant::now();

        if self.suggestion.is_some() {
            if let Key::Character(ref s) = event.logical_key {
                if let Some(ch) = s.chars().next() {
                    let ch_lower = ch.to_ascii_lowercase();
                    let matched = self.suggestion.as_ref()
                        .and_then(|s| s.options.iter().find(|(k, _)| *k == ch_lower).map(|(_, v)| v.clone()));
                    if let Some(action_text) = matched {
                        info!("[PHANTOM]: User chose: {action_text}");
                        self.suggestion = None;
                        return;
                    }
                }
            }
            self.suggestion = None;
        }

        if self.debug_hud {
            self.handle_debug_hud_key(&event);
            return;
        }

        if self.command_mode {
            self.handle_command_mode_key(&event);
            return;
        }

        if matches!(&event.logical_key, Key::Character(s) if s.as_str() == "`") {
            self.command_mode = true;
            self.command_input = Some(String::new());
            debug!("Command mode activated");
            return;
        }

        if let Some(combo) = winit_key_to_combo(&event) {
            if let Some(action) = self.keybinds.lookup(&combo) {
                self.dispatch_action(*action);
                return;
            }
        }

        if self.state == AppState::Boot {
            if self.boot.is_waiting() {
                self.boot.dismiss();
            } else {
                self.boot.skip();
            }
            return;
        }

        if let Some(terminal_event) = winit_key_to_terminal(&event) {
            let bytes = input::encode_key(&terminal_event);
            if !bytes.is_empty() {
                if let Some(pane) = self.panes.get_mut(self.focused_pane) {
                    if let Err(e) = pane.terminal.pty_write(&bytes) {
                        warn!("PTY write failed: {e}");
                    }
                }
            }
        }
    }

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
                if !self.panes.is_empty() {
                    self.focused_pane = (self.focused_pane + 1) % self.panes.len();
                    debug!("Focus next: pane {}", self.focused_pane);
                }
            }
            Action::FocusPrev => {
                if !self.panes.is_empty() {
                    self.focused_pane = (self.focused_pane + self.panes.len() - 1) % self.panes.len();
                    debug!("Focus prev: pane {}", self.focused_pane);
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
            Action::ScrollPageUp => {
                if let Some(pane) = self.panes.get_mut(self.focused_pane) {
                    pane.terminal.scroll_page_up();
                }
            }
            Action::ScrollPageDown => {
                if let Some(pane) = self.panes.get_mut(self.focused_pane) {
                    pane.terminal.scroll_page_down();
                }
            }
            Action::ScrollToTop => {
                if let Some(pane) = self.panes.get_mut(self.focused_pane) {
                    pane.terminal.scroll_to_top();
                }
            }
            Action::ScrollToBottom => {
                if let Some(pane) = self.panes.get_mut(self.focused_pane) {
                    pane.terminal.scroll_to_bottom();
                }
            }
            _ => {
                debug!("Action: {action} (not yet implemented)");
            }
        }
    }

    /// Handle keys when command mode is active.
    pub(crate) fn handle_command_mode_key(&mut self, event: &winit::event::KeyEvent) {
        match &event.logical_key {
            Key::Named(NamedKey::Escape) => {
                debug!("Command mode cancelled");
                self.command_mode = false;
                self.command_input = None;
            }
            Key::Named(NamedKey::Enter) => {
                let input = self.command_input.take().unwrap_or_default();
                self.command_mode = false;
                if !input.is_empty() {
                    self.execute_user_command(&input);
                }
            }
            Key::Named(NamedKey::Backspace) => {
                if let Some(ref mut buf) = self.command_input {
                    buf.pop();
                }
            }
            Key::Character(s) => {
                if let Some(ref mut buf) = self.command_input {
                    buf.push_str(s.as_str());
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

        if self.suggestion.is_some() {
            self.suggestion = None;
        }

        if self.debug_hud {
            self.handle_debug_hud_key(&event);
            return;
        }

        if self.command_mode {
            self.handle_command_mode_key(&event);
            return;
        }

        if !modifiers.state().control_key()
            && !modifiers.state().alt_key()
            && !modifiers.state().super_key()
        {
            if matches!(&event.logical_key, Key::Character(s) if s.as_str() == "`") {
                self.command_mode = true;
                self.command_input = Some(String::new());
                debug!("Command mode activated");
                return;
            }
        }

        if let Some(combo) = winit_key_to_combo_with_mods(&event, modifiers) {
            if let Some(action) = self.keybinds.lookup(&combo) {
                self.dispatch_action(*action);
                return;
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

            let bytes = input::encode_key(&terminal_event);
            if !bytes.is_empty() {
                if let Some(pane) = self.panes.get_mut(self.focused_pane) {
                    if let Err(e) = pane.terminal.pty_write(&bytes) {
                        warn!("PTY write failed: {e}");
                    }
                }
            }
        }
    }
}
