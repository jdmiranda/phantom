// Naga compile-time validation for shaders/text.wgsl.
//
// Parses and validates the WGSL source through naga's full validation pipeline
// (type-checking, resource binding resolution, entry-point analysis) without
// requiring a GPU device. If this test fails the shader will also fail to
// compile at runtime via `wgpu::Device::create_shader_module`.

/// The shader source embedded at test compile time — the same bytes wgpu sees.
const SHADER_SRC: &str = include_str!("../../../shaders/text.wgsl");

#[test]
fn text_wgsl_parses_and_validates() {
    use naga::valid::{Capabilities, ValidationFlags, Validator};

    // Parse WGSL → naga IR.
    let module = naga::front::wgsl::parse_str(SHADER_SRC)
        .expect("shaders/text.wgsl must parse without errors");

    // Validate the IR (type checks, binding resolution, entry-point analysis).
    let mut validator = Validator::new(ValidationFlags::all(), Capabilities::empty());
    validator
        .validate(&module)
        .expect("shaders/text.wgsl must pass naga validation");
}

#[test]
fn text_wgsl_has_vertex_and_fragment_entry_points() {
    use naga::ShaderStage;

    let module = naga::front::wgsl::parse_str(SHADER_SRC)
        .expect("shaders/text.wgsl must parse");

    let stages: Vec<ShaderStage> = module
        .entry_points
        .iter()
        .map(|ep| ep.stage)
        .collect();

    assert!(
        stages.contains(&ShaderStage::Vertex),
        "shaders/text.wgsl must declare a @vertex entry point"
    );
    assert!(
        stages.contains(&ShaderStage::Fragment),
        "shaders/text.wgsl must declare a @fragment entry point"
    );
}
