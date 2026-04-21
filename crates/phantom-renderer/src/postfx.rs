// Post-processing pipeline: CRT barrel distortion, scanlines, chromatic
// aberration, phosphor bloom, vignette, and animated film grain.
//
// The terminal scene is rendered to an offscreen texture. This pipeline then
// draws a full-screen triangle with CRT effects applied in the fragment shader,
// outputting to the final surface texture.

use wgpu::{
    util::{BufferInitDescriptor, DeviceExt},
    BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout, BindGroupLayoutDescriptor,
    BindGroupLayoutEntry, BindingResource, BindingType, Buffer, BufferBindingType, BufferUsages,
    ColorTargetState, ColorWrites, Device, FilterMode, FragmentState, MultisampleState,
    PipelineLayoutDescriptor, PrimitiveState, PrimitiveTopology, Queue, RenderPipeline,
    RenderPipelineDescriptor, SamplerBindingType, SamplerDescriptor, ShaderModuleDescriptor,
    ShaderStages, Texture, TextureDescriptor, TextureDimension, TextureFormat,
    TextureSampleType, TextureUsages, TextureView, TextureViewDimension, VertexState,
};

// ---------------------------------------------------------------------------
// WGSL shader — the CRT post-processing heart of Phantom
// ---------------------------------------------------------------------------

const POSTFX_SHADER: &str = r#"
// ---- Uniforms ----
// Layout uses vec4 for glow_color (with time packed in .w) to avoid vec3
// alignment headaches between Rust repr(C) and WGSL std140 rules.
struct PostFxParams {
    scanline_intensity: f32,     //  0
    bloom_intensity: f32,        //  4
    chromatic_aberration: f32,   //  8
    curvature: f32,              // 12
    vignette_intensity: f32,     // 16
    noise_intensity: f32,        // 20
    time: f32,                   // 24
    _pad0: f32,                  // 28
    glow_color: vec3<f32>,       // 32 (vec3 aligns to 16)
    _pad1: f32,                  // 44
    resolution: vec2<f32>,       // 48
    _pad2: vec2<f32>,            // 56
                                 // total: 64
};

@group(0) @binding(0) var scene_texture: texture_2d<f32>;
@group(0) @binding(1) var scene_sampler: sampler;
@group(0) @binding(2) var<uniform> params: PostFxParams;

// ---- Vertex shader: full-screen triangle from vertex index ----
struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VertexOutput {
    // Full-screen triangle trick: 3 vertices that cover the entire clip space.
    // Vertex 0: (-1, -1)  UV (0, 1)
    // Vertex 1: ( 3, -1)  UV (2, 1)
    // Vertex 2: (-1,  3)  UV (0, -1)
    // The GPU clips the oversized triangle to the viewport.
    var out: VertexOutput;
    let x = f32(i32(idx & 1u) * 4 - 1);
    let y = f32(i32(idx >> 1u) * 4 - 1);
    out.position = vec4<f32>(x, y, 0.0, 1.0);
    // UV: map clip space to 0..1, with y flipped so (0,0) is top-left.
    out.uv = vec2<f32>((x + 1.0) * 0.5, 1.0 - (y + 1.0) * 0.5);
    return out;
}

// ---- Utility functions ----

// Hash-based pseudo-random noise. Returns 0..1.
fn hash(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.xyx) * 0.1031);
    p3 = p3 + dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

// Barrel distortion: warp UVs outward from center to simulate CRT curvature.
fn barrel_distort(uv: vec2<f32>, amount: f32) -> vec2<f32> {
    let centered = uv - vec2<f32>(0.5, 0.5);
    let r2 = dot(centered, centered);
    let strength = amount * 2.0; // scale to a visually pleasing range
    let warped = centered * (1.0 + strength * r2 + strength * 0.5 * r2 * r2);
    return warped + vec2<f32>(0.5, 0.5);
}

// Check if UV is within the 0..1 range (for masking outside the CRT "glass").
fn in_bounds(uv: vec2<f32>) -> bool {
    return uv.x >= 0.0 && uv.x <= 1.0 && uv.y >= 0.0 && uv.y <= 1.0;
}

// Sample with bounds check — return black if outside.
fn sample_clamped(uv: vec2<f32>) -> vec4<f32> {
    if !in_bounds(uv) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    return textureSample(scene_texture, scene_sampler, uv);
}

// ---- Fragment shader: all CRT effects composited ----
@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    var uv = in.uv;

    // ------------------------------------------------------------------
    // 1. Barrel distortion (CRT screen curvature)
    // ------------------------------------------------------------------
    if params.curvature > 0.0 {
        uv = barrel_distort(uv, params.curvature);
    }

    // Outside the curved screen area — pure black.
    if !in_bounds(uv) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }

    // ------------------------------------------------------------------
    // 2. Chromatic aberration
    // ------------------------------------------------------------------
    var color: vec3<f32>;

    if params.chromatic_aberration > 0.0 {
        let center = vec2<f32>(0.5, 0.5);
        let dist_from_center = uv - center;
        let offset = dist_from_center * params.chromatic_aberration * 0.02;

        let r = sample_clamped(uv + offset).r;
        let g = sample_clamped(uv).g;
        let b = sample_clamped(uv - offset).b;
        color = vec3<f32>(r, g, b);
    } else {
        color = textureSample(scene_texture, scene_sampler, uv).rgb;
    }

    // ------------------------------------------------------------------
    // 3. Phosphor bloom / glow
    // ------------------------------------------------------------------
    if params.bloom_intensity > 0.0 {
        // Simple 9-tap box blur for bloom. Sample a cross + diagonals
        // at a distance proportional to bloom intensity.
        let texel = vec2<f32>(1.0 / params.resolution.x, 1.0 / params.resolution.y);
        let spread = texel * (2.0 + params.bloom_intensity * 4.0);

        var bloom = vec3<f32>(0.0, 0.0, 0.0);
        bloom += sample_clamped(uv + vec2<f32>(-spread.x, -spread.y)).rgb;
        bloom += sample_clamped(uv + vec2<f32>( 0.0,      -spread.y)).rgb;
        bloom += sample_clamped(uv + vec2<f32>( spread.x, -spread.y)).rgb;
        bloom += sample_clamped(uv + vec2<f32>(-spread.x,  0.0     )).rgb;
        bloom += sample_clamped(uv + vec2<f32>( spread.x,  0.0     )).rgb;
        bloom += sample_clamped(uv + vec2<f32>(-spread.x,  spread.y)).rgb;
        bloom += sample_clamped(uv + vec2<f32>( 0.0,       spread.y)).rgb;
        bloom += sample_clamped(uv + vec2<f32>( spread.x,  spread.y)).rgb;
        bloom = bloom / 8.0;

        // Tint bloom by the phosphor glow color and blend additively.
        let glow = bloom * params.glow_color;
        color = color + glow * params.bloom_intensity * 0.7;
    }

    // ------------------------------------------------------------------
    // 4. Scanlines
    // ------------------------------------------------------------------
    if params.scanline_intensity > 0.0 {
        let pixel_y = uv.y * params.resolution.y;
        // Sinusoidal scanline pattern — subtle darkening on alternating lines.
        // Using a sine wave gives a smoother, less harsh result than a step.
        let scanline = 1.0 - params.scanline_intensity * 0.3 * (1.0 + sin(pixel_y * 3.14159265 * 2.0)) * 0.5;
        color = color * scanline;
    }

    // ------------------------------------------------------------------
    // 5. Vignette
    // ------------------------------------------------------------------
    if params.vignette_intensity > 0.0 {
        let centered = uv - vec2<f32>(0.5, 0.5);
        let dist = length(centered);
        // Smooth falloff from center to edges. The 0.7071 is sqrt(0.5),
        // the maximum distance from center to corner in UV space.
        let vig = smoothstep(0.2, 0.7071, dist * params.vignette_intensity);
        color = color * (1.0 - vig * 0.85);
    }

    // ------------------------------------------------------------------
    // 6. Film grain / noise
    // ------------------------------------------------------------------
    if params.noise_intensity > 0.0 {
        let noise_uv = uv * params.resolution + vec2<f32>(params.time * 127.1, params.time * 311.7);
        let grain = hash(noise_uv) - 0.5; // -0.5 to +0.5
        color = color + vec3<f32>(grain * params.noise_intensity * 0.12);
    }

    return vec4<f32>(clamp(color, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
"#;

// ---------------------------------------------------------------------------
// Uniform data — must match the WGSL struct layout exactly
// ---------------------------------------------------------------------------

/// CRT post-processing parameters uploaded to the GPU each frame.
///
/// Layout matches the WGSL `PostFxParams` struct exactly. Padded to satisfy
/// WebGPU uniform buffer alignment rules (vec3 aligns to 16 bytes).
///
/// ```text
/// offset  field
///   0     scanline_intensity   f32
///   4     bloom_intensity      f32
///   8     chromatic_aberration f32
///  12     curvature            f32
///  16     vignette_intensity   f32
///  20     noise_intensity      f32
///  24     time                 f32
///  28     _pad0                f32
///  32     glow_color           vec3<f32> (12 bytes, aligned to 16)
///  44     _pad1                f32
///  48     resolution           vec2<f32>
///  56     _pad2                vec2<f32>
///  64     total
/// ```
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
pub struct PostFxParams {
    pub scanline_intensity: f32,
    pub bloom_intensity: f32,
    pub chromatic_aberration: f32,
    pub curvature: f32,
    pub vignette_intensity: f32,
    pub noise_intensity: f32,
    pub time: f32,
    pub _pad0: f32,
    pub glow_color: [f32; 3],
    pub _pad1: f32,
    pub resolution: [f32; 2],
    pub _pad2: [f32; 2],
}

// Compile-time check: the struct must be exactly 64 bytes to match the WGSL layout.
const _: () = assert!(size_of::<PostFxParams>() == 64);

impl PostFxParams {
    /// Create params from theme shader params, elapsed time, and screen dimensions.
    pub fn from_theme(
        scanline_intensity: f32,
        bloom_intensity: f32,
        chromatic_aberration: f32,
        curvature: f32,
        vignette_intensity: f32,
        noise_intensity: f32,
        glow_color: [f32; 3],
        time: f32,
        width: u32,
        height: u32,
    ) -> Self {
        Self {
            scanline_intensity,
            bloom_intensity,
            chromatic_aberration,
            curvature,
            vignette_intensity,
            noise_intensity,
            time,
            _pad0: 0.0,
            glow_color,
            _pad1: 0.0,
            resolution: [width as f32, height as f32],
            _pad2: [0.0; 2],
        }
    }
}

// ---------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------

/// Full-screen CRT post-processing pipeline.
///
/// Owns the offscreen render target that the scene renders *into*, and a
/// pipeline that composites that texture onto the final surface with CRT
/// effects applied in the fragment shader.
pub struct PostFxPipeline {
    /// Offscreen texture the scene renders into.
    offscreen_texture: Texture,
    /// View into the offscreen texture (used as both render target and shader input).
    offscreen_view: TextureView,
    /// Render pipeline for the post-processing full-screen triangle.
    pipeline: RenderPipeline,
    /// Bind group layout (needed for recreation on resize).
    bind_group_layout: BindGroupLayout,
    /// Bind group referencing the offscreen texture, sampler, and uniform buffer.
    bind_group: BindGroup,
    /// Uniform buffer holding `PostFxParams`.
    uniform_buf: Buffer,
    /// Surface format, stored for texture recreation on resize.
    format: TextureFormat,
}

impl PostFxPipeline {
    /// Create the post-processing pipeline.
    ///
    /// The offscreen texture is created at the given dimensions and matches the
    /// surface format so the scene can render directly into it.
    pub fn new(device: &Device, surface_format: TextureFormat, width: u32, height: u32) -> Self {
        // -- Shader module --
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("postfx-shader"),
            source: wgpu::ShaderSource::Wgsl(POSTFX_SHADER.into()),
        });

        // -- Offscreen texture --
        let (offscreen_texture, offscreen_view) =
            create_offscreen_texture(device, surface_format, width, height);

        // -- Sampler (linear filtering for smooth CRT look) --
        let sampler = device.create_sampler(&SamplerDescriptor {
            label: Some("postfx-sampler"),
            mag_filter: FilterMode::Linear,
            min_filter: FilterMode::Linear,
            mipmap_filter: FilterMode::Nearest,
            ..Default::default()
        });

        // -- Uniform buffer --
        let initial_params = PostFxParams {
            scanline_intensity: 0.0,
            bloom_intensity: 0.0,
            chromatic_aberration: 0.0,
            curvature: 0.0,
            vignette_intensity: 0.0,
            noise_intensity: 0.0,
            time: 0.0,
            _pad0: 0.0,
            glow_color: [1.0, 1.0, 1.0],
            _pad1: 0.0,
            resolution: [width as f32, height as f32],
            _pad2: [0.0; 2],
        };
        let uniform_buf = device.create_buffer_init(&BufferInitDescriptor {
            label: Some("postfx-uniform-buf"),
            contents: bytemuck::bytes_of(&initial_params),
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
        });

        // -- Bind group layout --
        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("postfx-bind-group-layout"),
            entries: &[
                // @binding(0): scene texture
                BindGroupLayoutEntry {
                    binding: 0,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Texture {
                        sample_type: TextureSampleType::Float { filterable: true },
                        view_dimension: TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                // @binding(1): sampler
                BindGroupLayoutEntry {
                    binding: 1,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Sampler(SamplerBindingType::Filtering),
                    count: None,
                },
                // @binding(2): PostFxParams uniform
                BindGroupLayoutEntry {
                    binding: 2,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        // -- Bind group --
        let bind_group = create_bind_group(
            device,
            &bind_group_layout,
            &offscreen_view,
            &sampler,
            &uniform_buf,
        );

        // -- Pipeline layout --
        let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("postfx-pipeline-layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        // -- Render pipeline --
        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("postfx-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[], // no vertex buffers — positions computed from vertex_index
            },
            fragment: Some(FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(ColorTargetState {
                    format: surface_format,
                    blend: None, // post-fx writes final pixels directly
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

        Self {
            offscreen_texture,
            offscreen_view,
            pipeline,
            bind_group_layout,
            bind_group,
            uniform_buf,
            format: surface_format,
        }
    }

    /// Recreate the offscreen texture after a window resize.
    ///
    /// The bind group is also recreated since it references the texture view.
    pub fn resize(&mut self, device: &Device, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }

        let (texture, view) = create_offscreen_texture(device, self.format, width, height);
        self.offscreen_texture = texture;
        self.offscreen_view = view;

        // Recreate sampler (same config) — needed for the new bind group.
        let sampler = device.create_sampler(&SamplerDescriptor {
            label: Some("postfx-sampler"),
            mag_filter: FilterMode::Linear,
            min_filter: FilterMode::Linear,
            mipmap_filter: FilterMode::Nearest,
            ..Default::default()
        });

        self.bind_group = create_bind_group(
            device,
            &self.bind_group_layout,
            &self.offscreen_view,
            &sampler,
            &self.uniform_buf,
        );
    }

    /// Returns the offscreen texture view that the scene should render into.
    ///
    /// The scene pass uses this as its color attachment. After the scene pass
    /// completes, call [`render`](Self::render) to composite the CRT effects
    /// onto the final surface.
    pub fn scene_view(&self) -> &TextureView {
        &self.offscreen_view
    }

    /// Update uniforms and draw the CRT post-processing pass.
    ///
    /// Renders a full-screen triangle that samples the offscreen scene texture
    /// with all CRT effects applied, outputting to `target_view` (the swap
    /// chain surface).
    pub fn render(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        target_view: &TextureView,
        queue: &Queue,
        params: &PostFxParams,
    ) {
        // Upload current frame's parameters.
        queue.write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(params));

        // Begin the post-processing render pass targeting the final surface.
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("postfx-pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });

        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        // Draw a single full-screen triangle (3 vertices, no vertex buffer).
        pass.draw(0..3, 0..1);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create the offscreen texture and its default view.
fn create_offscreen_texture(
    device: &Device,
    format: TextureFormat,
    width: u32,
    height: u32,
) -> (Texture, TextureView) {
    let texture = device.create_texture(&TextureDescriptor {
        label: Some("postfx-offscreen"),
        size: wgpu::Extent3d {
            width: width.max(1),
            height: height.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    (texture, view)
}

/// Create the bind group referencing the offscreen texture, sampler, and uniforms.
fn create_bind_group(
    device: &Device,
    layout: &BindGroupLayout,
    texture_view: &TextureView,
    sampler: &wgpu::Sampler,
    uniform_buf: &Buffer,
) -> BindGroup {
    device.create_bind_group(&BindGroupDescriptor {
        label: Some("postfx-bind-group"),
        layout,
        entries: &[
            BindGroupEntry {
                binding: 0,
                resource: BindingResource::TextureView(texture_view),
            },
            BindGroupEntry {
                binding: 1,
                resource: BindingResource::Sampler(sampler),
            },
            BindGroupEntry {
                binding: 2,
                resource: uniform_buf.as_entire_binding(),
            },
        ],
    })
}
