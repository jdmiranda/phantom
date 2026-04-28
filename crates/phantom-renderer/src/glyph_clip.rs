// === GlyphClipRect parallel-buffer pipeline (Phase 0.D — DO NOT DROP) ===
//
// Sibling of `clip.rs`, deliberately isolated from `text.rs` for the same
// durability reason: the type kept getting dropped by concurrent rewrites
// of `text.rs`. If you are tempted to inline this back into `text.rs`:
// don't. The point of this file is durability.
//
// Re-exported from `crate::text` as `crate::text::GlyphClipRect`.
// The integration test at `tests/clip_rect.rs` imports it through that path.
//
// This is the glyph-pipeline mirror of `ClipRect`. CPU-only for now —
// the grid shader does not yet apply per-instance clipping. The type is
// in place so the upload path can be wired without further public-API
// churn when the WGSL change lands.

use wgpu::{VertexAttribute, VertexBufferLayout, VertexFormat, VertexStepMode};

/// Per-instance scissor rectangle for clipping a glyph against an
/// axis-aligned region in pixel coordinates (top-left origin, matching
/// `GlyphInstance::position`).
///
/// Same layout as `ClipRect`: 16 bytes, 4-byte aligned, no padding.
/// Sentinel: zero-init / `GlyphClipRect::NONE` disables clipping.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GlyphClipRect {
    /// `[x, y, w, h]` in pixels, top-left origin.
    pub xywh: [f32; 4],
}

impl GlyphClipRect {
    /// The "no clipping" sentinel — bit-equal to `GlyphClipRect::default()`.
    pub const NONE: GlyphClipRect = GlyphClipRect { xywh: [0.0; 4] };

    /// Construct a glyph clip rect from explicit pixel dimensions.
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { xywh: [x, y, w, h] }
    }

    /// Convenience constructor returning the "no clip" sentinel.
    pub const fn none() -> Self {
        Self::NONE
    }

    /// Returns `true` when this rect should disable clipping.
    ///
    /// Mirrors `ClipRect::is_none`: any non-positive width or height means
    /// "no clip", which matches the (future) fragment-shader test.
    pub fn is_none(&self) -> bool {
        self.xywh[2] <= 0.0 || self.xywh[3] <= 0.0
    }

    /// Vertex buffer layout describing the per-instance glyph-clip attribute.
    ///
    /// `shader_location = 4` is reserved across both quad and glyph
    /// pipelines for the clip-rect attribute. Stride matches
    /// `size_of::<GlyphClipRect>() == 16`.
    pub fn buffer_layout() -> VertexBufferLayout<'static> {
        const ATTRS: &[VertexAttribute] = &[VertexAttribute {
            format: VertexFormat::Float32x4,
            offset: 0,
            shader_location: 4,
        }];
        VertexBufferLayout {
            array_stride: std::mem::size_of::<GlyphClipRect>() as u64,
            step_mode: VertexStepMode::Instance,
            attributes: ATTRS,
        }
    }
}
