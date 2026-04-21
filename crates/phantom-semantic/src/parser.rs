use regex::Regex;

use crate::types::*;

/// Semantic parser: classifies commands and extracts structured data from
/// terminal output.  Stateless — every method is pure.
pub struct SemanticParser;

impl SemanticParser {
    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    /// Classify a command string into a [`CommandType`].
    pub fn classify_command(cmd: &str) -> CommandType {
        let trimmed = cmd.trim();

        // Handle pipes/chains — classify the *first* command in the pipeline.
        let first_cmd = trimmed
            .split('|')
            .next()
            .unwrap_or(trimmed)
            .trim();

        let mut tokens = first_cmd.split_whitespace();
        let program = match tokens.next() {
            Some(p) => p,
            None => return CommandType::Unknown,
        };

        // Strip leading env vars (FOO=bar cargo build)
        if program.contains('=') {
            return Self::classify_command(
                &first_cmd[first_cmd.find(' ').map(|i| i + 1).unwrap_or(first_cmd.len())..],
            );
        }

        // Strip path prefix: /usr/bin/git -> git
        let base = program.rsplit('/').next().unwrap_or(program);

        let subcommand = tokens.next().unwrap_or("");

        match base {
            "git" => CommandType::Git(Self::classify_git_subcommand(subcommand)),
            "cargo" => CommandType::Cargo(Self::classify_cargo_subcommand(subcommand)),
            "docker" | "docker-compose" | "podman" => {
                CommandType::Docker(Self::classify_docker_subcommand(base, subcommand))
            }
            "npm" | "npx" | "yarn" | "pnpm" => {
                CommandType::Npm(Self::classify_npm_subcommand(subcommand))
            }
            "curl" | "wget" | "httpie" | "http" | "https" => {
                CommandType::Http(Self::classify_http_method(first_cmd))
            }
            // Common shell builtins / utilities
            "ls" | "cd" | "cat" | "echo" | "grep" | "find" | "awk" | "sed" | "sort" | "head"
            | "tail" | "wc" | "mkdir" | "rm" | "cp" | "mv" | "chmod" | "chown" | "kill"
            | "ps" | "top" | "htop" | "df" | "du" | "env" | "export" | "source" | "which"
            | "man" | "touch" | "ln" | "tar" | "zip" | "unzip" | "ssh" | "scp" | "rsync"
            | "make" | "cmake" | "python" | "python3" | "node" | "ruby" | "bash" | "zsh"
            | "sh" => CommandType::Shell,
            _ => CommandType::Unknown,
        }
    }

    /// Full parse pipeline: classify, detect content type, extract errors.
    pub fn parse(
        cmd: &str,
        stdout: &str,
        stderr: &str,
        exit_code: Option<i32>,
    ) -> ParsedOutput {
        let command_type = Self::classify_command(cmd);
        let combined = if stderr.is_empty() {
            stdout.to_string()
        } else if stdout.is_empty() {
            stderr.to_string()
        } else {
            format!("{stdout}\n{stderr}")
        };

        let (content_type, errors, warnings) =
            Self::parse_output(&command_type, stdout, stderr, &combined);

        ParsedOutput {
            command: cmd.to_string(),
            command_type,
            exit_code,
            content_type,
            errors,
            warnings,
            duration_ms: None,
            raw_output: combined,
        }
    }

    // -----------------------------------------------------------------------
    // Subcommand classifiers
    // -----------------------------------------------------------------------

    fn classify_git_subcommand(sub: &str) -> GitCommand {
        match sub {
            "status" => GitCommand::Status,
            "log" => GitCommand::Log,
            "diff" => GitCommand::Diff,
            "push" => GitCommand::Push,
            "pull" => GitCommand::Pull,
            "commit" => GitCommand::Commit,
            "branch" => GitCommand::Branch,
            "checkout" | "switch" => GitCommand::Checkout,
            "" => GitCommand::Other(String::new()),
            other => GitCommand::Other(other.to_string()),
        }
    }

    fn classify_cargo_subcommand(sub: &str) -> CargoCommand {
        match sub {
            "build" | "b" => CargoCommand::Build,
            "test" | "t" => CargoCommand::Test,
            "run" | "r" => CargoCommand::Run,
            "check" | "c" => CargoCommand::Check,
            "clippy" => CargoCommand::Clippy,
            "" => CargoCommand::Other(String::new()),
            other => CargoCommand::Other(other.to_string()),
        }
    }

    fn classify_docker_subcommand(base: &str, sub: &str) -> DockerCommand {
        if base == "docker-compose" {
            return DockerCommand::Compose;
        }
        match sub {
            "ps" => DockerCommand::Ps,
            "images" => DockerCommand::Images,
            "logs" => DockerCommand::Logs,
            "build" => DockerCommand::Build,
            "compose" => DockerCommand::Compose,
            "" => DockerCommand::Other(String::new()),
            other => DockerCommand::Other(other.to_string()),
        }
    }

    fn classify_npm_subcommand(sub: &str) -> NpmCommand {
        match sub {
            "install" | "i" | "ci" | "add" => NpmCommand::Install,
            "test" | "t" => NpmCommand::Test,
            "run" | "run-script" => NpmCommand::Run,
            "build" => NpmCommand::Build,
            "" => NpmCommand::Other(String::new()),
            other => NpmCommand::Other(other.to_string()),
        }
    }

    fn classify_http_method(cmd: &str) -> HttpCommand {
        let lower = cmd.to_lowercase();
        // curl -X METHOD or httpie METHOD
        if lower.contains("-x post") || lower.contains("-x \"post\"") || lower.starts_with("http post") {
            HttpCommand::Post
        } else if lower.contains("-x put") || lower.contains("-x \"put\"") || lower.starts_with("http put") {
            HttpCommand::Put
        } else if lower.contains("-x delete") || lower.contains("-x \"delete\"") || lower.starts_with("http delete") {
            HttpCommand::Delete
        } else {
            // Default for curl/wget without explicit method is GET
            HttpCommand::Get
        }
    }

    // -----------------------------------------------------------------------
    // Output parsers
    // -----------------------------------------------------------------------

    /// Orchestrate content-type detection and error extraction.
    fn parse_output(
        cmd_type: &CommandType,
        stdout: &str,
        stderr: &str,
        combined: &str,
    ) -> (ContentType, Vec<DetectedError>, Vec<DetectedError>) {
        match cmd_type {
            CommandType::Git(GitCommand::Status) => {
                let ct = Self::parse_git_status(combined);
                (ct, vec![], vec![])
            }
            CommandType::Git(GitCommand::Log) => {
                let ct = Self::parse_git_log(combined);
                (ct, vec![], vec![])
            }
            CommandType::Git(GitCommand::Diff) => (ContentType::GitDiff, vec![], vec![]),
            CommandType::Cargo(_) => Self::parse_cargo_output(stdout, stderr),
            CommandType::Http(_) => {
                if let Some(ct) = Self::parse_http_response(combined) {
                    (ct, vec![], vec![])
                } else {
                    Self::fallback_content_type(combined)
                }
            }
            _ => Self::fallback_content_type(combined),
        }
    }

    /// Try JSON, then table, then plain text.
    fn fallback_content_type(
        output: &str,
    ) -> (ContentType, Vec<DetectedError>, Vec<DetectedError>) {
        if let Some(ct) = Self::parse_json(output) {
            return (ct, vec![], vec![]);
        }
        if let Some(ct) = Self::detect_table(output) {
            return (ct, vec![], vec![]);
        }
        (ContentType::PlainText, vec![], vec![])
    }

    // -- git status ---------------------------------------------------------

    fn parse_git_status(output: &str) -> ContentType {
        let branch_re = Regex::new(r"On branch (\S+)").unwrap();
        let upstream_re =
            Regex::new(r"Your branch is .+ '([^']+)'").unwrap();
        let ahead_re = Regex::new(r"ahead of .+ by (\d+) commit").unwrap();
        let behind_re = Regex::new(r"behind .+ by (\d+) commit").unwrap();
        let modified_re = Regex::new(r"(?m)^\s+modified:\s+(.+)$").unwrap();
        let new_file_re = Regex::new(r"(?m)^\s+new file:\s+(.+)$").unwrap();
        let deleted_re = Regex::new(r"(?m)^\s+deleted:\s+(.+)$").unwrap();
        let renamed_re = Regex::new(r"(?m)^\s+renamed:\s+(.+)$").unwrap();

        let branch = branch_re
            .captures(output)
            .map(|c| c[1].to_string())
            .unwrap_or_default();
        let upstream = upstream_re
            .captures(output)
            .map(|c| c[1].to_string());
        let ahead = ahead_re
            .captures(output)
            .and_then(|c| c[1].parse().ok())
            .unwrap_or(0);
        let behind = behind_re
            .captures(output)
            .and_then(|c| c[1].parse().ok())
            .unwrap_or(0);

        // Staged files appear under "Changes to be committed:"
        // Modified (unstaged) under "Changes not staged for commit:"
        // Untracked under "Untracked files:"
        let sections: Vec<&str> = output.split("\n\n").collect();

        let mut staged = Vec::new();
        let mut modified = Vec::new();
        let mut untracked = Vec::new();

        #[derive(PartialEq)]
        enum Section {
            None,
            Staged,
            Unstaged,
            Untracked,
        }

        let mut current_section = Section::None;

        for line in output.lines() {
            let trimmed = line.trim();

            if trimmed.starts_with("Changes to be committed:") {
                current_section = Section::Staged;
                continue;
            }
            if trimmed.starts_with("Changes not staged for commit:") {
                current_section = Section::Unstaged;
                continue;
            }
            if trimmed.starts_with("Untracked files:") {
                current_section = Section::Untracked;
                continue;
            }
            // A blank line or a new header resets (but we handle section
            // transitions above, so we just skip hint lines).
            if trimmed.is_empty() || trimmed.starts_with('(') {
                continue;
            }

            match current_section {
                Section::Staged => {
                    // Lines like "  new file:   foo.rs" or "  modified:   bar.rs"
                    if let Some(caps) = new_file_re.captures(line) {
                        staged.push(caps[1].trim().to_string());
                    } else if let Some(caps) = modified_re.captures(line) {
                        staged.push(caps[1].trim().to_string());
                    } else if let Some(caps) = deleted_re.captures(line) {
                        staged.push(caps[1].trim().to_string());
                    } else if let Some(caps) = renamed_re.captures(line) {
                        staged.push(caps[1].trim().to_string());
                    }
                }
                Section::Unstaged => {
                    if let Some(caps) = modified_re.captures(line) {
                        modified.push(caps[1].trim().to_string());
                    } else if let Some(caps) = deleted_re.captures(line) {
                        modified.push(caps[1].trim().to_string());
                    }
                }
                Section::Untracked => {
                    // Untracked files are just bare filenames indented with a tab
                    // or spaces, no prefix label.
                    let _ = sections; // suppress unused warning
                    let name = trimmed.to_string();
                    if !name.is_empty()
                        && !name.starts_with("(use")
                        && !name.starts_with("nothing")
                    {
                        untracked.push(name);
                    }
                }
                Section::None => {}
            }
        }

        ContentType::GitStatus(GitStatusData {
            branch,
            upstream,
            modified,
            staged,
            untracked,
            ahead,
            behind,
        })
    }

    // -- git log ------------------------------------------------------------

    fn parse_git_log(output: &str) -> ContentType {
        let commit_re = Regex::new(r"(?m)^commit ([0-9a-f]+)").unwrap();
        let author_re = Regex::new(r"(?m)^Author:\s+(.+)$").unwrap();
        let date_re = Regex::new(r"(?m)^Date:\s+(.+)$").unwrap();

        // Split on "commit <hash>" boundaries.
        let blocks: Vec<&str> = commit_re
            .split(output)
            .filter(|s| !s.trim().is_empty())
            .collect();
        let hashes: Vec<String> = commit_re
            .captures_iter(output)
            .map(|c| c[1].to_string())
            .collect();

        let mut entries = Vec::new();

        for (i, block) in blocks.iter().enumerate() {
            let hash = hashes.get(i).cloned().unwrap_or_default();
            let author = author_re
                .captures(block)
                .map(|c| c[1].trim().to_string())
                .unwrap_or_default();
            let date = date_re
                .captures(block)
                .map(|c| c[1].trim().to_string())
                .unwrap_or_default();

            // Message lines are indented with 4 spaces after the Date header.
            let message: String = block
                .lines()
                .skip_while(|l| !l.starts_with("Date:"))
                .skip(1) // skip the Date line itself
                .map(|l| l.trim())
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join(" ");

            if !hash.is_empty() {
                entries.push(GitLogEntry {
                    hash,
                    author,
                    date,
                    message,
                });
            }
        }

        // Also try the compact `<hash> <message>` one-line format.
        if entries.is_empty() {
            let oneline_re = Regex::new(r"(?m)^([0-9a-f]{7,40})\s+(.+)$").unwrap();
            for caps in oneline_re.captures_iter(output) {
                entries.push(GitLogEntry {
                    hash: caps[1].to_string(),
                    author: String::new(),
                    date: String::new(),
                    message: caps[2].to_string(),
                });
            }
        }

        if entries.is_empty() {
            ContentType::PlainText
        } else {
            ContentType::GitLog(entries)
        }
    }

    // -- cargo output -------------------------------------------------------

    fn parse_cargo_output(
        stdout: &str,
        stderr: &str,
    ) -> (ContentType, Vec<DetectedError>, Vec<DetectedError>) {
        let mut errors = Self::parse_rust_errors(stderr);
        let warnings: Vec<DetectedError> = errors
            .iter()
            .filter(|e| e.severity == Severity::Warning)
            .cloned()
            .collect();
        errors.retain(|e| e.severity == Severity::Error);

        // Check for test results in stdout (cargo test prints there).
        let combined = format!("{stdout}\n{stderr}");
        if let Some(summary) = Self::parse_test_results(&combined) {
            return (ContentType::TestResults(summary), errors, warnings);
        }

        if errors.is_empty() && warnings.is_empty() {
            // Successful build / check — plain text
            (ContentType::PlainText, vec![], vec![])
        } else {
            (ContentType::CompilerOutput, errors, warnings)
        }
    }

    // -- rust errors --------------------------------------------------------

    fn parse_rust_errors(stderr: &str) -> Vec<DetectedError> {
        let mut results = Vec::new();

        // Primary pattern:
        //   error[E0308]: mismatched types
        //     --> src/main.rs:10:5
        let diag_re = Regex::new(
            r"(?m)^(error|warning)(?:\[([A-Z]\d+)\])?: (.+)\n\s*--> ([^:]+):(\d+):(\d+)",
        )
        .unwrap();

        let help_re = Regex::new(r"(?m)^\s*= help: (.+)$").unwrap();
        let suggestion_re = Regex::new(r"(?m)^help: (.+)$").unwrap();

        for caps in diag_re.captures_iter(stderr) {
            let severity_str = &caps[1];
            let code = caps.get(2).map(|m| m.as_str().to_string());
            let message = caps[3].to_string();
            let file = caps[4].to_string();
            let line: usize = caps[5].parse().unwrap_or(0);
            let column: usize = caps[6].parse().unwrap_or(0);

            let severity = match severity_str {
                "error" => Severity::Error,
                "warning" => Severity::Warning,
                _ => Severity::Info,
            };

            let error_type = if severity == Severity::Warning {
                ErrorType::Compiler
            } else {
                ErrorType::Compiler
            };

            // Try to find a suggestion near this diagnostic.
            // We search the text following this match for help/suggestion lines.
            let match_end = caps.get(0).unwrap().end();
            let remaining = &stderr[match_end..];
            // Take up to the next error/warning boundary.
            let next_diag = remaining.find("\nerror").or(remaining.find("\nwarning"));
            let scope = match next_diag {
                Some(pos) => &remaining[..pos],
                None => remaining,
            };

            let suggestion = help_re
                .captures(scope)
                .or_else(|| suggestion_re.captures(scope))
                .map(|c| c[1].trim().to_string());

            let raw_line = caps[0].lines().next().unwrap_or("").to_string();

            results.push(DetectedError {
                message,
                error_type,
                file: Some(file),
                line: Some(line),
                column: Some(column),
                code,
                severity,
                raw_line,
                suggestion,
            });
        }

        // Simpler pattern for linker errors or plain "error: ..." without location.
        let simple_err_re = Regex::new(r"(?m)^error: (.+)$").unwrap();
        for caps in simple_err_re.captures_iter(stderr) {
            let msg = caps[1].to_string();
            // Skip if we already captured this as a richer diagnostic.
            if results.iter().any(|e| e.message == msg) {
                continue;
            }
            // Skip the "aborting due to N previous errors" noise.
            if msg.starts_with("aborting due to") || msg.starts_with("could not compile") {
                continue;
            }
            results.push(DetectedError {
                message: msg,
                error_type: ErrorType::Compiler,
                file: None,
                line: None,
                column: None,
                code: None,
                severity: Severity::Error,
                raw_line: caps[0].to_string(),
                suggestion: None,
            });
        }

        results
    }

    // -- test results -------------------------------------------------------

    fn parse_test_results(output: &str) -> Option<TestSummary> {
        // `test result: ok. 5 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out`
        // `test result: FAILED. 3 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out`
        let result_re = Regex::new(
            r"test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored",
        )
        .unwrap();

        let caps = result_re.captures(output)?;
        let passed: u32 = caps[1].parse().unwrap_or(0);
        let failed: u32 = caps[2].parse().unwrap_or(0);
        let ignored: u32 = caps[3].parse().unwrap_or(0);
        let total = passed + failed + ignored;

        // Collect names of failing tests.
        // Pattern: `test some::path ... FAILED`
        let fail_re = Regex::new(r"(?m)^test (.+) \.\.\. FAILED$").unwrap();
        let failures: Vec<String> = fail_re
            .captures_iter(output)
            .map(|c| c[1].trim().to_string())
            .collect();

        Some(TestSummary {
            passed,
            failed,
            ignored,
            total,
            failures,
        })
    }

    // -- JSON detection -----------------------------------------------------

    fn parse_json(output: &str) -> Option<ContentType> {
        let trimmed = output.trim();
        if trimmed.is_empty() {
            return None;
        }
        // Quick gate: must start with { or [
        if !trimmed.starts_with('{') && !trimmed.starts_with('[') {
            return None;
        }
        // Try to parse as valid JSON.
        if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
            Some(ContentType::Json)
        } else {
            None
        }
    }

    // -- HTTP response parsing (curl -i) ------------------------------------

    fn parse_http_response(output: &str) -> Option<ContentType> {
        // Pattern: `HTTP/1.1 200 OK` or `HTTP/2 404 Not Found`
        let status_re =
            Regex::new(r"(?m)^HTTP/[\d.]+ (\d{3})\s*(.*)$").unwrap();
        let content_type_re =
            Regex::new(r"(?mi)^content-type:\s*(.+)$").unwrap();

        let caps = status_re.captures(output)?;
        let status: u16 = caps[1].parse().ok()?;
        let status_text = caps[2].trim().to_string();

        let content_type = content_type_re
            .captures(output)
            .map(|c| c[1].trim().to_string());

        // Body starts after the first blank line following headers.
        let body_preview = output
            .split("\r\n\r\n")
            .nth(1)
            .or_else(|| output.split("\n\n").nth(1))
            .unwrap_or("")
            .chars()
            .take(512)
            .collect::<String>();

        Some(ContentType::HttpResponse(HttpResponseData {
            status,
            status_text,
            content_type,
            body_preview,
        }))
    }

    // -- table detection ----------------------------------------------------

    fn detect_table(output: &str) -> Option<ContentType> {
        let lines: Vec<&str> = output
            .lines()
            .filter(|l| !l.trim().is_empty())
            .collect();

        if lines.len() < 2 {
            return None;
        }

        // Heuristic 1: tab-separated — if every non-empty line has at least one tab
        // and the number of tabs per line is consistent.
        let tab_counts: Vec<usize> = lines.iter().map(|l| l.matches('\t').count()).collect();
        if tab_counts.iter().all(|&c| c > 0) {
            let first = tab_counts[0];
            if tab_counts.iter().all(|&c| c == first) {
                return Some(ContentType::Table);
            }
        }

        // Heuristic 2: consistent column alignment via 2+ spaces.
        // Find positions of "  " (2+ space gaps) in each line.
        // If at least 60% of lines share the same number of gaps and those
        // positions are roughly consistent, it is a table.
        let gap_re = Regex::new(r" {2,}").unwrap();
        let gap_counts: Vec<usize> = lines.iter().map(|l| gap_re.find_iter(l).count()).collect();

        if gap_counts.iter().all(|&c| c > 0) {
            let first = gap_counts[0];
            let matching = gap_counts.iter().filter(|&&c| c == first).count();
            if matching as f64 / gap_counts.len() as f64 >= 0.6 {
                return Some(ContentType::Table);
            }
        }

        None
    }
}
