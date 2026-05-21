//! C-ABI vtable definition shared between the host and every guest dylib.
//!
//! This is the **only** thing that crosses the dylib boundary.  Every field is
//! `extern "C"` and `#[repr(C)]`-stable.  Neither side may pass Rust trait
//! objects, generics, or `std::string::String` across this boundary.
//!
//! ## String convention
//!
//! * **Into the vtable**: callers pass `(ptr: *const u8, len: usize)`.  The
//!   implementation borrows the bytes for the duration of the call only.
//! * **Out of the vtable**: `classify_command` returns a `u8` discriminant
//!   (see [`CommandTypeTag`]).  `parse` serialises its result to JSON, writes
//!   the bytes into a heap allocation, returns `(*mut u8, usize)`.  The
//!   caller must free that allocation with `free_buf`.
//!
//! ## Lifetime
//!
//! The vtable is valid for as long as the `Library` that produced it is
//! loaded.  `SkillHost` ensures the `Arc<Library>` is kept alive in
//! `DylibBacked`.

/// Discriminant returned by `classify_command`.
///
/// Must stay in sync with [`phantom_semantic::CommandType`].
/// New variants may be appended; existing values must never be renumbered.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum CommandTypeTag {
    Unknown  = 0,
    Git      = 1,
    Cargo    = 2,
    Docker   = 3,
    Npm      = 4,
    Http     = 5,
    Shell    = 6,
}

/// ABI version stamped into every vtable.
///
/// Increment this constant whenever the layout of [`SemanticSkillVtable`]
/// changes in any way (field added, removed, reordered, or resized).
/// The loader rejects dylibs whose `abi_version` does not equal this value.
pub const CURRENT_ABI_VERSION: u32 = 1;

/// The C-ABI vtable exported by every skill dylib.
///
/// Obtain one by calling `phantom_skill_register()` in the dylib.
/// All function pointers must be non-null.
///
/// # Layout stability
///
/// `abi_version` is the **first** field so the loader can read it safely
/// before trusting any of the function-pointer fields.  Any layout change
/// requires bumping [`CURRENT_ABI_VERSION`].
#[repr(C)]
pub struct SemanticSkillVtable {
    /// ABI version written by the dylib at registration time.
    ///
    /// Must equal [`CURRENT_ABI_VERSION`]; the loader returns
    /// `LoadError::AbiVersionMismatch` otherwise.
    pub abi_version: u32,

    /// Return a `CommandTypeTag` discriminant for `cmd[..cmd_len]`.
    ///
    /// # Safety
    /// `cmd` must point to valid UTF-8 for at least `cmd_len` bytes and remain
    /// valid for the duration of the call.
    pub classify_command: unsafe extern "C" fn(
        cmd: *const u8,
        cmd_len: usize,
    ) -> u8,

    /// Full parse pipeline.  Returns a heap-allocated JSON-encoded
    /// `ParsedOutput`.  The caller must pass the returned `(*mut u8, usize)`
    /// to `free_buf` when done.
    ///
    /// # Safety
    /// All pointer/length pairs must reference valid UTF-8 for their respective
    /// lengths and remain valid for the duration of the call.
    /// `out_len` must be a valid writable pointer.
    pub parse: unsafe extern "C" fn(
        cmd: *const u8,
        cmd_len: usize,
        stdout: *const u8,
        stdout_len: usize,
        stderr: *const u8,
        stderr_len: usize,
        has_exit_code: u8,
        exit_code: i32,
        out_len: *mut usize,
    ) -> *mut u8,

    /// Free a buffer that was returned by `parse`.
    ///
    /// # Safety
    /// `ptr` must be the exact pointer previously returned by `parse` with the
    /// given `len`.  Must be called exactly once per `parse` call.
    pub free_buf: unsafe extern "C" fn(ptr: *mut u8, len: usize),
}

/// Symbol name exported by every skill dylib.
pub const REGISTER_SYMBOL: &[u8] = b"phantom_skill_register\0";
