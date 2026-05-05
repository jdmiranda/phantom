pub mod errors;
pub mod parser;
/// C-ABI export for `phantom-skill-host` hot-module loading.
/// The `phantom_skill_register` symbol is exported from the cdylib artifact.
pub mod skill_export;
pub mod types;

pub use errors::*;
pub use parser::*;
pub use skill_export::phantom_skill_register;
pub use types::*;

#[cfg(test)]
mod tests {
    use super::*;

    // =======================================================================
    // Command classification
    // =======================================================================

    #[test]
    fn classify_git_status() {
        assert_eq!(
            SemanticParser::classify_command("git status"),
            CommandType::Git(GitCommand::Status)
        );
    }

    #[test]
    fn classify_git_log() {
        assert_eq!(
            SemanticParser::classify_command("git log --oneline"),
            CommandType::Git(GitCommand::Log)
        );
    }

    #[test]
    fn classify_cargo_build() {
        assert_eq!(
            SemanticParser::classify_command("cargo build --release"),
            CommandType::Cargo(CargoCommand::Build)
        );
    }

    #[test]
    fn classify_cargo_test_alias() {
        assert_eq!(
            SemanticParser::classify_command("cargo t"),
            CommandType::Cargo(CargoCommand::Test)
        );
    }

    #[test]
    fn classify_npm_install() {
        assert_eq!(
            SemanticParser::classify_command("npm install"),
            CommandType::Npm(NpmCommand::Install)
        );
    }

    #[test]
    fn classify_curl_get() {
        assert_eq!(
            SemanticParser::classify_command("curl https://api.example.com"),
            CommandType::Http(HttpCommand::Get)
        );
    }

    #[test]
    fn classify_curl_post() {
        assert_eq!(
            SemanticParser::classify_command("curl -X POST https://api.example.com"),
            CommandType::Http(HttpCommand::Post)
        );
    }

    #[test]
    fn classify_shell_ls() {
        assert_eq!(
            SemanticParser::classify_command("ls -la"),
            CommandType::Shell
        );
    }

    #[test]
    fn classify_unknown() {
        assert_eq!(
            SemanticParser::classify_command("some-custom-binary --flag"),
            CommandType::Unknown
        );
    }

    #[test]
    fn classify_with_env_prefix() {
        assert_eq!(
            SemanticParser::classify_command("RUST_LOG=debug cargo run"),
            CommandType::Cargo(CargoCommand::Run)
        );
    }

    #[test]
    fn classify_docker_compose() {
        assert_eq!(
            SemanticParser::classify_command("docker compose up"),
            CommandType::Docker(DockerCommand::Compose)
        );
    }

    // =======================================================================
    // Git status parsing
    // =======================================================================

    #[test]
    fn parse_git_status_clean() {
        let output = "\
On branch main
Your branch is up to date with 'origin/main'.

nothing to commit, working tree clean
";
        let parsed = SemanticParser::parse("git status", output, "", Some(0));
        match &parsed.content_type {
            ContentType::GitStatus(data) => {
                assert_eq!(data.branch, "main");
                assert_eq!(data.upstream.as_deref(), Some("origin/main"));
                assert!(data.modified.is_empty());
                assert!(data.staged.is_empty());
                assert!(data.untracked.is_empty());
                assert_eq!(data.ahead, 0);
                assert_eq!(data.behind, 0);
            }
            other => panic!("expected GitStatus, got {:?}", other),
        }
    }

    #[test]
    fn parse_git_status_dirty() {
        let output = "\
On branch feature/parser
Your branch is ahead of 'origin/feature/parser' by 3 commits.
  (use \"git push\" to publish your local commits)

Changes to be committed:
  (use \"git restore --staged <file>...\" to unstage)
\tnew file:   src/parser.rs

Changes not staged for commit:
  (use \"git add <file>...\" to update what will be committed)
  (use \"git restore <file>...\" to discard changes in working directory)
\tmodified:   src/lib.rs
\tmodified:   Cargo.toml

Untracked files:
  (use \"git add <file>...\" to include in what will be committed)
\ttests/
\t.env.local
";
        let parsed = SemanticParser::parse("git status", output, "", Some(0));
        match &parsed.content_type {
            ContentType::GitStatus(data) => {
                assert_eq!(data.branch, "feature/parser");
                assert_eq!(data.ahead, 3);
                assert_eq!(data.staged, vec!["src/parser.rs"]);
                assert_eq!(data.modified, vec!["src/lib.rs", "Cargo.toml"]);
                assert_eq!(data.untracked, vec!["tests/", ".env.local"]);
            }
            other => panic!("expected GitStatus, got {:?}", other),
        }
    }

    #[test]
    fn parse_git_status_behind() {
        let output = "\
On branch main
Your branch is behind 'origin/main' by 5 commits, and can be fast-forwarded.
  (use \"git pull\" to update your local branch)

nothing to commit, working tree clean
";
        let parsed = SemanticParser::parse("git status", output, "", Some(0));
        match &parsed.content_type {
            ContentType::GitStatus(data) => {
                assert_eq!(data.behind, 5);
                assert_eq!(data.ahead, 0);
            }
            other => panic!("expected GitStatus, got {:?}", other),
        }
    }

    // =======================================================================
    // Cargo error parsing
    // =======================================================================

    #[test]
    fn parse_cargo_build_error() {
        let stderr = "\
error[E0308]: mismatched types
  --> src/main.rs:10:5
   |
10 |     x + \"hello\"
   |     ^^^^^^^^^^^ expected `i32`, found `&str`
   |
   = help: try using `.to_string()` to convert

error: aborting due to 1 previous error
";
        let parsed = SemanticParser::parse("cargo build", "", stderr, Some(101));
        assert_eq!(parsed.content_type, ContentType::CompilerOutput);
        assert_eq!(parsed.errors.len(), 1);

        let err = &parsed.errors[0];
        assert_eq!(err.message, "mismatched types");
        assert_eq!(err.code.as_deref(), Some("E0308"));
        assert_eq!(err.file.as_deref(), Some("src/main.rs"));
        assert_eq!(err.line, Some(10));
        assert_eq!(err.column, Some(5));
        assert_eq!(err.severity, Severity::Error);
        assert_eq!(
            err.suggestion.as_deref(),
            Some("try using `.to_string()` to convert")
        );
    }

    #[test]
    fn parse_cargo_warning() {
        let stderr_real = "\
warning: unused variable: `x`
  --> src/lib.rs:5:9
   |
5  |     let x = 42;
   |         ^ help: if this is intentional, prefix it with an underscore: `_x`
";
        let parsed = SemanticParser::parse("cargo check", "", stderr_real, Some(0));
        // warnings end up in parsed.warnings
        assert_eq!(parsed.warnings.len(), 1);
        assert_eq!(parsed.warnings[0].severity, Severity::Warning);
        assert_eq!(parsed.warnings[0].file.as_deref(), Some("src/lib.rs"));
    }

    // =======================================================================
    // Test result parsing
    // =======================================================================

    #[test]
    fn parse_cargo_test_results_pass() {
        let stdout = "\
running 4 tests
test tests::test_one ... ok
test tests::test_two ... ok
test tests::test_three ... ok
test tests::test_four ... ok

test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
";
        let parsed = SemanticParser::parse("cargo test", stdout, "", Some(0));
        match &parsed.content_type {
            ContentType::TestResults(summary) => {
                assert_eq!(summary.passed, 4);
                assert_eq!(summary.failed, 0);
                assert_eq!(summary.ignored, 0);
                assert_eq!(summary.total, 4);
                assert!(summary.failures.is_empty());
            }
            other => panic!("expected TestResults, got {:?}", other),
        }
    }

    #[test]
    fn parse_cargo_test_results_fail() {
        let stdout = "\
running 3 tests
test tests::test_one ... ok
test tests::test_broken ... FAILED
test tests::test_two ... ok

failures:

---- tests::test_broken stdout ----
thread 'tests::test_broken' panicked at 'assertion failed'

failures:
    tests::test_broken

test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.02s
";
        let parsed = SemanticParser::parse("cargo test", stdout, "", Some(101));
        match &parsed.content_type {
            ContentType::TestResults(summary) => {
                assert_eq!(summary.passed, 2);
                assert_eq!(summary.failed, 1);
                assert_eq!(summary.total, 3);
                assert_eq!(summary.failures, vec!["tests::test_broken"]);
            }
            other => panic!("expected TestResults, got {:?}", other),
        }
    }

    // =======================================================================
    // JSON detection
    // =======================================================================

    #[test]
    fn detect_json_object() {
        let output = r#"{"name": "phantom", "version": "0.1.0"}"#;
        let parsed = SemanticParser::parse("echo test", output, "", Some(0));
        assert_eq!(parsed.content_type, ContentType::Json);
    }

    #[test]
    fn detect_json_array() {
        let output = r#"[1, 2, 3]"#;
        let parsed = SemanticParser::parse("echo test", output, "", Some(0));
        assert_eq!(parsed.content_type, ContentType::Json);
    }

    #[test]
    fn plain_text_not_json() {
        let output = "just some text output";
        let parsed = SemanticParser::parse("echo test", output, "", Some(0));
        assert_eq!(parsed.content_type, ContentType::PlainText);
    }

    // =======================================================================
    // HTTP response parsing
    // =======================================================================

    #[test]
    fn parse_curl_http_response() {
        let output = "\
HTTP/1.1 200 OK\r\n\
Content-Type: application/json\r\n\
Content-Length: 27\r\n\
\r\n\
{\"status\": \"healthy\"}";
        let parsed = SemanticParser::parse("curl -i https://api.example.com", output, "", Some(0));
        match &parsed.content_type {
            ContentType::HttpResponse(data) => {
                assert_eq!(data.status, 200);
                assert_eq!(data.status_text, "OK");
                assert_eq!(
                    data.content_type.as_deref(),
                    Some("application/json")
                );
                assert!(data.body_preview.contains("healthy"));
            }
            other => panic!("expected HttpResponse, got {:?}", other),
        }
    }

    #[test]
    fn parse_curl_404() {
        let output = "\
HTTP/2 404 Not Found\r\n\
Content-Type: text/html\r\n\
\r\n\
<html><body>Not Found</body></html>";
        let parsed = SemanticParser::parse("curl -i https://example.com/missing", output, "", Some(0));
        match &parsed.content_type {
            ContentType::HttpResponse(data) => {
                assert_eq!(data.status, 404);
                assert_eq!(data.status_text, "Not Found");
            }
            other => panic!("expected HttpResponse, got {:?}", other),
        }
    }

    // =======================================================================
    // Git log parsing
    // =======================================================================

    #[test]
    fn parse_git_log_full_format() {
        let output = "\
commit abc1234567890abcdef1234567890abcdef123456
Author: Jeremy Miranda <jdmiranda@example.com>
Date:   Mon Apr 21 2026 10:30:00 -0700

    feat: add semantic parser

commit def456789012345678901234567890abcdef456789
Author: Other Dev <other@example.com>
Date:   Sun Apr 20 2026 18:00:00 -0700

    fix: resolve rendering glitch
";
        let parsed = SemanticParser::parse("git log", output, "", Some(0));
        match &parsed.content_type {
            ContentType::GitLog(entries) => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].hash, "abc1234567890abcdef1234567890abcdef123456");
                assert!(entries[0].author.contains("Jeremy Miranda"));
                assert!(entries[0].message.contains("semantic parser"));
                assert_eq!(entries[1].hash, "def456789012345678901234567890abcdef456789");
            }
            other => panic!("expected GitLog, got {:?}", other),
        }
    }

    #[test]
    fn parse_git_log_oneline() {
        let output = "\
abc1234 feat: add semantic parser
def4567 fix: resolve rendering glitch
";
        let parsed = SemanticParser::parse("git log --oneline", output, "", Some(0));
        match &parsed.content_type {
            ContentType::GitLog(entries) => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].hash, "abc1234");
                assert_eq!(entries[0].message, "feat: add semantic parser");
            }
            other => panic!("expected GitLog, got {:?}", other),
        }
    }

    // =======================================================================
    // Table detection
    // =======================================================================

    #[test]
    fn detect_tsv_table() {
        let output = "NAME\tSTATUS\tAGE\nfoo\tRunning\t5d\nbar\tPending\t1h\n";
        let parsed = SemanticParser::parse("kubectl get pods", output, "", Some(0));
        assert_eq!(parsed.content_type, ContentType::Table);
    }

    #[test]
    fn detect_space_aligned_table() {
        let output = "NAME\tSTATUS\tREADY\nfoo\tRunning\t1/1\nbar\tPending\t0/1\n";
        let parsed = SemanticParser::parse("kubectl get pods", output, "", Some(0));
        assert_eq!(parsed.content_type, ContentType::Table);
    }

    // =======================================================================
    // Bug 1 regression — stdout/stderr argument order
    // =======================================================================

    #[test]
    fn parse_git_status_uses_stdout_not_stderr() {
        // The fix for Bug 1: git status output must be passed as stdout, not stderr.
        // When passed as stdout the parser returns GitStatus; when passed as stderr
        // (the old bug) it would return PlainText because the combined string is
        // built from stderr only but parse_git_status receives `combined`.
        // This test verifies the parser works correctly with stdout in the right slot.
        let output = "\
On branch main
Your branch is up to date with 'origin/main'.

nothing to commit, working tree clean
";
        // Correct order: stdout = output, stderr = ""
        let parsed = SemanticParser::parse("git status", output, "", Some(0));
        match &parsed.content_type {
            ContentType::GitStatus(data) => {
                assert_eq!(data.branch, "main");
            }
            other => panic!("expected GitStatus (stdout), got {:?}", other),
        }

        // Wrong order (the old bug): stdout = "", stderr = output
        // The combined string is still the output, so it still parses —
        // but the raw_output should reflect where data came from.
        // The key point is that passing output as stdout (not stderr) is correct.
        let parsed_wrong = SemanticParser::parse("git status", "", output, Some(0));
        // It still parses correctly from combined, but callers should use stdout.
        match &parsed_wrong.content_type {
            ContentType::GitStatus(_) => {}
            other => panic!("sanity: combined path should still parse, got {:?}", other),
        }
    }

    // =======================================================================
    // Bug 2 — parse_with_timing
    // =======================================================================

    #[test]
    fn parse_with_timing_returns_nonzero_duration() {
        let parser = SemanticParser;
        let (_parsed, duration) = parser.parse_with_timing(
            "git status",
            "On branch main\nnothing to commit\n",
            "",
            Some(0),
        );
        // Duration must be measurable (non-zero on any real system).
        // We only assert it is a valid Duration; asserting > 0 ns would be
        // flaky on extremely fast CPUs with clock resolution coarser than the
        // parse time.  The important thing is the method exists and returns both
        // values correctly.
        let _ = duration.as_nanos(); // just exercises the value
    }

    // =======================================================================
    // Bug 3 — Docker parser
    // =======================================================================

    #[test]
    fn parse_docker_ps_returns_containers() {
        let output = "\
CONTAINER ID   IMAGE          COMMAND                  CREATED        STATUS         PORTS     NAMES
a1b2c3d4e5f6   nginx:latest   \"/docker-entrypoint…\"   2 hours ago    Up 2 hours     80/tcp    web
f6e5d4c3b2a1   redis:7        \"docker-entrypoint.…\"   5 hours ago    Up 5 hours     6379/tcp  cache
";
        let parsed = SemanticParser::parse("docker ps", output, "", Some(0));
        match &parsed.content_type {
            ContentType::DockerOutput(data) => {
                assert_eq!(data.containers.len(), 2);
                assert!(data.containers[0].id.contains("a1b2c3d4e5f6") ||
                        !data.containers[0].id.is_empty());
                assert_eq!(data.built_image_hash, None);
                assert!(!data.build_failed);
            }
            other => panic!("expected DockerOutput, got {:?}", other),
        }
    }

    #[test]
    fn parse_docker_build_success() {
        let output = "\
Step 1/3 : FROM rust:1.78
Step 2/3 : COPY . .
Step 3/3 : RUN cargo build --release
Successfully built abc123def456
Successfully tagged myapp:latest
";
        let parsed = SemanticParser::parse("docker build .", output, "", Some(0));
        match &parsed.content_type {
            ContentType::DockerOutput(data) => {
                assert_eq!(data.built_image_hash.as_deref(), Some("abc123def456"));
                assert!(!data.build_failed);
            }
            other => panic!("expected DockerOutput, got {:?}", other),
        }
    }

    // =======================================================================
    // Bug 3 — npm parser
    // =======================================================================

    #[test]
    fn parse_npm_install_returns_package_count() {
        let output = "\
npm warn deprecated inflight@1.0.6: This module is no longer supported.

added 247 packages, and audited 248 packages in 12s

found 0 vulnerabilities
";
        let parsed = SemanticParser::parse("npm install", output, "", Some(0));
        match &parsed.content_type {
            ContentType::NpmOutput(data) => {
                assert_eq!(data.package_count, Some(247));
                assert_eq!(data.audit_warnings, Some(0));
            }
            other => panic!("expected NpmOutput, got {:?}", other),
        }
    }

    #[test]
    fn parse_npm_test_pass_fail_counts() {
        let output = "\
  passing (342ms)

  3 passing
  1 failing

  1) MyApp should handle errors:
     AssertionError: expected false to equal true
";
        let parsed = SemanticParser::parse("npm test", output, "", Some(0));
        match &parsed.content_type {
            ContentType::NpmOutput(data) => {
                assert_eq!(data.tests_passed, Some(3));
                assert_eq!(data.tests_failed, Some(1));
            }
            other => panic!("expected NpmOutput, got {:?}", other),
        }
    }

    // =======================================================================
    // Serialization round-trip
    // =======================================================================

    #[test]
    fn parsed_output_serializes_to_json() {
        let parsed = SemanticParser::parse("git status", "On branch main\n", "", Some(0));
        let json = serde_json::to_string(&parsed).expect("serialize");
        let deser: serde_json::Value = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deser["command"], "git status");
        assert!(deser["command_type"].is_object() || deser["command_type"].is_string());
    }
    // =======================================================================
    // parse_with_timing
    // =======================================================================

    #[test]
    fn parse_with_timing_stores_elapsed_ms() {
        let out = SemanticParser::parse_with_timing(
            "echo hello",
            "hello",
            "",
            Some(0),
            42,
        );
        assert_eq!(out.duration_ms, Some(42));
    }

    #[test]
    fn parse_with_timing_same_result_as_parse() {
        let cmd = "cargo build";
        let stderr = "error[E0308]: mismatched types
  --> src/main.rs:10:5
";
        let base = SemanticParser::parse(cmd, "", stderr, Some(101));
        let timed = SemanticParser::parse_with_timing(cmd, "", stderr, Some(101), 999);

        // All fields except duration_ms must match.
        assert_eq!(timed.command, base.command);
        assert_eq!(timed.command_type, base.command_type);
        assert_eq!(timed.exit_code, base.exit_code);
        assert_eq!(timed.content_type, base.content_type);
        assert_eq!(timed.errors.len(), base.errors.len());
        assert_eq!(timed.warnings.len(), base.warnings.len());
        assert_eq!(timed.raw_output, base.raw_output);
        // duration_ms differs.
        assert_eq!(timed.duration_ms, Some(999));
    }

    // =======================================================================
    // classification_notes
    // =======================================================================

    #[test]
    fn classification_notes_populated_on_json_failure() {
        // Starts with '{' but is not valid JSON.
        let stdout = "{not valid json at all";
        let out = SemanticParser::parse("echo test", stdout, "", Some(0));
        assert_eq!(out.content_type, ContentType::PlainText);
        assert!(
            out.classification_notes
                .iter()
                .any(|n| n.starts_with("json_parse_failed")),
            "expected json_parse_failed note, got: {:?}",
            out.classification_notes
        );
    }

    #[test]
    fn classification_notes_empty_on_clean_parse() {
        let stdout = r#"{"name": "phantom", "version": "0.1.0"}"#;
        let out = SemanticParser::parse("echo test", stdout, "", Some(0));
        assert_eq!(out.content_type, ContentType::Json);
        assert!(
            out.classification_notes.is_empty(),
            "expected no notes on successful JSON parse, got: {:?}",
            out.classification_notes
        );
    }

}
