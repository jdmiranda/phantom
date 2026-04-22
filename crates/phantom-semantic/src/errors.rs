use regex::Regex;

use crate::types::*;

// ---------------------------------------------------------------------------
// Highlight types
// ---------------------------------------------------------------------------

/// A highlighted region in terminal output, consumed by the render loop.
#[derive(Debug, Clone)]
pub struct HighlightRegion {
    /// 0-indexed line in the raw output.
    pub line: usize,
    /// Start column (0-indexed, inclusive).
    pub start_col: usize,
    /// End column (0-indexed, exclusive).
    pub end_col: usize,
    /// RGBA highlight color.
    pub color: [f32; 4],
    pub style: HighlightStyle,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HighlightStyle {
    /// Colored background behind text.
    Background,
    /// Colored underline beneath text.
    Underline,
    /// Box border around region.
    Border,
}

/// A clickable source reference extracted from error output.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceReference {
    pub file: String,
    pub line: usize,
    pub column: Option<usize>,
    /// Human-readable display text, e.g. `"src/main.rs:42:9"`.
    pub display_text: String,
    /// Which line of the raw output this reference appears on (0-indexed).
    pub output_line: usize,
}

// ---------------------------------------------------------------------------
// Colors
// ---------------------------------------------------------------------------

const COLOR_ERROR: [f32; 4] = [1.0, 0.2, 0.2, 0.3];
const COLOR_WARNING: [f32; 4] = [1.0, 0.8, 0.0, 0.3];
const COLOR_SOURCE_REF: [f32; 4] = [0.0, 0.8, 1.0, 1.0];

// ---------------------------------------------------------------------------
// ErrorHighlighter
// ---------------------------------------------------------------------------

/// Analyzes [`ParsedOutput`] to produce highlight regions, source references,
/// error summaries, and fix suggestions.
pub struct ErrorHighlighter;

impl ErrorHighlighter {
    /// From a [`ParsedOutput`], produce [`HighlightRegion`]s for the renderer.
    ///
    /// For each error the raw_line is highlighted with a red background.
    /// For each warning the raw_line is highlighted with a yellow background.
    /// File:line references are highlighted with a cyan underline.
    pub fn highlight(parsed: &ParsedOutput) -> Vec<HighlightRegion> {
        let output_lines: Vec<&str> = parsed.raw_output.lines().collect();
        let mut regions = Vec::new();

        // Error highlights (red background).
        for err in &parsed.errors {
            Self::highlight_raw_line(&output_lines, &err.raw_line, COLOR_ERROR, &mut regions);
        }

        // Warning highlights (yellow background).
        for warn in &parsed.warnings {
            Self::highlight_raw_line(&output_lines, &warn.raw_line, COLOR_WARNING, &mut regions);
        }

        // Source reference highlights (cyan underline).
        let refs = Self::extract_references(parsed);
        for src_ref in &refs {
            if let Some(line_text) = output_lines.get(src_ref.output_line) {
                if let Some(start) = line_text.find(&src_ref.display_text) {
                    regions.push(HighlightRegion {
                        line: src_ref.output_line,
                        start_col: start,
                        end_col: start + src_ref.display_text.len(),
                        color: COLOR_SOURCE_REF,
                        style: HighlightStyle::Underline,
                    });
                }
            }
        }

        regions
    }

    /// Extract all source file references from parsed output.
    ///
    /// Pulls from the structured [`DetectedError`] fields first, then scans the
    /// raw output for additional file:line:col patterns.
    pub fn extract_references(parsed: &ParsedOutput) -> Vec<SourceReference> {
        let mut refs = Vec::new();
        let output_lines: Vec<&str> = parsed.raw_output.lines().collect();

        // From structured errors/warnings.
        for err in parsed.errors.iter().chain(parsed.warnings.iter()) {
            if let Some(file) = &err.file {
                if let Some(line) = err.line {
                    let display = Self::format_display(file, line, err.column);
                    let output_line = Self::find_output_line(&output_lines, &display);
                    refs.push(SourceReference {
                        file: file.clone(),
                        line,
                        column: err.column,
                        display_text: display,
                        output_line,
                    });
                }
            }
        }

        // Scan raw output for additional references not already covered.
        let scanned = Self::scan_for_references(&parsed.raw_output);
        for candidate in scanned {
            let dominated = refs.iter().any(|existing| {
                existing.file == candidate.file
                    && existing.line == candidate.line
                    && existing.output_line == candidate.output_line
            });
            if !dominated {
                refs.push(candidate);
            }
        }

        refs
    }

    /// Scan raw text for file:line:col patterns across multiple languages.
    ///
    /// Recognized patterns:
    /// - `file.ext:42:9`        (Rust, Go, general)
    /// - `file.ext:42`          (Python, Java, general)
    /// - `at file.ext:42:9`     (TypeScript/Node stack traces)
    /// - `File "file.py", line 42`  (Python tracebacks)
    pub fn scan_for_references(text: &str) -> Vec<SourceReference> {
        let mut refs = Vec::new();

        // file.ext:line:col  or  file.ext:line
        // Requires the file portion to contain a dot (so we match actual file paths).
        // Allows leading whitespace, `-->`, `at `, or `(` (for stack traces).
        let general_re = Regex::new(
            r"(?m)(?:^|\s|-->\s*|at\s+|\()([A-Za-z0-9_./-]+\.[A-Za-z0-9]+):(\d+)(?::(\d+))?"
        )
        .unwrap();

        // Python traceback:  File "some/file.py", line 42
        let python_re = Regex::new(
            r#"(?m)File "([^"]+)", line (\d+)"#
        )
        .unwrap();

        for (line_idx, line) in text.lines().enumerate() {
            for caps in general_re.captures_iter(line) {
                let file = caps[1].to_string();
                let line_num: usize = match caps[2].parse() {
                    Ok(n) if n > 0 => n,
                    _ => continue,
                };
                let col: Option<usize> = caps.get(3).and_then(|m| m.as_str().parse().ok());

                let display = Self::format_display(&file, line_num, col);
                refs.push(SourceReference {
                    file,
                    line: line_num,
                    column: col,
                    display_text: display,
                    output_line: line_idx,
                });
            }

            for caps in python_re.captures_iter(line) {
                let file = caps[1].to_string();
                let line_num: usize = match caps[2].parse() {
                    Ok(n) if n > 0 => n,
                    _ => continue,
                };

                // Skip if the general regex already captured the same ref on this line.
                let dominated = refs.iter().any(|r| {
                    r.file == file && r.line == line_num && r.output_line == line_idx
                });
                if dominated {
                    continue;
                }

                let display = Self::format_display(&file, line_num, None);
                refs.push(SourceReference {
                    file,
                    line: line_num,
                    column: None,
                    display_text: display,
                    output_line: line_idx,
                });
            }
        }

        refs
    }

    /// Generate a one-line summary of errors for the agent system.
    ///
    /// Returns `None` when there are no errors or warnings.
    pub fn error_summary(parsed: &ParsedOutput) -> Option<String> {
        let n_errors = parsed.errors.len();
        let n_warnings = parsed.warnings.len();

        // Test failure summaries.
        if let ContentType::TestResults(ref summary) = parsed.content_type {
            if summary.failed > 0 {
                return Some(format!(
                    "Test failed: {} of {} tests",
                    summary.failed, summary.total
                ));
            }
            // All passed, no errors.
            if n_errors == 0 && n_warnings == 0 {
                return None;
            }
        }

        // HTTP error summaries.
        if let ContentType::HttpResponse(ref data) = parsed.content_type {
            if data.status >= 400 {
                return Some(format!("HTTP {} {}", data.status, data.status_text));
            }
        }

        if n_errors == 0 && n_warnings == 0 {
            return None;
        }

        // Build the command label for context.
        let label = Self::command_label(parsed);

        let mut parts = Vec::new();
        if n_errors > 0 {
            parts.push(format!(
                "{} error{}",
                n_errors,
                if n_errors == 1 { "" } else { "s" }
            ));
        }
        if n_warnings > 0 {
            parts.push(format!(
                "{} warning{}",
                n_warnings,
                if n_warnings == 1 { "" } else { "s" }
            ));
        }

        let summary = parts.join(", ");
        if label.is_empty() {
            Some(summary)
        } else {
            Some(format!("{summary} in {label}"))
        }
    }

    /// Should we suggest an agent fix for this output?
    ///
    /// Returns `true` for compiler errors, test failures, or runtime errors.
    /// Returns `false` for warnings only, success, or unknown output.
    pub fn should_suggest_fix(parsed: &ParsedOutput) -> bool {
        // Compiler errors.
        if parsed
            .errors
            .iter()
            .any(|e| matches!(e.error_type, ErrorType::Compiler | ErrorType::Syntax))
        {
            return true;
        }

        // Test failures.
        if let ContentType::TestResults(ref summary) = parsed.content_type {
            if summary.failed > 0 {
                return true;
            }
        }

        // Runtime errors.
        if parsed
            .errors
            .iter()
            .any(|e| matches!(e.error_type, ErrorType::Runtime))
        {
            return true;
        }

        // Non-zero exit code with detected errors of any kind.
        if let Some(code) = parsed.exit_code {
            if code != 0 && !parsed.errors.is_empty() {
                return true;
            }
        }

        false
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Find the 0-indexed output line containing `needle`. Returns 0 if not found.
    fn find_output_line(output_lines: &[&str], needle: &str) -> usize {
        output_lines
            .iter()
            .position(|l| l.contains(needle))
            .unwrap_or(0)
    }

    /// Format a display string like `"file.rs:42:9"` or `"file.rs:42"`.
    fn format_display(file: &str, line: usize, column: Option<usize>) -> String {
        match column {
            Some(col) => format!("{file}:{line}:{col}"),
            None => format!("{file}:{line}"),
        }
    }

    /// Highlight every output line whose text matches `raw_line`.
    fn highlight_raw_line(
        output_lines: &[&str],
        raw_line: &str,
        color: [f32; 4],
        regions: &mut Vec<HighlightRegion>,
    ) {
        if raw_line.is_empty() {
            return;
        }
        for (idx, text) in output_lines.iter().enumerate() {
            if text.contains(raw_line) {
                regions.push(HighlightRegion {
                    line: idx,
                    start_col: 0,
                    end_col: text.len(),
                    color,
                    style: HighlightStyle::Background,
                });
            }
        }
    }

    /// Short label describing the command for summary text.
    fn command_label(parsed: &ParsedOutput) -> String {
        match &parsed.command_type {
            CommandType::Cargo(sub) => {
                let sub_name = match sub {
                    CargoCommand::Build => "cargo build",
                    CargoCommand::Test => "cargo test",
                    CargoCommand::Run => "cargo run",
                    CargoCommand::Check => "cargo check",
                    CargoCommand::Clippy => "cargo clippy",
                    CargoCommand::Other(s) => return format!("cargo {s}"),
                };
                sub_name.to_string()
            }
            CommandType::Npm(sub) => {
                let sub_name = match sub {
                    NpmCommand::Install => "npm install",
                    NpmCommand::Test => "npm test",
                    NpmCommand::Run => "npm run",
                    NpmCommand::Build => "npm build",
                    NpmCommand::Other(s) => return format!("npm {s}"),
                };
                sub_name.to_string()
            }
            _ => {
                // Fall back to the first two tokens of the raw command.
                parsed
                    .command
                    .split_whitespace()
                    .take(2)
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::SemanticParser;

    // -----------------------------------------------------------------------
    // scan_for_references
    // -----------------------------------------------------------------------

    #[test]
    fn scan_rust_file_line_col() {
        let text = "  --> src/main.rs:10:5";
        let refs = ErrorHighlighter::scan_for_references(text);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].file, "src/main.rs");
        assert_eq!(refs[0].line, 10);
        assert_eq!(refs[0].column, Some(5));
        assert_eq!(refs[0].display_text, "src/main.rs:10:5");
    }

    #[test]
    fn scan_python_file_line() {
        let text = "  File \"/app/main.py\", line 42";
        let refs = ErrorHighlighter::scan_for_references(text);
        assert!(!refs.is_empty());
        let py_ref = refs.iter().find(|r| r.file.contains("main.py")).unwrap();
        assert_eq!(py_ref.line, 42);
    }

    #[test]
    fn scan_node_stack_trace() {
        let text = "    at Object.<anonymous> (src/index.ts:15:3)";
        let refs = ErrorHighlighter::scan_for_references(text);
        assert!(!refs.is_empty());
        let ts_ref = refs.iter().find(|r| r.file.contains("index.ts")).unwrap();
        assert_eq!(ts_ref.line, 15);
        assert_eq!(ts_ref.column, Some(3));
    }

    #[test]
    fn scan_go_file_line_col() {
        let text = "./cmd/server.go:88:12: undefined: foo";
        let refs = ErrorHighlighter::scan_for_references(text);
        assert!(!refs.is_empty());
        let go_ref = refs.iter().find(|r| r.file.contains("server.go")).unwrap();
        assert_eq!(go_ref.line, 88);
        assert_eq!(go_ref.column, Some(12));
    }

    #[test]
    fn scan_file_line_no_col() {
        let text = "utils.java:100: error: ';' expected";
        let refs = ErrorHighlighter::scan_for_references(text);
        assert!(!refs.is_empty());
        let java_ref = refs.iter().find(|r| r.file.contains("utils.java")).unwrap();
        assert_eq!(java_ref.line, 100);
        // Column may or may not be parsed; without it, should be None.
        // The pattern is `utils.java:100:` followed by a space, so col is None.
    }

    #[test]
    fn scan_multiple_refs_on_different_lines() {
        let text = "\
error[E0308]: mismatched types
  --> src/lib.rs:10:5
  --> src/main.rs:20:9";
        let refs = ErrorHighlighter::scan_for_references(text);
        assert!(refs.len() >= 2);
        let files: Vec<&str> = refs.iter().map(|r| r.file.as_str()).collect();
        assert!(files.contains(&"src/lib.rs"));
        assert!(files.contains(&"src/main.rs"));
    }

    #[test]
    fn scan_ignores_non_file_patterns() {
        // Pure text with colons but no file extension should yield no refs.
        let text = "Hello: world, this is a test";
        let refs = ErrorHighlighter::scan_for_references(text);
        assert!(refs.is_empty());
    }

    // -----------------------------------------------------------------------
    // error_summary
    // -----------------------------------------------------------------------

    #[test]
    fn summary_cargo_errors_and_warnings() {
        let stderr = "\
error[E0308]: mismatched types
  --> src/main.rs:10:5
   |
10 |     x + \"hello\"
   |     ^^^^^^^^^^^ expected `i32`, found `&str`

warning: unused variable: `y`
  --> src/lib.rs:5:9
   |
5  |     let y = 42;
   |         ^ help: prefix with underscore: `_y`

error: aborting due to 1 previous error
";
        let parsed = SemanticParser::parse("cargo build", "", stderr, Some(101));
        let summary = ErrorHighlighter::error_summary(&parsed);
        assert!(summary.is_some());
        let s = summary.unwrap();
        assert!(s.contains("1 error"), "got: {s}");
        assert!(s.contains("1 warning"), "got: {s}");
        assert!(s.contains("cargo build"), "got: {s}");
    }

    #[test]
    fn summary_test_failure() {
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
        let summary = ErrorHighlighter::error_summary(&parsed);
        assert!(summary.is_some());
        let s = summary.unwrap();
        assert!(s.contains("2 of 5"), "got: {s}");
    }

    #[test]
    fn summary_none_on_success() {
        let stdout = "\
running 2 tests
test tests::a ... ok
test tests::b ... ok

test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
";
        let parsed = SemanticParser::parse("cargo test", stdout, "", Some(0));
        let summary = ErrorHighlighter::error_summary(&parsed);
        assert!(summary.is_none());
    }

    #[test]
    fn summary_http_error() {
        let output = "\
HTTP/1.1 500 Internal Server Error\r\n\
Content-Type: text/plain\r\n\
\r\n\
Something went wrong";
        let parsed = SemanticParser::parse("curl -i https://api.example.com", output, "", Some(0));
        let summary = ErrorHighlighter::error_summary(&parsed);
        assert!(summary.is_some());
        let s = summary.unwrap();
        assert!(s.contains("500"), "got: {s}");
        assert!(s.contains("Internal Server Error"), "got: {s}");
    }

    // -----------------------------------------------------------------------
    // should_suggest_fix
    // -----------------------------------------------------------------------

    #[test]
    fn suggest_fix_on_compiler_error() {
        let stderr = "\
error[E0308]: mismatched types
  --> src/main.rs:10:5
   |
10 |     x + \"hello\"
   |     ^^^^^^^^^^^ expected `i32`, found `&str`

error: aborting due to 1 previous error
";
        let parsed = SemanticParser::parse("cargo build", "", stderr, Some(101));
        assert!(ErrorHighlighter::should_suggest_fix(&parsed));
    }

    #[test]
    fn no_suggest_fix_on_warning_only() {
        let stderr = "\
warning: unused variable: `x`
  --> src/lib.rs:5:9
   |
5  |     let x = 42;
   |         ^ help: prefix with underscore: `_x`
";
        let parsed = SemanticParser::parse("cargo check", "", stderr, Some(0));
        assert!(!ErrorHighlighter::should_suggest_fix(&parsed));
    }

    #[test]
    fn suggest_fix_on_test_failure() {
        let stdout = "\
running 3 tests
test tests::a ... ok
test tests::b ... FAILED
test tests::c ... ok

test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
";
        let parsed = SemanticParser::parse("cargo test", stdout, "", Some(101));
        assert!(ErrorHighlighter::should_suggest_fix(&parsed));
    }

    #[test]
    fn no_suggest_fix_on_clean_build() {
        let parsed = SemanticParser::parse("cargo build", "", "    Compiling foo v0.1.0\n    Finished dev\n", Some(0));
        assert!(!ErrorHighlighter::should_suggest_fix(&parsed));
    }

    // -----------------------------------------------------------------------
    // highlight
    // -----------------------------------------------------------------------

    #[test]
    fn highlight_produces_regions_for_errors() {
        let stderr = "\
error[E0308]: mismatched types
  --> src/main.rs:10:5
   |
10 |     x + \"hello\"
   |     ^^^^^^^^^^^ expected `i32`, found `&str`

error: aborting due to 1 previous error
";
        let parsed = SemanticParser::parse("cargo build", "", stderr, Some(101));
        let regions = ErrorHighlighter::highlight(&parsed);

        // Should have at least one error background region.
        let error_bgs: Vec<_> = regions
            .iter()
            .filter(|r| r.style == HighlightStyle::Background && r.color == COLOR_ERROR)
            .collect();
        assert!(!error_bgs.is_empty(), "expected error background highlights");

        // Should have at least one source reference underline.
        let underlines: Vec<_> = regions
            .iter()
            .filter(|r| r.style == HighlightStyle::Underline)
            .collect();
        assert!(!underlines.is_empty(), "expected source reference underlines");
    }

    #[test]
    fn highlight_warning_uses_yellow() {
        let stderr = "\
warning: unused variable: `x`
  --> src/lib.rs:5:9
   |
5  |     let x = 42;
   |         ^ help: prefix with underscore: `_x`
";
        let parsed = SemanticParser::parse("cargo check", "", stderr, Some(0));
        let regions = ErrorHighlighter::highlight(&parsed);

        let warn_bgs: Vec<_> = regions
            .iter()
            .filter(|r| r.style == HighlightStyle::Background && r.color == COLOR_WARNING)
            .collect();
        assert!(!warn_bgs.is_empty(), "expected warning background highlights");
    }

    // -----------------------------------------------------------------------
    // extract_references
    // -----------------------------------------------------------------------

    #[test]
    fn extract_refs_from_cargo_error() {
        let stderr = "\
error[E0308]: mismatched types
  --> src/main.rs:10:5
   |
10 |     x + \"hello\"
   |     ^^^^^^^^^^^ expected `i32`, found `&str`

error: aborting due to 1 previous error
";
        let parsed = SemanticParser::parse("cargo build", "", stderr, Some(101));
        let refs = ErrorHighlighter::extract_references(&parsed);

        assert!(!refs.is_empty());
        let main_ref = refs.iter().find(|r| r.file == "src/main.rs").unwrap();
        assert_eq!(main_ref.line, 10);
        assert_eq!(main_ref.column, Some(5));
        assert_eq!(main_ref.display_text, "src/main.rs:10:5");
    }

    #[test]
    fn extract_refs_deduplicates() {
        // The same file:line:col appears both in structured errors and raw scan.
        // We should not get duplicates.
        let stderr = "\
error[E0308]: mismatched types
  --> src/main.rs:10:5
   |
10 |     x + \"hello\"
   |     ^^^^^^^^^^^ expected `i32`, found `&str`

error: aborting due to 1 previous error
";
        let parsed = SemanticParser::parse("cargo build", "", stderr, Some(101));
        let refs = ErrorHighlighter::extract_references(&parsed);

        let main_refs: Vec<_> = refs
            .iter()
            .filter(|r| r.file == "src/main.rs" && r.line == 10)
            .collect();
        assert_eq!(main_refs.len(), 1, "expected deduplication, got {main_refs:?}");
    }

    // -----------------------------------------------------------------------
    // summary pluralization
    // -----------------------------------------------------------------------

    #[test]
    fn summary_plural_errors() {
        let parsed = ParsedOutput {
            command: "cargo build".to_string(),
            command_type: CommandType::Cargo(CargoCommand::Build),
            exit_code: Some(101),
            content_type: ContentType::CompilerOutput,
            errors: vec![
                DetectedError {
                    message: "err1".into(),
                    error_type: ErrorType::Compiler,
                    file: None,
                    line: None,
                    column: None,
                    code: None,
                    severity: Severity::Error,
                    raw_line: "error: err1".into(),
                    suggestion: None,
                },
                DetectedError {
                    message: "err2".into(),
                    error_type: ErrorType::Compiler,
                    file: None,
                    line: None,
                    column: None,
                    code: None,
                    severity: Severity::Error,
                    raw_line: "error: err2".into(),
                    suggestion: None,
                },
            ],
            warnings: vec![],
            duration_ms: None,
            raw_output: "error: err1\nerror: err2\n".to_string(),
        };
        let s = ErrorHighlighter::error_summary(&parsed).unwrap();
        assert!(s.contains("2 errors"), "got: {s}");
        assert!(!s.contains("warning"), "got: {s}");
    }
}
