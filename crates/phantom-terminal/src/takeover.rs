//! Interactive subprocess takeover detection.
//!
//! A "takeover candidate" is a subprocess that has taken exclusive control of
//! the terminal — typically signaled by alternate-screen entry (`CSI ?1049h`)
//! or, on the fallback path, by a process name that matches the known
//! interactive-program list.
//!
//! This module is **detection only**. It does not split panes, reparent PTYs,
//! or perform any UI action. Consumers (currently `phantom-app`) observe the
//! structured [`TakeoverCandidate`] value and decide what to do with it.
//!
//! # Signal hierarchy
//!
//! 1. **Alt-screen** (`CSI ?1049h` / `TermMode::ALT_SCREEN`): most reliable
//!    signal; virtually all full-screen programs use it.
//! 2. **Known-program list** (`KNOWN_INTERACTIVE`): fallback for programs that
//!    may not use alt-screen (e.g. some pager configurations). Only active when
//!    alt-screen is *not* set.
//!
//! # Usage
//!
//! ```rust,ignore
//! use phantom_terminal::takeover::{TakeoverCandidate, TakeoverDetector, TakeoverEvent};
//!
//! let mut detector = TakeoverDetector::default();
//! // Call once per frame after pty_read().
//! if let TakeoverEvent::Detected(candidate) = detector.tick(term.term(), term.pty_fd()) {
//!     // candidate.signal tells you why it triggered
//! }
//! ```

use std::os::unix::io::AsRawFd;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::Term;

use crate::process::foreground_process_name;

// ---------------------------------------------------------------------------
// Known interactive program names (fallback list)
// ---------------------------------------------------------------------------

/// Programs classified as interactive even when alt-screen is not set.
///
/// The list is intentionally short. False positives are bounded because the
/// primary alt-screen signal covers the vast majority of cases; these names
/// only fire when alt-screen is absent.
static KNOWN_INTERACTIVE: &[&str] = &[
    "vim", "vi", "nvim", "nano", "emacs", "pico", "joe", "micro", "helix", "hx", "kakoune",
    "kak", "less", "more", "most", "htop", "btop", "top", "atop", "glances", "nnn", "ranger",
    "lf", "mc", "tig", "lazygit", "fzf", "gum", "charm", "claude", "gemini", "aider",
    "python", "python3", "ipython", "irb", "iex", "node", "lua", "ghci", "sbcl", "mit-scheme",
    "mysql", "psql", "sqlite3", "redis-cli", "mongosh",
    "ssh", "mosh", "telnet",
    "man",
];

// ---------------------------------------------------------------------------
// TakeoverSignal — why did detection fire?
// ---------------------------------------------------------------------------

/// The primary signal that caused a subprocess to be classified as a candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TakeoverSignal {
    /// The subprocess entered the alternate screen buffer (`CSI ?1049h`).
    AltScreen,
    /// The foreground process name matched the known-interactive list
    /// (fallback; fires only when `AltScreen` is not set).
    KnownProgram,
}

// ---------------------------------------------------------------------------
// TakeoverCandidate — structured event payload
// ---------------------------------------------------------------------------

/// A structured description of a subprocess that has taken over the terminal.
///
/// Produced by [`TakeoverDetector::tick`] on a rising edge (candidate appeared)
/// and consumed by `phantom-app` to emit bus events and, optionally, split panes.
///
/// The payload is intentionally rich so that the pane lineage model (#365) can
/// record parent → child relationships without needing to re-query the PTY:
///
/// - `app_id` identifies the *parent* terminal pane (filled in by the adapter
///   layer, not by this module — see `TerminalAdapter`).
/// - `pid` / `process_name` identify the child subprocess.
/// - `signal` explains which detection path fired.
#[derive(Debug, Clone)]
pub struct TakeoverCandidate {
    /// The process name of the foreground subprocess (e.g. `"vim"`, `"htop"`).
    ///
    /// `None` when the name could not be resolved (e.g. the process exited
    /// faster than we could query it, or the platform is unsupported).
    pub process_name: Option<String>,

    /// Foreground process-group ID obtained via `TIOCGPGRP`.
    ///
    /// `None` when the ioctl failed (e.g. closed PTY).
    pub pgid: Option<i32>,

    /// Which signal caused the candidate to be emitted.
    pub signal: TakeoverSignal,
}

// ---------------------------------------------------------------------------
// TakeoverEvent — the result of a single tick
// ---------------------------------------------------------------------------

/// The result of a single [`TakeoverDetector::tick`] call.
#[derive(Debug)]
pub enum TakeoverEvent {
    /// Rising edge: a subprocess just entered takeover state.
    Detected(TakeoverCandidate),
    /// Falling edge: the takeover condition just cleared.
    Cleared,
    /// No edge this frame (condition unchanged).
    None,
}

// ---------------------------------------------------------------------------
// TakeoverDetector — per-terminal state machine
// ---------------------------------------------------------------------------

/// Per-terminal edge-detector for subprocess takeovers.
///
/// Call [`tick`](TakeoverDetector::tick) **once per frame** (after
/// `pty_read`). It returns a [`TakeoverEvent`] describing any edge that
/// occurred this frame.
///
/// Both rising and falling edges are observable from the single `tick` call —
/// this avoids the double-advance bug that would occur if `poll` and
/// `poll_clear` were called sequentially on separate methods that each update
/// internal state.
#[derive(Debug, Default)]
pub struct TakeoverDetector {
    /// Whether the terminal was in takeover state last frame.
    was_takeover: bool,
}

impl TakeoverDetector {
    /// Advance the detector by one frame and return any edge event.
    ///
    /// Call this exactly **once per frame** after `pty_read()`. The returned
    /// [`TakeoverEvent`] describes what happened:
    ///
    /// - [`TakeoverEvent::Detected`] — rising edge, subprocess just took over.
    /// - [`TakeoverEvent::Cleared`] — falling edge, takeover just ended.
    /// - [`TakeoverEvent::None`] — no state change.
    pub fn tick<T, F>(&mut self, term: &Term<T>, pty_fd: &F) -> TakeoverEvent
    where
        T: EventListener,
        F: AsRawFd,
    {
        let (is_takeover, candidate) = Self::classify(term, pty_fd);

        let rising = is_takeover && !self.was_takeover;
        let falling = !is_takeover && self.was_takeover;
        self.was_takeover = is_takeover;

        if rising {
            // SAFETY: `classify` returns `(true, Some(_))` together — every code
            // path in `classify` that yields `is_takeover == true` (alt-screen
            // branch and known-program fallback branch) constructs a
            // `Some(TakeoverCandidate)` in the same tuple. The `(true, None)`
            // shape is unreachable, so `expect` here is safe.
            TakeoverEvent::Detected(candidate.expect("classify guarantees Some on takeover"))
        } else if falling {
            TakeoverEvent::Cleared
        } else {
            TakeoverEvent::None
        }
    }

    /// Whether the terminal is currently in a takeover state.
    ///
    /// Reflects the state after the most recent [`tick`](TakeoverDetector::tick)
    /// call. Safe to query at any time for a point-in-time read.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.was_takeover
    }

    // -- Internal ------------------------------------------------------------

    /// Classify the current terminal state.
    ///
    /// Returns `(is_takeover, candidate_if_rising)`. The candidate is
    /// `None` when `is_takeover` is false or when we are not on a rising edge
    /// (caller decides which frame to emit based on `was_takeover`).
    fn classify<T, F>(term: &Term<T>, pty_fd: &F) -> (bool, Option<TakeoverCandidate>)
    where
        T: EventListener,
        F: AsRawFd,
    {
        // Signal 1: alt-screen (most reliable).
        if term.mode().contains(TermMode::ALT_SCREEN) {
            let (pgid, process_name) = query_foreground(pty_fd);
            return (
                true,
                Some(TakeoverCandidate {
                    process_name,
                    pgid,
                    signal: TakeoverSignal::AltScreen,
                }),
            );
        }

        // Signal 2: known-program fallback (only when alt-screen is absent).
        let (pgid, process_name) = query_foreground(pty_fd);
        if let Some(ref name) = process_name
            && KNOWN_INTERACTIVE.contains(&name.as_str())
        {
            return (
                true,
                Some(TakeoverCandidate {
                    process_name: Some(name.clone()),
                    pgid,
                    signal: TakeoverSignal::KnownProgram,
                }),
            );
        }

        (false, None)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Query the PTY for the foreground process group ID and process name.
///
/// Returns `(None, None)` on any failure (invalid fd, platform unsupported).
fn query_foreground<F: AsRawFd>(pty_fd: &F) -> (Option<i32>, Option<String>) {
    let fd = pty_fd.as_raw_fd();
    let mut pgid: libc::pid_t = 0;
    // SAFETY: TIOCGPGRP is safe on a valid PTY master fd.
    let res = unsafe { libc::ioctl(fd, libc::TIOCGPGRP, &mut pgid as *mut _) };
    if res < 0 || pgid <= 0 {
        return (None, None);
    }
    let name = foreground_process_name(pty_fd);
    (Some(pgid), name)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_interactive_list_contains_common_programs() {
        assert!(KNOWN_INTERACTIVE.contains(&"vim"));
        assert!(KNOWN_INTERACTIVE.contains(&"htop"));
        assert!(KNOWN_INTERACTIVE.contains(&"less"));
        assert!(KNOWN_INTERACTIVE.contains(&"ssh"));
        assert!(KNOWN_INTERACTIVE.contains(&"claude"));
    }

    #[test]
    fn takeover_signal_is_debug_and_eq() {
        let a = TakeoverSignal::AltScreen;
        let b = TakeoverSignal::KnownProgram;
        assert_ne!(a, b);
        assert_eq!(format!("{a:?}"), "AltScreen");
        assert_eq!(format!("{b:?}"), "KnownProgram");
    }

    #[test]
    fn takeover_candidate_debug() {
        let c = TakeoverCandidate {
            process_name: Some("vim".into()),
            pgid: Some(1234),
            signal: TakeoverSignal::AltScreen,
        };
        let s = format!("{c:?}");
        assert!(s.contains("vim"));
        assert!(s.contains("1234"));
        assert!(s.contains("AltScreen"));
    }

    #[test]
    fn detector_default_is_not_active() {
        let d = TakeoverDetector::default();
        assert!(!d.is_active());
    }

    // The following tests exercise the edge-detection logic by driving the
    // internal `was_takeover` state directly (white-box). `Term` requires a
    // real PTY to construct; these tests validate the state machine contract
    // without needing one.

    // Helper: simulate one frame of the tick() edge-detection logic.
    // Returns (rising_edge, falling_edge, new_was_takeover).
    fn edge_tick(was_takeover: bool, is_takeover: bool) -> (bool, bool, bool) {
        let rising = is_takeover && !was_takeover;
        let falling = !is_takeover && was_takeover;
        (rising, falling, is_takeover)
    }

    /// Verify the rising-edge contract: tick() reports Detected only on frame 1.
    #[test]
    fn rising_edge_fires_exactly_once() {
        // Frame 0 → 1: no takeover → takeover (rising edge)
        let (rising, falling, was) = edge_tick(false, true);
        assert!(rising, "frame 1 must be a rising edge");
        assert!(!falling, "frame 1 must not be a falling edge");

        // Frame 1 → 2: still in takeover (no edge)
        let (rising, falling, _) = edge_tick(was, true);
        assert!(!rising, "frame 2 must not be a rising edge");
        assert!(!falling, "frame 2 must not be a falling edge");
    }

    /// Verify the falling-edge contract: tick() reports Cleared exactly once.
    #[test]
    fn falling_edge_fires_exactly_once() {
        // Frame 0 → 1: takeover → no takeover (falling edge)
        let (rising, falling, was) = edge_tick(true, false);
        assert!(!rising, "frame 1 must not be a rising edge");
        assert!(falling, "frame 1 must be a falling edge");

        // Frame 1 → 2: still no takeover (no edge)
        let (rising, falling, _) = edge_tick(was, false);
        assert!(!rising, "frame 2 must not be a rising edge");
        assert!(!falling, "frame 2 must not be a falling edge");
    }

    /// Verify that a single frame can produce at most one edge (not both).
    #[test]
    fn tick_produces_at_most_one_edge_per_frame() {
        // Rising edge scenario
        let (rising, falling, _) = edge_tick(false, true);
        assert!(!(rising && falling), "cannot have both edges in the same frame");

        // Falling edge scenario
        let (rising, falling, _) = edge_tick(true, false);
        assert!(!(rising && falling), "cannot have both edges in the same frame");

        // No-change scenario (staying active)
        let (rising, falling, _) = edge_tick(true, true);
        assert!(!(rising && falling), "cannot have both edges in the same frame");
    }

    /// Non-interactive shell processes must NOT trigger (false-positive guard).
    #[test]
    fn shell_names_not_in_known_interactive_list() {
        assert!(!KNOWN_INTERACTIVE.contains(&"bash"));
        assert!(!KNOWN_INTERACTIVE.contains(&"zsh"));
        assert!(!KNOWN_INTERACTIVE.contains(&"fish"));
        assert!(!KNOWN_INTERACTIVE.contains(&"sh"));
        assert!(!KNOWN_INTERACTIVE.contains(&"dash"));
    }
}
