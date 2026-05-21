//! Multi-model routing layer for the AI brain.
//!
//! Classifies incoming [`AiEvent`]s by [`TaskComplexity`] and routes them to
//! the cheapest capable [`ModelBackend`], cascading to more powerful (and
//! expensive) backends when confidence is low or a backend is unavailable.
//!
//! Inspired by Arch-Router and the cascade pattern from RouteLLM research.

use crate::events::AiEvent;

// ---------------------------------------------------------------------------
// TaskComplexity
// ---------------------------------------------------------------------------

/// What kind of intelligence does this task need?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskComplexity {
    /// Fast classification: yes/no, error detection, intent routing.
    /// Can be handled by heuristics or a tiny model (Gemma 2B).
    Trivial,
    /// Short generation: summaries, suggestions, reformatting.
    /// Good for a small local model (Phi-3.5, Llama 3B).
    Simple,
    /// Multi-step reasoning, code generation, tool use.
    /// Needs a frontier model (Claude).
    Complex,
}

// ---------------------------------------------------------------------------
// BackendKind
// ---------------------------------------------------------------------------

/// The type of inference backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendKind {
    /// Rule-based scorer (current brain, no LLM).
    Heuristic,
    /// Local model via Ollama API (localhost:11434).
    Ollama { model: String },
    /// Anthropic Claude API.
    Claude { model: String },
    /// Generic OpenAI-compatible endpoint.
    OpenAICompat { base_url: String, model: String },
}

impl BackendKind {
    /// Whether this backend sends requests to a remote cloud service.
    ///
    /// Returns `true` for [`BackendKind::Claude`] and
    /// [`BackendKind::OpenAICompat`]; `false` for [`BackendKind::Heuristic`]
    /// and [`BackendKind::Ollama`] (which run locally).
    #[must_use] 
    pub fn is_cloud_provider(&self) -> bool {
        matches!(self, Self::Claude { .. } | Self::OpenAICompat { .. })
    }
}

// ---------------------------------------------------------------------------
// ModelBackend
// ---------------------------------------------------------------------------

/// A backend that can handle brain tasks.
#[derive(Debug, Clone)]
pub struct ModelBackend {
    /// Human-readable name for logging and identification.
    pub name: String,
    /// What kind of backend this is.
    pub kind: BackendKind,
    /// Which complexity tiers this backend can handle.
    pub capabilities: Vec<TaskComplexity>,
    /// Whether this backend is currently reachable.
    pub available: bool,
    /// Rolling average latency in ms (0 = not yet measured).
    pub avg_latency_ms: f32,
    /// Max context window in tokens.
    pub max_context: usize,
    /// Cost per 1K tokens (0.0 for local).
    pub cost_per_1k: f32,
}

impl ModelBackend {
    /// Built-in heuristic backend (always available, zero cost).
    #[must_use] 
    pub fn heuristic() -> Self {
        Self {
            name: "heuristic".into(),
            kind: BackendKind::Heuristic,
            capabilities: vec![TaskComplexity::Trivial],
            available: true,
            avg_latency_ms: 0.0,
            max_context: 0,
            cost_per_1k: 0.0,
        }
    }

    /// Default Ollama backend (phi3.5, local, handles Trivial + Simple).
    #[must_use] 
    pub fn ollama_default() -> Self {
        Self {
            name: "ollama-phi3.5".into(),
            kind: BackendKind::Ollama {
                model: "phi3.5:latest".into(),
            },
            capabilities: vec![TaskComplexity::Trivial, TaskComplexity::Simple],
            available: false, // must be health-checked first
            avg_latency_ms: 0.0,
            max_context: 8192,
            cost_per_1k: 0.0,
        }
    }

    /// Default Claude backend (sonnet, handles all tiers).
    #[must_use] 
    pub fn claude_default() -> Self {
        Self {
            name: "claude-sonnet".into(),
            kind: BackendKind::Claude {
                model: "claude-sonnet-4-20250514".into(),
            },
            capabilities: vec![
                TaskComplexity::Trivial,
                TaskComplexity::Simple,
                TaskComplexity::Complex,
            ],
            available: std::env::var("ANTHROPIC_API_KEY").is_ok(),
            avg_latency_ms: 0.0,
            max_context: 200_000,
            cost_per_1k: 0.003,
        }
    }

    /// Returns `true` if this backend makes outbound cloud API calls.
    ///
    /// Local backends (heuristic, Ollama) return `false`.
    /// Cloud backends (Claude, OpenAI-compat) return `true`.
    #[must_use] 
    pub fn is_cloud_provider(&self) -> bool {
        matches!(
            self.kind,
            BackendKind::Claude { .. } | BackendKind::OpenAICompat { .. }
        )
    }

    /// Returns a human-readable provider name for logging and error messages.
    #[must_use] 
    pub fn provider_name(&self) -> &'static str {
        match &self.kind {
            BackendKind::Heuristic => "heuristic",
            BackendKind::Ollama { .. } => "ollama",
            BackendKind::Claude { .. } => "claude",
            BackendKind::OpenAICompat { .. } => "openai-compat",
        }
    }
}

// ---------------------------------------------------------------------------
// PrivacyModeViolation
// ---------------------------------------------------------------------------

/// Error returned by [`BrainRouter::route_checked`] when privacy mode is
/// active and a cloud provider would otherwise be selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivacyModeViolation {
    /// Name of the cloud provider that triggered the violation.
    pub provider: String,
}

impl std::fmt::Display for PrivacyModeViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "privacy mode active: cloud provider '{}' blocked",
            self.provider
        )
    }
}

impl std::error::Error for PrivacyModeViolation {}

// ---------------------------------------------------------------------------
// RouterConfig
// ---------------------------------------------------------------------------

/// Router configuration.
pub struct RouterConfig {
    /// Available backends, in priority order (cheapest first).
    pub backends: Vec<ModelBackend>,
    /// Whether to cascade (try cheap first, escalate on failure).
    pub cascade: bool,
    /// Confidence threshold below which to escalate to next tier.
    pub confidence_threshold: f32,
    /// When `true`, cloud backends are rejected before dispatch.
    ///
    /// Set this to mirror the application-level `privacy_mode` flag from
    /// `PhantomConfig`. The router enforces the policy at the routing layer
    /// so that no cloud call can slip through regardless of which code path
    /// triggers routing.
    pub privacy_mode: bool,
    /// When `true`, only local backends (Ollama, heuristic) are used.
    /// Cloud backends are filtered out at routing time.
    ///
    /// Set this to mirror the application-level `offline_mode` flag from
    /// `PhantomConfig`. Can be auto-enabled after 3 consecutive cloud failures.
    pub offline_mode: bool,
}

impl RouterConfig {
    /// Build a `RouterConfig` that promotes the named backend to the front of
    /// the cascade so it is tried first across every task tier it supports.
    ///
    /// The `preferred_id` is matched against the default backend names:
    /// - `"claude-default"` / `"claude-sonnet"` / `"claude"` → promote `claude-sonnet`
    /// - `"claude-fast"` / `"claude-haiku"` → swap in a Haiku backend
    /// - `"ollama-phi3.5"` / `"ollama"` / `"phi3.5"` → promote the Ollama backend
    /// - `"ollama-llama3"` / `"llama3"` → swap in a llama3 Ollama backend
    ///
    /// Unknown IDs are silently ignored and the default cascade is returned.
    ///
    /// The heuristic backend is always present (it is free and zero-latency).
    #[must_use]
    pub fn with_preferred_provider(preferred_id: &str) -> Self {
        let mut cfg = Self::default();

        // Normalise the id: strip quotes and lower-case for loose matching.
        let id = preferred_id.trim().to_lowercase();

        match id.as_str() {
            // Promote the existing claude-sonnet backend to index 1 (after heuristic).
            "claude-default" | "claude-sonnet" | "claude" => {
                if let Some(pos) = cfg
                    .backends
                    .iter()
                    .position(|b| matches!(b.kind, BackendKind::Claude { .. }))
                {
                    let claude = cfg.backends.remove(pos);
                    cfg.backends.insert(1, claude);
                }
            }

            // Swap in a Haiku Claude backend and promote it.
            "claude-fast" | "claude-haiku" => {
                if let Some(pos) = cfg
                    .backends
                    .iter()
                    .position(|b| matches!(b.kind, BackendKind::Claude { .. }))
                {
                    cfg.backends[pos] = ModelBackend {
                        name: "claude-haiku".into(),
                        kind: BackendKind::Claude {
                            model: "claude-haiku-4-5".into(),
                        },
                        capabilities: vec![
                            TaskComplexity::Trivial,
                            TaskComplexity::Simple,
                            TaskComplexity::Complex,
                        ],
                        available: std::env::var("ANTHROPIC_API_KEY").is_ok(),
                        avg_latency_ms: 0.0,
                        max_context: 200_000,
                        cost_per_1k: 0.00025,
                    };
                    // Promote to index 1 (right after heuristic).
                    let claude = cfg.backends.remove(pos);
                    cfg.backends.insert(1, claude);
                }
            }

            // Promote the Ollama phi3.5 backend to index 1 (it is already there
            // by default, but this makes the intent explicit).
            "ollama-phi3.5" | "ollama" | "phi3.5" => {
                if let Some(pos) = cfg
                    .backends
                    .iter()
                    .position(|b| matches!(b.kind, BackendKind::Ollama { .. }))
                    && pos != 1
                {
                    let ollama = cfg.backends.remove(pos);
                    cfg.backends.insert(1, ollama);
                }
            }

            // Swap in a llama3 Ollama backend and promote it.
            "ollama-llama3" | "llama3" => {
                if let Some(pos) = cfg
                    .backends
                    .iter()
                    .position(|b| matches!(b.kind, BackendKind::Ollama { .. }))
                {
                    cfg.backends[pos] = ModelBackend {
                        name: "ollama-llama3".into(),
                        kind: BackendKind::Ollama {
                            model: "llama3:latest".into(),
                        },
                        capabilities: vec![
                            TaskComplexity::Trivial,
                            TaskComplexity::Simple,
                            TaskComplexity::Complex,
                        ],
                        available: false, // health-checked at startup
                        avg_latency_ms: 0.0,
                        max_context: 8192,
                        cost_per_1k: 0.0,
                    };
                    // Promote to index 1.
                    let ollama = cfg.backends.remove(pos);
                    cfg.backends.insert(1, ollama);
                }
            }

            _ => {
                log::warn!(
                    "BrainRouter: unknown preferred_provider '{preferred_id}'; \
                     using default cascade order"
                );
            }
        }

        cfg
    }
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            backends: vec![
                ModelBackend::heuristic(),
                ModelBackend::ollama_default(),
                ModelBackend::claude_default(),
            ],
            cascade: true,
            confidence_threshold: 0.7,
            privacy_mode: false,
            offline_mode: false,
        }
    }
}

// ---------------------------------------------------------------------------
// TaskClassifier
// ---------------------------------------------------------------------------

/// Classifies [`AiEvent`]s into [`TaskComplexity`] levels.
pub struct TaskClassifier;

impl TaskClassifier {
    /// Classify an event into a task complexity level.
    #[must_use] 
    pub fn classify(event: &AiEvent) -> TaskComplexity {
        match event {
            // Error detection / triage — heuristics can handle this.
            AiEvent::CommandComplete(parsed) => {
                if parsed.errors.is_empty() {
                    TaskComplexity::Trivial
                } else if parsed.errors.len() <= 2 {
                    TaskComplexity::Simple
                } else {
                    TaskComplexity::Complex
                }
            }
            // User idle — just scoring, trivial.
            AiEvent::UserIdle { .. } => TaskComplexity::Trivial,
            // File/git changes — classification.
            AiEvent::FileChanged(_) | AiEvent::GitStateChanged => TaskComplexity::Trivial,
            // Agent requests — user wants real work done.
            AiEvent::AgentRequest(_) => TaskComplexity::Complex,
            // Agent completion — summarization.
            AiEvent::AgentComplete { .. } => TaskComplexity::Simple,
            // User interrupt — could be anything, default to simple.
            AiEvent::Interrupt(_) => TaskComplexity::Simple,
            // Everything else (OutputChunk, AgentNeedsInput, WatcherTick, Shutdown).
            _ => TaskComplexity::Trivial,
        }
    }
}

// ---------------------------------------------------------------------------
// BrainRouter
// ---------------------------------------------------------------------------

/// Routes tasks to the best available backend using a cost-aware cascade.
pub struct BrainRouter {
    config: RouterConfig,
    /// Counter for consecutive cloud backend failures.
    /// Auto-enables offline mode after 3 failures.
    cloud_failure_count: u32,
}

impl BrainRouter {
    /// Create a new router with the given configuration.
    #[must_use] 
    pub fn new(config: RouterConfig) -> Self {
        Self {
            config,
            cloud_failure_count: 0,
        }
    }

    /// Select the best backend for a task.
    /// Returns backends in cascade order (cheapest capable first).
    ///
    /// When privacy mode is active, cloud backends are excluded from the
    /// candidate set — only local backends (heuristic, Ollama) are returned.
    #[must_use]
    pub fn route(&self, complexity: TaskComplexity) -> Vec<&ModelBackend> {
        // No `PeerGrantRegistry` check here: `BrainRouter` dispatches only within
        // this process. Cloud backends (Claude, OpenAI-compat) are called by the
        // brain thread; they do not traverse the relay. Cross-peer LLM calls are
        // gated at the relay by `CapabilityClass::Llm`.
        let mut candidates: Vec<&ModelBackend> = self
            .config
            .backends
            .iter()
            .filter(|b| {
                b.available
                    && b.capabilities.contains(&complexity)
                    // If offline mode is on, reject cloud backends
                    && !(self.config.offline_mode && b.is_cloud_provider())
                    // If privacy mode is on, reject cloud backends
                    && !(self.config.privacy_mode && b.is_cloud_provider())
            })
            .collect();

        // Sort by cost, then latency.
        candidates.sort_by(|a, b| {
            a.cost_per_1k
                .partial_cmp(&b.cost_per_1k)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(
                    a.avg_latency_ms
                        .partial_cmp(&b.avg_latency_ms)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
        });

        candidates
    }

    /// Update a backend's availability and latency after a call.
    ///
    /// If a cloud backend fails, increments failure counter and auto-enables
    /// offline mode after 3 consecutive cloud failures.
    pub fn record_result(&mut self, backend_name: &str, latency_ms: f32, success: bool) {
        if let Some(backend) = self
            .config
            .backends
            .iter_mut()
            .find(|b| b.name == backend_name)
        {
            if backend.avg_latency_ms == 0.0 {
                backend.avg_latency_ms = latency_ms;
            } else {
                // Exponential moving average.
                backend.avg_latency_ms = backend.avg_latency_ms * 0.8 + latency_ms * 0.2;
            }
            if !success {
                // Mark unavailable on failure, will be re-checked later.
                backend.available = false;

                // Track consecutive cloud failures.
                if backend.is_cloud_provider() {
                    self.cloud_failure_count += 1;
                    if self.cloud_failure_count >= 3 {
                        log::warn!(
                            "Cloud backend '{}' failed 3+ times; enabling offline mode",
                            backend_name
                        );
                        self.config.offline_mode = true;
                    }
                }
            } else {
                // On success, reset cloud failure counter.
                if backend.is_cloud_provider() {
                    self.cloud_failure_count = 0;
                }
            }
        }
    }

    /// Re-check availability of all backends.
    pub fn health_check(&mut self) {
        for backend in &mut self.config.backends {
            match &backend.kind {
                BackendKind::Heuristic => {
                    backend.available = true;
                }
                BackendKind::Ollama { .. } => {
                    // Will be checked by attempting a ping in the brain loop.
                    // For now, preserve current availability state.
                }
                BackendKind::Claude { .. } => {
                    // Available if API key is set.
                    backend.available = std::env::var("ANTHROPIC_API_KEY").is_ok();
                }
                BackendKind::OpenAICompat { .. } => {
                    // Check on demand — preserve current state.
                }
            }
        }
    }

    /// Set a backend's availability by name.
    pub fn set_backend_available(&mut self, name: &str, available: bool) {
        if let Some(backend) = self.config.backends.iter_mut().find(|b| b.name == name) {
            backend.available = available;
        }
    }

    /// Get the confidence threshold for cascade escalation.
    #[must_use] 
    pub fn confidence_threshold(&self) -> f32 {
        self.config.confidence_threshold
    }

    /// Whether cascade mode is enabled.
    #[must_use] 
    pub fn cascade_enabled(&self) -> bool {
        self.config.cascade
    }

    /// Enable or disable privacy mode on the router.
    ///
    /// When `true`, [`Self::route_checked`] will reject any cloud-provider
    /// backend (Claude, OpenAI-compat) with a [`PrivacyModeViolation`] error.
    pub fn set_privacy_mode(&mut self, enabled: bool) {
        self.config.privacy_mode = enabled;
    }

    /// Returns `true` if privacy mode is currently active.
    #[must_use] 
    pub fn privacy_mode(&self) -> bool {
        self.config.privacy_mode
    }

    /// Enable or disable offline mode on the router.
    ///
    /// When `true`, [`Self::route`] will filter to only local backends
    /// (heuristic, Ollama), rejecting all cloud providers.
    pub fn set_offline_mode(&mut self, enabled: bool) {
        self.config.offline_mode = enabled;
    }

    /// Returns `true` if offline mode is currently active.
    #[must_use] 
    pub fn offline_mode(&self) -> bool {
        self.config.offline_mode
    }

    /// Route a task with privacy enforcement.
    ///
    /// Behaves identically to [`Self::route`] except that when privacy mode is
    /// active any cloud-provider backend in the candidate list causes a
    /// [`PrivacyModeViolation`] error to be returned. Non-cloud backends
    /// (heuristic, Ollama) are returned normally.
    ///
    /// Privacy is enforced in two layers:
    ///
    /// 1. **Primary** — [`Self::route`] filters cloud backends out of the
    ///    candidate list when `privacy_mode` is true (see line ~304).
    /// 2. **Defence-in-depth** — [`Self::enforce_privacy`] (called below)
    ///    re-scans the post-filter candidate list and returns
    ///    [`PrivacyModeViolation`] if any cloud backend slipped through.
    ///
    /// Under current code the defence-in-depth layer never fires because the
    /// primary filter is always applied first. The secondary scan exists to
    /// catch a future regression where [`Self::route`]'s filter is removed,
    /// reordered, or short-circuited. See
    /// [`tests::enforce_privacy_catches_cloud_backend_in_candidates`] for a
    /// direct test of the defence-in-depth path that does not depend on
    /// [`Self::route`].
    ///
    /// # Errors
    ///
    /// Returns the first [`PrivacyModeViolation`] encountered among the
    /// selected candidates when privacy mode is on and at least one cloud
    /// backend would otherwise be selected.
    pub fn route_checked(
        &self,
        complexity: TaskComplexity,
    ) -> Result<Vec<&ModelBackend>, PrivacyModeViolation> {
        let candidates = self.route(complexity);
        Self::enforce_privacy(self.config.privacy_mode, &candidates)?;
        Ok(candidates)
    }

    /// Defence-in-depth privacy check on a candidate list.
    ///
    /// Returns [`PrivacyModeViolation`] when `privacy_mode` is true and any
    /// candidate is a cloud provider. Used by [`Self::route_checked`] as a
    /// secondary scan after [`Self::route`]'s primary cloud filter; also
    /// directly testable to prove the layer works without depending on
    /// [`Self::route`]'s correctness.
    ///
    /// # Errors
    ///
    /// Returns [`PrivacyModeViolation`] for the first cloud backend found.
    fn enforce_privacy(
        privacy_mode: bool,
        candidates: &[&ModelBackend],
    ) -> Result<(), PrivacyModeViolation> {
        if !privacy_mode {
            return Ok(());
        }
        for backend in candidates {
            if backend.is_cloud_provider() {
                return Err(PrivacyModeViolation {
                    provider: backend.provider_name().to_string(),
                });
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_agents::AgentTask;
    use phantom_semantic::{
        CommandType, ContentType, DetectedError, ErrorType, ParsedOutput, Severity,
    };

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn parsed_no_errors() -> ParsedOutput {
        ParsedOutput {
            command: "ls".into(),
            command_type: CommandType::Shell,
            exit_code: Some(0),
            content_type: ContentType::PlainText,
            errors: vec![],
            warnings: vec![],
            duration_ms: Some(5),
            raw_output: "file1".into(),
        }
    }

    fn parsed_one_error() -> ParsedOutput {
        ParsedOutput {
            command: "cargo build".into(),
            command_type: CommandType::Cargo(phantom_semantic::CargoCommand::Build),
            exit_code: Some(1),
            content_type: ContentType::CompilerOutput,
            errors: vec![make_error("type mismatch")],
            warnings: vec![],
            duration_ms: Some(500),
            raw_output: "error: type mismatch".into(),
        }
    }

    fn parsed_many_errors() -> ParsedOutput {
        ParsedOutput {
            command: "cargo build".into(),
            command_type: CommandType::Cargo(phantom_semantic::CargoCommand::Build),
            exit_code: Some(1),
            content_type: ContentType::CompilerOutput,
            errors: vec![
                make_error("error 1"),
                make_error("error 2"),
                make_error("error 3"),
            ],
            warnings: vec![],
            duration_ms: Some(2000),
            raw_output: "errors".into(),
        }
    }

    fn make_error(msg: &str) -> DetectedError {
        DetectedError {
            message: msg.into(),
            error_type: ErrorType::Compiler,
            file: None,
            line: None,
            column: None,
            code: None,
            severity: Severity::Error,
            raw_line: String::new(),
            suggestion: None,
        }
    }

    // =======================================================================
    // TaskClassifier tests
    // =======================================================================

    #[test]
    fn classify_command_complete_no_errors_is_trivial() {
        let event = AiEvent::CommandComplete(parsed_no_errors());
        assert_eq!(TaskClassifier::classify(&event), TaskComplexity::Trivial);
    }

    #[test]
    fn classify_command_complete_few_errors_is_simple() {
        let event = AiEvent::CommandComplete(parsed_one_error());
        assert_eq!(TaskClassifier::classify(&event), TaskComplexity::Simple);
    }

    #[test]
    fn classify_command_complete_many_errors_is_complex() {
        let event = AiEvent::CommandComplete(parsed_many_errors());
        assert_eq!(TaskClassifier::classify(&event), TaskComplexity::Complex);
    }

    #[test]
    fn classify_user_idle_is_trivial() {
        let event = AiEvent::UserIdle { seconds: 30.0 };
        assert_eq!(TaskClassifier::classify(&event), TaskComplexity::Trivial);
    }

    #[test]
    fn classify_file_changed_is_trivial() {
        let event = AiEvent::FileChanged("src/main.rs".into());
        assert_eq!(TaskClassifier::classify(&event), TaskComplexity::Trivial);
    }

    #[test]
    fn classify_git_state_changed_is_trivial() {
        let event = AiEvent::GitStateChanged;
        assert_eq!(TaskClassifier::classify(&event), TaskComplexity::Trivial);
    }

    #[test]
    fn classify_agent_request_is_complex() {
        let event = AiEvent::AgentRequest(AgentTask::FreeForm {
            prompt: "fix this".into(),
        });
        assert_eq!(TaskClassifier::classify(&event), TaskComplexity::Complex);
    }

    #[test]
    fn classify_agent_complete_is_simple() {
        let event = AiEvent::AgentComplete {
            id: 1,
            success: true,
            summary: "done".into(),
            spawn_tag: None,
        };
        assert_eq!(TaskClassifier::classify(&event), TaskComplexity::Simple);
    }

    #[test]
    fn classify_interrupt_is_simple() {
        let event = AiEvent::Interrupt("help".into());
        assert_eq!(TaskClassifier::classify(&event), TaskComplexity::Simple);
    }

    #[test]
    fn classify_output_chunk_is_trivial() {
        let event = AiEvent::OutputChunk("partial output".into());
        assert_eq!(TaskClassifier::classify(&event), TaskComplexity::Trivial);
    }

    // =======================================================================
    // BrainRouter tests
    // =======================================================================

    #[test]
    fn route_trivial_to_heuristic() {
        let router = BrainRouter::new(RouterConfig::default());
        let backends = router.route(TaskComplexity::Trivial);
        assert!(!backends.is_empty(), "should have at least one backend");
        assert_eq!(
            backends[0].name, "heuristic",
            "cheapest trivial backend should be heuristic"
        );
    }

    #[test]
    fn route_complex_to_claude_when_available() {
        let mut config = RouterConfig::default();
        // Force Claude available for this test.
        for b in &mut config.backends {
            if b.name == "claude-sonnet" {
                b.available = true;
            }
        }
        let router = BrainRouter::new(config);
        let backends = router.route(TaskComplexity::Complex);
        assert!(
            !backends.is_empty(),
            "should have a complex-capable backend"
        );
        // Claude is the only backend with Complex capability.
        assert_eq!(backends.last().unwrap().name, "claude-sonnet");
    }

    #[test]
    fn route_cascade_ordering_cheapest_first() {
        let mut config = RouterConfig::default();
        // Make all backends available.
        for b in &mut config.backends {
            b.available = true;
        }
        let router = BrainRouter::new(config);
        let backends = router.route(TaskComplexity::Trivial);

        // Should be sorted by cost: heuristic (0.0), ollama (0.0), claude (0.003).
        assert!(backends.len() >= 2);
        assert!(
            backends[0].cost_per_1k <= backends.last().unwrap().cost_per_1k,
            "backends should be sorted cheapest first"
        );
    }

    #[test]
    fn route_skips_unavailable_backends() {
        let mut config = RouterConfig::default();
        // Mark ALL backends unavailable.
        for b in &mut config.backends {
            b.available = false;
        }
        let router = BrainRouter::new(config);
        let backends = router.route(TaskComplexity::Trivial);
        assert!(
            backends.is_empty(),
            "no backends should be returned when all unavailable"
        );
    }

    #[test]
    fn route_returns_empty_when_no_backends_for_complexity() {
        let config = RouterConfig {
            backends: vec![ModelBackend::heuristic()],
            cascade: true,
            confidence_threshold: 0.7,
            privacy_mode: false,
            offline_mode: false,
        };
        let router = BrainRouter::new(config);
        // Heuristic only handles Trivial, not Complex.
        let backends = router.route(TaskComplexity::Complex);
        assert!(backends.is_empty(), "heuristic cannot handle Complex tasks");
    }

    #[test]
    fn route_with_only_heuristic_still_routes_trivial() {
        let config = RouterConfig {
            backends: vec![ModelBackend::heuristic()],
            cascade: false,
            confidence_threshold: 0.7,
            privacy_mode: false,
            offline_mode: false,
        };
        let router = BrainRouter::new(config);
        let backends = router.route(TaskComplexity::Trivial);
        assert_eq!(backends.len(), 1);
        assert_eq!(backends[0].name, "heuristic");
    }

    #[test]
    fn record_result_updates_latency_ema() {
        let mut router = BrainRouter::new(RouterConfig::default());
        // First call sets latency directly.
        router.record_result("heuristic", 100.0, true);
        let backend = router
            .config
            .backends
            .iter()
            .find(|b| b.name == "heuristic")
            .unwrap();
        assert!(
            (backend.avg_latency_ms - 100.0).abs() < f32::EPSILON,
            "first call should set latency directly"
        );

        // Second call applies EMA: 100 * 0.8 + 200 * 0.2 = 120.
        router.record_result("heuristic", 200.0, true);
        let backend = router
            .config
            .backends
            .iter()
            .find(|b| b.name == "heuristic")
            .unwrap();
        assert!(
            (backend.avg_latency_ms - 120.0).abs() < 0.01,
            "EMA should be 120, got {}",
            backend.avg_latency_ms
        );
    }

    #[test]
    fn record_result_marks_failed_backend_unavailable() {
        let mut router = BrainRouter::new(RouterConfig::default());
        // Heuristic starts available.
        assert!(router.config.backends[0].available);

        router.record_result("heuristic", 50.0, false);

        let backend = router
            .config
            .backends
            .iter()
            .find(|b| b.name == "heuristic")
            .unwrap();
        assert!(
            !backend.available,
            "failed backend should be marked unavailable"
        );
    }

    #[test]
    fn health_check_restores_heuristic_availability() {
        let mut router = BrainRouter::new(RouterConfig::default());
        // Mark heuristic unavailable.
        router.config.backends[0].available = false;

        router.health_check();

        let backend = router
            .config
            .backends
            .iter()
            .find(|b| b.name == "heuristic")
            .unwrap();
        assert!(
            backend.available,
            "health_check should restore heuristic availability"
        );
    }

    #[test]
    fn router_config_default_has_three_backends() {
        let config = RouterConfig::default();
        assert_eq!(
            config.backends.len(),
            3,
            "default config should have heuristic + ollama + claude"
        );
    }

    #[test]
    fn model_backend_heuristic_fields() {
        let b = ModelBackend::heuristic();
        assert_eq!(b.name, "heuristic");
        assert_eq!(b.kind, BackendKind::Heuristic);
        assert!(b.available);
        assert_eq!(b.cost_per_1k, 0.0);
        assert_eq!(b.capabilities, vec![TaskComplexity::Trivial]);
    }

    #[test]
    fn model_backend_ollama_default_fields() {
        let b = ModelBackend::ollama_default();
        assert_eq!(b.name, "ollama-phi3.5");
        assert!(!b.available, "ollama should start unavailable");
        assert_eq!(b.max_context, 8192);
        assert_eq!(b.cost_per_1k, 0.0);
        assert!(b.capabilities.contains(&TaskComplexity::Trivial));
        assert!(b.capabilities.contains(&TaskComplexity::Simple));
    }

    #[test]
    fn model_backend_claude_default_fields() {
        let b = ModelBackend::claude_default();
        assert_eq!(b.name, "claude-sonnet");
        assert_eq!(b.max_context, 200_000);
        assert!(b.cost_per_1k > 0.0);
        assert!(b.capabilities.contains(&TaskComplexity::Complex));
    }

    #[test]
    fn claude_backend_availability_depends_on_api_key() {
        // We can't easily control env vars in parallel tests, but we can
        // verify the constructor checks for ANTHROPIC_API_KEY.
        let b = ModelBackend::claude_default();
        let key_is_set = std::env::var("ANTHROPIC_API_KEY").is_ok();
        assert_eq!(
            b.available, key_is_set,
            "Claude availability should match whether ANTHROPIC_API_KEY is set"
        );
    }

    #[test]
    fn router_confidence_threshold_and_cascade() {
        let router = BrainRouter::new(RouterConfig::default());
        assert!((router.confidence_threshold() - 0.7).abs() < f32::EPSILON);
        assert!(router.cascade_enabled());
    }

    #[test]
    fn record_result_ignores_unknown_backend() {
        let mut router = BrainRouter::new(RouterConfig::default());
        // Should not panic or change any state.
        router.record_result("nonexistent-backend", 100.0, true);
        // Verify existing backends are unchanged.
        let heuristic = router
            .config
            .backends
            .iter()
            .find(|b| b.name == "heuristic")
            .unwrap();
        assert_eq!(heuristic.avg_latency_ms, 0.0);
    }

    // =======================================================================
    // Privacy mode tests
    // =======================================================================

    #[test]
    fn backend_kind_is_cloud_provider_correct() {
        assert!(!BackendKind::Heuristic.is_cloud_provider());
        assert!(
            !BackendKind::Ollama {
                model: "phi3.5".into()
            }
            .is_cloud_provider()
        );
        assert!(
            BackendKind::Claude {
                model: "claude-sonnet".into()
            }
            .is_cloud_provider()
        );
        assert!(
            BackendKind::OpenAICompat {
                base_url: "https://api.openai.com".into(),
                model: "gpt-4o".into()
            }
            .is_cloud_provider()
        );
    }

    #[test]
    fn privacy_mode_defaults_to_false() {
        let router = BrainRouter::new(RouterConfig::default());
        assert!(!router.privacy_mode());
    }

    #[test]
    fn set_privacy_mode_toggles_correctly() {
        let mut router = BrainRouter::new(RouterConfig::default());
        router.set_privacy_mode(true);
        assert!(router.privacy_mode());
        router.set_privacy_mode(false);
        assert!(!router.privacy_mode());
    }

    #[test]
    fn privacy_mode_excludes_cloud_backends_from_routing() {
        let mut config = RouterConfig::default();
        // Make all backends available.
        for b in &mut config.backends {
            b.available = true;
        }
        let mut router = BrainRouter::new(config);
        router.set_privacy_mode(true);

        // With privacy mode on, Claude should not appear in Complex routing.
        let backends = router.route(TaskComplexity::Complex);
        assert!(
            backends.iter().all(|b| !b.kind.is_cloud_provider()),
            "privacy mode must exclude cloud backends; got: {:?}",
            backends.iter().map(|b| &b.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn privacy_mode_does_not_exclude_local_backends() {
        let mut config = RouterConfig::default();
        // Make all backends available, including Ollama.
        for b in &mut config.backends {
            b.available = true;
        }
        let mut router = BrainRouter::new(config);
        router.set_privacy_mode(true);

        // Heuristic (local) must still route for Trivial tasks.
        let backends = router.route(TaskComplexity::Trivial);
        assert!(
            !backends.is_empty(),
            "privacy mode must not block local (heuristic) backend"
        );
        assert!(
            backends.iter().any(|b| b.name == "heuristic"),
            "heuristic backend must remain available in privacy mode"
        );
    }

    #[test]
    fn privacy_mode_off_still_routes_to_claude() {
        let mut config = RouterConfig::default();
        // Force Claude available.
        for b in &mut config.backends {
            if b.name == "claude-sonnet" {
                b.available = true;
            }
        }
        let mut router = BrainRouter::new(config);
        router.set_privacy_mode(false); // privacy mode OFF

        let backends = router.route(TaskComplexity::Complex);
        assert!(
            backends.iter().any(|b| b.name == "claude-sonnet"),
            "with privacy mode off, Claude must be routable"
        );
    }

    #[test]
    fn privacy_mode_complex_task_returns_empty_without_local_model() {
        // Only Claude in the config; privacy mode on → empty route for Complex.
        let config = RouterConfig {
            backends: vec![{
                let mut b = ModelBackend::claude_default();
                b.available = true;
                b
            }],
            cascade: true,
            confidence_threshold: 0.7,
            privacy_mode: false,
            offline_mode: false,
        };
        let mut router = BrainRouter::new(config);
        router.set_privacy_mode(true);

        let backends = router.route(TaskComplexity::Complex);
        assert!(
            backends.is_empty(),
            "with privacy mode on and only Claude, Complex must return empty"
        );
    }

    // -----------------------------------------------------------------------
    // Defence-in-depth: enforce_privacy direct tests (closes #475)
    //
    // route_checked's secondary cloud-backend scan cannot fire under normal
    // operation because route() filters cloud backends out first. These tests
    // exercise the secondary scan via the extracted helper, proving the layer
    // works even if route()'s primary filter is ever removed or short-circuited.
    // -----------------------------------------------------------------------

    #[test]
    fn enforce_privacy_off_returns_ok_with_cloud_present() {
        // Privacy off → cloud backends in candidates are fine.
        let claude = {
            let mut b = ModelBackend::claude_default();
            b.available = true;
            b
        };
        let candidates = vec![&claude];
        let result = BrainRouter::enforce_privacy(false, &candidates);
        assert!(
            result.is_ok(),
            "enforce_privacy must accept cloud candidates when privacy mode is OFF"
        );
    }

    #[test]
    fn enforce_privacy_on_with_only_local_returns_ok() {
        // Privacy on, candidates list contains only local backends → Ok.
        let heuristic = ModelBackend::heuristic();
        let candidates = vec![&heuristic];
        let result = BrainRouter::enforce_privacy(true, &candidates);
        assert!(
            result.is_ok(),
            "enforce_privacy must accept all-local candidates when privacy mode is ON"
        );
    }

    #[test]
    fn enforce_privacy_catches_cloud_backend_in_candidates() {
        // Defence-in-depth: simulate route() being bypassed or broken.
        // We pass a candidates list that contains a cloud backend even though
        // privacy_mode is true — exactly the scenario route_checked's secondary
        // scan exists to catch. enforce_privacy must return PrivacyModeViolation.
        let claude = {
            let mut b = ModelBackend::claude_default();
            b.available = true;
            b
        };
        let candidates = vec![&claude];
        let result = BrainRouter::enforce_privacy(true, &candidates);
        match result {
            Err(PrivacyModeViolation { provider }) => {
                assert_eq!(
                    provider, "claude",
                    "violation must name the offending cloud provider"
                );
            }
            Ok(_) => panic!(
                "defence-in-depth scan failed to catch cloud backend leaked past route() filter"
            ),
        }
    }

    #[test]
    fn enforce_privacy_catches_first_cloud_in_mixed_candidates() {
        // Defence-in-depth: cloud backend mixed in with local ones.
        // enforce_privacy must still flag the cloud backend.
        let heuristic = ModelBackend::heuristic();
        let openai = ModelBackend {
            name: "openai-gpt4o".into(),
            kind: BackendKind::OpenAICompat {
                base_url: "https://api.openai.com".into(),
                model: "gpt-4o".into(),
            },
            capabilities: vec![TaskComplexity::Complex],
            available: true,
            avg_latency_ms: 0.0,
            max_context: 128_000,
            cost_per_1k: 0.005,
        };
        let candidates = vec![&heuristic, &openai];
        let result = BrainRouter::enforce_privacy(true, &candidates);
        match result {
            Err(PrivacyModeViolation { provider }) => {
                assert_eq!(
                    provider, "openai-compat",
                    "violation must name the openai-compat cloud provider"
                );
            }
            Ok(_) => panic!(
                "defence-in-depth scan failed to catch openai-compat backend in mixed candidates"
            ),
        }
    }
}
