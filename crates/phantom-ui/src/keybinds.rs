use std::collections::HashMap;
use std::fmt;

// ---------------------------------------------------------------------------
// Action — every bindable command in Phantom
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    Copy,
    Paste,
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    SplitHorizontal,
    SplitVertical,
    FocusNext,
    FocusPrev,
    CloseFocused,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ToggleFullscreen,
    ShowCommandPalette,
    ScrollPageUp,
    ScrollPageDown,
    ScrollToTop,
    ScrollToBottom,
    CycleTheme,
    Quit,
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Copy => "Copy",
            Self::Paste => "Paste",
            Self::NewTab => "New Tab",
            Self::CloseTab => "Close Tab",
            Self::NextTab => "Next Tab",
            Self::PrevTab => "Previous Tab",
            Self::SplitHorizontal => "Split Horizontal",
            Self::SplitVertical => "Split Vertical",
            Self::FocusNext => "Focus Next",
            Self::FocusPrev => "Focus Previous",
            Self::CloseFocused => "Close Focused",
            Self::ZoomIn => "Zoom In",
            Self::ZoomOut => "Zoom Out",
            Self::ZoomReset => "Zoom Reset",
            Self::ToggleFullscreen => "Toggle Fullscreen",
            Self::ShowCommandPalette => "Command Palette",
            Self::ScrollPageUp => "Scroll Page Up",
            Self::ScrollPageDown => "Scroll Page Down",
            Self::ScrollToTop => "Scroll To Top",
            Self::ScrollToBottom => "Scroll To Bottom",
            Self::CycleTheme => "Cycle Theme",
            Self::Quit => "Quit",
        };
        f.write_str(label)
    }
}

// ---------------------------------------------------------------------------
// Key — every bindable physical key
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    F(u8),

    // Navigation
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,

    // Editing
    Backspace,
    Delete,
    Insert,
    Enter,
    Tab,
    Space,
    Escape,
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Char(c) => {
                let upper = c.to_uppercase().next().unwrap_or(*c);
                write!(f, "{upper}")
            }
            Self::F(n) => write!(f, "F{n}"),
            Self::Up => f.write_str("Up"),
            Self::Down => f.write_str("Down"),
            Self::Left => f.write_str("Left"),
            Self::Right => f.write_str("Right"),
            Self::Home => f.write_str("Home"),
            Self::End => f.write_str("End"),
            Self::PageUp => f.write_str("PageUp"),
            Self::PageDown => f.write_str("PageDown"),
            Self::Backspace => f.write_str("Backspace"),
            Self::Delete => f.write_str("Delete"),
            Self::Insert => f.write_str("Insert"),
            Self::Enter => f.write_str("Enter"),
            Self::Tab => f.write_str("Tab"),
            Self::Space => f.write_str("Space"),
            Self::Escape => f.write_str("Escape"),
        }
    }
}

// ---------------------------------------------------------------------------
// KeyCombo — a key plus modifier flags
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    pub key: Key,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
}

impl KeyCombo {
    /// Bare key, no modifiers.
    pub const fn bare(key: Key) -> Self {
        Self {
            key,
            ctrl: false,
            alt: false,
            shift: false,
            logo: false,
        }
    }

    /// Cmd/Super + key.
    pub const fn cmd(key: Key) -> Self {
        Self {
            key,
            ctrl: false,
            alt: false,
            shift: false,
            logo: true,
        }
    }

    /// Cmd/Super + Shift + key.
    pub const fn cmd_shift(key: Key) -> Self {
        Self {
            key,
            ctrl: false,
            alt: false,
            shift: true,
            logo: true,
        }
    }

    /// Ctrl + key.
    pub const fn ctrl(key: Key) -> Self {
        Self {
            key,
            ctrl: true,
            alt: false,
            shift: false,
            logo: false,
        }
    }

    /// Ctrl + Shift + key.
    pub const fn ctrl_shift(key: Key) -> Self {
        Self {
            key,
            ctrl: true,
            alt: false,
            shift: true,
            logo: false,
        }
    }
}

impl fmt::Display for KeyCombo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ctrl {
            f.write_str("Ctrl+")?;
        }
        if self.alt {
            f.write_str("Alt+")?;
        }
        if self.logo {
            f.write_str("Cmd+")?;
        }
        if self.shift {
            f.write_str("Shift+")?;
        }
        write!(f, "{}", self.key)
    }
}

// ---------------------------------------------------------------------------
// KeybindRegistry — intercept layer between raw input and the PTY
// ---------------------------------------------------------------------------

pub struct KeybindRegistry {
    bindings: HashMap<KeyCombo, Action>,
}

impl KeybindRegistry {
    /// Create a registry loaded with the default macOS-centric bindings.
    pub fn new() -> Self {
        let mut registry = Self {
            bindings: HashMap::with_capacity(32),
        };
        registry.load_defaults();
        registry
    }

    /// Look up an action for a given key combo. Returns `None` when the combo
    /// should pass through to the terminal PTY.
    pub fn lookup(&self, combo: &KeyCombo) -> Option<&Action> {
        self.bindings.get(combo)
    }

    /// Bind a key combo to an action, replacing any previous binding on that
    /// combo.
    pub fn bind(&mut self, combo: KeyCombo, action: Action) {
        self.bindings.insert(combo, action);
    }

    /// Remove a binding so the key combo passes through to the PTY.
    pub fn unbind(&mut self, combo: &KeyCombo) {
        self.bindings.remove(combo);
    }

    /// Number of active bindings.
    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    /// Whether the registry has no bindings at all.
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }

    /// Iterator over all current bindings.
    pub fn iter(&self) -> impl Iterator<Item = (&KeyCombo, &Action)> {
        self.bindings.iter()
    }

    /// Reset to the built-in default set, discarding any user overrides.
    pub fn reset_defaults(&mut self) {
        self.bindings.clear();
        self.load_defaults();
    }

    // -----------------------------------------------------------------------
    // Default bindings — macOS-centric (Cmd = logo key)
    // -----------------------------------------------------------------------

    fn load_defaults(&mut self) {
        let defaults: &[(KeyCombo, Action)] = &[
            // Clipboard
            (KeyCombo::cmd(Key::Char('c')), Action::Copy),
            (KeyCombo::cmd(Key::Char('v')), Action::Paste),
            // Tabs
            (KeyCombo::cmd(Key::Char('t')), Action::NewTab),
            (KeyCombo::cmd(Key::Char('w')), Action::CloseTab),
            (KeyCombo::cmd_shift(Key::Char(']')), Action::NextTab),
            (KeyCombo::cmd_shift(Key::Char('[')), Action::PrevTab),
            // Splits
            (KeyCombo::cmd(Key::Char('d')), Action::SplitHorizontal),
            (KeyCombo::cmd_shift(Key::Char('d')), Action::SplitVertical),
            // Focus navigation
            (KeyCombo::cmd(Key::Char(']')), Action::FocusNext),
            (KeyCombo::cmd(Key::Char('[')), Action::FocusPrev),
            // Zoom
            (KeyCombo::cmd(Key::Char('=')), Action::ZoomIn), // =/+ key
            (KeyCombo::cmd(Key::Char('-')), Action::ZoomOut),
            (KeyCombo::cmd(Key::Char('0')), Action::ZoomReset),
            // Scrollback
            (
                KeyCombo {
                    key: Key::PageUp,
                    ctrl: false,
                    alt: false,
                    shift: true,
                    logo: false,
                },
                Action::ScrollPageUp,
            ),
            (
                KeyCombo {
                    key: Key::PageDown,
                    ctrl: false,
                    alt: false,
                    shift: true,
                    logo: false,
                },
                Action::ScrollPageDown,
            ),
            (
                KeyCombo {
                    key: Key::Home,
                    ctrl: false,
                    alt: false,
                    shift: true,
                    logo: false,
                },
                Action::ScrollToTop,
            ),
            (
                KeyCombo {
                    key: Key::End,
                    ctrl: false,
                    alt: false,
                    shift: true,
                    logo: false,
                },
                Action::ScrollToBottom,
            ),
            // Fullscreen
            (
                KeyCombo::ctrl_shift(Key::Char('f')),
                Action::ToggleFullscreen,
            ),
            (KeyCombo::bare(Key::F(11)), Action::ToggleFullscreen),
            // Window
            (
                KeyCombo::cmd_shift(Key::Char('p')),
                Action::ShowCommandPalette,
            ),
            (KeyCombo::cmd(Key::Char('q')), Action::Quit),
        ];

        for (combo, action) in defaults {
            self.bindings.insert(*combo, *action);
        }
    }
}

impl Default for KeybindRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for KeybindRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeybindRegistry")
            .field("binding_count", &self.bindings.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_bindings_present() {
        let reg = KeybindRegistry::new();
        assert!(!reg.is_empty());
        assert!(reg.len() >= 15);
    }

    #[test]
    fn lookup_cmd_c_is_copy() {
        let reg = KeybindRegistry::new();
        let combo = KeyCombo::cmd(Key::Char('c'));
        assert_eq!(reg.lookup(&combo), Some(&Action::Copy));
    }

    #[test]
    fn lookup_cmd_v_is_paste() {
        let reg = KeybindRegistry::new();
        let combo = KeyCombo::cmd(Key::Char('v'));
        assert_eq!(reg.lookup(&combo), Some(&Action::Paste));
    }

    #[test]
    fn lookup_cmd_shift_bracket_tabs() {
        let reg = KeybindRegistry::new();
        assert_eq!(
            reg.lookup(&KeyCombo::cmd_shift(Key::Char(']'))),
            Some(&Action::NextTab),
        );
        assert_eq!(
            reg.lookup(&KeyCombo::cmd_shift(Key::Char('['))),
            Some(&Action::PrevTab),
        );
    }

    #[test]
    fn unbound_combo_returns_none() {
        let reg = KeybindRegistry::new();
        let combo = KeyCombo::ctrl(Key::Char('z'));
        assert_eq!(reg.lookup(&combo), None);
    }

    #[test]
    fn bind_override() {
        let mut reg = KeybindRegistry::new();
        let combo = KeyCombo::cmd(Key::Char('c'));
        reg.bind(combo, Action::Quit);
        assert_eq!(reg.lookup(&combo), Some(&Action::Quit));
    }

    #[test]
    fn unbind_removes() {
        let mut reg = KeybindRegistry::new();
        let combo = KeyCombo::cmd(Key::Char('c'));
        reg.unbind(&combo);
        assert_eq!(reg.lookup(&combo), None);
    }

    #[test]
    fn reset_defaults_restores() {
        let mut reg = KeybindRegistry::new();
        let combo = KeyCombo::cmd(Key::Char('c'));
        reg.unbind(&combo);
        assert_eq!(reg.lookup(&combo), None);
        reg.reset_defaults();
        assert_eq!(reg.lookup(&combo), Some(&Action::Copy));
    }

    #[test]
    fn key_combo_equality() {
        let a = KeyCombo::cmd(Key::Char('t'));
        let b = KeyCombo {
            key: Key::Char('t'),
            ctrl: false,
            alt: false,
            shift: false,
            logo: true,
        };
        assert_eq!(a, b);
    }

    #[test]
    fn display_key_combo() {
        let combo = KeyCombo::cmd_shift(Key::Char('p'));
        let s = format!("{combo}");
        assert_eq!(s, "Cmd+Shift+P");
    }

    #[test]
    fn display_action() {
        assert_eq!(format!("{}", Action::ShowCommandPalette), "Command Palette");
    }

    #[test]
    fn bare_key_no_modifiers() {
        let combo = KeyCombo::bare(Key::Escape);
        assert!(!combo.ctrl && !combo.alt && !combo.shift && !combo.logo);
        assert_eq!(format!("{combo}"), "Escape");
    }

    #[test]
    fn scroll_keybinds_registered() {
        let reg = KeybindRegistry::new();
        let combo = KeyCombo {
            key: Key::PageUp,
            ctrl: false,
            alt: false,
            shift: true,
            logo: false,
        };
        assert_eq!(reg.lookup(&combo), Some(&Action::ScrollPageUp));

        let combo = KeyCombo {
            key: Key::PageDown,
            ctrl: false,
            alt: false,
            shift: true,
            logo: false,
        };
        assert_eq!(reg.lookup(&combo), Some(&Action::ScrollPageDown));

        let combo = KeyCombo {
            key: Key::Home,
            ctrl: false,
            alt: false,
            shift: true,
            logo: false,
        };
        assert_eq!(reg.lookup(&combo), Some(&Action::ScrollToTop));

        let combo = KeyCombo {
            key: Key::End,
            ctrl: false,
            alt: false,
            shift: true,
            logo: false,
        };
        assert_eq!(reg.lookup(&combo), Some(&Action::ScrollToBottom));
    }

    #[test]
    fn iter_yields_all_bindings() {
        let reg = KeybindRegistry::new();
        let count = reg.iter().count();
        assert_eq!(count, reg.len());
    }
}
