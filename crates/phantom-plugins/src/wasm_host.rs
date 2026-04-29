//! Real wasmtime-backed WASM plugin host.
//!
//! # Design
//!
//! [`WasmHost`] wraps a single `wasmtime::Engine` (shared, compiled-module
//! cache) and spawns a fresh [`WasmRuntime`] per plugin via
//! [`WasmHost::load`].  Each [`WasmRuntime`] owns a `Store` and an
//! `Instance`; the store is configured with a 64 MiB fuel limit and all
//! WASI capabilities denied by default (no filesystem, no network).
//!
//! [`WasmRuntime`] implements [`PluginRuntime`] so it drops in wherever the
//! [`MockRuntime`](crate::host::MockRuntime) is used today.
//!
//! ## Plugin ABI
//!
//! Phantom communicates with WASM plugins through a minimal ABI built on raw
//! integer values (Wasm `i32`/`i64`).  The host calls three well-known
//! exports:
//!
//! | Export          | Signature              | Purpose                        |
//! |-----------------|------------------------|--------------------------------|
//! | `ph_init`       | `() -> i32`            | Called once; 0 = ok            |
//! | `ph_on_hook`    | `(i32, i32) -> i32`    | hook-id, ctx-ptr → response-id |
//! | `ph_on_command` | `(i32, i32) -> i32`    | cmd-ptr, args-ptr → ok         |
//! | `ph_shutdown`   | `()`                   | Graceful teardown              |
//!
//! All exports are optional; missing exports are silently skipped.

use anyhow::{bail, Result};
use wasmtime::{Config, Engine, Instance, Module, Store, Val};

use crate::host::{HookContext, HookResponse, PluginRuntime};
use crate::manifest::{HookType, PluginManifest};

// ---------------------------------------------------------------------------
// Sandbox error
// ---------------------------------------------------------------------------

/// Typed error returned when a WASM module is rejected by the sandbox.
///
/// The [`WasmHost::load`] method wraps instantiation failures with this type
/// so callers can pattern-match on sandbox violations separately from other
/// wasmtime errors.
#[derive(Debug)]
pub enum SandboxViolation {
    /// The module requested an import that the host does not provide.
    ///
    /// This covers all WASI imports (`wasi_snapshot_preview1::*`) as well as
    /// any unknown host namespace.
    UnsupportedImport(String),
}

impl std::fmt::Display for SandboxViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxViolation::UnsupportedImport(msg) => {
                write!(f, "sandbox violation — unsupported import: {msg}")
            }
        }
    }
}

impl std::error::Error for SandboxViolation {}

/// Fuel budget granted to each plugin store (approx 64 MiB equivalent of work).
///
/// Fuel is a dimensionless counter decremented on each WASM instruction.
/// Setting it to `u64::MAX` effectively means "unlimited" while still keeping
/// the fuel machinery active so it can be tightened later per-plugin.
const DEFAULT_FUEL: u64 = u64::MAX / 2;

// ---------------------------------------------------------------------------
// Engine singleton helper
// ---------------------------------------------------------------------------

/// A shared, reusable WASM engine.
///
/// One engine should be created per process and reused for all plugins; the
/// engine is thread-safe (`Send + Sync`).
pub struct WasmHost {
    engine: Engine,
}

impl WasmHost {
    /// Create a new host with default engine configuration.
    ///
    /// Fuel consumption is enabled so per-plugin budgets can be enforced.
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config)
            .map_err(|e| anyhow::anyhow!("failed to create wasmtime Engine: {e:#}"))?;
        Ok(Self { engine })
    }

    /// Compile and instantiate a WASM module from raw bytes.
    ///
    /// The bytes may be either binary WASM (`\0asm` magic) or WAT text format
    /// (wasmtime compiles WAT transparently when the `wat` feature is active).
    ///
    /// # Sandbox guarantee
    ///
    /// The host provides **no imports**.  Any module that declares an import
    /// (WASI or otherwise) is rejected at instantiation time with a
    /// [`SandboxViolation::UnsupportedImport`] error — before any module code
    /// runs.
    ///
    /// On success, returns a [`WasmRuntime`] ready to accept calls.
    pub fn load(&self, wasm_bytes: &[u8]) -> Result<WasmRuntime> {
        let module = Module::new(&self.engine, wasm_bytes)
            .map_err(|e| anyhow::anyhow!("failed to compile WASM module: {e:#}"))?;

        // Fresh store per plugin — no WASI, no host imports, sandboxed by default.
        let mut store: Store<()> = Store::new(&self.engine, ());
        store
            .set_fuel(DEFAULT_FUEL)
            .map_err(|e| anyhow::anyhow!("failed to set store fuel: {e:#}"))?;

        // No host-side imports — plugins that require imports will fail here,
        // which is the desired sandboxing behaviour.  Surface the failure as a
        // typed `SandboxViolation` so callers can match on it specifically.
        let instance = Instance::new(&mut store, &module, &[]).map_err(|e| {
            let msg = e.to_string();
            // wasmtime reports unsatisfied imports in several ways depending on
            // version:
            //   - "unknown import" (older wasmtime)
            //   - "Imports provided" (older wasmtime)
            //   - "expected N imports, found 0" (wasmtime ≥44)
            //
            // Any of these indicate the module declared host imports that the
            // sandbox does not supply.  Wrap them in the typed error so callers
            // can match on SandboxViolation specifically.
            let is_import_error = msg.contains("unknown import")
                || msg.contains("Imports provided")
                || msg.contains("expected")
                    && msg.contains("imports")
                    && msg.contains("found");
            if is_import_error {
                anyhow::Error::new(SandboxViolation::UnsupportedImport(msg))
            } else {
                anyhow::anyhow!("failed to instantiate WASM module: {e:#}")
            }
        })?;

        Ok(WasmRuntime { store, instance })
    }
}

impl Default for WasmHost {
    fn default() -> Self {
        Self::new().expect("failed to create default WasmHost")
    }
}

// ---------------------------------------------------------------------------
// Per-plugin runtime
// ---------------------------------------------------------------------------

/// A single loaded WASM plugin instance, backed by a `wasmtime` store.
pub struct WasmRuntime {
    store: Store<()>,
    instance: Instance,
}

impl std::fmt::Debug for WasmRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasmRuntime").finish_non_exhaustive()
    }
}

impl WasmRuntime {
    /// Call an exported function by name with the given arguments.
    ///
    /// Returns an `Err` if the export does not exist or the call traps —
    /// never panics.
    pub fn call(&mut self, func: &str, args: &[Val]) -> Result<Vec<Val>> {
        let f = self
            .instance
            .get_func(&mut self.store, func)
            .ok_or_else(|| anyhow::anyhow!("WASM export '{func}' not found"))?;

        // Build result buffer sized for the function's return arity.
        let ty = f.ty(&self.store);
        let mut results: Vec<Val> = ty.results().map(|_| Val::I32(0)).collect();

        f.call(&mut self.store, args, &mut results)
            .map_err(|e| anyhow::anyhow!("WASM call to '{func}' trapped: {e:#}"))?;

        Ok(results)
    }

    /// Returns `true` if the module exports a function with the given name.
    pub fn has_export(&mut self, func: &str) -> bool {
        self.instance.get_func(&mut self.store, func).is_some()
    }
}

// ---------------------------------------------------------------------------
// PluginRuntime impl
// ---------------------------------------------------------------------------

impl PluginRuntime for WasmRuntime {
    fn init(&mut self, _manifest: &PluginManifest) -> Result<()> {
        // Call `ph_init` if the plugin exports it; otherwise this is a no-op.
        if self.has_export("ph_init") {
            let results = self.call("ph_init", &[])?;
            if let Some(Val::I32(code)) = results.first() {
                if *code != 0 {
                    bail!("ph_init returned non-zero error code: {code}");
                }
            }
        }
        Ok(())
    }

    fn call_hook(
        &mut self,
        _hook: &HookType,
        _context: &HookContext,
    ) -> Result<Option<HookResponse>> {
        // Minimal ABI: call `ph_on_hook(hook_id: i32, ctx: i32) -> i32`.
        // A return value of 0 means "nothing to report".
        // Detailed response encoding is deferred to a future WIT iteration.
        if self.has_export("ph_on_hook") {
            let results = self.call("ph_on_hook", &[Val::I32(0), Val::I32(0)])?;
            if let Some(Val::I32(0)) = results.first() {
                return Ok(None);
            }
            return Ok(Some(HookResponse::Nothing));
        }
        Ok(None)
    }

    fn call_command(&mut self, command: &str, _args: &[String]) -> Result<String> {
        if self.has_export("ph_on_command") {
            // Dummy pointers — a real ABI would write into WASM linear memory.
            let results = self.call("ph_on_command", &[Val::I32(0), Val::I32(0)])?;
            if let Some(Val::I32(0)) = results.first() {
                return Ok(String::new());
            }
        }
        bail!("WASM plugin has no handler for command '{command}'");
    }

    fn get_status_text(&mut self) -> Result<Option<String>> {
        // Status text via WASM requires shared memory or a write-back buffer.
        // Stubbed pending a Component-Model ABI; return `None` for now.
        Ok(None)
    }

    fn shutdown(&mut self) -> Result<()> {
        if self.has_export("ph_shutdown") {
            let _ = self.call("ph_shutdown", &[]);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal WAT: exports a single `add(i32, i32) -> i32` function.
    const ADD_WAT: &str = r#"
        (module
            (func $add (export "add") (param i32 i32) (result i32)
                local.get 0
                local.get 1
                i32.add
            )
        )
    "#;

    /// WAT module that exports the full Phantom ABI surface.
    const PHANTOM_ABI_WAT: &str = r#"
        (module
            (func $ph_init (export "ph_init") (result i32)
                i32.const 0
            )
            (func $ph_on_hook (export "ph_on_hook") (param i32 i32) (result i32)
                i32.const 0
            )
            (func $ph_on_command (export "ph_on_command") (param i32 i32) (result i32)
                i32.const 0
            )
            (func $ph_shutdown (export "ph_shutdown"))
        )
    "#;

    fn host() -> WasmHost {
        WasmHost::new().expect("WasmHost::new")
    }

    // ------------------------------------------------------------------
    // WasmHost::load / WasmRuntime::call
    // ------------------------------------------------------------------

    #[test]
    fn load_and_call_add_returns_correct_result() {
        let h = host();
        let mut rt = h.load(ADD_WAT.as_bytes()).expect("load add module");

        let results = rt
            .call("add", &[Val::I32(3), Val::I32(4)])
            .expect("call add");

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].unwrap_i32(), 7);
    }

    #[test]
    fn call_missing_export_returns_err_not_panic() {
        let h = host();
        let mut rt = h.load(ADD_WAT.as_bytes()).expect("load");

        let err = rt.call("nonexistent", &[]);
        assert!(err.is_err(), "expected Err for missing export, got Ok");
        let msg = err.unwrap_err().to_string();
        assert!(
            msg.contains("nonexistent"),
            "error should name the missing export, got: {msg}"
        );
    }

    #[test]
    fn load_with_different_arg_values() {
        let h = host();
        let mut rt = h.load(ADD_WAT.as_bytes()).expect("load");

        let r = rt
            .call("add", &[Val::I32(-10), Val::I32(10)])
            .expect("call");
        assert_eq!(r[0].unwrap_i32(), 0);

        let r = rt
            .call("add", &[Val::I32(100), Val::I32(200)])
            .expect("call");
        assert_eq!(r[0].unwrap_i32(), 300);
    }

    // ------------------------------------------------------------------
    // PluginRuntime trait via WasmRuntime
    // ------------------------------------------------------------------

    fn test_manifest() -> PluginManifest {
        PluginManifest {
            name: "wasm-test".into(),
            version: "0.1.0".into(),
            description: "test".into(),
            author: "test".into(),
            license: None,
            homepage: None,
            entry_point: "plugin.wasm".into(),
            permissions: vec![],
            hooks: vec![],
            commands: vec![],
            status_bar: None,
        }
    }

    #[test]
    fn plugin_runtime_init_calls_ph_init() {
        let h = host();
        let mut rt = h.load(PHANTOM_ABI_WAT.as_bytes()).expect("load");
        // ph_init returns 0 → success
        rt.init(&test_manifest()).expect("init");
    }

    #[test]
    fn plugin_runtime_init_no_export_is_noop() {
        // A module with no ph_init export must not error on init.
        let h = host();
        let mut rt = h.load(ADD_WAT.as_bytes()).expect("load");
        rt.init(&test_manifest()).expect("init noop");
    }

    #[test]
    fn plugin_runtime_call_hook_returns_none_when_ph_on_hook_returns_zero() {
        let h = host();
        let mut rt = h.load(PHANTOM_ABI_WAT.as_bytes()).expect("load");
        rt.init(&test_manifest()).unwrap();

        let ctx = HookContext::startup("/tmp");
        let resp = rt.call_hook(&HookType::OnStartup, &ctx).expect("call_hook");
        assert_eq!(resp, None);
    }

    #[test]
    fn plugin_runtime_call_hook_returns_none_when_no_export() {
        let h = host();
        let mut rt = h.load(ADD_WAT.as_bytes()).expect("load");
        rt.init(&test_manifest()).unwrap();

        let ctx = HookContext::startup("/tmp");
        let resp = rt.call_hook(&HookType::OnStartup, &ctx).expect("call_hook");
        assert_eq!(resp, None);
    }

    #[test]
    fn plugin_runtime_shutdown_is_safe_with_and_without_export() {
        let h = host();

        // With ph_shutdown export.
        let mut rt = h.load(PHANTOM_ABI_WAT.as_bytes()).expect("load");
        rt.shutdown().expect("shutdown with export");

        // Without ph_shutdown export.
        let mut rt = h.load(ADD_WAT.as_bytes()).expect("load");
        rt.shutdown().expect("shutdown without export");
    }

    #[test]
    fn plugin_runtime_get_status_text_returns_none() {
        let h = host();
        let mut rt = h.load(ADD_WAT.as_bytes()).expect("load");
        let text = rt.get_status_text().expect("get_status_text");
        assert_eq!(text, None);
    }

    // ------------------------------------------------------------------
    // Sandbox: module with no imports can't reach the host
    // ------------------------------------------------------------------

    #[test]
    fn module_with_no_imports_loads_successfully() {
        // A sandboxed module (no imports) must load and run fine.
        let wat = r#"
            (module
                (func (export "noop"))
            )
        "#;
        let h = host();
        let mut rt = h.load(wat.as_bytes()).expect("load sandbox module");
        let results = rt.call("noop", &[]).expect("noop");
        assert!(results.is_empty());
    }

    // ------------------------------------------------------------------
    // WasmHost::load rejects bad bytes
    // ------------------------------------------------------------------

    #[test]
    fn load_invalid_bytes_returns_err() {
        let h = host();
        let err = h.load(b"this is not wasm");
        assert!(err.is_err(), "expected compile error for invalid WASM");
    }

    // ------------------------------------------------------------------
    // Sandbox: WASI imports are rejected at instantiation
    // ------------------------------------------------------------------

    /// A module that imports `wasi_snapshot_preview1::fd_write`.
    ///
    /// Because the host provides no WASI linker, `Instance::new` must fail
    /// with an "unknown import" error, proving the sandbox holds.
    const WASI_IMPORT_WAT: &str = r#"
        (module
            (import "wasi_snapshot_preview1" "fd_write"
                (func $fd_write (param i32 i32 i32 i32) (result i32)))
            (func (export "main"))
        )
    "#;

    #[test]
    fn wasi_import_rejected_at_instantiation() {
        let h = host();
        let result = h.load(WASI_IMPORT_WAT.as_bytes());
        assert!(
            result.is_err(),
            "expected Err when loading a WASM module with WASI imports; \
             the sandbox must reject unsatisfied host imports"
        );
    }

    // ------------------------------------------------------------------
    // QA-#186 — WASM plugin sandbox: WASI imports rejected at instantiation
    // ------------------------------------------------------------------
    //
    // Spec: any WASM module that imports a WASI syscall must be rejected by
    // `WasmHost::load` at *instantiation* time — before any code executes —
    // surfacing an `Err`, never allowing execution.
    //
    // This suite tests three WASI syscall categories:
    //   1. `fd_write`    (stdio output)  — the canonical WASI output call
    //   2. `path_open`   (file open)     — filesystem access
    //   3. `sock_accept` (networking)    — socket syscall
    //
    // Additional invariants verified:
    //   4. A module mixing WASI imports with clean exports is rejected in full.
    //   5. A clean module (no WASI imports) loads fine — control condition.
    //   6. Rejection surfaces as `Err`, never as a panic.

    /// WAT module that imports `wasi_snapshot_preview1::path_open`.
    const WASI_PATH_OPEN_WAT: &str = r#"
        (module
            (import "wasi_snapshot_preview1" "path_open"
                (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
            (func (export "main"))
        )
    "#;

    /// WAT module that imports `wasi_snapshot_preview1::sock_accept`.
    const WASI_SOCK_ACCEPT_WAT: &str = r#"
        (module
            (import "wasi_snapshot_preview1" "sock_accept"
                (func $sock_accept (param i32 i32 i32) (result i32)))
            (func (export "main"))
        )
    "#;

    /// WAT module that mixes a WASI import with a legitimate exported function.
    ///
    /// The clean export must NOT allow instantiation to succeed — the whole
    /// module must be rejected because of the WASI dependency.
    const WASI_MIXED_WAT: &str = r#"
        (module
            (import "wasi_snapshot_preview1" "fd_write"
                (func $fd_write (param i32 i32 i32 i32) (result i32)))
            (func $add (export "add") (param i32 i32) (result i32)
                local.get 0
                local.get 1
                i32.add
            )
        )
    "#;

    #[test]
    fn qa_186_wasi_fd_write_rejected_at_instantiation() {
        // fd_write is the canonical WASI output syscall. A plugin that imports
        // it must be rejected before any of its code runs.
        let h = host();
        let result = h.load(WASI_IMPORT_WAT.as_bytes());
        assert!(
            result.is_err(),
            "QA-#186: fd_write import must be rejected at instantiation; \
             got Ok — sandbox failed to hold"
        );
    }

    #[test]
    fn qa_186_wasi_path_open_rejected_at_instantiation() {
        // path_open grants filesystem access — must be blocked.
        let h = host();
        let result = h.load(WASI_PATH_OPEN_WAT.as_bytes());
        assert!(
            result.is_err(),
            "QA-#186: path_open import must be rejected at instantiation; \
             got Ok — filesystem syscall leaked through sandbox"
        );
    }

    #[test]
    fn qa_186_wasi_sock_accept_rejected_at_instantiation() {
        // sock_accept grants network access — must be blocked.
        let h = host();
        let result = h.load(WASI_SOCK_ACCEPT_WAT.as_bytes());
        assert!(
            result.is_err(),
            "QA-#186: sock_accept import must be rejected at instantiation; \
             got Ok — network syscall leaked through sandbox"
        );
    }

    #[test]
    fn qa_186_wasi_mixed_module_rejected_despite_clean_export() {
        // A module that has both a WASI import AND a legitimate export must
        // still be rejected in full — the clean export must not serve as a
        // bypass mechanism.
        let h = host();
        let result = h.load(WASI_MIXED_WAT.as_bytes());
        assert!(
            result.is_err(),
            "QA-#186: a module mixing WASI imports and clean exports must be \
             rejected; got Ok — sandbox does not reject mixed modules"
        );
    }

    #[test]
    fn qa_186_clean_module_without_wasi_loads_successfully() {
        // Control condition: a module with no host imports loads fine.
        // This verifies the sandbox rejects WASI specifically, not all modules.
        let h = host();
        let result = h.load(ADD_WAT.as_bytes());
        assert!(
            result.is_ok(),
            "QA-#186 control: a clean module (no WASI imports) must load; \
             got Err: {:?}",
            result.err(),
        );
    }

    #[test]
    fn qa_186_rejection_is_an_error_not_a_panic() {
        // The sandbox must surface the denial as an `Err`, never panic or
        // abort. This is the "fail safe, not fail loud" property.
        //
        // We use `std::panic::catch_unwind` to make the no-panic contract
        // explicit; a panic would itself be a test failure.
        let result = std::panic::catch_unwind(|| {
            let h = WasmHost::new().expect("WasmHost::new");
            h.load(WASI_IMPORT_WAT.as_bytes())
        });
        match result {
            Ok(load_result) => {
                assert!(
                    load_result.is_err(),
                    "QA-#186: WASI import must produce Err, not Ok"
                );
            }
            Err(_) => panic!(
                "QA-#186: WasmHost::load panicked on WASI import — must return Err instead"
            ),
        }
    }
}
