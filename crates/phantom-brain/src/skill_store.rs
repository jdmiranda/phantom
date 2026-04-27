//! Voyager-inspired skill library built on top of MemoryStore.
//!
//! Skills are stored as key-value pairs in project memory. Each skill has:
//! - A unique name (the key, prefixed with `skill:`)
//! - Code/prompt content (what the agent should do)
//! - A description (used for retrieval matching)
//! - Metadata (success count, last used, source task)
//!
//! # Voyager mapping
//!
//! | Voyager concept         | Phantom equivalent                          |
//! |-------------------------|---------------------------------------------|
//! | `program_name` key      | `skill:{name}` in MemoryStore               |
//! | `code` value            | `SkillEntry.code` (agent prompt/procedure)  |
//! | `description` value     | `SkillEntry.description` (for retrieval)    |
//! | OpenAI Embeddings       | Substring match (upgrade to embeddings later)|
//! | Chroma vector DB        | MemoryStore.search (upgrade to vector later) |
//! | `retrieval_top_k = 5`   | `RETRIEVAL_TOP_K = 5`                       |
//! | `add_new_skill`         | `SkillStore::add_skill`                     |
//! | `similarity_search`     | `SkillStore::retrieve`                      |

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use phantom_memory::{MemoryCategory, MemorySource, MemoryStore};

/// Maximum number of skills to retrieve for a given query.
const RETRIEVAL_TOP_K: usize = 5;

/// Prefix for skill keys in the memory store.
const SKILL_PREFIX: &str = "skill:";

// ---------------------------------------------------------------------------
// SkillEntry
// ---------------------------------------------------------------------------

/// A skill in the library. Contains the procedure (what to do) and metadata
/// for retrieval and quality tracking.
#[derive(Debug, Clone)]
pub struct SkillEntry {
    /// Unique skill name (e.g., "fix_borrow_checker_error").
    pub name: String,
    /// The skill procedure -- either a prompt template or a sequence of steps
    /// that an agent should follow. This is the "code" in Voyager terms.
    pub code: String,
    /// Human-readable description of what this skill does. Used for retrieval
    /// matching (the "embedding query" target).
    pub description: String,
    /// How many times this skill has been successfully used.
    pub success_count: u32,
    /// The task that originally produced this skill.
    pub source_task: String,
    /// Unix timestamp of last successful use.
    pub last_used: u64,
}

// ---------------------------------------------------------------------------
// SkillStore
// ---------------------------------------------------------------------------

/// In-memory skill library with persistence through MemoryStore.
///
/// Skills are cached in memory for fast retrieval and synced to the
/// MemoryStore for persistence. The retrieval is currently substring-based
/// (matching against descriptions); this can be upgraded to embedding-based
/// retrieval when a local embedding model is available.
pub struct SkillStore {
    /// In-memory skill cache: name -> SkillEntry.
    skills: HashMap<String, SkillEntry>,
}

impl SkillStore {
    /// Create an empty skill store.
    pub fn new() -> Self {
        Self {
            skills: HashMap::new(),
        }
    }

    /// Load skills from a MemoryStore (call at startup).
    ///
    /// Scans all memory entries with the `skill:` prefix and deserializes them
    /// into SkillEntry structs.
    pub fn load_from_memory(&mut self, memory: &MemoryStore) {
        for entry in memory.all() {
            let Some(name) = entry.key.strip_prefix(SKILL_PREFIX) else {
                continue;
            };
            // Value format: "desc\x1Ecode\x1Esource_task\x1Esuccess_count\x1Elast_used"
            let parts: Vec<&str> = entry.value.splitn(5, '\x1E').collect();
            if parts.len() < 3 {
                continue;
            }
            let skill = SkillEntry {
                name: name.to_owned(),
                description: parts[0].to_owned(),
                code: parts[1].to_owned(),
                source_task: parts[2].to_owned(),
                success_count: parts.get(3).and_then(|s| s.parse().ok()).unwrap_or(0),
                last_used: parts.get(4).and_then(|s| s.parse().ok()).unwrap_or(0),
            };
            self.skills.insert(name.to_owned(), skill);
        }
    }

    /// Add a new skill to the library and persist to memory.
    ///
    /// This is called when a task succeeds and the critic verifies it.
    /// Like Voyager's `add_new_skill`, it only stores skills that have
    /// been verified as working.
    pub fn add_skill(
        &mut self,
        name: &str,
        code: &str,
        description: &str,
        source_task: &str,
        memory: &mut MemoryStore,
    ) -> anyhow::Result<()> {
        let now = now_epoch();
        let entry = SkillEntry {
            name: name.to_owned(),
            code: code.to_owned(),
            description: description.to_owned(),
            success_count: 1,
            source_task: source_task.to_owned(),
            last_used: now,
        };

        // Serialize to memory value format.
        let value = format!(
            "{}\x1E{}\x1E{}\x1E{}\x1E{}",
            entry.description, entry.code, entry.source_task, entry.success_count, entry.last_used
        );

        let key = format!("{SKILL_PREFIX}{name}");
        memory.set(&key, &value, MemoryCategory::Convention, MemorySource::Agent)?;
        self.skills.insert(name.to_owned(), entry);

        Ok(())
    }

    /// Record a successful use of an existing skill.
    pub fn record_use(
        &mut self,
        name: &str,
        memory: &mut MemoryStore,
    ) -> anyhow::Result<()> {
        let Some(skill) = self.skills.get_mut(name) else {
            return Ok(());
        };

        skill.success_count += 1;
        skill.last_used = now_epoch();

        let value = format!(
            "{}\x1E{}\x1E{}\x1E{}\x1E{}",
            skill.description, skill.code, skill.source_task, skill.success_count, skill.last_used
        );

        let key = format!("{SKILL_PREFIX}{name}");
        memory.set(&key, &value, MemoryCategory::Convention, MemorySource::Agent)?;

        Ok(())
    }

    /// Retrieve the top-k most relevant skills for a task query.
    ///
    /// Currently uses substring matching on descriptions (case-insensitive).
    /// Each word in the query is matched independently, and skills are scored
    /// by the number of matching words. This is a placeholder for embedding-based
    /// retrieval (Voyager uses OpenAI Embeddings + Chroma with cosine similarity).
    ///
    /// Returns skills sorted by relevance (best first), capped at `RETRIEVAL_TOP_K`.
    pub fn retrieve(&self, query: &str) -> Vec<&SkillEntry> {
        if self.skills.is_empty() {
            return Vec::new();
        }

        let query_lower = query.to_lowercase();
        let query_words: Vec<&str> = query_lower.split_whitespace().collect();

        let mut scored: Vec<(&SkillEntry, f32)> = self
            .skills
            .values()
            .map(|skill| {
                let desc_lower = skill.description.to_lowercase();
                let code_lower = skill.code.to_lowercase();

                // Score: fraction of query words found in description or code.
                let matching = query_words
                    .iter()
                    .filter(|w| desc_lower.contains(*w) || code_lower.contains(*w))
                    .count();

                let word_score = if query_words.is_empty() {
                    0.0
                } else {
                    matching as f32 / query_words.len() as f32
                };

                // Only boost if there's at least one word match.
                let usage_boost = if word_score > 0.0 {
                    (skill.success_count as f32 * 0.05).min(0.2)
                } else {
                    0.0
                };

                (skill, word_score + usage_boost)
            })
            .filter(|(_, score)| *score > 0.0)
            .collect();

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(RETRIEVAL_TOP_K);
        scored.into_iter().map(|(skill, _)| skill).collect()
    }

    /// Format retrieved skills as context for an agent prompt.
    ///
    /// Mirrors Voyager's pattern of injecting top-5 relevant skills into
    /// the action agent's prompt so it can compose them.
    pub fn format_for_prompt(&self, query: &str) -> String {
        let skills = self.retrieve(query);
        if skills.is_empty() {
            return String::from("No relevant skills found in library.");
        }

        let mut out = String::from("Relevant skills from library:\n");
        for (i, skill) in skills.iter().enumerate() {
            out.push_str(&format!(
                "\n{}. {} (used {} times)\n   Description: {}\n   Procedure: {}\n",
                i + 1,
                skill.name,
                skill.success_count,
                skill.description,
                truncate_code(&skill.code, 200),
            ));
        }
        out
    }

    /// Get all skill names (for curriculum context).
    pub fn list_skill_names(&self) -> Vec<String> {
        self.skills.keys().cloned().collect()
    }

    /// Total number of skills in the library.
    pub fn count(&self) -> usize {
        self.skills.len()
    }

    /// Get a skill by name.
    pub fn get(&self, name: &str) -> Option<&SkillEntry> {
        self.skills.get(name)
    }
}

impl Default for SkillStore {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_secs()
}

fn truncate_code(s: &str, max_len: usize) -> String {
    if s.len() > max_len {
        format!("{}...", &s[..max_len])
    } else {
        s.to_owned()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_memory() -> (MemoryStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open_in("/tmp/test", dir.path()).unwrap();
        (store, dir)
    }

    #[test]
    fn add_and_retrieve_skill() {
        let mut store = SkillStore::new();
        let (mut memory, _dir) = tmp_memory();

        store
            .add_skill(
                "fix_borrow_error",
                "1. Read the file\n2. Find the borrow\n3. Add clone or lifetime",
                "Fix Rust borrow checker errors by adding lifetimes or cloning",
                "cargo build failed with E0382",
                &mut memory,
            )
            .unwrap();

        assert_eq!(store.count(), 1);

        let results = store.retrieve("borrow checker error");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "fix_borrow_error");
    }

    #[test]
    fn retrieve_returns_empty_for_no_match() {
        let mut store = SkillStore::new();
        let (mut memory, _dir) = tmp_memory();

        store
            .add_skill(
                "handle_borrow",
                "resolve borrows",
                "Resolve borrow checker issues",
                "task",
                &mut memory,
            )
            .unwrap();

        let results = store.retrieve("kubernetes deployment yaml");
        assert!(results.is_empty());
    }

    #[test]
    fn retrieve_caps_at_top_k() {
        let mut store = SkillStore::new();
        let (mut memory, _dir) = tmp_memory();

        for i in 0..10 {
            store
                .add_skill(
                    &format!("skill_{i}"),
                    &format!("fix error type {i}"),
                    &format!("Fix error type {i} in Rust code"),
                    "task",
                    &mut memory,
                )
                .unwrap();
        }

        let results = store.retrieve("fix error Rust");
        assert!(results.len() <= RETRIEVAL_TOP_K);
    }

    #[test]
    fn record_use_increments_count() {
        let mut store = SkillStore::new();
        let (mut memory, _dir) = tmp_memory();

        store
            .add_skill("s1", "code", "desc", "task", &mut memory)
            .unwrap();
        assert_eq!(store.get("s1").unwrap().success_count, 1);

        store.record_use("s1", &mut memory).unwrap();
        assert_eq!(store.get("s1").unwrap().success_count, 2);
    }

    #[test]
    fn load_from_memory_round_trips() {
        let (mut memory, _dir) = tmp_memory();

        // Add via store, persist to memory.
        {
            let mut store = SkillStore::new();
            store
                .add_skill(
                    "test_skill",
                    "do the thing",
                    "A test skill for testing",
                    "test task",
                    &mut memory,
                )
                .unwrap();
        }

        // Load into a fresh store from memory.
        let mut store2 = SkillStore::new();
        store2.load_from_memory(&memory);
        assert_eq!(store2.count(), 1);
        let skill = store2.get("test_skill").unwrap();
        assert_eq!(skill.code, "do the thing");
        assert_eq!(skill.description, "A test skill for testing");
        assert_eq!(skill.source_task, "test task");
    }

    #[test]
    fn format_for_prompt_shows_skills() {
        let mut store = SkillStore::new();
        let (mut memory, _dir) = tmp_memory();

        store
            .add_skill(
                "fix_lifetime",
                "Add explicit lifetime annotations",
                "Fix lifetime errors in Rust generics",
                "task",
                &mut memory,
            )
            .unwrap();

        let prompt = store.format_for_prompt("lifetime error");
        assert!(prompt.contains("fix_lifetime"));
        assert!(prompt.contains("used 1 times"));
    }

    #[test]
    fn list_skill_names() {
        let mut store = SkillStore::new();
        let (mut memory, _dir) = tmp_memory();

        store.add_skill("a", "code", "desc", "task", &mut memory).unwrap();
        store.add_skill("b", "code", "desc", "task", &mut memory).unwrap();

        let names = store.list_skill_names();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"a".to_owned()));
        assert!(names.contains(&"b".to_owned()));
    }
}
