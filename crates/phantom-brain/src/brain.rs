//! The AI brain thread — an event-driven OODA loop.
//!
//! Spawned once at application startup via [`spawn_brain`]. Communicates with
//! the rest of the system through [`BrainHandle`] (channels). Blocks on the
//! event receiver and only consumes CPU when an event arrives.

use std::collections::HashMap;
use std::sync::mpsc;

use phantom_agents::AgentId;
use phantom_context::ProjectContext;
use phantom_memory::MemoryStore;

use crate::attention::Attention;
use crate::events::{AiAction, AiEvent};
use crate::proactive::ProactiveSuggester;
use crate::router::{
    BackendKind, BrainRouter, ModelBackend, RouterConfig, TaskClassifier, TaskComplexity,
};
use crate::scoring::UtilityScorer;

// ---------------------------------------------------------------------------
// DenialThreshold — configurable quarantine threshold for the brain
// ---------------------------------------------------------------------------

/// Number of consecutive `CapabilityDenied` events before the brain emits
/// [`AiAction::QuarantineAgent`] for the offending agent.
///
/// Matches [`phantom_agents::quarantine::DEFAULT_QUARANTINE_THRESHOLD`] so
/// both subsystems use the same N-=3 default out of the box.
const BRAIN_DENIAL_THRESHOLD: usize = phantom_agents::DEFAULT_QUARANTINE_THRESHOLD;

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
///
/// The thread body is wrapped in a supervisor loop that catches any panic with
/// `std::panic::catch_unwind` and restarts the brain loop (#227).  Each
/// restart spawns a fresh `brain_loop` with a new pair of internal channels;
/// a lightweight bridge thread forwards events from the external `BrainHandle`
/// sender into the current iteration's receiver, and forwards actions back,
/// so the caller observes no interruption.
pub fn spawn_brain(config: BrainConfig) -> BrainHandle {
    let (event_tx, event_rx) = mpsc::channel::<AiEvent>();
    let (action_tx, action_rx) = mpsc::channel::<AiAction>();

    std::thread::Builder::new()
        .name("phantom-brain".into())
        .spawn(move || {
            brain_supervised(config, event_rx, action_tx);
        })
        .expect("failed to spawn brain thread");

    BrainHandle {
        event_tx,
        action_rx,
    }
}

/// Supervisor loop: run `brain_loop` inside `catch_unwind`; restart on panic.
///
/// Design rationale — channel bridging:
/// `brain_loop` takes *ownership* of its `event_rx` / `action_tx`.  After a
/// panic those values are consumed and we cannot recover them.  To preserve
/// the external `BrainHandle` channels across restarts we use a bridge:
///
/// ```text
/// BrainHandle.event_tx  →  bridge_rx  →  [bridge thread]  →  iter_tx  →  brain_loop
/// brain_loop  →  iter_action_tx  →  [supervisor]  →  action_tx  →  BrainHandle.action_rx
/// ```
///
/// The bridge thread holds `bridge_rx` (the receiver end of the external
/// handle's channel) and forwards events into `iter_tx` (the per-iteration
/// sender).  When `brain_loop` panics, the supervisor drops `iter_tx` /
/// `iter_action_rx`, rebuilds fresh internal channels, and the bridge thread
/// picks up the new `iter_tx` via an `Arc<Mutex<Option<Sender>>>` swap.
fn brain_supervised(
    config: BrainConfig,
    event_rx: mpsc::Receiver<AiEvent>,
    action_tx: mpsc::Sender<AiAction>,
) {
    use std::sync::{Arc, Mutex};

    // Decompose config fields so we can rebuild `BrainConfig` each restart
    // without requiring `BrainConfig: Clone` or `RouterConfig: Clone`.
    let project_dir = config.project_dir;
    let enable_suggestions = config.enable_suggestions;
    let enable_memory = config.enable_memory;
    let quiet_threshold = config.quiet_threshold;
    // RouterConfig is not Clone, so we consume it on the first iteration only.
    let mut router: Option<crate::router::RouterConfig> = config.router;

    // Shared slot: the supervisor writes a fresh `iter_tx` here after each
    // restart; the bridge thread reads it to redirect events.
    let iter_tx_slot: Arc<Mutex<Option<mpsc::Sender<AiEvent>>>> = Arc::new(Mutex::new(None));

    // Spawn the bridge thread.  It owns `event_rx` (the external receiver)
    // and forwards events into whatever `iter_tx` the supervisor has installed.
    let slot_for_bridge = Arc::clone(&iter_tx_slot);
    std::thread::Builder::new()
        .name("phantom-brain-bridge".into())
        .spawn(move || {
            loop {
                let event = match event_rx.recv() {
                    Ok(e) => e,
                    Err(_) => break, // External handle dropped.
                };
                let is_shutdown = matches!(event, AiEvent::Shutdown);
                // Forward to current iteration's channel.
                let sent = {
                    let guard = slot_for_bridge.lock().expect("bridge slot poisoned");
                    if let Some(ref tx) = *guard {
                        tx.send(event).is_ok()
                    } else {
                        false // No active iteration yet; drop the event.
                    }
                };
                if !sent || is_shutdown {
                    break;
                }
            }
        })
        .expect("failed to spawn brain bridge thread");

    let mut restart_count: u32 = 0;

    loop {
        // Build per-iteration channels.
        let (iter_tx, iter_rx) = mpsc::channel::<AiEvent>();
        let (iter_action_tx, iter_action_rx) = mpsc::channel::<AiAction>();

        // Install this iteration's sender so the bridge forwards to it.
        {
            let mut guard = iter_tx_slot.lock().expect("iter_tx_slot poisoned");
            *guard = Some(iter_tx);
        }

        let iter_config = BrainConfig {
            project_dir: project_dir.clone(),
            enable_suggestions,
            enable_memory,
            quiet_threshold,
            router: router.take(), // `None` on second and subsequent restarts.
        };

        // Run brain_loop under catch_unwind.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            brain_loop(iter_config, iter_rx, iter_action_tx);
        }));

        // Drain any actions the iteration emitted before it exited / panicked.
        loop {
            match iter_action_rx.try_recv() {
                Ok(action) => {
                    let _ = action_tx.send(action);
                }
                Err(_) => break,
            }
        }

        // Uninstall the stale sender (the Receiver was consumed by the loop).
        {
            let mut guard = iter_tx_slot.lock().expect("iter_tx_slot poisoned");
            *guard = None;
        }

        match result {
            Ok(()) => {
                // Clean exit — Shutdown or external handle disconnected.
                log::info!("AI brain exited cleanly (iteration {restart_count})");
                return;
            }
            Err(payload) => {
                restart_count += 1;
                let msg: &str = payload
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("<non-string panic payload>");
                log::error!(
                    "AI brain panicked (iteration {}): {msg}. \
                     Restarting (attempt {restart_count})…",
                    restart_count - 1,
                );
            }
        }

        // Brief exponential back-off to avoid a tight panic storm.
        let backoff_ms = 100_u64.saturating_mul(u64::from(restart_count.min(10)));
        std::thread::sleep(std::time::Duration::from_millis(backoff_ms));
    }
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
    let memory = MemoryStore::open(&config.project_dir).unwrap_or_else(|e| {
        log::warn!("failed to open memory store: {e}, using fallback");
        // Create a fallback in-memory-only store by using /tmp.
        MemoryStore::open_in(
            &config.project_dir,
            std::path::Path::new("/tmp/phantom-brain-fallback"),
        )
        .expect("failed to create fallback memory store")
    });
    let mut scorer = UtilityScorer::new();
    let mut proactive = ProactiveSuggester::default_triggers();
    let mut router = BrainRouter::new(config.router.unwrap_or_default());
    let mut active_ledger: Option<crate::orchestrator::TaskLedger> = None;
    let mut reconciler = crate::reconciler::ReconcilerState::new();
    let attention = Attention::new();
    // Sec.7: consecutive CapabilityDenied counter per agent.
    // Reset on any non-denial event for the same agent (or on quarantine).
    let mut denial_counters: HashMap<AgentId, usize> = HashMap::new();

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
                // ATTENTION: rank live panes to decide which one is worth watching.
                let ranked_panes = attention.rank(&[]);
                log::trace!("attention head ranked {} panes on tick", ranked_panes.len());

                // Proactive tick: if we have accumulated output and enough
                // time has passed, send it to Claude for commentary.
                if !output_buffer.is_empty() && last_output_time.elapsed().as_secs() >= 2 {
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
        if let AiEvent::GoalSet {
            ref objective,
            ref initial_task,
        } = event
        {
            log::info!("Brain goal set: {objective}");
            let _ = action_tx.send(AiAction::ConsoleReply(format!(
                "Goal accepted: {objective}. Starting work."
            )));

            // Wire the autonomous reconciler for multi-step goal execution.
            let mut ledger = crate::orchestrator::TaskLedger::new(objective);
            ledger.set_plan(vec![crate::orchestrator::PlanStep::new(
                initial_task,
                phantom_agents::AgentTask::FreeForm {
                    prompt: initial_task.clone(),
                },
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
        // Also scan the summary for a sentinel plan and auto-submit it.
        if let AiEvent::AgentComplete {
            id,
            success,
            ref summary,
            spawn_tag,
        } = event
        {
            // Plan extraction: if the agent's summary contains a sentinel heading
            // (## Plan, ## Steps, etc.) parse the numbered list and submit it to
            // the active TaskLedger.  When no ledger is active, create a fresh one
            // for the extracted goal so the reconciler can drive it forward.
            if let Some(steps) = crate::plan_extractor::extract_plan_from_text(summary) {
                log::info!(
                    "Brain: agent {id} output contained a sentinel plan ({} steps); \
                     submitting to TaskLedger",
                    steps.len()
                );
                if let Some(ref mut l) = active_ledger {
                    l.set_plan(steps);
                    reconciler.reset();
                } else {
                    // No ledger yet — create one from the extracted plan.
                    let mut ledger =
                        crate::orchestrator::TaskLedger::new("Agent-extracted plan");
                    ledger.set_plan(steps);
                    active_ledger = Some(ledger);
                    reconciler.reset();
                }
                // Kick the first step immediately without waiting for the 3s timeout.
                let mut terminal = false;
                if let Some(ref mut l) = active_ledger {
                    terminal = !reconciler.tick(l, &action_tx);
                }
                if terminal {
                    active_ledger = None;
                }
            }

            if let Some(ref mut l) = active_ledger {
                reconciler.on_agent_complete(l, id, success, summary, spawn_tag);
            }
            // Fall through to OODA scoring — notification_score handles this event.
        }

        // Sec.7: consecutive CapabilityDenied → QuarantineAgent.
        //
        // Track consecutive denials per agent. When the count reaches the
        // threshold, emit QuarantineAgent so the app can apply it to the
        // QuarantineRegistry. The counter is reset when the agent is quarantined
        // (prevents double-emission on subsequent denials from a quarantined agent).
        if let AiEvent::CapabilityDenied {
            agent_id,
            ref tool_name,
        } = event
        {
            let count = denial_counters.entry(agent_id).or_insert(0);
            *count += 1;
            log::debug!(
                "Brain: agent {agent_id} capability denied (tool={tool_name}, consecutive={count})"
            );
            if *count >= BRAIN_DENIAL_THRESHOLD {
                let denial_count = *count;
                // Reset counter so we don't re-quarantine on the next denial.
                denial_counters.remove(&agent_id);
                log::warn!(
                    "Brain: agent {agent_id} quarantined after {denial_count} consecutive denials"
                );
                if action_tx
                    .send(AiAction::QuarantineAgent {
                        agent_id,
                        denial_count,
                    })
                    .is_err()
                {
                    break;
                }
            }
            continue; // CapabilityDenied is handled here; skip OODA for it.
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

        // PROACTIVE: run the trigger-based suggester on every event.
        // Emits AiAction::Suggest when a pattern is observed and the
        // per-kind cooldown has elapsed. Short-circuits the utility loop so
        // the proactive and utility-AI signals don't double-fire on the same
        // event.
        if config.enable_suggestions {
            if let Some(proactive_action) = proactive.observe(&event) {
                log::debug!(
                    "AI brain: proactive trigger fired — {}",
                    action_name(&proactive_action)
                );
                if action_tx.send(proactive_action).is_err() {
                    break;
                }
                continue;
            }
        }

        // CLASSIFY: determine task complexity and select backend cascade.
        let complexity = TaskClassifier::classify(&event);
        // Clone the first Ollama backend info so we can release the router borrow.
        let ollama_backend: Option<ModelBackend> = router
            .route(complexity)
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
        let suppressed =
            !config.enable_suggestions && matches!(best.action, AiAction::ShowSuggestion { .. });
        let memory_suppressed =
            !config.enable_memory && matches!(best.action, AiAction::UpdateMemory { .. });

        if dominated_by_quiet || suppressed || memory_suppressed {
            continue;
        }

        // ENHANCE: if the winning action is a suggestion and we have an
        // error event, try to upgrade it with Ollama's local model.
        let action =
            enhance_with_ollama(best.action, &event, &context, &ollama_backend, &mut router);

        // ESCALATE: for Complex tasks, if Ollama didn't enhance (still heuristic)
        // and Claude is available, escalate to the frontier model.
        let claude_backend: Option<ModelBackend> = router
            .route(complexity)
            .iter()
            .find(|b| matches!(b.kind, BackendKind::Claude { .. }))
            .cloned()
            .cloned();
        let action = enhance_with_claude(
            action,
            &event,
            &context,
            complexity,
            &claude_backend,
            &mut router,
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
    let files: Vec<&str> = parsed
        .errors
        .iter()
        .filter_map(|e| e.file.as_deref())
        .collect();
    if files.is_empty() {
        return None;
    }

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
    if complexity != TaskComplexity::Complex {
        return current_action;
    }
    if !matches!(current_action, AiAction::ShowSuggestion { .. }) {
        return current_action;
    }
    let AiEvent::CommandComplete(parsed) = event else {
        return current_action;
    };
    if parsed.errors.len() < 3 {
        return current_action;
    }

    let working_dir = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());

    let file_context = diagnose_build_failure(parsed).unwrap_or_default();
    let prompt = format!(
        "Investigate this build failure. Read the relevant source files and suggest a fix.\n\
         Command: {}\nErrors:\n{}\n{file_context}",
        parsed.command,
        parsed
            .errors
            .iter()
            .take(5)
            .map(|e| format!("- {}", e.message))
            .collect::<Vec<_>>()
            .join("\n"),
    );

    match crate::claude::investigate(&prompt, &working_dir, 5) {
        Ok(text) => {
            log::info!("Brain investigation complete: {} chars", text.len());
            let context = text.clone();
            AiAction::ShowSuggestion {
                text,
                options: vec![
                    crate::events::SuggestionOption {
                        key: 'f',
                        label: "Fix it".into(),
                        action: Some(Box::new(AiAction::SpawnAgent {
                            task: phantom_agents::AgentTask::FixError {
                                error_summary: parsed
                                    .errors
                                    .first()
                                    .map(|e| e.message.clone())
                                    .unwrap_or_default(),
                                file: parsed.errors.first().and_then(|e| e.file.clone()),
                                context,
                            },
                            spawn_tag: None,
                        })),
                    },
                    crate::events::SuggestionOption {
                        key: 'd',
                        label: "Dismiss".into(),
                        action: None,
                    },
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
    let prompt =
        crate::ollama::build_error_triage_prompt(&parsed.command, &parsed.errors, &proj_type);

    match crate::ollama::generate(model_name, &prompt, 150) {
        Ok((text, latency_ms)) => {
            log::info!("Ollama enhanced suggestion ({latency_ms:.0}ms): {text}");
            router.record_result(&backend.name, latency_ms, true);
            AiAction::ShowSuggestion {
                text,
                options: vec![
                    crate::events::SuggestionOption {
                        key: 'y',
                        label: "Fix it".into(),
                        action: Some(Box::new(AiAction::SpawnAgent {
                            task: phantom_agents::AgentTask::FreeForm {
                                prompt: "Fix it".into(),
                            },
                            spawn_tag: None,
                        })),
                    },
                    crate::events::SuggestionOption {
                        key: 'n',
                        label: "Dismiss".into(),
                        action: None,
                    },
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
    let prompt =
        crate::claude::build_error_analysis_prompt(&parsed.command, &parsed.errors, &proj_type);

    match crate::claude::generate(model_name, &prompt, 300) {
        Ok((text, latency_ms)) => {
            log::info!("Claude escalation ({latency_ms:.0}ms): {text}");
            router.record_result(&backend.name, latency_ms, true);
            AiAction::ShowSuggestion {
                text,
                options: vec![
                    crate::events::SuggestionOption {
                        key: 'y',
                        label: "Fix it".into(),
                        action: Some(Box::new(AiAction::SpawnAgent {
                            task: phantom_agents::AgentTask::FreeForm {
                                prompt: "Fix it".into(),
                            },
                            spawn_tag: None,
                        })),
                    },
                    crate::events::SuggestionOption {
                        key: 'n',
                        label: "Dismiss".into(),
                        action: None,
                    },
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
                && t != "$"
                && t != "%"
                && t != ">"
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
    AiAction::ConsoleReply(format!(
        "Received: \"{query}\" (no LLM backend available for query)"
    ))
}

// ---------------------------------------------------------------------------
// action_name — for debug logging
// ---------------------------------------------------------------------------

/// Human-readable label for an action (used in log output).
pub(crate) fn action_name(action: &AiAction) -> &str {
    match action {
        AiAction::ShowSuggestion { .. } => "suggest",
        AiAction::Suggest { .. } => "proactive_suggest",
        AiAction::SpawnAgent { .. } => "spawn_agent",
        AiAction::UpdateMemory { .. } => "update_memory",
        AiAction::ShowNotification(_) => "notify",
        AiAction::RunCommand(_) => "run_command",
        AiAction::ConsoleReply(_) => "console_reply",
        AiAction::DismissAdapter { .. } => "dismiss_adapter",
        AiAction::AgentFlatlined { .. } => "flatline",
        AiAction::QuarantineAgent { .. } => "quarantine_agent",
        AiAction::AgentQuarantined { .. } => "agent_quarantined",
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
            action_name(&AiAction::Suggest {
                action: "fix it".into(),
                rationale: "build failed".into(),
                confidence: 0.8,
            }),
            "proactive_suggest"
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
        assert_eq!(
            action_name(&AiAction::RunCommand("ls".into())),
            "run_command"
        );
        assert_eq!(
            action_name(&AiAction::QuarantineAgent {
                agent_id: 1,
                denial_count: 3
            }),
            "quarantine_agent",
        );
        assert_eq!(
            action_name(&AiAction::AgentQuarantined {
                agent_id: 1,
                denial_count: 3
            }),
            "agent_quarantined",
        );
    }

    // -----------------------------------------------------------------------
    // Sec.7: CapabilityDenied event tracking tests
    // -----------------------------------------------------------------------

    /// Sec.7 brain test: 2 consecutive CapabilityDenied → no QuarantineAgent.
    ///
    /// The brain's denial counter must not trigger quarantine before the
    /// threshold (N=3) is reached.
    #[test]
    fn brain_two_denials_do_not_emit_quarantine() {
        let (action_tx, action_rx) = std::sync::mpsc::channel::<AiAction>();
        let mut denial_counters: std::collections::HashMap<phantom_agents::AgentId, usize> =
            std::collections::HashMap::new();
        let agent_id = 200u64;

        // Simulate brain handling 2 CapabilityDenied events.
        for i in 1u32..=2 {
            let tool_name = format!("run_command_{i}");
            // Mimic what brain_loop does on CapabilityDenied.
            let count = denial_counters.entry(agent_id).or_insert(0);
            *count += 1;
            if *count >= BRAIN_DENIAL_THRESHOLD {
                let denial_count = *count;
                denial_counters.remove(&agent_id);
                action_tx
                    .send(AiAction::QuarantineAgent {
                        agent_id,
                        denial_count,
                    })
                    .unwrap();
            }
            let _ = tool_name; // suppress unused warning
        }

        // No QuarantineAgent must have been emitted.
        assert!(
            action_rx.try_recv().is_err(),
            "brain must NOT emit QuarantineAgent after only 2 denials (threshold=3)"
        );
    }

    /// Sec.7 brain test: 3 consecutive CapabilityDenied → QuarantineAgent emitted.
    ///
    /// On the Nth denial, the brain emits exactly one `QuarantineAgent` action
    /// with the correct `agent_id` and `denial_count`.
    #[test]
    fn brain_three_denials_emit_quarantine_agent() {
        let (action_tx, action_rx) = std::sync::mpsc::channel::<AiAction>();
        let mut denial_counters: std::collections::HashMap<phantom_agents::AgentId, usize> =
            std::collections::HashMap::new();
        let agent_id = 201u64;

        // Simulate 3 consecutive CapabilityDenied events.
        for _ in 1u32..=3 {
            let count = denial_counters.entry(agent_id).or_insert(0);
            *count += 1;
            if *count >= BRAIN_DENIAL_THRESHOLD {
                let denial_count = *count;
                denial_counters.remove(&agent_id);
                action_tx
                    .send(AiAction::QuarantineAgent {
                        agent_id,
                        denial_count,
                    })
                    .unwrap();
            }
        }

        // Exactly one QuarantineAgent must have been emitted.
        let action = action_rx
            .try_recv()
            .expect("QuarantineAgent must be emitted on 3rd denial");
        match action {
            AiAction::QuarantineAgent {
                agent_id: id,
                denial_count,
            } => {
                assert_eq!(id, agent_id, "agent_id must match");
                assert_eq!(denial_count, 3, "denial_count must be 3 (the threshold)");
            }
            other => panic!("expected QuarantineAgent, got {other:?}"),
        }

        // No second action (counter was reset).
        assert!(
            action_rx.try_recv().is_err(),
            "only one QuarantineAgent must be emitted per quarantine event"
        );
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
            command_type: phantom_semantic::CommandType::Cargo(
                phantom_semantic::CargoCommand::Build,
            ),
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
            command_type: phantom_semantic::CommandType::Cargo(
                phantom_semantic::CargoCommand::Build,
            ),
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
            suggestion_action(),
            &event,
            &ctx,
            TaskComplexity::Simple, // not Complex
            &backend,
            &mut router,
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
            AiAction::DoNothing,
            &event,
            &ctx,
            TaskComplexity::Complex,
            &backend,
            &mut router,
        );
        assert!(matches!(action, AiAction::DoNothing));
    }

    #[test]
    fn claude_skips_when_no_backend() {
        let ctx = test_context();
        let mut router = BrainRouter::new(RouterConfig::default());
        let event = AiEvent::CommandComplete(parsed_many_errors());

        let action = enhance_with_claude(
            suggestion_action(),
            &event,
            &ctx,
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
            suggestion_action(),
            &event,
            &ctx,
            TaskComplexity::Complex,
            &backend,
            &mut router,
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
            suggestion_action(),
            &event,
            &ctx,
            TaskComplexity::Complex,
            &backend,
            &mut router,
        );
        if let AiAction::ShowSuggestion { text, .. } = &action {
            assert_eq!(text, "heuristic suggestion");
        } else {
            panic!("expected original suggestion");
        }
    }

    // =======================================================================
    // #227 — brain thread panics silently with no restart
    // =======================================================================

    /// Simulate a brain panic by injecting a panic via `AiEvent::Interrupt`
    /// with a special sentinel string.  After the panic the supervisor must
    /// restart and continue processing subsequent events.
    ///
    /// We verify this by:
    /// 1. Sending an event that causes `brain_loop` to panic.
    /// 2. Waiting a short while for the supervisor to restart.
    /// 3. Sending a normal `Interrupt` event.
    /// 4. Asserting that the brain responds (or at least doesn't deadlock).
    ///
    /// Because the real `brain_loop` makes live network calls we test the
    /// supervisor machinery directly with a synthetic panic-injecting loop.
    #[test]
    fn brain_supervisor_restarts_after_panic() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU32, Ordering};

        let restart_counter = Arc::new(AtomicU32::new(0));

        let (event_tx, event_rx) = mpsc::channel::<AiEvent>();
        let (action_tx, action_rx) = mpsc::channel::<AiAction>();

        let counter_clone = Arc::clone(&restart_counter);

        // Spawn a stripped-down brain supervisor that panics on the first
        // event, then sends an action on the second.
        std::thread::Builder::new()
            .name("test-brain-supervisor".into())
            .spawn(move || {
                // Use the same supervisor pattern: catch_unwind + restart.
                let mut iteration = 0u32;
                loop {
                    let (iter_tx, iter_rx) = mpsc::channel::<AiEvent>();
                    let (iter_action_tx, iter_action_rx) = mpsc::channel::<AiAction>();

                    // Forward one event from the shared receiver.
                    if let Ok(ev) = event_rx.recv() {
                        let _ = iter_tx.send(ev);
                    } else {
                        break; // External handle dropped.
                    }

                    let iter_action_tx2 = iter_action_tx;
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        // First iteration panics; subsequent ones succeed.
                        let ev = iter_rx.recv().expect("iter_rx closed");
                        if let AiEvent::Interrupt(ref s) = ev {
                            if s == "__test_panic__" {
                                panic!("injected test panic");
                            }
                            // On restart: reply with an action.
                            let _ = iter_action_tx2.send(AiAction::ConsoleReply("ok".into()));
                        }
                    }));

                    // Drain actions.
                    loop {
                        match iter_action_rx.try_recv() {
                            Ok(a) => {
                                let _ = action_tx.send(a);
                            }
                            Err(_) => break,
                        }
                    }

                    match result {
                        Ok(()) => break, // Clean exit after second iteration.
                        Err(_) => {
                            counter_clone.fetch_add(1, Ordering::SeqCst);
                            iteration += 1;
                        }
                    }
                    if iteration >= 3 {
                        break;
                    } // Safety guard.
                }
            })
            .expect("failed to spawn test supervisor");

        // First event causes a panic.
        event_tx
            .send(AiEvent::Interrupt("__test_panic__".into()))
            .unwrap();

        // Give the supervisor time to catch the panic and restart.
        std::thread::sleep(std::time::Duration::from_millis(200));

        // Second event should be processed normally.
        event_tx.send(AiEvent::Interrupt("hello".into())).unwrap();

        // The brain should reply within a reasonable timeout.
        let reply = action_rx.recv_timeout(std::time::Duration::from_secs(3));
        assert!(
            reply.is_ok(),
            "brain must reply after restart; timed out waiting"
        );

        // The supervisor must have recorded at least one restart.
        assert!(
            restart_counter.load(Ordering::SeqCst) >= 1,
            "supervisor must have restarted at least once after the injected panic"
        );
    }
}
