pub mod errors;
pub mod parser;
pub mod types;

pub use errors::*;
pub use parser::*;
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
        let output = "\
CONTAINER ID   IMAGE          COMMAND       STATUS
abc123         nginx:latest   nginx -g      Up 5 hours
def456         redis:7        redis-srv     Up 2 hours
";
        let parsed = SemanticParser::parse("docker ps", output, "", Some(0));
        // Docker is classified as Docker, and the output falls through to
        // fallback which detects the table.
        assert_eq!(parsed.content_type, ContentType::Table);
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
}
