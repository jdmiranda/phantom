//! GPU-accelerated video frame renderer.
//!
//! Owns a single GPU texture that gets updated each frame with new pixel data.
//! Renders as a textured quad positioned in pixel coordinates. Rendered in the
//! scene pass so it gets full CRT post-processing (scanlines, bloom, etc.).

use wgpu::{
    util::{BufferInitDescriptor, DeviceExt},
    BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout, BindGroupLayoutDescriptor,
    BindGroupLayoutEntry, BindingResource, BindingType, BlendState, Buffer, BufferBindingType,
    BufferUsages, ColorTargetState, ColorWrites, Device, Extent3d, FilterMode, FragmentState,
    MultisampleState, PipelineLayoutDescriptor, PrimitiveState, PrimitiveTopology, Queue,
    RenderPass, RenderPipeline, RenderPipelineDescriptor, SamplerBindingType, SamplerDescriptor,
    ShaderModuleDescriptor, ShaderStages, Texture, TextureDescriptor, TextureDimension,
    TextureFormat, TextureSampleType, TextureUsages, TextureView, TextureViewDimension,
    VertexState, AddressMode,
};

// Reuse the image shader — same textured quad logic.
const VIDEO_SHADER_SRC: &str = r#"
struct Uniforms { screen_size: vec2<f32> };
@group(0) @binding(0) var<uniform> uniforms: Uniforms;
@group(1) @binding(0) var frame_texture: texture_2d<f32>;
@group(1) @binding(1) var frame_sampler: sampler;

struct Instance { @location(0) pos: vec2<f32>, @location(1) size: vec2<f32> };
struct VOut { @builtin(position) clip_pos: vec4<f32>, @location(0) uv: vec2<f32> };

var<private> QUAD: array<vec2<f32>, 6> = array<vec2<f32>, 6>(
    vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0),
    vec2(1.0, 0.0), vec2(1.0, 1.0), vec2(0.0, 1.0),
);

@vertex fn vs_main(@builtin(vertex_index) vi: u32, inst: Instance) -> VOut {
    let c = QUAD[vi];
    let px = inst.pos + c * inst.size;
    let ndc = vec2(px.x / uniforms.screen_size.x * 2.0 - 1.0,
                   1.0 - px.y / uniforms.screen_size.y * 2.0);
    var o: VOut;
    o.clip_pos = vec4(ndc, 0.0, 1.0);
    o.uv = c;
    return o;
}

@fragment fn fs_main(in: VOut) -> @location(0) vec4<f32> {
    return textureSample(frame_texture, frame_sampler, in.uv);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct QuadInstance {
    pos: [f32; 2],
    size: [f32; 2],
}

/// GPU-side video frame renderer. Owns a single texture that gets updated
/// each frame with new RGBA pixel data from the decoder.
pub struct VideoRenderer {
    pipeline: RenderPipeline,
    texture_layout: BindGroupLayout,
    uniform_bind_group: BindGroup,
    uniform_buf: Buffer,
    sampler: wgpu::Sampler,

    // Current frame state (None until first frame uploaded).
    texture: Option<Texture>,
    texture_view: Option<TextureView>,
    texture_bind_group: Option<BindGroup>,
    instance_buf: Option<Buffer>,
    frame_width: u32,
    frame_height: u32,
}

impl VideoRenderer {
    pub fn new(device: &Device, format: TextureFormat) -> Self {
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("video-shader"),
            source: wgpu::ShaderSource::Wgsl(VIDEO_SHADER_SRC.into()),
        });

        let uniform_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("video-uniform-layout"),
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

        let texture_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("video-texture-layout"),
            entries: &[
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
                BindGroupLayoutEntry {
                    binding: 1,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Sampler(SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("video-pipeline-layout"),
            bind_group_layouts: &[&uniform_layout, &texture_layout],
            push_constant_ranges: &[],
        });

        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<QuadInstance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &[
                wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 0, shader_location: 0 },
                wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 8, shader_location: 1 },
            ],
        };

        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("video-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[instance_layout],
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

        let uniform_buf = device.create_buffer_init(&BufferInitDescriptor {
            label: Some("video-uniform-buf"),
            contents: bytemuck::cast_slice(&[0.0f32, 0.0]),
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
        });

        let uniform_bind_group = device.create_bind_group(&BindGroupDescriptor {
            label: Some("video-uniform-bg"),
            layout: &uniform_layout,
            entries: &[BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });

        let sampler = device.create_sampler(&SamplerDescriptor {
            label: Some("video-sampler"),
            address_mode_u: AddressMode::ClampToEdge,
            address_mode_v: AddressMode::ClampToEdge,
            mag_filter: FilterMode::Linear,
            min_filter: FilterMode::Linear,
            ..Default::default()
        });

        Self {
            pipeline,
            texture_layout,
            uniform_bind_group,
            uniform_buf,
            sampler,
            texture: None,
            texture_view: None,
            texture_bind_group: None,
            instance_buf: None,
            frame_width: 0,
            frame_height: 0,
        }
    }

    /// Upload a new RGBA frame. Recreates the texture only if the resolution changed.
    pub fn upload_frame(
        &mut self,
        device: &Device,
        queue: &Queue,
        width: u32,
        height: u32,
        rgba_data: &[u8],
    ) {
        // Recreate texture if resolution changed.
        if width != self.frame_width || height != self.frame_height {
            let texture = device.create_texture(&TextureDescriptor {
                label: Some("video-frame-texture"),
                size: Extent3d { width, height, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: TextureDimension::D2,
                format: TextureFormat::Rgba8UnormSrgb,
                usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            let bind_group = device.create_bind_group(&BindGroupDescriptor {
                label: Some("video-frame-bg"),
                layout: &self.texture_layout,
                entries: &[
                    BindGroupEntry { binding: 0, resource: BindingResource::TextureView(&view) },
                    BindGroupEntry { binding: 1, resource: BindingResource::Sampler(&self.sampler) },
                ],
            });
            self.texture = Some(texture);
            self.texture_view = Some(view);
            self.texture_bind_group = Some(bind_group);
            self.frame_width = width;
            self.frame_height = height;
        }

        // Upload pixel data to existing texture.
        if let Some(ref texture) = self.texture {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                rgba_data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width * 4),
                    rows_per_image: None,
                },
                Extent3d { width, height, depth_or_array_layers: 1 },
            );
        }
    }

    /// Update the screen-size uniform and instance position/size.
    pub fn prepare(
        &mut self,
        device: &Device,
        queue: &Queue,
        screen_size: [f32; 2],
        pos: [f32; 2],
        size: [f32; 2],
    ) {
        queue.write_buffer(&self.uniform_buf, 0, bytemuck::cast_slice(&screen_size));

        let instance = QuadInstance { pos, size };
        match &self.instance_buf {
            Some(buf) => {
                queue.write_buffer(buf, 0, bytemuck::bytes_of(&instance));
            }
            None => {
                self.instance_buf = Some(device.create_buffer_init(&BufferInitDescriptor {
                    label: Some("video-instance-buf"),
                    contents: bytemuck::bytes_of(&instance),
                    usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
                }));
            }
        }
    }

    /// Render the current frame. Call inside a render pass.
    pub fn render<'pass>(&'pass self, render_pass: &mut RenderPass<'pass>) {
        let (Some(bg), Some(inst)) = (&self.texture_bind_group, &self.instance_buf) else {
            return;
        };
        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.uniform_bind_group, &[]);
        render_pass.set_bind_group(1, bg, &[]);
        render_pass.set_vertex_buffer(0, inst.slice(..));
        render_pass.draw(0..6, 0..1);
    }

    /// Whether a frame has been uploaded.
    pub fn has_frame(&self) -> bool {
        self.texture.is_some()
    }

    /// Clear the current frame — stops rendering the video quad.
    pub fn clear(&mut self) {
        self.texture = None;
        self.texture_view = None;
        self.texture_bind_group = None;
        self.frame_width = 0;
        self.frame_height = 0;
    }
}
