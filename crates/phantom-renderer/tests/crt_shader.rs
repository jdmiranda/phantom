// crt_shader.rs — naga validation tests for shaders/crt.wgsl
//
// Validates that the CRT shader is syntactically correct WGSL and passes
// naga's IR-level validation without requiring a GPU device.  Runs in plain
// `cargo test -p phantom-renderer` — no display or graphics driver needed.
//
// naga is pulled in as a dev-dependency; the `wgsl-in` feature enables the
// WGSL frontend used here.

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
