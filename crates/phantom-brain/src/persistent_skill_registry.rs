//! Persistent skill registry for the AI brain.
//!
//! Stores learned skills between sessions as a JSONL file on disk.  Each skill
//! record captures its capability class, the handler that executes it, its
//! provenance (who authored it, when), and accumulated outcome statistics
//! (success / failure counts).
//!
//! # Concurrency model
//!
//! An `Arc<RwLock<HashMap<SkillId, Skill>>>` keeps the in-memory index safe
//! for concurrent reads.  Writes acquire the write lock, mutate the map, then
//! flush the full index to disk using an atomic rename (write to a `.tmp` file
//! then `rename` into place), so no reader ever sees a partial JSONL file.
//!
//! # File format
//!
//! One JSON object per line (`\n`-delimited).  Each line is a serialised
//! [`Skill`].  The file lives at `~/.local/share/phantom/skills.jsonl`.
//!
//! ```json
//! {"id":"fix-borrow","description":"Fix borrow checker errors","capability":"Compute","handler":"fix_borrow_handler","provenance":{"authored_by":"agent-1","created_at_ms":1714320000000,"success_count":3,"failure_count":0}}
//! ```
//!
//! # Example
//!
//! ```rust,no_run
//! use phantom_brain::persistent_skill_registry::{
//!     AgentRef, PersistentSkillRegistry, Skill, SkillHandler, SkillId,
//! };
//! use phantom_agents::role::CapabilityClass;
//! use std::sync::Arc;
//!
//! // Inside a tokio runtime:
//! // let registry = Arc::new(PersistentSkillRegistry::load_or_create(None).await.unwrap());
//! // let skill = Skill::new(
//! //     SkillId::from("fix-borrow"),
//! //     "Fix borrow checker errors".into(),
//! //     CapabilityClass::Compute,
//! //     SkillHandler::from("fix_borrow_handler"),
//! //     AgentRef::from("agent-1"),
//! // );
//! // registry.register(skill).await.unwrap();
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use phantom_agents::role::CapabilityClass;

// ---------------------------------------------------------------------------
// Newtypes
// ---------------------------------------------------------------------------

/// Unique identifier for a skill (e.g. `"fix-borrow-checker"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SkillId(String);

impl SkillId {
    /// Create a new `SkillId` from any string-like value.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// View the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for SkillId {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for SkillId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl std::fmt::Display for SkillId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// ---------------------------------------------------------------------------

/// A reference to the agent that authored or last modified a skill.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRef(String);

impl AgentRef {
    /// Create a new `AgentRef`.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// View the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for AgentRef {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for AgentRef {
    fn from(s: String) -> Self {
        Self(s)
    }
}

// ---------------------------------------------------------------------------

/// Identifies the executable handler for a skill.
///
/// In Phantom's runtime, this is resolved to an actual function/closure at
/// skill invocation time.  The registry stores it as an opaque string so
/// that skills survive serialisation/deserialisation across sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillHandler(String);

impl SkillHandler {
    /// Create a new `SkillHandler` reference.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// View the underlying string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for SkillHandler {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

impl From<String> for SkillHandler {
    fn from(s: String) -> Self {
        Self(s)
    }
}

// ---------------------------------------------------------------------------
// SkillProvenance
// ---------------------------------------------------------------------------

/// Provenance and quality-tracking metadata for a [`Skill`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillProvenance {
    /// The agent that first registered this skill.
    authored_by: AgentRef,
    /// Unix timestamp (milliseconds) when the skill was first registered.
    created_at_ms: u64,
    /// Number of times the skill has been recorded as successful.
    success_count: u64,
    /// Number of times the skill has been recorded as failed.
    failure_count: u64,
}

impl SkillProvenance {
    /// Create a fresh provenance record (zero outcomes).
    pub fn new(authored_by: AgentRef) -> Self {
        let created_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Self {
            authored_by,
            created_at_ms,
            success_count: 0,
            failure_count: 0,
        }
    }

    /// The agent that authored this skill.
    pub fn authored_by(&self) -> &AgentRef {
        &self.authored_by
    }

    /// Creation timestamp (Unix ms).
    pub fn created_at_ms(&self) -> u64 {
        self.created_at_ms
    }

    /// Number of successful outcomes recorded.
    pub fn success_count(&self) -> u64 {
        self.success_count
    }

    /// Number of failed outcomes recorded.
    pub fn failure_count(&self) -> u64 {
        self.failure_count
    }

    /// Success ratio `[0.0, 1.0]`, or `None` if no outcomes have been recorded.
    pub fn success_rate(&self) -> Option<f64> {
        let total = self.success_count + self.failure_count;
        if total == 0 {
            None
        } else {
            Some(self.success_count as f64 / total as f64)
        }
    }

    /// Increment the success counter by one.
    pub(super) fn increment_success(&mut self) {
        self.success_count += 1;
    }

    /// Increment the failure counter by one.
    pub(super) fn increment_failure(&mut self) {
        self.failure_count += 1;
    }
}

// ---------------------------------------------------------------------------
// Skill
// ---------------------------------------------------------------------------

/// A learned skill stored in the persistent registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    id: SkillId,
    description: String,
    capability: CapabilityClass,
    handler: SkillHandler,
    provenance: SkillProvenance,
}

impl Skill {
    /// Create a new skill (zero outcome counts, `created_at_ms` = now).
    pub fn new(
        id: SkillId,
        description: String,
        capability: CapabilityClass,
        handler: SkillHandler,
        authored_by: AgentRef,
    ) -> Self {
        Self {
            id,
            description,
            capability,
            handler,
            provenance: SkillProvenance::new(authored_by),
        }
    }

    /// Unique skill identifier.
    pub fn id(&self) -> &SkillId {
        &self.id
    }

    /// Human-readable description of what this skill does.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The capability class this skill requires.
    pub fn capability(&self) -> CapabilityClass {
        self.capability
    }

    /// Handler reference string (resolved at invocation time).
    pub fn handler(&self) -> &SkillHandler {
        &self.handler
    }

    /// Provenance and quality metadata.
    pub fn provenance(&self) -> &SkillProvenance {
        &self.provenance
    }
}

// ---------------------------------------------------------------------------
// PersistentSkillRegistry
// ---------------------------------------------------------------------------

/// Default path for the skills JSONL file.
fn default_skills_path() -> PathBuf {
    dirs_home()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local")
        .join("share")
        .join("phantom")
        .join("skills.jsonl")
}

/// Resolve `~` home directory without an external crate.
fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Persistent skill registry backed by a JSONL file.
///
/// All public methods are `async` and use `tokio::fs` for non-blocking I/O.
/// The in-memory index is protected by a `tokio::sync::RwLock` so concurrent
/// reads are cheap, while writes are serialised.
pub struct PersistentSkillRegistry {
    /// Path to the JSONL file on disk.
    path: PathBuf,
    /// In-memory index: skill id -> skill.
    index: Arc<RwLock<HashMap<SkillId, Skill>>>,
}

impl PersistentSkillRegistry {
    // -----------------------------------------------------------------------
    // Construction
    // -----------------------------------------------------------------------

    /// Load the registry from `path`, creating the file (and parent dirs) if
    /// it does not yet exist.  Pass `None` to use the default path
    /// (`~/.local/share/phantom/skills.jsonl`).
    pub async fn load_or_create(path: Option<PathBuf>) -> Result<Self> {
        let path = path.unwrap_or_else(default_skills_path);

        // Ensure the parent directory exists.
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("create skill store directory: {}", parent.display()))?;
        }

        let index = Self::load_from_disk(&path).await?;

        Ok(Self {
            path,
            index: Arc::new(RwLock::new(index)),
        })
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    /// Register a new skill (or overwrite if the id already exists).
    ///
    /// Updates the in-memory index and atomically flushes to disk.
    pub async fn register(&self, skill: Skill) -> Result<()> {
        {
            let mut idx = self.index.write().await;
            idx.insert(skill.id.clone(), skill);
        }
        self.flush().await
    }

    /// Look up a skill by its unique id.
    ///
    /// Returns a cloned [`Skill`] if found, `None` otherwise.
    pub async fn lookup_by_id(&self, id: &SkillId) -> Option<Skill> {
        let idx = self.index.read().await;
        idx.get(id).cloned()
    }

    /// Return all skills that match the given [`CapabilityClass`].
    ///
    /// Results are returned in registration order (HashMap iteration order).
    pub async fn search_by_capability(&self, cap: CapabilityClass) -> Vec<Skill> {
        let idx = self.index.read().await;
        idx.values()
            .filter(|s| s.capability == cap)
            .cloned()
            .collect()
    }

    /// Record a success or failure outcome for a skill.
    ///
    /// Does nothing (silently) if the skill id is unknown.  Flushes to disk
    /// after updating the counters.
    pub async fn record_outcome(&self, id: &SkillId, success: bool) -> Result<()> {
        {
            let mut idx = self.index.write().await;
            if let Some(skill) = idx.get_mut(id) {
                if success {
                    skill.provenance.increment_success();
                } else {
                    skill.provenance.increment_failure();
                }
            }
        }
        self.flush().await
    }

    /// Return the top `n` skills ranked by `success_count` descending.
    ///
    /// Skills with equal counts retain an unspecified order.
    pub async fn top_n(&self, n: usize) -> Vec<Skill> {
        let idx = self.index.read().await;
        let mut skills: Vec<Skill> = idx.values().cloned().collect();
        skills.sort_by(|a, b| {
            b.provenance
                .success_count
                .cmp(&a.provenance.success_count)
        });
        skills.truncate(n);
        skills
    }

    /// Total number of skills registered.
    pub async fn count(&self) -> usize {
        self.index.read().await.len()
    }

    // -----------------------------------------------------------------------
    // Persistence helpers
    // -----------------------------------------------------------------------

    /// Load skills from a JSONL file on disk.
    ///
    /// If the file does not exist, returns an empty map.  Malformed lines are
    /// logged and skipped rather than crashing the whole load.
    async fn load_from_disk(path: &Path) -> Result<HashMap<SkillId, Skill>> {
        if !tokio::fs::try_exists(path).await.unwrap_or(false) {
            return Ok(HashMap::new());
        }

        let content = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("read skill store: {}", path.display()))?;

        let mut map = HashMap::new();
        for (lineno, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Skill>(line) {
                Ok(skill) => {
                    map.insert(skill.id.clone(), skill);
                }
                Err(err) => {
                    log::warn!(
                        "skill_registry: skipping malformed line {} in {}: {}",
                        lineno + 1,
                        path.display(),
                        err
                    );
                }
            }
        }

        Ok(map)
    }

    /// Atomically write the entire index to disk.
    ///
    /// Writes to `<path>.tmp` then renames to `<path>` so readers never see
    /// a partial file.
    async fn flush(&self) -> Result<()> {
        let idx = self.index.read().await;

        let mut buf = String::new();
        for skill in idx.values() {
            let line = serde_json::to_string(skill)
                .with_context(|| format!("serialise skill {}", skill.id))?;
            buf.push_str(&line);
            buf.push('\n');
        }
        drop(idx);

        let tmp_path = self.path.with_extension("jsonl.tmp");
        tokio::fs::write(&tmp_path, &buf)
            .await
            .with_context(|| format!("write tmp skill store: {}", tmp_path.display()))?;

        tokio::fs::rename(&tmp_path, &self.path)
            .await
            .with_context(|| {
                format!(
                    "rename {} -> {}",
                    tmp_path.display(),
                    self.path.display()
                )
            })?;

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Returns a temp-dir-backed path for the skill store.
    fn tmp_skills_path(dir: &TempDir) -> PathBuf {
        dir.path().join("skills.jsonl")
    }

    fn sample_skill(id: &str, cap: CapabilityClass) -> Skill {
        Skill::new(
            SkillId::from(id),
            format!("Description for {id}"),
            cap,
            SkillHandler::from(format!("{id}_handler")),
            AgentRef::from("test-agent"),
        )
    }

    // -----------------------------------------------------------------------
    // TDD: failing tests written first, then implementation satisfies them
    // -----------------------------------------------------------------------

    /// insert + lookup round-trip.
    #[tokio::test]
    async fn register_and_lookup_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let registry = PersistentSkillRegistry::load_or_create(Some(tmp_skills_path(&dir)))
            .await
            .unwrap();

        let skill = sample_skill("fix-borrow", CapabilityClass::Compute);
        registry.register(skill).await.unwrap();

        let found = registry
            .lookup_by_id(&SkillId::from("fix-borrow"))
            .await
            .unwrap();
        assert_eq!(found.description(), "Description for fix-borrow");
        assert_eq!(found.capability(), CapabilityClass::Compute);
        assert_eq!(found.handler().as_str(), "fix-borrow_handler");
        assert_eq!(found.provenance().authored_by().as_str(), "test-agent");
    }

    /// lookup returns None for an unknown id.
    #[tokio::test]
    async fn lookup_unknown_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let registry = PersistentSkillRegistry::load_or_create(Some(tmp_skills_path(&dir)))
            .await
            .unwrap();

        let result = registry.lookup_by_id(&SkillId::from("ghost")).await;
        assert!(result.is_none());
    }

    /// `record_outcome` increments the correct counter.
    #[tokio::test]
    async fn record_outcome_updates_counts() {
        let dir = tempfile::tempdir().unwrap();
        let registry = PersistentSkillRegistry::load_or_create(Some(tmp_skills_path(&dir)))
            .await
            .unwrap();

        registry
            .register(sample_skill("deploy", CapabilityClass::Act))
            .await
            .unwrap();

        let id = SkillId::from("deploy");

        registry.record_outcome(&id, true).await.unwrap();
        registry.record_outcome(&id, true).await.unwrap();
        registry.record_outcome(&id, false).await.unwrap();

        let skill = registry.lookup_by_id(&id).await.unwrap();
        assert_eq!(skill.provenance().success_count(), 2);
        assert_eq!(skill.provenance().failure_count(), 1);
    }

    /// `record_outcome` for an unknown id is a silent no-op.
    #[tokio::test]
    async fn record_outcome_unknown_id_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let registry = PersistentSkillRegistry::load_or_create(Some(tmp_skills_path(&dir)))
            .await
            .unwrap();

        // Should not panic or error.
        registry
            .record_outcome(&SkillId::from("ghost"), true)
            .await
            .unwrap();

        assert_eq!(registry.count().await, 0);
    }

    /// `top_n` returns skills sorted by `success_count` descending.
    #[tokio::test]
    async fn top_n_ordering() {
        let dir = tempfile::tempdir().unwrap();
        let registry = PersistentSkillRegistry::load_or_create(Some(tmp_skills_path(&dir)))
            .await
            .unwrap();

        registry
            .register(sample_skill("a", CapabilityClass::Sense))
            .await
            .unwrap();
        registry
            .register(sample_skill("b", CapabilityClass::Sense))
            .await
            .unwrap();
        registry
            .register(sample_skill("c", CapabilityClass::Sense))
            .await
            .unwrap();

        // Give "b" the highest count and "c" the second highest.
        for _ in 0..5 {
            registry
                .record_outcome(&SkillId::from("b"), true)
                .await
                .unwrap();
        }
        for _ in 0..3 {
            registry
                .record_outcome(&SkillId::from("c"), true)
                .await
                .unwrap();
        }
        registry
            .record_outcome(&SkillId::from("a"), true)
            .await
            .unwrap();

        let top = registry.top_n(2).await;
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].id().as_str(), "b");
        assert_eq!(top[1].id().as_str(), "c");
    }

    /// `top_n` with n larger than count returns all skills.
    #[tokio::test]
    async fn top_n_larger_than_count_returns_all() {
        let dir = tempfile::tempdir().unwrap();
        let registry = PersistentSkillRegistry::load_or_create(Some(tmp_skills_path(&dir)))
            .await
            .unwrap();

        registry
            .register(sample_skill("x", CapabilityClass::Reflect))
            .await
            .unwrap();

        let top = registry.top_n(100).await;
        assert_eq!(top.len(), 1);
    }

    /// `search_by_capability` filters correctly.
    #[tokio::test]
    async fn search_by_capability_filters() {
        let dir = tempfile::tempdir().unwrap();
        let registry = PersistentSkillRegistry::load_or_create(Some(tmp_skills_path(&dir)))
            .await
            .unwrap();

        registry
            .register(sample_skill("sense-1", CapabilityClass::Sense))
            .await
            .unwrap();
        registry
            .register(sample_skill("sense-2", CapabilityClass::Sense))
            .await
            .unwrap();
        registry
            .register(sample_skill("compute-1", CapabilityClass::Compute))
            .await
            .unwrap();

        let sense_skills = registry
            .search_by_capability(CapabilityClass::Sense)
            .await;
        assert_eq!(sense_skills.len(), 2);
        for s in &sense_skills {
            assert_eq!(s.capability(), CapabilityClass::Sense);
        }

        let compute_skills = registry
            .search_by_capability(CapabilityClass::Compute)
            .await;
        assert_eq!(compute_skills.len(), 1);
        assert_eq!(compute_skills[0].id().as_str(), "compute-1");
    }

    /// `search_by_capability` returns empty when none match.
    #[tokio::test]
    async fn search_by_capability_no_match_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let registry = PersistentSkillRegistry::load_or_create(Some(tmp_skills_path(&dir)))
            .await
            .unwrap();

        registry
            .register(sample_skill("act-1", CapabilityClass::Act))
            .await
            .unwrap();

        let results = registry
            .search_by_capability(CapabilityClass::Coordinate)
            .await;
        assert!(results.is_empty());
    }

    /// Persistence: write then re-load recovers all skills.
    #[tokio::test]
    async fn file_persistence_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = tmp_skills_path(&dir);

        // Write phase.
        {
            let registry = PersistentSkillRegistry::load_or_create(Some(path.clone()))
                .await
                .unwrap();

            registry
                .register(sample_skill("persist-me", CapabilityClass::Reflect))
                .await
                .unwrap();
            registry
                .record_outcome(&SkillId::from("persist-me"), true)
                .await
                .unwrap();
            registry
                .record_outcome(&SkillId::from("persist-me"), false)
                .await
                .unwrap();
        }

        // Re-load phase (new registry instance from the same file).
        {
            let registry = PersistentSkillRegistry::load_or_create(Some(path))
                .await
                .unwrap();

            let skill = registry
                .lookup_by_id(&SkillId::from("persist-me"))
                .await
                .unwrap();

            assert_eq!(skill.description(), "Description for persist-me");
            assert_eq!(skill.capability(), CapabilityClass::Reflect);
            assert_eq!(skill.provenance().success_count(), 1);
            assert_eq!(skill.provenance().failure_count(), 1);
        }
    }

    /// Overwriting a skill via `register` replaces the old record.
    #[tokio::test]
    async fn register_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let registry = PersistentSkillRegistry::load_or_create(Some(tmp_skills_path(&dir)))
            .await
            .unwrap();

        let id = SkillId::from("overwrite-me");

        let skill_v1 = Skill::new(
            id.clone(),
            "Version 1".into(),
            CapabilityClass::Sense,
            SkillHandler::from("handler_v1"),
            AgentRef::from("agent-a"),
        );
        let skill_v2 = Skill::new(
            id.clone(),
            "Version 2".into(),
            CapabilityClass::Act,
            SkillHandler::from("handler_v2"),
            AgentRef::from("agent-b"),
        );

        registry.register(skill_v1).await.unwrap();
        registry.register(skill_v2).await.unwrap();

        assert_eq!(registry.count().await, 1);
        let found = registry.lookup_by_id(&id).await.unwrap();
        assert_eq!(found.description(), "Version 2");
        assert_eq!(found.capability(), CapabilityClass::Act);
    }

    /// Empty registry: `top_n` returns an empty vec, not a panic.
    #[tokio::test]
    async fn top_n_on_empty_registry() {
        let dir = tempfile::tempdir().unwrap();
        let registry = PersistentSkillRegistry::load_or_create(Some(tmp_skills_path(&dir)))
            .await
            .unwrap();

        let top = registry.top_n(5).await;
        assert!(top.is_empty());
    }

    /// `SkillProvenance::success_rate` returns None when no outcomes recorded.
    #[test]
    fn success_rate_none_when_no_outcomes() {
        let prov = SkillProvenance::new(AgentRef::from("a"));
        assert!(prov.success_rate().is_none());
    }

    /// `SkillProvenance::success_rate` computes correctly.
    #[test]
    fn success_rate_correct() {
        let mut prov = SkillProvenance::new(AgentRef::from("a"));
        prov.increment_success();
        prov.increment_success();
        prov.increment_success();
        prov.increment_failure();
        let rate = prov.success_rate().unwrap();
        assert!((rate - 0.75).abs() < 1e-10);
    }
}
