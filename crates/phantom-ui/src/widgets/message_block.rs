//! Issue #22 — Chat-style `MessageBlock` widget for agent panes.
//!
//! Each block renders one message turn: a 16×16 role-glyph (initial letter)
//! on the left, a role label on the first line, and body text that wraps
//! to fill the available width.  Height is computed automatically from the
//! wrapped line count so the caller does not need to pre-measure.
//!
//! Colors come from [`crate::tokens::Tokens`] / [`crate::tokens::ColorRoles`]
//! so a theme swap recolors all roles without touching this file.
//!
//! ```text
//! ┌─ rect ──────────────────────────────────────────────────────────┐
//! │ [U]  User                                                        │
//! │      hello, can you explain this error?                          │
//! └──────────────────────────────────────────────────────────────────┘
//! ```
//!
//! Layout constants:
//! - Avatar glyph column: 16 px wide + 8 px gap → `AVATAR_W + AVATAR_GAP`
//! - Body text column starts at `AVATAR_W + AVATAR_GAP` from rect left edge
//! - Row height = `ctx.cell_h()`; one extra row for the role label

use crate::layout::Rect;
use crate::render_ctx::RenderCtx;
use crate::tokens::Tokens;
use crate::widgets::{TextSegment, Widget};
use phantom_renderer::quads::QuadInstance;

// -----------------------------------------------------------------------
// Layout constants
// -----------------------------------------------------------------------

/// Width of the avatar glyph column in pixels.
pub const AVATAR_W: f32 = 16.0;
/// Horizontal gap between the avatar column and the body text column.
pub const AVATAR_GAP: f32 = 8.0;

// -----------------------------------------------------------------------
// Spinner frames
// -----------------------------------------------------------------------

/// Braille spinner frames cycling through 10 positions.
const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

// -----------------------------------------------------------------------
// LineKind
// -----------------------------------------------------------------------

/// Classifies each wrapped line so the renderer can apply distinct styling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// Ordinary prose text.
    Text,
    /// A line inside a fenced ``` code block.
    CodeLine,
    /// A line that is part of a tool call / tool result.
    ///
    /// Reserved for a future phase that classifies tool-call fences (e.g.
    /// `<tool_use>` blocks) so the renderer can style them distinctly from
    /// regular code. Currently `wrapped_lines` does not emit this variant.
    #[allow(dead_code)]
    ToolCall,
}

// -----------------------------------------------------------------------
// MessageRole
// -----------------------------------------------------------------------

/// Roles that can appear in the agent-pane chat feed.
///
/// Each role has a distinct token color and a single-character initial
/// used as the avatar glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    /// Human turn.
    User,
    /// AI agent turn.
    Agent,
    /// System / orchestrator message (not a human or agent turn).
    System,
    /// A tool invocation emitted by the agent.
    ToolUse,
    /// The result returned from a tool call.
    ToolResult,
}

impl MessageRole {
    /// Single uppercase ASCII initial for the avatar glyph column.
    #[must_use]
    pub fn initial(self) -> char {
        match self {
            Self::User => 'U',
            Self::Agent => 'A',
            Self::System => 'S',
            Self::ToolUse => 'T',
            Self::ToolResult => 'R',
        }
    }

    /// Short display label rendered on the first body line.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::User => "User",
            Self::Agent => "Agent",
            Self::System => "System",
            Self::ToolUse => "Tool Use",
            Self::ToolResult => "Tool Result",
        }
    }

    /// Resolve this role to an RGBA token color from the phosphor palette.
    ///
    /// Roles map to semantic color roles so a theme swap recolors everything:
    /// - `User`       → `text_primary`    (bright green)
    /// - `Agent`      → `text_accent`     (brighter accent)
    /// - `System`     → `text_secondary`  (muted)
    /// - `ToolUse`    → `status_info`     (cyan)
    /// - `ToolResult` → `status_ok`       (green OK)
    #[must_use]
    pub fn color(self, tokens: &Tokens) -> [f32; 4] {
        let c = &tokens.colors;
        match self {
            Self::User => c.text_primary,
            Self::Agent => c.text_accent,
            Self::System => c.text_secondary,
            Self::ToolUse => c.status_info,
            Self::ToolResult => c.status_ok,
        }
    }
}

// -----------------------------------------------------------------------
// ANSI stripping
// -----------------------------------------------------------------------

/// Strip ANSI escape sequences from `s`, returning clean text.
///
/// Handles:
/// - `ESC [ ... final_byte` — CSI sequences (including SGR color codes), drained
///   until the first ASCII alphabetic final byte.
/// - `ESC ]` (OSC), `ESC P` (DCS), `ESC ^` (PM), `ESC _` (APC), `ESC X` (SOS) —
///   string-terminator sequences drained until BEL (`\x07`) or ST (`ESC \`).
///   Without this, hyperlinks (`ESC]8;;URL BEL text ESC]8;; BEL`) and
///   tmux/iTerm2 passthrough would bleed through verbatim.
/// - Other two-character `ESC <byte>` sequences (e.g. `ESC (B`, `ESC =`,
///   `ESC >`, `ESC M`) — consume the single following byte.
fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next(); // consume '['
                    // Skip until we hit an ASCII letter (the final byte).
                    while let Some(&next) = chars.peek() {
                        chars.next();
                        if next.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                // OSC / DCS / PM / APC / SOS: drain until BEL or ST (ESC \).
                Some(']' | 'P' | '^' | '_' | 'X') => {
                    chars.next(); // consume introducer
                    while let Some(&next) = chars.peek() {
                        chars.next();
                        if next == '\x07' {
                            // BEL terminator.
                            break;
                        }
                        if next == '\x1b' {
                            // ST = ESC \ — consume the trailing '\' if present.
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                Some(_) => {
                    // Two-character ESC sequence: consume one more byte.
                    let _ = chars.next();
                }
                None => {
                    // Lone ESC at end of input — drop it.
                }
            }
        } else {
            result.push(c);
        }
    }
    result
}

// -----------------------------------------------------------------------
// MessageBlock
// -----------------------------------------------------------------------

/// A chat-style message block for agent panes.
///
/// # Layout
///
/// ```text
/// rect.x                 rect.x + AVATAR_W + AVATAR_GAP
///   |                       |
///   [I]   Role label
///         Body line 1
///         Body line 2 (wrapped)
///         ...
/// ```
///
/// The widget's rendered height is:
/// ```text
/// (1 + line_count) * cell_h
/// ```
/// where `1` is the role-label row and `line_count` is determined by
/// [`Self::compute_height`].
#[derive(Debug, Clone)]
pub struct MessageBlock {
    /// The speaker / message role.
    pub role: MessageRole,
    /// Raw body text. The widget wraps it at render time.
    pub body: String,
    /// Wall-clock timestamp (milliseconds since epoch). Currently unused
    /// in rendering but stored for future timestamp overlays.
    #[allow(dead_code)]
    pub(crate) timestamp_ms: u64,
    /// Render context for cell metrics. Defaults to `RenderCtx::fallback()`.
    ctx: RenderCtx,
    /// Optional spinner state. When `Some(frame)`, a braille spinner character
    /// is appended to the last rendered line (frame is an index into
    /// [`SPINNER_FRAMES`]).
    pub spinner_frame: Option<u8>,
}

impl MessageBlock {
    /// Construct a `MessageBlock` with `RenderCtx::fallback()` metrics.
    pub fn new(role: MessageRole, body: impl Into<String>, timestamp_ms: u64) -> Self {
        Self {
            role,
            body: body.into(),
            timestamp_ms,
            ctx: RenderCtx::fallback(),
            spinner_frame: None,
        }
    }

    /// Update the live render context so wrap calculations reflect the
    /// current font metrics.
    pub fn set_render_ctx(&mut self, ctx: RenderCtx) {
        self.ctx = ctx;
    }

    /// Advance the spinner to the next frame.
    ///
    /// If no spinner is active (`spinner_frame` is `None`), this starts the
    /// spinner at frame 0. Call this from the app update loop when the agent
    /// status is `Working`.
    pub fn advance_spinner(&mut self) {
        let next = match self.spinner_frame {
            None => 0,
            // Compute the modulo in `usize` so a future `SPINNER_FRAMES.len()`
            // beyond 255 cannot silently truncate via `as u8`. The current
            // 10-element table can never exceed `u8::MAX`, so the cast back
            // is always lossless.
            Some(f) => {
                let next_idx = (f as usize + 1) % SPINNER_FRAMES.len();
                next_idx as u8
            }
        };
        self.spinner_frame = Some(next);
    }

    /// Compute the pixel height this block occupies for a given `rect_width`.
    ///
    /// Height = `(1 + body_rows) * cell_h`, where the leading `1` accounts
    /// for the role-label row and `body_rows` is the wrapped line count.
    /// When the body is empty but a spinner is active, one extra row is
    /// reserved for the standalone spinner segment so the next widget in a
    /// vertical list does not overlap it.
    #[must_use]
    pub fn compute_height(&self, rect_width: f32) -> f32 {
        let line_count = self.wrapped_lines(rect_width).len();
        let spinner_row = usize::from(line_count == 0 && self.spinner_frame.is_some());
        // role-label row (1) + body rows (+ optional spinner row)
        (1 + line_count + spinner_row) as f32 * self.ctx.cell_h()
    }

    /// Wrap `body` into classified lines that fit within `rect_width`,
    /// honouring the avatar column offset.
    ///
    /// ANSI escape codes are stripped before wrapping.  Lines inside fenced
    /// ` ``` ` code blocks are tagged [`LineKind::CodeLine`]; all other lines
    /// are tagged [`LineKind::Text`].
    ///
    /// The available body-column width (in characters) is:
    /// ```text
    /// floor((rect_width - AVATAR_W - AVATAR_GAP) / cell_w)
    /// ```
    /// An empty body returns an empty `Vec`.  A `cell_w` of `0.0` (degenerate
    /// context) is treated as `1.0` to avoid division by zero.
    #[must_use]
    pub fn wrapped_lines(&self, rect_width: f32) -> Vec<(LineKind, String)> {
        let cell_w = self.ctx.cell_w().max(1.0);
        let body_px = (rect_width - AVATAR_W - AVATAR_GAP).max(0.0);
        let cols = (body_px / cell_w).floor() as usize;

        // Strip ANSI escape codes before any further processing.
        let clean_body = strip_ansi(&self.body);

        if cols == 0 || clean_body.is_empty() {
            return Vec::new();
        }

        let mut lines: Vec<(LineKind, String)> = Vec::new();
        let mut in_code_block = false;

        for raw_line in clean_body.lines() {
            // Detect fenced code block boundaries.
            let trimmed = raw_line.trim();
            if trimmed.starts_with("```") {
                in_code_block = !in_code_block;
                // Emit the fence line itself as a CodeLine.
                lines.push((LineKind::CodeLine, raw_line.to_owned()));
                continue;
            }

            let kind = if in_code_block {
                LineKind::CodeLine
            } else {
                LineKind::Text
            };

            // Word-wrap the raw_line into cols-wide chunks.
            let mut remaining: &str = raw_line;
            loop {
                if remaining.is_empty() {
                    break;
                }
                let chars: Vec<char> = remaining.chars().collect();
                if chars.len() <= cols {
                    lines.push((kind, remaining.to_owned()));
                    break;
                }
                // Find the last space within `cols` chars for a soft word-break.
                // Use char-index arithmetic — byte offsets are wrong for multibyte text.
                let break_at = chars[..cols]
                    .iter()
                    .enumerate()
                    .rfind(|&(_, c)| *c == ' ')
                    .map(|(i, _)| i)
                    .filter(|&i| i > 0)
                    .unwrap_or(cols); // hard-break if no space found

                lines.push((kind, chars[..break_at].iter().collect()));

                let skip = if break_at < chars.len() && chars[break_at] == ' ' {
                    break_at + 1
                } else {
                    break_at
                };
                remaining =
                    &remaining[chars[..skip].iter().map(|c| c.len_utf8()).sum::<usize>()..];
            }
        }

        lines
    }
}

impl Widget for MessageBlock {
    /// Emits quads: one full-width background in `surface_recessed`, plus
    /// an additional background quad behind each [`LineKind::CodeLine`] to
    /// distinguish code blocks from prose.
    fn render_quads(&self, rect: &Rect) -> Vec<QuadInstance> {
        let t = Tokens::phosphor(self.ctx);
        let cell_h = self.ctx.cell_h();
        let body_x = rect.x + AVATAR_W + AVATAR_GAP;
        let body_width = (rect.width - AVATAR_W - AVATAR_GAP).max(0.0);

        // Compute wrapped lines once. `compute_height` would otherwise wrap
        // a second time on the very next line — wasteful for long streaming
        // messages where the spinner forces a redraw every tick.
        let body_lines = self.wrapped_lines(rect.width);
        let spinner_row = usize::from(body_lines.is_empty() && self.spinner_frame.is_some());
        let total_height = (1 + body_lines.len() + spinner_row) as f32 * cell_h;

        // Full-block background.
        let mut quads = vec![QuadInstance {
            pos: [rect.x, rect.y],
            size: [rect.width, total_height],
            color: t.colors.surface_recessed,
            border_radius: 0.0,
        }];

        // Code-line highlight quads: dark charcoal overlay on top of
        // surface_recessed to give code blocks a subtle inset appearance.
        let code_bg: [f32; 4] = [0.05, 0.08, 0.05, 0.6];

        for (i, (kind, _)) in body_lines.iter().enumerate() {
            if *kind == LineKind::CodeLine {
                let line_y = rect.y + (i + 1) as f32 * cell_h;
                quads.push(QuadInstance {
                    pos: [body_x, line_y],
                    size: [body_width, cell_h],
                    color: code_bg,
                    border_radius: 0.0,
                });
            }
        }

        quads
    }

    /// Emits text segments:
    /// 1. Avatar initial in the glyph column (role color).
    /// 2. Role label on row 0 of the body column (role color).
    /// 3. One `TextSegment` per wrapped body line (text_primary for prose,
    ///    slightly dimmer for code lines).
    /// 4. If `spinner_frame` is `Some`, appends the spinner glyph to the last
    ///    line (or adds a standalone spinner segment when the body is empty).
    fn render_text(&self, rect: &Rect) -> Vec<TextSegment> {
        let t = Tokens::phosphor(self.ctx);
        let role_color = self.role.color(&t);
        let cell_h = self.ctx.cell_h();

        let avatar_x = rect.x;
        let body_x = rect.x + AVATAR_W + AVATAR_GAP;
        let label_y = rect.y;

        let mut segments = Vec::new();

        // Avatar glyph.
        segments.push(TextSegment {
            text: self.role.initial().to_string(),
            x: avatar_x,
            y: label_y,
            color: role_color,
        });

        // Role label.
        segments.push(TextSegment {
            text: self.role.label().to_owned(),
            x: body_x,
            y: label_y,
            color: role_color,
        });

        // Code line color: 85% of text_primary brightness.
        let code_color: [f32; 4] = [
            t.colors.text_primary[0] * 0.85,
            t.colors.text_primary[1] * 0.85,
            t.colors.text_primary[2] * 0.85,
            t.colors.text_primary[3],
        ];

        let body_lines = self.wrapped_lines(rect.width);
        let line_count = body_lines.len();

        for (i, (kind, line)) in body_lines.into_iter().enumerate() {
            let line_y = rect.y + (i + 1) as f32 * cell_h;
            let is_last = i + 1 == line_count;

            // Append spinner glyph to the last line when active.
            let text = if is_last {
                if let Some(frame) = self.spinner_frame {
                    let spinner = SPINNER_FRAMES[frame as usize % SPINNER_FRAMES.len()];
                    format!("{line} {spinner}")
                } else {
                    line
                }
            } else {
                line
            };

            let color = match kind {
                LineKind::CodeLine => code_color,
                _ => t.colors.text_primary,
            };

            segments.push(TextSegment {
                text,
                x: body_x,
                y: line_y,
                color,
            });
        }

        // Standalone spinner row when body is empty.
        if line_count == 0 && let Some(frame) = self.spinner_frame {
            let spinner = SPINNER_FRAMES[frame as usize % SPINNER_FRAMES.len()];
            segments.push(TextSegment {
                text: spinner.to_string(),
                x: body_x,
                y: rect.y + cell_h,
                color: t.colors.text_primary,
            });
        }

        segments
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_ctx::RenderCtx;
    use crate::tokens::Tokens;

    /// Helper: fallback context (cell_w = 8.0, cell_h = 16.0).
    fn fallback_ctx() -> RenderCtx {
        RenderCtx::fallback()
    }

    fn make_rect(width: f32) -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            width,
            height: 200.0,
        }
    }

    // ----------------------------------------------------------------
    // Role -> color mapping
    // ----------------------------------------------------------------

    #[test]
    fn user_role_color_matches_text_primary() {
        let tokens = Tokens::phosphor(fallback_ctx());
        assert_eq!(
            MessageRole::User.color(&tokens),
            tokens.colors.text_primary,
            "User should map to text_primary"
        );
    }

    #[test]
    fn agent_role_color_matches_text_accent() {
        let tokens = Tokens::phosphor(fallback_ctx());
        assert_eq!(
            MessageRole::Agent.color(&tokens),
            tokens.colors.text_accent,
            "Agent should map to text_accent"
        );
    }

    #[test]
    fn system_role_color_matches_text_secondary() {
        let tokens = Tokens::phosphor(fallback_ctx());
        assert_eq!(
            MessageRole::System.color(&tokens),
            tokens.colors.text_secondary,
            "System should map to text_secondary"
        );
    }

    #[test]
    fn tool_use_role_color_matches_status_info() {
        let tokens = Tokens::phosphor(fallback_ctx());
        assert_eq!(
            MessageRole::ToolUse.color(&tokens),
            tokens.colors.status_info,
            "ToolUse should map to status_info"
        );
    }

    #[test]
    fn tool_result_role_color_matches_status_ok() {
        let tokens = Tokens::phosphor(fallback_ctx());
        assert_eq!(
            MessageRole::ToolResult.color(&tokens),
            tokens.colors.status_ok,
            "ToolResult should map to status_ok"
        );
    }

    /// All five roles must have distinct colors so they are visually
    /// distinguishable in the agent pane.
    #[test]
    fn all_role_colors_are_distinct() {
        let tokens = Tokens::phosphor(fallback_ctx());
        let colors: Vec<[f32; 4]> = [
            MessageRole::User,
            MessageRole::Agent,
            MessageRole::System,
            MessageRole::ToolUse,
            MessageRole::ToolResult,
        ]
        .iter()
        .map(|r| r.color(&tokens))
        .collect();

        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(
                    colors[i], colors[j],
                    "roles at index {i} and {j} share the same color: {:?}",
                    colors[i]
                );
            }
        }
    }

    // ----------------------------------------------------------------
    // Height auto-computation
    // ----------------------------------------------------------------

    /// Single-line body -> height = 2 rows (label + 1 body line).
    #[test]
    fn single_line_body_height_is_two_rows() {
        let mut block = MessageBlock::new(MessageRole::User, "hello", 0);
        block.set_render_ctx(fallback_ctx());
        // Width wide enough that "hello" (5 chars) fits in one line.
        // Body column width = 800 - 16 - 8 = 776 px -> 97 chars at cell_w=8
        let h = block.compute_height(800.0);
        let cell_h = fallback_ctx().cell_h();
        assert_eq!(
            h,
            2.0 * cell_h,
            "single-line body: expected 2 rows, got {h}"
        );
    }

    /// Multiline body grows height proportionally.
    #[test]
    fn multiline_body_height_grows_with_lines() {
        // cell_w = 8, body col = 800 - 16 - 8 = 776 px -> 97 cols
        // body = 200 'a' chars -> ceil(200/97) = 3 lines
        let body: String = "a".repeat(200);
        let mut block = MessageBlock::new(MessageRole::Agent, body, 0);
        block.set_render_ctx(fallback_ctx());

        let ctx = fallback_ctx();
        let lines = block.wrapped_lines(800.0);
        let expected_h = (1 + lines.len()) as f32 * ctx.cell_h();
        let actual_h = block.compute_height(800.0);
        assert_eq!(actual_h, expected_h);
        // Also verify the block is taller than the two-row single-line case.
        assert!(
            actual_h > 2.0 * ctx.cell_h(),
            "multiline body should be taller than 2 rows"
        );
    }

    /// Empty body -> only the label row -> height = 1 row.
    #[test]
    fn empty_body_height_is_one_row() {
        let mut block = MessageBlock::new(MessageRole::System, "", 0);
        block.set_render_ctx(fallback_ctx());
        let cell_h = fallback_ctx().cell_h();
        assert_eq!(
            block.compute_height(800.0),
            cell_h,
            "empty body: expected 1 row"
        );
    }

    // ----------------------------------------------------------------
    // Wrap calculation
    // ----------------------------------------------------------------

    /// A body shorter than the column should not be wrapped.
    #[test]
    fn short_body_is_not_wrapped() {
        // Body col = 800 - 16 - 8 = 776 px / 8 = 97 cols
        let body = "short message";
        let mut block = MessageBlock::new(MessageRole::User, body, 0);
        block.set_render_ctx(fallback_ctx());
        let lines = block.wrapped_lines(800.0);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].1, body);
    }

    /// A body of exactly `cols` chars must fit in a single line.
    #[test]
    fn body_exactly_cols_wide_fits_one_line() {
        // cell_w=8, rect=800 -> body_col=776px -> 97 cols
        let body: String = "x".repeat(97);
        let mut block = MessageBlock::new(MessageRole::User, body.clone(), 0);
        block.set_render_ctx(fallback_ctx());
        let lines = block.wrapped_lines(800.0);
        assert_eq!(
            lines.len(),
            1,
            "body of exactly `cols` chars should be one line"
        );
        assert_eq!(lines[0].1, body);
    }

    /// A body of `cols+1` chars must split into two lines.
    #[test]
    fn body_one_over_cols_wraps_to_two_lines() {
        // 97 + 1 = 98 'x' chars, no spaces -> hard break at 97
        let body: String = "x".repeat(98);
        let mut block = MessageBlock::new(MessageRole::User, body, 0);
        block.set_render_ctx(fallback_ctx());
        let lines = block.wrapped_lines(800.0);
        assert_eq!(
            lines.len(),
            2,
            "98 chars at 97-col width should produce 2 lines"
        );
    }

    /// Word-wrap: prefer breaking at a space rather than mid-word.
    #[test]
    fn wrap_prefers_word_boundary() {
        let word_a: String = "a".repeat(48);
        let word_b: String = "b".repeat(48);
        let body = format!("{word_a} {word_b}cccc");
        let mut block = MessageBlock::new(MessageRole::User, body, 0);
        block.set_render_ctx(fallback_ctx());
        let lines = block.wrapped_lines(800.0);
        assert!(
            lines.len() >= 2,
            "should have wrapped at space boundary: {lines:?}"
        );
        assert!(
            !lines[0].1.starts_with('b'),
            "first line should not start with 'b': {:?}",
            lines[0].1
        );
    }

    /// Very narrow rect (body col <= 0 px) -> no lines (degenerate).
    #[test]
    fn zero_width_rect_produces_no_lines() {
        let mut block = MessageBlock::new(MessageRole::User, "hello", 0);
        block.set_render_ctx(fallback_ctx());
        let lines = block.wrapped_lines(AVATAR_W + AVATAR_GAP);
        assert!(
            lines.is_empty(),
            "zero-width body column should produce no lines"
        );
    }

    // ----------------------------------------------------------------
    // Widget trait -- render_quads
    // ----------------------------------------------------------------

    #[test]
    fn render_quads_emits_exactly_one_background_quad() {
        let mut block = MessageBlock::new(MessageRole::Agent, "hi", 0);
        block.set_render_ctx(fallback_ctx());
        let rect = make_rect(800.0);
        let quads = block.render_quads(&rect);
        // 1 background quad only (no code lines)
        assert_eq!(quads.len(), 1, "should emit exactly one background quad");
        let t = Tokens::phosphor(fallback_ctx());
        assert_eq!(
            quads[0].color, t.colors.surface_recessed,
            "background should use surface_recessed"
        );
    }

    #[test]
    fn render_quads_height_matches_compute_height() {
        let body = "hello world this is a test message";
        let mut block = MessageBlock::new(MessageRole::User, body, 42);
        block.set_render_ctx(fallback_ctx());
        let rect = make_rect(200.0);
        let quads = block.render_quads(&rect);
        assert_eq!(quads[0].size[1], block.compute_height(200.0));
    }

    // ----------------------------------------------------------------
    // Widget trait -- render_text
    // ----------------------------------------------------------------

    /// render_text must always emit at least: avatar + label (2 segments).
    #[test]
    fn render_text_always_has_avatar_and_label() {
        let mut block = MessageBlock::new(MessageRole::System, "", 0);
        block.set_render_ctx(fallback_ctx());
        let rect = make_rect(800.0);
        let texts = block.render_text(&rect);
        assert!(
            texts.len() >= 2,
            "should have at least avatar + label segments"
        );
    }

    /// Avatar segment uses the role initial.
    #[test]
    fn avatar_segment_uses_role_initial() {
        let mut block = MessageBlock::new(MessageRole::ToolUse, "ok", 0);
        block.set_render_ctx(fallback_ctx());
        let rect = make_rect(800.0);
        let texts = block.render_text(&rect);
        let avatar = &texts[0];
        assert_eq!(avatar.text, "T", "ToolUse avatar should be 'T'");
    }

    /// Label segment uses the role label string.
    #[test]
    fn label_segment_uses_role_label() {
        let mut block = MessageBlock::new(MessageRole::ToolResult, "result data", 0);
        block.set_render_ctx(fallback_ctx());
        let rect = make_rect(800.0);
        let texts = block.render_text(&rect);
        let label = &texts[1];
        assert_eq!(
            label.text, "Tool Result",
            "ToolResult label should be 'Tool Result'"
        );
    }

    /// Avatar and label use the role color.
    #[test]
    fn avatar_and_label_use_role_color() {
        let ctx = fallback_ctx();
        let mut block = MessageBlock::new(MessageRole::Agent, "body", 0);
        block.set_render_ctx(ctx);
        let rect = make_rect(800.0);
        let texts = block.render_text(&rect);
        let tokens = Tokens::phosphor(ctx);
        let expected = MessageRole::Agent.color(&tokens);
        assert_eq!(
            texts[0].color, expected,
            "avatar color should match role color"
        );
        assert_eq!(
            texts[1].color, expected,
            "label color should match role color"
        );
    }

    /// Body text segments use text_primary color (for prose lines).
    #[test]
    fn body_segments_use_text_primary_color() {
        let ctx = fallback_ctx();
        let mut block = MessageBlock::new(MessageRole::User, "some body text", 0);
        block.set_render_ctx(ctx);
        let rect = make_rect(800.0);
        let texts = block.render_text(&rect);
        let tokens = Tokens::phosphor(ctx);
        // Segments after [0]=avatar, [1]=label are body lines.
        for seg in texts.iter().skip(2) {
            assert_eq!(
                seg.color, tokens.colors.text_primary,
                "body segment color should be text_primary, got {:?}",
                seg.color
            );
        }
    }

    /// Body x-position must be offset by AVATAR_W + AVATAR_GAP.
    #[test]
    fn body_text_is_offset_past_avatar_column() {
        let ctx = fallback_ctx();
        let mut block = MessageBlock::new(MessageRole::User, "body text here", 0);
        block.set_render_ctx(ctx);
        let rect = make_rect(800.0);
        let texts = block.render_text(&rect);
        let expected_body_x = rect.x + AVATAR_W + AVATAR_GAP;
        for seg in texts.iter().skip(1) {
            assert_eq!(
                seg.x, expected_body_x,
                "segment '{}' should start at body_x={expected_body_x}, got {}",
                seg.text, seg.x
            );
        }
    }

    /// Segment count = 2 (avatar + label) + wrapped line count.
    #[test]
    fn segment_count_equals_two_plus_line_count() {
        let ctx = fallback_ctx();
        let body = "hello world";
        let mut block = MessageBlock::new(MessageRole::User, body, 0);
        block.set_render_ctx(ctx);
        let rect = make_rect(800.0);
        let line_count = block.wrapped_lines(800.0).len();
        let texts = block.render_text(&rect);
        assert_eq!(
            texts.len(),
            2 + line_count,
            "expected 2 + {line_count} segments, got {}",
            texts.len()
        );
    }

    // ----------------------------------------------------------------
    // MessageRole convenience methods
    // ----------------------------------------------------------------

    #[test]
    fn initials_are_unique_per_role() {
        let roles = [
            MessageRole::User,
            MessageRole::Agent,
            MessageRole::System,
            MessageRole::ToolUse,
            MessageRole::ToolResult,
        ];
        let initials: Vec<char> = roles.iter().map(|r| r.initial()).collect();
        let unique: std::collections::HashSet<char> = initials.iter().copied().collect();
        assert_eq!(
            unique.len(),
            initials.len(),
            "all role initials must be distinct"
        );
    }

    #[test]
    fn labels_are_non_empty_for_all_roles() {
        for role in [
            MessageRole::User,
            MessageRole::Agent,
            MessageRole::System,
            MessageRole::ToolUse,
            MessageRole::ToolResult,
        ] {
            assert!(
                !role.label().is_empty(),
                "label for {role:?} must not be empty"
            );
        }
    }

    // ----------------------------------------------------------------
    // Multibyte / Unicode regression tests
    // ----------------------------------------------------------------

    /// Wrapping a body that contains multibyte characters (emoji, accented
    /// letters, CJK) must not panic and must produce correct line boundaries.
    #[test]
    fn wrap_is_correct_for_multibyte_body() {
        let left: String = "🦀".repeat(50);
        let right: String = "🦀".repeat(50);
        let body = format!("{left} {right}");

        let mut block = MessageBlock::new(MessageRole::Agent, body.clone(), 0);
        block.set_render_ctx(fallback_ctx());

        let lines = block.wrapped_lines(800.0);

        assert_eq!(
            lines.len(),
            2,
            "emoji body should wrap into 2 lines: {lines:?}"
        );
        assert_eq!(lines[0].1, left, "first line should be the 50-emoji prefix");
        assert_eq!(lines[1].1, right, "second line should be the 50-emoji suffix");

        let reconstructed = lines
            .iter()
            .map(|(_, s)| s.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            reconstructed, body,
            "reconstructed body must match original"
        );
    }

    /// Accented / Latin-extended characters (2-byte UTF-8) wrap correctly.
    #[test]
    fn wrap_is_correct_for_accented_chars() {
        let left: String = "é".repeat(48);
        let right: String = "é".repeat(48);
        let body = format!("{left} {right}extra");

        let mut block = MessageBlock::new(MessageRole::User, body, 0);
        block.set_render_ctx(fallback_ctx());

        let lines = block.wrapped_lines(800.0);

        assert!(lines.len() >= 2, "accented body should wrap: {lines:?}");
        assert_eq!(lines[0].1, left, "first line should be 48 accented chars");
    }

    // ----------------------------------------------------------------
    // Fix 1 -- ANSI stripping
    // ----------------------------------------------------------------

    #[test]
    fn strip_ansi_removes_color_codes() {
        assert_eq!(
            strip_ansi("\x1b[32mhello\x1b[0m"),
            "hello",
            "SGR color sequences must be stripped"
        );
    }

    #[test]
    fn strip_ansi_preserves_normal_text() {
        assert_eq!(
            strip_ansi("hello world"),
            "hello world",
            "plain text must pass through unchanged"
        );
    }

    #[test]
    fn strip_ansi_removes_bold_and_compound_sequences() {
        let input = "\x1b[1m\x1b[33mwarning\x1b[0m";
        assert_eq!(strip_ansi(input), "warning");
    }

    #[test]
    fn wrapped_lines_strips_ansi_before_wrap() {
        let ansi_body = "\x1b[32mhello\x1b[0m";
        let mut block = MessageBlock::new(MessageRole::Agent, ansi_body, 0);
        block.set_render_ctx(fallback_ctx());
        let lines = block.wrapped_lines(800.0);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0].1, "hello",
            "ANSI codes must be stripped before wrapping"
        );
    }

    // ----------------------------------------------------------------
    // Fix 2 -- Code block detection + classification
    // ----------------------------------------------------------------

    #[test]
    fn code_block_lines_classified_as_code() {
        let body = "```\ncode line\n```";
        let mut block = MessageBlock::new(MessageRole::Agent, body, 0);
        block.set_render_ctx(fallback_ctx());
        let lines = block.wrapped_lines(800.0);
        // All three lines (opening fence, code, closing fence) are CodeLine.
        for (kind, text) in &lines {
            assert_eq!(
                *kind,
                LineKind::CodeLine,
                "line '{text}' should be CodeLine"
            );
        }
    }

    #[test]
    fn prose_before_code_block_is_text() {
        let body = "intro\n```\ncode\n```";
        let mut block = MessageBlock::new(MessageRole::Agent, body, 0);
        block.set_render_ctx(fallback_ctx());
        let lines = block.wrapped_lines(800.0);
        assert!(!lines.is_empty());
        assert_eq!(lines[0].0, LineKind::Text, "first line before ``` must be Text");
        assert_eq!(lines[1].0, LineKind::CodeLine, "fence line must be CodeLine");
        assert_eq!(
            lines[2].0,
            LineKind::CodeLine,
            "code content line must be CodeLine"
        );
    }

    #[test]
    fn render_quads_emits_code_bg_for_code_lines() {
        let body = "```\ncode here\n```";
        let mut block = MessageBlock::new(MessageRole::Agent, body, 0);
        block.set_render_ctx(fallback_ctx());
        let rect = make_rect(800.0);
        let quads = block.render_quads(&rect);
        // 1 background quad + 3 code-line quads (fence, code, fence)
        assert!(
            quads.len() > 1,
            "code block should emit additional background quads, got {}",
            quads.len()
        );
    }

    // ----------------------------------------------------------------
    // Fix 3 -- Spinner
    // ----------------------------------------------------------------

    #[test]
    fn spinner_advances_on_call() {
        let mut block = MessageBlock::new(MessageRole::Agent, "working", 0);
        block.set_render_ctx(fallback_ctx());

        assert_eq!(block.spinner_frame, None, "spinner starts as None");

        block.advance_spinner();
        assert_eq!(block.spinner_frame, Some(0), "first advance -> frame 0");

        block.advance_spinner();
        assert_eq!(block.spinner_frame, Some(1), "second advance -> frame 1");
    }

    #[test]
    fn spinner_cycles_through_all_frames() {
        let mut block = MessageBlock::new(MessageRole::Agent, "working", 0);
        block.set_render_ctx(fallback_ctx());
        // First advance: None -> 0.
        // Advance SPINNER_FRAMES.len() more times (10) to wrap back to 0.
        // Total: 11 advances -> frame = 10 % 10 = 0
        for _ in 0..=SPINNER_FRAMES.len() {
            block.advance_spinner();
        }
        // After 11 advances (None->0, 0->1, ..., 9->0):
        //   call 1:  None -> 0
        //   calls 2..11: 0->1->2->...->9->0
        // frame wraps back to 0
        assert_eq!(block.spinner_frame, Some(0), "spinner must wrap around to 0 after 11 advances");
    }

    #[test]
    fn spinner_glyph_appears_in_last_line() {
        let mut block = MessageBlock::new(MessageRole::Agent, "thinking", 0);
        block.set_render_ctx(fallback_ctx());
        block.advance_spinner(); // frame 0 -> '⠋'

        let rect = make_rect(800.0);
        let texts = block.render_text(&rect);
        let last_body = texts.iter().skip(2).last().expect("must have body segment");
        assert!(
            last_body.text.contains('⠋'),
            "last body line must contain spinner glyph, got: {:?}",
            last_body.text
        );
    }

    #[test]
    fn no_spinner_no_extra_chars() {
        let mut block = MessageBlock::new(MessageRole::Agent, "done", 0);
        block.set_render_ctx(fallback_ctx());

        let rect = make_rect(800.0);
        let texts = block.render_text(&rect);
        let last_body = texts.iter().skip(2).last().expect("must have body segment");
        assert_eq!(last_body.text, "done", "no spinner -> text must be unchanged");
    }

    #[test]
    fn spinner_on_empty_body_emits_standalone_segment() {
        let mut block = MessageBlock::new(MessageRole::Agent, "", 0);
        block.set_render_ctx(fallback_ctx());
        block.advance_spinner(); // frame 0

        let rect = make_rect(800.0);
        let texts = block.render_text(&rect);
        // avatar + label + standalone spinner = 3 segments
        assert_eq!(
            texts.len(),
            3,
            "spinner on empty body must emit a standalone segment"
        );
        assert!(
            texts[2].text.contains('⠋'),
            "standalone spinner segment must contain the spinner char"
        );
    }

    // ----------------------------------------------------------------
    // PR #596 review fixes
    // ----------------------------------------------------------------

    /// OSC hyperlink sequences (`ESC ] 8 ; ; URL BEL text ESC ] 8 ; ; BEL`)
    /// must be drained entirely; the visible text must remain.
    #[test]
    fn strip_ansi_drains_osc_hyperlink() {
        let input = "\x1b]8;;https://example.com\x07link text\x1b]8;;\x07 tail";
        assert_eq!(
            strip_ansi(input),
            "link text tail",
            "OSC hyperlink sequences must be drained to their BEL terminator"
        );
    }

    /// OSC sequences terminated by ST (`ESC \`) instead of BEL must also be
    /// drained completely.
    #[test]
    fn strip_ansi_drains_osc_with_string_terminator() {
        let input = "\x1b]0;window title\x1b\\hello";
        assert_eq!(
            strip_ansi(input),
            "hello",
            "OSC sequences terminated by ST (ESC \\) must be drained"
        );
    }

    /// DCS (`ESC P ... ESC \`) sequences must be drained.
    #[test]
    fn strip_ansi_drains_dcs() {
        let input = "before\x1b\x50tmux;\x1b\\after";
        assert_eq!(strip_ansi(input), "beforeafter");
    }

    /// `MessageBlock` body containing an OSC hyperlink must wrap to clean
    /// visible text only.
    #[test]
    fn wrapped_lines_strips_osc_hyperlink() {
        let body = "\x1b]8;;https://example.com\x07click here\x1b]8;;\x07";
        let mut block = MessageBlock::new(MessageRole::Agent, body, 0);
        block.set_render_ctx(fallback_ctx());
        let lines = block.wrapped_lines(800.0);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].1, "click here");
    }

    /// `compute_height` must reserve a row for the standalone spinner when
    /// the body is empty so the next widget in a vertical list does not
    /// overlap it.
    #[test]
    fn compute_height_reserves_row_for_standalone_spinner() {
        let cell_h = fallback_ctx().cell_h();

        let mut without = MessageBlock::new(MessageRole::Agent, "", 0);
        without.set_render_ctx(fallback_ctx());
        assert_eq!(without.compute_height(800.0), cell_h, "no spinner: 1 row");

        let mut with = MessageBlock::new(MessageRole::Agent, "", 0);
        with.set_render_ctx(fallback_ctx());
        with.advance_spinner();
        assert_eq!(
            with.compute_height(800.0),
            2.0 * cell_h,
            "empty body + spinner: 2 rows (label + standalone spinner)"
        );
    }

    /// `render_quads` must report the same height as `compute_height` even
    /// when only a spinner is rendered (no body lines).
    #[test]
    fn render_quads_height_includes_standalone_spinner() {
        let mut block = MessageBlock::new(MessageRole::Agent, "", 0);
        block.set_render_ctx(fallback_ctx());
        block.advance_spinner();
        let rect = make_rect(800.0);
        let quads = block.render_quads(&rect);
        assert_eq!(
            quads[0].size[1],
            block.compute_height(rect.width),
            "background quad height must match compute_height when spinner is active"
        );
    }
}
