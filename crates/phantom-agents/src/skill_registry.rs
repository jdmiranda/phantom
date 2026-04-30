//! Skill registry keyed by [`Disposition`].
//!
//! A *skill* is a named markdown string that is appended to an agent's system
//! prompt at spawn time. Skills carry behavioural guidelines ("use TDD",
//! "always spec-gate before coding", etc.) that are too verbose to embed in
//! the static role manifest but too important to omit.
//!
//! [`SkillRegistry`] maintains two tables:
//!
//! 1. `skills` — name → markdown content. Skills are registered once (at
//!    process start or from tests) and then referenced by name.
//! 2. `disposition_map` — [`Disposition`] → ordered list of skill names.
//!    Callers query this table via [`SkillRegistry::skills_for`] to get the
//!    `(name, content)` pairs that belong in a given agent's system prompt.
//!
//! # Default mappings
//!
//! [`SkillRegistry::new`] pre-populates a sensible set of mappings (see
//! inline docs on `new`). Callers can add more skills and mappings after
//! construction; default entries are never removed.
//!
//! # Example
//!
//! ```rust
//! use phantom_agents::skill_registry::SkillRegistry;
//! use phantom_agents::dispatch::Disposition;
//!
//! let mut registry = SkillRegistry::new();
//! registry.register("tdd", "# TDD\nWrite tests first.");
//! let pairs = registry.skills_for(&Disposition::Decompose);
//! assert!(pairs.iter().any(|(name, _)| *name == "tdd"));
//! ```

use std::collections::HashMap;

use crate::dispatch::Disposition;

/// A registry that maps [`Disposition`] variants to injectable skill modules.
///
/// Skills are stored as `(name, markdown_content)` pairs.  Each
/// [`Disposition`] variant maps to zero or more skill names; when
/// [`skills_for`] is called the registry resolves those names and returns the
/// corresponding `(name, content)` slices.
///
/// [`skills_for`]: SkillRegistry::skills_for
pub struct SkillRegistry {
    /// name → markdown content.
    skills: HashMap<String, String>,
    /// disposition → ordered list of skill names.
    disposition_map: HashMap<Disposition, Vec<String>>,
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillRegistry {
    /// Create a new registry with default skill content and disposition
    /// mappings pre-populated.
    ///
    /// Default mappings (Forge lineage):
    ///
    /// | Disposition | Skills |
    /// |---|---|
    /// | All | `tdd` |
    /// | `Decompose` | `planning`, `spec-gate` |
    /// | `Feature`, `BugFix` | `actor`, `safety` |
    /// | `Synthesize` | `composer`, `planning` |
    ///
    /// Skills are registered with placeholder content.  Production callers
    /// should call [`register`] to override with the real markdown.
    ///
    /// [`register`]: SkillRegistry::register
    pub fn new() -> Self {
        let mut registry = Self {
            skills: HashMap::new(),
            disposition_map: HashMap::new(),
        };

        // Register built-in skill placeholders.
        registry.register("tdd", "# TDD\nWrite failing tests first, then make them pass.");
        registry.register(
            "planning",
            "# Planning\nDecompose the task. Identify unknowns before writing code.",
        );
        registry.register(
            "spec-gate",
            "# Spec gate\nDo not begin implementation until the spec is accepted.",
        );
        registry.register(
            "actor",
            "# Actor\nExecute the plan step-by-step. Prefer atomic commits.",
        );
        registry.register(
            "safety",
            "# Safety\nNever mutate production state without explicit user consent.",
        );
        registry.register(
            "composer",
            "# Composer\nDelegate to sub-agents. Never do the work yourself.",
        );

        // Default disposition→skill mappings.
        for disposition in [
            Disposition::Chat,
            Disposition::Feature,
            Disposition::BugFix,
            Disposition::Refactor,
            Disposition::Chore,
            Disposition::Synthesize,
            Disposition::Decompose,
            Disposition::Audit,
        ] {
            registry.map_disposition(disposition, "tdd");
        }

        // Decompose is the planning-oriented disposition.
        registry.map_disposition(Disposition::Decompose, "planning");
        registry.map_disposition(Disposition::Decompose, "spec-gate");

        // Feature and BugFix are actor-style (they execute changes).
        registry.map_disposition(Disposition::Feature, "actor");
        registry.map_disposition(Disposition::Feature, "safety");
        registry.map_disposition(Disposition::BugFix, "actor");
        registry.map_disposition(Disposition::BugFix, "safety");

        // Synthesize is the composer-style disposition.
        registry.map_disposition(Disposition::Synthesize, "composer");
        registry.map_disposition(Disposition::Synthesize, "planning");

        registry
    }

    /// Register a skill module by name.
    ///
    /// If a skill with `name` already exists its content is replaced.
    pub fn register(&mut self, name: impl Into<String>, content: impl Into<String>) {
        self.skills.insert(name.into(), content.into());
    }

    /// Map `disposition` to an additional skill name.
    ///
    /// This is additive: existing mappings for `disposition` are preserved.
    /// Duplicate names in the list are kept — deduplication is the caller's
    /// responsibility if needed.
    pub fn map_disposition(&mut self, disposition: Disposition, skill_name: impl Into<String>) {
        self.disposition_map
            .entry(disposition)
            .or_default()
            .push(skill_name.into());
    }

    /// Return the `(name, content)` pairs for all skills mapped to
    /// `disposition`.
    ///
    /// Skills whose name is registered in the registry are returned in the
    /// order they appear in the disposition map.  Names that have no
    /// corresponding content entry are silently skipped (they were mapped but
    /// never registered).
    ///
    /// Returns an empty `Vec` if no skills are mapped to this disposition.
    pub fn skills_for(&self, disposition: &Disposition) -> Vec<(&str, &str)> {
        let Some(names) = self.disposition_map.get(disposition) else {
            return Vec::new();
        };
        names
            .iter()
            .filter_map(|name| {
                let content = self.skills.get(name)?;
                Some((name.as_str(), content.as_str()))
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_registry_has_tdd_for_every_disposition() {
        let registry = SkillRegistry::new();
        for disposition in [
            Disposition::Chat,
            Disposition::Feature,
            Disposition::BugFix,
            Disposition::Refactor,
            Disposition::Chore,
            Disposition::Synthesize,
            Disposition::Decompose,
            Disposition::Audit,
        ] {
            let skills = registry.skills_for(&disposition);
            assert!(
                skills.iter().any(|(name, _)| *name == "tdd"),
                "{disposition:?} must include the tdd skill"
            );
        }
    }

    #[test]
    fn decompose_returns_planning_and_spec_gate() {
        let registry = SkillRegistry::new();
        let skills = registry.skills_for(&Disposition::Decompose);
        let names: Vec<&str> = skills.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"planning"), "Decompose must include planning");
        assert!(names.contains(&"spec-gate"), "Decompose must include spec-gate");
    }

    #[test]
    fn feature_and_bugfix_return_actor_and_safety() {
        let registry = SkillRegistry::new();
        for disposition in [Disposition::Feature, Disposition::BugFix] {
            let skills = registry.skills_for(&disposition);
            let names: Vec<&str> = skills.iter().map(|(n, _)| *n).collect();
            assert!(names.contains(&"actor"), "{disposition:?} must include actor");
            assert!(names.contains(&"safety"), "{disposition:?} must include safety");
        }
    }

    #[test]
    fn synthesize_returns_composer_and_planning() {
        let registry = SkillRegistry::new();
        let skills = registry.skills_for(&Disposition::Synthesize);
        let names: Vec<&str> = skills.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"composer"), "Synthesize must include composer");
        assert!(names.contains(&"planning"), "Synthesize must include planning");
    }

    #[test]
    fn skills_for_unknown_disposition_returns_empty_when_not_mapped() {
        // A freshly-created, empty registry (no default calls) returns empty.
        let registry = SkillRegistry {
            skills: HashMap::new(),
            disposition_map: HashMap::new(),
        };
        assert!(registry.skills_for(&Disposition::Chat).is_empty());
    }

    #[test]
    fn register_overwrites_existing_content() {
        let mut registry = SkillRegistry::new();
        registry.register("tdd", "# Updated TDD\nNew content.");
        let skills = registry.skills_for(&Disposition::Chat);
        let tdd = skills.iter().find(|(n, _)| *n == "tdd");
        assert!(tdd.is_some());
        assert!(tdd.unwrap().1.contains("Updated TDD"));
    }

    #[test]
    fn register_and_map_custom_skill() {
        let mut registry = SkillRegistry::new();
        registry.register("custom", "# Custom skill");
        registry.map_disposition(Disposition::Audit, "custom");

        let skills = registry.skills_for(&Disposition::Audit);
        let names: Vec<&str> = skills.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"custom"), "Audit must include custom after mapping");
    }

    #[test]
    fn unregistered_skill_name_silently_skipped() {
        let mut registry = SkillRegistry {
            skills: HashMap::new(),
            disposition_map: HashMap::new(),
        };
        // Map a name that has no content entry.
        registry.map_disposition(Disposition::Chat, "ghost");
        let skills = registry.skills_for(&Disposition::Chat);
        assert!(skills.is_empty(), "unregistered name must be skipped");
    }

    #[test]
    fn skills_for_returns_content_slices() {
        let registry = SkillRegistry::new();
        let skills = registry.skills_for(&Disposition::Decompose);
        for (name, content) in &skills {
            assert!(!name.is_empty(), "name must be non-empty");
            assert!(!content.is_empty(), "content must be non-empty");
        }
    }
}
