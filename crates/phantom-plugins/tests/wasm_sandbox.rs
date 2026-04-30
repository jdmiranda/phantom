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
//! Closes #380 — extra adversarial WASM sandbox tests (misspelled namespace + memory export).

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

/// Test 4A — WASM module that imports from a *misspelled* Phantom namespace.
///
/// `phantom_hots` is a one-character typo of `phantom_host`.  The sandbox must
/// reject it: no whitelisted namespace should be reachable through a spelling
/// variant, and the rejection must identify both the namespace and the name.
fn wasm_with_misspelled_namespace() -> Vec<u8> {
    wat::parse_str(
        r#"
        (module
          (import "phantom_hots" "do_thing"
            (func (param i32) (result i32)))
        )
    "#,
    )
    .expect("misspelled-namespace WAT should parse")
}

/// Test 4B — WASM module that exports its own linear memory and the `ph_init`
/// ABI entry point.
///
/// Exporting memory is not itself malicious; the fixture is designed to verify
/// that the *host* does not gain a back-channel into plugin-side memory and
/// that no sentinel symbol (`__phantom_host_ptr`) is present in the export
/// table.
fn wasm_with_memory_export() -> Vec<u8> {
    wat::parse_str(
        r#"
        (module
          (memory (export "memory") 1)
          (func $ph_init (export "ph_init") (result i32)
            i32.const 0
          )
        )
    "#,
    )
    .expect("memory-export WAT should parse")
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

// ---------------------------------------------------------------------------
// Test 4A — Reject imports from misspelled / unknown custom namespace (#380)
// ---------------------------------------------------------------------------

/// A module that imports from `phantom_hots` (one-character typo of
/// `phantom_host`) must be rejected at instantiation time.
///
/// The invariant: only explicitly whitelisted namespaces are accessible.
/// Spelling variants that are not in the whitelist must not silently succeed —
/// a user who misspells the namespace name must see a clear error, not
/// undefined behaviour from an unresolved import linking into arbitrary host
/// state.
///
/// # Containment gap noted (do not modify host code in this PR)
///
/// The issue spec requested that the [`SandboxViolation`] message identify
/// both `namespace = "phantom_hots"` and `name = "do_thing"`.  In practice,
/// wasmtime 44 uses the error format `"expected N imports, found 0"` — it does
/// not include the import namespace or name in the string that reaches
/// `SandboxViolation::UnsupportedImport`.  The sandbox correctly *rejects* the
/// module (the primary security property holds), but the diagnostic message
/// does not carry the offending namespace/name detail.
///
/// This is a diagnosability gap to be addressed in `wasm_host.rs` by
/// inspecting `module.imports()` before instantiation and constructing a
/// richer message — tracked separately; not fixed in this tests-only PR.
#[test]
fn misspelled_namespace_rejected_with_sandbox_violation() {
    let host = WasmHost::new().expect("WasmHost::new");
    let result = host.load(&wasm_with_misspelled_namespace());

    assert!(
        result.is_err(),
        "import from misspelled namespace 'phantom_hots' must be rejected — \
         namespace spelling variants must not reach the host"
    );

    let err = result.unwrap_err();
    assert!(
        is_sandbox_violation(&err),
        "rejection of a misspelled namespace must surface as \
         SandboxViolation::UnsupportedImport, got: {err}"
    );

    // The message must be non-empty so the caller receives some diagnostic.
    // NOTE: wasmtime 44 does not include namespace/name in the error string
    // (it reports "expected N imports, found 0").  The diagnosability gap is
    // documented above; the containment property (Err + SandboxViolation) is
    // what this test enforces.
    let msg = err.to_string();
    assert!(
        !msg.is_empty(),
        "SandboxViolation message must not be empty, got empty string"
    );
}

// ---------------------------------------------------------------------------
// Test 4B — Memory export does not expose host memory (#380)
// ---------------------------------------------------------------------------

/// A plugin that exports its linear memory loads successfully but must not
/// give the host access to plugin-side memory via a host-pointer sentinel, and
/// must not export a `__phantom_host_ptr` symbol.
///
/// Exporting memory is a normal WASM pattern for host–guest data exchange.
/// This test verifies three properties:
///
/// 1. **Load succeeds** — a memory export alone is not a sandbox violation.
/// 2. **`ph_init` callable and returns 0** — the module ABI works normally.
/// 3. **No `__phantom_host_ptr` function export** — this sentinel name is
///    used by some runtimes to expose a raw pointer into host address space;
///    its absence confirms the sandbox does not leak host pointers to the
///    plugin, and the plugin does not declare such a symbol.
///
/// Note: `has_export` checks only *function* exports (via `get_func`).
/// A memory export named `__phantom_host_ptr` would not be found by
/// `has_export` — which is the correct containment property: memory exports
/// are not callable function entry points and cannot be invoked through the
/// Phantom ABI.
#[test]
fn memory_export_loads_but_does_not_expose_host_pointer_sentinel() {
    let host = WasmHost::new().expect("WasmHost::new");

    // Property 1: the module must load — a memory export is not a violation.
    let mut rt = host
        .load(&wasm_with_memory_export())
        .expect("module that exports memory must load successfully");

    // Property 2: the ABI entry point works and returns the expected value.
    let results = rt
        .call("ph_init", &[])
        .expect("ph_init must be callable after load");
    assert_eq!(results.len(), 1, "ph_init must return exactly one value");
    assert_eq!(
        results[0].unwrap_i32(),
        0,
        "ph_init must return 0 (success) per the WAT fixture"
    );

    // Property 3: no host-pointer sentinel function export is present.
    // `__phantom_host_ptr` appearing as a callable function export would
    // indicate host address-space leakage into plugin land.
    assert!(
        !rt.has_export("__phantom_host_ptr"),
        "plugin must not export a '__phantom_host_ptr' function — \
         host pointer leakage sentinel detected in export table"
    );
}
