//! Render context — substrate-level primitives passed to every widget.
//!
//! Carries live font/DPI metrics so widgets never bake hardcoded constants
//! like `CHAR_WIDTH = 8.0` or `LINE_HEIGHT = 18.0`. Resize the window or
//! change the font size, and `RenderCtx` recomputes; every widget consuming
//! it relayouts correctly without code changes.

/// Live, per-frame substrate metrics shared by every UI widget.
///
/// Constructed once per frame from the renderer's current state and threaded
/// through `Widget::render_quads` / `Widget::render_text` calls.
#[derive(Debug, Clone, Copy)]
pub struct RenderCtx {
    /// Monospace cell dimensions: (advance_width, line_height) in pixels.
    /// Source of truth for any text-positioning math in widgets.
    pub cell_size: (f32, f32),
    /// Logical-to-physical pixel ratio (1.0 on standard, 2.0 on Retina).
    pub dpi_scale: f32,
}

impl RenderCtx {
    /// Construct a `RenderCtx` from cell metrics and DPI scale.
    pub fn new(cell_size: (f32, f32), dpi_scale: f32) -> Self {
        Self { cell_size, dpi_scale }
    }

    /// Advance width of one monospace cell, in pixels.
    pub fn cell_w(&self) -> f32 {
        self.cell_size.0
    }

    /// Height of one text line (cell height), in pixels.
    pub fn cell_h(&self) -> f32 {
        self.cell_size.1
    }

    /// Approximate pixel width of a string at the current cell metrics.
    ///
    /// Assumes monospace shaping (one advance per char). For non-monospace
    /// content, prefer `phantom_renderer::text_metrics::measure`.
    pub fn measure_mono(&self, s: &str) -> f32 {
        s.chars().count() as f32 * self.cell_size.0
    }

    /// `4 * unit` spacing primitive — base for the spacing scale used by
    /// every widget that wants tokenized padding/margins.
    pub fn space(&self, n: f32) -> f32 {
        n * 4.0
    }

    /// A safe fallback for tests and code paths that don't yet thread metrics.
    /// Real values flow from the live renderer.
    pub fn fallback() -> Self {
        Self { cell_size: (8.0, 16.0), dpi_scale: 1.0 }
    }
}

impl Default for RenderCtx {
    fn default() -> Self {
        Self::fallback()
    }
}

// -------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_w_and_h_return_components() {
        let ctx = RenderCtx::new((9.5, 22.0), 2.0);
        assert_eq!(ctx.cell_w(), 9.5);
        assert_eq!(ctx.cell_h(), 22.0);
        assert_eq!(ctx.dpi_scale, 2.0);
    }

    #[test]
    fn measure_mono_scales_with_cell_width() {
        let small = RenderCtx::new((8.0, 16.0), 1.0);
        let big = RenderCtx::new((16.0, 32.0), 1.0);
        let s = "abcdef"; // 6 chars
        assert_eq!(small.measure_mono(s), 48.0);
        assert_eq!(big.measure_mono(s), 96.0);
    }

    #[test]
    fn measure_mono_handles_unicode_by_char_count() {
        let ctx = RenderCtx::new((10.0, 20.0), 1.0);
        // Box-drawing + a chevron — char_count counts code points, not bytes.
        assert_eq!(ctx.measure_mono("\u{276F} send"), 6.0 * 10.0);
    }

    #[test]
    fn measure_mono_empty_string_zero() {
        let ctx = RenderCtx::fallback();
        assert_eq!(ctx.measure_mono(""), 0.0);
    }

    #[test]
    fn space_n_is_4n() {
        let ctx = RenderCtx::fallback();
        assert_eq!(ctx.space(0.0), 0.0);
        assert_eq!(ctx.space(1.0), 4.0);
        assert_eq!(ctx.space(2.0), 8.0);
        assert_eq!(ctx.space(3.5), 14.0);
    }

    #[test]
    fn fallback_matches_legacy_assumptions() {
        // RenderCtx::fallback must equal the historical (8.0, 16.0) cell so
        // legacy code paths that don't yet thread metrics still produce the
        // same layout. If this test ever changes, audit every call site that
        // relies on the default.
        let ctx = RenderCtx::fallback();
        assert_eq!(ctx.cell_size, (8.0, 16.0));
        assert_eq!(ctx.dpi_scale, 1.0);
    }

    #[test]
    fn default_equals_fallback() {
        let a = RenderCtx::default();
        let b = RenderCtx::fallback();
        assert_eq!(a.cell_size, b.cell_size);
        assert_eq!(a.dpi_scale, b.dpi_scale);
    }
}

