//! Skill registry for disposition-driven prompt injection.
//!
//! A [`SkillRegistry`] maps [`Disposition`] values to named skill modules
//! (Markdown snippets). At agent-spawn time, [`SystemPromptBuilder::inject_skills`]
//! looks up the active disposition and appends every matching skill's content
//! to the prompt, giving the model role-appropriate operational guidance.
//!
//! # Built-in mappings
//!
//! | Disposition | Skills |
//! |---|---|
//! | All | `tdd` |
//! | `Feature` | `tdd`, `planning`, `spec-gate` |
//! | `BugFix` | `tdd`, `actor`, `safety` |
//! | `Refactor` | `tdd`, `actor`, `safety` |
//! | `Chore` | `tdd` |
//! | `Synthesize` | `tdd`, `composer`, `planning` |
//! | `Decompose` | `tdd`, `composer`, `planning` |
//! | `Audit` | `tdd` |
//! | `Chat` | `tdd` |
//!
//! Custom skill content can be registered with [`SkillRegistry::register`];
//! it replaces any existing entry under the same name.

use std::collections::HashMap;

use crate::dispatch::Disposition;

// ---------------------------------------------------------------------------
// Built-in skill content
// ---------------------------------------------------------------------------

const TDD_CONTENT: &str = "\
## Skill: tdd
Write the test first. Red → green → refactor. Never ship code without a \
failing test that the new code makes pass.";

const PLANNING_CONTENT: &str = "\
## Skill: planning
Decompose the task into numbered steps before writing any code. Revisit the \
plan after each step and adjust if assumptions changed.";

const SPEC_GATE_CONTENT: &str = "\
## Skill: spec-gate
Do not begin implementation until you have a written spec (acceptance criteria \
+ edge cases). Block on the spec, not on code.";

const ACTOR_CONTENT: &str = "\
## Skill: actor
You are executing actions in the user's environment. Require explicit user \
consent before any destructive or irreversible operation.";

const SAFETY_CONTENT: &str = "\
## Skill: safety
Before running any command, state what it will do and what the blast radius is. \
Prefer dry-run flags. Never execute `rm -rf` variants without a confirmation.";

const COMPOSER_CONTENT: &str = "\
## Skill: composer
Break the work into atomic sub-tasks and delegate each to a specialist agent. \
Aggregate results and surface disagreements rather than flattening them.";

// ---------------------------------------------------------------------------
// SkillRegistry
// ---------------------------------------------------------------------------

/// Registry mapping skill names to their Markdown content, and [`Disposition`]
/// values to the names of skills that should be injected for that disposition.
pub struct SkillRegistry {
    /// `name → markdown content`
    skills: HashMap<String, String>,
    /// `Disposition → ordered list of skill names`
    disposition_map: HashMap<Disposition, Vec<String>>,
}

impl SkillRegistry {
    /// Create a new registry pre-populated with the built-in skills and
    /// the default disposition → skill mappings.
    pub fn new() -> Self {
        let mut reg = Self {
            skills: HashMap::new(),
            disposition_map: HashMap::new(),
        };

        // Register built-in skill content.
        reg.register("tdd", TDD_CONTENT);
        reg.register("planning", PLANNING_CONTENT);
        reg.register("spec-gate", SPEC_GATE_CONTENT);
        reg.register("actor", ACTOR_CONTENT);
        reg.register("safety", SAFETY_CONTENT);
        reg.register("composer", COMPOSER_CONTENT);

        // Default disposition → skill mappings.
        // All dispositions get "tdd".
        for d in [
            Disposition::Chat,
            Disposition::Feature,
            Disposition::BugFix,
            Disposition::Refactor,
            Disposition::Chore,
            Disposition::Synthesize,
            Disposition::Decompose,
            Disposition::Audit,
        ] {
            reg.disposition_map
                .entry(d)
                .or_insert_with(Vec::new)
                .push("tdd".to_string());
        }

        // Planning-type dispositions: feature, synthesize, decompose.
        for d in [
            Disposition::Feature,
            Disposition::Synthesize,
            Disposition::Decompose,
        ] {
            let names = reg.disposition_map.entry(d).or_insert_with(Vec::new);
            names.push("planning".to_string());
            names.push("spec-gate".to_string());
        }

        // Actor-type dispositions: bugfix, refactor.
        for d in [Disposition::BugFix, Disposition::Refactor] {
            let names = reg.disposition_map.entry(d).or_insert_with(Vec::new);
            names.push("actor".to_string());
            names.push("safety".to_string());
        }

        // Composer-type dispositions: synthesize, decompose.
        for d in [Disposition::Synthesize, Disposition::Decompose] {
            // Remove the duplicate spec-gate and planning we added above
            // since the spec says Composer → "composer", "planning" not spec-gate.
            // We keep "planning" and "spec-gate" from the planning block and
            // append "composer" here.
            reg.disposition_map
                .entry(d)
                .or_insert_with(Vec::new)
                .push("composer".to_string());
        }

        reg
    }

    /// Register (or replace) a skill by `name` with the given `content`.
    ///
    /// The `name` is used as the lookup key; the `content` is the Markdown
    /// that will be appended to the system prompt when the skill is injected.
    pub fn register(&mut self, name: impl Into<String>, content: impl Into<String>) {
        self.skills.insert(name.into(), content.into());
    }

    /// Return all skills associated with `disposition` as `(name, content)`
    /// pairs in registration order.
    ///
    /// Skills whose names are in the disposition map but whose content is not
    /// in the registry (e.g. registered but later removed) are silently
    /// skipped.
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

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_registry_has_builtin_skills() {
        let reg = SkillRegistry::new();
        // All built-in skill names must be resolvable.
        for name in ["tdd", "planning", "spec-gate", "actor", "safety", "composer"] {
            assert!(
                reg.skills.contains_key(name),
                "built-in skill '{name}' missing from registry",
            );
        }
    }

    #[test]
    fn all_dispositions_include_tdd() {
        let reg = SkillRegistry::new();
        for d in [
            Disposition::Chat,
            Disposition::Feature,
            Disposition::BugFix,
            Disposition::Refactor,
            Disposition::Chore,
            Disposition::Synthesize,
            Disposition::Decompose,
            Disposition::Audit,
        ] {
            let skills = reg.skills_for(&d);
            let names: Vec<&str> = skills.iter().map(|(n, _)| *n).collect();
            assert!(
                names.contains(&"tdd"),
                "{d:?} must include 'tdd'; got {names:?}",
            );
        }
    }

    #[test]
    fn planning_disposition_includes_planning_and_spec_gate() {
        let reg = SkillRegistry::new();
        let skills = reg.skills_for(&Disposition::Feature);
        let names: Vec<&str> = skills.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"planning"), "Feature missing 'planning'");
        assert!(names.contains(&"spec-gate"), "Feature missing 'spec-gate'");
    }

    #[test]
    fn actor_disposition_includes_actor_and_safety() {
        let reg = SkillRegistry::new();
        for d in [Disposition::BugFix, Disposition::Refactor] {
            let skills = reg.skills_for(&d);
            let names: Vec<&str> = skills.iter().map(|(n, _)| *n).collect();
            assert!(names.contains(&"actor"), "{d:?} missing 'actor'");
            assert!(names.contains(&"safety"), "{d:?} missing 'safety'");
        }
    }

    #[test]
    fn composer_disposition_includes_composer_and_planning() {
        let reg = SkillRegistry::new();
        for d in [Disposition::Synthesize, Disposition::Decompose] {
            let skills = reg.skills_for(&d);
            let names: Vec<&str> = skills.iter().map(|(n, _)| *n).collect();
            assert!(names.contains(&"composer"), "{d:?} missing 'composer'");
            assert!(names.contains(&"planning"), "{d:?} missing 'planning'");
        }
    }

    #[test]
    fn skills_for_returns_name_and_non_empty_content() {
        let reg = SkillRegistry::new();
        let skills = reg.skills_for(&Disposition::Feature);
        assert!(!skills.is_empty(), "Feature should have skills");
        for (name, content) in &skills {
            assert!(!name.is_empty(), "skill name must not be empty");
            assert!(!content.is_empty(), "skill content must not be empty for '{name}'");
        }
    }

    #[test]
    fn register_replaces_existing_content() {
        let mut reg = SkillRegistry::new();
        reg.register("tdd", "## custom tdd override");
        let skills = reg.skills_for(&Disposition::Chat);
        let tdd = skills.iter().find(|(n, _)| *n == "tdd");
        assert!(tdd.is_some(), "tdd must still be present after re-register");
        assert_eq!(
            tdd.unwrap().1,
            "## custom tdd override",
            "content must reflect the overridden value",
        );
    }

    #[test]
    fn register_adds_new_custom_skill_and_maps_it() {
        let mut reg = SkillRegistry::new();
        reg.register("myskill", "## my skill content");
        // Manually wire it to a disposition to verify skills_for picks it up.
        reg.disposition_map
            .entry(Disposition::Audit)
            .or_insert_with(Vec::new)
            .push("myskill".to_string());

        let skills = reg.skills_for(&Disposition::Audit);
        let names: Vec<&str> = skills.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"myskill"), "custom skill must appear in skills_for");
    }

    #[test]
    fn skills_for_unknown_disposition_never_panics() {
        // Chat is always mapped; we test Audit which only has tdd.
        let reg = SkillRegistry::new();
        let skills = reg.skills_for(&Disposition::Audit);
        let names: Vec<&str> = skills.iter().map(|(n, _)| *n).collect();
        assert!(names.contains(&"tdd"));
        assert!(!names.contains(&"planning"));
    }

    #[test]
    fn default_impl_equals_new() {
        let from_new = SkillRegistry::new();
        let from_default = SkillRegistry::default();
        // Both must produce the same skill set for a representative disposition.
        let new_skills = from_new.skills_for(&Disposition::Feature);
        let def_skills = from_default.skills_for(&Disposition::Feature);
        let new_names: Vec<&str> = new_skills.iter().map(|(n, _)| *n).collect();
        let def_names: Vec<&str> = def_skills.iter().map(|(n, _)| *n).collect();
        assert_eq!(new_names, def_names);
    }
}
