//! Safety rails for the builder.
//!
//! Three knobs gate autonomy:
//!
//! 1. **`max_prs_per_hour`** — clamps the brain's per-hour rate-limiter cap.
//!    The brain still applies its own band-adjusted multiplier on top, but
//!    this is the absolute ceiling the builder enforces.
//! 2. **`max_concurrent_agents`** — every seeded loop spec's `max_concurrent`
//!    field is rewritten to this value. The default templates ship with 1;
//!    the builder may override.
//! 3. **`dry_run`** — when true, the brain still ticks (scoring, audit log,
//!    rate-limit accounting), but the forwarder thread swaps the production
//!    [`phantom_loop::LoopQueueActionHandler`] for a [`DryRunActionHandler`].
//!    The substrate driver never sees a request — useful for "show me what
//!    phantom would do" sanity-runs.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use phantom_brain::dispatch::ActionHandler;
use phantom_brain::events::{ConnectionState, SuggestionOption};
use phantom_agents::peer_routing::RemoteMessageContent;
use phantom_agents::{AgentId, AgentTask};
use phantom_agents::agent::PauseReason;
use phantom_agents::dispatch::Disposition;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// BuilderSafetyConfig
// ---------------------------------------------------------------------------

/// Tunable safety caps for the builder.
///
/// The defaults match the design intent of "active but rate-limited" — 5 PRs
/// per hour, 2 concurrent agents, dry-run OFF. Operators tighten or loosen
/// via CLI flags.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuilderSafetyConfig {
    /// Absolute ceiling on the brain's per-hour enqueue rate. Default: 5.
    ///
    /// The brain's internal [`phantom_brain::self_improvement::RateLimiter`]
    /// applies band-adjusted multipliers (conservative halves, aggressive
    /// doubles) on top of this value, so the effective cap can be 2–10 PRs/h
    /// depending on the trust band.
    pub max_prs_per_hour: u32,

    /// Maximum simultaneous agent runs across every loop. Default: 2.
    ///
    /// Wired into the seeded loop specs' `max_concurrent` field. Per-spec
    /// values higher than this cap are clamped down on write.
    pub max_concurrent_agents: u8,

    /// When true, the brain runs end-to-end (poll → score → gate → enqueue
    /// decision) but the forwarder uses [`DryRunActionHandler`] so no
    /// substrate spawn ever fires. Default: false.
    pub dry_run: bool,
}

impl Default for BuilderSafetyConfig {
    fn default() -> Self {
        Self {
            max_prs_per_hour: 5,
            max_concurrent_agents: 2,
            dry_run: false,
        }
    }
}

// ---------------------------------------------------------------------------
// DryRunActionHandler
// ---------------------------------------------------------------------------

/// Drop-in replacement for [`phantom_loop::LoopQueueActionHandler`] that
/// logs every would-be enqueue instead of pushing it onto the queue.
///
/// Tracks a counter for assertions in tests (`builder_smoke.rs --dry-run`
/// variant) and emits a tracing event with the queue name + payload so the
/// operator can audit the brain's choices without committing any PRs.
#[derive(Debug, Default)]
pub struct DryRunActionHandler {
    /// Count of enqueue actions intercepted. Reads atomically — the brain
    /// forwarder thread is the only writer, but assertions in tests run on
    /// a separate thread.
    pub enqueue_count: Arc<AtomicUsize>,
}

impl DryRunActionHandler {
    /// Build a fresh handler with a zeroed counter.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the current intercepted-enqueue count.
    #[must_use]
    pub fn count(&self) -> usize {
        self.enqueue_count.load(Ordering::Relaxed)
    }

    /// Clone-share the counter Arc with a test.
    #[must_use]
    pub fn counter(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.enqueue_count)
    }
}

impl ActionHandler for DryRunActionHandler {
    fn enqueue_loop_message(
        &mut self,
        queue: String,
        from_source: String,
        payload: serde_json::Value,
    ) {
        let count = self.enqueue_count.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::info!(
            queue = %queue,
            from_source = %from_source,
            payload = %payload,
            count = count,
            "[dry-run] brain would enqueue loop message (no-op)",
        );
    }

    // Every other ActionHandler method is a no-op. We do not inherit from
    // NoopInner because that is a private-ish helper in phantom-loop and we
    // do not want to take a hard dependency on its symbol exposure.
    fn show_suggestion(&mut self, _text: String, _options: Vec<SuggestionOption>) {}
    fn show_notification(&mut self, _msg: String) {}
    fn update_memory(&mut self, _key: String, _value: String) {}
    fn spawn_agent(
        &mut self,
        _task: AgentTask,
        _spawn_tag: Option<u64>,
        _disposition: Disposition,
    ) {
    }
    fn console_reply(&mut self, _reply: String) {}
    fn run_command(&mut self, _cmd: String) {}
    fn dismiss_adapter(&mut self, _app_id: u32) {}
    fn agent_flatlined(&mut self, _id: AgentId, _reason: String) {}
    fn suggest(&mut self, _action: String, _rationale: String, _confidence: f32) {}
    fn quarantine_agent(&mut self, _agent_id: AgentId, _denial_count: usize) {}
    fn agent_quarantined(&mut self, _agent_id: AgentId, _denial_count: usize) {}
    fn checkpoint_reached(&mut self, _step_idx: usize, _description: String) {}
    fn pause_agent(&mut self, _agent_id: AgentId, _reason: PauseReason) {}
    fn resume_agent(&mut self, _agent_id: AgentId) {}
    fn update_connection_state(&mut self, _state: ConnectionState) {}
    fn set_offline_mode(&mut self, _enabled: bool) {}
    fn deliver_inbound_relay(&mut self, _agent_id: AgentId, _content: RemoteMessageContent) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_brain::events::AiAction;
    use serde_json::json;

    #[test]
    fn default_caps_match_the_documented_intent() {
        let s = BuilderSafetyConfig::default();
        assert_eq!(s.max_prs_per_hour, 5);
        assert_eq!(s.max_concurrent_agents, 2);
        assert!(!s.dry_run);
    }

    #[test]
    fn dry_run_handler_counts_enqueue_attempts_without_pushing() {
        let mut h = DryRunActionHandler::new();
        AiAction::EnqueueLoopMessage {
            queue: "implementer-queue".into(),
            from_source: "gh-issues".into(),
            payload: json!({"external_id": "gh-issue:1"}),
        }
        .execute(&mut h);
        AiAction::EnqueueLoopMessage {
            queue: "implementer-queue".into(),
            from_source: "gh-issues".into(),
            payload: json!({"external_id": "gh-issue:2"}),
        }
        .execute(&mut h);
        assert_eq!(h.count(), 2);
    }

    #[test]
    fn dry_run_handler_ignores_non_enqueue_actions() {
        let mut h = DryRunActionHandler::new();
        AiAction::ShowNotification("noise".into()).execute(&mut h);
        AiAction::ConsoleReply("noise".into()).execute(&mut h);
        AiAction::DoNothing.execute(&mut h);
        assert_eq!(h.count(), 0);
    }
}
