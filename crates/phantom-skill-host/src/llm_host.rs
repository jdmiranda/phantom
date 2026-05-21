//! `LlmSkill` trait and `LlmHost` — the public API callers use for LLM dispatch.
//!
//! Callers (`phantom-app`) interact with `phantom-nlp`'s LLM backend
//! exclusively through `Arc<dyn LlmSkill>`.  Whether the implementation is a
//! monomorphised static call or a dylib vtable call is transparent.

use std::sync::Arc;

use phantom_nlp::{ClaudeLlmBackend, LlmBackend, OllamaLlmBackend, TranslateError};

// ---------------------------------------------------------------------------
// Public trait
// ---------------------------------------------------------------------------

/// An LLM completion skill — sends a system prompt + user message and returns
/// the assistant reply.
///
/// Callers hold `Arc<dyn LlmSkill + Send + Sync>` and call through this trait.
/// The static implementation delegates to `phantom_nlp`'s `LlmBackend`; the
/// dylib implementation adds one vtable lookup and a heap copy per call
/// (acceptable for the dev-only hot-reload path).
pub trait LlmSkill: Send + Sync {
    /// Stable name for logging (e.g. `"claude"`, `"ollama"`, `"phantom-nlp"`).
    fn name(&self) -> &str;

    /// Send `system_prompt` + `user_message` to the backend.
    ///
    /// Returns the assistant reply on success, or a [`TranslateError`] on
    /// network / configuration failure.
    fn complete(
        &self,
        system_prompt: &str,
        user_message: &str,
    ) -> Result<String, TranslateError>;
}

// ---------------------------------------------------------------------------
// Static implementation — wraps `Arc<dyn LlmBackend>`
// ---------------------------------------------------------------------------

/// Static implementation: thin wrapper around `Arc<dyn LlmBackend>`.
///
/// Zero overhead over calling `LlmBackend::complete` directly.
struct StaticLlmSkill {
    backend: Arc<dyn LlmBackend + Send + Sync>,
}

impl LlmSkill for StaticLlmSkill {
    fn name(&self) -> &str {
        self.backend.name()
    }

    fn complete(
        &self,
        system_prompt: &str,
        user_message: &str,
    ) -> Result<String, TranslateError> {
        self.backend.complete(system_prompt, user_message)
    }
}

// ---------------------------------------------------------------------------
// Dynamic (dylib) implementation — hot-modules feature only
// ---------------------------------------------------------------------------

#[cfg(all(debug_assertions, feature = "hot-modules"))]
pub(crate) mod dylib_impl {
    use super::{LlmSkill, TranslateError};
    use crate::llm_ffi::LlmSkillVtable;
    use std::sync::Arc;

    /// Dylib-backed LLM skill.
    ///
    /// Holds an `Arc<Library>` to keep the loaded dylib alive for as long as
    /// this value exists.  The vtable pointer borrows into the library's memory
    /// and is therefore valid for the same duration.
    ///
    /// # Safety invariant
    /// `vtable` must come from the same `Library` held by `_lib`.  This is
    /// enforced by `DylibBackedLlm::new` being the only constructor.
    pub struct DylibBackedLlm {
        /// The vtable residing in the dylib's .text / .rodata segment.
        vtable: *const LlmSkillVtable,
        /// Keeps the library alive.  Dropped after `vtable` due to field order.
        _lib: Arc<libloading::Library>,
    }

    // SAFETY: all vtable fn-pointers are `extern "C"` and operate only on
    // the data passed as arguments.  No global mutable state is touched.
    // The `_lib` Arc ensures the vtable memory lives long enough.
    unsafe impl Send for DylibBackedLlm {}
    unsafe impl Sync for DylibBackedLlm {}

    impl DylibBackedLlm {
        /// # Safety
        ///
        /// `vtable` must be a valid, non-null pointer to a `LlmSkillVtable`
        /// residing in `lib`'s address space, and all function pointers in the
        /// vtable must be non-null.
        pub unsafe fn new(vtable: *const LlmSkillVtable, lib: Arc<libloading::Library>) -> Self {
            Self { vtable, _lib: lib }
        }
    }

    impl LlmSkill for DylibBackedLlm {
        fn name(&self) -> &str {
            // SAFETY: `vtable` is valid for the lifetime of `_lib`; the
            // returned pointer is a null-terminated static string in the dylib.
            let ptr = unsafe { ((*self.vtable).name)() };
            if ptr.is_null() {
                return "phantom-nlp";
            }
            // SAFETY: ptr is a null-terminated static C string in the dylib.
            let cstr = unsafe { std::ffi::CStr::from_ptr(ptr as *const std::ffi::c_char) };
            // CStr::to_str returns Err only on non-UTF-8; our dylib always
            // exports ASCII.  Fallback keeps us infallible.
            cstr.to_str().unwrap_or("phantom-nlp")
        }

        fn complete(
            &self,
            system_prompt: &str,
            user_message: &str,
        ) -> Result<String, TranslateError> {
            let mut out_ptr: *mut u8 = std::ptr::null_mut();
            let mut out_len: usize = 0;
            let mut out_err: u8 = 0;

            // SAFETY: all string slices are valid UTF-8 that outlive the call;
            // out_ptr / out_len / out_err are valid writable stack variables.
            unsafe {
                ((*self.vtable).complete)(
                    system_prompt.as_ptr(),
                    system_prompt.len(),
                    user_message.as_ptr(),
                    user_message.len(),
                    &mut out_ptr,
                    &mut out_len,
                    &mut out_err,
                );
            }

            // Parse the result.
            let result = parse_result(out_ptr, out_len, out_err);

            // Free the buffer in all cases (success or error).
            // SAFETY: out_ptr came from the dylib's complete; free_buf is called exactly once.
            // free_buf_ffi handles len == 0 correctly (uses len.max(1) layout to match alloc_buf).
            if !out_ptr.is_null() {
                unsafe { ((*self.vtable).free_buf)(out_ptr, out_len) };
            }

            result
        }
    }

    /// Decode the out-params written by `complete_ffi` into a `Result`.
    fn parse_result(
        out_ptr: *mut u8,
        out_len: usize,
        out_err: u8,
    ) -> Result<String, TranslateError> {
        let msg = if out_ptr.is_null() || out_len == 0 {
            String::new()
        } else {
            // SAFETY: out_ptr / out_len came from the dylib's complete fn;
            // we copy the bytes before calling free_buf.
            let bytes = unsafe { std::slice::from_raw_parts(out_ptr, out_len) };
            // Checked UTF-8 — non-UTF-8 becomes a lossy replacement.
            String::from_utf8_lossy(bytes).into_owned()
        };

        match out_err {
            0 => Ok(msg),
            1 => Err(TranslateError::NotConfigured(msg)),
            2 => Err(TranslateError::Transport(msg)),
            _ => Err(TranslateError::Other(msg)),
        }
    }
}

// ---------------------------------------------------------------------------
// LlmHost — the factory
// ---------------------------------------------------------------------------

/// Factory that creates the correct `Arc<dyn LlmSkill>` implementation based
/// on feature flags and environment variables.
///
/// # Usage
///
/// ```rust,no_run
/// use phantom_skill_host::LlmHost;
///
/// // Returns None when no backend is configured.
/// if let Some(skill) = LlmHost::build() {
///     let reply = skill.complete("You are helpful.", "hello");
/// }
/// ```
pub struct LlmHost;

impl LlmHost {
    /// Build the LLM skill implementation.
    ///
    /// * When `hot-modules` is disabled or in release builds: always returns
    ///   the static implementation, using the same Claude → Ollama priority
    ///   as the previous direct construction in `phantom-app`.
    /// * When `hot-modules` is enabled and `PHANTOM_HOT_MODULES=1`: attempts
    ///   to load the dylib; on failure, falls back to static with a warning.
    ///
    /// Returns `None` when no backend is available (same as the old
    /// `nlp_backend: Option<Arc<dyn LlmBackend>>`).
    #[must_use]
    pub fn build() -> Option<Arc<dyn LlmSkill>> {
        #[cfg(all(debug_assertions, feature = "hot-modules"))]
        {
            if std::env::var_os("PHANTOM_HOT_MODULES").is_some() {
                match crate::llm_loader::load_llm_dylib() {
                    Ok(skill) => {
                        log::info!("skill-host: phantom-nlp LLM dylib loaded");
                        return Some(skill);
                    }
                    Err(e) => {
                        log::warn!(
                            "skill-host: failed to load phantom-nlp dylib: {e} \
                             — falling back to static LLM path"
                        );
                    }
                }
            } else {
                log::debug!(
                    "skill-host: PHANTOM_HOT_MODULES not set — using static LLM backend"
                );
            }
        }

        Self::build_static()
    }

    /// Build the static (monomorphised) LLM skill.
    ///
    /// Priority: `ClaudeLlmBackend` (cloud) → `OllamaLlmBackend` (local, if
    /// reachable) → `None`.  Mirrors the selection logic previously in
    /// `phantom-app::App::build_nlp_backend`.
    #[must_use]
    pub fn build_static() -> Option<Arc<dyn LlmSkill>> {
        // 1. Claude (reads ANTHROPIC_API_KEY from the environment).
        match ClaudeLlmBackend::from_env() {
            Ok(backend) => {
                log::info!("skill-host: NLP LLM backend: ClaudeLlmBackend (key present)");
                return Some(Arc::new(StaticLlmSkill {
                    backend: Arc::new(backend),
                }));
            }
            Err(TranslateError::NotConfigured(_)) => {
                log::debug!("skill-host: Claude unavailable, probing Ollama");
            }
            Err(e) => {
                log::warn!("skill-host: unexpected Claude init error: {e}");
            }
        }

        // 2. Ollama if the daemon is running.
        let ollama = OllamaLlmBackend::from_env();
        if ollama.is_available() {
            log::info!(
                "skill-host: NLP LLM backend: OllamaLlmBackend ({})",
                ollama.model()
            );
            return Some(Arc::new(StaticLlmSkill {
                backend: Arc::new(ollama),
            }));
        }

        log::debug!("skill-host: no LLM backend available (Claude key absent, Ollama not reachable)");
        None
    }

    /// Unconditionally return the static implementation for a given backend.
    ///
    /// Useful in tests that inject a `MockLlmBackend`.
    #[must_use]
    pub fn from_backend(backend: Arc<dyn LlmBackend + Send + Sync>) -> Arc<dyn LlmSkill> {
        Arc::new(StaticLlmSkill { backend })
    }
}

impl Default for LlmHost {
    fn default() -> Self {
        Self
    }
}

// ---------------------------------------------------------------------------
// LlmSkillAdapter — bridges `LlmSkill` → `LlmBackend`
// ---------------------------------------------------------------------------

/// Wraps an `Arc<dyn LlmSkill>` so it can be used anywhere a `&dyn LlmBackend`
/// is expected — notably in `phantom_nlp::translate()`.
///
/// This avoids requiring `phantom-app` to hold two separate Arc types or
/// duplicating the translate logic.
pub struct LlmSkillAdapter(Arc<dyn LlmSkill>);

impl LlmSkillAdapter {
    /// Wrap an `Arc<dyn LlmSkill>`.
    pub fn new(skill: Arc<dyn LlmSkill>) -> Self {
        Self(skill)
    }

    /// Borrow the inner skill.
    #[must_use]
    pub fn inner(&self) -> &Arc<dyn LlmSkill> {
        &self.0
    }
}

impl phantom_nlp::LlmBackend for LlmSkillAdapter {
    fn name(&self) -> &'static str {
        // `LlmSkill::name` returns `&str` (not `&'static str`) because the
        // dylib path reads from the C string at runtime.  We map to a static
        // fallback here; the name is used only for logging.
        "skill-host"
    }

    fn complete(
        &self,
        system_prompt: &str,
        user_message: &str,
    ) -> Result<String, phantom_nlp::TranslateError> {
        self.0.complete(system_prompt, user_message)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use phantom_nlp::MockLlmBackend;

    #[test]
    fn static_host_from_mock_name() {
        let backend: Arc<dyn LlmBackend + Send + Sync> =
            Arc::new(MockLlmBackend::new("{}"));
        let skill = LlmHost::from_backend(backend);
        assert_eq!(skill.name(), "mock");
    }

    #[test]
    fn static_host_from_mock_complete() {
        let backend: Arc<dyn LlmBackend + Send + Sync> =
            Arc::new(MockLlmBackend::new("hello from mock"));
        let skill = LlmHost::from_backend(backend);
        let reply = skill.complete("system", "user").unwrap();
        assert_eq!(reply, "hello from mock");
    }

    #[test]
    fn build_without_env_var_does_not_panic() {
        // PHANTOM_HOT_MODULES not set → static path → None or Some depending on env.
        // Either outcome is valid; the test just verifies no panic.
        unsafe { std::env::remove_var("PHANTOM_HOT_MODULES") };
        let _ = LlmHost::build();
    }
}
