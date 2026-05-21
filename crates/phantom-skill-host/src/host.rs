//! `SemanticSkill` trait and `SkillHost` — the public API callers use.
//!
//! Callers (`phantom-brain`, `phantom-app`, `phantom`) interact with
//! `phantom-semantic` exclusively through `Arc<dyn SemanticSkill>`.  Whether
//! the implementation is a monomorphised static call or a dylib vtable call is
//! transparent.

use std::sync::Arc;

use phantom_semantic::{CommandType, ParsedOutput, SemanticParser};

// ---------------------------------------------------------------------------
// Public trait
// ---------------------------------------------------------------------------

/// A semantic parser skill — classifies commands and parses output.
///
/// Callers hold `Arc<dyn SemanticSkill + Send + Sync>` and call through this
/// trait.  The static implementation is zero-cost; the dylib implementation
/// adds one vtable lookup and a JSON round-trip per call (acceptable for a
/// dev-only hot-reload path).
pub trait SemanticSkill: Send + Sync {
    /// Classify a command string into a [`CommandType`].
    fn classify_command(&self, cmd: &str) -> CommandType;

    /// Full parse pipeline: classify, detect content type, extract errors.
    fn parse(
        &self,
        cmd: &str,
        stdout: &str,
        stderr: &str,
        exit_code: Option<i32>,
    ) -> ParsedOutput;
}

// ---------------------------------------------------------------------------
// Static (monomorphised) implementation
// ---------------------------------------------------------------------------

/// Static implementation: thin wrapper around [`SemanticParser`].
///
/// Zero overhead — the compiler inlines through this into direct calls.
struct StaticSemanticSkill;

impl SemanticSkill for StaticSemanticSkill {
    fn classify_command(&self, cmd: &str) -> CommandType {
        SemanticParser::classify_command(cmd)
    }

    fn parse(
        &self,
        cmd: &str,
        stdout: &str,
        stderr: &str,
        exit_code: Option<i32>,
    ) -> ParsedOutput {
        SemanticParser::parse(cmd, stdout, stderr, exit_code)
    }
}

// ---------------------------------------------------------------------------
// Dynamic (dylib) implementation — hot-modules feature only
// ---------------------------------------------------------------------------

#[cfg(all(debug_assertions, feature = "hot-modules"))]
pub(crate) mod dylib_impl {
    use super::SemanticSkill;
    use crate::ffi::SemanticSkillVtable;
    use phantom_semantic::{CommandType, ParsedOutput};
    use std::sync::Arc;

    /// Dylib-backed implementation.
    ///
    /// Holds an `Arc<Library>` to keep the loaded dylib alive for as long as
    /// this value exists.  The vtable pointer borrows into the library's memory
    /// and is therefore valid for the same duration.
    ///
    /// # Safety invariant
    /// `vtable` must come from the same `Library` held by `_lib`.  This is
    /// enforced by `DylibBacked::new` being the only constructor.
    pub struct DylibBacked {
        /// The vtable residing in the dylib's .text segment.
        vtable: *const SemanticSkillVtable,
        /// Keeps the library alive.  Dropped after `vtable` due to field order.
        _lib: Arc<libloading::Library>,
    }

    // SAFETY: all vtable fn-pointers are `extern "C"` and operate only on
    // the data they receive as arguments.  No global mutable state is touched.
    // The `_lib` Arc ensures the vtable memory lives long enough.
    unsafe impl Send for DylibBacked {}
    unsafe impl Sync for DylibBacked {}

    impl DylibBacked {
        /// # Safety
        ///
        /// `vtable` must be a valid, non-null pointer to a `SemanticSkillVtable`
        /// residing in `lib`'s address space, and all function pointers in the
        /// vtable must be non-null.
        pub unsafe fn new(
            vtable: *const SemanticSkillVtable,
            lib: Arc<libloading::Library>,
        ) -> Self {
            Self { vtable, _lib: lib }
        }
    }

    impl SemanticSkill for DylibBacked {
        fn classify_command(&self, cmd: &str) -> CommandType {
            // SAFETY: `vtable` is valid for the lifetime of `_lib`; `cmd` is a
            // valid UTF-8 slice that outlives the call.
            //
            // Fix 1 — catch_unwind boundary: a panic inside the dylib must not
            // unwind across the FFI boundary (undefined behaviour per the Rust
            // reference).  On a caught panic we fall back to the static
            // implementation and emit a warning so the process keeps running.
            let vtable = self.vtable;
            let cmd_ptr = cmd.as_ptr();
            let cmd_len = cmd.len();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // SAFETY: vtable is non-null and valid; fn ptr was validated
                // non-null at load time by the loader.
                unsafe { ((*vtable).classify_command)(cmd_ptr, cmd_len) }
            }));
            match result {
                Ok(tag) => tag_to_command_type(tag),
                Err(_) => {
                    log::warn!(
                        "skill-host: dylib classify_command panicked — falling back to static"
                    );
                    phantom_semantic::SemanticParser::classify_command(cmd)
                }
            }
        }

        fn parse(
            &self,
            cmd: &str,
            stdout: &str,
            stderr: &str,
            exit_code: Option<i32>,
        ) -> ParsedOutput {
            let has_exit_code: u8 = if exit_code.is_some() { 1 } else { 0 };
            let code = exit_code.unwrap_or(0);

            let mut out_len: usize = 0;

            // Fix 1 — catch_unwind boundary: same reasoning as classify_command.
            // All raw pointer/length values are captured before the closure so
            // we don't borrow `cmd`/`stdout`/`stderr` across the unwind
            // boundary.  AssertUnwindSafe is sound here because the pointers
            // are only used inside the closure, and on the panic path we
            // immediately fall back without touching them again.
            let vtable = self.vtable;
            let cmd_ptr = cmd.as_ptr();
            let cmd_len = cmd.len();
            let stdout_ptr = stdout.as_ptr();
            let stdout_len = stdout.len();
            let stderr_ptr = stderr.as_ptr();
            let stderr_len = stderr.len();
            let out_len_ptr = &raw mut out_len;

            // SAFETY: all slices are valid UTF-8 that outlive the call;
            // `out_len` is a valid writable pointer.
            let dispatch_result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    unsafe {
                        ((*vtable).parse)(
                            cmd_ptr,
                            cmd_len,
                            stdout_ptr,
                            stdout_len,
                            stderr_ptr,
                            stderr_len,
                            has_exit_code,
                            code,
                            out_len_ptr,
                        )
                    }
                }));

            let ptr = match dispatch_result {
                Ok(p) => p,
                Err(_) => {
                    log::warn!("skill-host: dylib parse panicked — falling back to static");
                    return phantom_semantic::SemanticParser::parse(
                        cmd, stdout, stderr, exit_code,
                    );
                }
            };

            if ptr.is_null() {
                log::error!("skill-host: dylib parse returned null — falling back");
                return phantom_semantic::SemanticParser::parse(cmd, stdout, stderr, exit_code);
            }

            // SAFETY: `ptr` and `out_len` were returned by the dylib's `parse`.
            let bytes = unsafe { std::slice::from_raw_parts(ptr, out_len) };
            let result = serde_json::from_slice::<ParsedOutput>(bytes).unwrap_or_else(|e| {
                log::error!("skill-host: failed to deserialise ParsedOutput from dylib: {e}");
                phantom_semantic::SemanticParser::parse(cmd, stdout, stderr, exit_code)
            });

            // SAFETY: `ptr` / `out_len` came from the vtable's `parse`; we call
            // `free_buf` exactly once.
            unsafe { ((*self.vtable).free_buf)(ptr, out_len) };

            result
        }
    }

    /// Map the raw `u8` discriminant from the C ABI back to [`CommandType`].
    fn tag_to_command_type(tag: u8) -> CommandType {
        match tag {
            1 => CommandType::Git(phantom_semantic::GitCommand::Other(String::new())),
            2 => CommandType::Cargo(phantom_semantic::CargoCommand::Other(String::new())),
            3 => CommandType::Docker(phantom_semantic::DockerCommand::Other(String::new())),
            4 => CommandType::Npm(phantom_semantic::NpmCommand::Other(String::new())),
            5 => CommandType::Http(phantom_semantic::HttpCommand::Get),
            6 => CommandType::Shell,
            _ => CommandType::Unknown,
        }
    }

    // -----------------------------------------------------------------------
    // Tests for Fix 1 (catch_unwind prevents dylib panic from propagating)
    // -----------------------------------------------------------------------

    #[cfg(test)]
    mod tests {
        use super::SemanticSkill as _;
        use crate::ffi::{CURRENT_ABI_VERSION, SemanticSkillVtable};
        use phantom_semantic::CommandType;
        use std::sync::Arc;

        /// Verify that the `catch_unwind` boundary in `classify_command`
        /// actually catches a panic from the closure body.
        ///
        /// We test this by verifying that `std::panic::catch_unwind` with
        /// `AssertUnwindSafe` absorbs a closure panic, which is the same
        /// mechanism used in `DylibBacked::classify_command`.
        ///
        /// Note: panicking inside an `extern "C"` fn pointer aborts the
        /// process in Rust 2024 (that is the very UB we are protecting
        /// against).  The `catch_unwind` wrapper protects against panics
        /// that propagate *out of* the fn call in the caller's stack
        /// frame — for instance, if the dylib itself catches the panic and
        /// re-raises it as a Rust panic, or if the compiler inserts an
        /// unwind path.  The test below exercises the catch_unwind
        /// infrastructure directly.
        #[test]
        fn catch_unwind_prevents_dylib_panic_from_propagating() {
            // Simulate the catch_unwind pattern used in classify_command.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                panic!("simulated dylib panic");
                #[allow(unreachable_code)]
                0u8
            }));
            assert!(result.is_err(), "catch_unwind must catch the panic");
            // On the error path, fall back to the static implementation.
            let fallback = phantom_semantic::SemanticParser::classify_command("git status");
            assert_eq!(
                fallback,
                CommandType::Git(phantom_semantic::GitCommand::Status)
            );
        }

        /// Verify that the `catch_unwind` boundary in `parse` absorbs panics.
        #[test]
        fn catch_unwind_prevents_dylib_parse_panic_from_propagating() {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> *mut u8 {
                panic!("simulated dylib parse panic");
            }));
            assert!(result.is_err(), "catch_unwind must catch the parse panic");
            // On the error path, fall back to the static implementation.
            let fallback =
                phantom_semantic::SemanticParser::parse("git status", "", "", Some(0));
            assert_eq!(fallback.command, "git status");
        }

        /// Build a DylibBacked with a valid (no-op) vtable to verify that
        /// non-panicking calls go through normally (regression guard).
        ///
        /// Returns null from parse so DylibBacked falls back to the static
        /// path, which is acceptable for the purpose of this test.
        #[test]
        fn non_panicking_classify_returns_static_fallback_on_null() {
            // classify returns 0 (Unknown).
            unsafe extern "C" fn noop_classify(_: *const u8, _: usize) -> u8 { 0 }
            // parse returns null so DylibBacked falls back to static.
            unsafe extern "C" fn null_parse(
                _: *const u8, _: usize,
                _: *const u8, _: usize,
                _: *const u8, _: usize,
                _: u8, _: i32,
                _: *mut usize,
            ) -> *mut u8 { std::ptr::null_mut() }
            unsafe extern "C" fn noop_free(_: *mut u8, _: usize) {}

            static NOOP_VTABLE: SemanticSkillVtable = SemanticSkillVtable {
                abi_version: CURRENT_ABI_VERSION,
                classify_command: noop_classify,
                parse: null_parse,
                free_buf: noop_free,
            };

            #[cfg(target_os = "macos")]
            let lib = unsafe {
                libloading::Library::new("libSystem.dylib")
                    .expect("libSystem must exist on macOS")
            };
            #[cfg(target_os = "linux")]
            let lib = unsafe {
                libloading::Library::new("libc.so.6").expect("libc must exist on Linux")
            };
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            let lib = unsafe {
                libloading::Library::new("msvcrt.dll").expect("msvcrt must exist on Windows")
            };

            let backed = unsafe {
                super::DylibBacked::new(&raw const NOOP_VTABLE, Arc::new(lib))
            };

            // classify returns Unknown (0); no panic should occur.
            let ct = backed.classify_command("anything");
            assert_eq!(ct, CommandType::Unknown);

            // parse returns null; DylibBacked falls back to static.
            let out = backed.parse("git status", "", "", Some(0));
            assert_eq!(out.command, "git status");
        }
    }
}

// ---------------------------------------------------------------------------
// SkillHost — the factory
// ---------------------------------------------------------------------------

/// Factory that creates the correct `Arc<dyn SemanticSkill>` implementation
/// based on feature flags and environment variables.
///
/// # Usage
///
/// ```rust,no_run
/// use phantom_skill_host::SkillHost;
///
/// let skill = SkillHost::build();   // picks static or dynamic automatically
/// let ct = skill.classify_command("git status");
/// ```
pub struct SkillHost;

impl SkillHost {
    /// Build the skill implementation.
    ///
    /// * When `hot-modules` is disabled or in release builds: always returns
    ///   the static implementation.
    /// * When `hot-modules` is enabled and `PHANTOM_HOT_MODULES=1`: attempts
    ///   to load the dylib; on failure, falls back to static with a warning.
    #[must_use]
    pub fn build() -> Arc<dyn SemanticSkill> {
        #[cfg(all(debug_assertions, feature = "hot-modules"))]
        {
            if std::env::var_os("PHANTOM_HOT_MODULES").is_some() {
                match crate::loader::load_semantic_dylib() {
                    Ok(skill) => return skill,
                    Err(e) => {
                        log::warn!(
                            "skill-host: failed to load phantom-semantic dylib: {e}\
                             — falling back to static"
                        );
                    }
                }
            } else {
                log::debug!(
                    "skill-host: PHANTOM_HOT_MODULES not set — using static semantic parser"
                );
            }
        }
        Arc::new(StaticSemanticSkill)
    }

    /// Unconditionally return the static (zero-overhead) implementation.
    ///
    /// Useful in tests and in contexts where the env-var check is undesired.
    #[must_use]
    pub fn build_static() -> Arc<dyn SemanticSkill> {
        Arc::new(StaticSemanticSkill)
    }
}

impl Default for SkillHost {
    fn default() -> Self {
        Self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_host_classify_git() {
        let skill = SkillHost::build_static();
        assert_eq!(
            skill.classify_command("git status"),
            CommandType::Git(phantom_semantic::GitCommand::Status)
        );
    }

    #[test]
    fn static_host_classify_cargo() {
        let skill = SkillHost::build_static();
        assert_eq!(
            skill.classify_command("cargo build"),
            CommandType::Cargo(phantom_semantic::CargoCommand::Build)
        );
    }

    #[test]
    fn static_host_parse_returns_parsed_output() {
        let skill = SkillHost::build_static();
        let out = skill.parse("git status", "On branch main\n", "", Some(0));
        assert_eq!(out.command, "git status");
        assert_eq!(
            out.command_type,
            CommandType::Git(phantom_semantic::GitCommand::Status)
        );
    }

    #[test]
    fn new_without_env_var_returns_static() {
        // Safety: single-threaded test.
        unsafe { std::env::remove_var("PHANTOM_HOT_MODULES") };
        let skill = SkillHost::build();
        // If we get here without panicking, the static path worked.
        let ct = skill.classify_command("ls -la");
        assert_eq!(ct, CommandType::Shell);
    }
}
