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
    RenderPipelineDescriptor, Sampler, SamplerBindingType, SamplerDescriptor,
    ShaderModuleDescriptor, ShaderStages, Texture, TextureDescriptor, TextureDimension,
    TextureFormat, TextureSampleType, TextureUsages, TextureView, TextureViewDimension,
    VertexState,
};

// ---------------------------------------------------------------------------
// WGSL shader — the CRT post-processing heart of Phantom
// ---------------------------------------------------------------------------
//
// The authoritative shader source lives in `shaders/crt.wgsl` at the workspace
// root.  It is embedded at compile time via `include_str!` so the binary is
// self-contained and naga validation tests work without a working-directory
// constraint.
//
// Hot-reload (debug builds only):
//   Set `PHANTOM_HOT_SHADERS=1` in the environment before launching the binary.
//   On each pipeline creation (startup or resize-triggered recreation) the
//   shader is read from `shaders/crt.wgsl` relative to the current working
//   directory, allowing live WGSL iteration without a Rust recompile.
//   If the file cannot be read, the embedded copy is used as a fallback.

/// WGSL source for the CRT post-processing shader, embedded at compile time
/// from `shaders/crt.wgsl`.
///
/// Exposed as `pub` so integration tests can validate it with naga without
/// needing a GPU device.
pub const CRT_WGSL: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../shaders/crt.wgsl"));

/// Return the WGSL source to use for the current pipeline creation.
///
/// * **Release builds / tests**: always returns [`CRT_WGSL`] (the compile-time
///   embedded string, zero allocation).
/// * **Debug builds with `PHANTOM_HOT_SHADERS=1`**: reads `shaders/crt.wgsl`
///   from the current working directory.  Falls back to the embedded copy and
///   logs a warning on I/O error.
fn crt_wgsl_source() -> std::borrow::Cow<'static, str> {
    #[cfg(debug_assertions)]
    if std::env::var_os("PHANTOM_HOT_SHADERS").is_some() {
        match std::fs::read_to_string("shaders/crt.wgsl") {
            Ok(src) => {
                log::debug!("postfx: hot-reloaded shaders/crt.wgsl from disk");
                return std::borrow::Cow::Owned(src);
            }
            Err(e) => {
                log::warn!(
                    "postfx: hot-reload failed ({}), using embedded shader",
                    e
                );
            }
        }
    }
    std::borrow::Cow::Borrowed(CRT_WGSL)
}

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
    /// Linear sampler for the offscreen texture — created once, reused on resize.
    sampler: Sampler,
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
        // `crt_wgsl_source()` returns the embedded compile-time copy in
        // release/test builds, or reads from disk when PHANTOM_HOT_SHADERS
        // is set in a debug build.
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("postfx-shader"),
            source: wgpu::ShaderSource::Wgsl(crt_wgsl_source()),
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
            sampler,
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

        // Reuse the existing sampler — it's immutable and config-independent.
        self.bind_group = create_bind_group(
            device,
            &self.bind_group_layout,
            &self.offscreen_view,
            &self.sampler,
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

    /// Returns the offscreen scene texture itself (for screenshot readback).
    ///
    /// The texture is created with `COPY_SRC` usage so it can be copied into
    /// a staging buffer for PNG encoding. This is the pre-CRT scene — clean
    /// pixels, useful for UI debugging without shader distortion.
    pub fn scene_texture(&self) -> &Texture {
        &self.offscreen_texture
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
        usage: TextureUsages::RENDER_ATTACHMENT
            | TextureUsages::TEXTURE_BINDING
            | TextureUsages::COPY_SRC,
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
