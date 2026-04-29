// crt_shader.rs — naga validation tests for shaders/crt.wgsl
//
// Validates that the CRT shader is syntactically correct WGSL and passes
// naga's IR-level validation without requiring a GPU device.  Runs in plain
// `cargo test -p phantom-renderer` — no display or graphics driver needed.
//
// naga is pulled in as a dev-dependency; the `wgsl-in` feature enables the
// WGSL frontend used here.

use naga::ResourceBinding;
use phantom_renderer::postfx::CRT_WGSL;

// ---------------------------------------------------------------------------
// Naga parse + validation
// ---------------------------------------------------------------------------

/// Parse `shaders/crt.wgsl` with naga and run the full IR validator.
///
/// Catches:
///   - WGSL syntax errors
///   - Type mismatches and undeclared identifiers
///   - Missing entry points (`vs_main`, `fs_main`)
///   - Uniform struct layout violations
///   - Use of unsupported built-ins
///
/// No GPU device is required; naga validates entirely on the CPU.
#[test]
fn crt_wgsl_passes_naga_validation() {
    // Parse the embedded WGSL source into the naga IR module.
    let module = naga::front::wgsl::parse_str(CRT_WGSL)
        .expect("shaders/crt.wgsl must parse as valid WGSL");

    // Run the full naga IR validator with all validation flags enabled.
    let mut validator = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::empty(),
    );
    validator
        .validate(&module)
        .expect("shaders/crt.wgsl must pass naga IR validation");
}

/// Confirm both entry points declared in the shader are present in the module.
#[test]
fn crt_wgsl_has_vertex_and_fragment_entry_points() {
    let module = naga::front::wgsl::parse_str(CRT_WGSL)
        .expect("shaders/crt.wgsl must parse");

    let entry_names: Vec<&str> = module
        .entry_points
        .iter()
        .map(|ep| ep.name.as_str())
        .collect();

    assert!(
        entry_names.contains(&"vs_main"),
        "expected entry point 'vs_main', found: {:?}",
        entry_names
    );
    assert!(
        entry_names.contains(&"fs_main"),
        "expected entry point 'fs_main', found: {:?}",
        entry_names
    );
}

/// Confirm the embedded shader source is non-empty (guards against a broken
/// `include_str!` path — e.g., if `shaders/crt.wgsl` is accidentally deleted).
#[test]
fn crt_wgsl_is_not_empty() {
    assert!(
        !CRT_WGSL.is_empty(),
        "CRT_WGSL must not be empty — check shaders/crt.wgsl and the include_str! path"
    );
    assert!(
        CRT_WGSL.contains("vs_main"),
        "CRT_WGSL must contain 'vs_main'"
    );
    assert!(
        CRT_WGSL.contains("fs_main"),
        "CRT_WGSL must contain 'fs_main'"
    );
}

// ---------------------------------------------------------------------------
// Binding slot assertions — issue #131
//
// These tests parse the WGSL with naga and assert that every global variable's
// @group/@binding annotation matches what `PostFxPipeline::new` registers in
// its `BindGroupLayoutDescriptor`.  A mismatch compiles silently and only blows
// up at runtime with a cryptic GPU error; these tests catch it at `cargo test`.
//
// Authoritative layout (shaders/crt.wgsl lines 52-54):
//   @group(0) @binding(0)  var scene_texture : texture_2d<f32>
//   @group(0) @binding(1)  var scene_sampler : sampler
//   @group(0) @binding(2)  var<uniform> params : PostFxParams
//
// Rust BindGroupLayoutEntry descriptors in postfx.rs::PostFxPipeline::new:
//   binding 0 → BindingType::Texture { … }
//   binding 1 → BindingType::Sampler(SamplerBindingType::Filtering)
//   binding 2 → BindingType::Buffer { ty: BufferBindingType::Uniform, … }
// ---------------------------------------------------------------------------

/// Helper: parse CRT_WGSL and return a map from variable name → ResourceBinding.
///
/// Panics if parsing fails — the calling test immediately fails with a clear message.
fn crt_global_bindings() -> std::collections::HashMap<String, ResourceBinding> {
    let module = naga::front::wgsl::parse_str(CRT_WGSL)
        .expect("shaders/crt.wgsl must parse as valid WGSL");

    module
        .global_variables
        .iter()
        .filter_map(|(_, var)| {
            var.binding
                .as_ref()
                .map(|b| (var.name.clone().unwrap_or_default(), b.clone()))
        })
        .collect()
}

/// `scene_texture` must be at @group(0) @binding(0).
///
/// The Rust layout assigns `BindGroupLayoutEntry { binding: 0, ty:
/// BindingType::Texture { … } }` — any slot drift is caught here.
#[test]
fn crt_wgsl_scene_texture_at_group0_binding0() {
    let bindings = crt_global_bindings();

    let binding = bindings
        .get("scene_texture")
        .expect("global variable 'scene_texture' not found in shaders/crt.wgsl");

    assert_eq!(
        binding.group, 0,
        "scene_texture: expected @group(0), got @group({})",
        binding.group
    );
    assert_eq!(
        binding.binding, 0,
        "scene_texture: expected @binding(0), got @binding({})",
        binding.binding
    );
}

/// `scene_sampler` must be at @group(0) @binding(1).
///
/// The Rust layout assigns `BindGroupLayoutEntry { binding: 1, ty:
/// BindingType::Sampler(SamplerBindingType::Filtering) }`.
#[test]
fn crt_wgsl_scene_sampler_at_group0_binding1() {
    let bindings = crt_global_bindings();

    let binding = bindings
        .get("scene_sampler")
        .expect("global variable 'scene_sampler' not found in shaders/crt.wgsl");

    assert_eq!(
        binding.group, 0,
        "scene_sampler: expected @group(0), got @group({})",
        binding.group
    );
    assert_eq!(
        binding.binding, 1,
        "scene_sampler: expected @binding(1), got @binding({})",
        binding.binding
    );
}

/// `params` (PostFxParams uniform buffer) must be at @group(0) @binding(2).
///
/// The Rust layout assigns `BindGroupLayoutEntry { binding: 2, ty:
/// BindingType::Buffer { ty: BufferBindingType::Uniform, … } }`.
#[test]
fn crt_wgsl_params_uniform_at_group0_binding2() {
    let bindings = crt_global_bindings();

    let binding = bindings
        .get("params")
        .expect("global variable 'params' not found in shaders/crt.wgsl");

    assert_eq!(
        binding.group, 0,
        "params: expected @group(0), got @group({})",
        binding.group
    );
    assert_eq!(
        binding.binding, 2,
        "params: expected @binding(2), got @binding({})",
        binding.binding
    );
}

/// All bound globals must live in group 0 — the Rust pipeline only creates one
/// bind group layout at index 0.  A stray @group(1) or higher would mean the
/// Rust `PipelineLayoutDescriptor` is missing a bind group layout slot.
#[test]
fn crt_wgsl_all_bindings_in_group0() {
    let bindings = crt_global_bindings();

    assert!(
        !bindings.is_empty(),
        "Expected at least one bound global variable in shaders/crt.wgsl"
    );

    for (name, binding) in &bindings {
        assert_eq!(
            binding.group, 0,
            "global '{}' is at @group({}) — only @group(0) is declared in the Rust pipeline layout",
            name, binding.group
        );
    }
}

/// Exactly three globals are bound: scene_texture, scene_sampler, params.
///
/// An extra binding that someone adds to the WGSL but forgets to register in
/// the Rust `BindGroupLayoutDescriptor` would be silently ignored by the GPU
/// driver — catching it early here is cheap insurance.
#[test]
fn crt_wgsl_exactly_three_bindings() {
    let bindings = crt_global_bindings();

    assert_eq!(
        bindings.len(),
        3,
        "Expected exactly 3 bound globals (scene_texture, scene_sampler, params), \
         found {}: {:?}",
        bindings.len(),
        bindings.keys().collect::<Vec<_>>()
    );
}

/// Binding slots 0, 1, and 2 must all be occupied — no gaps that would leave
/// a Rust `BindGroupLayoutEntry` pointing at an empty WGSL slot.
#[test]
fn crt_wgsl_binding_slots_are_contiguous_0_1_2() {
    let bindings = crt_global_bindings();

    let mut slots: Vec<u32> = bindings.values().map(|b| b.binding).collect();
    slots.sort_unstable();

    assert_eq!(
        slots,
        vec![0, 1, 2],
        "Expected binding slots [0, 1, 2] in @group(0), got {:?}",
        slots
    );
}
