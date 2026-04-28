// === ClipRect parallel-buffer pipeline (Phase 0.D — DO NOT DROP) ===
//
// This file is the canonical home of the `ClipRect` newtype, deliberately
// isolated from `quads.rs` so concurrent rewrites of the larger renderer
// module cannot accidentally drop the type. If you are tempted to inline
// this into `quads.rs`: don't. The point of this file is durability.
//
// Re-exported from `crate::quads` as `crate::quads::ClipRect`.
// The integration test at `tests/clip_rect.rs` imports it through that path.
//
// Contract (mirrored by `GlyphClipRect` in `glyph_clip.rs`):
//   * 16 bytes, 4-byte aligned, no padding (matches WGSL `vec4<f32>`).
//   * `Default` and `NONE` are both the zero sentinel.
//   * `is_none()` is true when width <= 0 OR height <= 0.
//   * `buffer_layout()` describes a per-instance attribute at shader_location 4.
//
// The shader-side `discard` test must stay aligned with `is_none()`:
//   if (clip.z > 0.0 && clip.w > 0.0) && (frag outside [clip.x..clip.x+clip.z,
//                                                       clip.y..clip.y+clip.w])
//   then discard;

use wgpu::{VertexAttribute, VertexBufferLayout, VertexFormat, VertexStepMode};

/// Per-instance scissor rectangle for clipping a quad against an axis-aligned
/// region in pixel coordinates (top-left origin, matching `QuadInstance::pos`).
///
/// `xywh = [x, y, w, h]` is uploaded as a `vec4<f32>` to the GPU at
/// `@location(4)` of the quad pipeline. The shader treats `w <= 0 || h <= 0`
/// as "no clipping" — `ClipRect::NONE` is exactly the zero-init sentinel.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ClipRect {
    /// `[x, y, w, h]` in pixels, top-left origin.
    pub xywh: [f32; 4],
}

impl ClipRect {
    /// The "no clipping" sentinel — bit-equal to `ClipRect::default()`.
    pub const NONE: ClipRect = ClipRect { xywh: [0.0; 4] };

    /// Construct a clip rect from explicit pixel dimensions.
    ///
    /// `(x, y)` is the top-left corner; `(w, h)` is the size. If either
    /// `w` or `h` is non-positive, this rect is the "no clip" sentinel.
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { xywh: [x, y, w, h] }
    }

    /// Convenience constructor returning the "no clip" sentinel.
    pub const fn none() -> Self {
        Self::NONE
    }

    /// Returns `true` when this rect should disable clipping.
    ///
    /// The contract: any non-positive width or height means "no clip".
    /// This matches the fragment shader test
    /// `clip.z > 0.0 && clip.w > 0.0`.
    pub fn is_none(&self) -> bool {
        self.xywh[2] <= 0.0 || self.xywh[3] <= 0.0
    }

    /// Vertex buffer layout describing the per-instance clip attribute.
    ///
    /// Uses `shader_location = 4`, reserved across both quad and glyph
    /// pipelines for the clip-rect attribute. The stride must equal
    /// `size_of::<ClipRect>() == 16`.
    pub fn buffer_layout() -> VertexBufferLayout<'static> {
        const ATTRS: &[VertexAttribute] = &[VertexAttribute {
            format: VertexFormat::Float32x4,
            offset: 0,
            shader_location: 4,
        }];
        VertexBufferLayout {
            array_stride: std::mem::size_of::<ClipRect>() as u64,
            step_mode: VertexStepMode::Instance,
            attributes: ATTRS,
        }
    }
}
