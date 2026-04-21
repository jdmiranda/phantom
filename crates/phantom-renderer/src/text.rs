// cosmic-text integration: shaping, rasterization, atlas upload

use crate::atlas::{GlyphAtlas, GlyphEntry};
use cosmic_text::{
    Attrs, Buffer, CacheKey, Family, FontSystem, LayoutGlyph, Metrics, Shaping, SwashCache,
};

/// A single glyph instance for GPU instanced rendering.
///
/// Each visible glyph in the terminal grid becomes one of these,
/// uploaded to a vertex buffer and drawn as an instanced quad.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GlyphInstance {
    /// Top-left position in pixels.
    pub position: [f32; 2],
    /// UV rectangle in the atlas: [min_u, min_v, max_u, max_v].
    pub uv_rect: [f32; 4],
    /// RGBA color, linear.
    pub color: [f32; 4],
    /// Size of the glyph quad in pixels: [width, height].
    pub size: [f32; 2],
}

impl GlyphInstance {
    /// Vertex buffer layout for instanced rendering.
    pub const ATTRIBS: [wgpu::VertexAttribute; 4] = wgpu::vertex_attr_array![
        // position
        0 => Float32x2,
        // uv_rect
        1 => Float32x4,
        // color
        2 => Float32x4,
        // size
        3 => Float32x2,
    ];

    pub fn buffer_layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<GlyphInstance>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &Self::ATTRIBS,
        }
    }
}

/// A single terminal cell with character and color data.
#[derive(Clone, Copy, Debug)]
pub struct TerminalCell {
    pub ch: char,
    pub fg: [f32; 4],
}

impl Default for TerminalCell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: [1.0, 1.0, 1.0, 1.0],
        }
    }
}

/// Handles font loading, text shaping, glyph rasterization, and atlas upload.
///
/// Owns a `FontSystem` for system font discovery and shaping, and a `SwashCache`
/// for rasterization. Works with an external `GlyphAtlas` to cache glyph bitmaps
/// on the GPU.
pub struct TextRenderer {
    font_system: FontSystem,
    swash_cache: SwashCache,
    font_size: f32,
    line_height: f32,
    /// Cached monospace cell dimensions: (width, height).
    cell_size: Option<(f32, f32)>,
}

impl TextRenderer {
    /// Create a new text renderer with the given font size.
    ///
    /// Initializes the system font database and configures for monospace rendering.
    /// Line height is set to 1.2x the font size by default, suitable for terminal use.
    pub fn new(font_size: f32) -> Self {
        let font_system = FontSystem::new();
        let swash_cache = SwashCache::new();
        let line_height = (font_size * 1.2).ceil();

        Self {
            font_system,
            swash_cache,
            font_size,
            line_height,
            cell_size: None,
        }
    }

    /// Access the underlying font system.
    pub fn font_system(&self) -> &FontSystem {
        &self.font_system
    }

    /// Mutably access the underlying font system.
    pub fn font_system_mut(&mut self) -> &mut FontSystem {
        &mut self.font_system
    }

    /// Access the swash cache.
    pub fn swash_cache_mut(&mut self) -> &mut SwashCache {
        &mut self.swash_cache
    }

    /// Get the configured font size.
    pub fn font_size(&self) -> f32 {
        self.font_size
    }

    /// Get the configured line height.
    pub fn line_height(&self) -> f32 {
        self.line_height
    }

    /// Set the font size and invalidate cached cell metrics.
    pub fn set_font_size(&mut self, font_size: f32) {
        self.font_size = font_size;
        self.line_height = (font_size * 1.2).ceil();
        self.cell_size = None;
    }

    /// Measure the monospace cell size: (width, height).
    ///
    /// Uses the advance width of 'M' (em-width) for cell width and the configured
    /// line height for cell height. Results are cached until font size changes.
    pub fn measure_cell(&mut self) -> (f32, f32) {
        if let Some(cached) = self.cell_size {
            return cached;
        }

        let metrics = Metrics::new(self.font_size, self.line_height);
        let mut buffer = Buffer::new(&mut self.font_system, metrics);

        let attrs = Attrs::new().family(Family::Monospace);

        buffer.set_text(&mut self.font_system, "M", attrs, Shaping::Advanced);
        buffer.set_size(
            &mut self.font_system,
            Some(self.font_size * 4.0),
            Some(self.line_height * 2.0),
        );
        buffer.shape_until_scroll(&mut self.font_system, true);

        let mut cell_width = self.font_size * 0.6; // fallback

        for run in buffer.layout_runs() {
            for glyph in run.glyphs.iter() {
                cell_width = glyph.w;
            }
        }

        let cell_height = self.line_height;
        let size = (cell_width, cell_height);
        self.cell_size = Some(size);

        log::info!(
            "Cell metrics: {:.1} x {:.1} at {:.1}pt",
            size.0,
            size.1,
            self.font_size
        );

        size
    }

    /// Rasterize a single glyph and upload it to the atlas.
    ///
    /// Returns the atlas entry with UV coordinates and placement offsets,
    /// or `None` if the glyph could not be rasterized (whitespace, missing, atlas full).
    pub fn rasterize_glyph(
        &mut self,
        atlas: &mut GlyphAtlas,
        queue: &wgpu::Queue,
        cache_key: CacheKey,
    ) -> Option<GlyphEntry> {
        atlas.get_or_insert(
            queue,
            &mut self.font_system,
            &mut self.swash_cache,
            cache_key,
        )
    }

    /// Prepare glyph instances for a grid of terminal cells.
    ///
    /// Given a 2D grid of cells (row-major, `rows * cols` elements), produces a
    /// `Vec<GlyphInstance>` suitable for GPU instanced rendering. Each non-whitespace
    /// glyph is rasterized (if not already cached) and positioned on the pixel grid.
    ///
    /// # Arguments
    /// * `atlas` - The glyph atlas for caching rasterized glyphs.
    /// * `queue` - The wgpu queue for atlas texture uploads.
    /// * `cells` - Row-major grid of terminal cells (`rows * cols` elements).
    /// * `cols` - Number of columns per row.
    /// * `origin` - Top-left pixel offset for the grid: (x, y).
    pub fn prepare_glyphs(
        &mut self,
        atlas: &mut GlyphAtlas,
        queue: &wgpu::Queue,
        cells: &[TerminalCell],
        cols: usize,
        origin: (f32, f32),
    ) -> Vec<GlyphInstance> {
        if cols == 0 {
            return Vec::new();
        }

        let (cell_w, cell_h) = self.measure_cell();
        let metrics = Metrics::new(self.font_size, self.line_height);
        let attrs = Attrs::new().family(Family::Monospace);
        let rows = cells.len() / cols;

        let mut instances = Vec::with_capacity(cells.len());

        for row in 0..rows {
            for col in 0..cols {
                let cell = &cells[row * cols + col];

                // Skip spaces and control characters -- nothing to rasterize.
                if cell.ch <= ' ' {
                    continue;
                }

                // Shape this character through cosmic-text to get proper font
                // selection, fallback, and glyph IDs.
                let mut buffer = Buffer::new(&mut self.font_system, metrics);
                let text: String = cell.ch.to_string();
                buffer.set_text(&mut self.font_system, &text, attrs, Shaping::Advanced);
                buffer.set_size(
                    &mut self.font_system,
                    Some(cell_w * 2.0),
                    Some(cell_h * 2.0),
                );
                buffer.shape_until_scroll(&mut self.font_system, true);

                for run in buffer.layout_runs() {
                    for glyph in run.glyphs.iter() {
                        let physical = glyph.physical((0.0, 0.0), 1.0);

                        if let Some(entry) = self.rasterize_glyph(atlas, queue, physical.cache_key)
                        {
                            // Cell origin on the pixel grid.
                            let cell_x = origin.0 + (col as f32) * cell_w;
                            let baseline_y = origin.1 + (row as f32) * cell_h + run.line_y;

                            // Glyph position: integer position from physical +
                            // bearing from the rasterized image placement.
                            let px = cell_x + physical.x as f32 + entry.left as f32;
                            let py = baseline_y + physical.y as f32 - entry.top as f32;

                            instances.push(GlyphInstance {
                                position: [px, py],
                                uv_rect: [
                                    entry.uv_min[0],
                                    entry.uv_min[1],
                                    entry.uv_max[0],
                                    entry.uv_max[1],
                                ],
                                color: cell.fg,
                                size: [entry.width as f32, entry.height as f32],
                            });
                        }
                    }
                }
            }
        }

        instances
    }

    /// Prepare glyph instances from a pre-shaped cosmic-text `Buffer`.
    ///
    /// Lower-level method for cases where the caller already has a shaped buffer
    /// (e.g., UI overlays, debug text, non-grid text).
    pub fn prepare_glyphs_from_buffer(
        &mut self,
        atlas: &mut GlyphAtlas,
        queue: &wgpu::Queue,
        buffer: &Buffer,
        default_color: [f32; 4],
        origin: (f32, f32),
    ) -> Vec<GlyphInstance> {
        let mut instances = Vec::new();

        for run in buffer.layout_runs() {
            for glyph in run.glyphs.iter() {
                let physical = glyph.physical((origin.0, origin.1), 1.0);

                if let Some(entry) = self.rasterize_glyph(atlas, queue, physical.cache_key) {
                    let color = glyph_color(glyph, default_color);

                    let px = physical.x as f32 + entry.left as f32;
                    let py = run.line_y + physical.y as f32 - entry.top as f32;

                    instances.push(GlyphInstance {
                        position: [px, py],
                        uv_rect: [
                            entry.uv_min[0],
                            entry.uv_min[1],
                            entry.uv_max[0],
                            entry.uv_max[1],
                        ],
                        color,
                        size: [entry.width as f32, entry.height as f32],
                    });
                }
            }
        }

        instances
    }
}

/// Extract glyph color: use the glyph's color override if present, otherwise the default.
fn glyph_color(glyph: &LayoutGlyph, default: [f32; 4]) -> [f32; 4] {
    match glyph.color_opt {
        Some(c) => {
            let (r, g, b, a) = c.as_rgba_tuple();
            [
                r as f32 / 255.0,
                g as f32 / 255.0,
                b as f32 / 255.0,
                a as f32 / 255.0,
            ]
        }
        None => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyph_instance_is_pod() {
        let instance = GlyphInstance {
            position: [0.0, 0.0],
            uv_rect: [0.0, 0.0, 1.0, 1.0],
            color: [1.0, 1.0, 1.0, 1.0],
            size: [10.0, 20.0],
        };
        let bytes = bytemuck::bytes_of(&instance);
        assert_eq!(bytes.len(), std::mem::size_of::<GlyphInstance>());
        // 12 floats * 4 bytes = 48
        assert_eq!(std::mem::size_of::<GlyphInstance>(), 48);
    }

    #[test]
    fn measure_cell_returns_positive_dimensions() {
        let mut renderer = TextRenderer::new(14.0);
        let (w, h) = renderer.measure_cell();
        assert!(w > 0.0, "cell width should be positive, got {w}");
        assert!(h > 0.0, "cell height should be positive, got {h}");
        assert!(
            w < renderer.font_size(),
            "cell width {w} unexpectedly large"
        );
    }

    #[test]
    fn measure_cell_is_cached() {
        let mut renderer = TextRenderer::new(14.0);
        let first = renderer.measure_cell();
        let second = renderer.measure_cell();
        assert_eq!(first.0, second.0);
        assert_eq!(first.1, second.1);
    }

    #[test]
    fn set_font_size_invalidates_cache() {
        let mut renderer = TextRenderer::new(14.0);
        let _ = renderer.measure_cell();
        renderer.set_font_size(20.0);
        assert!(renderer.cell_size.is_none());
    }

    #[test]
    fn default_terminal_cell() {
        let cell = TerminalCell::default();
        assert_eq!(cell.ch, ' ');
        assert_eq!(cell.fg, [1.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn buffer_layout_stride() {
        let layout = GlyphInstance::buffer_layout();
        assert_eq!(layout.array_stride, 48);
        assert_eq!(layout.step_mode, wgpu::VertexStepMode::Instance);
        assert_eq!(layout.attributes.len(), 4);
    }
}
