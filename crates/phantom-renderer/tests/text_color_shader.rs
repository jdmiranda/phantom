// Naga compile-time validation for shaders/text_color.wgsl (closes #356).
//
// Parses and validates the WGSL source for the color-glyph pipeline through
// naga's full validation pipeline without requiring a GPU device.
//
// Additionally checks that the fragment shader does NOT reference a `color`
// output multiply — i.e. that the foreground-tint operation is absent from the
// color pipeline (the root cause of #356 in text.wgsl).

/// Color glyph shader source — the same bytes wgpu sees at runtime.
const COLOR_SHADER_SRC: &str = include_str!("../../../shaders/text_color.wgsl");

#[test]
fn text_color_wgsl_parses_and_validates() {
    use naga::valid::{Capabilities, ValidationFlags, Validator};

    let module = naga::front::wgsl::parse_str(COLOR_SHADER_SRC)
        .expect("shaders/text_color.wgsl must parse without errors");

    let mut validator = Validator::new(ValidationFlags::all(), Capabilities::empty());
    validator
        .validate(&module)
        .expect("shaders/text_color.wgsl must pass naga validation");
}

#[test]
fn text_color_wgsl_has_vertex_and_fragment_entry_points() {
    use naga::ShaderStage;

    let module = naga::front::wgsl::parse_str(COLOR_SHADER_SRC)
        .expect("shaders/text_color.wgsl must parse");

    let stages: Vec<ShaderStage> = module
        .entry_points
        .iter()
        .map(|ep| ep.stage)
        .collect();

    assert!(
        stages.contains(&ShaderStage::Vertex),
        "shaders/text_color.wgsl must declare a @vertex entry point"
    );
    assert!(
        stages.contains(&ShaderStage::Fragment),
        "shaders/text_color.wgsl must declare a @fragment entry point"
    );
}

/// Regression guard for #356: the color-glyph fragment shader must NOT
/// multiply the sampled RGBA by a foreground `color` attribute.
///
/// The monochrome shader (`text.wgsl`) produces:
///   `vec4<f32>(in.color.rgb, in.color.a * atlas_alpha)`
/// which tints every glyph — including color emoji — by the theme foreground.
///
/// The color shader must sample all four channels and return them unchanged:
///   `textureSample(atlas_texture, atlas_sampler, in.uv)`
///
/// This test checks that the shader source contains the correct `textureSample`
/// call and does NOT contain `in.color` in the fragment function (which would
/// indicate the tint multiply is still present).
#[test]
fn text_color_wgsl_fragment_does_not_tint_by_foreground_color() {
    // The fragment function must sample the full RGBA atlas color directly.
    assert!(
        COLOR_SHADER_SRC.contains("textureSample(atlas_texture, atlas_sampler, in.uv)"),
        "color shader fs_main must return textureSample directly (no tint multiply)"
    );

    // The color shader's VertexOutput must NOT propagate `color` to the fragment
    // stage (no `@location(1) color` in VertexOutput for the color pipeline).
    // We verify the fragment function body does not reference `in.color`.
    // Locate the fs_main body by finding everything after `fn fs_main`.
    let fs_start = COLOR_SHADER_SRC
        .find("fn fs_main")
        .expect("color shader must have fn fs_main");
    let fs_body = &COLOR_SHADER_SRC[fs_start..];

    assert!(
        !fs_body.contains("in.color"),
        "color shader fs_main must NOT reference in.color — \
         applying foreground tint to color emoji is the bug fixed by #356"
    );
}
