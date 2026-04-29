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
pub(crate) const AVATAR_W: f32 = 16.0;
/// Horizontal gap between the avatar column and the body text column.
pub(crate) const AVATAR_GAP: f32 = 8.0;

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
///         …
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
}

impl MessageBlock {
    /// Construct a `MessageBlock` with `RenderCtx::fallback()` metrics.
    pub fn new(role: MessageRole, body: impl Into<String>, timestamp_ms: u64) -> Self {
        Self {
            role,
            body: body.into(),
            timestamp_ms,
            ctx: RenderCtx::fallback(),
        }
    }

    /// Update the live render context so wrap calculations reflect the
    /// current font metrics.
    pub fn set_render_ctx(&mut self, ctx: RenderCtx) {
        self.ctx = ctx;
    }

    /// Compute the pixel height this block occupies for a given `rect_width`.
    ///
    /// Height = `(1 + wrapped_line_count) * cell_h`, where the leading `1`
    /// accounts for the role-label row.
    pub fn compute_height(&self, rect_width: f32) -> f32 {
        let line_count = self.wrapped_lines(rect_width).len();
        // role-label row (1) + body rows
        (1 + line_count) as f32 * self.ctx.cell_h()
    }

    /// Wrap `body` into lines that fit within `rect_width`, honouring the
    /// avatar column offset.
    ///
    /// The available body-column width (in characters) is:
    /// ```text
    /// floor((rect_width - AVATAR_W - AVATAR_GAP) / cell_w)
    /// ```
    /// An empty body returns an empty `Vec`.  A `cell_w` of `0.0` (degenerate
    /// context) is treated as `1.0` to avoid division by zero.
    pub fn wrapped_lines(&self, rect_width: f32) -> Vec<String> {
        let cell_w = self.ctx.cell_w().max(1.0);
        let body_px = (rect_width - AVATAR_W - AVATAR_GAP).max(0.0);
        let cols = (body_px / cell_w).floor() as usize;

        if cols == 0 || self.body.is_empty() {
            return Vec::new();
        }

        let mut lines = Vec::new();
        let mut remaining = self.body.as_str();

        while !remaining.is_empty() {
            let chars: Vec<char> = remaining.chars().collect();
            if chars.len() <= cols {
                lines.push(remaining.to_owned());
                break;
            }
            // Find the last space within `cols` chars to word-wrap cleanly.
            // Use char-index arithmetic throughout — `rfind` on a `&str`
            // returns a *byte* offset, which is wrong for multibyte text.
            let break_at = chars[..cols]
                .iter()
                .enumerate()
                .rfind(|&(_, c)| *c == ' ')
                .map(|(i, _)| i)
                .filter(|&i| i > 0)
                .unwrap_or(cols); // hard-break if no space found

            lines.push(chars[..break_at].iter().collect());
            // Advance past the space (if any) so it doesn't start the next line.
            let skip = if break_at < chars.len() && chars[break_at] == ' ' {
                break_at + 1
            } else {
                break_at
            };
            // Build remaining from the unconsumed chars.
            remaining = &remaining[chars[..skip].iter().map(|c| c.len_utf8()).sum::<usize>()..];
        }

        lines
    }
}

impl Widget for MessageBlock {
    /// Emits a single full-width background quad in `surface_recessed` so
    /// each block is visually distinct from the raw terminal surface.
    fn render_quads(&self, rect: &Rect) -> Vec<QuadInstance> {
        let t = Tokens::phosphor(self.ctx);
        vec![QuadInstance {
            pos: [rect.x, rect.y],
            size: [rect.width, self.compute_height(rect.width)],
            color: t.colors.surface_recessed,
            border_radius: 0.0,
        }]
    }

    /// Emits text segments:
    /// 1. Avatar initial in the glyph column (role color, dim background implied
    ///    by `surface_recessed` quad).
    /// 2. Role label on row 0 of the body column (role color).
    /// 3. One `TextSegment` per wrapped body line (text_primary color, slightly
    ///    dimmer than the label so the label stands out as a header).
    fn render_text(&self, rect: &Rect) -> Vec<TextSegment> {
        let t = Tokens::phosphor(self.ctx);
        let role_color = self.role.color(&t);
        let cell_h = self.ctx.cell_h();

        // Column positions.
        let avatar_x = rect.x;
        let body_x = rect.x + AVATAR_W + AVATAR_GAP;

        // Row 0 — avatar initial + role label.
        let row0_y = rect.y + cell_h * 0.5 - cell_h * 0.5; // == rect.y (top of row)
        let label_y = row0_y;

        let mut segments = Vec::new();

        // Avatar glyph (initial letter).
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

        // Body lines — each on its own row after the label row.
        let body_lines = self.wrapped_lines(rect.width);
        for (i, line) in body_lines.iter().enumerate() {
            let line_y = rect.y + (i + 1) as f32 * cell_h;
            segments.push(TextSegment {
                text: line.clone(),
                x: body_x,
                y: line_y,
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
        Rect { x: 0.0, y: 0.0, width, height: 200.0 }
    }

    // ----------------------------------------------------------------
    // Role → color mapping
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

    /// Single-line body → height = 2 rows (label + 1 body line).
    #[test]
    fn single_line_body_height_is_two_rows() {
        let mut block = MessageBlock::new(MessageRole::User, "hello", 0);
        block.set_render_ctx(fallback_ctx());
        // Width wide enough that "hello" (5 chars) fits in one line.
        // Body column width = 800 - 16 - 8 = 776 px → 97 chars at cell_w=8
        let h = block.compute_height(800.0);
        let cell_h = fallback_ctx().cell_h();
        assert_eq!(h, 2.0 * cell_h, "single-line body: expected 2 rows, got {h}");
    }

    /// Multiline body grows height proportionally.
    #[test]
    fn multiline_body_height_grows_with_lines() {
        // cell_w = 8, body col = 800 - 16 - 8 = 776 px → 97 cols
        // body = 200 'a' chars → ceil(200/97) = 3 lines
        let body: String = "a".repeat(200);
        let mut block = MessageBlock::new(MessageRole::Agent, body, 0);
        block.set_render_ctx(fallback_ctx());

        let ctx = fallback_ctx();
        let lines = block.wrapped_lines(800.0);
        let expected_h = (1 + lines.len()) as f32 * ctx.cell_h();
        let actual_h = block.compute_height(800.0);
        assert_eq!(actual_h, expected_h);
        // Also verify the block is taller than the two-row single-line case.
        assert!(actual_h > 2.0 * ctx.cell_h(), "multiline body should be taller than 2 rows");
    }

    /// Empty body → only the label row → height = 1 row.
    #[test]
    fn empty_body_height_is_one_row() {
        let mut block = MessageBlock::new(MessageRole::System, "", 0);
        block.set_render_ctx(fallback_ctx());
        let cell_h = fallback_ctx().cell_h();
        assert_eq!(block.compute_height(800.0), cell_h, "empty body: expected 1 row");
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
        assert_eq!(lines[0], body);
    }

    /// A body of exactly `cols` chars must fit in a single line.
    #[test]
    fn body_exactly_cols_wide_fits_one_line() {
        // cell_w=8, rect=800 → body_col=776px → 97 cols
        let body: String = "x".repeat(97);
        let mut block = MessageBlock::new(MessageRole::User, body.clone(), 0);
        block.set_render_ctx(fallback_ctx());
        let lines = block.wrapped_lines(800.0);
        assert_eq!(lines.len(), 1, "body of exactly `cols` chars should be one line");
        assert_eq!(lines[0], body);
    }

    /// A body of `cols+1` chars must split into two lines.
    #[test]
    fn body_one_over_cols_wraps_to_two_lines() {
        // 97 + 1 = 98 'x' chars, no spaces → hard break at 97
        let body: String = "x".repeat(98);
        let mut block = MessageBlock::new(MessageRole::User, body, 0);
        block.set_render_ctx(fallback_ctx());
        let lines = block.wrapped_lines(800.0);
        assert_eq!(lines.len(), 2, "98 chars at 97-col width should produce 2 lines");
    }

    /// Word-wrap: prefer breaking at a space rather than mid-word.
    #[test]
    fn wrap_prefers_word_boundary() {
        // Build a line just over 97 chars with a space somewhere inside.
        // "word " * 10 = 50 chars, then another long word to force wrap.
        // Use a simple scenario: 96 chars before a space, then more text.
        let word_a: String = "a".repeat(48);
        let word_b: String = "b".repeat(48);
        let body = format!("{word_a} {word_b}cccc");
        // word_a + " " + word_b = 97 chars; adding "cccc" forces a second line.
        let mut block = MessageBlock::new(MessageRole::User, body, 0);
        block.set_render_ctx(fallback_ctx());
        let lines = block.wrapped_lines(800.0);
        // First line should end at the space boundary (48 a's + space is ≤97 cols).
        assert!(
            lines.len() >= 2,
            "should have wrapped at space boundary: {lines:?}"
        );
        // First line must not start with 'b'.
        assert!(
            !lines[0].starts_with('b'),
            "first line should not start with 'b': {:?}",
            lines[0]
        );
    }

    /// Very narrow rect (body col ≤ 0 px) → no lines (degenerate).
    #[test]
    fn zero_width_rect_produces_no_lines() {
        let mut block = MessageBlock::new(MessageRole::User, "hello", 0);
        block.set_render_ctx(fallback_ctx());
        // rect_width ≤ AVATAR_W + AVATAR_GAP → body_px = 0 → cols = 0
        let lines = block.wrapped_lines(AVATAR_W + AVATAR_GAP);
        assert!(lines.is_empty(), "zero-width body column should produce no lines");
    }

    // ----------------------------------------------------------------
    // Widget trait — render_quads
    // ----------------------------------------------------------------

    #[test]
    fn render_quads_emits_exactly_one_background_quad() {
        let mut block = MessageBlock::new(MessageRole::Agent, "hi", 0);
        block.set_render_ctx(fallback_ctx());
        let rect = make_rect(800.0);
        let quads = block.render_quads(&rect);
        assert_eq!(quads.len(), 1, "should emit exactly one background quad");
        let t = Tokens::phosphor(fallback_ctx());
        assert_eq!(quads[0].color, t.colors.surface_recessed, "background should use surface_recessed");
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
    // Widget trait — render_text
    // ----------------------------------------------------------------

    /// render_text must always emit at least: avatar + label (2 segments).
    #[test]
    fn render_text_always_has_avatar_and_label() {
        let mut block = MessageBlock::new(MessageRole::System, "", 0);
        block.set_render_ctx(fallback_ctx());
        let rect = make_rect(800.0);
        let texts = block.render_text(&rect);
        assert!(texts.len() >= 2, "should have at least avatar + label segments");
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
        assert_eq!(label.text, "Tool Result", "ToolResult label should be 'Tool Result'");
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
        assert_eq!(texts[0].color, expected, "avatar color should match role color");
        assert_eq!(texts[1].color, expected, "label color should match role color");
    }

    /// Body text segments use text_primary color.
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
                seg.color,
                tokens.colors.text_primary,
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
        // Label (index 1) and body lines (index 2+) all start at body_x.
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
        assert_eq!(unique.len(), initials.len(), "all role initials must be distinct");
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
            assert!(!role.label().is_empty(), "label for {role:?} must not be empty");
        }
    }

    // ----------------------------------------------------------------
    // Multibyte / Unicode regression tests
    // ----------------------------------------------------------------

    /// Wrapping a body that contains multibyte characters (emoji, accented
    /// letters, CJK) must not panic and must produce correct line boundaries.
    ///
    /// With cell_w=8, rect=800 → body col = (800 - 16 - 8) / 8 = 97 cols.
    /// Each emoji is one *char* but 4 UTF-8 bytes.  Using a byte offset from
    /// `rfind` as a char index would either panic (out-of-bounds) or slice at
    /// a non-char boundary — this test catches both failure modes.
    #[test]
    fn wrap_is_correct_for_multibyte_body() {
        // 50 emoji chars + space + 50 more emoji → 101 chars total.
        // The space is at char index 50, well inside the 97-col limit,
        // so the wrap should break there and NOT at a mid-emoji byte offset.
        let left: String = "🦀".repeat(50); // 50 chars, 200 bytes
        let right: String = "🦀".repeat(50); // 50 chars, 200 bytes
        let body = format!("{left} {right}"); // 101 chars, 401 bytes

        let mut block = MessageBlock::new(MessageRole::Agent, body.clone(), 0);
        block.set_render_ctx(fallback_ctx());

        // Must not panic:
        let lines = block.wrapped_lines(800.0);

        // The space lives at char index 50 (≤ 97), so we expect a soft wrap
        // there, giving us exactly 2 lines.
        assert_eq!(lines.len(), 2, "emoji body should wrap into 2 lines: {lines:?}");

        // First line must be exactly the 50 crab emoji.
        assert_eq!(lines[0], left, "first line should be the 50-emoji prefix");

        // Second line must be the remaining 50 emoji (space consumed by wrap).
        assert_eq!(lines[1], right, "second line should be the 50-emoji suffix");

        // Sanity-check that the output round-trips back to the original body
        // (minus the joining space which is consumed).
        let reconstructed = lines.join(" ");
        assert_eq!(reconstructed, body, "reconstructed body must match original");
    }

    /// Accented / Latin-extended characters (2-byte UTF-8) wrap correctly.
    #[test]
    fn wrap_is_correct_for_accented_chars() {
        // 'é' is U+00E9, 2 bytes in UTF-8 but 1 char.
        // 48 × 'é' + space + 48 × 'é' = 97 chars exactly at the break point.
        let left: String = "é".repeat(48);
        let right: String = "é".repeat(48);
        let body = format!("{left} {right}extra"); // space at index 48 → within 97 cols

        let mut block = MessageBlock::new(MessageRole::User, body, 0);
        block.set_render_ctx(fallback_ctx());

        // Must not panic:
        let lines = block.wrapped_lines(800.0);

        assert!(lines.len() >= 2, "accented body should wrap: {lines:?}");
        assert_eq!(lines[0], left, "first line should be 48 accented chars");
    }
}
