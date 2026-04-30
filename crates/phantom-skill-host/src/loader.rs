//! Dylib loader — resolves `phantom_skill_register` and wraps the vtable.
//!
//! Active only when `hot-modules` feature + debug build.

#![cfg(all(debug_assertions, feature = "hot-modules"))]

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context};

use crate::ffi::{SemanticSkillVtable, REGISTER_SYMBOL};
use crate::host::{dylib_impl::DylibBacked, SemanticSkill};

/// Platform-specific dylib filename for `phantom-semantic`.
#[cfg(target_os = "macos")]
pub const DYLIB_NAME: &str = "libphantom_semantic.dylib";
#[cfg(target_os = "linux")]
pub const DYLIB_NAME: &str = "libphantom_semantic.so";
#[cfg(target_os = "windows")]
pub const DYLIB_NAME: &str = "phantom_semantic.dll";
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub const DYLIB_NAME: &str = "libphantom_semantic.so";

/// Type of the `phantom_skill_register` symbol exported by skill dylibs.
///
/// # Safety
///
/// The returned pointer must point to a valid `SemanticSkillVtable` with all
/// function pointers non-null.  The vtable must remain valid for the lifetime
/// of the library.
type RegisterFn = unsafe extern "C" fn() -> *const SemanticSkillVtable;

/// Locate and load the `phantom-semantic` dylib, resolve its vtable, and
/// return a `DylibBacked` skill.
///
/// Search order:
/// 1. `PHANTOM_HOT_MODULES_DIR` environment variable (if set).
/// 2. `<cargo_target_dir>/debug/` (from `CARGO_MANIFEST_DIR` walking up to
///    find `Cargo.lock`, then appending `target/debug`).
/// 3. `./target/debug/` relative to the process working directory.
pub fn load_semantic_dylib() -> anyhow::Result<Arc<dyn SemanticSkill>> {
    let path = resolve_dylib_path()?;
    load_from_path(&path)
}

/// Load from an explicit path.  Used by tests and the watcher.
pub fn load_from_path(path: &std::path::Path) -> anyhow::Result<Arc<dyn SemanticSkill>> {
    log::info!("skill-host: loading phantom-semantic dylib from {:?}", path);

    // SAFETY: We validate the symbol immediately after loading.
    let lib = unsafe {
        libloading::Library::new(path)
            .with_context(|| format!("failed to open dylib {:?}", path))?
    };

    // Validate: symbol must exist.
    // SAFETY: We immediately call through this pointer with correct arguments;
    // the library is kept alive in the Arc we build below.
    let register: libloading::Symbol<RegisterFn> = unsafe {
        lib.get(REGISTER_SYMBOL)
            .with_context(|| format!("symbol `phantom_skill_register` not found in {:?}", path))?
    };

    // Call register to get the vtable pointer.
    // SAFETY: `register` is a valid `RegisterFn` from the dylib.
    let vtable_ptr: *const SemanticSkillVtable = unsafe { register() };

    if vtable_ptr.is_null() {
        bail!("`phantom_skill_register` returned null vtable pointer");
    }

    // Validate that all function pointers are non-null.
    // SAFETY: we just checked vtable_ptr is non-null; it points into the
    // dylib's read-only data which outlives the `Arc<Library>` below.
    // Return Err rather than panicking — a malformed dylib must not crash the process.
    unsafe {
        let vt = &*vtable_ptr;
        // The cast via `as usize` on a fn pointer is defined behaviour.
        if vt.classify_command as usize == 0 {
            bail!("classify_command fn ptr is null in {:?}", path);
        }
        if vt.parse as usize == 0 {
            bail!("parse fn ptr is null in {:?}", path);
        }
        if vt.free_buf as usize == 0 {
            bail!("free_buf fn ptr is null in {:?}", path);
        }
    }

    let lib_arc = Arc::new(lib);

    // SAFETY: vtable_ptr is non-null, all fields validated, `lib_arc` will
    // keep the library alive for the lifetime of `DylibBacked`.
    let backed = unsafe { DylibBacked::new(vtable_ptr, lib_arc) };

    log::info!("skill-host: phantom-semantic dylib loaded successfully");
    Ok(Arc::new(backed))
}

/// Resolve the path to the `phantom-semantic` dylib artifact.
fn resolve_dylib_path() -> anyhow::Result<PathBuf> {
    // 1. Explicit override.
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
            // Stop if we found Cargo.lock (workspace root) but no dylib yet.
            if dir.join("Cargo.lock").exists() {
                // The dylib doesn't exist yet — tell the caller clearly.
                bail!(
                    "workspace root found at {:?} but {:?} does not exist \
                     — run `cargo build -p phantom-semantic` first",
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
         `cargo build -p phantom-semantic`",
        DYLIB_NAME
    )
}

/// Return the resolved dylib path without loading, for use by the watcher.
pub fn dylib_path() -> anyhow::Result<PathBuf> {
    resolve_dylib_path()
}
