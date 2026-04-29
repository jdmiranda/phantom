//! Selection highlight primitive.
//!
//! [`SelectionRect`] is the single source of truth for any text-selection or
//! message-block-selection state in the Phantom UI. It is completely
//! data-only: it stores the anchor and focus coordinates and hands pixel
//! rectangles back to the renderer on demand.
//!
//! ## Coordinate convention
//!
//! `(col, row)` are **zero-based** column and row indices in the terminal or
//! message-block grid. They are **not** pixel coordinates — call
//! [`SelectionRect::pixel_rects`] with the current [`RenderCtx`] to convert.
//!
//! ## Selection modes
//!
//! | Mode | Shape |
//! |------|-------|
//! | [`SelectionMode::Rectangular`] | A fixed-width box from top-left to bottom-right |
//! | [`SelectionMode::FlowingText`] | First partial row → full middle rows → last partial row (terminal-style) |
//!
//! ## Color token
//!
//! All callers must source the fill color from `Tokens::colors::selection_bg`.
//! No raw RGBA constant is embedded here — a theme swap automatically
//! recolors every selection rectangle.

use crate::RenderCtx;

// ---------------------------------------------------------------------------
// SelectionMode
// ---------------------------------------------------------------------------

/// How a multi-line selection should be shaped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectionMode {
    /// Rectangular column-aligned box.
    ///
    /// All rows share exactly the same `[col_start, col_end]` columns, even
    /// when the selection spans multiple lines. This mirrors the "block
    /// selection" found in many editors (e.g. Vim's visual-block mode).
    Rectangular,

    /// Flowing text selection.
    ///
    /// - First row: from `col_start` to end-of-line.
    /// - Middle rows: full width.
    /// - Last row: from column 0 to `col_end`.
    ///
    /// This is the behaviour expected in a terminal or a prose editor.
    #[default]
    FlowingText,
}

// ---------------------------------------------------------------------------
// SelectionRect
// ---------------------------------------------------------------------------

/// A text or block selection anchored at two (col, row) grid coordinates.
///
/// # Normalization
///
/// [`SelectionRect::new`] normalizes coordinates so that
/// `start.row ≤ end.row` and, when on the same row, `start.col ≤ end.col`.
/// This means callers may pass anchor and focus in any order (e.g. the user
/// can drag backwards).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionRect {
    /// Normalized start coordinate `(col, row)` — always ≤ `end`.
    pub start: (usize, usize),
    /// Normalized end coordinate `(col, row)` — always ≥ `start`.
    pub end: (usize, usize),
    /// How to shape a multi-line selection.
    pub mode: SelectionMode,
}

impl SelectionRect {
    /// Construct a selection from two arbitrary grid coordinates.
    ///
    /// Coordinates are normalized: `start` is always the logically earlier
    /// position (top-left in [`SelectionMode::Rectangular`]; earlier in
    /// reading order in [`SelectionMode::FlowingText`]).
    pub fn new(a: (usize, usize), b: (usize, usize), mode: SelectionMode) -> Self {
        let (start, end) = normalize(a, b);
        Self { start, end, mode }
    }

    /// `true` when the selection covers exactly one cell (zero-width is
    /// treated as an insertion point, not a selection).
    pub fn is_collapsed(&self) -> bool {
        self.start == self.end
    }

    /// Number of rows spanned by the selection (inclusive, ≥ 1).
    pub fn row_span(&self) -> usize {
        self.end.1 - self.start.1 + 1
    }

    /// Convert this selection into one or more pixel rectangles given live
    /// cell metrics and the total number of columns in the grid.
    ///
    /// Each returned `PixelRect` is in physical pixels relative to the window
    /// origin.
    ///
    /// # Parameters
    ///
    /// - `ctx` — live render context providing `cell_w()` and `cell_h()`.
    /// - `origin` — `(x, y)` pixel offset of the grid's top-left corner.
    /// - `grid_cols` — total column count (used by `FlowingText` mode to
    ///   compute "full row" widths).
    /// - `alpha` — fill alpha (callers should source this from the
    ///   `selection_bg` token; `0.5` is a reasonable default).
    pub fn pixel_rects(
        &self,
        ctx: RenderCtx,
        origin: (f32, f32),
        grid_cols: usize,
        alpha: f32,
    ) -> Vec<PixelRect> {
        let cw = ctx.cell_w();
        let ch = ctx.cell_h();
        let (ox, oy) = origin;

        let (sc, sr) = self.start;
        let (ec, er) = self.end;

        match self.mode {
            SelectionMode::Rectangular => {
                // One rectangle spanning all rows at the same column range.
                let x = ox + sc as f32 * cw;
                let y = oy + sr as f32 * ch;
                let w = (ec - sc + 1) as f32 * cw;
                let h = (er - sr + 1) as f32 * ch;
                vec![PixelRect { x, y, width: w, height: h, alpha }]
            }
            SelectionMode::FlowingText => {
                let mut rects = Vec::new();

                if sr == er {
                    // Single row — simple range.
                    let x = ox + sc as f32 * cw;
                    let y = oy + sr as f32 * ch;
                    let w = (ec - sc + 1) as f32 * cw;
                    rects.push(PixelRect { x, y, width: w, height: ch, alpha });
                } else {
                    // First (partial) row: sc → end-of-row.
                    {
                        let x = ox + sc as f32 * cw;
                        let y = oy + sr as f32 * ch;
                        let cols_remaining = grid_cols.saturating_sub(sc);
                        let w = cols_remaining as f32 * cw;
                        rects.push(PixelRect { x, y, width: w, height: ch, alpha });
                    }

                    // Full middle rows.
                    for row in (sr + 1)..er {
                        let x = ox;
                        let y = oy + row as f32 * ch;
                        let w = grid_cols as f32 * cw;
                        rects.push(PixelRect { x, y, width: w, height: ch, alpha });
                    }

                    // Last (partial) row: col 0 → ec.
                    {
                        let x = ox;
                        let y = oy + er as f32 * ch;
                        let w = (ec + 1) as f32 * cw;
                        rects.push(PixelRect { x, y, width: w, height: ch, alpha });
                    }
                }

                rects
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PixelRect
// ---------------------------------------------------------------------------

/// A pixel-space rectangle produced by [`SelectionRect::pixel_rects`].
///
/// The renderer composites these over the terminal surface using the
/// `selection_bg` token color at the stored `alpha`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PixelRect {
    /// Left edge in pixels (from the window origin).
    pub x: f32,
    /// Top edge in pixels.
    pub y: f32,
    /// Width in pixels.
    pub width: f32,
    /// Height in pixels.
    pub height: f32,
    /// Alpha opacity (0.0 = transparent, 1.0 = opaque).
    ///
    /// Sourced from the theme's `selection_bg` alpha channel.
    pub alpha: f32,
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Normalize two `(col, row)` grid coordinates so the first is always the
/// logically earlier position.
///
/// Ordering: rows first (top wins), then columns within a row (left wins).
fn normalize(a: (usize, usize), b: (usize, usize)) -> ((usize, usize), (usize, usize)) {
    let (ac, ar) = a;
    let (bc, br) = b;

    if ar < br {
        (a, b)
    } else if ar > br {
        (b, a)
    } else {
        // Same row — order by column.
        if ac <= bc { (a, b) } else { (b, a) }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Helpers ----------------------------------------------------------------

    fn ctx() -> RenderCtx {
        RenderCtx::new((8.0, 16.0), 1.0)
    }

    fn sel(a: (usize, usize), b: (usize, usize), mode: SelectionMode) -> SelectionRect {
        SelectionRect::new(a, b, mode)
    }

    // -- Normalization -------------------------------------------------------

    /// Forward order: start ≤ end, no flip needed.
    #[test]
    fn forward_order_unchanged() {
        let s = sel((2, 0), (5, 3), SelectionMode::Rectangular);
        assert_eq!(s.start, (2, 0));
        assert_eq!(s.end, (5, 3));
    }

    /// Reversed rows: start row > end row → must flip.
    #[test]
    fn reversed_rows_flips_start_and_end() {
        let s = sel((5, 3), (2, 0), SelectionMode::Rectangular);
        assert_eq!(s.start, (2, 0));
        assert_eq!(s.end, (5, 3));
    }

    /// Same row, reversed columns → must flip columns only.
    #[test]
    fn same_row_reversed_cols_flips() {
        let s = sel((7, 2), (3, 2), SelectionMode::FlowingText);
        assert_eq!(s.start.0, 3, "col start should be left-most");
        assert_eq!(s.end.0, 7, "col end should be right-most");
        assert_eq!(s.start.1, 2, "row unchanged");
        assert_eq!(s.end.1, 2, "row unchanged");
    }

    /// Identical coordinates → collapsed (insertion point).
    #[test]
    fn collapsed_when_same_point() {
        let s = sel((4, 1), (4, 1), SelectionMode::FlowingText);
        assert!(s.is_collapsed());
    }

    /// Different coordinates → not collapsed.
    #[test]
    fn not_collapsed_when_range() {
        let s = sel((0, 0), (1, 0), SelectionMode::FlowingText);
        assert!(!s.is_collapsed());
    }

    // -- Row span ------------------------------------------------------------

    #[test]
    fn single_row_span_is_one() {
        let s = sel((0, 5), (10, 5), SelectionMode::FlowingText);
        assert_eq!(s.row_span(), 1);
    }

    #[test]
    fn multi_row_span_inclusive() {
        let s = sel((0, 2), (0, 7), SelectionMode::FlowingText);
        assert_eq!(s.row_span(), 6); // rows 2, 3, 4, 5, 6, 7
    }

    // -- SelectionMode enum --------------------------------------------------

    #[test]
    fn default_mode_is_flowing_text() {
        assert_eq!(SelectionMode::default(), SelectionMode::FlowingText);
    }

    // -- Rectangular pixel_rects ---------------------------------------------

    #[test]
    fn rectangular_single_rect_covers_all_rows() {
        let s = sel((2, 1), (5, 3), SelectionMode::Rectangular);
        let rects = s.pixel_rects(ctx(), (0.0, 0.0), 80, 0.5);

        // Rectangular always returns one quad.
        assert_eq!(rects.len(), 1);

        let r = &rects[0];
        let cw = 8.0_f32;
        let ch = 16.0_f32;

        assert_eq!(r.x, 2.0 * cw, "x from start col");
        assert_eq!(r.y, 1.0 * ch, "y from start row");
        assert_eq!(r.width, 4.0 * cw, "cols 2-5 inclusive = 4 cols");
        assert_eq!(r.height, 3.0 * ch, "rows 1-3 inclusive = 3 rows");
        assert_eq!(r.alpha, 0.5);
    }

    #[test]
    fn rectangular_with_origin_offset() {
        let s = sel((0, 0), (2, 1), SelectionMode::Rectangular);
        let rects = s.pixel_rects(ctx(), (100.0, 50.0), 80, 0.5);

        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].x, 100.0, "origin x applied");
        assert_eq!(rects[0].y, 50.0, "origin y applied");
    }

    // -- FlowingText pixel_rects --------------------------------------------

    /// Single row — same as rectangular: just one rect.
    #[test]
    fn flowing_single_row_one_rect() {
        let s = sel((3, 2), (7, 2), SelectionMode::FlowingText);
        let rects = s.pixel_rects(ctx(), (0.0, 0.0), 80, 0.5);

        assert_eq!(rects.len(), 1);
        let r = &rects[0];
        assert_eq!(r.x, 3.0 * 8.0);
        assert_eq!(r.width, 5.0 * 8.0); // cols 3..=7
    }

    /// Two-row selection: first partial + last partial → 2 rects (no middle).
    #[test]
    fn flowing_two_rows_two_rects() {
        let s = sel((4, 1), (6, 2), SelectionMode::FlowingText);
        let grid_cols = 80;
        let rects = s.pixel_rects(ctx(), (0.0, 0.0), grid_cols, 0.5);

        // first row (partial) + last row (partial) = 2
        assert_eq!(rects.len(), 2);

        let cw = 8.0_f32;
        let ch = 16.0_f32;

        // First rect: col 4 → end of row (80 cols total).
        let first = &rects[0];
        assert_eq!(first.x, 4.0 * cw);
        assert_eq!(first.y, 1.0 * ch);
        assert_eq!(first.width, (grid_cols - 4) as f32 * cw);

        // Last rect: col 0 → col 6 (inclusive).
        let last = &rects[1];
        assert_eq!(last.x, 0.0);
        assert_eq!(last.y, 2.0 * ch);
        assert_eq!(last.width, 7.0 * cw); // cols 0..=6 = 7 cols
    }

    /// Three-row selection: partial + full middle + partial → 3 rects.
    #[test]
    fn flowing_three_rows_three_rects() {
        let s = sel((2, 0), (5, 2), SelectionMode::FlowingText);
        let grid_cols = 40;
        let rects = s.pixel_rects(ctx(), (0.0, 0.0), grid_cols, 0.5);

        // 3 rows → 3 rects
        assert_eq!(rects.len(), 3);

        let cw = 8.0_f32;
        let ch = 16.0_f32;

        // Row 0: col 2 → end.
        assert_eq!(rects[0].x, 2.0 * cw);
        assert_eq!(rects[0].y, 0.0);
        assert_eq!(rects[0].width, (grid_cols - 2) as f32 * cw);

        // Row 1: full row.
        assert_eq!(rects[1].x, 0.0);
        assert_eq!(rects[1].y, 1.0 * ch);
        assert_eq!(rects[1].width, grid_cols as f32 * cw);

        // Row 2: col 0 → col 5.
        assert_eq!(rects[2].x, 0.0);
        assert_eq!(rects[2].y, 2.0 * ch);
        assert_eq!(rects[2].width, 6.0 * cw); // cols 0..=5
    }

    /// Four-row selection: two full middle rows.
    #[test]
    fn flowing_four_rows_four_rects() {
        let s = sel((0, 0), (0, 3), SelectionMode::FlowingText);
        let rects = s.pixel_rects(ctx(), (0.0, 0.0), 80, 0.5);
        // first + middle (rows 1, 2) + last = 4
        assert_eq!(rects.len(), 4);
    }

    // -- Click-drag → start+end mapping ------------------------------------

    /// Simulated left-to-right drag: mouse pressed at (col=1,row=0), released
    /// at (col=10,row=3). The SelectionRect must record these in document
    /// order regardless of the API call order.
    #[test]
    fn click_drag_left_to_right_forward() {
        let anchor = (1_usize, 0_usize); // mousedown
        let focus = (10_usize, 3_usize); // mouseup
        let s = SelectionRect::new(anchor, focus, SelectionMode::FlowingText);
        assert_eq!(s.start, anchor);
        assert_eq!(s.end, focus);
    }

    /// Simulated right-to-left (backward) drag: mouse pressed at
    /// (col=10,row=3), released at (col=1,row=0). The normalized start must
    /// still be the top-left corner.
    #[test]
    fn click_drag_right_to_left_normalizes() {
        let anchor = (10_usize, 3_usize);
        let focus = (1_usize, 0_usize);
        let s = SelectionRect::new(anchor, focus, SelectionMode::FlowingText);
        assert_eq!(s.start, (1, 0));
        assert_eq!(s.end, (10, 3));
    }

    /// Drag within a single row, from right to left.
    #[test]
    fn click_drag_same_row_backward() {
        let s = SelectionRect::new((9, 5), (3, 5), SelectionMode::FlowingText);
        assert_eq!(s.start.0, 3, "leftmost col is start");
        assert_eq!(s.end.0, 9, "rightmost col is end");
    }

    // -- Alpha stored correctly ----------------------------------------------

    #[test]
    fn pixel_rect_stores_alpha() {
        let s = sel((0, 0), (3, 0), SelectionMode::FlowingText);
        let rects = s.pixel_rects(ctx(), (0.0, 0.0), 80, 0.35);
        assert!(!rects.is_empty());
        for r in &rects {
            assert!((r.alpha - 0.35).abs() < 1e-6, "alpha mismatch: {}", r.alpha);
        }
    }

    // -- PixelRect struct round-trips ----------------------------------------

    #[test]
    fn pixel_rect_copy_clone_eq() {
        let pr = PixelRect { x: 1.0, y: 2.0, width: 32.0, height: 16.0, alpha: 0.5 };
        let pr2 = pr;          // Copy
        let pr3 = pr.clone();  // Clone
        assert_eq!(pr, pr2);
        assert_eq!(pr, pr3);
    }
}
