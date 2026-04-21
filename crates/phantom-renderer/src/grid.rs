// Terminal grid rendering pipeline: the bridge between terminal cell state and GPU.
//
// Takes terminal cells and produces draw calls via two sub-pipelines:
//   1. QuadRenderer — instanced background quads for cell backgrounds
//   2. GridRenderer — textured glyph quads sampled from the GlyphAtlas
//
// The GridRenderer owns its own WGSL shader, render pipeline, instance buffer,
// and uniform buffer. It renders textured glyph quads from the atlas using
// instanced drawing with GlyphInstance data from the TextRenderer.

use crate::atlas::GlyphAtlas;
use crate::quads::QuadInstance;
use crate::text::{GlyphInstance, TerminalCell, TextRenderer};

use wgpu::{
    util::{BufferInitDescriptor, DeviceExt},
    BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout, BindGroupLayoutDescriptor,
    BindGroupLayoutEntry, BindingType, BlendState, Buffer, BufferBindingType, BufferUsages,
    ColorTargetState, ColorWrites, Device, FragmentState, MultisampleState,
    PipelineLayoutDescriptor, PrimitiveState, PrimitiveTopology, Queue, RenderPass,
    RenderPipeline, RenderPipelineDescriptor, ShaderModuleDescriptor, ShaderStages, TextureFormat,
    VertexState,
};

// ---------------------------------------------------------------------------
// WGSL shader for textured glyph quad rendering
// ---------------------------------------------------------------------------

const GLYPH_SHADER_SRC: &str = r#"
// ---- Uniforms (group 0) ----
struct Uniforms {
    screen_size: vec2<f32>,
};

@group(0) @binding(0) var<uniform> uniforms: Uniforms;

// ---- Atlas texture + sampler (group 1) ----
@group(1) @binding(0) var atlas_texture: texture_2d<f32>;
@group(1) @binding(1) var atlas_sampler: sampler;

// ---- Per-instance glyph data ----
struct GlyphInstance {
    @location(0) position: vec2<f32>,
    @location(1) uv_rect: vec4<f32>,    // [min_u, min_v, max_u, max_v]
    @location(2) color: vec4<f32>,
    @location(3) size: vec2<f32>,
};

struct VertexOutput {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
};

// Unit quad: two triangles covering (0,0) to (1,1).
var<private> UNIT_QUAD: array<vec2<f32>, 6> = array<vec2<f32>, 6>(
    vec2<f32>(0.0, 0.0),
    vec2<f32>(1.0, 0.0),
    vec2<f32>(0.0, 1.0),
    vec2<f32>(1.0, 0.0),
    vec2<f32>(1.0, 1.0),
    vec2<f32>(0.0, 1.0),
);

@vertex
fn vs_main(
    @builtin(vertex_index) vertex_idx: u32,
    instance: GlyphInstance,
) -> VertexOutput {
    let corner = UNIT_QUAD[vertex_idx];

    // Expand unit quad to glyph pixel dimensions at the instance position.
    let pixel_pos = instance.position + corner * instance.size;

    // Convert pixel coordinates to NDC.
    // NDC x: -1 (left) to +1 (right)
    // NDC y: +1 (top)  to -1 (bottom) — y flipped so (0,0) is top-left.
    let ndc = vec2<f32>(
        (pixel_pos.x / uniforms.screen_size.x) * 2.0 - 1.0,
        1.0 - (pixel_pos.y / uniforms.screen_size.y) * 2.0,
    );

    // Interpolate UV from the atlas sub-rectangle.
    let uv = mix(instance.uv_rect.xy, instance.uv_rect.zw, corner);

    var out: VertexOutput;
    out.clip_pos = vec4<f32>(ndc, 0.0, 1.0);
    out.uv = uv;
    out.color = instance.color;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // The atlas is R8Unorm — the red channel holds the alpha mask.
    let atlas_alpha = textureSample(atlas_texture, atlas_sampler, in.uv).r;

    // Multiply instance color by the atlas alpha for anti-aliased text.
    return vec4<f32>(in.color.rgb, in.color.a * atlas_alpha);
}
"#;

// ---------------------------------------------------------------------------
// Uniform buffer
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    screen_size: [f32; 2],
    // Pad to 16-byte alignment (WebGPU uniform buffer requirement).
    _pad: [f32; 2],
}

// ---------------------------------------------------------------------------
// GridRenderer — the glyph quad rendering pipeline
// ---------------------------------------------------------------------------

/// Initial glyph instance buffer capacity (number of glyphs).
const INITIAL_GLYPH_CAPACITY: u32 = 4096;

/// Renders textured glyph quads from the atlas via GPU instancing.
///
/// Each visible glyph in the terminal grid is drawn as a textured quad,
/// sampling from the R8Unorm glyph atlas. The pipeline uses:
///   - Bind group 0: screen-size uniform
///   - Bind group 1: atlas texture + sampler (from GlyphAtlas)
///   - Instance buffer: GlyphInstance per glyph
#[allow(dead_code)]
pub struct GridRenderer {
    pipeline: RenderPipeline,
    instance_buf: Buffer,
    uniform_buf: Buffer,
    uniform_bind_group: BindGroup,
    uniform_bind_group_layout: BindGroupLayout,
    instance_count: u32,
    instance_capacity: u32,
}

impl GridRenderer {
    /// Create the glyph rendering pipeline.
    ///
    /// # Arguments
    /// * `device` — wgpu device for resource creation.
    /// * `format` — surface texture format for the color target.
    /// * `atlas_bind_group_layout` — bind group layout from `GlyphAtlas::bind_group_layout()`,
    ///   bound at group 1 (texture + sampler).
    pub fn new(
        device: &Device,
        format: TextureFormat,
        atlas_bind_group_layout: &BindGroupLayout,
    ) -> Self {
        // -- Shader module --
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("grid-glyph-shader"),
            source: wgpu::ShaderSource::Wgsl(GLYPH_SHADER_SRC.into()),
        });

        // -- Uniform bind group layout (group 0) --
        let uniform_bind_group_layout =
            device.create_bind_group_layout(&BindGroupLayoutDescriptor {
                label: Some("grid-uniform-layout"),
                entries: &[BindGroupLayoutEntry {
                    binding: 0,
                    visibility: ShaderStages::VERTEX,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        // -- Pipeline layout: group 0 = uniforms, group 1 = atlas --
        let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("grid-pipeline-layout"),
            bind_group_layouts: &[&uniform_bind_group_layout, atlas_bind_group_layout],
            push_constant_ranges: &[],
        });

        // -- Render pipeline --
        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("grid-glyph-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[GlyphInstance::buffer_layout()],
            },
            fragment: Some(FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(ColorTargetState {
                    format,
                    blend: Some(BlendState::ALPHA_BLENDING),
                    write_mask: ColorWrites::ALL,
                })],
            }),
            primitive: PrimitiveState {
                topology: PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        // -- Uniform buffer --
        let uniforms = Uniforms {
            screen_size: [1.0, 1.0],
            _pad: [0.0; 2],
        };
        let uniform_buf = device.create_buffer_init(&BufferInitDescriptor {
            label: Some("grid-uniform-buf"),
            contents: bytemuck::bytes_of(&uniforms),
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
        });

        // -- Uniform bind group --
        let uniform_bind_group = device.create_bind_group(&BindGroupDescriptor {
            label: Some("grid-uniform-bind-group"),
            layout: &uniform_bind_group_layout,
            entries: &[BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });

        // -- Instance buffer (pre-allocated) --
        let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("grid-glyph-instance-buf"),
            size: (INITIAL_GLYPH_CAPACITY as u64) * (std::mem::size_of::<GlyphInstance>() as u64),
            usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            instance_buf,
            uniform_buf,
            uniform_bind_group,
            uniform_bind_group_layout,
            instance_count: 0,
            instance_capacity: INITIAL_GLYPH_CAPACITY,
        }
    }

    /// Upload glyph instances and screen-size uniform for the current frame.
    ///
    /// Call once per frame before `render`. If the instance buffer is too small
    /// it will be reallocated with doubled capacity.
    pub fn prepare(
        &mut self,
        device: &Device,
        queue: &Queue,
        glyphs: &[GlyphInstance],
        screen_size: [f32; 2],
    ) {
        // -- Update uniforms --
        let uniforms = Uniforms {
            screen_size,
            _pad: [0.0; 2],
        };
        queue.write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&uniforms));

        self.instance_count = glyphs.len() as u32;
        if self.instance_count == 0 {
            return;
        }

        let required_bytes =
            (glyphs.len() as u64) * (std::mem::size_of::<GlyphInstance>() as u64);

        // Grow buffer if needed (double until large enough).
        if glyphs.len() as u32 > self.instance_capacity {
            let mut new_cap = self.instance_capacity;
            while new_cap < glyphs.len() as u32 {
                new_cap *= 2;
            }
            self.instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("grid-glyph-instance-buf"),
                size: (new_cap as u64) * (std::mem::size_of::<GlyphInstance>() as u64),
                usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.instance_capacity = new_cap;
            log::debug!("grid glyph instance buffer grown to {new_cap} instances");
        }

        queue.write_buffer(
            &self.instance_buf,
            0,
            &bytemuck::cast_slice(glyphs)[..required_bytes as usize],
        );
    }

    /// Record glyph draw commands into an existing render pass.
    ///
    /// # Arguments
    /// * `render_pass` — the active render pass (color attachment format must match).
    /// * `atlas_bind_group` — the atlas bind group from `GlyphAtlas::bind_group()`,
    ///   bound at group 1.
    pub fn render<'pass>(
        &'pass self,
        render_pass: &mut RenderPass<'pass>,
        atlas_bind_group: &'pass BindGroup,
    ) {
        if self.instance_count == 0 {
            return;
        }
        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.uniform_bind_group, &[]);
        render_pass.set_bind_group(1, atlas_bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.instance_buf.slice(..));
        // 6 vertices per quad (two triangles), one instance per glyph.
        render_pass.draw(0..6, 0..self.instance_count);
    }

    /// Number of glyphs that will be drawn on the next `render` call.
    pub fn glyph_count(&self) -> u32 {
        self.instance_count
    }

    /// Current instance buffer capacity in number of glyphs.
    pub fn capacity(&self) -> u32 {
        self.instance_capacity
    }
}

// ---------------------------------------------------------------------------
// Terminal cell with background color
// ---------------------------------------------------------------------------

/// Extended terminal cell carrying both foreground and background colors.
///
/// Used by `GridRenderData::prepare` to generate both background quads and
/// glyph instances in a single pass over the terminal grid.
#[derive(Clone, Copy, Debug)]
pub struct GridCell {
    /// The character to render.
    pub ch: char,
    /// Foreground color (RGBA, linear).
    pub fg: [f32; 4],
    /// Background color (RGBA, linear). Fully transparent = no background quad.
    pub bg: [f32; 4],
}

impl Default for GridCell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: [1.0, 1.0, 1.0, 1.0],
            bg: [0.0, 0.0, 0.0, 0.0], // transparent — no quad emitted
        }
    }
}

impl GridCell {
    /// Convert to a `TerminalCell` for the text renderer.
    fn as_terminal_cell(&self) -> TerminalCell {
        TerminalCell {
            ch: self.ch,
            fg: self.fg,
        }
    }
}

// ---------------------------------------------------------------------------
// Default background color constant
// ---------------------------------------------------------------------------

/// Returns true if a background color should be considered "default" (no quad needed).
///
/// A background is default if it is fully transparent (alpha == 0) or matches
/// the terminal's default background.
fn is_default_bg(bg: &[f32; 4]) -> bool {
    bg[3] <= 0.0
}

// ---------------------------------------------------------------------------
// GridRenderData — high-level terminal grid to draw data conversion
// ---------------------------------------------------------------------------

/// Convenience type that converts full terminal grid state into GPU-ready
/// draw data for both the QuadRenderer (backgrounds) and GridRenderer (glyphs).
///
/// Decouples the terminal model from rendering specifics. Typical per-frame
/// usage:
///
/// ```ignore
/// let (bg_quads, glyph_instances) = GridRenderData::prepare(
///     &grid_cells, cols, rows,
///     &mut text_renderer, &mut atlas, queue,
///     origin, cell_size,
/// );
/// quad_renderer.prepare(device, queue, &bg_quads, screen_size);
/// grid_renderer.prepare(device, queue, &glyph_instances, screen_size);
/// ```
pub struct GridRenderData;

impl GridRenderData {
    /// Convert terminal grid state to background quads and glyph instances.
    ///
    /// Iterates the cell grid once, emitting:
    ///   - A `QuadInstance` for every cell whose background differs from the default.
    ///   - Delegates glyph rasterization/placement to `TextRenderer::prepare_glyphs`.
    ///
    /// # Arguments
    /// * `cells` — row-major grid of `GridCell` (`rows * cols` elements).
    /// * `cols` — number of columns per row.
    /// * `rows` — number of rows.
    /// * `text_renderer` — the text shaping/rasterization engine.
    /// * `atlas` — the glyph atlas for caching rasterized glyphs on the GPU.
    /// * `queue` — wgpu queue for atlas texture uploads.
    /// * `origin` — top-left pixel coordinate of the grid: (x, y).
    /// * `cell_size` — monospace cell dimensions: (width, height) in pixels.
    ///
    /// # Returns
    /// `(background_quads, glyph_instances)` ready for `QuadRenderer` and `GridRenderer`.
    pub fn prepare(
        cells: &[GridCell],
        cols: usize,
        rows: usize,
        text_renderer: &mut TextRenderer,
        atlas: &mut GlyphAtlas,
        queue: &Queue,
        origin: (f32, f32),
        cell_size: (f32, f32),
    ) -> (Vec<QuadInstance>, Vec<GlyphInstance>) {
        let total = cols * rows;
        debug_assert!(
            cells.len() >= total,
            "grid cells ({}) < cols*rows ({}*{}={})",
            cells.len(),
            cols,
            rows,
            total,
        );

        // --- Background quads ---
        let mut bg_quads = Vec::new();

        for row in 0..rows {
            for col in 0..cols {
                let cell = &cells[row * cols + col];

                if is_default_bg(&cell.bg) {
                    continue;
                }

                let x = origin.0 + (col as f32) * cell_size.0;
                let y = origin.1 + (row as f32) * cell_size.1;

                bg_quads.push(QuadInstance {
                    pos: [x, y],
                    size: [cell_size.0, cell_size.1],
                    color: cell.bg,
                    border_radius: 0.0,
                });
            }
        }

        // --- Glyph instances ---
        // Convert GridCells to TerminalCells for the text renderer.
        let terminal_cells: Vec<TerminalCell> =
            cells[..total].iter().map(|c| c.as_terminal_cell()).collect();

        let glyph_instances =
            text_renderer.prepare_glyphs(atlas, queue, &terminal_cells, cols, origin);

        (bg_quads, glyph_instances)
    }

    /// Optimized variant that merges adjacent cells with the same background
    /// color into wider quads, reducing draw overhead for colored rows.
    ///
    /// Otherwise identical to `prepare`.
    pub fn prepare_merged(
        cells: &[GridCell],
        cols: usize,
        rows: usize,
        text_renderer: &mut TextRenderer,
        atlas: &mut GlyphAtlas,
        queue: &Queue,
        origin: (f32, f32),
        cell_size: (f32, f32),
    ) -> (Vec<QuadInstance>, Vec<GlyphInstance>) {
        let total = cols * rows;
        debug_assert!(
            cells.len() >= total,
            "grid cells ({}) < cols*rows ({}*{}={})",
            cells.len(),
            cols,
            rows,
            total,
        );

        // --- Merged background quads ---
        let mut bg_quads = Vec::new();

        for row in 0..rows {
            let mut col = 0;
            while col < cols {
                let cell = &cells[row * cols + col];

                if is_default_bg(&cell.bg) {
                    col += 1;
                    continue;
                }

                // Scan for a run of cells with the same background color.
                let run_start = col;
                let run_bg = cell.bg;
                col += 1;

                while col < cols {
                    let next = &cells[row * cols + col];
                    if next.bg != run_bg {
                        break;
                    }
                    col += 1;
                }

                let run_len = col - run_start;
                let x = origin.0 + (run_start as f32) * cell_size.0;
                let y = origin.1 + (row as f32) * cell_size.1;

                bg_quads.push(QuadInstance {
                    pos: [x, y],
                    size: [cell_size.0 * (run_len as f32), cell_size.1],
                    color: run_bg,
                    border_radius: 0.0,
                });
            }
        }

        // --- Glyph instances ---
        let terminal_cells: Vec<TerminalCell> =
            cells[..total].iter().map(|c| c.as_terminal_cell()).collect();

        let glyph_instances =
            text_renderer.prepare_glyphs(atlas, queue, &terminal_cells, cols, origin);

        (bg_quads, glyph_instances)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_grid_cell_has_transparent_bg() {
        let cell = GridCell::default();
        assert_eq!(cell.ch, ' ');
        assert_eq!(cell.bg[3], 0.0);
        assert!(is_default_bg(&cell.bg));
    }

    #[test]
    fn non_default_bg_detected() {
        let bg = [0.1, 0.1, 0.1, 1.0];
        assert!(!is_default_bg(&bg));
    }

    #[test]
    fn grid_cell_to_terminal_cell() {
        let gc = GridCell {
            ch: 'A',
            fg: [1.0, 0.0, 0.0, 1.0],
            bg: [0.0, 0.0, 0.0, 1.0],
        };
        let tc = gc.as_terminal_cell();
        assert_eq!(tc.ch, 'A');
        assert_eq!(tc.fg, [1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn prepare_skips_transparent_bg() {
        // We can test the background quad logic without GPU by checking
        // is_default_bg on various inputs.
        let transparent = [0.0, 0.0, 0.0, 0.0];
        let opaque = [0.2, 0.2, 0.2, 1.0];
        assert!(is_default_bg(&transparent));
        assert!(!is_default_bg(&opaque));
    }

    #[test]
    fn uniforms_are_16_byte_aligned() {
        assert_eq!(std::mem::size_of::<Uniforms>(), 16);
    }
}
