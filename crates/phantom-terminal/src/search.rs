//! Scrollback search index for find-in-terminal (Cmd+F).
//!
//! [`ScrollbackIndex`] performs a case-insensitive substring search over every
//! row in the grid — both visible lines and scrollback history — and caches the
//! results so the renderer can highlight match spans without re-scanning every
//! frame.
//!
//! # Row numbering
//!
//! Alacritty's `Grid` uses a signed `Line` type:
//! - Negative values (`Line(-1)`, `Line(-2)`, …) index scrollback rows above
//!   the current viewport, counting *up* from the top visible row.
//! - Non-negative values index visible rows within the current viewport.
//!
//! `ScrollbackIndex` uses these same `i32` row indices as keys so callers can
//! pass them straight to the renderer without conversion.

use std::collections::HashMap;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Cell;

// ─────────────────────────────────────────────────────────────────────────────
// ScrollbackIndex
// ─────────────────────────────────────────────────────────────────────────────

/// Cached results of a find-in-terminal search across all grid rows.
///
/// Call [`ScrollbackIndex::index`] after every query change to rebuild the
/// cache. The renderer then calls [`ScrollbackIndex::matches_in_row`] for each
/// row it is about to draw.
#[derive(Debug, Default)]
pub struct ScrollbackIndex {
    /// Map from row index (signed `Line` value) to sorted, non-overlapping
    /// byte-column spans `(col_start, col_end)` within that row.
    matches: HashMap<i32, Vec<(usize, usize)>>,
    /// Flat list of all matches in grid order (top-to-bottom, left-to-right),
    /// stored as `(row, col_start)` so callers can jump to the nth match.
    all_matches: Vec<(i32, usize)>,
    /// The query that produced these results (cached to detect no-ops).
    last_query: String,
}

impl ScrollbackIndex {
    /// Create an empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild the index by scanning all rows of `grid` for `query`.
    ///
    /// The search is case-insensitive and treats each terminal row as a plain
    /// string of characters (leading/trailing spaces included — they can be
    /// part of a meaningful match in, e.g., formatted table output).
    ///
    /// If `query` is empty the index is cleared and all match counts reset to
    /// zero.
    pub fn index(
        &mut self,
        grid: &alacritty_terminal::grid::Grid<Cell>,
        query: &str,
    ) {
        if query == self.last_query {
            return; // Nothing changed.
        }
        self.last_query = query.to_owned();
        self.matches.clear();
        self.all_matches.clear();

        if query.is_empty() {
            return;
        }

        let query_lower = query.to_lowercase();
        let screen_lines = grid.screen_lines() as i32;
        let history_size = grid.history_size() as i32;

        // Iterate from the top of scrollback (most negative line index) down
        // through all visible lines.  The topmost scrollback line is at
        // `Line(-(history_size))`.
        for row_i in (-history_size)..screen_lines {
            let line = Line(row_i);

            // Build the row's character string.
            let row = &grid[line];
            let row_len = row.len();
            let mut row_str = String::with_capacity(row_len);
            for col in 0..row_len {
                row_str.push(row[Column(col)].c);
            }

            // Case-insensitive search in the lowercased version.  We need to
            // map byte offsets in the lowercased string back to column indices.
            // Since each terminal cell is exactly one character (and we are
            // lowercasing char-by-char), char index == column index.
            let row_lower = row_str.to_lowercase();

            let spans = find_non_overlapping(&row_lower, &query_lower);
            if !spans.is_empty() {
                for &(start, _end) in &spans {
                    self.all_matches.push((row_i, start));
                }
                self.matches.insert(row_i, spans);
            }
        }
    }

    /// Return the match spans (col_start, col_end) for `row`.
    ///
    /// Returns an empty slice when there are no matches in `row` or when the
    /// index has not been built yet.
    #[must_use]
    pub fn matches_in_row(&self, row: i32) -> &[(usize, usize)] {
        self.matches.get(&row).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Total number of matches across all rows.
    #[must_use]
    pub fn total_matches(&self) -> usize {
        self.all_matches.len()
    }

    /// Return the `(row, col_start)` of the n-th match (0-indexed).
    ///
    /// Returns `None` when `n >= total_matches()`.
    #[must_use]
    pub fn nth_match(&self, n: usize) -> Option<(i32, usize)> {
        self.all_matches.get(n).copied()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Helper
// ─────────────────────────────────────────────────────────────────────────────

/// Find all non-overlapping occurrences of `needle` in `haystack` and return
/// them as `(start_char_idx, end_char_idx)` spans.
///
/// Both strings must be pre-lowercased by the caller for case-insensitive
/// matching.  Since terminal cells are single chars, byte offset == char index.
fn find_non_overlapping(haystack: &str, needle: &str) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs_start = start + pos;
        let abs_end = abs_start + needle.len();
        spans.push((abs_start, abs_end));
        start = abs_end; // advance past this match (non-overlapping)
    }
    spans
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrollback_index_no_matches_returns_zero() {
        let idx = ScrollbackIndex::new();
        assert_eq!(idx.total_matches(), 0);
        assert_eq!(idx.matches_in_row(0), &[]);
        assert_eq!(idx.nth_match(0), None);
    }

    #[test]
    fn find_non_overlapping_basic() {
        let spans = find_non_overlapping("hello world hello", "hello");
        assert_eq!(spans, vec![(0, 5), (12, 17)]);
    }

    #[test]
    fn find_non_overlapping_case_sensitive_contract() {
        // Caller is responsible for lowercasing; this tests the raw function.
        let spans = find_non_overlapping("abc ABC", "abc");
        assert_eq!(spans, vec![(0, 3)]);
    }

    #[test]
    fn find_non_overlapping_empty_needle() {
        let spans = find_non_overlapping("anything", "");
        assert!(spans.is_empty());
    }

    #[test]
    fn find_non_overlapping_no_match() {
        let spans = find_non_overlapping("hello", "xyz");
        assert!(spans.is_empty());
    }

    #[test]
    fn scrollback_index_case_insensitive_via_find_helper() {
        // The helper itself is case-sensitive; the index pre-lowercases both.
        // Test the lowercasing path directly.
        let haystack = "Hello World".to_lowercase();
        let needle = "hello".to_lowercase();
        let spans = find_non_overlapping(&haystack, &needle);
        assert_eq!(spans, vec![(0, 5)]);
    }
}
