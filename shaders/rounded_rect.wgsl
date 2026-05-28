// shaders/rounded_rect.wgsl — anti-aliased rounded rectangle (SDF).
//
// This shader is design documentation for the eventual dedicated rounded-rect
// pipeline. The current `phantom_renderer::primitives::PrimitivesBatch`
// implementation reuses the existing quad pipeline in `crates/phantom-renderer
// /src/quads.rs`, which already implements the same SDF inline. Keep the two
// in sync if the inlined version is ever extracted.
//
// The signed-distance function returns the distance from `p` to the boundary
// of a rounded rectangle centered at the origin, with half-extents
// `half_size` and corner radius `r`. A negative result is inside the shape;
// a positive result is outside. Anti-aliasing is a one-pixel smoothstep
// across zero.

struct Uniforms {
    screen_size: vec2<f32>,
};

@group(0) @binding(0) var<uniform> uniforms: Uniforms;

struct VertexOutput {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) local_uv: vec2<f32>,
    @location(2) size_px: vec2<f32>,
    @location(3) border_radius: f32,
};

fn sdf_rounded_rect(p: vec2<f32>, half_size: vec2<f32>, radius: f32) -> f32 {
    let r = min(radius, min(half_size.x, half_size.y));
    let q = abs(p) - half_size + vec2<f32>(r, r);
    return length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs_rounded_rect(in: VertexOutput) -> @location(0) vec4<f32> {
    if in.border_radius <= 0.0 {
        return in.color;
    }
    let local_pos = (in.local_uv - vec2<f32>(0.5, 0.5)) * in.size_px;
    let half_size = in.size_px * 0.5;
    let dist = sdf_rounded_rect(local_pos, half_size, in.border_radius);
    let alpha = 1.0 - smoothstep(-0.5, 0.5, dist);
    return vec4<f32>(in.color.rgb, in.color.a * alpha);
}
