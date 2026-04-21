// Instanced quad rendering for backgrounds, cursors, selections, UI chrome.
//
// A single draw call renders all quads via GPU instancing. Each QuadInstance
// carries position, size, color, and border radius. The vertex shader expands
// a unit quad per-instance; the fragment shader applies an SDF rounded-rect
// for smooth corners.

use wgpu::{
    util::{BufferInitDescriptor, DeviceExt},
    BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout, BindGroupLayoutDescriptor,
    BindGroupLayoutEntry, BindingType, BlendState, Buffer, BufferBindingType, BufferUsages,
    ColorTargetState, ColorWrites, Device, FragmentState, MultisampleState, PipelineLayoutDescriptor,
    PrimitiveState, PrimitiveTopology, Queue, RenderPass, RenderPipeline,
    RenderPipelineDescriptor, ShaderModuleDescriptor, ShaderStages, TextureFormat, VertexAttribute,
    VertexBufferLayout, VertexFormat, VertexState, VertexStepMode,
};

// ---------------------------------------------------------------------------
// WGSL shaders
// ---------------------------------------------------------------------------

const SHADER_SRC: &str = r#"
// ---- Uniforms ----
struct Uniforms {
    screen_size: vec2<f32>,
};

@group(0) @binding(0) var<uniform> uniforms: Uniforms;

// ---- Per-instance data ----
struct QuadInstance {
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
    @location(3) border_radius: f32,
};

struct VertexOutput {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) local_uv: vec2<f32>,   // 0..1 within the quad
    @location(2) size_px: vec2<f32>,     // quad size in pixels
    @location(3) border_radius: f32,
};

// Unit quad: two triangles covering (0,0) to (1,1).
// Vertex indices 0..5 map to the 6 corners of two triangles.
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
    instance: QuadInstance,
) -> VertexOutput {
    let uv = UNIT_QUAD[vertex_idx];

    // Pixel position of this vertex.
    let pixel_pos = instance.pos + uv * instance.size;

    // Convert pixel coordinates to NDC.
    // NDC x: -1 (left) to +1 (right)
    // NDC y: +1 (top) to -1 (bottom)  — y is flipped so (0,0) pixels maps to top-left.
    let ndc = vec2<f32>(
        (pixel_pos.x / uniforms.screen_size.x) * 2.0 - 1.0,
        1.0 - (pixel_pos.y / uniforms.screen_size.y) * 2.0,
    );

    var out: VertexOutput;
    out.clip_pos = vec4<f32>(ndc, 0.0, 1.0);
    out.color = instance.color;
    out.local_uv = uv;
    out.size_px = instance.size;
    out.border_radius = instance.border_radius;
    return out;
}

// Signed-distance function for a rounded rectangle centered at the origin.
// `half_size` is half the rectangle dimensions; `radius` is the corner radius.
fn sdf_rounded_rect(p: vec2<f32>, half_size: vec2<f32>, radius: f32) -> f32 {
    let r = min(radius, min(half_size.x, half_size.y));
    let q = abs(p) - half_size + vec2<f32>(r, r);
    return length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // Early out: no border radius means a plain rectangle — skip SDF math.
    if in.border_radius <= 0.0 {
        return in.color;
    }

    // Map local_uv (0..1) to a coordinate system centered on the quad.
    let local_pos = (in.local_uv - vec2<f32>(0.5, 0.5)) * in.size_px;
    let half_size = in.size_px * 0.5;

    let dist = sdf_rounded_rect(local_pos, half_size, in.border_radius);

    // Anti-alias: smoothstep over ~1 pixel at the boundary.
    let alpha = 1.0 - smoothstep(-0.5, 0.5, dist);

    return vec4<f32>(in.color.rgb, in.color.a * alpha);
}
"#;

// ---------------------------------------------------------------------------
// CPU-side instance data
// ---------------------------------------------------------------------------

/// A single quad instance uploaded to the GPU.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct QuadInstance {
    /// Top-left position in pixels.
    pub pos: [f32; 2],
    /// Width and height in pixels.
    pub size: [f32; 2],
    /// RGBA color (linear or sRGB depending on surface format).
    pub color: [f32; 4],
    /// Corner radius in pixels. 0 = sharp corners.
    pub border_radius: f32,
}

impl QuadInstance {
    /// Vertex buffer layout describing per-instance attributes.
    fn layout() -> VertexBufferLayout<'static> {
        const ATTRS: &[VertexAttribute] = &[
            // pos: [f32; 2]
            VertexAttribute {
                format: VertexFormat::Float32x2,
                offset: 0,
                shader_location: 0,
            },
            // size: [f32; 2]
            VertexAttribute {
                format: VertexFormat::Float32x2,
                offset: 8,
                shader_location: 1,
            },
            // color: [f32; 4]
            VertexAttribute {
                format: VertexFormat::Float32x4,
                offset: 16,
                shader_location: 2,
            },
            // border_radius: f32
            VertexAttribute {
                format: VertexFormat::Float32,
                offset: 32,
                shader_location: 3,
            },
        ];

        VertexBufferLayout {
            array_stride: std::mem::size_of::<QuadInstance>() as u64,
            step_mode: VertexStepMode::Instance,
            attributes: ATTRS,
        }
    }
}

// ---------------------------------------------------------------------------
// Uniform buffer
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    screen_size: [f32; 2],
    // Pad to 16-byte alignment (required by WebGPU uniform buffer rules).
    _pad: [f32; 2],
}

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

/// Instanced quad renderer. Draws all quads in a single draw call.
#[allow(dead_code)]
pub struct QuadRenderer {
    pipeline: RenderPipeline,
    instance_buf: Buffer,
    uniform_buf: Buffer,
    bind_group: BindGroup,
    bind_group_layout: BindGroupLayout,
    instance_count: u32,
    /// Capacity of the instance buffer in number of quads.
    instance_capacity: u32,
}

/// Initial instance buffer capacity (number of quads).
const INITIAL_CAPACITY: u32 = 1024;

impl QuadRenderer {
    /// Create the quad renderer: compiles shaders, creates pipeline and buffers.
    pub fn new(device: &Device, format: TextureFormat) -> Self {
        // -- Shader module --
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("quad-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SRC.into()),
        });

        // -- Bind group layout --
        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("quad-bind-group-layout"),
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

        // -- Pipeline layout --
        let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("quad-pipeline-layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        // -- Render pipeline --
        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("quad-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[QuadInstance::layout()],
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
            label: Some("quad-uniform-buf"),
            contents: bytemuck::bytes_of(&uniforms),
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
        });

        // -- Instance buffer (pre-allocated) --
        let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("quad-instance-buf"),
            size: (INITIAL_CAPACITY as u64) * (std::mem::size_of::<QuadInstance>() as u64),
            usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        // -- Bind group --
        let bind_group = device.create_bind_group(&BindGroupDescriptor {
            label: Some("quad-bind-group"),
            layout: &bind_group_layout,
            entries: &[BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });

        Self {
            pipeline,
            instance_buf,
            uniform_buf,
            bind_group,
            bind_group_layout,
            instance_count: 0,
            instance_capacity: INITIAL_CAPACITY,
        }
    }

    /// Upload quad instances and screen-size uniform for the current frame.
    ///
    /// Call this once per frame before `render`. If the instance buffer is too
    /// small it will be reallocated (and the bind group remains valid since the
    /// instance buffer is not bound via a bind group).
    pub fn prepare(
        &mut self,
        device: &Device,
        queue: &Queue,
        quads: &[QuadInstance],
        screen_size: [f32; 2],
    ) {
        // -- Update uniforms --
        let uniforms = Uniforms {
            screen_size,
            _pad: [0.0; 2],
        };
        queue.write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&uniforms));

        self.instance_count = quads.len() as u32;
        if self.instance_count == 0 {
            return;
        }

        let required_bytes =
            (quads.len() as u64) * (std::mem::size_of::<QuadInstance>() as u64);

        // Grow buffer if needed (double until large enough).
        if quads.len() as u32 > self.instance_capacity {
            let mut new_cap = self.instance_capacity;
            while new_cap < quads.len() as u32 {
                new_cap *= 2;
            }
            self.instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("quad-instance-buf"),
                size: (new_cap as u64) * (std::mem::size_of::<QuadInstance>() as u64),
                usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.instance_capacity = new_cap;
            log::debug!("quad instance buffer grown to {new_cap} quads");
        }

        queue.write_buffer(
            &self.instance_buf,
            0,
            &bytemuck::cast_slice(quads)[..required_bytes as usize],
        );
    }

    /// Record draw commands into an existing render pass.
    ///
    /// The render pass must have been created with a color attachment whose
    /// format matches the `TextureFormat` passed to `QuadRenderer::new`.
    pub fn render<'pass>(&'pass self, render_pass: &mut RenderPass<'pass>) {
        if self.instance_count == 0 {
            return;
        }
        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.instance_buf.slice(..));
        // 6 vertices per quad (two triangles), N instances.
        render_pass.draw(0..6, 0..self.instance_count);
    }
}
