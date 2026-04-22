//! The AI brain thread — an event-driven OODA loop.
//!
//! Spawned once at application startup via [`spawn_brain`]. Communicates with
//! the rest of the system through [`BrainHandle`] (channels). Blocks on the
//! event receiver and only consumes CPU when an event arrives.

use std::sync::mpsc;

use phantom_context::ProjectContext;
use phantom_memory::MemoryStore;

use crate::events::{AiAction, AiEvent};
use crate::router::{BrainRouter, RouterConfig, TaskClassifier};
use crate::scoring::UtilityScorer;

// ---------------------------------------------------------------------------
// BrainHandle
// ---------------------------------------------------------------------------

/// Handle for communicating with the AI brain from other threads.
///
/// Cheaply cloneable on the sender side (use `clone_sender` to fan-in events
/// from multiple producers). The action receiver is single-consumer.
pub struct BrainHandle {
    pub(crate) event_tx: mpsc::Sender<AiEvent>,
    pub(crate) action_rx: mpsc::Receiver<AiAction>,
}

impl BrainHandle {
    /// Send an event to the brain (non-blocking).
    ///
    /// Returns `Err` if the brain thread has shut down.
    pub fn send_event(&self, event: AiEvent) -> Result<(), mpsc::SendError<AiEvent>> {
        self.event_tx.send(event)
    }

    /// Poll for an action from the brain (non-blocking).
    ///
    /// Returns `None` if no action is available yet.
    pub fn try_recv_action(&self) -> Option<AiAction> {
        self.action_rx.try_recv().ok()
    }

    /// Get a clone of the event sender (for fan-in from multiple threads).
    pub fn event_sender(&self) -> mpsc::Sender<AiEvent> {
        self.event_tx.clone()
    }
}

// ---------------------------------------------------------------------------
// BrainConfig
// ---------------------------------------------------------------------------

/// Configuration for the AI brain thread.
pub struct BrainConfig {
    /// The project root directory.
    pub project_dir: String,
    /// Whether the brain should emit suggestions.
    pub enable_suggestions: bool,
    /// Whether the brain should update project memory.
    pub enable_memory: bool,
    /// Minimum score an action must exceed to be emitted (default: 0.5).
    pub quiet_threshold: f32,
    /// Router configuration. If `None`, uses default (heuristic + ollama + claude).
    pub router: Option<RouterConfig>,
}

impl Default for BrainConfig {
    fn default() -> Self {
        Self {
            project_dir: ".".into(),
            enable_suggestions: true,
            enable_memory: true,
            quiet_threshold: 0.5,
            router: None,
        }
    }
}

// ---------------------------------------------------------------------------
// spawn_brain
// ---------------------------------------------------------------------------

/// Spawn the AI brain thread. Returns a handle for bidirectional communication.
///
/// The brain runs on a dedicated OS thread named `phantom-brain`. It blocks on
/// the event channel and processes events through the OODA cycle. Send
/// [`AiEvent::Shutdown`] to stop it gracefully.
pub fn spawn_brain(config: BrainConfig) -> BrainHandle {
    let (event_tx, event_rx) = mpsc::channel();
    let (action_tx, action_rx) = mpsc::channel();

    std::thread::Builder::new()
        .name("phantom-brain".into())
        .spawn(move || {
            brain_loop(config, event_rx, action_tx);
        })
        .expect("failed to spawn brain thread");

    BrainHandle { event_tx, action_rx }
}

// ---------------------------------------------------------------------------
// brain_loop — the OODA cycle
// ---------------------------------------------------------------------------

/// The main brain loop — event-driven OODA cycle.
///
/// Runs on its own thread. Blocks on the event receiver. For each event:
/// 1. **Observe**: receive the event.
/// 2. **Orient**: update the world model (context, memory, history).
/// 3. **Decide**: score all possible actions via [`UtilityScorer`].
/// 4. **Act**: emit the winning action if it exceeds the quiet threshold.
fn brain_loop(
    config: BrainConfig,
    event_rx: mpsc::Receiver<AiEvent>,
    action_tx: mpsc::Sender<AiAction>,
) {
    let context = ProjectContext::detect(std::path::Path::new(&config.project_dir));
    let memory = MemoryStore::open(&config.project_dir)
        .unwrap_or_else(|e| {
            log::warn!("failed to open memory store: {e}, using fallback");
            // Create a fallback in-memory-only store by using /tmp.
            MemoryStore::open_in(&config.project_dir, std::path::Path::new("/tmp/phantom-brain-fallback"))
                .expect("failed to create fallback memory store")
        });
    let mut scorer = UtilityScorer::new();
    let mut router = BrainRouter::new(config.router.unwrap_or_default());

    log::info!(
        "AI brain online — project: {} [{:?}]",
        context.name,
        context.project_type
    );

    loop {
        // OBSERVE: block until an event arrives.
        let event = match event_rx.recv() {
            Ok(e) => e,
            Err(_) => break, // channel closed — all senders dropped
        };

        // Handle shutdown.
        if matches!(event, AiEvent::Shutdown) {
            log::info!("AI brain shutting down");
            break;
        }

        // ORIENT: update world model.
        orient(&event, &mut scorer);

        // CLASSIFY: determine task complexity and select backend cascade.
        let complexity = TaskClassifier::classify(&event);
        let backends = router.route(complexity);

        log::debug!(
            "AI brain: complexity={:?}, routed to [{}]",
            complexity,
            backends
                .iter()
                .map(|b| b.name.as_str())
                .collect::<Vec<_>>()
                .join(" → ")
        );

        // For now, all backends cascade to heuristic scoring.
        // Future: Ollama/Claude backends will handle Simple/Complex tasks
        // directly, with the router recording latency and success metrics.
        let _ = &mut router; // suppress unused_mut until backends do real work

        // DECIDE: score all actions, pick the best.
        let best = scorer.evaluate(&event, &context, &memory);

        log::debug!(
            "AI brain: {} (score: {:.2}, reason: {})",
            action_name(&best.action),
            best.score,
            best.reason
        );

        // ACT: only emit if score exceeds threshold and suggestions are enabled.
        let dominated_by_quiet = best.score <= config.quiet_threshold;
        let suppressed = !config.enable_suggestions && matches!(best.action, AiAction::ShowSuggestion { .. });
        let memory_suppressed = !config.enable_memory && matches!(best.action, AiAction::UpdateMemory { .. });

        if !dominated_by_quiet && !suppressed && !memory_suppressed {
            if action_tx.send(best.action).is_err() {
                break; // render thread dropped its receiver
            }
        }
    }

    log::info!("AI brain thread exiting");
}

// ---------------------------------------------------------------------------
// orient — update world model
// ---------------------------------------------------------------------------

/// Update the scorer's state based on the incoming event.
fn orient(event: &AiEvent, scorer: &mut UtilityScorer) {
    match event {
        AiEvent::CommandComplete(parsed) => {
            scorer.last_had_errors = !parsed.errors.is_empty();
            // User just ran a command — reset idle tracking.
            scorer.user_acted();
        }
        AiEvent::UserIdle { seconds } => {
            scorer.idle_time = *seconds;
            scorer.decay_chattiness(*seconds);
        }
        AiEvent::Interrupt(_) | AiEvent::AgentRequest(_) => {
            // Explicit user action — reset chattiness.
            scorer.user_acted();
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// action_name — for debug logging
// ---------------------------------------------------------------------------

/// Human-readable label for an action (used in log output).
pub(crate) fn action_name(action: &AiAction) -> &str {
    match action {
        AiAction::ShowSuggestion { .. } => "suggest",
        AiAction::SpawnAgent(_) => "spawn_agent",
        AiAction::UpdateMemory { .. } => "update_memory",
        AiAction::ShowNotification(_) => "notify",
        AiAction::RunCommand(_) => "run_command",
        AiAction::DoNothing => "quiet",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_name_covers_all_variants() {
        assert_eq!(action_name(&AiAction::DoNothing), "quiet");
        assert_eq!(
            action_name(&AiAction::ShowSuggestion {
                text: "x".into(),
                options: vec![]
            }),
            "suggest"
        );
        assert_eq!(
            action_name(&AiAction::SpawnAgent(phantom_agents::AgentTask::FreeForm {
                prompt: "x".into()
            })),
            "spawn_agent"
        );
        assert_eq!(
            action_name(&AiAction::UpdateMemory {
                key: "k".into(),
                value: "v".into()
            }),
            "update_memory"
        );
        assert_eq!(
            action_name(&AiAction::ShowNotification("n".into())),
            "notify"
        );
        assert_eq!(action_name(&AiAction::RunCommand("ls".into())), "run_command");
    }

    #[test]
    fn orient_command_complete_resets_idle() {
        let mut scorer = UtilityScorer::new();
        scorer.idle_time = 30.0;
        scorer.chattiness = 0.3;

        let parsed = phantom_semantic::ParsedOutput {
            command: "ls".into(),
            command_type: phantom_semantic::CommandType::Shell,
            exit_code: Some(0),
            content_type: phantom_semantic::ContentType::PlainText,
            errors: vec![],
            warnings: vec![],
            duration_ms: Some(10),
            raw_output: "file1\nfile2".into(),
        };

        orient(&AiEvent::CommandComplete(parsed), &mut scorer);

        assert_eq!(scorer.idle_time, 0.0);
        assert_eq!(scorer.chattiness, 0.0);
    }

    #[test]
    fn orient_user_idle_updates_idle_time() {
        let mut scorer = UtilityScorer::new();
        orient(&AiEvent::UserIdle { seconds: 15.0 }, &mut scorer);
        assert_eq!(scorer.idle_time, 15.0);
    }

    #[test]
    fn orient_interrupt_resets_chattiness() {
        let mut scorer = UtilityScorer::new();
        scorer.chattiness = 0.5;
        scorer.suggestions_since_input = 3;

        orient(&AiEvent::Interrupt("help".into()), &mut scorer);

        assert_eq!(scorer.chattiness, 0.0);
        assert_eq!(scorer.suggestions_since_input, 0);
    }
}
