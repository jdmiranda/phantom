// Inline image rendering via the Kitty graphics protocol.
//
// Each image is uploaded to its own GPU texture and rendered as a textured quad
// at the specified cell position. The pipeline uses:
//   - Bind group 0: screen_size uniform (shared across all images)
//   - Bind group 1: per-image texture + sampler
//
// Images are drawn after cell backgrounds and text so they overlay the grid.

use std::collections::HashMap;

use wgpu::{
    util::{BufferInitDescriptor, DeviceExt},
    AddressMode, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingResource, BindingType, BlendState,
    Buffer, BufferBindingType, BufferUsages, ColorTargetState, ColorWrites, Device, Extent3d,
    FilterMode, FragmentState, MultisampleState, PipelineLayoutDescriptor, PrimitiveState,
    PrimitiveTopology, Queue, RenderPass, RenderPipeline, RenderPipelineDescriptor,
    SamplerBindingType, SamplerDescriptor, ShaderModuleDescriptor, ShaderStages, Texture,
    TextureDescriptor, TextureDimension, TextureFormat, TextureSampleType, TextureUsages,
    TextureView, TextureViewDimension, VertexState,
};

// ---------------------------------------------------------------------------
// WGSL shader
// ---------------------------------------------------------------------------

const IMAGE_SHADER_SRC: &str = r#"
// ---- Uniforms (group 0) ----
struct Uniforms {
    screen_size: vec2<f32>,
};

@group(0) @binding(0) var<uniform> uniforms: Uniforms;

// ---- Image texture + sampler (group 1) ----
@group(1) @binding(0) var image_texture: texture_2d<f32>;
@group(1) @binding(1) var image_sampler: sampler;

// ---- Per-instance data ----
struct ImageInstance {
    @location(0) pos: vec2<f32>,
    @location(1) size: vec2<f32>,
};

struct VertexOutput {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
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
    instance: ImageInstance,
) -> VertexOutput {
    let corner = UNIT_QUAD[vertex_idx];

    // Expand unit quad to image pixel dimensions at the instance position.
    let pixel_pos = instance.pos + corner * instance.size;

    // Convert pixel coordinates to NDC.
    // NDC x: -1 (left) to +1 (right)
    // NDC y: +1 (top)  to -1 (bottom) — y flipped so (0,0) is top-left.
    let ndc = vec2<f32>(
        (pixel_pos.x / uniforms.screen_size.x) * 2.0 - 1.0,
        1.0 - (pixel_pos.y / uniforms.screen_size.y) * 2.0,
    );

    var out: VertexOutput;
    out.clip_pos = vec4<f32>(ndc, 0.0, 1.0);
    out.uv = corner;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return textureSample(image_texture, image_sampler, in.uv);
}
"#;

// ---------------------------------------------------------------------------
// GPU instance data
// ---------------------------------------------------------------------------

/// Per-image instance data uploaded to a tiny vertex buffer for each draw.
#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct ImageInstance {
    /// Top-left position in pixels.
    pos: [f32; 2],
    /// Display size in pixels.
    size: [f32; 2],
}

impl ImageInstance {
    fn layout() -> wgpu::VertexBufferLayout<'static> {
        const ATTRS: &[wgpu::VertexAttribute] = &[
            // pos: [f32; 2]
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x2,
                offset: 0,
                shader_location: 0,
            },
            // size: [f32; 2]
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x2,
                offset: 8,
                shader_location: 1,
            },
        ];

        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<ImageInstance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
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
// Public types
// ---------------------------------------------------------------------------

/// A decoded image ready for GPU upload.
pub struct DecodedImage {
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
    /// RGBA pixel data, `width * height * 4` bytes.
    pub data: Vec<u8>,
}

impl DecodedImage {
    /// Decode a PNG file from raw bytes into an RGBA `DecodedImage`.
    pub fn from_png(png_bytes: &[u8]) -> Option<Self> {
        let decoder = png::Decoder::new(png_bytes);
        let mut reader = decoder.read_info().ok()?;
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).ok()?;
        buf.truncate(info.buffer_size());

        let width = info.width;
        let height = info.height;

        let rgba = match info.color_type {
            png::ColorType::Rgba => buf,
            png::ColorType::Rgb => {
                let pixel_count = (width * height) as usize;
                let mut rgba = Vec::with_capacity(pixel_count * 4);
                for chunk in buf.chunks_exact(3) {
                    rgba.extend_from_slice(chunk);
                    rgba.push(255);
                }
                rgba
            }
            png::ColorType::GrayscaleAlpha => {
                let pixel_count = (width * height) as usize;
                let mut rgba = Vec::with_capacity(pixel_count * 4);
                for chunk in buf.chunks_exact(2) {
                    let g = chunk[0];
                    let a = chunk[1];
                    rgba.extend_from_slice(&[g, g, g, a]);
                }
                rgba
            }
            png::ColorType::Grayscale => {
                let pixel_count = (width * height) as usize;
                let mut rgba = Vec::with_capacity(pixel_count * 4);
                for &g in &buf[..pixel_count] {
                    rgba.extend_from_slice(&[g, g, g, 255]);
                }
                rgba
            }
            png::ColorType::Indexed => {
                // Indexed PNG is uncommon for terminal images; skip.
                return None;
            }
        };

        Some(Self {
            width,
            height,
            data: rgba,
        })
    }

    /// Create a `DecodedImage` from raw RGB data (24-bit, no alpha).
    pub fn from_rgb(width: u32, height: u32, rgb: &[u8]) -> Option<Self> {
        let expected = (width * height * 3) as usize;
        if rgb.len() < expected {
            return None;
        }

        let mut rgba = Vec::with_capacity((width * height * 4) as usize);
        for chunk in rgb[..expected].chunks_exact(3) {
            rgba.extend_from_slice(chunk);
            rgba.push(255);
        }

        Some(Self {
            width,
            height,
            data: rgba,
        })
    }

    /// Create a `DecodedImage` from raw RGBA data (32-bit).
    pub fn from_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Option<Self> {
        let expected = (width * height * 4) as usize;
        if rgba.len() < expected {
            return None;
        }

        Some(Self {
            width,
            height,
            data: rgba,
        })
    }
}

/// A GPU-resident image with its texture and positioning info.
pub struct ImagePlacement {
    /// The GPU texture holding the image data.
    pub texture: Texture,
    /// View into the texture.
    pub view: TextureView,
    /// Bind group for this image (texture + sampler), bound at group 1.
    pub bind_group: BindGroup,
    /// Instance buffer containing position + size for this image.
    instance_buf: Buffer,
    /// Pixel position x.
    pub x: f32,
    /// Pixel position y.
    pub y: f32,
    /// Display width in pixels.
    pub width: f32,
    /// Display height in pixels.
    pub height: f32,
    /// Cell column in the terminal grid.
    pub cell_col: u32,
    /// Cell row in the terminal grid.
    pub cell_row: u32,
    /// Number of cells wide the image spans.
    pub cols_span: u32,
    /// Number of cells tall the image spans.
    pub rows_span: u32,
}

// ---------------------------------------------------------------------------
// ImageManager
// ---------------------------------------------------------------------------

/// Manages image textures for inline rendering in the terminal.
///
/// Each image gets its own GPU texture, bind group, and instance buffer.
/// A shared render pipeline and uniform buffer handle coordinate transforms.
pub struct ImageManager {
    images: HashMap<u32, ImagePlacement>,
    pipeline: RenderPipeline,
    texture_bind_group_layout: BindGroupLayout,
    uniform_bind_group: BindGroup,
    #[allow(dead_code)]
    uniform_bind_group_layout: BindGroupLayout,
    uniform_buf: Buffer,
    sampler: wgpu::Sampler,
    next_id: u32,
}

impl ImageManager {
    /// Create the image rendering pipeline.
    pub fn new(device: &Device, format: TextureFormat) -> Self {
        // -- Shader module --
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("image-shader"),
            source: wgpu::ShaderSource::Wgsl(IMAGE_SHADER_SRC.into()),
        });

        // -- Uniform bind group layout (group 0) --
        let uniform_bind_group_layout =
            device.create_bind_group_layout(&BindGroupLayoutDescriptor {
                label: Some("image-uniform-layout"),
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

        // -- Texture bind group layout (group 1) --
        let texture_bind_group_layout =
            device.create_bind_group_layout(&BindGroupLayoutDescriptor {
                label: Some("image-texture-layout"),
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

        // -- Pipeline layout: group 0 = uniforms, group 1 = per-image texture --
        let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("image-pipeline-layout"),
            bind_group_layouts: &[&uniform_bind_group_layout, &texture_bind_group_layout],
            push_constant_ranges: &[],
        });

        // -- Render pipeline --
        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("image-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[ImageInstance::layout()],
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
            label: Some("image-uniform-buf"),
            contents: bytemuck::bytes_of(&uniforms),
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
        });

        // -- Uniform bind group --
        let uniform_bind_group = device.create_bind_group(&BindGroupDescriptor {
            label: Some("image-uniform-bind-group"),
            layout: &uniform_bind_group_layout,
            entries: &[BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });

        // -- Shared sampler for all image textures --
        let sampler = device.create_sampler(&SamplerDescriptor {
            label: Some("image-sampler"),
            address_mode_u: AddressMode::ClampToEdge,
            address_mode_v: AddressMode::ClampToEdge,
            mag_filter: FilterMode::Linear,
            min_filter: FilterMode::Linear,
            ..Default::default()
        });

        Self {
            images: HashMap::new(),
            pipeline,
            texture_bind_group_layout,
            uniform_bind_group,
            uniform_bind_group_layout,
            uniform_buf,
            sampler,
            next_id: 1,
        }
    }

    /// Upload a decoded image and place it at the given cell position.
    ///
    /// Returns an image ID that can be used to remove the image later.
    pub fn place_image(
        &mut self,
        device: &Device,
        queue: &Queue,
        image: DecodedImage,
        cell_col: u32,
        cell_row: u32,
        cell_size: (f32, f32),
    ) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);

        // Compute pixel position and display size.
        let x = cell_col as f32 * cell_size.0;
        let y = cell_row as f32 * cell_size.1;
        let width = image.width as f32;
        let height = image.height as f32;

        // How many cells the image spans.
        let cols_span = (width / cell_size.0).ceil() as u32;
        let rows_span = (height / cell_size.1).ceil() as u32;

        // -- Create GPU texture --
        let texture = device.create_texture(&TextureDescriptor {
            label: Some("image-texture"),
            size: Extent3d {
                width: image.width,
                height: image.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::Rgba8UnormSrgb,
            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
            view_formats: &[],
        });

        // Upload pixel data.
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &image.data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(image.width * 4),
                rows_per_image: None,
            },
            Extent3d {
                width: image.width,
                height: image.height,
                depth_or_array_layers: 1,
            },
        );

        let view = texture.create_view(&Default::default());

        // -- Per-image bind group (texture + sampler) --
        let bind_group = device.create_bind_group(&BindGroupDescriptor {
            label: Some("image-bind-group"),
            layout: &self.texture_bind_group_layout,
            entries: &[
                BindGroupEntry {
                    binding: 0,
                    resource: BindingResource::TextureView(&view),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        // -- Instance buffer with position + size --
        let instance = ImageInstance {
            pos: [x, y],
            size: [width, height],
        };
        let instance_buf = device.create_buffer_init(&BufferInitDescriptor {
            label: Some("image-instance-buf"),
            contents: bytemuck::bytes_of(&instance),
            usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
        });

        self.images.insert(
            id,
            ImagePlacement {
                texture,
                view,
                bind_group,
                instance_buf,
                x,
                y,
                width,
                height,
                cell_col,
                cell_row,
                cols_span,
                rows_span,
            },
        );

        id
    }

    /// Remove an image by ID. The GPU resources are dropped immediately.
    pub fn remove_image(&mut self, id: u32) {
        self.images.remove(&id);
    }

    /// Clear all images.
    pub fn clear(&mut self) {
        self.images.clear();
    }

    /// Number of placed images.
    pub fn image_count(&self) -> usize {
        self.images.len()
    }

    /// Update the screen-size uniform buffer for the current frame.
    pub fn prepare(&mut self, queue: &Queue, screen_size: [f32; 2]) {
        let uniforms = Uniforms {
            screen_size,
            _pad: [0.0; 2],
        };
        queue.write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&uniforms));
    }

    /// Update a placement's pixel position (e.g., after a scroll or resize).
    ///
    /// This rewrites the instance buffer so the image is drawn at the new position.
    pub fn update_position(
        &mut self,
        queue: &Queue,
        id: u32,
        cell_col: u32,
        cell_row: u32,
        cell_size: (f32, f32),
    ) {
        if let Some(placement) = self.images.get_mut(&id) {
            let x = cell_col as f32 * cell_size.0;
            let y = cell_row as f32 * cell_size.1;
            placement.x = x;
            placement.y = y;
            placement.cell_col = cell_col;
            placement.cell_row = cell_row;

            let instance = ImageInstance {
                pos: [x, y],
                size: [placement.width, placement.height],
            };
            queue.write_buffer(
                &placement.instance_buf,
                0,
                bytemuck::bytes_of(&instance),
            );
        }
    }

    /// Render all placed images.
    ///
    /// Each image is drawn as a separate draw call since each has its own
    /// texture bind group. The pipeline and uniform bind group are shared.
    pub fn render<'pass>(&'pass self, render_pass: &mut RenderPass<'pass>) {
        if self.images.is_empty() {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.uniform_bind_group, &[]);

        for placement in self.images.values() {
            render_pass.set_bind_group(1, &placement.bind_group, &[]);
            render_pass.set_vertex_buffer(0, placement.instance_buf.slice(..));
            // 6 vertices (two triangles), 1 instance per image.
            render_pass.draw(0..6, 0..1);
        }
    }

    /// Get a reference to a placed image by ID.
    pub fn get(&self, id: u32) -> Option<&ImagePlacement> {
        self.images.get(&id)
    }

    /// Iterate over all placed images.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &ImagePlacement)> {
        self.images.iter().map(|(&id, p)| (id, p))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_instance_is_pod() {
        let instance = ImageInstance {
            pos: [10.0, 20.0],
            size: [100.0, 80.0],
        };
        let bytes = bytemuck::bytes_of(&instance);
        assert_eq!(bytes.len(), std::mem::size_of::<ImageInstance>());
        // 4 floats * 4 bytes = 16
        assert_eq!(std::mem::size_of::<ImageInstance>(), 16);
    }

    #[test]
    fn uniforms_are_16_byte_aligned() {
        assert_eq!(std::mem::size_of::<Uniforms>(), 16);
    }

    #[test]
    fn decoded_image_from_rgb() {
        let width = 2;
        let height = 2;
        let rgb = vec![
            255, 0, 0, // red
            0, 255, 0, // green
            0, 0, 255, // blue
            255, 255, 0, // yellow
        ];
        let img = DecodedImage::from_rgb(width, height, &rgb).unwrap();
        assert_eq!(img.width, 2);
        assert_eq!(img.height, 2);
        assert_eq!(img.data.len(), 16); // 2*2*4
        // First pixel: red with alpha 255
        assert_eq!(&img.data[0..4], &[255, 0, 0, 255]);
        // Second pixel: green with alpha 255
        assert_eq!(&img.data[4..8], &[0, 255, 0, 255]);
    }

    #[test]
    fn decoded_image_from_rgba() {
        let width = 1;
        let height = 1;
        let rgba = vec![128, 64, 32, 200];
        let img = DecodedImage::from_rgba(width, height, rgba.clone()).unwrap();
        assert_eq!(img.width, 1);
        assert_eq!(img.height, 1);
        assert_eq!(img.data, rgba);
    }

    #[test]
    fn decoded_image_from_rgb_too_short() {
        let img = DecodedImage::from_rgb(2, 2, &[0, 0, 0]);
        assert!(img.is_none());
    }

    #[test]
    fn decoded_image_from_rgba_too_short() {
        let img = DecodedImage::from_rgba(2, 2, vec![0, 0, 0, 0]);
        assert!(img.is_none());
    }

    #[test]
    fn decoded_image_from_png_valid() {
        // Create a minimal 1x1 red PNG in memory.
        let mut png_data = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png_data, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[255, 0, 0, 255]).unwrap();
        }

        let img = DecodedImage::from_png(&png_data).unwrap();
        assert_eq!(img.width, 1);
        assert_eq!(img.height, 1);
        assert_eq!(img.data, vec![255, 0, 0, 255]);
    }

    #[test]
    fn decoded_image_from_png_rgb() {
        // Create a 1x1 green PNG with RGB (no alpha).
        let mut png_data = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png_data, 1, 1);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[0, 255, 0]).unwrap();
        }

        let img = DecodedImage::from_png(&png_data).unwrap();
        assert_eq!(img.width, 1);
        assert_eq!(img.height, 1);
        assert_eq!(img.data, vec![0, 255, 0, 255]);
    }

    #[test]
    fn decoded_image_from_png_grayscale() {
        // Create a 1x1 gray PNG.
        let mut png_data = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png_data, 1, 1);
            encoder.set_color(png::ColorType::Grayscale);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[128]).unwrap();
        }

        let img = DecodedImage::from_png(&png_data).unwrap();
        assert_eq!(img.data, vec![128, 128, 128, 255]);
    }

    #[test]
    fn decoded_image_from_png_invalid_bytes() {
        let img = DecodedImage::from_png(&[0, 1, 2, 3]);
        assert!(img.is_none());
    }

    #[test]
    fn instance_buffer_layout() {
        let layout = ImageInstance::layout();
        assert_eq!(layout.array_stride, 16);
        assert_eq!(layout.step_mode, wgpu::VertexStepMode::Instance);
        assert_eq!(layout.attributes.len(), 2);
    }
}
