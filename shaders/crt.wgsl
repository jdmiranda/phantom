// shaders/crt.wgsl — Phantom CRT post-processing shader
//
// Full-screen CRT effect pipeline: barrel distortion, chromatic aberration,
// phosphor bloom, scanlines, vignette, and animated film grain.
//
// The terminal scene is rendered to an offscreen texture, then this shader
// draws a full-screen triangle sampling that texture with every CRT effect
// composited in the fragment stage.
//
// All effect strengths are controlled by the PostFxParams uniform — no
// constants are hardcoded. Set any field to 0.0 to disable that effect.

// ---------------------------------------------------------------------------
// Uniforms
// ---------------------------------------------------------------------------
//
// Layout uses vec4 for glow_color (with time packed in .w) to avoid vec3
// alignment headaches between Rust repr(C) and WGSL std140 rules.
//
// Byte offsets:
//   0   scanline_intensity   f32
//   4   bloom_intensity      f32
//   8   chromatic_aberration f32
//  12   curvature            f32
//  16   vignette_intensity   f32
//  20   noise_intensity      f32
//  24   time                 f32
//  28   _pad0                f32
//  32   glow_color           vec3<f32>  (aligned to 16 bytes)
//  44   _pad1                f32
//  48   resolution           vec2<f32>
//  56   _pad2                vec2<f32>
//  64   total size
//
// MUST match PostFxParams in crates/phantom-renderer/src/postfx.rs exactly.
struct PostFxParams {
    scanline_intensity: f32,     //  0
    bloom_intensity: f32,        //  4
    chromatic_aberration: f32,   //  8
    curvature: f32,              // 12
    vignette_intensity: f32,     // 16
    noise_intensity: f32,        // 20
    time: f32,                   // 24
    _pad0: f32,                  // 28
    glow_color: vec3<f32>,       // 32 (vec3 aligns to 16)
    _pad1: f32,                  // 44
    resolution: vec2<f32>,       // 48
    _pad2: vec2<f32>,            // 56
                                 // total: 64
};

@group(0) @binding(0) var scene_texture: texture_2d<f32>;
@group(0) @binding(1) var scene_sampler: sampler;
@group(0) @binding(2) var<uniform> params: PostFxParams;

// ---------------------------------------------------------------------------
// Vertex shader: full-screen triangle from vertex index
// ---------------------------------------------------------------------------
//
// Emits 3 vertices that cover the entire clip space using the oversized
// triangle trick — the GPU clips to the viewport automatically:
//
//   Vertex 0: (-1, -1)  UV (0, 1)
//   Vertex 1: ( 3, -1)  UV (2, 1)
//   Vertex 2: (-1,  3)  UV (0, -1)
//
// No vertex buffer needed — positions are computed from @builtin(vertex_index).

struct VertexOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VertexOutput {
    var out: VertexOutput;
    let x = f32(i32(idx & 1u) * 4 - 1);
    let y = f32(i32(idx >> 1u) * 4 - 1);
    out.position = vec4<f32>(x, y, 0.0, 1.0);
    // UV: map clip space to 0..1, with y flipped so (0,0) is top-left.
    out.uv = vec2<f32>((x + 1.0) * 0.5, 1.0 - (y + 1.0) * 0.5);
    return out;
}

// ---------------------------------------------------------------------------
// Utility functions
// ---------------------------------------------------------------------------

// Hash-based pseudo-random noise. Returns a value in 0..1.
fn hash(p: vec2<f32>) -> f32 {
    var p3 = fract(vec3<f32>(p.xyx) * 0.1031);
    p3 = p3 + dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

// Barrel distortion: warp UVs outward from center to simulate CRT curvature.
fn barrel_distort(uv: vec2<f32>, amount: f32) -> vec2<f32> {
    let centered = uv - vec2<f32>(0.5, 0.5);
    let r2 = dot(centered, centered);
    let strength = amount * 2.0; // scale to a visually pleasing range
    let warped = centered * (1.0 + strength * r2 + strength * 0.5 * r2 * r2);
    return warped + vec2<f32>(0.5, 0.5);
}

// Returns true when the UV coordinate is within the 0..1 unit square.
fn in_bounds(uv: vec2<f32>) -> bool {
    return uv.x >= 0.0 && uv.x <= 1.0 && uv.y >= 0.0 && uv.y <= 1.0;
}

// Sample the scene texture, returning opaque black outside the 0..1 UV range.
// Used for barrel-distorted UVs that may warp outside the texture area.
fn sample_clamped(uv: vec2<f32>) -> vec4<f32> {
    if !in_bounds(uv) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }
    return textureSample(scene_texture, scene_sampler, uv);
}

// ---------------------------------------------------------------------------
// Fragment shader: all CRT effects composited
// ---------------------------------------------------------------------------

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    var uv = in.uv;

    // ------------------------------------------------------------------
    // 1. Barrel distortion (CRT screen curvature)
    //
    // Warps UVs outward from center so the image appears to curve away
    // at the edges, like a real CRT tube. Pixels outside the curved
    // screen boundary are drawn as pure black (the bezel).
    // ------------------------------------------------------------------
    if params.curvature > 0.0 {
        uv = barrel_distort(uv, params.curvature);
    }

    // Outside the curved screen area — pure black.
    if !in_bounds(uv) {
        return vec4<f32>(0.0, 0.0, 0.0, 1.0);
    }

    // ------------------------------------------------------------------
    // 2. Chromatic aberration
    //
    // Splits R/G/B channels slightly in the radial direction from center,
    // simulating the colour fringing of cheap CRT optics. The offset
    // scales with distance from center so corners show more fringing.
    // ------------------------------------------------------------------
    var color: vec3<f32>;

    if params.chromatic_aberration > 0.0 {
        let center = vec2<f32>(0.5, 0.5);
        let dist_from_center = uv - center;
        let offset = dist_from_center * params.chromatic_aberration * 0.02;

        let r = sample_clamped(uv + offset).r;
        let g = sample_clamped(uv).g;
        let b = sample_clamped(uv - offset).b;
        color = vec3<f32>(r, g, b);
    } else {
        color = textureSample(scene_texture, scene_sampler, uv).rgb;
    }

    // ------------------------------------------------------------------
    // 3. Phosphor bloom / glow
    //
    // 8-tap box-blur blended additively over the base colour, tinted by
    // the phosphor glow_color uniform. Larger bloom_intensity values
    // increase both the spread radius and the additive weight.
    // ------------------------------------------------------------------
    if params.bloom_intensity > 0.0 {
        let texel = vec2<f32>(1.0 / params.resolution.x, 1.0 / params.resolution.y);
        let spread = texel * (2.0 + params.bloom_intensity * 4.0);

        var bloom = vec3<f32>(0.0, 0.0, 0.0);
        bloom += sample_clamped(uv + vec2<f32>(-spread.x, -spread.y)).rgb;
        bloom += sample_clamped(uv + vec2<f32>( 0.0,      -spread.y)).rgb;
        bloom += sample_clamped(uv + vec2<f32>( spread.x, -spread.y)).rgb;
        bloom += sample_clamped(uv + vec2<f32>(-spread.x,  0.0     )).rgb;
        bloom += sample_clamped(uv + vec2<f32>( spread.x,  0.0     )).rgb;
        bloom += sample_clamped(uv + vec2<f32>(-spread.x,  spread.y)).rgb;
        bloom += sample_clamped(uv + vec2<f32>( 0.0,       spread.y)).rgb;
        bloom += sample_clamped(uv + vec2<f32>( spread.x,  spread.y)).rgb;
        bloom = bloom / 8.0;

        // Tint bloom by the phosphor glow color and blend additively.
        let glow = bloom * params.glow_color;
        color = color + glow * params.bloom_intensity * 0.7;
    }

    // ------------------------------------------------------------------
    // 4. Scanlines
    //
    // Sinusoidal darkening on alternating horizontal pixel rows, imitating
    // the shadow-mask of a CRT. A sine wave gives a smooth, less harsh
    // result than a binary step function.
    // ------------------------------------------------------------------
    if params.scanline_intensity > 0.0 {
        let pixel_y = uv.y * params.resolution.y;
        let scanline = 1.0 - params.scanline_intensity * 0.3
                       * (1.0 + sin(pixel_y * 3.14159265 * 2.0)) * 0.5;
        color = color * scanline;
    }

    // ------------------------------------------------------------------
    // 5. Vignette
    //
    // Smooth radial darkening from center toward corners, mimicking the
    // reduced brightness near the edges of a CRT tube. Uses smoothstep
    // so the transition is gradual and non-distracting.
    // ------------------------------------------------------------------
    if params.vignette_intensity > 0.0 {
        let centered = uv - vec2<f32>(0.5, 0.5);
        let dist = length(centered);
        // 0.7071 = sqrt(0.5): maximum UV distance from center to corner.
        let vig = smoothstep(0.2, 0.7071, dist * params.vignette_intensity);
        color = color * (1.0 - vig * 0.85);
    }

    // ------------------------------------------------------------------
    // 6. Film grain / noise
    //
    // High-frequency luminance noise that varies per frame via the time
    // uniform, giving an organic, analog quality. The noise UV is scaled
    // by resolution so grain size stays consistent across window sizes.
    // ------------------------------------------------------------------
    if params.noise_intensity > 0.0 {
        let noise_uv = uv * params.resolution
                     + vec2<f32>(params.time * 127.1, params.time * 311.7);
        let grain = hash(noise_uv) - 0.5; // -0.5 to +0.5
        color = color + vec3<f32>(grain * params.noise_intensity * 0.12);
    }

    return vec4<f32>(clamp(color, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
