//! Dylib loader for `phantom-nlp` — resolves `phantom_llm_register` and wraps
//! the LLM vtable.
//!
//! Active only when `hot-modules` feature + debug build.

#![cfg(all(debug_assertions, feature = "hot-modules"))]

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context};

use crate::llm_ffi::{LlmSkillVtable, REGISTER_SYMBOL};
use crate::llm_host::{LlmSkill, dylib_impl::DylibBackedLlm};

/// Platform-specific dylib filename for `phantom-nlp`.
#[cfg(target_os = "macos")]
pub const DYLIB_NAME: &str = "libphantom_nlp.dylib";
#[cfg(target_os = "linux")]
pub const DYLIB_NAME: &str = "libphantom_nlp.so";
#[cfg(target_os = "windows")]
pub const DYLIB_NAME: &str = "phantom_nlp.dll";
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub const DYLIB_NAME: &str = "libphantom_nlp.so";

/// Type of the `phantom_llm_register` symbol exported by the `phantom-nlp` dylib.
///
/// # Safety
///
/// The returned pointer must point to a valid `LlmSkillVtable` with all
/// function pointers non-null.  The vtable must remain valid for the lifetime
/// of the library.
type RegisterFn = unsafe extern "C" fn() -> *const LlmSkillVtable;

/// Locate and load the `phantom-nlp` dylib, resolve its vtable, and return a
/// `DylibBackedLlm` skill.
///
/// Search order:
/// 1. `PHANTOM_HOT_MODULES_DIR` environment variable (if set).
/// 2. `<workspace_root>/target/debug/` (walking up from cwd to find
///    `Cargo.lock`, then appending `target/debug`).
/// 3. `./target/debug/` relative to the process working directory.
pub fn load_llm_dylib() -> anyhow::Result<Arc<dyn LlmSkill>> {
    let path = resolve_dylib_path()?;
    load_from_path(&path)
}

/// Load from an explicit path.  Used by tests and the watcher.
pub fn load_from_path(path: &std::path::Path) -> anyhow::Result<Arc<dyn LlmSkill>> {
    log::info!("skill-host: loading phantom-nlp LLM dylib from {:?}", path);

    // SAFETY: We validate the symbol immediately after loading.
    let lib = unsafe {
        libloading::Library::new(path)
            .with_context(|| format!("failed to open phantom-nlp dylib {:?}", path))?
    };

    // Validate: symbol must exist.
    // SAFETY: We immediately call through this pointer with correct arguments;
    // the library is kept alive in the Arc we build below.
    let register: libloading::Symbol<RegisterFn> = unsafe {
        lib.get(REGISTER_SYMBOL).with_context(|| {
            format!("symbol `phantom_llm_register` not found in {:?}", path)
        })?
    };

    // Call register to get the vtable pointer.
    // SAFETY: `register` is a valid `RegisterFn` from the dylib.
    let vtable_ptr: *const LlmSkillVtable = unsafe { register() };

    if vtable_ptr.is_null() {
        bail!("`phantom_llm_register` returned null vtable pointer");
    }

    // Validate all function pointers are non-null.
    // SAFETY: vtable_ptr is non-null; it points into the dylib's read-only
    // data which outlives the `Arc<Library>` below.
    unsafe {
        let vt = &*vtable_ptr;
        if vt.name as usize == 0 {
            bail!("LlmSkillVtable.name fn ptr is null in {:?}", path);
        }
        if vt.complete as usize == 0 {
            bail!("LlmSkillVtable.complete fn ptr is null in {:?}", path);
        }
        if vt.free_buf as usize == 0 {
            bail!("LlmSkillVtable.free_buf fn ptr is null in {:?}", path);
        }
    }

    let lib_arc = Arc::new(lib);

    // SAFETY: vtable_ptr is non-null, all fields validated, `lib_arc` will
    // keep the library alive for the lifetime of `DylibBackedLlm`.
    let backed = unsafe { DylibBackedLlm::new(vtable_ptr, lib_arc) };

    log::info!("skill-host: phantom-nlp LLM dylib loaded successfully");
    Ok(Arc::new(backed))
}

/// Resolve the path to the `phantom-nlp` dylib artifact.
fn resolve_dylib_path() -> anyhow::Result<PathBuf> {
    // 1. Explicit override — same env var as the semantic skill for consistency.
    if let Some(dir) = std::env::var_os("PHANTOM_HOT_MODULES_DIR") {
        let path = PathBuf::from(dir).join(DYLIB_NAME);
        if path.exists() {
            return Ok(path);
        }
        bail!(
            "PHANTOM_HOT_MODULES_DIR is set but {:?} does not exist",
            path
        );
    }

    // 2. Walk up from cwd to find workspace root (has Cargo.lock), then
    //    append `target/debug/`.
    if let Ok(cwd) = std::env::current_dir() {
        let mut dir = cwd.as_path();
        loop {
            let candidate = dir.join("target/debug").join(DYLIB_NAME);
            if candidate.exists() {
                return Ok(candidate);
            }
            if dir.join("Cargo.lock").exists() {
                bail!(
                    "workspace root found at {:?} but {:?} does not exist \
                     — run `cargo build -p phantom-nlp` first",
                    dir,
                    dir.join("target/debug").join(DYLIB_NAME)
                );
            }
            match dir.parent() {
                Some(p) => dir = p,
                None => break,
            }
        }
    }

    bail!(
        "could not locate {}; set PHANTOM_HOT_MODULES_DIR or run \
         `cargo build -p phantom-nlp`",
        DYLIB_NAME
    )
}

/// Return the resolved dylib path without loading, for use by a future watcher.
pub fn dylib_path() -> anyhow::Result<PathBuf> {
    resolve_dylib_path()
}
