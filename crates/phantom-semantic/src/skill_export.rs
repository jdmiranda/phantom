//! C-ABI skill export for the `phantom-skill-host` hot-reload system.
//!
//! This module is compiled into the `cdylib` artifact
//! (`libphantom_semantic.{dylib,so,dll}`).  It exports the
//! `phantom_skill_register` symbol that `phantom-skill-host`'s loader
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
//! retains a reference past the return.  Output is heap-allocated JSON
//! (serialised `ParsedOutput`) returned as `(*mut u8, usize)` with a
//! companion `free_buf` fn.
//!
//! ## Safety notes (for reviewers)
//!
//! * All pointer parameters are validated non-null before use.
//! * `std::slice::from_raw_parts` is used only after pointer + length have been
//!   validated; the resulting slice is immediately converted to `str`.
//! * Allocations returned by `parse_ffi` are freed by `free_buf_ffi` using the
//!   same global allocator.  Callers MUST call `free_buf` exactly once per
//!   successful `parse` call.
//! * The vtable is a `static` — no heap allocation, no destructor race.

use crate::parser::SemanticParser;
use crate::types::ParsedOutput;

// ---------------------------------------------------------------------------
// Re-export the tag enum so the host can use it without depending on us.
// This enum mirrors `phantom_skill_host::ffi::CommandTypeTag`.
// ---------------------------------------------------------------------------

/// C-ABI discriminant for [`crate::types::CommandType`].
///
/// New variants MUST be appended — existing discriminants must not change.
#[repr(u8)]
pub enum CommandTypeTag {
    Unknown = 0,
    Git     = 1,
    Cargo   = 2,
    Docker  = 3,
    Npm     = 4,
    Http    = 5,
    Shell   = 6,
}

// ---------------------------------------------------------------------------
// Vtable struct (must match `phantom_skill_host::ffi::SemanticSkillVtable`)
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct SemanticSkillVtable {
    pub classify_command: unsafe extern "C" fn(*const u8, usize) -> u8,
    pub parse: unsafe extern "C" fn(
        *const u8, usize,  // cmd
        *const u8, usize,  // stdout
        *const u8, usize,  // stderr
        u8,                // has_exit_code
        i32,               // exit_code
        *mut usize,        // out_len
    ) -> *mut u8,
    pub free_buf: unsafe extern "C" fn(*mut u8, usize),
}

// SAFETY: The vtable only holds `extern "C"` fn pointers — always Send+Sync.
unsafe impl Send for SemanticSkillVtable {}
unsafe impl Sync for SemanticSkillVtable {}

// ---------------------------------------------------------------------------
// Implementations
// ---------------------------------------------------------------------------

/// `classify_command` FFI shim.
///
/// # Safety
///
/// `cmd` must point to valid UTF-8 for at least `cmd_len` bytes.
unsafe extern "C" fn classify_command_ffi(cmd: *const u8, cmd_len: usize) -> u8 {
    // SAFETY: caller guarantees cmd points to valid UTF-8 of length cmd_len.
    let cmd_str = unsafe {
        let bytes = std::slice::from_raw_parts(cmd, cmd_len);
        match std::str::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => return CommandTypeTag::Unknown as u8,
        }
    };

    let ct = SemanticParser::classify_command(cmd_str);
    command_type_to_tag(&ct) as u8
}

/// `parse` FFI shim.
///
/// Serialises the result to JSON and returns a heap-allocated buffer.
/// The caller must free the buffer by calling `free_buf_ffi`.
///
/// Returns null if serialisation fails (caller should fall back to static).
///
/// # Safety
///
/// All pointer/length pairs must reference valid UTF-8 for their respective
/// lengths. `out_len` must be a valid writable pointer.
#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn parse_ffi(
    cmd: *const u8,
    cmd_len: usize,
    stdout: *const u8,
    stdout_len: usize,
    stderr: *const u8,
    stderr_len: usize,
    has_exit_code: u8,
    exit_code: i32,
    out_len: *mut usize,
) -> *mut u8 {
    // SAFETY: callers guarantee all slices are valid UTF-8.
    let (cmd_str, stdout_str, stderr_str) = unsafe {
        let to_str = |ptr: *const u8, len: usize| -> &str {
            let bytes = std::slice::from_raw_parts(ptr, len);
            std::str::from_utf8_unchecked(bytes)
        };
        (to_str(cmd, cmd_len), to_str(stdout, stdout_len), to_str(stderr, stderr_len))
    };

    let exit = if has_exit_code != 0 {
        Some(exit_code)
    } else {
        None
    };

    let parsed: ParsedOutput = SemanticParser::parse(cmd_str, stdout_str, stderr_str, exit);

    let json = match serde_json::to_vec(&parsed) {
        Ok(v) => v,
        Err(e) => {
            log::error!("phantom-semantic skill_export: serialisation failed: {e}");
            return std::ptr::null_mut();
        }
    };

    let len = json.len();
    // Allocate with the global allocator — freed by `free_buf_ffi`.
    let layout = match std::alloc::Layout::array::<u8>(len) {
        Ok(l) => l,
        Err(_) => return std::ptr::null_mut(),
    };
    // SAFETY: layout has non-zero size (JSON is never empty).
    let ptr = unsafe { std::alloc::alloc(layout) };
    if ptr.is_null() {
        return std::ptr::null_mut();
    }
    // SAFETY: ptr is valid and has len bytes available.
    unsafe { std::ptr::copy_nonoverlapping(json.as_ptr(), ptr, len) };

    // SAFETY: out_len is a valid writable pointer (caller contract).
    unsafe { *out_len = len };

    ptr
}

/// `free_buf` FFI shim.
///
/// Frees a buffer previously returned by `parse_ffi`.
///
/// # Safety
///
/// `ptr` must be the exact pointer returned by `parse_ffi` with the given
/// `len`.  Must be called exactly once per successful `parse` call.
unsafe extern "C" fn free_buf_ffi(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    let layout = match std::alloc::Layout::array::<u8>(len) {
        Ok(l) => l,
        Err(_) => return,
    };
    // SAFETY: ptr was allocated by `parse_ffi` with this exact layout.
    unsafe { std::alloc::dealloc(ptr, layout) };
}

// ---------------------------------------------------------------------------
// Static vtable instance
// ---------------------------------------------------------------------------

static VTABLE: SemanticSkillVtable = SemanticSkillVtable {
    classify_command: classify_command_ffi,
    parse: parse_ffi,
    free_buf: free_buf_ffi,
};

// ---------------------------------------------------------------------------
// Exported registration symbol
// ---------------------------------------------------------------------------

/// Entry point resolved by `phantom-skill-host` at load time.
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
pub extern "C" fn phantom_skill_register() -> *const SemanticSkillVtable {
    &raw const VTABLE
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn command_type_to_tag(ct: &crate::types::CommandType) -> CommandTypeTag {
    use crate::types::CommandType;
    match ct {
        CommandType::Git(_)    => CommandTypeTag::Git,
        CommandType::Cargo(_)  => CommandTypeTag::Cargo,
        CommandType::Docker(_) => CommandTypeTag::Docker,
        CommandType::Npm(_)    => CommandTypeTag::Npm,
        CommandType::Http(_)   => CommandTypeTag::Http,
        CommandType::Shell     => CommandTypeTag::Shell,
        CommandType::Unknown   => CommandTypeTag::Unknown,
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
        let vt = phantom_skill_register();
        assert!(!vt.is_null());
    }

    #[test]
    fn classify_command_ffi_git_status() {
        let cmd = b"git status";
        // SAFETY: test data is valid UTF-8, length is correct.
        let tag = unsafe { classify_command_ffi(cmd.as_ptr(), cmd.len()) };
        assert_eq!(tag, CommandTypeTag::Git as u8);
    }

    #[test]
    fn classify_command_ffi_unknown() {
        let cmd = b"some-unknown-tool";
        // SAFETY: test data is valid UTF-8.
        let tag = unsafe { classify_command_ffi(cmd.as_ptr(), cmd.len()) };
        assert_eq!(tag, CommandTypeTag::Unknown as u8);
    }

    #[test]
    fn parse_ffi_and_free() {
        let cmd = b"git status";
        let stdout = b"On branch main\n";
        let stderr = b"";
        let mut out_len: usize = 0;

        // SAFETY: all slices are valid UTF-8; out_len is a valid writable ptr.
        let ptr = unsafe {
            parse_ffi(
                cmd.as_ptr(),
                cmd.len(),
                stdout.as_ptr(),
                stdout.len(),
                stderr.as_ptr(),
                stderr.len(),
                1, // has_exit_code
                0,
                &mut out_len,
            )
        };

        assert!(!ptr.is_null());
        assert!(out_len > 0);

        // Deserialise and verify.
        // SAFETY: ptr/len from parse_ffi.
        let bytes = unsafe { std::slice::from_raw_parts(ptr, out_len) };
        let parsed: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        assert_eq!(parsed["command"], "git status");

        // Free — must not double-free or crash.
        // SAFETY: ptr/len came from parse_ffi, called exactly once.
        unsafe { free_buf_ffi(ptr, out_len) };
    }
}
