//! Adversarial WASM sandbox tests for `phantom-plugins`.
//!
//! These tests verify that the [`WasmHost`] sandbox holds under hostile
//! inputs: modules that declare WASI or unknown-namespace imports must be
//! rejected *before* any code executes, the registry must remain usable after
//! a rejection, and a conforming module (no imports) must load successfully.
//!
//! All WASM modules are generated inline from WAT source using the `wat` crate
//! so the test file is self-contained — no pre-compiled `.wasm` fixtures.
//!
//! # Issue reference
//! Closes #186 — WASM plugin sandbox: adversarial tests.

use phantom_plugins::{SandboxViolation, WasmHost};

// ---------------------------------------------------------------------------
// WAT helpers
// ---------------------------------------------------------------------------

/// WASM module that imports `wasi_snapshot_preview1::fd_write` (stdio output).
fn wasm_with_fd_write() -> Vec<u8> {
    wat::parse_str(
        r#"
        (module
          (import "wasi_snapshot_preview1" "fd_write"
            (func (param i32 i32 i32 i32) (result i32)))
        )
    "#,
    )
    .expect("fd_write WAT should parse")
}

/// WASM module that imports `wasi_snapshot_preview1::sock_open` (socket).
fn wasm_with_sock_open() -> Vec<u8> {
    wat::parse_str(
        r#"
        (module
          (import "wasi_snapshot_preview1" "sock_open"
            (func (param i32 i32 i32) (result i32)))
        )
    "#,
    )
    .expect("sock_open WAT should parse")
}

/// Conforming WASM module with no host imports.
fn wasm_valid_no_wasi() -> Vec<u8> {
    wat::parse_str(
        r#"
        (module
          (func (export "init") (result i32) i32.const 42)
        )
    "#,
    )
    .expect("valid WAT should parse")
}

/// WASM module that imports from an unknown/internal host namespace.
fn wasm_with_custom_namespace() -> Vec<u8> {
    wat::parse_str(
        r#"
        (module
          (import "phantom_host_internal" "secret_fn"
            (func (param i32) (result i32)))
        )
    "#,
    )
    .expect("custom-namespace WAT should parse")
}

// ---------------------------------------------------------------------------
// Helper: check that an anyhow error is a SandboxViolation::UnsupportedImport
// ---------------------------------------------------------------------------

fn is_sandbox_violation(err: &anyhow::Error) -> bool {
    err.downcast_ref::<SandboxViolation>().is_some()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Loading a WASM module that imports `fd_write` must be rejected.
///
/// `fd_write` is the canonical WASI output syscall.  No plugin should be
/// allowed to write to file descriptors directly — all I/O goes through the
/// Phantom host ABI.
#[test]
fn wasi_fd_write_rejected() {
    let host = WasmHost::new().expect("WasmHost::new");
    let result = host.load(&wasm_with_fd_write());

    assert!(
        result.is_err(),
        "fd_write import must be rejected — sandbox failed to hold"
    );
    let err = result.unwrap_err();
    assert!(
        is_sandbox_violation(&err),
        "rejection must surface as SandboxViolation::UnsupportedImport, got: {err}"
    );
}

/// Loading a WASM module that imports `sock_open` must be rejected.
///
/// Network access is not available to plugins.  `sock_open` is a WASI socket
/// syscall that would allow unrestricted outbound connections.
#[test]
fn wasi_sock_open_rejected() {
    let host = WasmHost::new().expect("WasmHost::new");
    let result = host.load(&wasm_with_sock_open());

    assert!(
        result.is_err(),
        "sock_open import must be rejected — network access must be blocked"
    );
    let err = result.unwrap_err();
    assert!(
        is_sandbox_violation(&err),
        "rejection must surface as SandboxViolation::UnsupportedImport, got: {err}"
    );
}

/// After a bad module is rejected the registry remains in a consistent state.
///
/// This verifies that a single load failure does not corrupt the host engine
/// or leave it in an unusable state — subsequent loads of valid modules must
/// succeed.
#[test]
fn registry_stable_after_rejection() {
    let host = WasmHost::new().expect("WasmHost::new");

    // First load: a hostile module — must fail.
    let bad = host.load(&wasm_with_fd_write());
    assert!(bad.is_err(), "hostile module must be rejected");

    // Second load: a conforming module — must succeed.
    let good = host.load(&wasm_valid_no_wasi());
    if let Err(ref e) = good {
        panic!("host must remain usable after a rejection; got: {e}");
    }
    assert!(good.is_ok());
}

/// A conforming WASM module (no host imports, exports `init`) must load and
/// the `init` export must be callable.
///
/// This is the control condition: we verify that the sandbox rejects WASI
/// specifically, not all modules.
#[test]
fn valid_plugin_loads_correctly() {
    let host = WasmHost::new().expect("WasmHost::new");
    let mut rt = host
        .load(&wasm_valid_no_wasi())
        .expect("valid module (no WASI imports) must load");

    // The module exports `init() -> i32`; it returns 42.
    let results = rt.call("init", &[]).expect("init export must be callable");

    assert_eq!(results.len(), 1, "init must return one value");
    assert_eq!(
        results[0].unwrap_i32(),
        42,
        "init must return 42 per the WAT source"
    );
}

/// Loading a module that imports from an unknown namespace must be rejected.
///
/// The Phantom host only exposes the documented plugin ABI surface.  A module
/// importing from `phantom_host_internal` or any other undocumented namespace
/// has no legitimate use and must be denied.
#[test]
fn custom_namespace_rejected() {
    let host = WasmHost::new().expect("WasmHost::new");
    let result = host.load(&wasm_with_custom_namespace());

    assert!(
        result.is_err(),
        "import from unknown namespace 'phantom_host_internal' must be rejected"
    );
    // The error is either a SandboxViolation or a generic instantiation error —
    // both are acceptable as long as the load fails.
    let err = result.unwrap_err();
    assert!(
        !err.to_string().is_empty(),
        "rejection must include a diagnostic message"
    );
}
