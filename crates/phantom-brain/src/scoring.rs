//! Utility AI scoring engine.
//!
//! Each scorer examines context and returns a [`ScoredAction`] with a 0.0–1.0
//! score. The brain calls [`UtilityScorer::evaluate`] on every event, which
//! runs all scorers and returns the highest-scoring action. If nothing beats
//! the quiet baseline, the brain stays silent.
//!
//! Inspired by game AI utility systems — see `docs/research/ai-control-loop.md`.

use phantom_context::ProjectContext;
use phantom_memory::MemoryStore;
use phantom_semantic::ParsedOutput;

use crate::events::{AiAction, AiEvent};

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
}

impl UtilityScorer {
    /// Create a scorer with default (silent) initial state.
    pub fn new() -> Self {
        Self {
            idle_time: 0.0,
            last_had_errors: false,
            suggestions_since_input: 0,
            chattiness: 0.0,
        }
    }

    // -----------------------------------------------------------------------
    // Individual scorers
    // -----------------------------------------------------------------------

    /// Score "suggest fixing an error" action.
    ///
    /// - **0.9** if errors just appeared and no fix is in progress.
    /// - **0.3** if errors are old (idle > 30s since the error).
    /// - **0.0** if no errors or the user is actively typing (idle < 2s).
    pub fn fix_score(&self, parsed: &ParsedOutput, _context: &ProjectContext) -> ScoredAction {
        let has_errors = !parsed.errors.is_empty();

        if !has_errors {
            return ScoredAction {
                action: AiAction::DoNothing,
                score: 0.0,
                reason: "no errors to fix".into(),
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
                    options: vec![('y', "Yes, explain".into()), ('n', "No thanks".into())],
                },
                score: 0.7,
                reason: "user idle after error — may be stuck".into(),
            };
        }

        if idle_time > 30.0 {
            return ScoredAction {
                action: AiAction::ShowSuggestion {
                    text: "Need help with anything?".into(),
                    options: vec![('y', "Yes".into()), ('n', "No".into())],
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
    /// - **0.5** if a long-running process is likely (deploy, CI commands).
    /// - **0.0** otherwise.
    pub fn watcher_score(&self, context: &ProjectContext) -> ScoredAction {
        // Heuristic: if the project has CI/deploy-related context, offer a watcher.
        let has_ci = context.commands.build.is_some() || context.commands.test.is_some();

        if has_ci {
            ScoredAction {
                action: AiAction::ShowSuggestion {
                    text: "Want me to watch this build?".into(),
                    options: vec![('y', "Watch it".into()), ('n', "No".into())],
                },
                score: 0.5,
                reason: "project has build commands, watcher may help".into(),
            }
        } else {
            ScoredAction {
                action: AiAction::DoNothing,
                score: 0.0,
                reason: "no long-running process detected".into(),
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
        let score = (0.5 + self.chattiness).min(1.0);
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
        let best = candidates
            .into_iter()
            .max_by(|a, b| a.score.partial_cmp(&b.score).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or_else(|| self.quiet_score());

        // Update chattiness if we're going to act (non-quiet).
        if !matches!(best.action, AiAction::DoNothing) && best.score > 0.0 {
            self.chattiness = (self.chattiness + 0.1).min(1.0);
            self.suggestions_since_input += 1;
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
                ('f', "Fix it".into()),
                ('e', "Explain".into()),
                ('d', "Dismiss".into()),
            ],
        }
    }
}

impl Default for UtilityScorer {
    fn default() -> Self {
        Self::new()
    }
}
