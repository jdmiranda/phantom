//! Phantom AI Brain — the ambient intelligence thread.
//!
//! This crate implements the AI brain for Phantom, an event-driven OODA loop
//! that observes terminal activity, orients using project context and memory,
//! decides using utility AI scoring, and acts by emitting suggestions, spawning
//! agents, or updating memory.
//!
//! # Architecture
//!
//! The brain runs on a dedicated OS thread (`phantom-brain`), sleeping on a
//! channel until an [`AiEvent`] arrives. It processes every event through
//! the scoring engine and emits [`AiAction`]s only when they beat the quiet
//! baseline.
//!
//! ```text
//! ┌─────────────┐   AiEvent    ┌───────────┐   AiAction   ┌──────────┐
//! │ Terminal I/O │ ──────────▶  │ AI Brain  │ ──────────▶  │ Renderer │
//! │ File Watch   │              │ (OODA)    │              │ / App    │
//! │ Timers       │              └───────────┘              └──────────┘
//! └─────────────┘
//! ```
//!
//! # Usage
//!
//! ```rust,ignore
//! use phantom_brain::{spawn_brain, BrainConfig, AiEvent};
//!
//! let handle = spawn_brain(BrainConfig::default());
//!
//! // Send events from any thread
//! handle.send_event(AiEvent::UserIdle { seconds: 15.0 }).ok();
//!
//! // Poll for actions in the render loop
//! if let Some(action) = handle.try_recv_action() {
//!     // apply action to UI
//! }
//!
//! // Shutdown
//! handle.send_event(AiEvent::Shutdown).ok();
//! ```

pub mod brain;
pub mod events;
pub mod ollama;
pub mod router;
pub mod scoring;

pub use brain::*;
pub use events::*;
pub use router::*;
pub use scoring::*;

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_context::{
        Framework, GitInfo, PackageManager, ProjectCommands, ProjectContext, ProjectType,
    };
    use phantom_memory::MemoryStore;
    use phantom_semantic::{
        CommandType, ContentType, DetectedError, ErrorType, ParsedOutput, Severity,
    };

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Build a minimal ProjectContext for testing (no filesystem access).
    fn test_context() -> ProjectContext {
        ProjectContext {
            root: "/tmp/test-project".into(),
            name: "test-project".into(),
            project_type: ProjectType::Rust,
            package_manager: PackageManager::Cargo,
            framework: Framework::None,
            commands: ProjectCommands {
                build: Some("cargo build".into()),
                test: Some("cargo test".into()),
                run: Some("cargo run".into()),
                lint: None,
                format: None,
            },
            git: Some(GitInfo {
                branch: "main".into(),
                remote_url: None,
                is_dirty: false,
                ahead: 0,
                behind: 0,
                last_commit_message: None,
                last_commit_age: None,
            }),
            rust_version: None,
            node_version: None,
            python_version: None,
        }
    }

    /// Build a MemoryStore backed by a temp directory.
    fn test_memory() -> (MemoryStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::open_in("/tmp/test-project", dir.path()).unwrap();
        (store, dir)
    }

    /// Build a ParsedOutput with errors.
    fn parsed_with_errors() -> ParsedOutput {
        ParsedOutput {
            command: "cargo build".into(),
            command_type: CommandType::Cargo(phantom_semantic::CargoCommand::Build),
            exit_code: Some(101),
            content_type: ContentType::CompilerOutput,
            errors: vec![DetectedError {
                message: "mismatched types".into(),
                error_type: ErrorType::Compiler,
                file: Some("src/main.rs".into()),
                line: Some(10),
                column: Some(5),
                code: Some("E0308".into()),
                severity: Severity::Error,
                raw_line: "error[E0308]: mismatched types".into(),
                suggestion: Some("try using .to_string()".into()),
            }],
            warnings: vec![],
            duration_ms: Some(1500),
            raw_output: "error[E0308]: mismatched types".into(),
        }
    }

    /// Build a ParsedOutput with no errors.
    fn parsed_success() -> ParsedOutput {
        ParsedOutput {
            command: "cargo build".into(),
            command_type: CommandType::Cargo(phantom_semantic::CargoCommand::Build),
            exit_code: Some(0),
            content_type: ContentType::PlainText,
            errors: vec![],
            warnings: vec![],
            duration_ms: Some(800),
            raw_output: "Compiling phantom v0.1.0\n    Finished".into(),
        }
    }

    // =======================================================================
    // 1. fix_score: high when errors are fresh
    // =======================================================================

    #[test]
    fn fix_score_high_on_fresh_errors() {
        let mut scorer = UtilityScorer::new();
        scorer.idle_time = 5.0; // idle enough to not be "actively typing"
        let ctx = test_context();
        let parsed = parsed_with_errors();

        let scored = scorer.fix_score(&parsed, &ctx);
        assert!(
            scored.score >= 0.9,
            "expected >= 0.9, got {}",
            scored.score
        );
    }

    // =======================================================================
    // 2. fix_score: low when errors are old
    // =======================================================================

    #[test]
    fn fix_score_low_on_stale_errors() {
        let mut scorer = UtilityScorer::new();
        scorer.idle_time = 60.0; // errors are old
        let ctx = test_context();
        let parsed = parsed_with_errors();

        let scored = scorer.fix_score(&parsed, &ctx);
        assert!(
            (scored.score - 0.3).abs() < f32::EPSILON,
            "expected 0.3, got {}",
            scored.score
        );
    }

    // =======================================================================
    // 3. fix_score: zero when no errors
    // =======================================================================

    #[test]
    fn fix_score_zero_on_success() {
        let scorer = UtilityScorer::new();
        let ctx = test_context();
        let parsed = parsed_success();

        let scored = scorer.fix_score(&parsed, &ctx);
        assert!(
            scored.score == 0.0,
            "expected 0.0, got {}",
            scored.score
        );
    }

    // =======================================================================
    // 4. fix_score: zero when user is actively typing
    // =======================================================================

    #[test]
    fn fix_score_zero_when_user_typing() {
        let mut scorer = UtilityScorer::new();
        scorer.idle_time = 0.5; // user just typed
        let ctx = test_context();
        let parsed = parsed_with_errors();

        let scored = scorer.fix_score(&parsed, &ctx);
        assert!(
            scored.score == 0.0,
            "expected 0.0 when user is typing, got {}",
            scored.score
        );
    }

    // =======================================================================
    // 5. explain_score: high when idle after error
    // =======================================================================

    #[test]
    fn explain_score_high_on_idle_after_error() {
        let scorer = UtilityScorer::new();
        let parsed = parsed_with_errors();

        let scored = scorer.explain_score(&parsed, 15.0);
        assert!(
            (scored.score - 0.7).abs() < f32::EPSILON,
            "expected 0.7, got {}",
            scored.score
        );
    }

    // =======================================================================
    // 6. explain_score: medium when idle without error
    // =======================================================================

    #[test]
    fn explain_score_medium_on_long_idle_no_error() {
        let scorer = UtilityScorer::new();
        let parsed = parsed_success();

        let scored = scorer.explain_score(&parsed, 45.0);
        assert!(
            (scored.score - 0.3).abs() < f32::EPSILON,
            "expected 0.3, got {}",
            scored.score
        );
    }

    // =======================================================================
    // 7. explain_score: zero when user is active
    // =======================================================================

    #[test]
    fn explain_score_zero_when_active() {
        let scorer = UtilityScorer::new();
        let parsed = parsed_with_errors();

        let scored = scorer.explain_score(&parsed, 2.0);
        assert!(
            scored.score == 0.0,
            "expected 0.0, got {}",
            scored.score
        );
    }

    // =======================================================================
    // 8. memory_score: high on new pattern
    // =======================================================================

    #[test]
    fn memory_score_high_on_new_pattern() {
        let (memory, _dir) = test_memory();
        let scorer = UtilityScorer::new();
        let parsed = parsed_success();
        let event = AiEvent::CommandComplete(parsed);

        let scored = scorer.memory_score(&event, &memory);
        assert!(
            (scored.score - 0.6).abs() < f32::EPSILON,
            "expected 0.6 for new pattern, got {}",
            scored.score
        );
    }

    // =======================================================================
    // 9. memory_score: low on known pattern
    // =======================================================================

    #[test]
    fn memory_score_low_on_known_pattern() {
        let (mut memory, _dir) = test_memory();
        // Pre-populate memory with the pattern.
        memory
            .set(
                "cmd:cargo",
                "seen command: cargo build",
                phantom_memory::MemoryCategory::Context,
                phantom_memory::MemorySource::Auto,
            )
            .unwrap();

        let scorer = UtilityScorer::new();
        let parsed = parsed_success();
        let event = AiEvent::CommandComplete(parsed);

        let scored = scorer.memory_score(&event, &memory);
        assert!(
            (scored.score - 0.1).abs() < f32::EPSILON,
            "expected 0.1 for known pattern, got {}",
            scored.score
        );
    }

    // =======================================================================
    // 10. watcher_score: 0.5 when project has build commands
    // =======================================================================

    #[test]
    fn watcher_score_positive_with_build_commands() {
        let scorer = UtilityScorer::new();
        let ctx = test_context();

        let scored = scorer.watcher_score(&ctx);
        assert!(
            (scored.score - 0.5).abs() < f32::EPSILON,
            "expected 0.5, got {}",
            scored.score
        );
    }

    // =======================================================================
    // 11. watcher_score: zero when no build commands
    // =======================================================================

    #[test]
    fn watcher_score_zero_without_build_commands() {
        let scorer = UtilityScorer::new();
        let mut ctx = test_context();
        ctx.commands.build = None;
        ctx.commands.test = None;

        let scored = scorer.watcher_score(&ctx);
        assert!(
            scored.score == 0.0,
            "expected 0.0, got {}",
            scored.score
        );
    }

    // =======================================================================
    // 12. quiet_score: baseline at 0.5 with zero chattiness
    // =======================================================================

    #[test]
    fn quiet_score_baseline() {
        let scorer = UtilityScorer::new();
        let scored = scorer.quiet_score();
        assert!(
            (scored.score - 0.5).abs() < f32::EPSILON,
            "expected 0.5, got {}",
            scored.score
        );
    }

    // =======================================================================
    // 13. quiet_score: increases with chattiness
    // =======================================================================

    #[test]
    fn quiet_score_increases_with_chattiness() {
        let mut scorer = UtilityScorer::new();
        scorer.chattiness = 0.3;

        let scored = scorer.quiet_score();
        assert!(
            (scored.score - 0.8).abs() < f32::EPSILON,
            "expected 0.8, got {}",
            scored.score
        );
    }

    // =======================================================================
    // 14. quiet_score: capped at 1.0
    // =======================================================================

    #[test]
    fn quiet_score_capped_at_one() {
        let mut scorer = UtilityScorer::new();
        scorer.chattiness = 0.9;

        let scored = scorer.quiet_score();
        assert!(
            scored.score <= 1.0,
            "expected <= 1.0, got {}",
            scored.score
        );
    }

    // =======================================================================
    // 15. chattiness: increments on action
    // =======================================================================

    #[test]
    fn chattiness_increments_on_evaluate() {
        let mut scorer = UtilityScorer::new();
        scorer.idle_time = 5.0; // idle enough for fix_score to fire
        let ctx = test_context();
        let (memory, _dir) = test_memory();

        let parsed = parsed_with_errors();
        let event = AiEvent::CommandComplete(parsed);

        let _ = scorer.evaluate(&event, &ctx, &memory);

        // Chattiness should have increased (the fix_score at 0.9 should win).
        assert!(
            scorer.chattiness > 0.0,
            "chattiness should increase after a winning action"
        );
    }

    // =======================================================================
    // 16. chattiness: decays with idle time
    // =======================================================================

    #[test]
    fn chattiness_decays_with_idle() {
        let mut scorer = UtilityScorer::new();
        scorer.chattiness = 0.5;

        scorer.decay_chattiness(4.0); // 4s * 0.05 = 0.2 decay

        let expected = 0.3;
        assert!(
            (scorer.chattiness - expected).abs() < f32::EPSILON,
            "expected {}, got {}",
            expected,
            scorer.chattiness
        );
    }

    // =======================================================================
    // 17. chattiness: does not go below zero
    // =======================================================================

    #[test]
    fn chattiness_floor_at_zero() {
        let mut scorer = UtilityScorer::new();
        scorer.chattiness = 0.1;

        scorer.decay_chattiness(100.0); // way more than needed

        assert!(
            scorer.chattiness == 0.0,
            "chattiness should not go below 0, got {}",
            scorer.chattiness
        );
    }

    // =======================================================================
    // 18. user_acted resets all counters
    // =======================================================================

    #[test]
    fn user_acted_resets_state() {
        let mut scorer = UtilityScorer::new();
        scorer.chattiness = 0.5;
        scorer.suggestions_since_input = 5;
        scorer.idle_time = 30.0;

        scorer.user_acted();

        assert_eq!(scorer.chattiness, 0.0);
        assert_eq!(scorer.suggestions_since_input, 0);
        assert_eq!(scorer.idle_time, 0.0);
    }

    // =======================================================================
    // 19. evaluate picks highest score
    // =======================================================================

    #[test]
    fn evaluate_picks_highest_score() {
        let mut scorer = UtilityScorer::new();
        scorer.idle_time = 5.0; // enough for fix_score to fire at 0.9
        let ctx = test_context();
        let (memory, _dir) = test_memory();

        let parsed = parsed_with_errors();
        let event = AiEvent::CommandComplete(parsed);

        let best = scorer.evaluate(&event, &ctx, &memory);

        // fix_score should win at 0.9 vs quiet at 0.5.
        assert!(
            best.score >= 0.8,
            "expected winning score >= 0.8, got {}",
            best.score
        );
    }

    // =======================================================================
    // 20. evaluate returns quiet on success with no triggers
    // =======================================================================

    #[test]
    fn evaluate_returns_quiet_on_success() {
        let mut scorer = UtilityScorer::new();
        let ctx = test_context();
        let (mut memory, _dir) = test_memory();
        // Pre-populate memory so memory_score is low.
        memory
            .set(
                "cmd:cargo",
                "known",
                phantom_memory::MemoryCategory::Context,
                phantom_memory::MemorySource::Auto,
            )
            .unwrap();

        let parsed = parsed_success();
        let event = AiEvent::CommandComplete(parsed);

        let best = scorer.evaluate(&event, &ctx, &memory);

        // The quiet score (0.5) should be the baseline winner.
        // The winning score should be at or near the quiet threshold.
        assert!(
            best.score <= 0.6,
            "expected quiet-ish score, got {} (reason: {})",
            best.score,
            best.reason
        );
    }

    // =======================================================================
    // 21. notification_score: high on agent complete
    // =======================================================================

    #[test]
    fn notification_score_high_on_agent_complete() {
        let scorer = UtilityScorer::new();
        let event = AiEvent::AgentComplete {
            id: 1,
            success: true,
            summary: "Fixed the bug".into(),
        };

        let scored = scorer.notification_score(&event);
        assert!(
            (scored.score - 0.8).abs() < f32::EPSILON,
            "expected 0.8, got {}",
            scored.score
        );
    }

    // =======================================================================
    // 22. notification_score: medium on git change
    // =======================================================================

    #[test]
    fn notification_score_medium_on_git_change() {
        let scorer = UtilityScorer::new();
        let event = AiEvent::GitStateChanged;

        let scored = scorer.notification_score(&event);
        assert!(
            (scored.score - 0.4).abs() < f32::EPSILON,
            "expected 0.4, got {}",
            scored.score
        );
    }

    // =======================================================================
    // 23. notification_score: medium on file change
    // =======================================================================

    #[test]
    fn notification_score_medium_on_file_change() {
        let scorer = UtilityScorer::new();
        let event = AiEvent::FileChanged("src/main.rs".into());

        let scored = scorer.notification_score(&event);
        assert!(
            (scored.score - 0.4).abs() < f32::EPSILON,
            "expected 0.4, got {}",
            scored.score
        );
    }

    // =======================================================================
    // 24. notification_score: zero on irrelevant events
    // =======================================================================

    #[test]
    fn notification_score_zero_on_irrelevant() {
        let scorer = UtilityScorer::new();
        let event = AiEvent::UserIdle { seconds: 10.0 };

        let scored = scorer.notification_score(&event);
        assert!(
            scored.score == 0.0,
            "expected 0.0, got {}",
            scored.score
        );
    }

    // =======================================================================
    // 25. BrainHandle: send and receive
    // =======================================================================

    #[test]
    fn brain_handle_send_recv() {
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let (action_tx, action_rx) = std::sync::mpsc::channel();

        let handle = BrainHandle { event_tx, action_rx };

        // Send an event.
        handle.send_event(AiEvent::Shutdown).unwrap();
        let received = event_rx.recv().unwrap();
        assert!(matches!(received, AiEvent::Shutdown));

        // Send an action from the "brain side" and receive it.
        action_tx.send(AiAction::DoNothing).unwrap();
        let action = handle.try_recv_action();
        assert!(action.is_some());
        assert!(matches!(action.unwrap(), AiAction::DoNothing));
    }

    // =======================================================================
    // 26. BrainHandle: try_recv_action returns None when empty
    // =======================================================================

    #[test]
    fn brain_handle_try_recv_none_when_empty() {
        let (event_tx, _event_rx) = std::sync::mpsc::channel();
        let (_action_tx, action_rx) = std::sync::mpsc::channel::<AiAction>();

        let handle = BrainHandle { event_tx, action_rx };

        assert!(handle.try_recv_action().is_none());
    }

    // =======================================================================
    // 27. memory_score: low on non-command events
    // =======================================================================

    #[test]
    fn memory_score_low_on_non_command_event() {
        let (memory, _dir) = test_memory();
        let scorer = UtilityScorer::new();
        let event = AiEvent::FileChanged("src/lib.rs".into());

        let scored = scorer.memory_score(&event, &memory);
        assert!(
            (scored.score - 0.1).abs() < f32::EPSILON,
            "expected 0.1, got {}",
            scored.score
        );
    }

    // =======================================================================
    // 28. evaluate: idle event with prior errors triggers explain
    // =======================================================================

    #[test]
    fn evaluate_idle_after_error_triggers_explain() {
        let mut scorer = UtilityScorer::new();
        scorer.last_had_errors = true;
        let ctx = test_context();
        let (memory, _dir) = test_memory();

        let event = AiEvent::UserIdle { seconds: 15.0 };

        let best = scorer.evaluate(&event, &ctx, &memory);

        // explain_score should fire at 0.7 which beats quiet at 0.5.
        assert!(
            best.score >= 0.6,
            "expected explain to win (>= 0.6), got {} (reason: {})",
            best.score,
            best.reason
        );
    }

    // =======================================================================
    // 29. Default trait on UtilityScorer
    // =======================================================================

    #[test]
    fn utility_scorer_default() {
        let scorer = UtilityScorer::default();
        assert_eq!(scorer.idle_time, 0.0);
        assert!(!scorer.last_had_errors);
        assert_eq!(scorer.suggestions_since_input, 0);
        assert_eq!(scorer.chattiness, 0.0);
    }

    // =======================================================================
    // 30. event_sender cloning
    // =======================================================================

    #[test]
    fn brain_handle_event_sender_clone() {
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let (_action_tx, action_rx) = std::sync::mpsc::channel::<AiAction>();

        let handle = BrainHandle { event_tx, action_rx };
        let sender_clone = handle.event_sender();

        sender_clone.send(AiEvent::GitStateChanged).unwrap();

        let received = event_rx.recv().unwrap();
        assert!(matches!(received, AiEvent::GitStateChanged));
    }
}
