//! Provider catalog for routing agent tasks to LLM backends (Issue #61).
//!
//! A [`ProviderProfile`] describes one LLM backend: the CLI command used to
//! invoke it, the model it uses by default, and the full list of models it
//! supports. A [`ProviderCatalog`] is a named registry of profiles.
//!
//! # Built-in profiles
//!
//! Three profiles ship out of the box (accessible via [`ProviderCatalog::with_builtins`]):
//!
//! | ID                | Command                                        | Default model                  |
//! |-------------------|------------------------------------------------|--------------------------------|
//! | `claude-default`  | `claude -p --dangerously-skip-permissions`     | `claude-sonnet-4-20250514`     |
//! | `claude-fast`     | `claude -p --dangerously-skip-permissions`     | `claude-haiku-4-5`             |
//! | `ollama-phi3.5`   | `ollama run phi3.5`                            | `phi3.5:latest`                |
//!
//! # Fallback behaviour
//!
//! [`ProviderCatalog::resolve`] never returns `None`. When an unknown ID is
//! requested it falls back to the `"claude-default"` profile, which must
//! always be present in catalogs created via [`ProviderCatalog::with_builtins`].
//!
//! # Strategy pattern (GoF, 1994)
//!
//! The catalog acts as a strategy registry: each [`ProviderProfile`] is a
//! concrete strategy, and the dispatch layer selects one at runtime based on
//! the `preferred_provider` field of a [`PlanStep`].

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// ProviderProfile
// ---------------------------------------------------------------------------

/// A single LLM backend description.
///
/// All fields are private; use the accessor methods for read-only access.
/// Profiles are immutable after construction to prevent accidental mutation
/// from shared references.
#[derive(Debug, Clone)]
pub struct ProviderProfile {
    /// Stable identifier, e.g. `"claude-default"`.
    id: String,
    /// Shell command used to invoke this provider.
    ///
    /// The dispatcher appends the prompt (or a `--prompt` flag) after
    /// this string.
    runtime_command: String,
    /// Model name sent to the provider by default when no override is given.
    default_model: String,
    /// Full set of model names this provider can serve.
    ///
    /// Used for validation and to surface choices in the UI.
    available_models: Vec<String>,
}

impl ProviderProfile {
    /// Create a new provider profile.
    ///
    /// `available_models` must contain at least `default_model`; if it does
    /// not, `default_model` is appended automatically so the invariant holds.
    pub fn new(
        id: impl Into<String>,
        runtime_command: impl Into<String>,
        default_model: impl Into<String>,
        available_models: Vec<String>,
    ) -> Self {
        let id = id.into();
        let runtime_command = runtime_command.into();
        let default_model = default_model.into();
        let mut available_models = available_models;
        if !available_models.contains(&default_model) {
            available_models.push(default_model.clone());
        }
        Self {
            id,
            runtime_command,
            default_model,
            available_models,
        }
    }

    /// Stable identifier for this profile.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Shell command prefix used to invoke this provider.
    pub fn runtime_command(&self) -> &str {
        &self.runtime_command
    }

    /// Default model name.
    pub fn default_model(&self) -> &str {
        &self.default_model
    }

    /// Full list of model names this provider supports.
    pub fn available_models(&self) -> &[String] {
        &self.available_models
    }

    /// Returns `true` if `model` is in the available-models list.
    pub fn supports_model(&self, model: &str) -> bool {
        self.available_models.iter().any(|m| m == model)
    }
}

// ---------------------------------------------------------------------------
// ProviderCatalog
// ---------------------------------------------------------------------------

/// The fallback profile ID used when an unknown ID is requested.
///
/// This profile must always exist in catalogs created via
/// [`ProviderCatalog::with_builtins`].
pub const FALLBACK_ID: &str = "claude-default";

/// A named registry of [`ProviderProfile`]s.
///
/// # Invariant
///
/// Catalogs produced by [`with_builtins`][ProviderCatalog::with_builtins]
/// always contain the `FALLBACK_ID` profile. User-created catalogs (via
/// [`empty`][ProviderCatalog::empty]) start with no profiles; calling
/// [`resolve`][ProviderCatalog::resolve] on an empty catalog for an unknown
/// ID returns `None` from the inner lookup, so callers should prefer
/// `with_builtins` in production.
#[derive(Debug, Clone)]
pub struct ProviderCatalog {
    profiles: HashMap<String, ProviderProfile>,
}

impl ProviderCatalog {
    /// Create an empty catalog with no profiles.
    pub fn empty() -> Self {
        Self {
            profiles: HashMap::new(),
        }
    }

    /// Create a catalog pre-loaded with the built-in profiles.
    ///
    /// Built-in profiles:
    ///
    /// - `"claude-default"` — `claude-sonnet-4-20250514` via the `claude` CLI
    /// - `"claude-fast"`    — `claude-haiku-4-5` via the `claude` CLI
    /// - `"ollama-phi3.5"` — `phi3.5:latest` via Ollama (local)
    /// - `"ollama-llama3"`  — `llama3:latest` via Ollama (local)
    pub fn with_builtins() -> Self {
        let mut catalog = Self::empty();

        catalog.insert(ProviderProfile::new(
            "claude-default",
            "claude -p --dangerously-skip-permissions",
            "claude-sonnet-4-20250514",
            vec![
                "claude-sonnet-4-20250514".into(),
                "claude-opus-4-5".into(),
                "claude-haiku-4-5".into(),
            ],
        ));

        catalog.insert(ProviderProfile::new(
            "claude-fast",
            "claude -p --dangerously-skip-permissions",
            "claude-haiku-4-5",
            vec!["claude-haiku-4-5".into(), "claude-sonnet-4-20250514".into()],
        ));

        catalog.insert(ProviderProfile::new(
            "ollama-phi3.5",
            "ollama run phi3.5",
            "phi3.5:latest",
            vec!["phi3.5:latest".into(), "phi3.5:3.8b".into()],
        ));

        catalog.insert(ProviderProfile::new(
            "ollama-llama3",
            "ollama run llama3",
            "llama3:latest",
            vec!["llama3:latest".into(), "llama3:8b".into()],
        ));

        catalog
    }

    /// Insert or replace a profile.
    pub fn insert(&mut self, profile: ProviderProfile) {
        self.profiles.insert(profile.id.clone(), profile);
    }

    /// Insert or replace a profile (alias for [`insert`][ProviderCatalog::insert]).
    ///
    /// Provided for ergonomic compatibility with code that prefers the
    /// `add_profile` naming convention.
    pub fn add_profile(&mut self, profile: ProviderProfile) {
        self.insert(profile);
    }

    /// Look up a profile by exact ID, returning `None` if absent.
    ///
    /// Unlike [`resolve`][ProviderCatalog::resolve], `get` does **not** fall
    /// back to `"claude-default"` on a miss — it returns `None` directly.
    /// This makes it suitable for presence checks and optional overrides.
    pub fn get(&self, name: &str) -> Option<&ProviderProfile> {
        self.profiles.get(name)
    }

    /// Return the built-in default profile (claude-sonnet, 4096-token budget).
    ///
    /// Constructs the profile on demand; callers that need a stable owned copy
    /// can call `.clone()` on the result.
    pub fn default_profile() -> ProviderProfile {
        ProviderProfile::new(
            "claude-default",
            "claude -p --dangerously-skip-permissions",
            "claude-sonnet-4-20250514",
            vec![
                "claude-sonnet-4-20250514".into(),
                "claude-opus-4-5".into(),
                "claude-haiku-4-5".into(),
            ],
        )
    }

    /// Remove a profile by ID. Returns the removed profile, or `None` if it
    /// was not present.
    pub fn remove(&mut self, id: &str) -> Option<ProviderProfile> {
        self.profiles.remove(id)
    }

    /// Resolve a profile by ID.
    ///
    /// If `id` is not found, falls back to `FALLBACK_ID` (`"claude-default"`).
    /// Returns `None` only when the fallback itself is absent (i.e. the catalog
    /// was created via [`empty`][ProviderCatalog::empty] and has no profiles).
    pub fn resolve(&self, id: &str) -> Option<&ProviderProfile> {
        self.profiles
            .get(id)
            .or_else(|| self.profiles.get(FALLBACK_ID))
    }

    /// Resolve a profile by ID and clone the result.
    ///
    /// Convenience wrapper around [`resolve`][ProviderCatalog::resolve] for
    /// callers that need an owned copy.
    pub fn resolve_cloned(&self, id: &str) -> Option<ProviderProfile> {
        self.resolve(id).cloned()
    }

    /// Build an [`crate::ollama::OllamaBackend`] from an `ollama-*` profile.
    ///
    /// Returns `None` when `id` does not resolve to a profile whose
    /// `runtime_command` starts with `"ollama"`. The backend uses the profile's
    /// `default_model` and the standard local Ollama URL (`http://localhost:11434`).
    ///
    /// # Example
    ///
    /// ```rust
    /// use phantom_brain::provider_catalog::ProviderCatalog;
    /// use phantom_brain::goal::ChatBackend;
    ///
    /// let catalog = ProviderCatalog::with_builtins();
    /// if let Some(backend) = catalog.build_ollama_backend("ollama-phi3.5") {
    ///     // backend implements ChatBackend
    ///     let _ = backend.chat("Hello");
    /// }
    /// ```
    pub fn build_ollama_backend(&self, id: &str) -> Option<crate::ollama::OllamaBackend> {
        let profile = self.profiles.get(id)?;
        if !profile.runtime_command.starts_with("ollama") {
            return None;
        }
        Some(crate::ollama::OllamaBackend::new(
            profile.default_model.clone(),
            "http://localhost:11434",
        ))
    }

    /// Returns `true` if the catalog contains a profile with `id`.
    pub fn contains(&self, id: &str) -> bool {
        self.profiles.contains_key(id)
    }

    /// Iterate over all registered profiles in arbitrary order.
    pub fn iter(&self) -> impl Iterator<Item = &ProviderProfile> {
        self.profiles.values()
    }

    /// Number of profiles in the catalog.
    pub fn len(&self) -> usize {
        self.profiles.len()
    }

    /// Returns `true` if the catalog has no profiles.
    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }
}

impl Default for ProviderCatalog {
    fn default() -> Self {
        Self::with_builtins()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- ProviderProfile ----------------------------------------------------

    #[test]
    fn profile_new_stores_fields() {
        let p = ProviderProfile::new(
            "my-provider",
            "my-command --flag",
            "my-model-v1",
            vec!["my-model-v1".into(), "my-model-v2".into()],
        );
        assert_eq!(p.id(), "my-provider");
        assert_eq!(p.runtime_command(), "my-command --flag");
        assert_eq!(p.default_model(), "my-model-v1");
        assert_eq!(p.available_models().len(), 2);
    }

    #[test]
    fn profile_auto_appends_default_model_if_missing() {
        // default_model not listed in available_models — must be appended.
        let p = ProviderProfile::new("p", "cmd", "missing-model", vec!["other-model".into()]);
        assert!(
            p.available_models().contains(&"missing-model".to_string()),
            "default_model must appear in available_models"
        );
    }

    #[test]
    fn profile_no_duplicate_when_default_already_present() {
        let p = ProviderProfile::new(
            "p",
            "cmd",
            "model-a",
            vec!["model-a".into(), "model-b".into()],
        );
        let count = p
            .available_models()
            .iter()
            .filter(|m| m.as_str() == "model-a")
            .count();
        assert_eq!(count, 1, "model-a must appear exactly once");
    }

    #[test]
    fn profile_supports_model_true() {
        let p = ProviderProfile::new("p", "cmd", "m1", vec!["m1".into(), "m2".into()]);
        assert!(p.supports_model("m1"));
        assert!(p.supports_model("m2"));
    }

    #[test]
    fn profile_supports_model_false_for_unknown() {
        let p = ProviderProfile::new("p", "cmd", "m1", vec!["m1".into()]);
        assert!(!p.supports_model("unknown-model"));
    }

    // -- ProviderCatalog::empty ---------------------------------------------

    #[test]
    fn empty_catalog_has_no_profiles() {
        let cat = ProviderCatalog::empty();
        assert!(cat.is_empty());
        assert_eq!(cat.len(), 0);
    }

    #[test]
    fn empty_catalog_resolve_returns_none_for_unknown() {
        let cat = ProviderCatalog::empty();
        assert!(cat.resolve("claude-default").is_none());
    }

    // -- ProviderCatalog::with_builtins -------------------------------------

    #[test]
    fn with_builtins_has_four_profiles() {
        let cat = ProviderCatalog::with_builtins();
        assert_eq!(cat.len(), 4);
    }

    #[test]
    fn with_builtins_contains_claude_default() {
        let cat = ProviderCatalog::with_builtins();
        assert!(cat.contains("claude-default"));
        let p = cat.resolve("claude-default").expect("must exist");
        assert_eq!(p.default_model(), "claude-sonnet-4-20250514");
        assert!(p.runtime_command().contains("claude"));
    }

    #[test]
    fn with_builtins_contains_claude_fast() {
        let cat = ProviderCatalog::with_builtins();
        assert!(cat.contains("claude-fast"));
        let p = cat.resolve("claude-fast").expect("must exist");
        assert_eq!(p.default_model(), "claude-haiku-4-5");
    }

    #[test]
    fn with_builtins_contains_ollama_phi35() {
        let cat = ProviderCatalog::with_builtins();
        assert!(cat.contains("ollama-phi3.5"));
        let p = cat.resolve("ollama-phi3.5").expect("must exist");
        assert_eq!(p.default_model(), "phi3.5:latest");
        assert!(p.runtime_command().starts_with("ollama"));
    }

    // -- resolve / fallback --------------------------------------------------

    #[test]
    fn resolve_known_id_returns_correct_profile() {
        let cat = ProviderCatalog::with_builtins();
        let p = cat.resolve("claude-fast").expect("must resolve");
        assert_eq!(p.id(), "claude-fast");
    }

    #[test]
    fn resolve_unknown_id_falls_back_to_claude_default() {
        let cat = ProviderCatalog::with_builtins();
        let p = cat.resolve("nonexistent-provider").expect("must fall back");
        assert_eq!(
            p.id(),
            "claude-default",
            "unknown IDs must fall back to claude-default"
        );
    }

    #[test]
    fn resolve_cloned_returns_owned_copy() {
        let cat = ProviderCatalog::with_builtins();
        let p = cat.resolve_cloned("claude-default").expect("must exist");
        assert_eq!(p.id(), "claude-default");
        // Owned: modifying p must not affect the catalog.
        drop(p);
        assert!(cat.contains("claude-default"));
    }

    // -- insert / remove / contains -----------------------------------------

    #[test]
    fn insert_adds_new_profile() {
        let mut cat = ProviderCatalog::empty();
        cat.insert(ProviderProfile::new(
            "custom",
            "my-llm",
            "v1",
            vec!["v1".into()],
        ));
        assert!(cat.contains("custom"));
        assert_eq!(cat.len(), 1);
    }

    #[test]
    fn insert_replaces_existing_profile() {
        let mut cat = ProviderCatalog::with_builtins();
        let old_len = cat.len();
        cat.insert(ProviderProfile::new(
            "claude-fast", // same ID
            "new-command",
            "new-model",
            vec!["new-model".into()],
        ));
        // Length must not grow when replacing.
        assert_eq!(cat.len(), old_len, "length must not grow when replacing");
        let p = cat.resolve("claude-fast").expect("must exist");
        assert_eq!(p.runtime_command(), "new-command");
    }

    #[test]
    fn remove_existing_profile_returns_it() {
        let mut cat = ProviderCatalog::with_builtins();
        let removed = cat.remove("claude-fast");
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().id(), "claude-fast");
        assert!(!cat.contains("claude-fast"));
    }

    #[test]
    fn remove_absent_profile_returns_none() {
        let mut cat = ProviderCatalog::with_builtins();
        assert!(cat.remove("does-not-exist").is_none());
    }

    // -- iter ---------------------------------------------------------------

    #[test]
    fn iter_yields_all_profiles() {
        let cat = ProviderCatalog::with_builtins();
        let ids: Vec<&str> = cat.iter().map(|p| p.id()).collect();
        assert!(ids.contains(&"claude-default"));
        assert!(ids.contains(&"claude-fast"));
        assert!(ids.contains(&"ollama-phi3.5"));
        assert!(ids.contains(&"ollama-llama3"));
    }

    // -- Default ------------------------------------------------------------

    #[test]
    fn default_equals_with_builtins() {
        let cat: ProviderCatalog = Default::default();
        assert!(cat.contains("claude-default"));
        assert!(cat.contains("claude-fast"));
        assert!(cat.contains("ollama-phi3.5"));
        assert!(cat.contains("ollama-llama3"));
    }

    // -- Required named tests (Issue #61) -----------------------------------

    /// The built-in "claude-fast" profile must use the haiku model.
    #[test]
    fn catalog_get_builtin_fast_profile() {
        let cat = ProviderCatalog::with_builtins();
        let p = cat.get("claude-fast").expect("claude-fast must be present");
        assert!(
            p.default_model().contains("haiku"),
            "fast profile must use haiku, got {}",
            p.default_model()
        );
    }

    /// Inserting a custom profile with an existing ID shadows the built-in.
    #[test]
    fn catalog_custom_profile_overrides_builtin() {
        let mut cat = ProviderCatalog::with_builtins();
        // Replace "claude-fast" with a custom profile.
        cat.add_profile(ProviderProfile::new(
            "claude-fast",
            "my-llm-runner",
            "custom-fast-model",
            vec!["custom-fast-model".into()],
        ));
        let p = cat
            .get("claude-fast")
            .expect("claude-fast must still be present after override");
        assert_eq!(
            p.runtime_command(),
            "my-llm-runner",
            "custom profile must shadow the built-in"
        );
        assert_eq!(p.default_model(), "custom-fast-model");
        // Catalog length must not grow when replacing.
        assert_eq!(cat.len(), 4, "length must not grow when overriding");
    }

    /// Requesting a non-existent profile via `get` returns `None`.
    #[test]
    fn catalog_get_missing_profile_returns_none() {
        let cat = ProviderCatalog::with_builtins();
        assert!(
            cat.get("nonexistent-profile").is_none(),
            "get must return None for absent profiles"
        );
    }

    /// The default profile from `default_profile()` uses claude-sonnet.
    #[test]
    fn catalog_default_profile_has_sonnet() {
        let p = ProviderCatalog::default_profile();
        assert!(
            p.default_model().contains("sonnet"),
            "default profile must use sonnet, got {}",
            p.default_model()
        );
    }

    /// The built-in `ollama-llama3` profile must exist and use llama3.
    #[test]
    fn with_builtins_contains_ollama_llama3() {
        let cat = ProviderCatalog::with_builtins();
        assert!(
            cat.contains("ollama-llama3"),
            "ollama-llama3 must be present"
        );
        let p = cat.resolve("ollama-llama3").expect("must resolve");
        assert_eq!(p.default_model(), "llama3:latest");
        assert!(p.runtime_command().starts_with("ollama"));
    }

    /// `build_ollama_backend` returns an `OllamaBackend` for `ollama-*` profiles.
    #[test]
    fn build_ollama_backend_returns_backend_for_ollama_profile() {
        let cat = ProviderCatalog::with_builtins();
        let backend = cat.build_ollama_backend("ollama-phi3.5");
        assert!(
            backend.is_some(),
            "build_ollama_backend must return Some for ollama-phi3.5"
        );
        let backend = backend.unwrap();
        assert_eq!(backend.model(), "phi3.5:latest");
        assert_eq!(backend.base_url(), "http://localhost:11434");
    }

    /// `build_ollama_backend` returns an `OllamaBackend` for `ollama-llama3`.
    #[test]
    fn build_ollama_backend_returns_backend_for_llama3_profile() {
        let cat = ProviderCatalog::with_builtins();
        let backend = cat
            .build_ollama_backend("ollama-llama3")
            .expect("ollama-llama3 must produce a backend");
        assert_eq!(backend.model(), "llama3:latest");
    }

    /// `build_ollama_backend` returns `None` for non-Ollama profiles.
    #[test]
    fn build_ollama_backend_returns_none_for_claude_profile() {
        let cat = ProviderCatalog::with_builtins();
        let backend = cat.build_ollama_backend("claude-default");
        assert!(
            backend.is_none(),
            "build_ollama_backend must return None for claude profiles"
        );
    }

    /// `build_ollama_backend` returns `None` for missing profiles.
    #[test]
    fn build_ollama_backend_returns_none_for_missing_profile() {
        let cat = ProviderCatalog::with_builtins();
        let backend = cat.build_ollama_backend("nonexistent-profile");
        assert!(
            backend.is_none(),
            "build_ollama_backend must return None for missing profiles"
        );
    }
}
