// shaders/glow.wgsl — soft halo (gaussian-falloff) around a quad.
//
// Design documentation for the eventual dedicated glow pipeline. The current
// `phantom_renderer::primitives::PrimitivesBatch::draw_glow` reuses the
// existing quad pipeline by stacking concentric rounded-rect layers at
// decreasing alpha; this WGSL is the cleaner single-pass version we will
// land once a second pipeline is justified.
//
// The fragment computes the SDF distance from the inner rect boundary and
// maps it through an exponential-falloff curve. Inside the inner rect the
// contribution is the full color; outside, alpha falls off smoothly over
// `glow_radius` pixels.

struct GlowParams {
    inner_half_size: vec2<f32>,
    glow_radius: f32,
    inner_radius: f32,
};

@group(0) @binding(1) var<uniform> glow: GlowParams;

struct VertexOutput {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) color: vec4<f32>,
    @location(1) local_uv: vec2<f32>,
    @location(2) size_px: vec2<f32>,
};

fn sdf_rounded_rect(p: vec2<f32>, half_size: vec2<f32>, radius: f32) -> f32 {
    let r = min(radius, min(half_size.x, half_size.y));
    let q = abs(p) - half_size + vec2<f32>(r, r);
    return length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs_glow(in: VertexOutput) -> @location(0) vec4<f32> {
    let local_pos = (in.local_uv - vec2<f32>(0.5, 0.5)) * in.size_px;
    let dist = sdf_rounded_rect(local_pos, glow.inner_half_size, glow.inner_radius);

    // Negative distance is inside the inner rect; treat it as full intensity.
    // Positive distance falls off exponentially over `glow_radius` pixels.
    let d = max(dist, 0.0);
    let t = d / max(glow.glow_radius, 0.0001);
    let falloff = exp(-3.0 * t * t);
    return vec4<f32>(in.color.rgb, in.color.a * falloff);
}
