//! C-ABI vtable definition for the LLM skill dylib boundary.
//!
//! This is the **only** thing that crosses the dylib boundary between
//! `phantom-skill-host` and `phantom-nlp`.  Every field is `extern "C"` and
//! `#[repr(C)]`-stable.
//!
//! ## String convention
//!
//! * **Into the vtable**: callers pass `(ptr: *const u8, len: usize)` pairs.
//!   The implementation borrows the bytes for the duration of the call only.
//! * **Out of the vtable**: `complete` writes a heap-allocated UTF-8 string
//!   into `(*mut *mut u8, *mut usize)` and sets `*out_err` to a status code:
//!   - `0` — success
//!   - `1` — `NotConfigured`
//!   - `2` — `Transport`
//!   - `3` — `Other`
//!   The caller must free the buffer with `free_buf` in all cases (success and
//!   error).
//!
//! ## Lifetime
//!
//! The vtable is valid for as long as the `Library` that produced it is loaded.
//! `LlmHost` ensures the `Arc<Library>` is kept alive in `DylibBackedLlm`.

/// C-ABI vtable exported by the `phantom-nlp` dylib.
///
/// Obtain one by calling `phantom_llm_register()` in the dylib.
/// All function pointers must be non-null.
#[repr(C)]
pub struct LlmSkillVtable {
    /// Return the backend's stable name as a pointer to a null-terminated
    /// static C string that lives for the lifetime of the dylib.
    pub name: unsafe extern "C" fn() -> *const u8,

    /// Send `system_prompt` + `user_message` to the LLM backend.
    ///
    /// On success: writes the reply bytes into `*out_ptr / *out_len` and sets
    /// `*out_err = 0`.
    /// On failure: writes the error message into `*out_ptr / *out_len` and
    /// sets `*out_err` to the discriminant (1–3).
    ///
    /// The caller MUST call `free_buf(*out_ptr, *out_len)` in both cases
    /// (unless `*out_ptr` is null, which indicates an allocation failure).
    ///
    /// # Safety
    /// All pointer/length pairs must reference valid UTF-8 for their lengths.
    /// `out_ptr`, `out_len`, `out_err` must be valid writable pointers.
    pub complete: unsafe extern "C" fn(
        system_prompt: *const u8,
        system_prompt_len: usize,
        user_message: *const u8,
        user_message_len: usize,
        out_ptr: *mut *mut u8,
        out_len: *mut usize,
        out_err: *mut u8,
    ),

    /// Free a buffer that was written by `complete`.
    ///
    /// # Safety
    /// `ptr` must be the exact pointer written to `*out_ptr` by `complete`,
    /// with the given `len`.  Must be called exactly once.
    pub free_buf: unsafe extern "C" fn(ptr: *mut u8, len: usize),
}

/// Symbol name exported by the `phantom-nlp` dylib.
pub const REGISTER_SYMBOL: &[u8] = b"phantom_llm_register\0";

/// Error discriminant values returned in `*out_err` by `complete`.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LlmErrorDisc {
    /// Success — `*out_ptr / *out_len` contains the reply.
    Ok = 0,
    /// `TranslateError::NotConfigured`.
    NotConfigured = 1,
    /// `TranslateError::Transport`.
    Transport = 2,
    /// `TranslateError::Other`.
    Other = 3,
}
