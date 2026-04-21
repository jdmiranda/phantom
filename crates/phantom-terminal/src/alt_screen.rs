//! Alternate screen buffer detection.
//!
//! When an interactive program (vim, htop, less, etc.) starts, it switches the
//! terminal into alternate screen mode via `\e[?1049h`. When it exits, it
//! restores the normal screen via `\e[?1049l`. This module exposes a simple
//! predicate to check whether the terminal is currently in that mode.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::Term;

/// Returns `true` if the terminal is currently displaying the alternate screen
/// buffer (i.e. an interactive full-screen program is running).
#[inline]
pub fn is_alt_screen<T: EventListener>(term: &Term<T>) -> bool {
    term.mode().contains(TermMode::ALT_SCREEN)
}

/// Returns `true` if the terminal has the VI mode flag set.
/// (Useful for future tether label differentiation.)
#[inline]
pub fn is_vi_mode<T: EventListener>(term: &Term<T>) -> bool {
    term.mode().contains(TermMode::VI)
}
