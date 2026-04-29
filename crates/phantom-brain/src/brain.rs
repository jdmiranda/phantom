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
    let mut active_ledger: Option<crate::orchestrator::TaskLedger> = None;
    let mut reconciler = crate::reconciler::ReconcilerState::new();

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

    // Accumulated terminal output for batched observation.
    let mut output_buffer = String::new();
    let mut last_output_time = std::time::Instant::now();

    loop {
        // OBSERVE: wait for an event with timeout for proactive ticks.
        // The timeout lets the brain act even without incoming events
        // (e.g., process accumulated output after a quiet period).
        let event = match event_rx.recv_timeout(std::time::Duration::from_secs(3)) {
            Ok(e) => e,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Proactive tick: if we have accumulated output and enough
                // time has passed, send it to Claude for commentary.
                if !output_buffer.is_empty()
                    && last_output_time.elapsed().as_secs() >= 2
                {
                    let batch = std::mem::take(&mut output_buffer);
                    let reply = observe_terminal_output(&batch, &context, &mut router);
                    if let Some(action) = reply {
                        if action_tx.send(action).is_err() {
                            break;
                        }
                    }
                }
                // Drive the autonomous task ledger forward on every tick.
                let mut terminal = false;
                if let Some(ref mut l) = active_ledger {
                    terminal = !reconciler.tick(l, &action_tx);
                }
                if terminal {
                    active_ledger = None;
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };

        // Handle shutdown.
        if matches!(event, AiEvent::Shutdown) {
            log::info!("AI brain shutting down");
            break;
        }

        // Handle goal-setting: activate goal pursuit mode.
        if let AiEvent::GoalSet { ref objective, ref initial_task } = event {
            log::info!("Brain goal set: {objective}");
            let _ = action_tx.send(AiAction::ConsoleReply(format!(
                "Goal accepted: {objective}. Starting work."
            )));

            // Wire the autonomous reconciler for multi-step goal execution.
            let mut ledger = crate::orchestrator::TaskLedger::new(objective);
            ledger.set_plan(vec![crate::orchestrator::PlanStep::new(
                initial_task,
                phantom_agents::AgentTask::FreeForm { prompt: initial_task.clone() },
            )]);
            active_ledger = Some(ledger);
            reconciler.reset();
            // Kick the first dispatch immediately — don't wait for the 3s timeout.
            let mut terminal = false;
            if let Some(ref mut l) = active_ledger {
                terminal = !reconciler.tick(l, &action_tx);
            }
            if terminal {
                active_ledger = None;
            }
            continue;
        }

        // Forward AgentComplete to the reconciler to advance the task ledger.
        if let AiEvent::AgentComplete { id, success, ref summary, spawn_tag } = event {
            if let Some(ref mut l) = active_ledger {
                reconciler.on_agent_complete(l, id, success, summary, spawn_tag);
            }
            // Fall through to OODA scoring — notification_score handles this event.
        }

        // ACCUMULATE: OutputChunk events are batched — don't process each one
        // individually. Accumulate and let the timeout tick flush them.
        if let AiEvent::OutputChunk(ref text) = event {
            output_buffer.push_str(text);
            // Cap buffer to prevent unbounded growth.
            if output_buffer.len() > 4096 {
                let drain = output_buffer.len() - 4096;
                output_buffer.drain(..drain);
            }
            last_output_time = std::time::Instant::now();
            continue; // Don't run OODA for raw output — wait for batch.
        }

        // DIRECT RESPONSE: Interrupt events are explicit user queries from the
        // console. Bypass the utility scorer — the user asked directly, so we
        // always respond. Try Claude first, fall back to heuristic acknowledgement.
        if let AiEvent::Interrupt(ref query) = event {
            if !query.is_empty() {
                let reply = handle_console_query(query, &context, &mut router);
                if action_tx.send(reply).is_err() {
                    break;
                }
                scorer.user_acted();
                continue;
            }
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

        // INVESTIGATE: for Complex tasks, run tool-augmented investigation.
        let action = enhance_with_investigation(action, &event, complexity);

        if action_tx.send(action).is_err() {
            break; // render thread dropped its receiver
        }
    }

    log::info!("AI brain thread exiting");
}

/// Read source files referenced by errors for richer diagnosis.
fn diagnose_build_failure(parsed: &phantom_semantic::ParsedOutput) -> Option<String> {
    let files: Vec<&str> = parsed.errors.iter()
        .filter_map(|e| e.file.as_deref())
        .collect();
    if files.is_empty() { return None; }

    let working_dir = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());

    let mut out = String::new();
    for path in files.iter().take(3) {
        let full = std::path::Path::new(&working_dir).join(path);
        if let Ok(content) = std::fs::read_to_string(&full) {
            let truncated: String = content.lines().take(200).collect::<Vec<_>>().join("\n");
            out.push_str(&format!("\n--- {path} ---\n{truncated}\n"));
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// For Complex tasks, run a tool-augmented investigation instead of text-only.
fn enhance_with_investigation(
    current_action: AiAction,
    event: &AiEvent,
    complexity: TaskComplexity,
) -> AiAction {
    if complexity != TaskComplexity::Complex { return current_action; }
    if !matches!(current_action, AiAction::ShowSuggestion { .. }) { return current_action; }
    let AiEvent::CommandComplete(parsed) = event else { return current_action; };
    if parsed.errors.len() < 3 { return current_action; }

    let working_dir = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());

    let file_context = diagnose_build_failure(parsed).unwrap_or_default();
    let prompt = format!(
        "Investigate this build failure. Read the relevant source files and suggest a fix.\n\
         Command: {}\nErrors:\n{}\n{file_context}",
        parsed.command,
        parsed.errors.iter().take(5).map(|e| format!("- {}", e.message)).collect::<Vec<_>>().join("\n"),
    );

    match crate::claude::investigate(&prompt, &working_dir, 5) {
        Ok(text) => {
            log::info!("Brain investigation complete: {} chars", text.len());
            let context = text.clone();
            AiAction::ShowSuggestion {
                text,
                options: vec![
                    crate::events::SuggestionOption { key: 'f', label: "Fix it".into(), action: Some(Box::new(AiAction::SpawnAgent { task: phantom_agents::AgentTask::FixError {
                        error_summary: parsed.errors.first().map(|e| e.message.clone()).unwrap_or_default(),
                        file: parsed.errors.first().and_then(|e| e.file.clone()),
                        context,
                    }, spawn_tag: None })) },
                    crate::events::SuggestionOption { key: 'd', label: "Dismiss".into(), action: None },
                ],
            }
        }
        Err(e) => {
            log::warn!("Brain investigation failed: {e}");
            current_action
        }
    }
}

// ---------------------------------------------------------------------------
// orient — update world model
// ---------------------------------------------------------------------------

/// Update the scorer's state based on the incoming event.
fn orient(event: &AiEvent, scorer: &mut UtilityScorer) {
    match event {
        AiEvent::CommandComplete(parsed) => {
            scorer.last_had_errors = !parsed.errors.is_empty();
            scorer.last_command = Some(parsed.command.clone());
            // A command completed — no longer an active process.
            scorer.has_active_process = false;
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
        AiEvent::WatcherTick { .. } => {
            // A watcher tick means something is actively running.
            scorer.has_active_process = true;
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
                    crate::events::SuggestionOption { key: 'y', label: "Fix it".into(), action: Some(Box::new(AiAction::SpawnAgent { task: phantom_agents::AgentTask::FreeForm { prompt: "Fix it".into() }, spawn_tag: None })) },
                    crate::events::SuggestionOption { key: 'n', label: "Dismiss".into(), action: None },
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
                    crate::events::SuggestionOption { key: 'y', label: "Fix it".into(), action: Some(Box::new(AiAction::SpawnAgent { task: phantom_agents::AgentTask::FreeForm { prompt: "Fix it".into() }, spawn_tag: None })) },
                    crate::events::SuggestionOption { key: 'n', label: "Dismiss".into(), action: None },
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
// observe_terminal_output — proactive commentary on what the brain sees
// ---------------------------------------------------------------------------

/// Process a batch of accumulated terminal output. Called from the proactive
/// timeout tick when output has been quiet for 2+ seconds (e.g., a command
/// finished and the shell prompt returned).
///
/// Returns `Some(AiAction::ConsoleReply)` if Claude has something useful to
/// say, `None` if the output is unremarkable (empty, just a prompt, etc.).
fn observe_terminal_output(
    output: &str,
    context: &ProjectContext,
    router: &mut BrainRouter,
) -> Option<AiAction> {
    let trimmed = output.trim();

    // Skip trivial output (empty, just prompt characters, very short).
    if trimmed.is_empty() || trimmed.len() < 10 {
        return None;
    }

    // Skip if it's just prompt lines ($ % > #).
    let non_prompt_lines: Vec<&str> = trimmed
        .lines()
        .filter(|l| {
            let t = l.trim();
            !t.is_empty()
                && !t.ends_with("$ ")
                && !t.ends_with("% ")
                && !t.ends_with("> ")
                && !t.ends_with("# ")
                && t != "$" && t != "%" && t != ">"
        })
        .collect();

    if non_prompt_lines.is_empty() {
        return None;
    }

    // Try Claude.
    let claude_backend: Option<ModelBackend> = router
        .route(TaskComplexity::Simple)
        .iter()
        .find(|b| matches!(b.kind, BackendKind::Claude { .. }))
        .cloned()
        .cloned();

    let Some(backend) = claude_backend else {
        return None; // No Claude available — stay quiet.
    };

    let model_name = match &backend.kind {
        BackendKind::Claude { model } => model.as_str(),
        _ => return None,
    };

    // Only send the last ~2K chars to keep API costs reasonable.
    let obs = if trimmed.len() > 2000 {
        &trimmed[trimmed.len() - 2000..]
    } else {
        trimmed
    };

    let prompt = format!(
        "You are Phantom, an AI brain embedded in a terminal emulator. \
         You are observing a developer working in a {} project ({}).\n\n\
         Here is the latest terminal output:\n```\n{}\n```\n\n\
         If there's something noteworthy (an error, a warning, an interesting result, \
         a potential issue), comment briefly (1 sentence). \
         If the output is routine (successful build, normal ls, etc.), respond with \
         exactly the word QUIET and nothing else.",
        format!("{:?}", context.project_type),
        context.name,
        obs,
    );

    match crate::claude::generate(model_name, &prompt, 100) {
        Ok((text, latency_ms)) => {
            let reply = text.trim().to_string();
            router.record_result(&backend.name, latency_ms, true);

            // If Claude said QUIET, the output was unremarkable.
            if reply == "QUIET" || reply.to_uppercase().starts_with("QUIET") {
                log::trace!("Brain observed output, nothing noteworthy ({latency_ms:.0}ms)");
                return None;
            }

            log::info!("Brain observation ({latency_ms:.0}ms): {reply}");
            Some(AiAction::ConsoleReply(reply))
        }
        Err(e) => {
            log::debug!("Brain observation failed: {e}");
            router.record_result(&backend.name, 0.0, false);
            None
        }
    }
}

// ---------------------------------------------------------------------------
// handle_console_query — direct response to user Interrupt events
// ---------------------------------------------------------------------------

/// Handle a user's console query. Tries Claude first (if available),
/// falls back to a heuristic acknowledgement.
fn handle_console_query(
    query: &str,
    context: &ProjectContext,
    router: &mut BrainRouter,
) -> AiAction {
    // Try to find a Claude backend.
    let claude_backend: Option<ModelBackend> = router
        .route(TaskComplexity::Simple)
        .iter()
        .find(|b| matches!(b.kind, BackendKind::Claude { .. }))
        .cloned()
        .cloned();

    if let Some(backend) = claude_backend {
        let model_name = match &backend.kind {
            BackendKind::Claude { model } => model.as_str(),
            _ => unreachable!(),
        };

        let prompt = format!(
            "You are Phantom, an AI-native terminal emulator's brain. \
             The user is working in a {} project ({}) and typed this in the console:\n\n\
             \"{}\"\n\n\
             Respond concisely (1-3 sentences). Be helpful and direct.",
            format!("{:?}", context.project_type),
            context.name,
            query,
        );

        match crate::claude::generate(model_name, &prompt, 200) {
            Ok((text, latency_ms)) => {
                log::info!("Console query answered via Claude ({latency_ms:.0}ms)");
                router.record_result(&backend.name, latency_ms, true);
                return AiAction::ConsoleReply(text);
            }
            Err(e) => {
                log::warn!("Claude console query failed: {e}");
                router.record_result(&backend.name, 0.0, false);
            }
        }
    }

    // Fallback: acknowledge the query without LLM.
    AiAction::ConsoleReply(format!("Received: \"{query}\" (no LLM backend available for query)"))
}

// ---------------------------------------------------------------------------
// action_name — for debug logging
// ---------------------------------------------------------------------------

/// Human-readable label for an action (used in log output).
pub(crate) fn action_name(action: &AiAction) -> &str {
    match action {
        AiAction::ShowSuggestion { .. } => "suggest",
        AiAction::SpawnAgent { .. } => "spawn_agent",
        AiAction::UpdateMemory { .. } => "update_memory",
        AiAction::ShowNotification(_) => "notify",
        AiAction::RunCommand(_) => "run_command",
        AiAction::ConsoleReply(_) => "console_reply",
        AiAction::DismissAdapter { .. } => "dismiss_adapter",
        AiAction::AgentFlatlined { .. } => "flatline",
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
            action_name(&AiAction::SpawnAgent {
                task: phantom_agents::AgentTask::FreeForm { prompt: "x".into() },
                spawn_tag: None,
            }),
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
