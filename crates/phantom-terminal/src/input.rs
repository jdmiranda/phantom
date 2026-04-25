// Keyboard/mouse events -> terminal input bytes
//
// Converts pre-processed key events into VT100/xterm escape sequences
// that get written to the PTY. The main crate maps winit events into
// our own PhantomKey/KeyEvent types, keeping this crate free of windowing deps.

/// Terminal key identifiers, independent of any windowing backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PhantomKey {
    Char(char),
    Enter,
    Backspace,
    Tab,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    Insert,
    F(u8), // F1-F12
}

/// Modifier key state at the time of a key event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PhantomModifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
}

impl PhantomModifiers {
    pub const NONE: Self = Self {
        ctrl: false,
        alt: false,
        shift: false,
        logo: false,
    };

    /// xterm modifier parameter value for CSI sequences.
    /// 1 = none, 2 = shift, 3 = alt, 5 = ctrl, etc.
    /// Only meaningful when at least one modifier is held.
    fn csi_param(self) -> u8 {
        let mut v: u8 = 1;
        if self.shift {
            v += 1;
        }
        if self.alt {
            v += 2;
        }
        if self.ctrl {
            v += 4;
        }
        v
    }

    fn any(self) -> bool {
        self.ctrl || self.alt || self.shift
    }
}

/// A fully-resolved key event ready for encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEvent {
    pub key: PhantomKey,
    pub mods: PhantomModifiers,
}

/// Encode a key event into the byte sequence expected by a VT100/xterm PTY.
pub fn encode_key(event: &KeyEvent) -> Vec<u8> {
    let KeyEvent { key, mods } = event;

    match key {
        // ── Regular characters ──────────────────────────────────────
        PhantomKey::Char(ch) => encode_char(*ch, *mods),

        // ── Simple fixed keys ───────────────────────────────────────
        PhantomKey::Enter => vec![b'\r'],
        PhantomKey::Backspace => {
            if mods.alt {
                vec![0x1b, 0x7f]
            } else {
                vec![0x7f]
            }
        }
        PhantomKey::Tab => {
            if mods.shift {
                b"\x1b[Z".to_vec()
            } else {
                vec![b'\t']
            }
        }
        PhantomKey::Escape => vec![0x1b],

        // ── Arrow keys ──────────────────────────────────────────────
        PhantomKey::Up => encode_arrow(b'A', *mods),
        PhantomKey::Down => encode_arrow(b'B', *mods),
        PhantomKey::Right => encode_arrow(b'C', *mods),
        PhantomKey::Left => encode_arrow(b'D', *mods),

        // ── Home / End ──────────────────────────────────────────────
        PhantomKey::Home => encode_arrow(b'H', *mods),
        PhantomKey::End => encode_arrow(b'F', *mods),

        // ── Page Up / Down, Delete, Insert ──────────────────────────
        PhantomKey::PageUp => encode_tilde(5, *mods),
        PhantomKey::PageDown => encode_tilde(6, *mods),
        PhantomKey::Delete => encode_tilde(3, *mods),
        PhantomKey::Insert => encode_tilde(2, *mods),

        // ── Function keys ───────────────────────────────────────────
        PhantomKey::F(n) => encode_function_key(*n, *mods),
    }
}

/// Encode a paste payload using bracketed paste mode.
pub fn encode_paste(text: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(text.len() + 12);
    buf.extend_from_slice(b"\x1b[200~");
    buf.extend_from_slice(text.as_bytes());
    buf.extend_from_slice(b"\x1b[201~");
    buf
}

// ─── Private helpers ────────────────────────────────────────────────────────

/// Encode a printable character, respecting Ctrl and Alt modifiers.
fn encode_char(ch: char, mods: PhantomModifiers) -> Vec<u8> {
    if mods.ctrl {
        // Ctrl+letter produces byte 1..=26 (Ctrl+A=1, Ctrl+Z=26).
        // Ctrl+[ = 0x1b (ESC), Ctrl+\ = 0x1c, Ctrl+] = 0x1d, etc.
        if ch.is_ascii_alphabetic() {
            let byte = (ch.to_ascii_uppercase() as u8) - b'@';
            if mods.alt {
                return vec![0x1b, byte];
            }
            return vec![byte];
        }
        // Common Ctrl+symbol mappings
        match ch {
            '[' | '3' => return maybe_alt(0x1b, mods), // ESC
            '\\' | '4' => return maybe_alt(0x1c, mods),
            ']' | '5' => return maybe_alt(0x1d, mods),
            '^' | '6' => return maybe_alt(0x1e, mods),
            '_' | '7' => return maybe_alt(0x1f, mods),
            ' ' | '2' | '@' => return maybe_alt(0x00, mods), // NUL
            '/' | '8' => return maybe_alt(0x7f, mods),
            _ => {} // fall through to plain char
        }
    }

    // Alt+key: prefix the character's UTF-8 bytes with ESC.
    let mut buf = Vec::with_capacity(5);
    if mods.alt {
        buf.push(0x1b);
    }

    let mut utf8 = [0u8; 4];
    let encoded = ch.encode_utf8(&mut utf8);
    buf.extend_from_slice(encoded.as_bytes());
    buf
}

/// Wrap a control byte with an optional Alt (ESC) prefix.
fn maybe_alt(byte: u8, mods: PhantomModifiers) -> Vec<u8> {
    if mods.alt {
        vec![0x1b, byte]
    } else {
        vec![byte]
    }
}

/// Encode an arrow / Home / End key: `\x1b[X` or `\x1b[1;{mod}X`.
fn encode_arrow(suffix: u8, mods: PhantomModifiers) -> Vec<u8> {
    if mods.any() {
        let m = mods.csi_param();
        format!("\x1b[1;{m}{}", suffix as char).into_bytes()
    } else {
        vec![0x1b, b'[', suffix]
    }
}

/// Encode a tilde-terminated CSI sequence: `\x1b[N~` or `\x1b[N;{mod}~`.
fn encode_tilde(num: u8, mods: PhantomModifiers) -> Vec<u8> {
    if mods.any() {
        let m = mods.csi_param();
        format!("\x1b[{num};{m}~").into_bytes()
    } else {
        format!("\x1b[{num}~").into_bytes()
    }
}

/// Encode function keys F1-F12 using standard xterm sequences.
///
/// F1-F4  use SS3: `\x1bOP` .. `\x1bOS` (or `\x1b[1;{mod}P` with modifiers)
/// F5-F12 use CSI tilde: `\x1b[15~`, `\x1b[17~`, `\x1b[18~`, `\x1b[19~`,
///                        `\x1b[20~`, `\x1b[21~`, `\x1b[23~`, `\x1b[24~`
fn encode_function_key(n: u8, mods: PhantomModifiers) -> Vec<u8> {
    match n {
        1..=4 => {
            let suffix = b'P' + (n - 1); // P, Q, R, S
            if mods.any() {
                let m = mods.csi_param();
                format!("\x1b[1;{m}{}", suffix as char).into_bytes()
            } else {
                vec![0x1b, b'O', suffix]
            }
        }
        5..=12 => {
            // CSI code for F5..F12 (note the gap: no 16, no 22)
            let code: u8 = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                12 => 24,
                _ => unreachable!(),
            };
            encode_tilde(code, mods)
        }
        _ => Vec::new(), // F13+ not supported
    }
}

// ─── Mouse types and SGR 1006 encoding ─────────────────────────────────────

/// Terminal mouse button identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    ScrollUp,
    ScrollDown,
}

/// Terminal mouse tracking mode (derived from DEC private modes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseMode {
    /// No mouse tracking requested by the running program.
    None,
    /// Basic click tracking (mode 1000).
    Click,
    /// Button-event tracking — clicks + drag with held button (mode 1002).
    Drag,
    /// Any-event tracking — all motion, whether a button is held or not (mode 1003).
    Motion,
}

/// SGR button code for the given mouse button.
fn sgr_button_code(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::ScrollUp => 64,
        MouseButton::ScrollDown => 65,
    }
}

/// Encode a mouse button press or release using SGR 1006 format.
///
/// Format: `\x1b[<Cb;Cx;CyM` (press) or `\x1b[<Cb;Cx;Cym` (release).
/// Coordinates are 1-based.
pub fn encode_mouse_sgr(button: MouseButton, col: u16, row: u16, pressed: bool) -> Vec<u8> {
    let cb = sgr_button_code(button);
    let cx = col.saturating_add(1);
    let cy = row.saturating_add(1);
    let suffix = if pressed { 'M' } else { 'm' };
    format!("\x1b[<{cb};{cx};{cy}{suffix}").into_bytes()
}

/// Encode a mouse motion event using SGR 1006 format.
///
/// Motion events use button code + 32.
/// Format: `\x1b[<Cb;Cx;CyM`.
/// Coordinates are 1-based.
pub fn encode_mouse_motion_sgr(button: MouseButton, col: u16, row: u16) -> Vec<u8> {
    let cb = sgr_button_code(button) + 32;
    let cx = col.saturating_add(1);
    let cy = row.saturating_add(1);
    format!("\x1b[<{cb};{cx};{cy}M").into_bytes()
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn key(k: PhantomKey) -> KeyEvent {
        KeyEvent {
            key: k,
            mods: PhantomModifiers::NONE,
        }
    }

    fn key_ctrl(k: PhantomKey) -> KeyEvent {
        KeyEvent {
            key: k,
            mods: PhantomModifiers {
                ctrl: true,
                ..PhantomModifiers::NONE
            },
        }
    }

    fn key_alt(k: PhantomKey) -> KeyEvent {
        KeyEvent {
            key: k,
            mods: PhantomModifiers {
                alt: true,
                ..PhantomModifiers::NONE
            },
        }
    }

    fn key_shift(k: PhantomKey) -> KeyEvent {
        KeyEvent {
            key: k,
            mods: PhantomModifiers {
                shift: true,
                ..PhantomModifiers::NONE
            },
        }
    }

    fn key_mods(k: PhantomKey, ctrl: bool, alt: bool, shift: bool) -> KeyEvent {
        KeyEvent {
            key: k,
            mods: PhantomModifiers {
                ctrl,
                alt,
                shift,
                logo: false,
            },
        }
    }

    // ── Characters ──────────────────────────────────────────────

    #[test]
    fn plain_ascii() {
        assert_eq!(encode_key(&key(PhantomKey::Char('a'))), b"a");
        assert_eq!(encode_key(&key(PhantomKey::Char('Z'))), b"Z");
        assert_eq!(encode_key(&key(PhantomKey::Char('5'))), b"5");
    }

    #[test]
    fn unicode_char() {
        let e = key(PhantomKey::Char('\u{1F600}')); // grinning face
        assert_eq!(encode_key(&e), "\u{1F600}".as_bytes());
    }

    #[test]
    fn ctrl_letters() {
        // Ctrl+A = 0x01, Ctrl+C = 0x03, Ctrl+Z = 0x1A
        assert_eq!(encode_key(&key_ctrl(PhantomKey::Char('a'))), vec![0x01]);
        assert_eq!(encode_key(&key_ctrl(PhantomKey::Char('c'))), vec![0x03]);
        assert_eq!(encode_key(&key_ctrl(PhantomKey::Char('z'))), vec![0x1a]);
        // uppercase letters behave the same
        assert_eq!(encode_key(&key_ctrl(PhantomKey::Char('C'))), vec![0x03]);
    }

    #[test]
    fn alt_char() {
        assert_eq!(encode_key(&key_alt(PhantomKey::Char('x'))), b"\x1bx");
    }

    #[test]
    fn ctrl_alt_char() {
        let e = key_mods(PhantomKey::Char('c'), true, true, false);
        assert_eq!(encode_key(&e), vec![0x1b, 0x03]);
    }

    // ── Simple keys ─────────────────────────────────────────────

    #[test]
    fn enter() {
        assert_eq!(encode_key(&key(PhantomKey::Enter)), b"\r");
    }

    #[test]
    fn backspace() {
        assert_eq!(encode_key(&key(PhantomKey::Backspace)), vec![0x7f]);
    }

    #[test]
    fn alt_backspace() {
        assert_eq!(
            encode_key(&key_alt(PhantomKey::Backspace)),
            vec![0x1b, 0x7f]
        );
    }

    #[test]
    fn tab_and_shift_tab() {
        assert_eq!(encode_key(&key(PhantomKey::Tab)), b"\t");
        assert_eq!(encode_key(&key_shift(PhantomKey::Tab)), b"\x1b[Z");
    }

    #[test]
    fn escape_key() {
        assert_eq!(encode_key(&key(PhantomKey::Escape)), vec![0x1b]);
    }

    // ── Arrow keys ──────────────────────────────────────────────

    #[test]
    fn arrows_plain() {
        assert_eq!(encode_key(&key(PhantomKey::Up)), b"\x1b[A");
        assert_eq!(encode_key(&key(PhantomKey::Down)), b"\x1b[B");
        assert_eq!(encode_key(&key(PhantomKey::Right)), b"\x1b[C");
        assert_eq!(encode_key(&key(PhantomKey::Left)), b"\x1b[D");
    }

    #[test]
    fn arrows_with_modifiers() {
        // Shift+Up = \x1b[1;2A
        assert_eq!(
            encode_key(&key_shift(PhantomKey::Up)),
            b"\x1b[1;2A"
        );
        // Ctrl+Right = \x1b[1;5C
        assert_eq!(
            encode_key(&key_ctrl(PhantomKey::Right)),
            b"\x1b[1;5C"
        );
        // Ctrl+Shift+Left = \x1b[1;6D
        assert_eq!(
            encode_key(&key_mods(PhantomKey::Left, true, false, true)),
            b"\x1b[1;6D"
        );
        // Ctrl+Alt+Shift+Down = \x1b[1;8B
        assert_eq!(
            encode_key(&key_mods(PhantomKey::Down, true, true, true)),
            b"\x1b[1;8B"
        );
    }

    // ── Home / End ──────────────────────────────────────────────

    #[test]
    fn home_end() {
        assert_eq!(encode_key(&key(PhantomKey::Home)), b"\x1b[H");
        assert_eq!(encode_key(&key(PhantomKey::End)), b"\x1b[F");
    }

    #[test]
    fn home_with_ctrl() {
        assert_eq!(
            encode_key(&key_ctrl(PhantomKey::Home)),
            b"\x1b[1;5H"
        );
    }

    // ── Page / Delete / Insert ──────────────────────────────────

    #[test]
    fn page_up_down() {
        assert_eq!(encode_key(&key(PhantomKey::PageUp)), b"\x1b[5~");
        assert_eq!(encode_key(&key(PhantomKey::PageDown)), b"\x1b[6~");
    }

    #[test]
    fn delete_insert() {
        assert_eq!(encode_key(&key(PhantomKey::Delete)), b"\x1b[3~");
        assert_eq!(encode_key(&key(PhantomKey::Insert)), b"\x1b[2~");
    }

    #[test]
    fn delete_with_shift() {
        assert_eq!(
            encode_key(&key_shift(PhantomKey::Delete)),
            b"\x1b[3;2~"
        );
    }

    // ── Function keys ───────────────────────────────────────────

    #[test]
    fn f1_through_f4() {
        assert_eq!(encode_key(&key(PhantomKey::F(1))), b"\x1bOP");
        assert_eq!(encode_key(&key(PhantomKey::F(2))), b"\x1bOQ");
        assert_eq!(encode_key(&key(PhantomKey::F(3))), b"\x1bOR");
        assert_eq!(encode_key(&key(PhantomKey::F(4))), b"\x1bOS");
    }

    #[test]
    fn f5_through_f12() {
        assert_eq!(encode_key(&key(PhantomKey::F(5))), b"\x1b[15~");
        assert_eq!(encode_key(&key(PhantomKey::F(6))), b"\x1b[17~");
        assert_eq!(encode_key(&key(PhantomKey::F(7))), b"\x1b[18~");
        assert_eq!(encode_key(&key(PhantomKey::F(8))), b"\x1b[19~");
        assert_eq!(encode_key(&key(PhantomKey::F(9))), b"\x1b[20~");
        assert_eq!(encode_key(&key(PhantomKey::F(10))), b"\x1b[21~");
        assert_eq!(encode_key(&key(PhantomKey::F(11))), b"\x1b[23~");
        assert_eq!(encode_key(&key(PhantomKey::F(12))), b"\x1b[24~");
    }

    #[test]
    fn f1_with_shift() {
        // Shift+F1 = \x1b[1;2P
        assert_eq!(
            encode_key(&key_shift(PhantomKey::F(1))),
            b"\x1b[1;2P"
        );
    }

    #[test]
    fn f5_with_ctrl() {
        assert_eq!(
            encode_key(&key_ctrl(PhantomKey::F(5))),
            b"\x1b[15;5~"
        );
    }

    #[test]
    fn f_out_of_range() {
        assert_eq!(encode_key(&key(PhantomKey::F(0))), b"");
        assert_eq!(encode_key(&key(PhantomKey::F(13))), b"");
    }

    // ── Modifier parameter encoding ─────────────────────────────

    #[test]
    fn modifier_param_values() {
        let none = PhantomModifiers::NONE;
        assert_eq!(none.csi_param(), 1);

        let shift = PhantomModifiers { shift: true, ..PhantomModifiers::NONE };
        assert_eq!(shift.csi_param(), 2);

        let alt = PhantomModifiers { alt: true, ..PhantomModifiers::NONE };
        assert_eq!(alt.csi_param(), 3);

        let alt_shift = PhantomModifiers { alt: true, shift: true, ..PhantomModifiers::NONE };
        assert_eq!(alt_shift.csi_param(), 4);

        let ctrl = PhantomModifiers { ctrl: true, ..PhantomModifiers::NONE };
        assert_eq!(ctrl.csi_param(), 5);

        let ctrl_shift = PhantomModifiers { ctrl: true, shift: true, ..PhantomModifiers::NONE };
        assert_eq!(ctrl_shift.csi_param(), 6);

        let ctrl_alt = PhantomModifiers { ctrl: true, alt: true, ..PhantomModifiers::NONE };
        assert_eq!(ctrl_alt.csi_param(), 7);

        let all = PhantomModifiers { ctrl: true, alt: true, shift: true, logo: false };
        assert_eq!(all.csi_param(), 8);
    }

    // ── Paste ───────────────────────────────────────────────────

    #[test]
    fn bracketed_paste() {
        let result = encode_paste("hello");
        assert_eq!(result, b"\x1b[200~hello\x1b[201~");
    }

    #[test]
    fn bracketed_paste_empty() {
        let result = encode_paste("");
        assert_eq!(result, b"\x1b[200~\x1b[201~");
    }

    #[test]
    fn bracketed_paste_multiline() {
        let result = encode_paste("line1\nline2");
        assert_eq!(result, b"\x1b[200~line1\nline2\x1b[201~");
    }

    // ── Mouse SGR encoding ─────────────────────────────────────

    #[test]
    fn sgr_left_press() {
        // Left button press at col=0, row=0 → \x1b[<0;1;1M
        let bytes = encode_mouse_sgr(MouseButton::Left, 0, 0, true);
        assert_eq!(bytes, b"\x1b[<0;1;1M");
    }

    #[test]
    fn sgr_left_release() {
        let bytes = encode_mouse_sgr(MouseButton::Left, 5, 10, false);
        assert_eq!(bytes, b"\x1b[<0;6;11m");
    }

    #[test]
    fn sgr_right_press() {
        let bytes = encode_mouse_sgr(MouseButton::Right, 79, 23, true);
        assert_eq!(bytes, b"\x1b[<2;80;24M");
    }

    #[test]
    fn sgr_middle_press() {
        let bytes = encode_mouse_sgr(MouseButton::Middle, 0, 0, true);
        assert_eq!(bytes, b"\x1b[<1;1;1M");
    }

    #[test]
    fn sgr_scroll_up() {
        let bytes = encode_mouse_sgr(MouseButton::ScrollUp, 10, 5, true);
        assert_eq!(bytes, b"\x1b[<64;11;6M");
    }

    #[test]
    fn sgr_scroll_down() {
        let bytes = encode_mouse_sgr(MouseButton::ScrollDown, 10, 5, true);
        assert_eq!(bytes, b"\x1b[<65;11;6M");
    }

    #[test]
    fn sgr_motion_left_held() {
        // Motion with left button held: code = 0 + 32 = 32
        let bytes = encode_mouse_motion_sgr(MouseButton::Left, 20, 10);
        assert_eq!(bytes, b"\x1b[<32;21;11M");
    }

    #[test]
    fn sgr_motion_right_held() {
        let bytes = encode_mouse_motion_sgr(MouseButton::Right, 0, 0);
        assert_eq!(bytes, b"\x1b[<34;1;1M");
    }
}
