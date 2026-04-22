//! Error-to-agent suggestion pipeline.
//!
//! When a command fails, Phantom analyzes the output and offers to spawn an AI
//! agent to diagnose or fix the problem. This module bridges
//! [`phantom_semantic`]'s error detection with the agent runtime.

use phantom_semantic::{ContentType, DetectedError, ErrorHighlighter, ParsedOutput};

use crate::agent::AgentTask;

// ---------------------------------------------------------------------------
// Suggestion types
// ---------------------------------------------------------------------------

/// A suggestion to spawn an agent, shown to the user after an error.
#[derive(Debug, Clone)]
pub struct AgentSuggestion {
    /// Prompt line displayed in the terminal, e.g.
    /// `"[PHANTOM]: Build failed. 2 errors. Fix it?"`.
    pub prompt_text: String,
    /// Available responses the user can pick.
    pub options: Vec<SuggestionOption>,
    /// Pre-built task ready to hand to `AgentManager::spawn`.
    pub task: AgentTask,
    /// Visual severity (drives accent color in the renderer).
    pub severity: SuggestionSeverity,
}

/// One option in the suggestion prompt.
#[derive(Debug, Clone)]
pub struct SuggestionOption {
    /// Hotkey character, e.g. `'Y'`.
    pub key: char,
    /// Human-readable label, e.g. `"Apply fix"`.
    pub label: String,
    /// What happens when the user picks this option.
    pub action: SuggestionAction,
}

/// Action triggered by selecting a suggestion option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SuggestionAction {
    /// Create an agent with the pre-built task.
    SpawnAgent,
    /// Ask the agent to explain the error without modifying files.
    Explain,
    /// Ignore the suggestion.
    Dismiss,
}

/// Accent severity (maps to color in the renderer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionSeverity {
    /// Red accent -- compilation or runtime errors.
    Error,
    /// Yellow accent -- warnings, non-zero exit with minor issues.
    Warning,
}

// ---------------------------------------------------------------------------
// Colors
// ---------------------------------------------------------------------------

/// Bright white for the prompt line.
const COLOR_PROMPT: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
/// Dim green for the options bar.
const COLOR_OPTIONS: [f32; 4] = [0.0, 0.85, 0.4, 0.7];

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Analyze command output and generate a suggestion if appropriate.
///
/// Returns `None` when:
/// - The command succeeded without errors.
/// - The output contains only warnings (no compiler/runtime errors).
/// - `ErrorHighlighter` decides a fix suggestion is not warranted.
pub fn suggest(parsed: &ParsedOutput, working_dir: &str) -> Option<AgentSuggestion> {
    if !ErrorHighlighter::should_suggest_fix(parsed) {
        return None;
    }

    let summary = ErrorHighlighter::error_summary(parsed)?;

    let task = build_task(parsed, working_dir);

    let severity = if parsed.errors.is_empty() {
        SuggestionSeverity::Warning
    } else {
        SuggestionSeverity::Error
    };

    Some(AgentSuggestion {
        prompt_text: format!("[PHANTOM]: {summary}"),
        options: default_options(),
        task,
        severity,
    })
}

/// Format a suggestion as terminal output lines with RGBA colors.
///
/// Returns a list of `(text, color)` pairs ready for the renderer.
pub fn format_suggestion(suggestion: &AgentSuggestion) -> Vec<(String, [f32; 4])> {
    let options_line = suggestion
        .options
        .iter()
        .map(|opt| format!("[{}] {}", opt.key, opt.label))
        .collect::<Vec<_>>()
        .join("  ");

    vec![
        (suggestion.prompt_text.clone(), COLOR_PROMPT),
        (format!("  {options_line}"), COLOR_OPTIONS),
    ]
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Build the default set of suggestion options.
fn default_options() -> Vec<SuggestionOption> {
    vec![
        SuggestionOption {
            key: 'Y',
            label: "Apply fix".into(),
            action: SuggestionAction::SpawnAgent,
        },
        SuggestionOption {
            key: 'N',
            label: "Explain only".into(),
            action: SuggestionAction::Explain,
        },
        SuggestionOption {
            key: 'D',
            label: "Dismiss".into(),
            action: SuggestionAction::Dismiss,
        },
    ]
}

/// Build an [`AgentTask`] from parsed error output.
///
/// Strategy:
/// 1. If there are structured errors, pick the first one with file info.
/// 2. Fall back to the first error without file info.
/// 3. For test failures, build a context from the failure names.
/// 4. Otherwise use the raw summary as the error description.
fn build_task(parsed: &ParsedOutput, working_dir: &str) -> AgentTask {
    // Test failure path -- specific task shape.
    if let ContentType::TestResults(ref summary) = parsed.content_type {
        if summary.failed > 0 {
            let failure_list = if summary.failures.is_empty() {
                format!("{} test(s) failed", summary.failed)
            } else {
                summary.failures.join(", ")
            };
            return AgentTask::FixError {
                error_summary: format!(
                    "Test failures: {failure_list} ({} of {} tests)",
                    summary.failed, summary.total
                ),
                file: None,
                context: build_context(parsed, working_dir),
            };
        }
    }

    // Find the best structured error (prefer ones with file info).
    let primary_error = parsed
        .errors
        .iter()
        .find(|e| e.file.is_some())
        .or_else(|| parsed.errors.first());

    match primary_error {
        Some(err) => AgentTask::FixError {
            error_summary: build_error_summary(err),
            file: err.file.clone(),
            context: build_context(parsed, working_dir),
        },
        None => {
            // No structured errors -- fall back to the raw summary.
            let summary = ErrorHighlighter::error_summary(parsed)
                .unwrap_or_else(|| "Unknown error".into());
            AgentTask::FixError {
                error_summary: summary,
                file: None,
                context: build_context(parsed, working_dir),
            }
        }
    }
}

/// Build a single-line error description from a [`DetectedError`].
fn build_error_summary(err: &DetectedError) -> String {
    let mut parts = Vec::new();

    if let Some(code) = &err.code {
        parts.push(format!("[{code}]"));
    }
    parts.push(err.message.clone());

    if let Some(file) = &err.file {
        let loc = match (err.line, err.column) {
            (Some(l), Some(c)) => format!(" at {file}:{l}:{c}"),
            (Some(l), None) => format!(" at {file}:{l}"),
            _ => format!(" in {file}"),
        };
        parts.push(loc);
    }

    parts.join(" ")
}

/// Build context string including command, working directory, and error details.
fn build_context(parsed: &ParsedOutput, working_dir: &str) -> String {
    let mut ctx = format!("Command: `{}`\nDirectory: {working_dir}", parsed.command);

    let error_count = parsed.errors.len();
    let warning_count = parsed.warnings.len();

    if error_count > 0 || warning_count > 0 {
        ctx.push_str(&format!(
            "\nDiagnostics: {error_count} error(s), {warning_count} warning(s)"
        ));
    }

    // Include up to 3 error messages for additional context.
    for (i, err) in parsed.errors.iter().take(3).enumerate() {
        ctx.push_str(&format!("\n  [{}]: {}", i + 1, err.raw_line));
    }

    // Append compiler suggestion if the primary error has one.
    if let Some(err) = parsed.errors.first() {
        if let Some(suggestion) = &err.suggestion {
            ctx.push_str(&format!("\nCompiler suggests: {suggestion}"));
        }
    }

    ctx
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_semantic::{
        CargoCommand, CommandType, ContentType, DetectedError, ErrorType, ParsedOutput,
        SemanticParser, Severity, TestSummary,
    };

    // -----------------------------------------------------------------------
    // Helper: build a minimal ParsedOutput by hand
    // -----------------------------------------------------------------------

    fn make_parsed(
        command: &str,
        command_type: CommandType,
        exit_code: Option<i32>,
        content_type: ContentType,
        errors: Vec<DetectedError>,
        warnings: Vec<DetectedError>,
        raw_output: &str,
    ) -> ParsedOutput {
        ParsedOutput {
            command: command.to_string(),
            command_type,
            exit_code,
            content_type,
            errors,
            warnings,
            duration_ms: None,
            raw_output: raw_output.to_string(),
        }
    }

    fn compiler_error(msg: &str, code: Option<&str>, file: Option<&str>, line: Option<usize>, col: Option<usize>) -> DetectedError {
        DetectedError {
            message: msg.into(),
            error_type: ErrorType::Compiler,
            file: file.map(Into::into),
            line,
            column: col,
            code: code.map(Into::into),
            severity: Severity::Error,
            raw_line: format!("error{}: {msg}", code.map(|c| format!("[{c}]")).unwrap_or_default()),
            suggestion: None,
        }
    }

    // -----------------------------------------------------------------------
    // suggest() -- compiler errors
    // -----------------------------------------------------------------------

    #[test]
    fn suggest_on_single_compiler_error() {
        let stderr = "\
error[E0308]: mismatched types
  --> src/main.rs:10:5
   |
10 |     x + \"hello\"
   |     ^^^^^^^^^^^ expected `i32`, found `&str`

error: aborting due to 1 previous error
";
        let parsed = SemanticParser::parse("cargo build", "", stderr, Some(101));
        let suggestion = suggest(&parsed, "/tmp/project");

        assert!(suggestion.is_some());
        let s = suggestion.unwrap();
        assert!(s.prompt_text.starts_with("[PHANTOM]:"));
        assert!(s.prompt_text.contains("error"));
        assert_eq!(s.severity, SuggestionSeverity::Error);
        assert_eq!(s.options.len(), 3);

        // Task should reference the file.
        match &s.task {
            AgentTask::FixError { file, error_summary, .. } => {
                assert_eq!(file.as_deref(), Some("src/main.rs"));
                assert!(error_summary.contains("mismatched types"));
            }
            other => panic!("expected FixError, got {other:?}"),
        }
    }

    #[test]
    fn suggest_on_multiple_compiler_errors() {
        let parsed = make_parsed(
            "cargo build",
            CommandType::Cargo(CargoCommand::Build),
            Some(101),
            ContentType::CompilerOutput,
            vec![
                compiler_error("mismatched types", Some("E0308"), Some("src/main.rs"), Some(10), Some(5)),
                compiler_error("cannot find value `foo`", Some("E0425"), Some("src/lib.rs"), Some(20), None),
            ],
            vec![],
            "error[E0308]: mismatched types\nerror[E0425]: cannot find value `foo`\n",
        );

        let suggestion = suggest(&parsed, "/tmp/project");
        assert!(suggestion.is_some());
        let s = suggestion.unwrap();
        assert!(s.prompt_text.contains("2 errors"));
        assert_eq!(s.severity, SuggestionSeverity::Error);

        // Primary error should be the first one with file info.
        match &s.task {
            AgentTask::FixError { file, .. } => {
                assert_eq!(file.as_deref(), Some("src/main.rs"));
            }
            other => panic!("expected FixError, got {other:?}"),
        }
    }

    #[test]
    fn suggest_on_syntax_error() {
        let parsed = make_parsed(
            "cargo check",
            CommandType::Cargo(CargoCommand::Check),
            Some(101),
            ContentType::CompilerOutput,
            vec![DetectedError {
                message: "expected `;`".into(),
                error_type: ErrorType::Syntax,
                file: Some("src/lib.rs".into()),
                line: Some(15),
                column: Some(1),
                code: None,
                severity: Severity::Error,
                raw_line: "error: expected `;`".into(),
                suggestion: Some("add `;` here".into()),
            }],
            vec![],
            "error: expected `;`\n",
        );

        let suggestion = suggest(&parsed, "/tmp/project");
        assert!(suggestion.is_some());
        let s = suggestion.unwrap();
        assert_eq!(s.severity, SuggestionSeverity::Error);

        // Context should include the compiler suggestion.
        match &s.task {
            AgentTask::FixError { context, .. } => {
                assert!(context.contains("add `;` here"), "got: {context}");
            }
            other => panic!("expected FixError, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // suggest() -- test failures
    // -----------------------------------------------------------------------

    #[test]
    fn suggest_on_test_failure() {
        let stdout = "\
running 5 tests
test tests::a ... ok
test tests::b ... ok
test tests::c ... FAILED
test tests::d ... FAILED
test tests::e ... ok

test result: FAILED. 3 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out
";
        let parsed = SemanticParser::parse("cargo test", stdout, "", Some(101));
        let suggestion = suggest(&parsed, "/tmp/project");

        assert!(suggestion.is_some());
        let s = suggestion.unwrap();
        assert!(s.prompt_text.contains("Test failed"));

        match &s.task {
            AgentTask::FixError { error_summary, .. } => {
                assert!(error_summary.contains("Test failures"), "got: {error_summary}");
                assert!(error_summary.contains("2 of 5"), "got: {error_summary}");
            }
            other => panic!("expected FixError, got {other:?}"),
        }
    }

    #[test]
    fn suggest_test_failure_includes_failure_names() {
        let parsed = make_parsed(
            "cargo test",
            CommandType::Cargo(CargoCommand::Test),
            Some(101),
            ContentType::TestResults(TestSummary {
                passed: 3,
                failed: 2,
                ignored: 0,
                total: 5,
                failures: vec!["tests::broken_a".into(), "tests::broken_b".into()],
            }),
            vec![],
            vec![],
            "",
        );

        let suggestion = suggest(&parsed, "/tmp/project");
        assert!(suggestion.is_some());
        let s = suggestion.unwrap();

        match &s.task {
            AgentTask::FixError { error_summary, .. } => {
                assert!(error_summary.contains("tests::broken_a"), "got: {error_summary}");
                assert!(error_summary.contains("tests::broken_b"), "got: {error_summary}");
            }
            other => panic!("expected FixError, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // suggest() -- no suggestion on success
    // -----------------------------------------------------------------------

    #[test]
    fn no_suggest_on_clean_build() {
        let parsed = SemanticParser::parse(
            "cargo build",
            "",
            "    Compiling foo v0.1.0\n    Finished dev\n",
            Some(0),
        );
        assert!(suggest(&parsed, "/tmp/project").is_none());
    }

    #[test]
    fn no_suggest_on_passing_tests() {
        let stdout = "\
running 3 tests
test tests::a ... ok
test tests::b ... ok
test tests::c ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
";
        let parsed = SemanticParser::parse("cargo test", stdout, "", Some(0));
        assert!(suggest(&parsed, "/tmp/project").is_none());
    }

    #[test]
    fn no_suggest_on_warnings_only() {
        let stderr = "\
warning: unused variable: `x`
  --> src/lib.rs:5:9
   |
5  |     let x = 42;
   |         ^ help: prefix with underscore: `_x`
";
        let parsed = SemanticParser::parse("cargo check", "", stderr, Some(0));
        assert!(suggest(&parsed, "/tmp/project").is_none());
    }

    // -----------------------------------------------------------------------
    // build_task -- detailed task construction
    // -----------------------------------------------------------------------

    #[test]
    fn build_task_prefers_error_with_file() {
        let parsed = make_parsed(
            "cargo build",
            CommandType::Cargo(CargoCommand::Build),
            Some(101),
            ContentType::CompilerOutput,
            vec![
                compiler_error("aborting due to previous error", None, None, None, None),
                compiler_error("type mismatch", Some("E0308"), Some("src/foo.rs"), Some(42), Some(7)),
            ],
            vec![],
            "",
        );

        let task = build_task(&parsed, "/tmp/project");
        match task {
            AgentTask::FixError { file, error_summary, .. } => {
                assert_eq!(file.as_deref(), Some("src/foo.rs"));
                assert!(error_summary.contains("type mismatch"));
                assert!(error_summary.contains("E0308"));
            }
            other => panic!("expected FixError, got {other:?}"),
        }
    }

    #[test]
    fn build_task_falls_back_to_first_error_without_file() {
        let parsed = make_parsed(
            "cargo build",
            CommandType::Cargo(CargoCommand::Build),
            Some(101),
            ContentType::CompilerOutput,
            vec![compiler_error("linker `cc` not found", None, None, None, None)],
            vec![],
            "error: linker `cc` not found\n",
        );

        let task = build_task(&parsed, "/tmp/project");
        match task {
            AgentTask::FixError { file, error_summary, .. } => {
                assert!(file.is_none());
                assert!(error_summary.contains("linker"));
            }
            other => panic!("expected FixError, got {other:?}"),
        }
    }

    #[test]
    fn build_task_context_includes_command_and_dir() {
        let parsed = make_parsed(
            "cargo build --release",
            CommandType::Cargo(CargoCommand::Build),
            Some(101),
            ContentType::CompilerOutput,
            vec![compiler_error("boom", None, None, None, None)],
            vec![],
            "error: boom\n",
        );

        let task = build_task(&parsed, "/home/user/project");
        match task {
            AgentTask::FixError { context, .. } => {
                assert!(context.contains("cargo build --release"), "got: {context}");
                assert!(context.contains("/home/user/project"), "got: {context}");
            }
            other => panic!("expected FixError, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // format_suggestion
    // -----------------------------------------------------------------------

    #[test]
    fn format_suggestion_produces_two_lines() {
        let suggestion = AgentSuggestion {
            prompt_text: "[PHANTOM]: Build failed. 1 error.".into(),
            options: default_options(),
            task: AgentTask::FixError {
                error_summary: "test".into(),
                file: None,
                context: "test".into(),
            },
            severity: SuggestionSeverity::Error,
        };

        let lines = format_suggestion(&suggestion);
        assert_eq!(lines.len(), 2);

        // First line is the prompt in bright white.
        assert!(lines[0].0.contains("[PHANTOM]"));
        assert_eq!(lines[0].1, COLOR_PROMPT);

        // Second line has all option keys.
        assert!(lines[1].0.contains("[Y]"));
        assert!(lines[1].0.contains("[N]"));
        assert!(lines[1].0.contains("[D]"));
        assert_eq!(lines[1].1, COLOR_OPTIONS);
    }

    #[test]
    fn format_suggestion_preserves_prompt_text() {
        let custom_text = "[PHANTOM]: HTTP 500 Internal Server Error";
        let suggestion = AgentSuggestion {
            prompt_text: custom_text.into(),
            options: vec![SuggestionOption {
                key: 'R',
                label: "Retry".into(),
                action: SuggestionAction::SpawnAgent,
            }],
            task: AgentTask::FixError {
                error_summary: "http error".into(),
                file: None,
                context: "curl".into(),
            },
            severity: SuggestionSeverity::Error,
        };

        let lines = format_suggestion(&suggestion);
        assert_eq!(lines[0].0, custom_text);
        assert!(lines[1].0.contains("[R] Retry"));
    }

    // -----------------------------------------------------------------------
    // severity classification
    // -----------------------------------------------------------------------

    #[test]
    fn severity_is_error_when_errors_present() {
        let parsed = make_parsed(
            "cargo build",
            CommandType::Cargo(CargoCommand::Build),
            Some(101),
            ContentType::CompilerOutput,
            vec![compiler_error("boom", None, Some("src/lib.rs"), Some(1), None)],
            vec![],
            "error: boom\n",
        );

        let suggestion = suggest(&parsed, "/tmp").unwrap();
        assert_eq!(suggestion.severity, SuggestionSeverity::Error);
    }

    #[test]
    fn severity_is_warning_when_only_test_failures_no_errors() {
        // Test failures with no DetectedError entries in `errors` vec --
        // severity falls to Warning since errors vec is empty.
        let parsed = make_parsed(
            "cargo test",
            CommandType::Cargo(CargoCommand::Test),
            Some(101),
            ContentType::TestResults(TestSummary {
                passed: 2,
                failed: 1,
                ignored: 0,
                total: 3,
                failures: vec!["tests::broken".into()],
            }),
            vec![], // no structured errors
            vec![],
            "",
        );

        let suggestion = suggest(&parsed, "/tmp");
        assert!(suggestion.is_some());
        let s = suggestion.unwrap();
        assert_eq!(s.severity, SuggestionSeverity::Warning);
    }

    // -----------------------------------------------------------------------
    // default_options
    // -----------------------------------------------------------------------

    #[test]
    fn default_options_has_three_choices() {
        let opts = default_options();
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[0].key, 'Y');
        assert_eq!(opts[0].action, SuggestionAction::SpawnAgent);
        assert_eq!(opts[1].key, 'N');
        assert_eq!(opts[1].action, SuggestionAction::Explain);
        assert_eq!(opts[2].key, 'D');
        assert_eq!(opts[2].action, SuggestionAction::Dismiss);
    }

    // -----------------------------------------------------------------------
    // build_error_summary formatting
    // -----------------------------------------------------------------------

    #[test]
    fn build_error_summary_includes_code_and_location() {
        let err = DetectedError {
            message: "mismatched types".into(),
            error_type: ErrorType::Compiler,
            file: Some("src/main.rs".into()),
            line: Some(10),
            column: Some(5),
            code: Some("E0308".into()),
            severity: Severity::Error,
            raw_line: "error[E0308]: mismatched types".into(),
            suggestion: None,
        };

        let summary = build_error_summary(&err);
        assert!(summary.contains("[E0308]"), "got: {summary}");
        assert!(summary.contains("mismatched types"), "got: {summary}");
        assert!(summary.contains("src/main.rs:10:5"), "got: {summary}");
    }

    #[test]
    fn build_error_summary_without_code_or_column() {
        let err = DetectedError {
            message: "linker error".into(),
            error_type: ErrorType::Compiler,
            file: Some("build.rs".into()),
            line: Some(3),
            column: None,
            code: None,
            severity: Severity::Error,
            raw_line: "error: linker error".into(),
            suggestion: None,
        };

        let summary = build_error_summary(&err);
        assert!(!summary.contains('['), "got: {summary}");
        assert!(summary.contains("build.rs:3"), "got: {summary}");
        // Should NOT contain a spurious colon after the line number.
        assert!(!summary.contains("build.rs:3:"), "got: {summary}");
    }

    // -----------------------------------------------------------------------
    // runtime error suggestions
    // -----------------------------------------------------------------------

    #[test]
    fn suggest_on_runtime_error() {
        let parsed = make_parsed(
            "cargo run",
            CommandType::Cargo(CargoCommand::Run),
            Some(101),
            ContentType::PlainText,
            vec![DetectedError {
                message: "index out of bounds".into(),
                error_type: ErrorType::Runtime,
                file: Some("src/main.rs".into()),
                line: Some(55),
                column: None,
                code: None,
                severity: Severity::Error,
                raw_line: "thread 'main' panicked at 'index out of bounds'".into(),
                suggestion: None,
            }],
            vec![],
            "thread 'main' panicked at 'index out of bounds'\n",
        );

        let suggestion = suggest(&parsed, "/tmp/project");
        assert!(suggestion.is_some());
        let s = suggestion.unwrap();
        assert_eq!(s.severity, SuggestionSeverity::Error);
        match &s.task {
            AgentTask::FixError { error_summary, file, .. } => {
                assert!(error_summary.contains("index out of bounds"));
                assert_eq!(file.as_deref(), Some("src/main.rs"));
            }
            other => panic!("expected FixError, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // context truncation (max 3 errors in context)
    // -----------------------------------------------------------------------

    #[test]
    fn context_caps_at_three_errors() {
        let errors: Vec<DetectedError> = (0..5)
            .map(|i| compiler_error(&format!("error_{i}"), None, None, None, None))
            .collect();

        let parsed = make_parsed(
            "cargo build",
            CommandType::Cargo(CargoCommand::Build),
            Some(101),
            ContentType::CompilerOutput,
            errors,
            vec![],
            "errors\n",
        );

        let task = build_task(&parsed, "/tmp");
        match task {
            AgentTask::FixError { context, .. } => {
                // Should contain errors 0..3 but not 3 or 4.
                assert!(context.contains("error_0"), "got: {context}");
                assert!(context.contains("error_2"), "got: {context}");
                assert!(!context.contains("error_3"), "got: {context}");
                assert!(!context.contains("error_4"), "got: {context}");
            }
            other => panic!("expected FixError, got {other:?}"),
        }
    }
}
