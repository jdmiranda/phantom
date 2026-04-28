//! Free-function text measurement for variable-width strings.
//!
//! This module wraps cosmic-text's shaping pipeline to give widgets accurate
//! pixel widths and heights for strings that may contain wide characters,
//! emoji, mixed scripts, or which need soft-wrapping.
//!
//! `TextRenderer::measure_cell` is sufficient when you can assume monospace
//! cells; use [`measure_text`] anywhere that assumption breaks (intent input,
//! message blocks, agent output panes).
//!
//! # Example
//! ```ignore
//! use cosmic_text::FontSystem;
//! use phantom_renderer::text_metrics::measure_text;
//!
//! let mut fs = FontSystem::new();
//! let m = measure_text(&mut fs, "hello world", 14.0, 16.8, None);
//! assert!(m.width > 0.0);
//! assert_eq!(m.line_count, 1);
//! ```
//!
//! Caching is the caller's responsibility — calling this every frame on the
//! same string is wasteful.
use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping};

/// Result of shaping and laying out a string.
///
/// `width` is the maximum pixel advance across all visual lines (i.e. the
/// rightmost glyph edge). `height` is `line_count * line_height`. `line_count`
/// is the number of laid-out visual lines after wrapping.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextMeasurement {
    /// Maximum visual line width in pixels.
    pub width: f32,
    /// Total laid-out height in pixels (`line_count * line_height`).
    pub height: f32,
    /// Number of laid-out visual lines (1 for non-wrapping, more if wrapped).
    pub line_count: usize,
}

/// Measure shaped text using cosmic-text.
///
/// Used by widgets that need accurate widths for non-monospace text or strings
/// that may include emoji, wide characters, or soft-wrapping.
///
/// Matches the family/shaping settings used by
/// [`crate::text::TextRenderer::measure_cell`] (`Family::Monospace` +
/// `Shaping::Advanced`) so single-character results stay consistent across
/// the two APIs.
///
/// # Arguments
/// * `font_system` - Shared cosmic-text font system. Borrowed mutably because
///   shaping mutates internal caches.
/// * `text` - String to measure. May be empty.
/// * `font_size` - Font size in points.
/// * `line_height` - Line height in pixels (typically `font_size * 1.2`).
/// * `max_width` - If `Some`, soft-wrap at this pixel width. If `None`, no wrap.
///
/// # Returns
/// A [`TextMeasurement`]. For an empty string, returns `width: 0.0`,
/// `height: 0.0`, `line_count: 0`.
pub fn measure_text(
    font_system: &mut FontSystem,
    text: &str,
    font_size: f32,
    line_height: f32,
    max_width: Option<f32>,
) -> TextMeasurement {
    if text.is_empty() {
        return TextMeasurement {
            width: 0.0,
            height: 0.0,
            line_count: 0,
        };
    }

    let metrics = Metrics::new(font_size, line_height);
    let mut buffer = Buffer::new(font_system, metrics);
    let attrs = Attrs::new().family(Family::Monospace);

    buffer.set_text(font_system, text, attrs, Shaping::Advanced);

    // Set buffer bounds. Width bounds the wrap point; height must be generous
    // enough to lay out every wrapped line (otherwise cosmic-text clips them).
    let height_bound = line_height.max(1.0) * (text.len() as f32 + 1.0);
    buffer.set_size(font_system, max_width, Some(height_bound));
    buffer.shape_until_scroll(font_system, true);

    let mut max_line_width: f32 = 0.0;
    let mut line_count: usize = 0;

    for run in buffer.layout_runs() {
        line_count += 1;
        let mut run_width: f32 = 0.0;
        for glyph in run.glyphs.iter() {
            let edge = glyph.x + glyph.w;
            if edge > run_width {
                run_width = edge;
            }
        }
        if run_width > max_line_width {
            max_line_width = run_width;
        }
    }

    // If shaping produced zero runs (e.g., text was only invisible control
    // chars), still report at least one line so callers can lay it out.
    if line_count == 0 {
        line_count = 1;
    }

    TextMeasurement {
        width: max_line_width,
        height: line_height * line_count as f32,
        line_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::TextRenderer;

    fn fs() -> FontSystem {
        FontSystem::new()
    }

    #[test]
    fn measure_text_empty_string_is_zero() {
        let mut font_system = fs();
        let m = measure_text(&mut font_system, "", 14.0, 16.8, None);
        assert_eq!(m.width, 0.0, "empty string width should be 0.0");
        assert_eq!(m.height, 0.0, "empty string height should be 0.0");
        assert_eq!(
            m.line_count, 0,
            "empty string returns line_count: 0 (documented contract)"
        );
    }

    #[test]
    fn measure_text_doubles_with_font_size() {
        let mut font_system = fs();
        let small = measure_text(&mut font_system, "hello world", 14.0, 16.8, None);
        let large = measure_text(&mut font_system, "hello world", 28.0, 33.6, None);

        let ratio = large.width / small.width;
        assert!(
            (ratio - 2.0).abs() < 0.05,
            "doubling font size should ~double width: small={} large={} ratio={:.3}",
            small.width,
            large.width,
            ratio
        );

        // 0.5px tolerance on the absolute scaling check
        assert!(
            (large.width - small.width * 2.0).abs() <= 0.5_f32.max(small.width * 0.05),
            "expected large.width ≈ 2 * small.width within 0.5px (or 5%): \
             small={} large={}",
            small.width,
            large.width
        );
    }

    #[test]
    fn measure_text_width_monotonic_with_length() {
        let mut font_system = fs();
        let short = measure_text(&mut font_system, "a", 14.0, 16.8, None);
        let mid = measure_text(&mut font_system, "ab", 14.0, 16.8, None);
        let long = measure_text(&mut font_system, "abcdefghij", 14.0, 16.8, None);

        assert!(
            mid.width >= short.width,
            "longer string must not be narrower: short={} mid={}",
            short.width,
            mid.width
        );
        assert!(
            long.width >= mid.width,
            "longer string must not be narrower: mid={} long={}",
            mid.width,
            long.width
        );
        assert!(
            long.width > short.width,
            "10-char string should be wider than 1-char: short={} long={}",
            short.width,
            long.width
        );
    }

    #[test]
    fn measure_text_max_width_forces_wrap() {
        let mut font_system = fs();
        let long_text = "the quick brown fox jumps over the lazy dog and then keeps going";
        let unwrapped = measure_text(&mut font_system, long_text, 14.0, 16.8, None);
        assert_eq!(
            unwrapped.line_count, 1,
            "no max_width should produce a single line"
        );

        let wrapped = measure_text(&mut font_system, long_text, 14.0, 16.8, Some(100.0));
        assert!(
            wrapped.line_count > 1,
            "max_width=100 on long text must wrap to multiple lines, got line_count={}",
            wrapped.line_count
        );
        assert!(
            wrapped.width <= 100.0 + 1.0,
            "wrapped width should not exceed max_width by more than 1px, got {}",
            wrapped.width
        );
        assert!(
            wrapped.height >= 16.8 * 2.0 - 0.01,
            "multi-line wrapped text should be at least 2 line_heights tall, got {}",
            wrapped.height
        );
    }

    #[test]
    fn measure_text_single_m_matches_renderer_cell_width() {
        // Cross-check the free function against TextRenderer::measure_cell.
        // Both must use Family::Monospace + Shaping::Advanced and produce the
        // same advance width for "M" within 1px tolerance.
        let mut renderer = TextRenderer::new(14.0);
        let (cell_w, _cell_h) = renderer.measure_cell();
        let line_height = renderer.line_height();

        let mut font_system = fs();
        let m = measure_text(&mut font_system, "M", 14.0, line_height, None);

        assert!(
            (m.width - cell_w).abs() < 1.0,
            "measure_text('M').width = {} should match TextRenderer cell_width = {} within 1px",
            m.width,
            cell_w
        );
    }
}
