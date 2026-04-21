//! Boot sequence animation for Phantom.
//!
//! A data-driven, cinematic startup sequence that plays before handing off to
//! the terminal. The [`BootSequence`] state machine is purely logical -- it
//! tracks elapsed time, current phase, and what text/effects should be visible
//! at any given moment. The app layer reads this state each frame and drives
//! the GPU renderers (QuadRenderer, TextRenderer, PostFxPipeline) accordingly.
//!
//! # Timeline
//!
//! | Phase                | Window       | Description                            |
//! |----------------------|--------------|----------------------------------------|
//! | `BlackScreen`        | 0.0 -- 0.3s  | Blank with blinking cursor             |
//! | `CrtWarmup`          | 0.3 -- 0.8s  | CRT effects fade in, phosphor glow     |
//! | `LogoReveal`         | 0.8 -- 1.2s  | ASCII PHANTOM logo types out           |
//! | `SystemCheck`        | 1.2 -- 2.5s  | Status lines appear one by one         |
//! | `Welcome`            | 2.5 -- 3.5s  | Welcome message, brief pause           |
//! | `TransitionToTerminal` | 3.5 -- 4.0s | Fade/transition to live terminal     |
//! | `Done`               | 4.0s+        | Boot complete, hand off                |

// ---------------------------------------------------------------------------
// Phase boundaries (seconds)
// ---------------------------------------------------------------------------

const T_BLACK_END: f32 = 0.3;
const T_WARMUP_END: f32 = 0.8;
const T_LOGO_END: f32 = 1.2;
const T_SYSCHECK_END: f32 = 2.5;
const T_WELCOME_END: f32 = 3.5;
const T_TRANSITION_END: f32 = 4.0;

/// Cursor blink period in seconds (on + off cycle).
const CURSOR_BLINK_PERIOD: f32 = 0.6;

/// Characters typed per second for the typewriter effect.
const TYPE_SPEED: f32 = 280.0;

/// Delay between successive system-check lines (seconds).
const SYSCHECK_LINE_DELAY: f32 = 0.22;

// ---------------------------------------------------------------------------
// Colors (linear RGBA)
// ---------------------------------------------------------------------------

/// Phosphor green -- the signature Phantom color.
const GREEN: [f32; 4] = [0.0, 1.0, 0.38, 1.0];

/// Dimmed green for secondary text.
const GREEN_DIM: [f32; 4] = [0.0, 0.65, 0.25, 0.7];

/// Bright white-green for emphasis.
const GREEN_BRIGHT: [f32; 4] = [0.55, 1.0, 0.72, 1.0];

/// Status bracket green.
const STATUS_GREEN: [f32; 4] = [0.0, 1.0, 0.38, 1.0];

// ---------------------------------------------------------------------------
// ASCII logo
// ---------------------------------------------------------------------------

const LOGO_LINES: &[&str] = &[
    " \u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557} \u{2588}\u{2588}\u{2557}  \u{2588}\u{2588}\u{2557} \u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557} \u{2588}\u{2588}\u{2588}\u{2557}   \u{2588}\u{2588}\u{2557}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557} \u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557} \u{2588}\u{2588}\u{2588}\u{2557}   \u{2588}\u{2588}\u{2588}\u{2557}",
    " \u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2588}\u{2588}\u{2557}\u{2588}\u{2588}\u{2551}  \u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2588}\u{2588}\u{2557}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557}  \u{2588}\u{2588}\u{2551}\u{255a}\u{2550}\u{2550}\u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{255d}\u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2550}\u{2588}\u{2588}\u{2557}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557} \u{2588}\u{2588}\u{2588}\u{2588}\u{2551}",
    " \u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2554}\u{255d}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2554}\u{2588}\u{2588}\u{2557} \u{2588}\u{2588}\u{2551}   \u{2588}\u{2588}\u{2551}   \u{2588}\u{2588}\u{2551}   \u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2554}\u{2588}\u{2588}\u{2588}\u{2588}\u{2554}\u{2588}\u{2588}\u{2551}",
    " \u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2550}\u{255d} \u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2551}\u{255a}\u{2588}\u{2588}\u{2557}\u{2588}\u{2588}\u{2551}   \u{2588}\u{2588}\u{2551}   \u{2588}\u{2588}\u{2551}   \u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2551}\u{255a}\u{2588}\u{2588}\u{2554}\u{255d}\u{2588}\u{2588}\u{2551}",
    " \u{2588}\u{2588}\u{2551}     \u{2588}\u{2588}\u{2551}  \u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2551}  \u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2551} \u{255a}\u{2588}\u{2588}\u{2588}\u{2588}\u{2551}   \u{2588}\u{2588}\u{2551}   \u{255a}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2554}\u{255d}\u{2588}\u{2588}\u{2551} \u{255a}\u{2550}\u{255d} \u{2588}\u{2588}\u{2551}",
    " \u{255a}\u{2550}\u{255d}     \u{255a}\u{2550}\u{255d}  \u{255a}\u{2550}\u{255d}\u{255a}\u{2550}\u{255d}  \u{255a}\u{2550}\u{255d}\u{255a}\u{2550}\u{255d}  \u{255a}\u{2550}\u{2550}\u{2550}\u{255d}   \u{255a}\u{2550}\u{255d}    \u{255a}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{255d} \u{255a}\u{2550}\u{255d}     \u{255a}\u{2550}\u{255d}",
    "                        v0.1.0",
];

/// Starting row for the logo (centered-ish on an 80x24 terminal).
const LOGO_START_ROW: usize = 4;

// ---------------------------------------------------------------------------
// System check lines
// ---------------------------------------------------------------------------

const SYSCHECK_LINES: &[&str] = &[
    "[ OK ] Phantom Engine .......................... online",
    "[ OK ] Context Engine .......................... ready",
    "[ OK ] Agent Runtime ........................... 5 slots ready",
    "[ OK ] Shader Pipeline ......................... CRT active",
    "[ OK ] Session ................................. new",
];

/// Row offset from top for the first system-check line (after logo + gap).
const SYSCHECK_START_ROW: usize = LOGO_START_ROW + LOGO_LINES.len() + 2;

const WELCOME_TEXT: &str = "Welcome to Phantom.";
const WELCOME_ROW: usize = SYSCHECK_START_ROW + SYSCHECK_LINES.len() + 2;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Visual style hint for a boot text line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LineStyle {
    /// Plain informational text.
    Normal,
    /// System-check status line (always passes in the boot sequence).
    Status,
    /// ASCII art logo line.
    Logo,
}

/// A single line of text to render during the boot sequence.
#[derive(Clone, Debug)]
pub struct BootTextLine {
    /// The full text content of this line.
    pub text: String,
    /// RGBA color (linear).
    pub color: [f32; 4],
    /// Row position (0-indexed from top of screen).
    pub row: usize,
    /// Number of characters currently visible (typewriter effect).
    /// Always `<= text.len()`. When equal, the line is fully revealed.
    pub chars_visible: usize,
    /// Visual style hint for the renderer.
    pub style: LineStyle,
}

/// The current phase of the boot sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootPhase {
    /// 0.0 -- 0.3s: blank screen with blinking cursor.
    BlackScreen,
    /// 0.3 -- 0.8s: CRT effects fade in, phosphor glow.
    CrtWarmup,
    /// 0.8 -- 1.2s: ASCII logo types out line by line.
    LogoReveal,
    /// 1.2 -- 2.5s: status lines appear one by one.
    SystemCheck,
    /// 2.5 -- 3.5s: welcome message, brief pause.
    Welcome,
    /// 3.5 -- 4.0s: fade/transition to live terminal.
    TransitionToTerminal,
    /// Boot complete. The app should switch to normal terminal rendering.
    Done,
}

/// Data-driven boot sequence state machine.
///
/// Advance each frame with [`update`](Self::update). Read the current visual
/// state via [`phase`](Self::phase), [`crt_intensity`](Self::crt_intensity),
/// [`visible_text`](Self::visible_text), and helper methods. The app layer is
/// responsible for actually rendering -- this struct is purely logical.
pub struct BootSequence {
    elapsed: f32,
    phase: BootPhase,
    /// True when the blinking cursor is in its "on" half-cycle.
    cursor_on: bool,
}

impl BootSequence {
    /// Create a new boot sequence, starting at `BlackScreen` / t=0.
    pub fn new() -> Self {
        Self {
            elapsed: 0.0,
            phase: BootPhase::BlackScreen,
            cursor_on: true,
        }
    }

    /// Advance the sequence by `dt` seconds and update the current phase.
    pub fn update(&mut self, dt: f32) {
        if self.phase == BootPhase::Done {
            return;
        }

        self.elapsed += dt;

        // Cursor blink: toggle based on a simple modular clock.
        self.cursor_on = (self.elapsed % CURSOR_BLINK_PERIOD) < (CURSOR_BLINK_PERIOD * 0.5);

        // Phase transitions based on absolute elapsed time.
        self.phase = if self.elapsed < T_BLACK_END {
            BootPhase::BlackScreen
        } else if self.elapsed < T_WARMUP_END {
            BootPhase::CrtWarmup
        } else if self.elapsed < T_LOGO_END {
            BootPhase::LogoReveal
        } else if self.elapsed < T_SYSCHECK_END {
            BootPhase::SystemCheck
        } else if self.elapsed < T_WELCOME_END {
            BootPhase::Welcome
        } else if self.elapsed < T_TRANSITION_END {
            BootPhase::TransitionToTerminal
        } else {
            BootPhase::Done
        };
    }

    /// The current boot phase.
    pub fn phase(&self) -> BootPhase {
        self.phase
    }

    /// Returns `true` once the boot sequence is finished and the app should
    /// switch to normal terminal rendering.
    pub fn is_done(&self) -> bool {
        self.phase == BootPhase::Done
    }

    /// Total elapsed time since boot start, in seconds.
    pub fn elapsed(&self) -> f32 {
        self.elapsed
    }

    /// Whether the blinking cursor should currently be visible.
    pub fn cursor_visible(&self) -> bool {
        self.cursor_on
    }

    // -----------------------------------------------------------------------
    // CRT effect intensity
    // -----------------------------------------------------------------------

    /// CRT shader intensity, ramping from 0.0 to 1.0.
    ///
    /// - `BlackScreen`: 0.0 (no CRT effects).
    /// - `CrtWarmup`: smooth ramp from 0.0 to 1.0.
    /// - All later phases: 1.0.
    pub fn crt_intensity(&self) -> f32 {
        match self.phase {
            BootPhase::BlackScreen => 0.0,
            BootPhase::CrtWarmup => {
                let t = (self.elapsed - T_BLACK_END) / (T_WARMUP_END - T_BLACK_END);
                smoothstep(t.clamp(0.0, 1.0))
            }
            _ => 1.0,
        }
    }

    /// Phosphor glow intensity, slightly offset from CRT warmup for a layered
    /// feel. Peaks at 1.0 during `LogoReveal` and stays there.
    pub fn glow_intensity(&self) -> f32 {
        match self.phase {
            BootPhase::BlackScreen => 0.0,
            BootPhase::CrtWarmup => {
                // Glow lags slightly behind the main CRT warmup.
                let t = ((self.elapsed - T_BLACK_END) / (T_WARMUP_END - T_BLACK_END) - 0.15)
                    .clamp(0.0, 1.0);
                smoothstep(t)
            }
            _ => 1.0,
        }
    }

    /// Scanline intensity, fading in during warmup.
    pub fn scanline_intensity(&self) -> f32 {
        match self.phase {
            BootPhase::BlackScreen => 0.0,
            BootPhase::CrtWarmup => {
                let t = (self.elapsed - T_BLACK_END) / (T_WARMUP_END - T_BLACK_END);
                smoothstep(t.clamp(0.0, 1.0)) * 0.7
            }
            _ => 0.7,
        }
    }

    /// Transition-to-terminal progress (0.0 at start of transition, 1.0 at end).
    /// Useful for fading out the boot screen or cross-fading to the terminal.
    pub fn transition_progress(&self) -> f32 {
        match self.phase {
            BootPhase::TransitionToTerminal => {
                let t = (self.elapsed - T_WELCOME_END) / (T_TRANSITION_END - T_WELCOME_END);
                smoothstep(t.clamp(0.0, 1.0))
            }
            BootPhase::Done => 1.0,
            _ => 0.0,
        }
    }

    /// Overall boot screen opacity. 1.0 for most phases, fades to 0.0 during
    /// the transition so the terminal can appear underneath.
    pub fn screen_opacity(&self) -> f32 {
        1.0 - self.transition_progress()
    }

    // -----------------------------------------------------------------------
    // Text content
    // -----------------------------------------------------------------------

    /// Returns all text lines that should be visible at the current time,
    /// with typewriter animation state.
    ///
    /// The returned lines include:
    /// - A blinking cursor (during `BlackScreen` and `CrtWarmup`)
    /// - ASCII logo lines (from `LogoReveal` onward)
    /// - System-check status lines (from `SystemCheck` onward)
    /// - Welcome message (from `Welcome` onward)
    pub fn visible_text(&self) -> Vec<BootTextLine> {
        let mut lines = Vec::new();

        match self.phase {
            BootPhase::BlackScreen => {
                // Blinking cursor only.
                if self.cursor_on {
                    lines.push(BootTextLine {
                        text: "\u{2588}".to_string(),
                        color: GREEN,
                        row: LOGO_START_ROW,
                        chars_visible: 1,
                        style: LineStyle::Normal,
                    });
                }
            }

            BootPhase::CrtWarmup => {
                // Blinking cursor, now with CRT glow behind it.
                if self.cursor_on {
                    lines.push(BootTextLine {
                        text: "\u{2588}".to_string(),
                        color: GREEN,
                        row: LOGO_START_ROW,
                        chars_visible: 1,
                        style: LineStyle::Normal,
                    });
                }
            }

            BootPhase::LogoReveal => {
                self.build_logo_lines(&mut lines);
            }

            BootPhase::SystemCheck => {
                // Logo is fully revealed.
                self.build_logo_lines_full(&mut lines);
                self.build_syscheck_lines(&mut lines);
            }

            BootPhase::Welcome => {
                self.build_logo_lines_full(&mut lines);
                self.build_syscheck_lines_full(&mut lines);
                self.build_welcome_line(&mut lines);
            }

            BootPhase::TransitionToTerminal => {
                // Everything visible, fading out (opacity handled by screen_opacity).
                self.build_logo_lines_full(&mut lines);
                self.build_syscheck_lines_full(&mut lines);
                self.build_welcome_line_full(&mut lines);
            }

            BootPhase::Done => {
                // Nothing -- the terminal has taken over.
            }
        }

        lines
    }

    // -----------------------------------------------------------------------
    // Internal line builders
    // -----------------------------------------------------------------------

    /// Logo lines with typewriter animation (during `LogoReveal`).
    fn build_logo_lines(&self, out: &mut Vec<BootTextLine>) {
        let phase_elapsed = self.elapsed - T_WARMUP_END;
        let total_chars: usize = LOGO_LINES.iter().map(|l| l.chars().count()).sum();
        let chars_typed = (phase_elapsed * TYPE_SPEED).max(0.0) as usize;

        let mut chars_remaining = chars_typed;
        for (i, &line) in LOGO_LINES.iter().enumerate() {
            let line_char_count = line.chars().count();
            if chars_remaining == 0 {
                break;
            }
            let visible = chars_remaining.min(line_char_count);
            chars_remaining = chars_remaining.saturating_sub(line_char_count);

            // Version line gets a dimmer color.
            let is_version = i == LOGO_LINES.len() - 1;
            let color = if is_version { GREEN_DIM } else { GREEN_BRIGHT };

            out.push(BootTextLine {
                text: line.to_string(),
                color,
                row: LOGO_START_ROW + i,
                chars_visible: visible,
                style: if is_version { LineStyle::Normal } else { LineStyle::Logo },
            });
        }

        // Blinking cursor at the end of the typing front.
        if chars_typed < total_chars && self.cursor_on {
            // Find which line and column the cursor is on.
            let mut cursor_chars = chars_typed;
            for (i, &line) in LOGO_LINES.iter().enumerate() {
                let lc = line.chars().count();
                if cursor_chars < lc {
                    // Cursor is on this line, at column `cursor_chars`.
                    // We encode it as a separate line so the renderer can
                    // overlay the block cursor character.
                    out.push(BootTextLine {
                        text: "\u{2588}".to_string(),
                        color: GREEN,
                        row: LOGO_START_ROW + i,
                        // The renderer should position this at column `cursor_chars`.
                        chars_visible: 1,
                        style: LineStyle::Normal,
                    });
                    break;
                }
                cursor_chars -= lc;
            }
        }
    }

    /// Fully-revealed logo (no animation).
    fn build_logo_lines_full(&self, out: &mut Vec<BootTextLine>) {
        for (i, &line) in LOGO_LINES.iter().enumerate() {
            let is_version = i == LOGO_LINES.len() - 1;
            let color = if is_version { GREEN_DIM } else { GREEN_BRIGHT };

            out.push(BootTextLine {
                text: line.to_string(),
                color,
                row: LOGO_START_ROW + i,
                chars_visible: line.chars().count(),
                style: if is_version { LineStyle::Normal } else { LineStyle::Logo },
            });
        }
    }

    /// System-check lines with staggered reveal (during `SystemCheck`).
    fn build_syscheck_lines(&self, out: &mut Vec<BootTextLine>) {
        let phase_elapsed = self.elapsed - T_LOGO_END;

        for (i, &line) in SYSCHECK_LINES.iter().enumerate() {
            let line_appear_time = i as f32 * SYSCHECK_LINE_DELAY;
            if phase_elapsed < line_appear_time {
                break;
            }

            let line_elapsed = phase_elapsed - line_appear_time;
            let line_char_count = line.chars().count();
            // Each status line types out quickly -- snappy, confident.
            let chars_visible = (line_elapsed * TYPE_SPEED * 1.5)
                .min(line_char_count as f32) as usize;

            out.push(BootTextLine {
                text: line.to_string(),
                color: STATUS_GREEN,
                row: SYSCHECK_START_ROW + i,
                chars_visible,
                style: LineStyle::Status,
            });
        }
    }

    /// All system-check lines fully revealed.
    fn build_syscheck_lines_full(&self, out: &mut Vec<BootTextLine>) {
        for (i, &line) in SYSCHECK_LINES.iter().enumerate() {
            out.push(BootTextLine {
                text: line.to_string(),
                color: STATUS_GREEN,
                row: SYSCHECK_START_ROW + i,
                chars_visible: line.chars().count(),
                style: LineStyle::Status,
            });
        }
    }

    /// Welcome message with typewriter effect (during `Welcome`).
    fn build_welcome_line(&self, out: &mut Vec<BootTextLine>) {
        let phase_elapsed = self.elapsed - T_SYSCHECK_END;
        // Brief pause before the welcome text starts typing.
        let type_delay = 0.3;
        let type_elapsed = (phase_elapsed - type_delay).max(0.0);
        let chars_visible = (type_elapsed * TYPE_SPEED * 0.4)
            .min(WELCOME_TEXT.chars().count() as f32) as usize;

        if chars_visible > 0 {
            out.push(BootTextLine {
                text: WELCOME_TEXT.to_string(),
                color: GREEN_BRIGHT,
                row: WELCOME_ROW,
                chars_visible,
                style: LineStyle::Normal,
            });
        }
    }

    /// Welcome message fully revealed.
    fn build_welcome_line_full(&self, out: &mut Vec<BootTextLine>) {
        out.push(BootTextLine {
            text: WELCOME_TEXT.to_string(),
            color: GREEN_BRIGHT,
            row: WELCOME_ROW,
            chars_visible: WELCOME_TEXT.chars().count(),
            style: LineStyle::Normal,
        });
    }
}

impl Default for BootSequence {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

/// Attempt to skip remaining boot animation (e.g., on keypress).
/// Jumps directly to the transition phase so it still looks clean.
impl BootSequence {
    /// Skip ahead to the transition phase. If already past that, go to `Done`.
    pub fn skip(&mut self) {
        match self.phase {
            BootPhase::Done => {}
            BootPhase::TransitionToTerminal => {
                self.elapsed = T_TRANSITION_END;
                self.phase = BootPhase::Done;
            }
            _ => {
                self.elapsed = T_WELCOME_END;
                self.phase = BootPhase::TransitionToTerminal;
            }
        }
    }
}

/// Attempt to quickly skip all the way to Done (double-press, impatient user).
impl BootSequence {
    pub fn skip_immediate(&mut self) {
        self.elapsed = T_TRANSITION_END;
        self.phase = BootPhase::Done;
    }
}

/// Hermite smoothstep: maps [0, 1] to [0, 1] with ease-in/ease-out.
///
///   smoothstep(t) = 3t^2 - 2t^3
///
/// Input is assumed to already be clamped to [0, 1].
fn smoothstep(t: f32) -> f32 {
    t * t * (3.0 - 2.0 * t)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state() {
        let seq = BootSequence::new();
        assert_eq!(seq.phase(), BootPhase::BlackScreen);
        assert!(!seq.is_done());
        assert_eq!(seq.crt_intensity(), 0.0);
    }

    #[test]
    fn default_is_new() {
        let a = BootSequence::new();
        let b = BootSequence::default();
        assert_eq!(a.phase(), b.phase());
        assert_eq!(a.elapsed(), b.elapsed());
    }

    #[test]
    fn phase_transitions_at_correct_times() {
        let mut seq = BootSequence::new();

        // Just before CRT warmup.
        seq.update(0.29);
        assert_eq!(seq.phase(), BootPhase::BlackScreen);

        // Into CRT warmup.
        seq.update(0.02);
        assert_eq!(seq.phase(), BootPhase::CrtWarmup);

        // Reset and jump to logo.
        let mut seq = BootSequence::new();
        seq.update(0.85);
        assert_eq!(seq.phase(), BootPhase::LogoReveal);

        // System check.
        let mut seq = BootSequence::new();
        seq.update(1.3);
        assert_eq!(seq.phase(), BootPhase::SystemCheck);

        // Welcome.
        let mut seq = BootSequence::new();
        seq.update(2.6);
        assert_eq!(seq.phase(), BootPhase::Welcome);

        // Transition.
        let mut seq = BootSequence::new();
        seq.update(3.6);
        assert_eq!(seq.phase(), BootPhase::TransitionToTerminal);

        // Done.
        let mut seq = BootSequence::new();
        seq.update(4.1);
        assert_eq!(seq.phase(), BootPhase::Done);
        assert!(seq.is_done());
    }

    #[test]
    fn crt_intensity_ramps_during_warmup() {
        let mut seq = BootSequence::new();

        seq.update(0.1);
        assert_eq!(seq.crt_intensity(), 0.0);

        seq = BootSequence::new();
        seq.update(0.55);
        let intensity = seq.crt_intensity();
        assert!(
            intensity > 0.0 && intensity < 1.0,
            "expected mid-ramp, got {intensity}"
        );

        seq = BootSequence::new();
        seq.update(0.85);
        assert_eq!(seq.crt_intensity(), 1.0);
    }

    #[test]
    fn visible_text_empty_at_start() {
        let seq = BootSequence::new();
        // Before any update, cursor_on is true, but elapsed is 0 so phase is BlackScreen.
        let lines = seq.visible_text();
        // Should have the blinking cursor.
        assert!(
            lines.len() <= 1,
            "expected at most a cursor, got {} lines",
            lines.len()
        );
    }

    #[test]
    fn logo_lines_appear_during_reveal() {
        let mut seq = BootSequence::new();
        seq.update(1.0); // mid logo reveal
        let lines = seq.visible_text();
        let logo_lines: Vec<_> = lines.iter().filter(|l| l.style == LineStyle::Logo).collect();
        assert!(!logo_lines.is_empty(), "expected logo lines during LogoReveal");
    }

    #[test]
    fn syscheck_lines_appear_during_system_check() {
        let mut seq = BootSequence::new();
        seq.update(2.0); // mid system check
        let lines = seq.visible_text();
        let status_lines: Vec<_> = lines.iter().filter(|l| l.style == LineStyle::Status).collect();
        assert!(
            !status_lines.is_empty(),
            "expected status lines during SystemCheck"
        );
    }

    #[test]
    fn welcome_appears_during_welcome_phase() {
        let mut seq = BootSequence::new();
        seq.update(3.2); // late welcome phase
        let lines = seq.visible_text();
        let welcome: Vec<_> = lines
            .iter()
            .filter(|l| l.text.contains("Welcome"))
            .collect();
        assert!(
            !welcome.is_empty(),
            "expected welcome line during Welcome phase"
        );
    }

    #[test]
    fn no_text_after_done() {
        let mut seq = BootSequence::new();
        seq.update(5.0);
        assert!(seq.is_done());
        let lines = seq.visible_text();
        assert!(lines.is_empty(), "expected no lines after Done");
    }

    #[test]
    fn skip_jumps_to_transition() {
        let mut seq = BootSequence::new();
        seq.update(0.5);
        seq.skip();
        assert_eq!(seq.phase(), BootPhase::TransitionToTerminal);
    }

    #[test]
    fn skip_from_transition_goes_to_done() {
        let mut seq = BootSequence::new();
        seq.update(3.6);
        assert_eq!(seq.phase(), BootPhase::TransitionToTerminal);
        seq.skip();
        assert_eq!(seq.phase(), BootPhase::Done);
    }

    #[test]
    fn skip_immediate_goes_to_done() {
        let mut seq = BootSequence::new();
        seq.update(0.5);
        seq.skip_immediate();
        assert!(seq.is_done());
    }

    #[test]
    fn done_phase_does_not_advance() {
        let mut seq = BootSequence::new();
        seq.update(5.0);
        let elapsed_before = seq.elapsed();
        seq.update(1.0);
        // Elapsed should not change once Done.
        assert_eq!(seq.elapsed(), elapsed_before);
    }

    #[test]
    fn transition_progress_range() {
        let mut seq = BootSequence::new();
        seq.update(3.5);
        assert_eq!(seq.transition_progress(), 0.0);

        seq = BootSequence::new();
        seq.update(3.75);
        let p = seq.transition_progress();
        assert!(
            p > 0.0 && p < 1.0,
            "expected mid-transition, got {p}"
        );

        seq = BootSequence::new();
        seq.update(4.1);
        assert_eq!(seq.transition_progress(), 1.0);
    }

    #[test]
    fn screen_opacity_inverse_of_transition() {
        let mut seq = BootSequence::new();
        seq.update(1.0);
        assert_eq!(seq.screen_opacity(), 1.0);

        seq = BootSequence::new();
        seq.update(4.1);
        assert_eq!(seq.screen_opacity(), 0.0);
    }

    #[test]
    fn smoothstep_boundaries() {
        assert_eq!(smoothstep(0.0), 0.0);
        assert_eq!(smoothstep(1.0), 1.0);
        assert_eq!(smoothstep(0.5), 0.5);
    }

    #[test]
    fn typewriter_shows_partial_chars() {
        let mut seq = BootSequence::new();
        // Very early in logo reveal -- should have partial chars_visible.
        seq.update(T_WARMUP_END + 0.01);
        assert_eq!(seq.phase(), BootPhase::LogoReveal);
        let lines = seq.visible_text();
        let logo_lines: Vec<_> = lines.iter().filter(|l| l.style == LineStyle::Logo).collect();
        if let Some(first) = logo_lines.first() {
            assert!(
                first.chars_visible < first.text.chars().count(),
                "expected partial reveal, got {}/{} chars visible",
                first.chars_visible,
                first.text.chars().count()
            );
        }
    }

    #[test]
    fn all_syscheck_lines_visible_near_end() {
        let mut seq = BootSequence::new();
        seq.update(2.4); // near end of SystemCheck
        let lines = seq.visible_text();
        let status_lines: Vec<_> = lines.iter().filter(|l| l.style == LineStyle::Status).collect();
        assert_eq!(
            status_lines.len(),
            SYSCHECK_LINES.len(),
            "all system check lines should be visible near end of phase"
        );
    }

    #[test]
    fn logo_constant_count() {
        // Verify the logo has the expected number of lines.
        assert_eq!(LOGO_LINES.len(), 7);
    }

    #[test]
    fn syscheck_constant_count() {
        assert_eq!(SYSCHECK_LINES.len(), 5);
    }
}
