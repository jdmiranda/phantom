// Glyph atlas text rendering pipeline with built-in halo shader.
//
// Renders per-glyph textured quads sampled from an R8Unorm glyph atlas.
// Each visible glyph is drawn as an instanced quad: the vertex shader
// expands a unit-quad to the glyph's pixel dimensions and maps atlas UVs;
// the fragment shader applies the atlas alpha mask to the per-glyph tint.
//
// In addition to the sharp atlas sample, the fragment shader samples eight
// neighbor texels with a normalised 3x3 gaussian kernel and adds the
// weighted neighbours scaled by `glow_alpha` to produce a soft phosphor
// halo around each glyph.  This replaces the CPU-side
// `append_glow_halos_stacked` approximation (which stacked extra scaled
// quads to fake a halo) with a single-pass shader effect.
//
// The neighbour sampling radius is expressed in pixels (atlas pixels) and
// converted to UV-space offsets using the `atlas_size` uniform.  The
// kernel weights match a sigma ≈ 1.0 gaussian; with `glow_radius_px = 1`
// we get a tight rim, `= 2` widens the halo, etc.
//
// Bind groups
// -----------
//   group(0) binding(0) — screen-size + glow-params uniform (VERTEX+FRAGMENT)
//   group(1) binding(0) — atlas_texture                     (FRAGMENT)
//   group(1) binding(1) — atlas_sampler                     (FRAGMENT)
//
// Per-instance vertex attributes (step mode: Instance)
// -------------------------------------------------------
//   location(0) position : vec2<f32>   — top-left pixel coordinate
//   location(1) uv_rect  : vec4<f32>   — [min_u, min_v, max_u, max_v]
//   location(2) color    : vec4<f32>   — per-glyph RGBA tint (linear)
//   location(3) size     : vec2<f32>   — glyph quad size in pixels [w, h]

// ---- Uniforms (group 0) ----
//
// Padded to 16-byte alignment per WebGPU uniform-buffer rules.  Fields:
//   screen_size      — viewport size in pixels (for vertex NDC math).
//   atlas_size       — atlas texture size in pixels (for UV→texel
//                      conversion in the halo neighbour loop).
//   glow_alpha       — peak alpha multiplier for the halo neighbours.  0.0
//                      disables the halo path entirely.  Tuned 0.3–0.6 in
//                      practice (bright phosphor themes).
//   glow_radius_px   — neighbour sample radius in atlas pixels.  1.0 = a
//                      compact rim, 2.0 = wider halo; the kernel
//                      normalises so larger values do not over-bomb.
struct Uniforms {
    screen_size: vec2<f32>,
    atlas_size: vec2<f32>,
    glow_alpha: f32,
    glow_radius_px: f32,
    _pad: vec2<f32>,
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

// 3x3 gaussian kernel weights (sigma ~ 1.0, normalised so center + 8
// neighbours sum to 1.0).  The center weight is folded into the sharp
// sample; the eight outer weights drive the halo neighbour sum.
//
//   1 2 1
//   2 4 2 / 16
//   1 2 1
//
// Outer weights sum: 4*1 + 4*2 = 12, so a halo contribution of 1.0 at
// max-weighted offsets requires alpha-multiplying the sum by 1/12.
//
// We renormalise differently — the outer weights here are scaled so the
// outer sum is 1.0, producing a halo that adds at most `glow_alpha` to
// the final alpha at the centre of a fully-bright glyph.
const KERNEL_CORNER: f32 = 1.0 / 12.0;  // 4 corners
const KERNEL_EDGE: f32 = 2.0 / 12.0;    // 4 edges

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    // The atlas is R8Unorm — the red channel holds the alpha mask.
    let sharp_alpha = textureSample(atlas_texture, atlas_sampler, in.uv).r;

    // --- Halo neighbour sum ---
    //
    // When `glow_alpha` is positive, sample 8 neighbours offset by
    // `glow_radius_px` atlas pixels and accumulate a gaussian-weighted
    // alpha.  Sampling in UV space requires converting pixel offsets to
    // UV deltas via `atlas_size`.  The sampler uses ClampToEdge so reads
    // outside the atlas return the edge texel; combined with the
    // sparse-atlas layout this keeps the halo from leaking into
    // neighbouring glyphs (the surrounding atlas regions are usually
    // zero-alpha empty space).
    var halo: f32 = 0.0;
    if uniforms.glow_alpha > 0.0001 {
        let off = uniforms.glow_radius_px / uniforms.atlas_size;
        let dx = off.x;
        let dy = off.y;

        // 4 edges (weight 2/12 each).
        halo += textureSample(atlas_texture, atlas_sampler, in.uv + vec2<f32>( dx,  0.0)).r * KERNEL_EDGE;
        halo += textureSample(atlas_texture, atlas_sampler, in.uv + vec2<f32>(-dx,  0.0)).r * KERNEL_EDGE;
        halo += textureSample(atlas_texture, atlas_sampler, in.uv + vec2<f32>(0.0,  dy)).r * KERNEL_EDGE;
        halo += textureSample(atlas_texture, atlas_sampler, in.uv + vec2<f32>(0.0, -dy)).r * KERNEL_EDGE;

        // 4 corners (weight 1/12 each).
        halo += textureSample(atlas_texture, atlas_sampler, in.uv + vec2<f32>( dx,  dy)).r * KERNEL_CORNER;
        halo += textureSample(atlas_texture, atlas_sampler, in.uv + vec2<f32>( dx, -dy)).r * KERNEL_CORNER;
        halo += textureSample(atlas_texture, atlas_sampler, in.uv + vec2<f32>(-dx,  dy)).r * KERNEL_CORNER;
        halo += textureSample(atlas_texture, atlas_sampler, in.uv + vec2<f32>(-dx, -dy)).r * KERNEL_CORNER;

        halo *= uniforms.glow_alpha;
    }

    // The final alpha is the sharp glyph alpha (preserves anti-aliasing)
    // plus the halo contribution, clamped to 1.0.  The same per-instance
    // colour tints both — so the halo is naturally the "current text
    // colour" matching `text-shadow: 0 0 8px currentColor` in the mockup.
    let final_alpha = clamp(sharp_alpha + halo, 0.0, 1.0);
    return vec4<f32>(in.color.rgb, in.color.a * final_alpha);
}
