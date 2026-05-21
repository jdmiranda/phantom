//! Dylib loader — resolves `phantom_skill_register` and wraps the vtable.
//!
//! Active only when `hot-modules` feature + debug build.
//!
//! ## Security gates applied before `Library::new`
//!
//! 1. **SHA-256 sidecar check** (Fix 3) — if `<dylib>.sha256` exists the
//!    file digest must match before the OS loader is invoked.  A missing
//!    sidecar emits a warning and proceeds, allowing dev iteration.
//!
//! ## Security gates applied after `Library::new`
//!
//! 2. **ABI version check** (Fix 2) — `vtable.abi_version` must equal
//!    [`CURRENT_ABI_VERSION`]; mismatches return [`LoadError::AbiVersionMismatch`]
//!    before any function pointer is dereferenced.

#![cfg(all(debug_assertions, feature = "hot-modules"))]

use std::io::Read as _;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use sha2::{Digest, Sha256};

use crate::ffi::{CURRENT_ABI_VERSION, REGISTER_SYMBOL, SemanticSkillVtable};
use crate::host::{SemanticSkill, dylib_impl::DylibBacked};

// ---------------------------------------------------------------------------
// LoadError
// ---------------------------------------------------------------------------

/// Errors produced by the security gates in the dylib loader.
///
/// These are erased to `anyhow::Error` at the call site but can be recovered
/// with `anyhow::Error::downcast_ref::<LoadError>()` in tests.
#[derive(Debug)]
pub enum LoadError {
    /// The `.sha256` sidecar file exists but the digest does not match the
    /// dylib on disk.  The dylib was **not** loaded.
    SignatureVerificationFailed {
        path: std::path::PathBuf,
        expected: String,
        got: String,
    },

    /// The vtable's `abi_version` field does not match [`CURRENT_ABI_VERSION`].
    AbiVersionMismatch { expected: u32, got: u32 },
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SignatureVerificationFailed { path, expected, got } => write!(
                f,
                "SHA-256 digest mismatch for {path:?}: expected {expected}, got {got}"
            ),
            Self::AbiVersionMismatch { expected, got } => write!(
                f,
                "ABI version mismatch in dylib: expected {expected}, got {got}. \
                 Rebuild the dylib against the current phantom-skill-host."
            ),
        }
    }
}

impl std::error::Error for LoadError {}

// ---------------------------------------------------------------------------
// Platform dylib filename
// ---------------------------------------------------------------------------

/// Platform-specific dylib filename for `phantom-semantic`.
#[cfg(target_os = "macos")]
pub const DYLIB_NAME: &str = "libphantom_semantic.dylib";
#[cfg(target_os = "linux")]
pub const DYLIB_NAME: &str = "libphantom_semantic.so";
#[cfg(target_os = "windows")]
pub const DYLIB_NAME: &str = "phantom_semantic.dll";
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub const DYLIB_NAME: &str = "libphantom_semantic.so";

// ---------------------------------------------------------------------------
// RegisterFn
// ---------------------------------------------------------------------------

/// Type of the `phantom_skill_register` symbol exported by skill dylibs.
///
/// # Safety
///
/// The returned pointer must point to a valid `SemanticSkillVtable` with all
/// function pointers non-null.  The vtable must remain valid for the lifetime
/// of the library.
type RegisterFn = unsafe extern "C" fn() -> *const SemanticSkillVtable;

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

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

    // Fix 3: SHA-256 sidecar verification — must happen before the OS loads
    // the dylib so that a tampered file is never executed.
    verify_sha256(path)?;

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
        anyhow::bail!("`phantom_skill_register` returned null vtable pointer");
    }

    // Fix 2: ABI version check — read `abi_version` (the first repr(C) field)
    // before touching any function-pointer field.  A layout mismatch detected
    // here prevents silent UB from a mismatched vtable struct.
    //
    // SAFETY: vtable_ptr is non-null and points into the dylib's read-only
    // data segment, which is valid for the lifetime of `lib`.
    let got_version = unsafe { (*vtable_ptr).abi_version };
    if got_version != CURRENT_ABI_VERSION {
        return Err(LoadError::AbiVersionMismatch {
            expected: CURRENT_ABI_VERSION,
            got: got_version,
        }
        .into());
    }

    // Validate that all function pointers are non-null.
    // SAFETY: we just checked vtable_ptr is non-null and abi_version matches;
    // it points into the dylib's read-only data which outlives the `Arc<Library>` below.
    // Return Err rather than panicking — a malformed dylib must not crash the process.
    unsafe {
        let vt = &*vtable_ptr;
        // The cast via `as usize` on a fn pointer is defined behaviour.
        if vt.classify_command as usize == 0 {
            anyhow::bail!("classify_command fn ptr is null in {:?}", path);
        }
        if vt.parse as usize == 0 {
            anyhow::bail!("parse fn ptr is null in {:?}", path);
        }
        if vt.free_buf as usize == 0 {
            anyhow::bail!("free_buf fn ptr is null in {:?}", path);
        }
    }

    let lib_arc = Arc::new(lib);

    // SAFETY: vtable_ptr is non-null, abi_version validated, all fn ptrs
    // non-null, `lib_arc` will keep the library alive for the lifetime of
    // `DylibBacked`.
    let backed = unsafe { DylibBacked::new(vtable_ptr, lib_arc) };

    log::info!("skill-host: phantom-semantic dylib loaded successfully");
    Ok(Arc::new(backed))
}

// ---------------------------------------------------------------------------
// SHA-256 sidecar verification (Fix 3)
// ---------------------------------------------------------------------------

/// Verify the SHA-256 digest of `dylib_path` against its `.sha256` sidecar.
///
/// * Sidecar present and digest matches → `Ok(())`.
/// * Sidecar present and digest mismatches → `Err(LoadError::SignatureVerificationFailed)`.
/// * Sidecar absent → `warn!` + `Ok(())` (dev builds without sidecars still work).
pub(crate) fn verify_sha256(dylib_path: &std::path::Path) -> anyhow::Result<()> {
    let sidecar = dylib_path.with_extension("sha256");

    if !sidecar.exists() {
        log::warn!(
            "skill-host: no .sha256 sidecar found for {:?} — skipping verification",
            dylib_path
        );
        return Ok(());
    }

    // Read the expected hex digest (one line, optional trailing whitespace).
    let expected_hex = std::fs::read_to_string(&sidecar)
        .with_context(|| format!("failed to read sidecar {:?}", sidecar))?;
    let expected_hex = expected_hex.trim().to_ascii_lowercase();

    // Stream-hash the dylib to avoid loading it fully into memory.
    let mut hasher = Sha256::new();
    let mut file = std::fs::File::open(dylib_path)
        .with_context(|| format!("failed to open {:?} for hashing", dylib_path))?;
    let mut buf = [0u8; 65536];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("I/O error while hashing {:?}", dylib_path))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got_hex = hex::encode(hasher.finalize());

    if got_hex != expected_hex {
        return Err(LoadError::SignatureVerificationFailed {
            path: dylib_path.to_path_buf(),
            expected: expected_hex,
            got: got_hex,
        }
        .into());
    }

    log::info!(
        "skill-host: SHA-256 verified for {:?} ({}…)",
        dylib_path,
        &got_hex[..16]
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Resolve the path to the `phantom-semantic` dylib artifact.
fn resolve_dylib_path() -> anyhow::Result<PathBuf> {
    // 1. Explicit override.
    if let Some(dir) = std::env::var_os("PHANTOM_HOT_MODULES_DIR") {
        let path = PathBuf::from(dir).join(DYLIB_NAME);
        if path.exists() {
            return Ok(path);
        }
        anyhow::bail!(
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
                anyhow::bail!(
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

    anyhow::bail!(
        "could not locate {}; set PHANTOM_HOT_MODULES_DIR or run \
         `cargo build -p phantom-semantic`",
        DYLIB_NAME
    )
}

/// Return the resolved dylib path without loading, for use by the watcher.
pub fn dylib_path() -> anyhow::Result<PathBuf> {
    resolve_dylib_path()
}

// ---------------------------------------------------------------------------
// Tests for Fix 2 (ABI version mismatch) and Fix 3 (SHA-256 sidecar)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_file(dir: &TempDir, name: &str, contents: &[u8]) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    // -----------------------------------------------------------------------
    // Fix 3 tests
    // -----------------------------------------------------------------------

    /// When the sidecar digest matches the file, `verify_sha256` returns Ok.
    #[test]
    fn sha256_matching_sidecar_returns_ok() {
        let dir = TempDir::new().unwrap();
        let payload = b"fake dylib bytes";
        let dylib = write_file(&dir, "skill.dylib", payload);

        use sha2::{Digest, Sha256};
        let digest = hex::encode(Sha256::digest(payload));
        std::fs::write(dylib.with_extension("sha256"), &digest).unwrap();

        verify_sha256(&dylib).expect("matching sidecar should return Ok");
    }

    /// When the sidecar digest does not match, `verify_sha256` returns
    /// `LoadError::SignatureVerificationFailed`.
    #[test]
    fn sha256_mismatch_returns_error() {
        let dir = TempDir::new().unwrap();
        let dylib = write_file(&dir, "skill.dylib", b"real bytes");
        // Write a deliberately wrong digest.
        std::fs::write(
            dylib.with_extension("sha256"),
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
        )
        .unwrap();

        let err = verify_sha256(&dylib).unwrap_err();
        let load_err = err
            .downcast::<LoadError>()
            .expect("error should downcast to LoadError");
        assert!(
            matches!(load_err, LoadError::SignatureVerificationFailed { .. }),
            "expected SignatureVerificationFailed, got {load_err:?}"
        );
    }

    /// When no sidecar exists, `verify_sha256` logs a warning and returns Ok.
    #[test]
    fn missing_sidecar_logs_warn_and_continues() {
        let dir = TempDir::new().unwrap();
        let dylib = write_file(&dir, "skill.dylib", b"some bytes");
        // No sidecar file written.
        verify_sha256(&dylib).expect("missing sidecar should return Ok (warn + proceed)");
    }

    // -----------------------------------------------------------------------
    // Fix 2 test — ABI version mismatch
    // -----------------------------------------------------------------------

    /// Verify that `LoadError::AbiVersionMismatch` is produced when the vtable
    /// reports a version that differs from `CURRENT_ABI_VERSION`.
    ///
    /// We exercise the version-check logic directly (we cannot build a real
    /// dylib in a unit test), asserting both the variant shape and the error
    /// message content.
    #[test]
    fn abi_version_mismatch_returns_error() {
        let got: u32 = 0; // wrong version
        let expected = CURRENT_ABI_VERSION;
        assert_ne!(got, expected, "test precondition: versions must differ");

        // Replicate the gate from load_from_path.
        let result: Result<(), LoadError> = if got != expected {
            Err(LoadError::AbiVersionMismatch { expected, got })
        } else {
            Ok(())
        };

        let err = result.unwrap_err();
        assert!(
            matches!(
                err,
                LoadError::AbiVersionMismatch {
                    expected: e,
                    got: g
                } if e == CURRENT_ABI_VERSION && g == 0
            ),
            "expected AbiVersionMismatch{{expected={}, got=0}}, got {err:?}",
            CURRENT_ABI_VERSION
        );
        let msg = err.to_string();
        assert!(
            msg.contains(&CURRENT_ABI_VERSION.to_string()),
            "message should mention expected version; got: {msg}"
        );
        assert!(
            msg.contains('0'),
            "message should mention the bad version 0; got: {msg}"
        );
    }
}
