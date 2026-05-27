// Color-glyph atlas text rendering pipeline.
//
// Renders per-glyph textured quads sampled from an Rgba8UnormSrgb color atlas.
// Used exclusively for SwashContent::Color glyphs (CBDT/COLR/sbix bitmaps,
// i.e. full-color emoji).
//
// Unlike the monochrome text pipeline (text.wgsl), the fragment shader samples
// all four RGBA channels and does NOT multiply by the per-instance foreground
// color. This preserves the original glyph colors across all themes.
//
// CRT post-processing (scanlines, bloom, curvature) is applied on top of the
// composed frame, so color emoji still picks up the retro aesthetic.
//
// Bind groups
// -----------
//   group(0) binding(0) — screen-size uniform  (VERTEX stage)
//   group(1) binding(0) — atlas_texture         (FRAGMENT stage, Rgba8UnormSrgb)
//   group(1) binding(1) — atlas_sampler         (FRAGMENT stage)
//
// Per-instance vertex attributes (step mode: Instance)
// -------------------------------------------------------
//   location(0) position : vec2<f32>   — top-left pixel coordinate
//   location(1) uv_rect  : vec4<f32>   — [min_u, min_v, max_u, max_v]
//   location(2) color    : vec4<f32>   — unused (ignored); kept for layout
//                                        compatibility with the mono pipeline
//   location(3) size     : vec2<f32>   — glyph quad size in pixels [w, h]

// ---- Uniforms (group 0) ----
//
// Same struct layout as `shaders/text.wgsl`'s Uniforms so a single uniform
// buffer can serve both pipelines. The color pipeline only reads
// `screen_size` (the halo fields are mono-text concerns), but declares
// the full layout for binding compatibility.
struct Uniforms {
    screen_size: vec2<f32>,
    atlas_size: vec2<f32>,
    glow_alpha: f32,
    glow_radius_px: f32,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> uniforms: Uniforms;

// ---- Color atlas texture + sampler (group 1) ----
@group(1) @binding(0) var atlas_texture: texture_2d<f32>;
@group(1) @binding(1) var atlas_sampler: sampler;

// ---- Per-instance glyph data (same layout as mono pipeline) ----
struct GlyphInstance {
    @location(0) position: vec2<f32>,
    @location(1) uv_rect: vec4<f32>,    // [min_u, min_v, max_u, max_v]
    @location(2) color: vec4<f32>,      // ignored for color glyphs
    @location(3) size: vec2<f32>,
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
    instance: GlyphInstance,
) -> VertexOutput {
    let corner = UNIT_QUAD[vertex_idx];

    // Expand unit quad to glyph pixel dimensions at the instance position.
    let pixel_pos = instance.position + corner * instance.size;

    // Convert pixel coordinates to NDC (y flipped: (0,0) is top-left).
    let ndc = vec2<f32>(
        (pixel_pos.x / uniforms.screen_size.x) * 2.0 - 1.0,
        1.0 - (pixel_pos.y / uniforms.screen_size.y) * 2.0,
    );

    // Interpolate UV from the atlas sub-rectangle.
    let uv = mix(instance.uv_rect.xy, instance.uv_rect.zw, corner);

    var out: VertexOutput;
    out.clip_pos = vec4<f32>(ndc, 0.0, 1.0);
    out.uv = uv;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // Sample all four RGBA channels from the color atlas.
    // Do NOT tint by foreground color — the bitmap already contains the
    // correct emoji colors (CBDT/COLR/sbix). Applying the foreground
    // multiply is exactly the bug this shader variant is here to prevent.
    return textureSample(atlas_texture, atlas_sampler, in.uv);
}
