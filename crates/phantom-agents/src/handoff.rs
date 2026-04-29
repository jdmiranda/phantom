//! Handoff context — structured transfer of state between agents.
//!
//! When an agent hands off a task to another agent (e.g. Orchestrator →
//! Specialist) the receiving agent needs context: what was attempted, what
//! failed, and what is already known.  [`HandoffContext`] packages that
//! information and is embedded in the receiving agent's initial system-prompt
//! block so it can resume without re-discovering ground the delegator already
//! covered.
//!
//! # Construction
//!
//! ```rust
//! use phantom_agents::handoff::HandoffContext;
//! use phantom_agents::agent::AgentTask;
//!
//! let ctx = HandoffContext::builder(1, 2, AgentTask::FreeForm { prompt: "do x".into() })
//!     .summary("Completed the first sub-step; blocked on auth.")
//!     .failed_attempt("tried `curl -X POST /auth` — 403")
//!     .memory_ref("mem-block-a1b2")
//!     .build();
//!
//! let prompt_block = ctx.to_prompt_block();
//! assert!(prompt_block.contains("HANDOFF CONTEXT"));
//! ```

use serde::{Deserialize, Serialize};

use crate::agent::AgentTask;
use crate::correlation::CorrelationId;
use crate::role::AgentId;

// ---------------------------------------------------------------------------
// HandoffContext
// ---------------------------------------------------------------------------

/// Context transferred from a delegating agent to the agent it spawns.
///
/// All fields are private; access them through the typed accessors or
/// serialise the struct with serde. Use [`HandoffContext::builder`] to
/// construct.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffContext {
    /// Agent id that is transferring the task.
    from_agent: AgentId,
    /// Agent id that is receiving the task.
    to_agent: AgentId,
    /// The task being transferred.
    task: AgentTask,
    /// Human-readable summary of what `from_agent` accomplished before
    /// handing off. Empty string means "nothing done yet".
    summary: String,
    /// Descriptions of approaches that were already tried and failed.
    /// Prevents the receiving agent from repeating known dead ends.
    failed_attempts: Vec<String>,
    /// Identifiers of memory blocks the receiving agent should consult.
    memory_refs: Vec<String>,
    /// Optional causality token linking this handoff to a pipeline run.
    correlation_id: Option<CorrelationId>,
}

impl HandoffContext {
    /// Start building a [`HandoffContext`].
    ///
    /// `from_agent` → `to_agent` is the direction of the handoff.
    /// `task` is the task being transferred.
    pub fn builder(from_agent: AgentId, to_agent: AgentId, task: AgentTask) -> HandoffContextBuilder {
        HandoffContextBuilder {
            from_agent,
            to_agent,
            task,
            summary: String::new(),
            failed_attempts: Vec::new(),
            memory_refs: Vec::new(),
            correlation_id: None,
        }
    }

    // ── Accessors ─────────────────────────────────────────────────────────

    /// The agent that is handing off.
    pub fn from_agent(&self) -> AgentId {
        self.from_agent
    }

    /// The agent that is receiving.
    pub fn to_agent(&self) -> AgentId {
        self.to_agent
    }

    /// The task being transferred.
    pub fn task(&self) -> &AgentTask {
        &self.task
    }

    /// What the from-agent accomplished before handing off.
    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// Descriptions of already-tried, already-failed approaches.
    pub fn failed_attempts(&self) -> &[String] {
        &self.failed_attempts
    }

    /// Memory-block identifiers the receiving agent should consult.
    pub fn memory_refs(&self) -> &[String] {
        &self.memory_refs
    }

    /// Correlation id linking this handoff to a pipeline run.
    pub fn correlation_id(&self) -> Option<CorrelationId> {
        self.correlation_id
    }

    // ── Helpers ───────────────────────────────────────────────────────────

    /// Build a system-prompt block that can be prepended to the receiving
    /// agent's initial [`crate::agent::AgentMessage::System`] message.
    ///
    /// The block is intentionally terse: each section appears only when it
    /// carries content.
    #[must_use]
    pub fn to_prompt_block(&self) -> String {
        let mut lines: Vec<String> = Vec::new();

        lines.push("=== HANDOFF CONTEXT ===".into());
        lines.push(format!(
            "From agent: {from}  →  To agent: {to}",
            from = self.from_agent,
            to = self.to_agent,
        ));

        if let Some(cid) = self.correlation_id {
            lines.push(format!("Correlation: {cid}"));
        }

        if !self.summary.is_empty() {
            lines.push(String::new());
            lines.push("What was accomplished:".into());
            lines.push(format!("  {}", self.summary));
        }

        if !self.failed_attempts.is_empty() {
            lines.push(String::new());
            lines.push("Failed attempts (do not repeat these):".into());
            for attempt in &self.failed_attempts {
                lines.push(format!("  - {attempt}"));
            }
        }

        if !self.memory_refs.is_empty() {
            lines.push(String::new());
            lines.push("Relevant memory blocks:".into());
            for mem in &self.memory_refs {
                lines.push(format!("  - {mem}"));
            }
        }

        lines.push("=== END HANDOFF CONTEXT ===".into());
        lines.join("\n")
    }
}

// ---------------------------------------------------------------------------
// HandoffContextBuilder
// ---------------------------------------------------------------------------

/// Incremental builder for [`HandoffContext`].
///
/// Obtain via [`HandoffContext::builder`].
pub struct HandoffContextBuilder {
    from_agent: AgentId,
    to_agent: AgentId,
    task: AgentTask,
    summary: String,
    failed_attempts: Vec<String>,
    memory_refs: Vec<String>,
    correlation_id: Option<CorrelationId>,
}

impl HandoffContextBuilder {
    /// Set what the from-agent accomplished.
    pub fn summary(mut self, s: impl Into<String>) -> Self {
        self.summary = s.into();
        self
    }

    /// Record one failed attempt description.
    pub fn failed_attempt(mut self, desc: impl Into<String>) -> Self {
        self.failed_attempts.push(desc.into());
        self
    }

    /// Record multiple failed-attempt descriptions at once.
    pub fn failed_attempts(mut self, descs: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.failed_attempts.extend(descs.into_iter().map(Into::into));
        self
    }

    /// Append one memory-block reference.
    pub fn memory_ref(mut self, id: impl Into<String>) -> Self {
        self.memory_refs.push(id.into());
        self
    }

    /// Append multiple memory-block references at once.
    pub fn memory_refs(mut self, ids: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.memory_refs.extend(ids.into_iter().map(Into::into));
        self
    }

    /// Attach a correlation id (links this handoff to a pipeline run).
    pub fn correlation_id(mut self, cid: CorrelationId) -> Self {
        self.correlation_id = Some(cid);
        self
    }

    /// Finalise and return the [`HandoffContext`].
    pub fn build(self) -> HandoffContext {
        HandoffContext {
            from_agent: self.from_agent,
            to_agent: self.to_agent,
            task: self.task,
            summary: self.summary,
            failed_attempts: self.failed_attempts,
            memory_refs: self.memory_refs,
            correlation_id: self.correlation_id,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentTask;
    use crate::correlation::CorrelationId;

    fn free_task(p: &str) -> AgentTask {
        AgentTask::FreeForm { prompt: p.into() }
    }

    // ── Construction ─────────────────────────────────────────────────────────

    #[test]
    fn builder_sets_required_fields() {
        let ctx = HandoffContext::builder(1, 2, free_task("do x")).build();
        assert_eq!(ctx.from_agent(), 1);
        assert_eq!(ctx.to_agent(), 2);
        assert_eq!(ctx.summary(), "");
        assert!(ctx.failed_attempts().is_empty());
        assert!(ctx.memory_refs().is_empty());
        assert!(ctx.correlation_id().is_none());
    }

    #[test]
    fn builder_sets_summary() {
        let ctx = HandoffContext::builder(1, 2, free_task("task"))
            .summary("completed phase 1")
            .build();
        assert_eq!(ctx.summary(), "completed phase 1");
    }

    #[test]
    fn builder_empty_failed_attempts() {
        let ctx = HandoffContext::builder(1, 2, free_task("task")).build();
        assert!(ctx.failed_attempts().is_empty(), "fresh context must have no failed attempts");
    }

    #[test]
    fn builder_adds_failed_attempts_one_by_one() {
        let ctx = HandoffContext::builder(1, 2, free_task("task"))
            .failed_attempt("tried curl — 403")
            .failed_attempt("tried wget — timeout")
            .build();
        assert_eq!(ctx.failed_attempts().len(), 2);
        assert_eq!(ctx.failed_attempts()[0], "tried curl — 403");
        assert_eq!(ctx.failed_attempts()[1], "tried wget — timeout");
    }

    #[test]
    fn builder_adds_failed_attempts_batch() {
        let ctx = HandoffContext::builder(1, 2, free_task("task"))
            .failed_attempts(["a", "b", "c"])
            .build();
        assert_eq!(ctx.failed_attempts().len(), 3);
    }

    #[test]
    fn builder_adds_memory_refs() {
        let ctx = HandoffContext::builder(1, 2, free_task("task"))
            .memory_ref("mem-a1b2")
            .memory_ref("mem-c3d4")
            .build();
        assert_eq!(ctx.memory_refs().len(), 2);
        assert_eq!(ctx.memory_refs()[0], "mem-a1b2");
    }

    #[test]
    fn builder_adds_memory_refs_batch() {
        let ctx = HandoffContext::builder(1, 2, free_task("task"))
            .memory_refs(["x", "y"])
            .build();
        assert_eq!(ctx.memory_refs().len(), 2);
    }

    #[test]
    fn builder_sets_correlation_id() {
        let cid = CorrelationId::new();
        let ctx = HandoffContext::builder(1, 2, free_task("task"))
            .correlation_id(cid)
            .build();
        assert_eq!(ctx.correlation_id(), Some(cid));
    }

    #[test]
    fn builder_correlation_id_absent_by_default() {
        let ctx = HandoffContext::builder(1, 2, free_task("task")).build();
        assert!(ctx.correlation_id().is_none());
    }

    // ── Serde round-trip ──────────────────────────────────────────────────────

    #[test]
    fn handoff_context_serde_round_trip() {
        let cid = CorrelationId::new();
        let original = HandoffContext::builder(10, 20, free_task("refactor parser"))
            .summary("parsed top-level exprs")
            .failed_attempt("tried recursive descent — stack overflow")
            .memory_ref("mem-xyz")
            .correlation_id(cid)
            .build();

        let json = serde_json::to_string(&original).unwrap();
        let restored: HandoffContext = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.from_agent(), 10);
        assert_eq!(restored.to_agent(), 20);
        assert_eq!(restored.summary(), "parsed top-level exprs");
        assert_eq!(restored.failed_attempts().len(), 1);
        assert_eq!(restored.memory_refs().len(), 1);
        assert_eq!(restored.correlation_id(), Some(cid));
    }

    // ── prompt block ──────────────────────────────────────────────────────────

    #[test]
    fn prompt_block_contains_required_sections() {
        let ctx = HandoffContext::builder(1, 2, free_task("task"))
            .summary("did step 1")
            .failed_attempt("tried X — failed")
            .memory_ref("mem-abc")
            .build();

        let block = ctx.to_prompt_block();
        assert!(block.contains("HANDOFF CONTEXT"), "must have section header");
        assert!(block.contains("From agent: 1"), "must identify from-agent");
        assert!(block.contains("To agent: 2"), "must identify to-agent");
        assert!(block.contains("did step 1"), "must include summary");
        assert!(block.contains("tried X — failed"), "must list failed attempt");
        assert!(block.contains("mem-abc"), "must list memory ref");
        assert!(block.contains("END HANDOFF CONTEXT"), "must have section footer");
    }

    #[test]
    fn prompt_block_includes_correlation_id_when_present() {
        let cid = CorrelationId::new();
        let ctx = HandoffContext::builder(1, 2, free_task("task"))
            .correlation_id(cid)
            .build();
        let block = ctx.to_prompt_block();
        assert!(
            block.contains(&cid.to_string()),
            "prompt block must embed the correlation id; got:\n{block}",
        );
    }

    #[test]
    fn prompt_block_omits_empty_sections() {
        // No summary, no failed attempts, no memory refs.
        let ctx = HandoffContext::builder(5, 7, free_task("x")).build();
        let block = ctx.to_prompt_block();
        // Section labels must not appear when the content is absent.
        assert!(
            !block.contains("What was accomplished"),
            "summary section must be absent when summary is empty; got:\n{block}",
        );
        assert!(
            !block.contains("Failed attempts"),
            "failed-attempts section must be absent when empty; got:\n{block}",
        );
        assert!(
            !block.contains("Relevant memory blocks"),
            "memory-refs section must be absent when empty; got:\n{block}",
        );
    }

    // ── multi-agent chain ──────────────────────────────────────────────────────

    #[test]
    fn multi_agent_chain_correlation_id_propagates() {
        // Orchestrator → Specialist A → Specialist B, all sharing a correlation id.
        let cid = CorrelationId::new();
        let hop1 = HandoffContext::builder(1, 2, free_task("phase A"))
            .summary("done with A")
            .correlation_id(cid)
            .build();
        let hop2 = HandoffContext::builder(2, 3, free_task("phase B"))
            .summary("done with B")
            .failed_attempt("step X failed")
            .correlation_id(cid)
            .build();

        assert_eq!(hop1.correlation_id(), Some(cid));
        assert_eq!(hop2.correlation_id(), Some(cid));
        assert_ne!(hop1.from_agent(), hop2.from_agent());
        assert_eq!(hop1.to_agent(), hop2.from_agent(), "agent 2 is both receiver and sender");
    }
}
