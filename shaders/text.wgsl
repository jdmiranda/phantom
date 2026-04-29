// Glyph atlas text rendering pipeline.
//
// Renders per-glyph textured quads sampled from an R8Unorm glyph atlas.
// Each visible glyph is drawn as an instanced quad: the vertex shader
// expands a unit-quad to the glyph's pixel dimensions and maps atlas UVs;
// the fragment shader applies the atlas alpha mask to the per-glyph tint.
//
// Bind groups
// -----------
//   group(0) binding(0) — screen-size uniform  (VERTEX stage)
//   group(1) binding(0) — atlas_texture         (FRAGMENT stage)
//   group(1) binding(1) — atlas_sampler         (FRAGMENT stage)
//
// Per-instance vertex attributes (step mode: Instance)
// -------------------------------------------------------
//   location(0) position : vec2<f32>   — top-left pixel coordinate
//   location(1) uv_rect  : vec4<f32>   — [min_u, min_v, max_u, max_v]
//   location(2) color    : vec4<f32>   — per-glyph RGBA tint (linear)
//   location(3) size     : vec2<f32>   — glyph quad size in pixels [w, h]

// ---- Uniforms (group 0) ----
struct Uniforms {
    screen_size: vec2<f32>,
};

@group(0) @binding(0) var<uniform> uniforms: Uniforms;

// ---- Atlas texture + sampler (group 1) ----
@group(1) @binding(0) var atlas_texture: texture_2d<f32>;
@group(1) @binding(1) var atlas_sampler: sampler;

// ---- Per-instance glyph data ----
struct GlyphInstance {
    @location(0) position: vec2<f32>,
    @location(1) uv_rect: vec4<f32>,    // [min_u, min_v, max_u, max_v]
    @location(2) color: vec4<f32>,
    @location(3) size: vec2<f32>,
};

struct VertexOutput {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
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
    instance: GlyphInstance,
) -> VertexOutput {
    let corner = UNIT_QUAD[vertex_idx];

    // Expand unit quad to glyph pixel dimensions at the instance position.
    let pixel_pos = instance.position + corner * instance.size;

    // Convert pixel coordinates to NDC.
    // NDC x: -1 (left) to +1 (right)
    // NDC y: +1 (top)  to -1 (bottom) — y flipped so (0,0) is top-left.
    let ndc = vec2<f32>(
        (pixel_pos.x / uniforms.screen_size.x) * 2.0 - 1.0,
        1.0 - (pixel_pos.y / uniforms.screen_size.y) * 2.0,
    );

    // Interpolate UV from the atlas sub-rectangle.
    let uv = mix(instance.uv_rect.xy, instance.uv_rect.zw, corner);

    var out: VertexOutput;
    out.clip_pos = vec4<f32>(ndc, 0.0, 1.0);
    out.uv = uv;
    out.color = instance.color;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // The atlas is R8Unorm — the red channel holds the alpha mask.
    let atlas_alpha = textureSample(atlas_texture, atlas_sampler, in.uv).r;

    // Multiply instance color by the atlas alpha for anti-aliased text.
    return vec4<f32>(in.color.rgb, in.color.a * atlas_alpha);
}
