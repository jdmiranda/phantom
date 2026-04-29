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
//! | Phase                  | Window       | Description                                    |
//! |------------------------|--------------|------------------------------------------------|
//! | `BlackScreen`          | 0.0 -- 0.5s  | Static noise — dead CRT interference           |
//! | `CrtWarmup`            | 0.5 -- 1.5s  | Noise clears center-out, scan beam sweeps      |
//! | `LogoReveal`           | 1.5 -- 3.5s  | PHANTOM logo glitches in char-by-char          |
//! | `SystemCheck`          | 3.5 -- 6.0s  | Status lines with animated progress bars       |
//! | `Welcome`              | 6.0s -- key  | SYSTEM READY. + blinking prompt, waits for key |
//! | `TransitionToTerminal` | key + 0.5s   | Quick fade to terminal                         |
//! | `Done`                 | after fade   | Boot complete, hand off                        |

// ---------------------------------------------------------------------------
// Phase boundaries (seconds)
// ---------------------------------------------------------------------------

const T_BLACK_END: f32 = 0.5;
const T_WARMUP_END: f32 = 1.5;
const T_LOGO_END: f32 = 3.5;
const T_SYSCHECK_END: f32 = 6.0;
const T_WELCOME_END: f32 = 6.5; // just past syscheck for the pause logic
const T_TRANSITION_END: f32 = 7.0;

/// Cursor blink period in seconds (on + off cycle).
const CURSOR_BLINK_PERIOD: f32 = 0.6;

/// Default terminal dimensions (used if not configured).
const DEFAULT_ROWS: usize = 24;
const DEFAULT_COLS: usize = 80;

/// Delay between successive system-check lines (seconds).
const SYSCHECK_LINE_DELAY: f32 = 0.3;

/// Duration for a progress bar to fill.
const PROGRESS_BAR_DURATION: f32 = 0.4;

/// Progress bar width in characters.
const PROGRESS_BAR_WIDTH: usize = 20;

// ---------------------------------------------------------------------------
// Colors (linear RGBA)
// ---------------------------------------------------------------------------

/// Phosphor green -- the signature Phantom color.
const GREEN: [f32; 4] = [0.2, 1.0, 0.5, 1.0];

/// Dimmed green for secondary text / noise.
const GREEN_DIM: [f32; 4] = [0.0, 0.65, 0.25, 0.5];

/// Bright white-green for emphasis.
const GREEN_BRIGHT: [f32; 4] = [0.55, 1.0, 0.72, 1.0];

/// Status green for system check lines.
const STATUS_GREEN: [f32; 4] = [0.2, 1.0, 0.5, 1.0];

/// Very dim green for noise characters during warmup clearing.
const GREEN_FAINT: [f32; 4] = [0.0, 0.4, 0.15, 0.3];

/// Scan beam bright line.
const SCAN_BEAM_COLOR: [f32; 4] = [0.6, 1.0, 0.8, 0.9];

// ---------------------------------------------------------------------------
// Glitch / noise characters
// ---------------------------------------------------------------------------

const GLITCH_CHARS: &[char] = &[
    '\u{2591}', '\u{2592}', '\u{2593}', '\u{2588}', '\u{2580}', '\u{2584}',
    '\u{258C}', '\u{2590}', '\u{2502}', '\u{2524}', '\u{2561}', '\u{2562}',
    '\u{2556}', '\u{2555}', '\u{2563}', '\u{2551}', '\u{2557}', '\u{255D}',
    '\u{255C}', '\u{255B}', '\u{2510}', '\u{2514}', '\u{2534}', '\u{252C}',
    '\u{251C}', '\u{2500}', '\u{253C}',
];

// ---------------------------------------------------------------------------
// ASCII logo
// ---------------------------------------------------------------------------

const LOGO_LINES: &[&str] = &[
    " \u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557} \u{2588}\u{2588}\u{2557}  \u{2588}\u{2588}\u{2557} \u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557} \u{2588}\u{2588}\u{2588}\u{2557}   \u{2588}\u{2588}\u{2557}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557} \u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557} \u{2588}\u{2588}\u{2588}\u{2557}   \u{2588}\u{2588}\u{2588}\u{2557}",
    " \u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2588}\u{2588}\u{2557}\u{2588}\u{2588}\u{2551}  \u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2588}\u{2588}\u{2557}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557}  \u{2588}\u{2588}\u{2551}\u{255A}\u{2550}\u{2550}\u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{255D}\u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2550}\u{2588}\u{2588}\u{2557}\u{2588}\u{2588}\u{2588}\u{2588}\u{2557} \u{2588}\u{2588}\u{2588}\u{2588}\u{2551}",
    " \u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2554}\u{255D}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2554}\u{2588}\u{2588}\u{2557} \u{2588}\u{2588}\u{2551}   \u{2588}\u{2588}\u{2551}   \u{2588}\u{2588}\u{2551}   \u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2554}\u{2588}\u{2588}\u{2588}\u{2588}\u{2554}\u{2588}\u{2588}\u{2551}",
    " \u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2550}\u{255D} \u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2554}\u{2550}\u{2550}\u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2551}\u{255A}\u{2588}\u{2588}\u{2557}\u{2588}\u{2588}\u{2551}   \u{2588}\u{2588}\u{2551}   \u{2588}\u{2588}\u{2551}   \u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2551}\u{255A}\u{2588}\u{2588}\u{2554}\u{255D}\u{2588}\u{2588}\u{2551}",
    " \u{2588}\u{2588}\u{2551}     \u{2588}\u{2588}\u{2551}  \u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2551}  \u{2588}\u{2588}\u{2551}\u{2588}\u{2588}\u{2551} \u{255A}\u{2588}\u{2588}\u{2588}\u{2588}\u{2551}   \u{2588}\u{2588}\u{2551}   \u{255A}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2588}\u{2554}\u{255D}\u{2588}\u{2588}\u{2551} \u{255A}\u{2550}\u{255D} \u{2588}\u{2588}\u{2551}",
    " \u{255A}\u{2550}\u{255D}     \u{255A}\u{2550}\u{255D}  \u{255A}\u{2550}\u{255D}\u{255A}\u{2550}\u{255D}  \u{255A}\u{2550}\u{255D}\u{255A}\u{2550}\u{255D}  \u{255A}\u{2550}\u{2550}\u{2550}\u{255D}   \u{255A}\u{2550}\u{255D}    \u{255A}\u{2550}\u{2550}\u{2550}\u{2550}\u{2550}\u{255D} \u{255A}\u{2550}\u{255D}     \u{255A}\u{2550}\u{255D}",
    "                        v0.1.0",
];

/// Laughing skull ASCII art — displayed above the logo during reveal.
const SKULL_LINES: &[&str] = &[
    "                    ██████████████                    ",
    "                ████░░░░░░░░░░░░░░████                ",
    "              ██░░░░░░░░░░░░░░░░░░░░░░██              ",
    "            ██░░░░░░░░░░░░░░░░░░░░░░░░░░██            ",
    "          ██░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░██          ",
    "         ██░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░██         ",
    "        ██░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░██        ",
    "        ██░░░░░░████░░░░░░░░░░░░████░░░░░░░░██        ",
    "        ██░░░░████████░░░░░░░░████████░░░░░░██        ",
    "        ██░░░░████████░░░░░░░░████████░░░░░░██        ",
    "        ██░░░░░░████░░░░░░░░░░░░████░░░░░░░░██        ",
    "         ██░░░░░░░░░░░░██████░░░░░░░░░░░░░░██         ",
    "          ██░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░██          ",
    "           ██░░░░░░████░░░░░░████░░░░░░░░██           ",
    "            ██░░░░░░░░████████░░░░░░░░░░██            ",
    "              ██░░░░░░░░░░░░░░░░░░░░░░██              ",
    "                ████░░░░░░░░░░░░░░████                ",
    "                  ██████████████████                   ",
    "                     ██  ██  ██                        ",
    "                   ██████████████                      ",
];

// Logo/content row positions are computed dynamically from screen size.
// See BootSequence::logo_start_row() etc.

// ---------------------------------------------------------------------------
// System check lines — content and their status labels
// ---------------------------------------------------------------------------

struct SysCheckDef {
    label: &'static str,
    dots: &'static str,
    status: &'static str,
}

const SYSCHECK_DEFS: &[SysCheckDef] = &[
    SysCheckDef { label: "\u{25A0} NEURAL CORE", dots: " ............ ", status: "ONLINE" },
    SysCheckDef { label: "\u{25A0} RENDER ENGINE", dots: " .......... ", status: "ACTIVE" },
    SysCheckDef { label: "\u{25A0} AGENT MESH", dots: " ............. ", status: "SYNCED" },
    SysCheckDef { label: "\u{25A0} MEMORY BANKS", dots: " ........... ", status: "847 LOADED" },
    SysCheckDef { label: "\u{25A0} SHADER PIPELINE", dots: " ........ ", status: "CRT ACTIVE" },
];

/// Kept for backwards compatibility in tests — mirrors SYSCHECK_DEFS labels.
#[cfg(test)]
const SYSCHECK_LINES: &[&str] = &[
    "\u{25A0} NEURAL CORE",
    "\u{25A0} RENDER ENGINE",
    "\u{25A0} AGENT MESH",
    "\u{25A0} MEMORY BANKS",
    "\u{25A0} SHADER PIPELINE",
];

const WELCOME_TEXT: &str = "SYSTEM READY.";

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
    /// 0.0 -- 0.5s: static noise, dead CRT interference.
    BlackScreen,
    /// 0.5 -- 1.5s: noise clears center-out, scan beam sweeps.
    CrtWarmup,
    /// 1.5 -- 3.5s: PHANTOM logo glitches in character by character.
    LogoReveal,
    /// 3.5 -- 6.0s: status lines with animated progress bars.
    SystemCheck,
    /// 6.0s -- keypress: SYSTEM READY + blinking prompt.
    Welcome,
    /// keypress + 0.5s: fade to terminal.
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
    /// When true, the boot sequence pauses at Welcome and waits for a keypress.
    waiting_for_keypress: bool,
    /// Screen dimensions in character cells.
    cols: usize,
    rows: usize,
}

impl BootSequence {
    /// Create a new boot sequence, starting at `BlackScreen` / t=0.
    pub fn new() -> Self {
        Self::with_size(DEFAULT_COLS, DEFAULT_ROWS)
    }

    /// Create a boot sequence sized to the given terminal dimensions.
    pub fn with_size(cols: usize, rows: usize) -> Self {
        Self {
            elapsed: 0.0,
            phase: BootPhase::BlackScreen,
            cursor_on: true,
            waiting_for_keypress: false,
            cols: cols.max(40),
            rows: rows.max(10),
        }
    }

    /// Update screen dimensions (e.g. after resize).
    pub fn set_size(&mut self, cols: usize, rows: usize) {
        self.cols = cols.max(40);
        self.rows = rows.max(10);
    }

    // -- Dynamic layout helpers --

    /// Content block height: skull + gap + logo + rule + syschk + welcome + prompt.
    fn content_height(&self) -> usize {
        SKULL_LINES.len() + 2 + LOGO_LINES.len() + 1 + 2 + SYSCHECK_DEFS.len() + 3
    }

    /// Vertically centered start row for the skull (top of content block).
    fn skull_start_row(&self) -> usize {
        self.rows.saturating_sub(self.content_height()) / 3
    }

    /// Start row for the PHANTOM logo (after skull + gap).
    fn logo_start_row(&self) -> usize {
        self.skull_start_row() + SKULL_LINES.len() + 2
    }

    fn rule_row(&self) -> usize {
        self.logo_start_row() + LOGO_LINES.len() + 1
    }

    fn syscheck_start_row(&self) -> usize {
        self.rule_row() + 2
    }

    fn welcome_row(&self) -> usize {
        self.syscheck_start_row() + SYSCHECK_DEFS.len() + 1
    }

    fn prompt_row(&self) -> usize {
        self.welcome_row() + 1
    }

    /// Advance the sequence by `dt` seconds and update the current phase.
    pub fn update(&mut self, dt: f32) {
        if self.phase == BootPhase::Done {
            return;
        }

        self.elapsed += dt;

        // Cursor blink: toggle based on a simple modular clock.
        self.cursor_on = (self.elapsed % CURSOR_BLINK_PERIOD) < (CURSOR_BLINK_PERIOD * 0.5);

        // Once we reach the Welcome phase, set the waiting flag — but only if
        // we haven't already been dismissed (phase would be TransitionToTerminal
        // or later after dismiss). We check by ensuring we're not already past
        // Welcome in the phase machine.
        if !self.waiting_for_keypress
            && self.elapsed >= T_SYSCHECK_END
            && self.phase != BootPhase::TransitionToTerminal
            && self.phase != BootPhase::Done
        {
            self.waiting_for_keypress = true;
        }

        // If waiting for keypress, clamp elapsed so we never advance past Welcome.
        if self.waiting_for_keypress {
            self.elapsed = self.elapsed.min(T_WELCOME_END);
        }

        // Phase transitions based on absolute elapsed time.
        self.phase = if self.elapsed < T_BLACK_END {
            BootPhase::BlackScreen
        } else if self.elapsed < T_WARMUP_END {
            BootPhase::CrtWarmup
        } else if self.elapsed < T_LOGO_END {
            BootPhase::LogoReveal
        } else if self.elapsed < T_SYSCHECK_END {
            BootPhase::SystemCheck
        } else if self.elapsed < T_WELCOME_END || self.waiting_for_keypress {
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
    /// with animation state.
    ///
    /// The returned lines include:
    /// - Full-screen noise (during `BlackScreen`)
    /// - Clearing noise with scan beam (during `CrtWarmup`)
    /// - Glitching ASCII logo (from `LogoReveal` onward)
    /// - System-check status lines with progress bars (from `SystemCheck` onward)
    /// - SYSTEM READY + blinking prompt (from `Welcome` onward)
    pub fn visible_text(&self) -> Vec<BootTextLine> {
        let mut lines = Vec::new();

        match self.phase {
            BootPhase::BlackScreen => {
                self.build_noise_lines(1.0, None, &mut lines);
            }

            BootPhase::CrtWarmup => {
                let phase_t = (self.elapsed - T_BLACK_END) / (T_WARMUP_END - T_BLACK_END);
                // Noise clears from center outward. clear_radius grows 0 -> ((self.rows as f32 / 2.0).powi(2) + (self.cols as f32 / 2.0).powi(2)).sqrt().
                let clear_radius = phase_t * ((self.rows as f32 / 2.0).powi(2) + (self.cols as f32 / 2.0).powi(2)).sqrt() * 1.3;
                self.build_noise_lines(1.0 - phase_t * 0.7, Some(clear_radius), &mut lines);

                // Scan beam: a bright horizontal line sweeping top to bottom once.
                let beam_row = (phase_t * (self.rows as f32)).min(self.rows as f32 - 1.0);
                let beam_row_int = beam_row as usize;
                if beam_row_int < self.rows {
                    let beam_text: String = "\u{2500}".repeat(self.cols);
                    lines.push(BootTextLine {
                        text: beam_text,
                        color: SCAN_BEAM_COLOR,
                        row: beam_row_int,
                        chars_visible: self.cols,
                        style: LineStyle::Normal,
                    });
                }
            }

            BootPhase::LogoReveal => {
                self.build_skull_lines(&mut lines);
                self.build_logo_lines(&mut lines);
            }

            BootPhase::SystemCheck => {
                self.build_skull_lines(&mut lines);
                self.build_logo_lines_full(&mut lines);
                self.build_syscheck_lines(&mut lines);
            }

            BootPhase::Welcome => {
                self.build_skull_lines(&mut lines);
                self.build_logo_lines_full(&mut lines);
                self.build_syscheck_lines_full(&mut lines);
                self.build_welcome_line(&mut lines);
            }

            BootPhase::TransitionToTerminal => {
                // Everything visible, fading out (opacity handled by screen_opacity).
                self.build_skull_lines(&mut lines);
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
    // Noise generation
    // -----------------------------------------------------------------------

    /// Generate full-screen noise lines. `density` controls how many cells are
    /// filled (0.0 = empty, 1.0 = ~35% filled). `clear_radius` if provided
    /// means cells within that distance from center are cleared.
    fn build_noise_lines(
        &self,
        density: f32,
        clear_radius: Option<f32>,
        out: &mut Vec<BootTextLine>,
    ) {
        let frame = (self.elapsed * 30.0) as u32; // ~30 fps frame counter

        for row in 0..self.rows {
            let mut line_chars = String::with_capacity(self.cols);
            let mut has_content = false;

            for col in 0..self.cols {
                // Deterministic hash for this cell at this frame.
                let hash = noise_hash(row, col, frame);

                // Should this cell be filled?
                let fill_threshold = (density * 0.35 * 255.0) as u32;
                let cell_val = (hash >> 8) & 0xFF;

                // If within clear radius, skip this cell.
                if let Some(radius) = clear_radius {
                    let dr = row as f32 - (self.rows as f32 / 2.0);
                    let dc = col as f32 - (self.cols as f32 / 2.0);
                    let dist = (dr * dr + dc * dc).sqrt();
                    if dist < radius {
                        line_chars.push(' ');
                        continue;
                    }
                }

                if cell_val < fill_threshold {
                    let ch = noise_char_from_hash(hash);
                    line_chars.push(ch);
                    has_content = true;
                } else {
                    line_chars.push(' ');
                }
            }

            if has_content {
                let len = line_chars.chars().count();
                out.push(BootTextLine {
                    text: line_chars,
                    color: GREEN_DIM,
                    row,
                    chars_visible: len,
                    style: LineStyle::Normal,
                });
            } else {
                // Emit empty row so spacing is maintained.
                out.push(BootTextLine {
                    text: " ".repeat(self.cols),
                    color: GREEN_FAINT,
                    row,
                    chars_visible: self.cols,
                    style: LineStyle::Normal,
                });
            }
        }
    }

    // -----------------------------------------------------------------------
    // Logo with glitch-in effect
    // -----------------------------------------------------------------------

    /// Logo lines with glitch-in animation (during `LogoReveal`).
    /// Build the skull ASCII art lines. Appears with a glitch-in effect during
    /// LogoReveal, then stays solid in later phases.
    fn build_skull_lines(&self, out: &mut Vec<BootTextLine>) {
        let skull_start = self.skull_start_row();
        let phase_elapsed = (self.elapsed - T_WARMUP_END).max(0.0);
        let skull_color: [f32; 4] = [0.15, 0.9, 0.4, 0.9];

        for (i, &line) in SKULL_LINES.iter().enumerate() {
            let line_char_count = line.chars().count();
            // Center the skull horizontally.
            let padding = self.cols.saturating_sub(line_char_count) / 2;
            let padded: String = " ".repeat(padding) + line;

            // Glitch in: characters lock progressively over 0.8s.
            let lock_progress = (phase_elapsed / 0.8).clamp(0.0, 1.0);
            let chars_locked = (lock_progress * padded.chars().count() as f32) as usize;

            let displayed: String = padded
                .chars()
                .enumerate()
                .map(|(ci, ch)| {
                    if ci < chars_locked || ch == ' ' {
                        ch
                    } else {
                        noise_char_from_hash(noise_hash(skull_start + i, ci, (self.elapsed * 15.0) as u32))
                    }
                })
                .collect();

            out.push(BootTextLine {
                text: displayed,
                color: skull_color,
                row: skull_start + i,
                chars_visible: padded.chars().count(),
                style: LineStyle::Logo,
            });
        }
    }

    ///
    /// Each character position has a "lock time". Before that time, the position
    /// shows random glitch characters cycling rapidly. After lock time, it shows
    /// the correct character. Lock times progress left to right with slight
    /// per-character variation for organic feel.
    fn build_logo_lines(&self, out: &mut Vec<BootTextLine>) {
        let phase_elapsed = self.elapsed - T_WARMUP_END;
        let _phase_duration = T_LOGO_END - T_WARMUP_END; // 2.0s

        // Logo glitch phase: first 1.6s for the main logo, last 0.4s for extras.
        let logo_duration = 1.6;
        let frame = (self.elapsed * 30.0) as u32;

        for (i, &logo_line) in LOGO_LINES.iter().enumerate() {
            let is_version = i == LOGO_LINES.len() - 1;
            let final_color = if is_version { GREEN_DIM } else { GREEN_BRIGHT };

            let char_count = logo_line.chars().count();
            if char_count == 0 {
                continue;
            }

            let mut display = String::with_capacity(char_count * 4);
            let mut any_visible = false;

            let logo_chars: Vec<char> = logo_line.chars().collect();

            for (ci, &real_ch) in logo_chars.iter().enumerate() {
                if real_ch == ' ' {
                    display.push(' ');
                    continue;
                }

                // Lock time for this character: progresses left to right.
                // Add per-character jitter based on position.
                let base_progress = ci as f32 / char_count.max(1) as f32;
                let row_offset = i as f32 * 0.02;
                let jitter = ((ci * 7919 + i * 6271) % 100) as f32 / 100.0 * 0.15;
                let lock_time = (base_progress + row_offset + jitter) * logo_duration;

                if phase_elapsed >= lock_time {
                    // Character is locked — show the real character.
                    display.push(real_ch);
                    any_visible = true;
                } else if phase_elapsed >= lock_time - 0.3 {
                    // Character is "trying" — show random glitch chars.
                    let hash = noise_hash(i, ci, frame);
                    let ch = noise_char_from_hash(hash);
                    display.push(ch);
                    any_visible = true;
                } else {
                    display.push(' ');
                }
            }

            if any_visible || phase_elapsed > logo_duration * 0.5 {
                let len = display.chars().count();
                // Color shifts from dim glitch green to final bright green as chars lock.
                let lock_fraction = if is_version {
                    if phase_elapsed > logo_duration { 1.0 } else { 0.0 }
                } else {
                    (phase_elapsed / logo_duration).clamp(0.0, 1.0)
                };
                let color = lerp_color(GREEN_DIM, final_color, lock_fraction);

                out.push(BootTextLine {
                    text: display,
                    color,
                    row: self.logo_start_row() + i,
                    chars_visible: len,
                    style: if is_version { LineStyle::Normal } else { LineStyle::Logo },
                });
            }
        }

        // Horizontal rule below logo — draws itself left to right.
        let rule_progress = ((phase_elapsed - logo_duration) / 0.3).clamp(0.0, 1.0);
        if rule_progress > 0.0 {
            let rule_width = 56; // width of the logo roughly
            let visible_chars = (rule_progress * rule_width as f32) as usize;
            let rule_text: String = "\u{2550}".repeat(rule_width);
            let padding = (self.cols - rule_width) / 2;
            let padded_rule = format!("{}{}", " ".repeat(padding), rule_text);
            let total_visible = padding + visible_chars;
            out.push(BootTextLine {
                text: padded_rule,
                color: GREEN_DIM,
                row: self.rule_row(),
                chars_visible: total_visible,
                style: LineStyle::Normal,
            });
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
                row: self.logo_start_row() + i,
                chars_visible: line.chars().count(),
                style: if is_version { LineStyle::Normal } else { LineStyle::Logo },
            });
        }

        // Full horizontal rule.
        let rule_width = 56;
        let padding = (self.cols - rule_width) / 2;
        let rule_text = format!("{}{}", " ".repeat(padding), "\u{2550}".repeat(rule_width));
        let len = rule_text.chars().count();
        out.push(BootTextLine {
            text: rule_text,
            color: GREEN_DIM,
            row: self.rule_row(),
            chars_visible: len,
            style: LineStyle::Normal,
        });
    }

    // -----------------------------------------------------------------------
    // System check lines with progress bars
    // -----------------------------------------------------------------------

    /// System-check lines with staggered reveal and animated progress bars.
    fn build_syscheck_lines(&self, out: &mut Vec<BootTextLine>) {
        let phase_elapsed = self.elapsed - T_LOGO_END;

        for (i, def) in SYSCHECK_DEFS.iter().enumerate() {
            let line_appear_time = i as f32 * SYSCHECK_LINE_DELAY;
            if phase_elapsed < line_appear_time {
                break;
            }

            let line_elapsed = phase_elapsed - line_appear_time;
            let text = build_syscheck_text(def, line_elapsed);
            let len = text.chars().count();

            out.push(BootTextLine {
                text,
                color: STATUS_GREEN,
                row: self.syscheck_start_row() + i,
                chars_visible: len,
                style: LineStyle::Status,
            });
        }
    }

    /// All system-check lines fully revealed.
    fn build_syscheck_lines_full(&self, out: &mut Vec<BootTextLine>) {
        for (i, def) in SYSCHECK_DEFS.iter().enumerate() {
            let text = build_syscheck_text(def, 10.0); // large elapsed = fully done
            let len = text.chars().count();
            out.push(BootTextLine {
                text,
                color: STATUS_GREEN,
                row: self.syscheck_start_row() + i,
                chars_visible: len,
                style: LineStyle::Status,
            });
        }
    }

    // -----------------------------------------------------------------------
    // Welcome / prompt
    // -----------------------------------------------------------------------

    /// Welcome message and blinking prompt (during `Welcome`).
    fn build_welcome_line(&self, out: &mut Vec<BootTextLine>) {
        // "SYSTEM READY." appears immediately.
        let len = WELCOME_TEXT.chars().count();
        out.push(BootTextLine {
            text: WELCOME_TEXT.to_string(),
            color: GREEN_BRIGHT,
            row: self.welcome_row(),
            chars_visible: len,
            style: LineStyle::Normal,
        });

        // Blinking prompt below.
        if self.waiting_for_keypress {
            let cursor = if self.cursor_on { "_" } else { " " };
            let prompt = format!("> PRESS ANY KEY TO INITIALIZE {cursor}");
            let plen = prompt.chars().count();
            // Only show when cursor is on for the blink effect on the whole line.
            out.push(BootTextLine {
                text: prompt,
                color: if self.cursor_on { GREEN } else { GREEN_DIM },
                row: self.prompt_row(),
                chars_visible: plen,
                style: LineStyle::Normal,
            });
        }
    }

    /// Welcome message fully revealed (for transition phase).
    fn build_welcome_line_full(&self, out: &mut Vec<BootTextLine>) {
        let len = WELCOME_TEXT.chars().count();
        out.push(BootTextLine {
            text: WELCOME_TEXT.to_string(),
            color: GREEN_BRIGHT,
            row: self.welcome_row(),
            chars_visible: len,
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
// Utility: dismiss / skip / is_waiting
// ---------------------------------------------------------------------------

impl BootSequence {
    /// Dismiss the boot screen (called on keypress while paused at Welcome).
    /// Starts the transition to terminal.
    pub fn dismiss(&mut self) {
        if self.waiting_for_keypress {
            self.waiting_for_keypress = false;
            self.elapsed = T_WELCOME_END;
            self.phase = BootPhase::TransitionToTerminal;
        }
    }

    /// Returns true if the boot sequence is paused waiting for user input.
    pub fn is_waiting(&self) -> bool {
        self.waiting_for_keypress
    }
}

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
                self.waiting_for_keypress = false;
                self.elapsed = T_WELCOME_END;
                self.phase = BootPhase::TransitionToTerminal;
            }
        }
    }
}

/// Attempt to quickly skip all the way to Done (double-press, impatient user).
impl BootSequence {
    pub fn skip_immediate(&mut self) {
        self.waiting_for_keypress = false;
        self.elapsed = T_TRANSITION_END;
        self.phase = BootPhase::Done;
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Deterministic pseudo-random hash for noise generation.
/// Uses position and frame to create varied but repeatable patterns.
fn noise_hash(row: usize, col: usize, frame: u32) -> u32 {
    let mut h = (row as u32).wrapping_mul(7919)
        .wrapping_add((col as u32).wrapping_mul(6271))
        .wrapping_add(frame.wrapping_mul(173));
    // Mix bits for better distribution.
    h ^= h >> 13;
    h = h.wrapping_mul(0x5BD1E995);
    h ^= h >> 15;
    h
}

/// Pick a glitch character from a hash value.
fn noise_char_from_hash(hash: u32) -> char {
    GLITCH_CHARS[(hash as usize) % GLITCH_CHARS.len()]
}

/// Build the display text for a system check line at a given elapsed time.
///
/// Stages:
/// 1. Label appears instantly.
/// 2. Dots appear instantly with label.
/// 3. Progress bar fills over `PROGRESS_BAR_DURATION`.
/// 4. Status text appears when bar is complete.
fn build_syscheck_text(def: &SysCheckDef, elapsed: f32) -> String {
    let bar_progress = (elapsed / PROGRESS_BAR_DURATION).clamp(0.0, 1.0);
    let filled = (bar_progress * PROGRESS_BAR_WIDTH as f32) as usize;
    let empty = PROGRESS_BAR_WIDTH - filled;

    let bar: String = format!(
        "{}{}",
        "\u{2588}".repeat(filled),
        "\u{2591}".repeat(empty),
    );

    let status = if bar_progress >= 1.0 {
        format!(" {}", def.status)
    } else {
        String::new()
    };

    format!("{}{}{}{}", def.label, def.dots, bar, status)
}

/// Linear interpolation between two RGBA colors.
fn lerp_color(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
        a[3] + (b[3] - a[3]) * t,
    ]
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
        seq.update(0.49);
        assert_eq!(seq.phase(), BootPhase::BlackScreen);

        // Into CRT warmup.
        seq.update(0.02);
        assert_eq!(seq.phase(), BootPhase::CrtWarmup);

        // Reset and jump to logo.
        let mut seq = BootSequence::new();
        seq.update(1.6);
        assert_eq!(seq.phase(), BootPhase::LogoReveal);

        // System check.
        let mut seq = BootSequence::new();
        seq.update(4.0);
        assert_eq!(seq.phase(), BootPhase::SystemCheck);

        // Welcome (waits for keypress, so we need to get there and it should pause).
        let mut seq = BootSequence::new();
        seq.update(6.1);
        assert_eq!(seq.phase(), BootPhase::Welcome);

        // After dismiss, transition.
        seq.dismiss();
        assert_eq!(seq.phase(), BootPhase::TransitionToTerminal);

        // After transition ends, done.
        seq.update(0.6);
        assert_eq!(seq.phase(), BootPhase::Done);
        assert!(seq.is_done());
    }

    #[test]
    fn crt_intensity_ramps_during_warmup() {
        let mut seq = BootSequence::new();

        seq.update(0.3);
        assert_eq!(seq.crt_intensity(), 0.0);

        seq = BootSequence::new();
        seq.update(1.0);
        let intensity = seq.crt_intensity();
        assert!(
            intensity > 0.0 && intensity < 1.0,
            "expected mid-ramp, got {intensity}"
        );

        seq = BootSequence::new();
        seq.update(1.6);
        assert_eq!(seq.crt_intensity(), 1.0);
    }

    #[test]
    fn visible_text_has_noise_at_start() {
        let mut seq = BootSequence::new();
        seq.update(0.1);
        let lines = seq.visible_text();
        // Should have noise lines (one per terminal row).
        assert!(
            lines.len() >= DEFAULT_ROWS,
            "expected at least {} noise lines, got {}",
            DEFAULT_ROWS,
            lines.len()
        );
    }

    #[test]
    fn logo_lines_appear_during_reveal() {
        let mut seq = BootSequence::new();
        seq.update(2.5); // mid logo reveal
        let lines = seq.visible_text();
        let logo_lines: Vec<_> = lines.iter().filter(|l| l.style == LineStyle::Logo).collect();
        assert!(!logo_lines.is_empty(), "expected logo lines during LogoReveal");
    }

    #[test]
    fn syscheck_lines_appear_during_system_check() {
        let mut seq = BootSequence::new();
        seq.update(5.0); // mid system check
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
        seq.update(6.2);
        let lines = seq.visible_text();
        let welcome: Vec<_> = lines
            .iter()
            .filter(|l| l.text.contains("SYSTEM READY"))
            .collect();
        assert!(
            !welcome.is_empty(),
            "expected SYSTEM READY line during Welcome phase"
        );
    }

    #[test]
    fn no_text_after_done() {
        let mut seq = BootSequence::new();
        seq.skip_immediate();
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
        seq.update(0.5);
        seq.skip();
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
        seq.skip_immediate();
        let elapsed_before = seq.elapsed();
        seq.update(1.0);
        // Elapsed should not change once Done.
        assert_eq!(seq.elapsed(), elapsed_before);
    }

    #[test]
    fn transition_progress_range() {
        // Before transition.
        let mut seq = BootSequence::new();
        seq.update(1.0);
        assert_eq!(seq.transition_progress(), 0.0);

        // During transition.
        seq = BootSequence::new();
        seq.update(0.1);
        seq.skip(); // jumps to transition
        seq.update(0.25); // mid-transition
        let p = seq.transition_progress();
        assert!(
            p > 0.0 && p < 1.0,
            "expected mid-transition, got {p}"
        );

        // After transition.
        seq = BootSequence::new();
        seq.skip_immediate();
        assert_eq!(seq.transition_progress(), 1.0);
    }

    #[test]
    fn screen_opacity_inverse_of_transition() {
        let mut seq = BootSequence::new();
        seq.update(1.0);
        assert_eq!(seq.screen_opacity(), 1.0);

        seq = BootSequence::new();
        seq.skip_immediate();
        assert_eq!(seq.screen_opacity(), 0.0);
    }

    #[test]
    fn smoothstep_boundaries() {
        assert_eq!(smoothstep(0.0), 0.0);
        assert_eq!(smoothstep(1.0), 1.0);
        assert_eq!(smoothstep(0.5), 0.5);
    }

    #[test]
    fn noise_hash_deterministic() {
        let a = noise_hash(5, 10, 42);
        let b = noise_hash(5, 10, 42);
        assert_eq!(a, b);
    }

    #[test]
    fn noise_hash_varies() {
        let a = noise_hash(0, 0, 0);
        let b = noise_hash(0, 0, 1);
        assert_ne!(a, b);
    }

    #[test]
    fn syscheck_progress_bar_fills() {
        let def = &SYSCHECK_DEFS[0];
        let early = build_syscheck_text(def, 0.1);
        let late = build_syscheck_text(def, 10.0);
        // Early should not have status text.
        assert!(!early.contains("ONLINE"), "bar should not be complete at 0.1s");
        // Late should have status text.
        assert!(late.contains("ONLINE"), "bar should be complete at 10.0s");
    }

    #[test]
    fn waiting_for_keypress_pauses() {
        let mut seq = BootSequence::new();
        // Advance well past welcome.
        seq.update(10.0);
        // Should be paused at Welcome because waiting_for_keypress.
        assert_eq!(seq.phase(), BootPhase::Welcome);
        assert!(seq.is_waiting());
        // Dismiss should transition.
        seq.dismiss();
        assert_eq!(seq.phase(), BootPhase::TransitionToTerminal);
    }

    #[test]
    fn logo_constant_count() {
        assert_eq!(LOGO_LINES.len(), 7);
    }

    #[test]
    fn syscheck_constant_count() {
        assert_eq!(SYSCHECK_LINES.len(), 5);
    }

    #[test]
    fn lerp_color_boundaries() {
        let a = [0.0, 0.0, 0.0, 0.0];
        let b = [1.0, 1.0, 1.0, 1.0];
        let mid = lerp_color(a, b, 0.5);
        assert!((mid[0] - 0.5).abs() < 0.001);
        assert!((mid[3] - 0.5).abs() < 0.001);
    }

    #[test]
    fn glitch_chars_nonempty() {
        assert!(!GLITCH_CHARS.is_empty());
    }

    // ── QA #158: First launch — boot animation reaches Done without panic ──

    /// Simulate the full boot sequence using fixed-size ticks until Done.
    /// Each tick is 16 ms (~60 fps). The sequence must reach Done within
    /// a bounded number of ticks and must never panic.
    #[test]
    fn qa_158_boot_reaches_done_via_incremental_ticks() {
        let mut seq = BootSequence::with_size(80, 24);

        // Tick at ~60 fps. Maximum budget: 10 seconds of simulated time.
        let dt = 1.0 / 60.0;
        let max_ticks = (10.0 / dt) as u32 + 1;

        let mut ticks = 0u32;
        // Boot waits for a keypress at Welcome; dismiss it after it arrives.
        let mut dismissed = false;

        for _ in 0..max_ticks {
            seq.update(dt);
            ticks += 1;

            // Dismiss once the sequence is waiting for a keypress.
            if !dismissed && seq.is_waiting() {
                seq.dismiss();
                dismissed = true;
            }

            if seq.is_done() {
                break;
            }

            // Reading visible_text each frame must never panic.
            let _ = seq.visible_text();
        }

        assert!(
            seq.is_done(),
            "boot sequence did not reach Done within {max_ticks} ticks (~10 s); \
             stopped at {:?} after {ticks} ticks",
            seq.phase(),
        );
        // After Done the visible text must be empty (terminal has taken over).
        assert!(
            seq.visible_text().is_empty(),
            "expected no visible text after Done, got {} lines",
            seq.visible_text().len(),
        );
    }

    /// Boot sequence must reach Done within 8 seconds of simulated time
    /// (10 s budget gives ample headroom for 7 s sequence + jitter).
    #[test]
    fn qa_158_boot_completes_within_reasonable_elapsed_time() {
        let mut seq = BootSequence::new();
        // Simulate dismiss at Welcome and let transition play out.
        seq.update(6.2); // into Welcome
        assert_eq!(seq.phase(), BootPhase::Welcome);
        seq.dismiss();
        seq.update(0.6); // through transition (0.5 s)
        assert!(seq.is_done(), "boot should be Done ~0.5 s after dismiss");
        assert!(seq.elapsed() < 8.0, "elapsed {} exceeds 8 s budget", seq.elapsed());
    }

    /// Shell responsiveness stub: after Done, visible_text is empty and
    /// the boot state machine accepts further update() calls without panic.
    #[test]
    fn qa_158_boot_done_state_is_stable() {
        let mut seq = BootSequence::new();
        seq.skip_immediate();
        assert!(seq.is_done());

        // Further ticks must be no-ops and must not panic.
        for _ in 0..100 {
            seq.update(0.016);
        }
        assert!(seq.is_done(), "Done must remain Done indefinitely");
        assert!(seq.visible_text().is_empty());
    }
}
