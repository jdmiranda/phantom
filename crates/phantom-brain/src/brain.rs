//! The AI brain thread — an event-driven OODA loop.
//!
//! Spawned once at application startup via [`spawn_brain`]. Communicates with
//! the rest of the system through [`BrainHandle`] (channels). Blocks on the
//! event receiver and only consumes CPU when an event arrives.

use std::sync::mpsc;

use phantom_context::ProjectContext;
use phantom_memory::MemoryStore;

use crate::events::{AiAction, AiEvent};
use crate::router::{BackendKind, BrainRouter, ModelBackend, RouterConfig, TaskClassifier, TaskComplexity};
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

    // Health-check Ollama at startup.
    if crate::ollama::is_available() {
        router.set_backend_available("ollama-phi3.5", true);
        log::info!("Ollama detected — local model dispatch enabled");
    } else {
        log::info!("Ollama not detected — using heuristic-only mode");
    }

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
        // Clone the first Ollama backend info so we can release the router borrow.
        let ollama_backend: Option<ModelBackend> = router.route(complexity)
            .iter()
            .find(|b| matches!(b.kind, BackendKind::Ollama { .. }))
            .cloned()
            .cloned();

        // DECIDE: score all actions via heuristics, pick the best.
        let best = scorer.evaluate(&event, &context, &memory);

        log::debug!(
            "AI brain: complexity={:?}, {} (score: {:.2})",
            complexity,
            action_name(&best.action),
            best.score,
        );

        // ACT: only emit if score exceeds threshold and suggestions are enabled.
        let dominated_by_quiet = best.score <= config.quiet_threshold;
        let suppressed = !config.enable_suggestions && matches!(best.action, AiAction::ShowSuggestion { .. });
        let memory_suppressed = !config.enable_memory && matches!(best.action, AiAction::UpdateMemory { .. });

        if dominated_by_quiet || suppressed || memory_suppressed {
            continue;
        }

        // ENHANCE: if the winning action is a suggestion and we have an
        // error event, try to upgrade it with Ollama's local model.
        let action = enhance_with_ollama(
            best.action, &event, &context, &ollama_backend, &mut router,
        );

        // ESCALATE: for Complex tasks, if Ollama didn't enhance (still heuristic)
        // and Claude is available, escalate to the frontier model.
        let claude_backend: Option<ModelBackend> = router.route(complexity)
            .iter()
            .find(|b| matches!(b.kind, BackendKind::Claude { .. }))
            .cloned()
            .cloned();
        let action = enhance_with_claude(
            action, &event, &context, complexity, &claude_backend, &mut router,
        );

        if action_tx.send(action).is_err() {
            break; // render thread dropped its receiver
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
// enhance_with_ollama — upgrade heuristic suggestions with local model
// ---------------------------------------------------------------------------

/// If the winning action is a suggestion and an Ollama backend is available
/// and the event is an error, call the local model to generate a real
/// response. Falls back to the heuristic suggestion on any failure.
fn enhance_with_ollama(
    heuristic_action: AiAction,
    event: &AiEvent,
    context: &ProjectContext,
    ollama_backend: &Option<ModelBackend>,
    router: &mut BrainRouter,
) -> AiAction {
    // Only enhance suggestions.
    if !matches!(heuristic_action, AiAction::ShowSuggestion { .. }) {
        return heuristic_action;
    }

    let Some(backend) = ollama_backend else {
        return heuristic_action;
    };
    let model_name = match &backend.kind {
        BackendKind::Ollama { model } => model.as_str(),
        _ => return heuristic_action,
    };

    // Only generate for error events (the highest-value use case).
    let AiEvent::CommandComplete(parsed) = event else {
        return heuristic_action;
    };
    if parsed.errors.is_empty() {
        return heuristic_action;
    }

    let proj_type = format!("{:?}", context.project_type);
    let prompt = crate::ollama::build_error_triage_prompt(
        &parsed.command,
        &parsed.errors,
        &proj_type,
    );

    match crate::ollama::generate(model_name, &prompt, 150) {
        Ok((text, latency_ms)) => {
            log::info!("Ollama enhanced suggestion ({latency_ms:.0}ms): {text}");
            router.record_result(&backend.name, latency_ms, true);
            AiAction::ShowSuggestion {
                text,
                options: vec![
                    ('y', "Fix it".into()),
                    ('n', "Dismiss".into()),
                ],
            }
        }
        Err(e) => {
            log::warn!("Ollama dispatch failed, using heuristic: {e}");
            router.record_result(&backend.name, 0.0, false);
            heuristic_action
        }
    }
}

// ---------------------------------------------------------------------------
// enhance_with_claude — escalate to frontier model for complex tasks
// ---------------------------------------------------------------------------

/// If the task is Complex, Claude is available, and the current suggestion
/// could benefit from deeper analysis, call Claude for a better response.
fn enhance_with_claude(
    current_action: AiAction,
    event: &AiEvent,
    context: &ProjectContext,
    complexity: TaskComplexity,
    claude_backend: &Option<ModelBackend>,
    router: &mut BrainRouter,
) -> AiAction {
    // Only escalate Complex tasks.
    if complexity != TaskComplexity::Complex {
        return current_action;
    }

    // Only enhance suggestions.
    if !matches!(current_action, AiAction::ShowSuggestion { .. }) {
        return current_action;
    }

    let Some(backend) = claude_backend else {
        return current_action;
    };
    let model_name = match &backend.kind {
        BackendKind::Claude { model } => model.as_str(),
        _ => return current_action,
    };

    // Only generate for error events with 3+ errors (Complex classification).
    let AiEvent::CommandComplete(parsed) = event else {
        return current_action;
    };
    if parsed.errors.len() < 3 {
        return current_action;
    }

    let proj_type = format!("{:?}", context.project_type);
    let prompt = crate::claude::build_error_analysis_prompt(
        &parsed.command,
        &parsed.errors,
        &proj_type,
    );

    match crate::claude::generate(model_name, &prompt, 300) {
        Ok((text, latency_ms)) => {
            log::info!("Claude escalation ({latency_ms:.0}ms): {text}");
            router.record_result(&backend.name, latency_ms, true);
            AiAction::ShowSuggestion {
                text,
                options: vec![
                    ('y', "Fix it".into()),
                    ('n', "Dismiss".into()),
                ],
            }
        }
        Err(e) => {
            log::warn!("Claude escalation failed, using previous: {e}");
            router.record_result(&backend.name, 0.0, false);
            current_action
        }
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

    // =======================================================================
    // enhance_with_claude gating tests
    // =======================================================================

    fn test_context() -> phantom_context::ProjectContext {
        phantom_context::ProjectContext {
            root: "/tmp/test".into(),
            name: "test".into(),
            project_type: phantom_context::ProjectType::Rust,
            package_manager: phantom_context::PackageManager::Cargo,
            framework: phantom_context::Framework::None,
            commands: phantom_context::ProjectCommands {
                build: Some("cargo build".into()),
                test: Some("cargo test".into()),
                run: None,
                lint: None,
                format: None,
            },
            git: None,
            rust_version: None,
            node_version: None,
            python_version: None,
        }
    }

    fn make_error(msg: &str) -> phantom_semantic::DetectedError {
        phantom_semantic::DetectedError {
            message: msg.into(),
            error_type: phantom_semantic::ErrorType::Compiler,
            file: None,
            line: None,
            column: None,
            code: None,
            severity: phantom_semantic::Severity::Error,
            raw_line: String::new(),
            suggestion: None,
        }
    }

    fn parsed_many_errors() -> phantom_semantic::ParsedOutput {
        phantom_semantic::ParsedOutput {
            command: "cargo build".into(),
            command_type: phantom_semantic::CommandType::Cargo(phantom_semantic::CargoCommand::Build),
            exit_code: Some(1),
            content_type: phantom_semantic::ContentType::CompilerOutput,
            errors: vec![make_error("e1"), make_error("e2"), make_error("e3")],
            warnings: vec![],
            duration_ms: Some(2000),
            raw_output: "errors".into(),
        }
    }

    fn parsed_few_errors() -> phantom_semantic::ParsedOutput {
        phantom_semantic::ParsedOutput {
            command: "cargo build".into(),
            command_type: phantom_semantic::CommandType::Cargo(phantom_semantic::CargoCommand::Build),
            exit_code: Some(1),
            content_type: phantom_semantic::ContentType::CompilerOutput,
            errors: vec![make_error("e1")],
            warnings: vec![],
            duration_ms: Some(500),
            raw_output: "error".into(),
        }
    }

    fn suggestion_action() -> AiAction {
        AiAction::ShowSuggestion {
            text: "heuristic suggestion".into(),
            options: vec![],
        }
    }

    #[test]
    fn claude_skips_non_complex_tasks() {
        let ctx = test_context();
        let mut router = BrainRouter::new(RouterConfig::default());
        let backend = Some(ModelBackend::claude_default());
        let event = AiEvent::CommandComplete(parsed_many_errors());

        let action = enhance_with_claude(
            suggestion_action(), &event, &ctx,
            TaskComplexity::Simple, // not Complex
            &backend, &mut router,
        );
        // Should return original action unchanged.
        if let AiAction::ShowSuggestion { text, .. } = &action {
            assert_eq!(text, "heuristic suggestion");
        } else {
            panic!("expected ShowSuggestion, got {action:?}");
        }
    }

    #[test]
    fn claude_skips_non_suggestion_actions() {
        let ctx = test_context();
        let mut router = BrainRouter::new(RouterConfig::default());
        let backend = Some(ModelBackend::claude_default());
        let event = AiEvent::CommandComplete(parsed_many_errors());

        let action = enhance_with_claude(
            AiAction::DoNothing, &event, &ctx,
            TaskComplexity::Complex,
            &backend, &mut router,
        );
        assert!(matches!(action, AiAction::DoNothing));
    }

    #[test]
    fn claude_skips_when_no_backend() {
        let ctx = test_context();
        let mut router = BrainRouter::new(RouterConfig::default());
        let event = AiEvent::CommandComplete(parsed_many_errors());

        let action = enhance_with_claude(
            suggestion_action(), &event, &ctx,
            TaskComplexity::Complex,
            &None, // no Claude backend
            &mut router,
        );
        if let AiAction::ShowSuggestion { text, .. } = &action {
            assert_eq!(text, "heuristic suggestion");
        } else {
            panic!("expected original suggestion");
        }
    }

    #[test]
    fn claude_skips_few_errors() {
        let ctx = test_context();
        let mut router = BrainRouter::new(RouterConfig::default());
        let backend = Some(ModelBackend::claude_default());
        let event = AiEvent::CommandComplete(parsed_few_errors()); // only 1 error

        let action = enhance_with_claude(
            suggestion_action(), &event, &ctx,
            TaskComplexity::Complex,
            &backend, &mut router,
        );
        // Should NOT escalate — fewer than 3 errors.
        if let AiAction::ShowSuggestion { text, .. } = &action {
            assert_eq!(text, "heuristic suggestion");
        } else {
            panic!("expected original suggestion");
        }
    }

    #[test]
    fn claude_skips_non_command_events() {
        let ctx = test_context();
        let mut router = BrainRouter::new(RouterConfig::default());
        let backend = Some(ModelBackend::claude_default());
        let event = AiEvent::UserIdle { seconds: 30.0 };

        let action = enhance_with_claude(
            suggestion_action(), &event, &ctx,
            TaskComplexity::Complex,
            &backend, &mut router,
        );
        if let AiAction::ShowSuggestion { text, .. } = &action {
            assert_eq!(text, "heuristic suggestion");
        } else {
            panic!("expected original suggestion");
        }
    }
}
