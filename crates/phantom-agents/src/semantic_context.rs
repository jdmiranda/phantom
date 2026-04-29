//! Semantic context ring-buffer for agent reasoning.
//!
//! After every `RunCommand` tool call the agent runtime parses the raw output
//! with [`phantom_semantic::SemanticParser`] and pushes the result into the
//! agent's [`SemanticContext`].  The most-recent N (default 10) parsed outputs
//! are retained so agents can see structured command history rather than raw
//! text blobs.
//!
//! The context is injected into agent system prompts via
//! [`SemanticContext::as_prompt_section`], which renders a compact Markdown
//! block the model can reference.  Agents that have never run a command omit
//! the section entirely.

use phantom_semantic::{ContentType, ParsedOutput, SemanticParser};

/// Maximum number of parsed outputs retained per agent.
const MAX_ENTRIES: usize = 10;

/// A ring-buffer of the most-recently parsed command outputs for one agent.
///
/// Entries are ordered oldest-first; the last entry is always the most recent.
/// When the buffer is full the oldest entry is evicted (FIFO).
#[derive(Debug, Default, Clone)]
pub struct SemanticContext {
    entries: Vec<ParsedOutput>,
}

impl SemanticContext {
    /// Create an empty context.
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a new [`ParsedOutput`] onto the ring-buffer.
    ///
    /// When the buffer is full the oldest entry is evicted so the length never
    /// exceeds [`MAX_ENTRIES`].
    pub fn push(&mut self, output: ParsedOutput) {
        if self.entries.len() >= MAX_ENTRIES {
            self.entries.remove(0);
        }
        self.entries.push(output);
    }

    /// Parse `command` output (stdout + stderr + exit_code) and push the
    /// result onto the ring-buffer.
    ///
    /// This is the canonical call-site: `execute_run_command` calls this after
    /// every successful (or failed) command invocation.
    pub fn parse_and_push(
        &mut self,
        command: &str,
        stdout: &str,
        stderr: &str,
        exit_code: Option<i32>,
    ) -> &ParsedOutput {
        let parsed = SemanticParser::parse(command, stdout, stderr, exit_code);
        self.push(parsed);
        &self.entries[self.entries.len() - 1]
    }

    /// Return all retained entries, oldest-first.
    pub fn entries(&self) -> &[ParsedOutput] {
        &self.entries
    }

    /// Return the most-recently pushed entry, if any.
    pub fn latest(&self) -> Option<&ParsedOutput> {
        self.entries.last()
    }

    /// Number of entries currently held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no entries have been pushed yet.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Render the context as a Markdown section for inclusion in agent system
    /// prompts.
    ///
    /// Returns `None` when the context is empty (no commands have been run).
    /// Otherwise returns a `String` suitable for inserting under a
    /// `## Recent command output` heading.
    pub fn as_prompt_section(&self) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }

        let mut lines = Vec::new();
        lines.push("## Recent command output".to_string());
        lines.push(String::new());
        lines.push(
            "The following structured output from recent commands is available for your reasoning:"
                .to_string(),
        );
        lines.push(String::new());

        for entry in &self.entries {
            lines.push(format!("### `{}`", entry.command));
            lines.push(format!("- **type**: {}", content_type_label(&entry.content_type)));
            if let Some(code) = entry.exit_code {
                lines.push(format!("- **exit_code**: {code}"));
            }
            if !entry.errors.is_empty() {
                lines.push(format!(
                    "- **errors**: {} detected",
                    entry.errors.len()
                ));
                for e in &entry.errors {
                    let loc = match (&e.file, e.line) {
                        (Some(f), Some(l)) => format!(" at {f}:{l}"),
                        _ => String::new(),
                    };
                    lines.push(format!("  - `{}`{loc}", e.message));
                }
            }
            if !entry.warnings.is_empty() {
                lines.push(format!(
                    "- **warnings**: {} detected",
                    entry.warnings.len()
                ));
            }
            // Append structured content summary.
            append_content_summary(&entry.content_type, &mut lines);
            lines.push(String::new());
        }

        Some(lines.join("\n"))
    }
}

/// Short human-readable label for a [`ContentType`] variant.
fn content_type_label(ct: &ContentType) -> &'static str {
    match ct {
        ContentType::PlainText => "plain_text",
        ContentType::Json => "json",
        ContentType::Table => "table",
        ContentType::GitStatus(_) => "git.status",
        ContentType::GitLog(_) => "git.log",
        ContentType::GitDiff => "git.diff",
        ContentType::CompilerOutput => "build.error",
        ContentType::TestResults(_) => "test.result",
        ContentType::HttpResponse(_) => "http.response",
    }
}

/// Append a brief structured summary of the content to `lines`.
fn append_content_summary(ct: &ContentType, lines: &mut Vec<String>) {
    match ct {
        ContentType::GitStatus(data) => {
            lines.push(format!("- **branch**: {}", data.branch));
            if data.ahead > 0 {
                lines.push(format!("- **ahead**: {}", data.ahead));
            }
            if data.behind > 0 {
                lines.push(format!("- **behind**: {}", data.behind));
            }
            if !data.staged.is_empty() {
                lines.push(format!("- **staged**: {}", data.staged.join(", ")));
            }
            if !data.modified.is_empty() {
                lines.push(format!("- **modified**: {}", data.modified.join(", ")));
            }
            if !data.untracked.is_empty() {
                lines.push(format!("- **untracked**: {}", data.untracked.join(", ")));
            }
        }
        ContentType::GitLog(entries) => {
            lines.push(format!("- **commits**: {}", entries.len()));
            if let Some(first) = entries.first() {
                lines.push(format!("- **latest**: {} — {}", &first.hash[..7.min(first.hash.len())], first.message));
            }
        }
        ContentType::TestResults(summary) => {
            lines.push(format!(
                "- **passed**: {}, **failed**: {}, **total**: {}",
                summary.passed, summary.failed, summary.total
            ));
            if !summary.failures.is_empty() {
                lines.push(format!("- **failed tests**: {}", summary.failures.join(", ")));
            }
        }
        ContentType::HttpResponse(data) => {
            lines.push(format!("- **status**: {} {}", data.status, data.status_text));
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_semantic::{ContentType, SemanticParser};

    // -----------------------------------------------------------------------
    // TDD: failing tests written first — implementation follows
    // -----------------------------------------------------------------------

    // -- Issue #74 acceptance: git status → GitStatus variant ----------------

    /// `git status` output must produce a `SemanticOutput::Git` (i.e.
    /// `ContentType::GitStatus`) variant — not `None`, not `PlainText`.
    #[test]
    fn git_status_output_produces_git_status_content_type() {
        let stdout = "\
On branch main
Your branch is up to date with 'origin/main'.

nothing to commit, working tree clean
";
        let parsed = SemanticParser::parse("git status", stdout, "", Some(0));
        assert!(
            matches!(parsed.content_type, ContentType::GitStatus(_)),
            "expected GitStatus, got {:?}",
            parsed.content_type
        );
    }

    /// The structured `GitStatusData` inside the variant must expose the
    /// branch name and file lists the agent will use for reasoning.
    #[test]
    fn git_status_content_type_carries_branch_and_files() {
        let stdout = "\
On branch feature/parser
Your branch is ahead of 'origin/feature/parser' by 2 commits.

Changes not staged for commit:
\tmodified:   src/lib.rs

Untracked files:
\tfoo.txt
";
        let parsed = SemanticParser::parse("git status", stdout, "", Some(0));
        let ContentType::GitStatus(ref data) = parsed.content_type else {
            panic!("expected GitStatus, got {:?}", parsed.content_type)
        };
        assert_eq!(data.branch, "feature/parser");
        assert_eq!(data.ahead, 2);
        assert!(data.modified.contains(&"src/lib.rs".to_string()));
        assert!(data.untracked.contains(&"foo.txt".to_string()));
    }

    // -- Issue #74 acceptance: raw-text fallback for unrecognized commands ---

    /// An unrecognized command with no special output must produce `PlainText`
    /// rather than panicking or returning `None`.
    #[test]
    fn unrecognized_command_falls_back_to_plain_text() {
        let parsed = SemanticParser::parse("my-custom-tool --flag", "some output", "", Some(0));
        assert_eq!(
            parsed.content_type,
            ContentType::PlainText,
            "unrecognized command must fall back to PlainText"
        );
    }

    /// The raw output is always preserved in `ParsedOutput::raw_output` so the
    /// agent still has access to the original text even when classification
    /// produced a richer type.
    #[test]
    fn raw_output_always_preserved() {
        let stdout = "On branch main\nnothing to commit, working tree clean\n";
        let parsed = SemanticParser::parse("git status", stdout, "", Some(0));
        assert!(
            parsed.raw_output.contains("On branch main"),
            "raw_output must preserve original stdout"
        );
    }

    // -- SemanticContext ring-buffer -----------------------------------------

    /// Pushing onto an empty context increments the length to 1.
    #[test]
    fn push_onto_empty_context_length_is_one() {
        let mut ctx = SemanticContext::new();
        assert!(ctx.is_empty());

        ctx.parse_and_push("git status", "On branch main\nnothing to commit, working tree clean\n", "", Some(0));
        assert_eq!(ctx.len(), 1);
        assert!(!ctx.is_empty());
    }

    /// `latest()` returns `Some(&ParsedOutput)` with the most recently pushed entry.
    #[test]
    fn latest_returns_most_recent_entry() {
        let mut ctx = SemanticContext::new();
        ctx.parse_and_push("echo hello", "hello\n", "", Some(0));
        ctx.parse_and_push("git status", "On branch main\nnothing to commit, working tree clean\n", "", Some(0));

        let latest = ctx.latest().expect("context should have entries");
        assert_eq!(latest.command, "git status");
        assert!(matches!(latest.content_type, ContentType::GitStatus(_)));
    }

    /// Pushing more than MAX_ENTRIES entries evicts the oldest, keeping the
    /// buffer at MAX_ENTRIES.
    #[test]
    fn ring_buffer_evicts_oldest_at_max_capacity() {
        let mut ctx = SemanticContext::new();

        // Push MAX_ENTRIES + 2 entries.
        for i in 0..(MAX_ENTRIES + 2) {
            ctx.parse_and_push(
                &format!("echo {i}"),
                &format!("{i}\n"),
                "",
                Some(0),
            );
        }

        assert_eq!(ctx.len(), MAX_ENTRIES, "must not exceed MAX_ENTRIES");

        // The oldest (echo 0, echo 1) must have been evicted.
        let commands: Vec<&str> = ctx.entries().iter().map(|e| e.command.as_str()).collect();
        assert!(
            !commands.contains(&"echo 0"),
            "echo 0 should have been evicted"
        );
        assert!(
            !commands.contains(&"echo 1"),
            "echo 1 should have been evicted"
        );
        assert!(
            commands.contains(&format!("echo {}", MAX_ENTRIES + 1).as_str()),
            "most recent entry must be present"
        );
    }

    // -- as_prompt_section --------------------------------------------------

    /// Empty context returns `None` — no heading injected.
    #[test]
    fn empty_context_prompt_section_is_none() {
        let ctx = SemanticContext::new();
        assert!(ctx.as_prompt_section().is_none());
    }

    /// After pushing entries, `as_prompt_section` returns `Some(String)` that
    /// contains the "## Recent command output" heading.
    #[test]
    fn non_empty_context_prompt_section_has_heading() {
        let mut ctx = SemanticContext::new();
        ctx.parse_and_push("git status", "On branch main\nnothing to commit, working tree clean\n", "", Some(0));

        let section = ctx.as_prompt_section().expect("section must be Some");
        assert!(
            section.contains("## Recent command output"),
            "section must contain the standard heading; got:\n{section}"
        );
    }

    /// The git.status content type label surfaces in the prompt section.
    #[test]
    fn git_status_content_type_surfaces_in_prompt_section() {
        let mut ctx = SemanticContext::new();
        ctx.parse_and_push(
            "git status",
            "On branch main\nnothing to commit, working tree clean\n",
            "",
            Some(0),
        );

        let section = ctx.as_prompt_section().unwrap();
        assert!(
            section.contains("git.status"),
            "section must label git status output; got:\n{section}"
        );
    }

    /// The git status branch name surfaces in the prompt section.
    #[test]
    fn git_status_branch_surfaces_in_prompt_section() {
        let mut ctx = SemanticContext::new();
        ctx.parse_and_push(
            "git status",
            "On branch feature/xyz\nnothing to commit, working tree clean\n",
            "",
            Some(0),
        );

        let section = ctx.as_prompt_section().unwrap();
        assert!(
            section.contains("feature/xyz"),
            "branch name must surface in prompt section; got:\n{section}"
        );
    }

    /// Test results surface pass/fail counts in the prompt section.
    #[test]
    fn test_results_surface_in_prompt_section() {
        let mut ctx = SemanticContext::new();
        let stdout = "\
running 3 tests
test tests::a ... ok
test tests::b ... ok
test tests::c ... ok

test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
";
        ctx.parse_and_push("cargo test", stdout, "", Some(0));

        let section = ctx.as_prompt_section().unwrap();
        assert!(
            section.contains("test.result"),
            "content type label must be test.result; got:\n{section}"
        );
        assert!(
            section.contains("passed"),
            "passed count must surface; got:\n{section}"
        );
    }

    // -- content_type_label --------------------------------------------------

    #[test]
    fn content_type_label_git_status() {
        use phantom_semantic::GitStatusData;
        let ct = ContentType::GitStatus(GitStatusData {
            branch: "main".into(),
            upstream: None,
            modified: vec![],
            staged: vec![],
            untracked: vec![],
            ahead: 0,
            behind: 0,
        });
        assert_eq!(content_type_label(&ct), "git.status");
    }

    #[test]
    fn content_type_label_plain_text() {
        assert_eq!(content_type_label(&ContentType::PlainText), "plain_text");
    }

    #[test]
    fn content_type_label_compiler_output() {
        assert_eq!(content_type_label(&ContentType::CompilerOutput), "build.error");
    }
}
