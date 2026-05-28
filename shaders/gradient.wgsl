// shaders/gradient.wgsl — vertical two-stop linear gradient.
//
// Design documentation for the eventual dedicated gradient pipeline. The
// current `phantom_renderer::primitives::PrimitivesBatch::draw_gradient_rect`
// emits a stack of solid stripes via the existing quad pipeline; this WGSL
// is the single-pass version we will land once the dedicated pipeline is
// wired up.
//
// The fragment interpolates between `top_color` and `bottom_color` based on
// the local UV's y-coordinate, giving a continuous vertical gradient with
// no banding.

struct GradientParams {
    top_color: vec4<f32>,
    bottom_color: vec4<f32>,
};

@group(0) @binding(1) var<uniform> gradient: GradientParams;

struct VertexOutput {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) local_uv: vec2<f32>,
};

@fragment
fn fs_gradient(in: VertexOutput) -> @location(0) vec4<f32> {
    let t = clamp(in.local_uv.y, 0.0, 1.0);
    return mix(gradient.top_color, gradient.bottom_color, t);
}
