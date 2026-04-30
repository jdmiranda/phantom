// Glyph atlas: alpha-only GPU texture (R8Unorm) for monochrome glyphs plus a
// separate RGBA atlas (Rgba8UnormSrgb) for color emoji and other color glyphs.
//
// Caches rasterized glyphs (from cosmic-text/swash) into two GPU textures:
//   - `GlyphAtlas`      — R8Unorm, single-channel alpha mask. Monochrome text,
//                          icon fonts, SubpixelMask glyphs.
//   - `ColorGlyphAtlas` — Rgba8UnormSrgb, full-color RGBA. SwashContent::Color
//                          glyphs (emoji, CBDT/COLR/sbix bitmaps). Rendered
//                          WITHOUT foreground tinting so colors are preserved.
//
// The text renderer looks up cached glyphs by `CacheKey` and gets back UV
// coordinates plus an `is_color` flag that tells the draw caller which atlas
// (and which shader pipeline) to use for a given instance.

use std::collections::HashMap;

use cosmic_text::{CacheKey, FontSystem, SwashCache, SwashContent};
use etagere::{size2, Allocation, AtlasAllocator};
use wgpu::{
    AddressMode, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingResource, BindingType, Device,
    Extent3d, FilterMode, Queue, SamplerBindingType, SamplerDescriptor, ShaderStages, Texture,
    TextureDescriptor, TextureDimension, TextureFormat, TextureSampleType, TextureUsages,
    TextureView, TextureViewDimension,
};

/// Initial alpha atlas dimensions. 1024x1024 single-channel = 1 MiB.
const ATLAS_SIZE: u32 = 1024;

/// Initial color atlas dimensions. 512x512 RGBA = 1 MiB.
///
/// Color emoji glyphs are typically larger than monochrome glyphs, but there
/// are far fewer of them in typical terminal output. 512x512 gives 64 KiB of
/// layout area — enough for ~256 16x16 emoji or ~64 32x32 emoji — without
/// blowing the memory budget of the alpha atlas.
const COLOR_ATLAS_SIZE: u32 = 512;

/// Cached glyph location in an atlas.
///
/// `is_color` indicates which atlas (and shader pipeline) to use:
/// - `false` → `GlyphAtlas` (R8Unorm, alpha mask, apply foreground tint)
/// - `true`  → `ColorGlyphAtlas` (Rgba8UnormSrgb, full color, NO tint)
#[derive(Debug, Clone, Copy)]
pub struct GlyphEntry {
    /// Pixel rectangle in atlas (x, y, w, h).
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// Normalized UV coordinates for the quad shader.
    pub uv_min: [f32; 2],
    pub uv_max: [f32; 2],
    /// Glyph placement offset (pixels from pen position).
    pub left: i32,
    pub top: i32,
    /// True when this entry lives in the color atlas (Rgba8UnormSrgb).
    ///
    /// When `true`, the caller MUST route this instance through the color-glyph
    /// shader pipeline (which samples RGBA and skips the foreground tint
    /// multiply). When `false` the standard alpha-mask pipeline applies.
    pub is_color: bool,
}

/// Alpha-only glyph atlas backed by a wgpu R8Unorm texture.
pub struct GlyphAtlas {
    texture: Texture,
    view: TextureView,
    allocator: AtlasAllocator,
    cache: HashMap<CacheKey, GlyphEntry>,
    bind_group: BindGroup,
    bind_group_layout: BindGroupLayout,
    width: u32,
    height: u32,
}

impl GlyphAtlas {
    /// Create a new glyph atlas with the given GPU device.
    #[must_use]
    pub fn new(device: &Device, _queue: &Queue) -> Self {
        let width = ATLAS_SIZE;
        let height = ATLAS_SIZE;

        let texture = device.create_texture(&TextureDescriptor {
            label: Some("glyph-atlas"),
            size: Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::R8Unorm,
            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
            view_formats: &[],
        });

        let view = texture.create_view(&Default::default());

        let sampler = device.create_sampler(&SamplerDescriptor {
            label: Some("glyph-atlas-sampler"),
            address_mode_u: AddressMode::ClampToEdge,
            address_mode_v: AddressMode::ClampToEdge,
            mag_filter: FilterMode::Linear,
            min_filter: FilterMode::Linear,
            ..Default::default()
        });

        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("glyph-atlas-layout"),
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

        let bind_group = device.create_bind_group(&BindGroupDescriptor {
            label: Some("glyph-atlas-bind-group"),
            layout: &bind_group_layout,
            entries: &[
                BindGroupEntry {
                    binding: 0,
                    resource: BindingResource::TextureView(&view),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: BindingResource::Sampler(&sampler),
                },
            ],
        });

        let allocator = AtlasAllocator::new(size2(width as i32, height as i32));

        Self {
            texture,
            view,
            allocator,
            cache: HashMap::new(),
            bind_group,
            bind_group_layout,
            width,
            height,
        }
    }

    /// Try to allocate a rectangular region in the atlas.
    ///
    /// Returns the etagere `Allocation` on success, or `None` if the atlas is full.
    pub fn allocate(&mut self, width: u32, height: u32) -> Option<Allocation> {
        if width == 0 || height == 0 {
            return None;
        }
        self.allocator.allocate(size2(width as i32, height as i32))
    }

    /// Upload glyph bitmap data into a previously allocated region.
    ///
    /// `data` must be `rect_width * rect_height` bytes of R8 pixel data.
    pub fn upload(&self, queue: &Queue, x: u32, y: u32, width: u32, height: u32, data: &[u8]) {
        debug_assert_eq!(
            data.len(),
            (width * height) as usize,
            "upload data size mismatch: expected {}x{}={}, got {}",
            width,
            height,
            width * height,
            data.len(),
        );

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width),
                rows_per_image: None,
            },
            Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Look up a glyph in the cache, or rasterize + allocate + upload it.
    ///
    /// Returns `None` if the glyph has no visible pixels (e.g. space) or the
    /// atlas is full.
    pub fn get_or_insert(
        &mut self,
        queue: &Queue,
        font_system: &mut FontSystem,
        swash_cache: &mut SwashCache,
        cache_key: CacheKey,
    ) -> Option<GlyphEntry> {
        // Fast path: already cached.
        if let Some(&entry) = self.cache.get(&cache_key) {
            return Some(entry);
        }

        // Rasterize the glyph via cosmic-text's swash integration.
        let image = swash_cache.get_image(font_system, cache_key).as_ref()?;

        let glyph_width = image.placement.width;
        let glyph_height = image.placement.height;

        if glyph_width == 0 || glyph_height == 0 {
            return None;
        }

        // Extract alpha channel depending on content type.
        // Color glyphs are NOT handled here — callers should use
        // `ColorGlyphAtlas::get_or_insert` for `SwashContent::Color` images.
        // If a color glyph somehow reaches this path, fall back to alpha-only
        // by extracting the alpha channel, preserving correctness at the cost
        // of losing color (same as the previous behaviour, better than a panic).
        let alpha_data: Vec<u8> = match image.content {
            SwashContent::Mask => image.data.clone(),
            SwashContent::Color => {
                // Fall-back: alpha-only extraction.
                // Callers that care about emoji color should route color glyphs
                // through `ColorGlyphAtlas` before reaching this point.
                image.data.chunks_exact(4).map(|px| px[3]).collect()
            }
            SwashContent::SubpixelMask => {
                // RGB subpixel — average the channels as a luminance proxy.
                image
                    .data
                    .chunks_exact(3)
                    .map(|px| {
                        let r = px[0] as u16;
                        let g = px[1] as u16;
                        let b = px[2] as u16;
                        ((r + g + b) / 3) as u8
                    })
                    .collect()
            }
        };

        let placement_left = image.placement.left;
        let placement_top = image.placement.top;

        // Allocate space in the atlas.
        let allocation = self.allocate(glyph_width, glyph_height)?;
        let rect = allocation.rectangle;

        let x = rect.min.x as u32;
        let y = rect.min.y as u32;

        // Upload the alpha bitmap.
        self.upload(queue, x, y, glyph_width, glyph_height, &alpha_data);

        // Compute normalized UV coordinates.
        let atlas_w = self.width as f32;
        let atlas_h = self.height as f32;
        let entry = GlyphEntry {
            x,
            y,
            width: glyph_width,
            height: glyph_height,
            uv_min: [x as f32 / atlas_w, y as f32 / atlas_h],
            uv_max: [
                (x + glyph_width) as f32 / atlas_w,
                (y + glyph_height) as f32 / atlas_h,
            ],
            left: placement_left,
            top: placement_top,
            is_color: false,
        };

        self.cache.insert(cache_key, entry);
        Some(entry)
    }

    /// The bind group layout for the atlas texture + sampler.
    ///
    /// Bind at group index appropriate for your pipeline (typically group 1).
    #[must_use]
    pub fn bind_group_layout(&self) -> &BindGroupLayout {
        &self.bind_group_layout
    }

    /// The bind group containing the atlas texture view and sampler.
    #[must_use]
    pub fn bind_group(&self) -> &BindGroup {
        &self.bind_group
    }

    /// The texture view, in case you need it outside the standard bind group.
    #[must_use]
    pub fn texture_view(&self) -> &TextureView {
        &self.view
    }

    /// Atlas dimensions in pixels.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Number of cached glyphs.
    #[must_use]
    pub fn glyph_count(&self) -> usize {
        self.cache.len()
    }

    /// Evict all cached glyphs and reset the allocator.
    ///
    /// The GPU texture is not cleared — old data is simply overwritten as new
    /// glyphs are allocated. This is safe because we always upload before
    /// sampling a region.
    pub fn clear(&mut self) {
        self.cache.clear();
        self.allocator.clear();
    }
}

// ---------------------------------------------------------------------------
// ColorGlyphAtlas — Rgba8UnormSrgb texture for full-color emoji glyphs
// ---------------------------------------------------------------------------

/// Full-color glyph atlas backed by a wgpu `Rgba8UnormSrgb` texture.
///
/// Stores `SwashContent::Color` glyphs (CBDT, COLR, sbix bitmaps) in their
/// native RGBA form. The associated shader pipeline samples all four channels
/// and does NOT multiply by the foreground tint color, so emoji render with
/// their original colors across all themes.
///
/// Memory: `COLOR_ATLAS_SIZE²` × 4 bytes = 1 MiB at the default 512×512 size.
pub struct ColorGlyphAtlas {
    texture: Texture,
    view: TextureView,
    allocator: AtlasAllocator,
    cache: HashMap<CacheKey, GlyphEntry>,
    bind_group: BindGroup,
    bind_group_layout: BindGroupLayout,
    width: u32,
    height: u32,
}

impl ColorGlyphAtlas {
    /// Create a new color glyph atlas with the given GPU device.
    #[must_use]
    pub fn new(device: &Device, _queue: &Queue) -> Self {
        let width = COLOR_ATLAS_SIZE;
        let height = COLOR_ATLAS_SIZE;

        let texture = device.create_texture(&TextureDescriptor {
            label: Some("color-glyph-atlas"),
            size: Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::Rgba8UnormSrgb,
            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
            view_formats: &[],
        });

        let view = texture.create_view(&Default::default());

        let sampler = device.create_sampler(&SamplerDescriptor {
            label: Some("color-glyph-atlas-sampler"),
            address_mode_u: AddressMode::ClampToEdge,
            address_mode_v: AddressMode::ClampToEdge,
            mag_filter: FilterMode::Linear,
            min_filter: FilterMode::Linear,
            ..Default::default()
        });

        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("color-glyph-atlas-layout"),
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

        let bind_group = device.create_bind_group(&BindGroupDescriptor {
            label: Some("color-glyph-atlas-bind-group"),
            layout: &bind_group_layout,
            entries: &[
                BindGroupEntry {
                    binding: 0,
                    resource: BindingResource::TextureView(&view),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: BindingResource::Sampler(&sampler),
                },
            ],
        });

        let allocator = AtlasAllocator::new(size2(width as i32, height as i32));

        Self {
            texture,
            view,
            allocator,
            cache: HashMap::new(),
            bind_group,
            bind_group_layout,
            width,
            height,
        }
    }

    /// Try to allocate a rectangular region in the color atlas.
    pub fn allocate(&mut self, width: u32, height: u32) -> Option<Allocation> {
        if width == 0 || height == 0 {
            return None;
        }
        self.allocator.allocate(size2(width as i32, height as i32))
    }

    /// Upload RGBA glyph bitmap data into a previously allocated region.
    ///
    /// `data` must be `rect_width * rect_height * 4` bytes of RGBA pixel data.
    pub fn upload(&self, queue: &Queue, x: u32, y: u32, width: u32, height: u32, data: &[u8]) {
        debug_assert_eq!(
            data.len(),
            (width * height * 4) as usize,
            "color upload size mismatch: expected {}x{}x4={}, got {}",
            width,
            height,
            width * height * 4,
            data.len(),
        );

        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &self.texture,
                mip_level: 0,
                origin: wgpu::Origin3d { x, y, z: 0 },
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: None,
            },
            Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
    }

    /// Look up a color glyph in the cache, or rasterize + allocate + upload it.
    ///
    /// Only `SwashContent::Color` images are stored; other content types return
    /// `None` so the caller falls through to the monochrome `GlyphAtlas`.
    ///
    /// Returns a `GlyphEntry` with `is_color = true` on success.
    pub fn get_or_insert(
        &mut self,
        queue: &Queue,
        font_system: &mut FontSystem,
        swash_cache: &mut SwashCache,
        cache_key: CacheKey,
    ) -> Option<GlyphEntry> {
        // Fast path: already cached.
        if let Some(&entry) = self.cache.get(&cache_key) {
            return Some(entry);
        }

        // Rasterize via swash.
        let image = swash_cache.get_image(font_system, cache_key).as_ref()?;

        // Only handle RGBA color bitmaps; fall through for everything else.
        if image.content != SwashContent::Color {
            return None;
        }

        let glyph_width = image.placement.width;
        let glyph_height = image.placement.height;

        if glyph_width == 0 || glyph_height == 0 {
            return None;
        }

        let rgba_data = image.data.clone();
        let placement_left = image.placement.left;
        let placement_top = image.placement.top;

        // Allocate space in the color atlas.
        let allocation = self.allocate(glyph_width, glyph_height)?;
        let rect = allocation.rectangle;

        let x = rect.min.x as u32;
        let y = rect.min.y as u32;

        // Upload the full RGBA bitmap.
        self.upload(queue, x, y, glyph_width, glyph_height, &rgba_data);

        // Compute normalized UV coordinates.
        let atlas_w = self.width as f32;
        let atlas_h = self.height as f32;
        let entry = GlyphEntry {
            x,
            y,
            width: glyph_width,
            height: glyph_height,
            uv_min: [x as f32 / atlas_w, y as f32 / atlas_h],
            uv_max: [
                (x + glyph_width) as f32 / atlas_w,
                (y + glyph_height) as f32 / atlas_h,
            ],
            left: placement_left,
            top: placement_top,
            is_color: true,
        };

        self.cache.insert(cache_key, entry);
        Some(entry)
    }

    /// The bind group layout for the color atlas texture + sampler.
    #[must_use]
    pub fn bind_group_layout(&self) -> &BindGroupLayout {
        &self.bind_group_layout
    }

    /// The bind group containing the color atlas texture view and sampler.
    #[must_use]
    pub fn bind_group(&self) -> &BindGroup {
        &self.bind_group
    }

    /// The texture view for the color atlas.
    #[must_use]
    pub fn texture_view(&self) -> &TextureView {
        &self.view
    }

    /// Color atlas dimensions in pixels.
    #[must_use]
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Number of cached color glyphs.
    #[must_use]
    pub fn glyph_count(&self) -> usize {
        self.cache.len()
    }

    /// Evict all cached color glyphs and reset the allocator.
    pub fn clear(&mut self) {
        self.cache.clear();
        self.allocator.clear();
    }
}
