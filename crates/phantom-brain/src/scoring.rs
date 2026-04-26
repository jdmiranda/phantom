//! Utility AI scoring engine.
//!
//! Each scorer examines context and returns a [`ScoredAction`] with a 0.0–1.0
//! score. The brain calls [`UtilityScorer::evaluate`] on every event, which
//! runs all scorers and returns the highest-scoring action. If nothing beats
//! the quiet baseline, the brain stays silent.
//!
//! Inspired by game AI utility systems — see `docs/research/ai-control-loop.md`.

use std::collections::hash_map::DefaultHasher;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::time::Instant;

use phantom_context::ProjectContext;
use phantom_memory::MemoryStore;
use phantom_semantic::ParsedOutput;

use crate::events::{AiAction, AiEvent, SuggestionOption};

// ---------------------------------------------------------------------------
// ScoredAction
// ---------------------------------------------------------------------------

/// A candidate action with its utility score and reasoning.
#[derive(Debug, Clone)]
pub struct ScoredAction {
    /// The action to take if this candidate wins.
    pub action: AiAction,
    /// Utility score in the range `[0.0, 1.0]`.
    pub score: f32,
    /// Human-readable explanation (for debug logging).
    pub reason: String,
}

/// Maximum number of suggestions without user input before dampening hard.
const MAX_SUGGESTIONS_WITHOUT_INPUT: u32 = 2;

/// Minimum seconds between any two non-quiet actions from the brain.
const ACTION_COOLDOWN_SECS: f32 = 15.0;

/// How long a duplicate suggestion is suppressed (seconds).
const DEDUP_WINDOW_SECS: u64 = 60;

// ---------------------------------------------------------------------------
// UtilityScorer
// ---------------------------------------------------------------------------

/// The utility scoring engine that decides what the brain should do.
///
/// Maintains state about recent activity (chattiness dampener, idle time,
/// error state) so that scoring is context-aware across events.
pub struct UtilityScorer {
    /// How long since the last user input (seconds).
    pub idle_time: f32,
    /// Whether the last completed command had errors.
    pub last_had_errors: bool,
    /// How many times we've acted (non-quiet) since the user last did something.
    pub suggestions_since_input: u32,
    /// Chattiness dampener — increases the bar to suggest when we've been noisy.
    /// Increments by 0.1 per action, decays by 0.05 per second of idle time,
    /// resets to 0 on [`user_acted`].
    pub chattiness: f32,
    /// Recent suggestion texts for deduplication (text, timestamp).
    recent_suggestions: VecDeque<(String, Instant)>,
    /// When the last non-quiet action was emitted (for cooldown).
    last_action_time: Option<Instant>,
    /// Whether a long-running command is currently active.
    pub has_active_process: bool,
    /// Hash of the last error text we suggested a fix for.
    pub last_error_signature: Option<u64>,
    /// The most recent command the user ran (from `CommandComplete`).
    pub last_command: Option<String>,
}

impl UtilityScorer {
    /// Create a scorer with default (silent) initial state.
    pub fn new() -> Self {
        Self {
            idle_time: 0.0,
            last_had_errors: false,
            suggestions_since_input: 0,
            chattiness: 0.0,
            recent_suggestions: VecDeque::with_capacity(6),
            last_action_time: None,
            has_active_process: false,
            last_error_signature: None,
            last_command: None,
        }
    }

    /// Check if a suggestion text was recently shown (within dedup window).
    fn is_duplicate(&self, text: &str) -> bool {
        let now = Instant::now();
        self.recent_suggestions.iter().any(|(t, when)| {
            t == text && now.duration_since(*when).as_secs() < DEDUP_WINDOW_SECS
        })
    }

    /// Record a suggestion that was emitted (for dedup tracking).
    fn record_suggestion(&mut self, text: &str) {
        let now = Instant::now();
        // Evict expired entries.
        while self.recent_suggestions.front().is_some_and(|(_, when)| {
            now.duration_since(*when).as_secs() >= DEDUP_WINDOW_SECS
        }) {
            self.recent_suggestions.pop_front();
        }
        self.recent_suggestions.push_back((text.to_string(), now));
        // Cap at 5 entries.
        while self.recent_suggestions.len() > 5 {
            self.recent_suggestions.pop_front();
        }
    }

    /// Returns true if the action cooldown has not yet elapsed.
    fn on_cooldown(&self) -> bool {
        self.last_action_time.is_some_and(|t| {
            t.elapsed().as_secs_f32() < ACTION_COOLDOWN_SECS
        })
    }

    // -----------------------------------------------------------------------
    // Individual scorers
    // -----------------------------------------------------------------------

    /// Score "suggest fixing an error" action.
    ///
    /// - **0.9** if errors just appeared and no fix is in progress.
    /// - **0.3** if errors are old (idle > 30s since the error).
    /// - **0.0** if no errors or the user is actively typing (idle < 2s).
    pub fn fix_score(&mut self, parsed: &ParsedOutput, _context: &ProjectContext) -> ScoredAction {
        let has_errors = !parsed.errors.is_empty();

        if !has_errors {
            return ScoredAction {
                action: AiAction::DoNothing,
                score: 0.0,
                reason: "no errors to fix".into(),
            };
        }

        // Compute error signature for dedup.
        let sig = {
            let mut hasher = DefaultHasher::new();
            for e in &parsed.errors {
                e.message.hash(&mut hasher);
            }
            hasher.finish()
        };

        if self.last_error_signature == Some(sig) {
            return ScoredAction {
                action: AiAction::DoNothing,
                score: 0.0,
                reason: "already suggested for this error".into(),
            };
        }

        // User is actively typing — don't interrupt.
        if self.idle_time < 2.0 {
            return ScoredAction {
                action: self.build_fix_action(parsed),
                score: 0.0,
                reason: "errors present but user is typing".into(),
            };
        }

        // Record signature so we don't re-suggest for the same error.
        self.last_error_signature = Some(sig);

        // Fresh errors.
        if self.idle_time < 30.0 {
            return ScoredAction {
                action: self.build_fix_action(parsed),
                score: 0.9,
                reason: "fresh errors, user idle".into(),
            };
        }

        // Stale errors.
        ScoredAction {
            action: self.build_fix_action(parsed),
            score: 0.3,
            reason: "errors are old".into(),
        }
    }

    /// Score "offer to explain" action.
    ///
    /// - **0.7** if idle > 10s after an error (user might be stuck).
    /// - **0.3** if idle > 30s without an error (user might want context).
    /// - **0.0** if user is actively typing (idle < 5s).
    pub fn explain_score(&self, parsed: &ParsedOutput, idle_time: f32) -> ScoredAction {
        // Suppress "user stuck" heuristic when inside a REPL session.
        const REPL_COMMANDS: &[&str] = &[
            "python", "python3", "node", "irb", "ghci", "psql", "mysql",
            "sqlite3", "lua", "julia", "erl", "iex",
        ];
        if let Some(ref cmd) = self.last_command {
            let first_word = cmd.split_whitespace().next().unwrap_or("");
            if REPL_COMMANDS.iter().any(|r| first_word.ends_with(r)) {
                return ScoredAction {
                    action: AiAction::DoNothing,
                    score: 0.0,
                    reason: "user in REPL, not stuck".into(),
                };
            }
        }

        if idle_time < 5.0 {
            return ScoredAction {
                action: AiAction::DoNothing,
                score: 0.0,
                reason: "user is active, no explanation needed".into(),
            };
        }

        let has_errors = !parsed.errors.is_empty();

        if has_errors && idle_time > 10.0 {
            return ScoredAction {
                action: AiAction::ShowSuggestion {
                    text: "Want me to explain this error?".into(),
                    options: vec![
                        SuggestionOption { key: 'y', label: "Yes, explain".into(), action: Some(Box::new(AiAction::ConsoleReply("Let me explain...".into()))) },
                        SuggestionOption { key: 'n', label: "No thanks".into(), action: None },
                    ],
                },
                score: 0.7,
                reason: "user idle after error — may be stuck".into(),
            };
        }

        if idle_time > 30.0 {
            return ScoredAction {
                action: AiAction::ShowSuggestion {
                    text: "Need help with anything?".into(),
                    options: vec![
                        SuggestionOption { key: 'y', label: "Yes".into(), action: Some(Box::new(AiAction::ConsoleReply("Let me explain...".into()))) },
                        SuggestionOption { key: 'n', label: "No".into(), action: None },
                    ],
                },
                score: 0.3,
                reason: "user idle for a while, offering help".into(),
            };
        }

        ScoredAction {
            action: AiAction::DoNothing,
            score: 0.0,
            reason: "not enough idle time to offer explanation".into(),
        }
    }

    /// Score "update project memory" action.
    ///
    /// - **0.6** if a new pattern is detected (key not already in memory).
    /// - **0.1** otherwise (pattern already known).
    pub fn memory_score(&self, event: &AiEvent, memory: &MemoryStore) -> ScoredAction {
        match event {
            AiEvent::CommandComplete(parsed) => {
                // Detect a potential new pattern: the command type as a memory key.
                let key = format!("cmd:{}", parsed.command.split_whitespace().next().unwrap_or("unknown"));

                if memory.get(&key).is_none() {
                    ScoredAction {
                        action: AiAction::UpdateMemory {
                            key,
                            value: format!("seen command: {}", parsed.command),
                        },
                        score: 0.6,
                        reason: "new command pattern detected".into(),
                    }
                } else {
                    ScoredAction {
                        action: AiAction::DoNothing,
                        score: 0.1,
                        reason: "pattern already in memory".into(),
                    }
                }
            }
            _ => ScoredAction {
                action: AiAction::DoNothing,
                score: 0.1,
                reason: "non-command event, low memory relevance".into(),
            },
        }
    }

    /// Score "spawn a watcher" action.
    ///
    /// - **0.5** if a long-running process is *actively running*.
    /// - **0.0** otherwise (just having build commands isn't enough).
    pub fn watcher_score(&self, _context: &ProjectContext) -> ScoredAction {
        // Only offer to watch if there's evidence of an active long-running process.
        // Previously this fired for any project with build/test commands, causing
        // "Want me to watch this build?" spam every 5s on idle.
        if self.has_active_process {
            ScoredAction {
                action: AiAction::ShowSuggestion {
                    text: "Want me to watch this build?".into(),
                    options: vec![
                        SuggestionOption { key: 'y', label: "Watch it".into(), action: Some(Box::new(AiAction::SpawnAgent(phantom_agents::AgentTask::FreeForm { prompt: "Watch the build".into() }))) },
                        SuggestionOption { key: 'n', label: "No".into(), action: None },
                    ],
                },
                score: 0.5,
                reason: "active long-running process detected".into(),
            }
        } else {
            ScoredAction {
                action: AiAction::DoNothing,
                score: 0.0,
                reason: "no active process to watch".into(),
            }
        }
    }

    /// Score "show notification" action.
    ///
    /// - **0.8** for agent completion events.
    /// - **0.4** for file/git changes.
    /// - **0.0** for everything else.
    pub fn notification_score(&self, event: &AiEvent) -> ScoredAction {
        match event {
            AiEvent::AgentComplete { summary, .. } => ScoredAction {
                action: AiAction::ShowNotification(summary.clone()),
                score: 0.8,
                reason: "agent completed — user should know".into(),
            },
            AiEvent::GitStateChanged => ScoredAction {
                action: AiAction::ShowNotification("Git state changed".into()),
                score: 0.4,
                reason: "git state changed".into(),
            },
            AiEvent::FileChanged(path) => ScoredAction {
                action: AiAction::ShowNotification(format!("File changed: {path}")),
                score: 0.4,
                reason: "watched file changed".into(),
            },
            _ => ScoredAction {
                action: AiAction::DoNothing,
                score: 0.0,
                reason: "no notification needed".into(),
            },
        }
    }

    /// The quiet score — the baseline that the brain must beat to act.
    ///
    /// Starts at 0.5 and increases with the chattiness dampener. The more
    /// we've suggested recently, the higher the bar to suggest again.
    pub fn quiet_score(&self) -> ScoredAction {
        // Sentient mode: low baseline (0.1) so the brain speaks up more often.
        let score = (0.1 + self.chattiness).min(1.0);
        ScoredAction {
            action: AiAction::DoNothing,
            score,
            reason: format!("quiet baseline (chattiness={:.2})", self.chattiness),
        }
    }

    /// Evaluate all scorers for the given event and return the best action.
    ///
    /// This is the main entry point called by the brain loop on every event.
    /// After picking a winner, it updates the chattiness dampener.
    pub fn evaluate(
        &mut self,
        event: &AiEvent,
        context: &ProjectContext,
        memory: &MemoryStore,
    ) -> ScoredAction {
        // Update idle time from the event if applicable.
        if let AiEvent::UserIdle { seconds } = event {
            self.idle_time = *seconds;
        }

        // Collect all candidate scores.
        let mut candidates: Vec<ScoredAction> = Vec::new();

        // Always include the quiet baseline.
        candidates.push(self.quiet_score());

        // Notification scorer — applicable to many event types.
        candidates.push(self.notification_score(event));

        // Memory scorer.
        candidates.push(self.memory_score(event, memory));

        // Watcher scorer.
        candidates.push(self.watcher_score(context));

        // Scorers that need a ParsedOutput.
        if let AiEvent::CommandComplete(parsed) = event {
            self.last_had_errors = !parsed.errors.is_empty();
            candidates.push(self.fix_score(parsed, context));
            candidates.push(self.explain_score(parsed, self.idle_time));
        }

        // If idle event, try explain with a synthetic parsed output state.
        if let AiEvent::UserIdle { seconds } = event {
            if self.last_had_errors {
                // Create a minimal "has errors" marker for explain_score.
                let synthetic = ParsedOutput {
                    command: String::new(),
                    command_type: phantom_semantic::CommandType::Unknown,
                    exit_code: Some(1),
                    content_type: phantom_semantic::ContentType::PlainText,
                    errors: vec![phantom_semantic::DetectedError {
                        message: "previous error".into(),
                        error_type: phantom_semantic::ErrorType::Other,
                        file: None,
                        line: None,
                        column: None,
                        code: None,
                        severity: phantom_semantic::Severity::Error,
                        raw_line: String::new(),
                        suggestion: None,
                    }],
                    warnings: vec![],
                    duration_ms: None,
                    raw_output: String::new(),
                };
                candidates.push(self.explain_score(&synthetic, *seconds));
            }
        }

        // Pick the highest-scoring action.
        let mut best = candidates
            .into_iter()
            .max_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or_else(|| self.quiet_score());

        // --- Spam prevention gates ---

        // Gate 1: If we've already suggested too many times without user input,
        // heavily dampen all non-urgent scores.
        if self.suggestions_since_input >= MAX_SUGGESTIONS_WITHOUT_INPUT
            && !matches!(best.action, AiAction::DoNothing)
            && best.score < 0.8 // Allow truly urgent actions (agent complete, etc.)
        {
            best.score *= 0.2;
            best.reason = format!("{} (dampened: {} suggestions without input)", best.reason, self.suggestions_since_input);
        }

        // Gate 2: Dedup — suppress if we recently showed the same suggestion text.
        if let AiAction::ShowSuggestion { ref text, .. } = best.action {
            if self.is_duplicate(text) {
                return ScoredAction {
                    action: AiAction::DoNothing,
                    score: 0.0,
                    reason: format!("suppressed duplicate: {}", text),
                };
            }
        }

        // Gate 3: Cooldown — enforce minimum gap between actions.
        if !matches!(best.action, AiAction::DoNothing) && self.on_cooldown() {
            return ScoredAction {
                action: AiAction::DoNothing,
                score: 0.0,
                reason: "on cooldown".into(),
            };
        }

        // Update chattiness if we're going to act (non-quiet).
        if !matches!(best.action, AiAction::DoNothing) && best.score > 0.0 {
            self.chattiness = (self.chattiness + 0.1).min(0.5);
            self.suggestions_since_input += 1;
            self.last_action_time = Some(Instant::now());

            // Record suggestion text for dedup.
            if let AiAction::ShowSuggestion { ref text, .. } = best.action {
                self.record_suggestion(text);
            }
        }

        best
    }

    /// Called when the user takes an action (keystroke, command, etc.).
    ///
    /// Resets the chattiness dampener and suggestion counter so the brain
    /// is willing to suggest again.
    pub fn user_acted(&mut self) {
        self.chattiness = 0.0;
        self.suggestions_since_input = 0;
        self.idle_time = 0.0;
        self.last_error_signature = None; // re-enable fix suggestions for re-runs
        // Don't clear recent_suggestions — dedup persists across user actions.
        // Don't clear last_action_time — cooldown is absolute, not relative to user.
    }

    /// Decay chattiness based on elapsed idle time.
    ///
    /// Call this periodically (e.g. on `UserIdle` events). Decays at
    /// 0.05 per second of idle time.
    pub fn decay_chattiness(&mut self, idle_seconds: f32) {
        self.chattiness = (self.chattiness - 0.05 * idle_seconds).max(0.0);
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Build a fix suggestion action from parsed output.
    fn build_fix_action(&self, parsed: &ParsedOutput) -> AiAction {
        let error_summary = parsed
            .errors
            .first()
            .map(|e| e.message.clone())
            .unwrap_or_else(|| "unknown error".into());

        AiAction::ShowSuggestion {
            text: format!("Fix: {error_summary}"),
            options: vec![
                SuggestionOption { key: 'f', label: "Fix it".into(), action: Some(Box::new(AiAction::SpawnAgent(phantom_agents::AgentTask::FreeForm { prompt: error_summary }))) },
                SuggestionOption { key: 'e', label: "Explain".into(), action: Some(Box::new(AiAction::ConsoleReply("Let me explain...".into()))) },
                SuggestionOption { key: 'd', label: "Dismiss".into(), action: None },
            ],
        }
    }
}

impl Default for UtilityScorer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
impl UtilityScorer {
    /// Create a scorer with a pre-set last_action_time for testing.
    pub fn new_with_expired_cooldown() -> Self {
        let mut s = Self::new();
        // Set last_action_time far enough in the past that cooldown is expired.
        s.last_action_time = Some(Instant::now() - std::time::Duration::from_secs(60));
        s
    }
}
