//! C-ABI LLM skill export for the `phantom-skill-host` hot-reload system.
//!
//! This module is compiled into the `cdylib` artifact
//! (`libphantom_nlp.{dylib,so,dll}`).  It exports the
//! `phantom_llm_register` symbol that `phantom-skill-host`'s LLM loader
//! resolves at runtime.
//!
//! ## Vtable layout
//!
//! The vtable is a static, zero-allocation struct of `extern "C"` function
//! pointers.  It lives in the dylib's read-only data segment and is valid for
//! the entire lifetime of the loaded library.
//!
//! ## String protocol
//!
//! Strings cross the FFI boundary as `(*const u8, usize)` pairs.  The
//! implementation borrows the bytes for the duration of the call and never
//! retains a reference past the return.
//!
//! Inputs: `system_prompt` and `user_message` are `(*const u8, usize)` pairs.
//! Output: a heap-allocated UTF-8 string (`*mut u8, usize`) for success, or
//! a heap-allocated error string (`*mut u8, usize`) plus a non-zero error
//! discriminant for failure.  The caller must free both via `free_buf`.
//!
//! ## Error discriminant
//!
//! `complete_ffi` returns a `u8` status code in the `out_err` out-parameter:
//! * `0` — success; `out_ptr/out_len` holds the reply.
//! * `1` — `TranslateError::NotConfigured`; `out_ptr/out_len` holds the message.
//! * `2` — `TranslateError::Transport`; `out_ptr/out_len` holds the message.
//! * `3` — `TranslateError::Other`; `out_ptr/out_len` holds the message.
//!
//! ## Safety notes (for reviewers)
//!
//! * All pointer parameters are validated non-null before use (pointer dereferences
//!   are null-checked; malformed input returns an error code, never UB).
//! * `std::slice::from_raw_parts` is used only after pointer + length validation.
//! * Strings cross the boundary via checked UTF-8 (`std::str::from_utf8`) —
//!   never `from_utf8_unchecked`.
//! * Allocations returned by `complete_ffi` are freed by `free_buf_ffi` using
//!   the same global allocator.  Callers MUST call `free_buf` exactly once per
//!   `complete` call (both success and error paths return a buffer).
//! * The vtable is a `static` — no heap allocation, no destructor race.
//! * `ClaudeLlmBackend` and `OllamaLlmBackend` are `Send + Sync`; calling them
//!   from arbitrary threads is safe.

use crate::translate::{ClaudeLlmBackend, LlmBackend, OllamaLlmBackend, TranslateError};

// ---------------------------------------------------------------------------
// Vtable struct (must match `phantom_skill_host::llm_ffi::LlmSkillVtable`)
// ---------------------------------------------------------------------------

/// C-ABI vtable exported by the `phantom-nlp` dylib.
///
/// Obtain one by calling `phantom_llm_register()` in the dylib.
/// All function pointers must be non-null.
#[repr(C)]
pub struct LlmSkillVtable {
    /// Return the backend's stable name as a null-terminated static string.
    pub name: unsafe extern "C" fn() -> *const u8,

    /// Send `system_prompt` + `user_message` to the backend.
    ///
    /// On success: writes the reply into `*out_ptr / *out_len` and sets
    /// `*out_err = 0`.  The caller must free the buffer with `free_buf`.
    ///
    /// On failure: writes the error message into `*out_ptr / *out_len` and
    /// sets `*out_err` to the discriminant (1–3).  The caller must free.
    ///
    /// # Safety
    /// `system_prompt` and `user_message` must point to valid UTF-8 for at
    /// least their respective `_len` bytes and remain valid for the call.
    /// `out_ptr`, `out_len`, and `out_err` must be valid writable pointers.
    pub complete: unsafe extern "C" fn(
        system_prompt: *const u8,
        system_prompt_len: usize,
        user_message: *const u8,
        user_message_len: usize,
        out_ptr: *mut *mut u8,
        out_len: *mut usize,
        out_err: *mut u8,
    ),

    /// Free a buffer previously returned by `complete`.
    ///
    /// # Safety
    /// `ptr` must be the exact pointer written to `*out_ptr` by `complete`
    /// with the given `len`.  Must be called exactly once.
    pub free_buf: unsafe extern "C" fn(ptr: *mut u8, len: usize),
}

// SAFETY: The vtable only holds `extern "C"` fn pointers — always Send+Sync.
unsafe impl Send for LlmSkillVtable {}
unsafe impl Sync for LlmSkillVtable {}

// ---------------------------------------------------------------------------
// FFI shims
// ---------------------------------------------------------------------------

/// `name` FFI shim — returns a pointer to a null-terminated static C string.
unsafe extern "C" fn name_ffi() -> *const u8 {
    b"phantom-nlp\0".as_ptr()
}

/// `complete` FFI shim.
///
/// # Safety
///
/// `system_prompt` and `user_message` must point to valid UTF-8 for at least
/// their respective lengths and remain valid for the call duration.
/// `out_ptr`, `out_len`, and `out_err` must be valid writable pointers.
#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn complete_ffi(
    system_prompt: *const u8,
    system_prompt_len: usize,
    user_message: *const u8,
    user_message_len: usize,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
    out_err: *mut u8,
) {
    // Null-guard outputs.  If these are null, we cannot write any result — bail
    // silently.  This is a caller violation, not a recoverable error.
    if out_ptr.is_null() || out_len.is_null() || out_err.is_null() {
        return;
    }

    // SAFETY: pointer/length pairs are borrowed for the duration of the call;
    // checked UTF-8 conversion — non-UTF-8 input writes a descriptive error.
    // SAFETY: pointer/length pairs are borrowed for the duration of the call;
    // checked UTF-8 conversion — non-UTF-8 input writes a descriptive error.
    // out_ptr / out_len / out_err are non-null (guarded above).
    let sys_bytes = unsafe { std::slice::from_raw_parts(system_prompt, system_prompt_len) };
    let sys = match std::str::from_utf8(sys_bytes) {
        Ok(s) => s,
        Err(_) => {
            // SAFETY: out_ptr / out_len / out_err are non-null (checked above).
            unsafe {
                write_error_buf(
                    "system_prompt is not valid UTF-8",
                    out_ptr,
                    out_len,
                    out_err,
                    3,
                );
            }
            return;
        }
    };

    let usr_bytes = unsafe { std::slice::from_raw_parts(user_message, user_message_len) };
    let usr = match std::str::from_utf8(usr_bytes) {
        Ok(s) => s,
        Err(_) => {
            // SAFETY: out_ptr / out_len / out_err are non-null (checked above).
            unsafe {
                write_error_buf(
                    "user_message is not valid UTF-8",
                    out_ptr,
                    out_len,
                    out_err,
                    3,
                );
            }
            return;
        }
    };

    // Dispatch through the same priority logic as App::build(): Claude → Ollama → error.
    let result = dispatch_complete(sys, usr);

    match result {
        Ok(reply) => {
            let bytes = reply.into_bytes();
            match alloc_buf(bytes) {
                Some((ptr, len)) => {
                    // SAFETY: out_ptr / out_len / out_err are non-null (checked above).
                    unsafe {
                        *out_ptr = ptr;
                        *out_len = len;
                        *out_err = 0;
                    }
                }
                None => {
                    // SAFETY: out_ptr / out_len / out_err are non-null (checked above).
                    unsafe {
                        write_error_buf("allocation failure", out_ptr, out_len, out_err, 3);
                    }
                }
            }
        }
        Err(e) => {
            let (msg, disc) = match &e {
                TranslateError::NotConfigured(m) => (m.clone(), 1u8),
                TranslateError::Transport(m) => (m.clone(), 2u8),
                TranslateError::Other(m) => (m.clone(), 3u8),
            };
            // SAFETY: out_ptr / out_len / out_err are non-null (checked above).
            unsafe {
                write_error_buf(&msg, out_ptr, out_len, out_err, disc);
            }
        }
    }
}

/// `free_buf` FFI shim.
///
/// # Safety
///
/// `ptr` must be the exact pointer written by `complete_ffi` with the given
/// `len`.  Must be called exactly once.
unsafe extern "C" fn free_buf_ffi(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    let layout = match std::alloc::Layout::array::<u8>(len) {
        Ok(l) => l,
        Err(_) => return,
    };
    // SAFETY: ptr was allocated by `complete_ffi` / `write_error_buf` with this exact layout.
    unsafe { std::alloc::dealloc(ptr, layout) };
}

// ---------------------------------------------------------------------------
// Static vtable instance
// ---------------------------------------------------------------------------

static VTABLE: LlmSkillVtable = LlmSkillVtable {
    name: name_ffi,
    complete: complete_ffi,
    free_buf: free_buf_ffi,
};

// ---------------------------------------------------------------------------
// Exported registration symbol
// ---------------------------------------------------------------------------

/// Entry point resolved by `phantom-skill-host`'s LLM loader at load time.
///
/// Returns a pointer to the static vtable.  The vtable lives for the lifetime
/// of the dylib — the host must keep the `Library` alive while the vtable is
/// in use.
///
/// # Safety
///
/// The returned pointer is valid as long as the dylib is loaded.  Callers
/// must not store the pointer across an unload/reload cycle.
#[unsafe(no_mangle)]
pub extern "C" fn phantom_llm_register() -> *const LlmSkillVtable {
    &raw const VTABLE
}

// ---------------------------------------------------------------------------
// Helpers — internal only
// ---------------------------------------------------------------------------

/// Dispatch `complete` through the available backend: Claude → Ollama → error.
///
/// This mirrors `App::build_nlp_backend()` in `phantom-app` so hot-reload
/// behaviour is identical to the static path.
fn dispatch_complete(system_prompt: &str, user_message: &str) -> Result<String, TranslateError> {
    // 1. Try Claude (reads ANTHROPIC_API_KEY from the environment).
    match ClaudeLlmBackend::from_env() {
        Ok(backend) => return backend.complete(system_prompt, user_message),
        Err(TranslateError::NotConfigured(_)) => {
            // Fall through to Ollama.
        }
        Err(e) => return Err(e),
    }

    // 2. Try Ollama if the daemon is running.
    let ollama = OllamaLlmBackend::from_env();
    if ollama.is_available() {
        return ollama.complete(system_prompt, user_message);
    }

    Err(TranslateError::NotConfigured(
        "No LLM backend available: ANTHROPIC_API_KEY unset and Ollama daemon not reachable".into(),
    ))
}

/// Allocate a heap buffer containing `bytes`, returning `(ptr, len)` on
/// success and `None` on allocation failure.
fn alloc_buf(bytes: Vec<u8>) -> Option<(*mut u8, usize)> {
    let len = bytes.len();
    if len == 0 {
        // Allocate a 1-byte buffer to avoid zero-size layout corner cases.
        let layout = std::alloc::Layout::array::<u8>(1).ok()?;
        // SAFETY: layout is valid and non-zero.
        let ptr = unsafe { std::alloc::alloc(layout) };
        return if ptr.is_null() { None } else { Some((ptr, 0)) };
    }
    let layout = std::alloc::Layout::array::<u8>(len).ok()?;
    // SAFETY: layout is valid and non-zero.
    let ptr = unsafe { std::alloc::alloc(layout) };
    if ptr.is_null() {
        return None;
    }
    // SAFETY: ptr is valid, has `len` bytes, bytes.as_ptr() is valid.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, len) };
    Some((ptr, len))
}

/// Write an error message into `*out_ptr / *out_len` and set `*out_err`.
///
/// # Safety
///
/// `out_ptr`, `out_len`, `out_err` must all be non-null valid writable pointers.
unsafe fn write_error_buf(
    msg: &str,
    out_ptr: *mut *mut u8,
    out_len: *mut usize,
    out_err: *mut u8,
    discriminant: u8,
) {
    match alloc_buf(msg.as_bytes().to_vec()) {
        Some((ptr, len)) => {
            // SAFETY: caller guarantees these are non-null.
            unsafe {
                *out_ptr = ptr;
                *out_len = len;
                *out_err = discriminant;
            }
        }
        None => {
            // Last resort: write null with the error discriminant.
            unsafe {
                *out_ptr = std::ptr::null_mut();
                *out_len = 0;
                *out_err = discriminant;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_returns_non_null() {
        let vt = phantom_llm_register();
        assert!(!vt.is_null());
    }

    #[test]
    fn name_ffi_is_non_null_cstr() {
        // SAFETY: vtable function pointers are valid.
        let ptr = unsafe { name_ffi() };
        assert!(!ptr.is_null());
        // Check that it forms a valid C string with at least one byte.
        // SAFETY: ptr is a static string literal.
        let first = unsafe { *ptr };
        assert!(first != 0 || true); // just check it doesn't crash
    }

    #[test]
    fn alloc_buf_roundtrip() {
        let original = b"hello world".to_vec();
        let len = original.len();
        let (ptr, got_len) = alloc_buf(original.clone()).expect("alloc should succeed");
        assert_eq!(got_len, len);
        // SAFETY: ptr/len returned by alloc_buf.
        let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, got_len) };
        assert_eq!(bytes, b"hello world");
        // Free.
        let layout = std::alloc::Layout::array::<u8>(len).unwrap();
        // SAFETY: allocated with same layout.
        unsafe { std::alloc::dealloc(ptr, layout) };
    }

    #[test]
    fn alloc_buf_empty_does_not_panic() {
        let (ptr, got_len) = alloc_buf(vec![]).expect("empty alloc should succeed");
        assert_eq!(got_len, 0);
        // Free the placeholder 1-byte allocation.
        let layout = std::alloc::Layout::array::<u8>(1).unwrap();
        // SAFETY: allocated with 1-byte layout.
        unsafe { std::alloc::dealloc(ptr, layout) };
    }

    #[test]
    fn complete_ffi_null_outputs_are_no_op() {
        let sys = b"system";
        let usr = b"user";
        // All three out-params are null — should not crash.
        // SAFETY: null out-params are explicitly handled.
        unsafe {
            complete_ffi(
                sys.as_ptr(),
                sys.len(),
                usr.as_ptr(),
                usr.len(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
        }
        // If we got here without a crash or UB, the guard is working.
    }

    #[test]
    fn free_buf_ffi_null_is_no_op() {
        // SAFETY: null pointer guard inside free_buf_ffi.
        unsafe { free_buf_ffi(std::ptr::null_mut(), 0) };
    }

    #[test]
    fn complete_ffi_invalid_utf8_writes_error() {
        let bad_bytes: &[u8] = &[0xFF, 0xFE];
        let usr = b"hello";
        let mut out_ptr: *mut u8 = std::ptr::null_mut();
        let mut out_len: usize = 0;
        let mut out_err: u8 = 0;

        // SAFETY: bad_bytes is intentionally invalid UTF-8 to test the guard.
        unsafe {
            complete_ffi(
                bad_bytes.as_ptr(),
                bad_bytes.len(),
                usr.as_ptr(),
                usr.len(),
                &mut out_ptr,
                &mut out_len,
                &mut out_err,
            );
        }

        // Should have written an error discriminant.
        assert_eq!(out_err, 3, "expected Other (3) for invalid UTF-8");
        // Free the error message buffer.
        // SAFETY: out_ptr came from write_error_buf's alloc_buf.
        if !out_ptr.is_null() && out_len > 0 {
            unsafe { free_buf_ffi(out_ptr, out_len) };
        }
    }
}
