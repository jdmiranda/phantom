use std::sync::OnceLock;

use phantom_context::ProjectContext;
use regex::Regex;

// ---------------------------------------------------------------------------
// Static regex patterns — compiled once via OnceLock — safe to call on the hot path
// ---------------------------------------------------------------------------

static RE_BINARY_TOKEN: OnceLock<Regex> = OnceLock::new();
static RE_TEST: OnceLock<Regex> = OnceLock::new();
static RE_RUN: OnceLock<Regex> = OnceLock::new();
static RE_LINT: OnceLock<Regex> = OnceLock::new();
static RE_FMT: OnceLock<Regex> = OnceLock::new();
static RE_CHANGES: OnceLock<Regex> = OnceLock::new();
static RE_STATUS: OnceLock<Regex> = OnceLock::new();
static RE_DEPLOY: OnceLock<Regex> = OnceLock::new();
static RE_FIX: OnceLock<Regex> = OnceLock::new();
static RE_EXPLAIN: OnceLock<Regex> = OnceLock::new();
static RE_PERF: OnceLock<Regex> = OnceLock::new();
static RE_SHOW: OnceLock<Regex> = OnceLock::new();
static RE_CI: OnceLock<Regex> = OnceLock::new();

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// The resolved action from natural language input.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedAction {
    /// Run a specific shell command.
    RunCommand(String),

    /// Show information (no command to run, just display).
    ShowInfo(String),

    /// Spawn an agent with a task description.
    SpawnAgent(String),

    /// Ambiguous input — need clarification from the user.
    Ambiguous { input: String, options: Vec<String> },

    /// Not recognized as natural language — pass through to shell as-is.
    PassThrough,
}

// ---------------------------------------------------------------------------
// Interpreter
// ---------------------------------------------------------------------------

/// Natural language command interpreter.
///
/// This is NOT a chatbot. It maps natural language to concrete shell actions
/// using pattern matching against known project commands. When in doubt,
/// it returns `PassThrough` — better to let the shell handle it than to
/// misinterpret intent.
pub struct NlpInterpreter;

impl NlpInterpreter {
    /// Try to interpret natural language input as an action.
    ///
    /// Returns `PassThrough` if the input doesn't match any known pattern
    /// or looks like a real shell command.
    pub fn interpret(input: &str, ctx: &ProjectContext) -> ResolvedAction {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return ResolvedAction::PassThrough;
        }

        // --- Stage 1: hard pass-through for obvious shell commands ----------
        if is_shell_command(trimmed) {
            return ResolvedAction::PassThrough;
        }

        // --- Stage 2: single-word known binaries ---------------------------
        if is_known_binary(trimmed) {
            return ResolvedAction::PassThrough;
        }

        let lower = trimmed.to_lowercase();

        // --- Stage 3: project command patterns -----------------------------
        if let Some(action) = match_project_commands(&lower, ctx) {
            return action;
        }

        // --- Stage 4: git / VCS patterns -----------------------------------
        if let Some(action) = match_git_patterns(&lower) {
            return action;
        }

        // --- Stage 5: deploy patterns --------------------------------------
        if let Some(action) = match_deploy_patterns(&lower, trimmed) {
            return action;
        }

        // --- Stage 6: agent delegation patterns ----------------------------
        if let Some(action) = match_agent_patterns(&lower) {
            return action;
        }

        // --- Stage 7: nothing matched — pass through -----------------------
        ResolvedAction::PassThrough
    }

    /// Heuristic: does the input look like natural language rather than a
    /// shell command?
    ///
    /// Returns `true` when the input contains spaces and starts with a
    /// common English verb/question word, and does NOT contain shell
    /// operators.
    pub fn is_natural_language(input: &str) -> bool {
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return false;
        }

        // Shell-like prefixes — not natural language.
        if trimmed.starts_with('/')
            || trimmed.starts_with("./")
            || trimmed.starts_with("~/")
            || trimmed.starts_with('.')
        {
            return false;
        }

        // Shell operators — not natural language.
        if trimmed.contains('|')
            || trimmed.contains('>')
            || trimmed.contains("&&")
            || trimmed.contains("||")
        {
            return false;
        }

        // Single word that looks like a binary — not natural language.
        if !trimmed.contains(' ') && is_known_binary(trimmed) {
            return false;
        }

        // Single well-known NLP verbs (build, test, run, etc.) are natural
        // language even without spaces.
        const SINGLE_WORD_NLP: &[&str] = &[
            "build", "test", "lint", "format", "fmt", "run", "start",
            "deploy", "status", "check", "explain",
        ];
        if SINGLE_WORD_NLP.contains(&trimmed.to_lowercase().as_str()) {
            return true;
        }

        // Needs at least one space for multi-word natural language.
        if !trimmed.contains(' ') {
            return false;
        }

        let first_word = trimmed
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_lowercase();

        const NL_STARTERS: &[&str] = &[
            "what", "show", "is", "do", "run", "build", "test", "deploy",
            "fix", "explain", "check", "find", "search", "list", "tell",
            "how", "why", "where", "when", "which", "can", "could",
            "please", "help", "start", "stop", "restart", "lint", "format",
            "something", "recent", "get", "set", "open", "close",
        ];

        NL_STARTERS.contains(&first_word.as_str())
    }
}

// ---------------------------------------------------------------------------
// Internal matchers
// ---------------------------------------------------------------------------

/// Returns true if the input looks like a direct shell command.
fn is_shell_command(input: &str) -> bool {
    // Starts with path-like prefixes.
    if input.starts_with('/')
        || input.starts_with("./")
        || input.starts_with("~/")
    {
        return true;
    }

    // Contains shell operators.
    if input.contains('|')
        || input.contains('>')
        || input.contains("&&")
        || input.contains("||")
    {
        return true;
    }

    // Starts with common shell builtins / flags.
    if input.starts_with('-') || input.starts_with("sudo ") {
        return true;
    }

    false
}

/// A conservative list of single-word tokens that are almost certainly
/// intended as executable binaries, not natural language requests.
fn is_known_binary(word: &str) -> bool {
    // Must be a single token of lowercase alpha (maybe with hyphens/underscores).
    // Compiled once via OnceLock — safe to call on the hot path
    let re = RE_BINARY_TOKEN
        .get_or_init(|| Regex::new(r"^[a-z][a-z0-9_-]*$").expect("RE_BINARY_TOKEN: invalid pattern"));
    if !re.is_match(word) {
        return false;
    }

    const BINARIES: &[&str] = &[
        "git", "ls", "cat", "cd", "cp", "mv", "rm", "mkdir", "rmdir",
        "pwd", "echo", "grep", "find", "sed", "awk", "curl", "wget",
        "ssh", "scp", "tar", "zip", "unzip", "man", "which", "whoami",
        "ps", "top", "htop", "kill", "killall", "df", "du", "free",
        "uname", "env", "export", "source", "chmod", "chown", "head",
        "tail", "less", "more", "sort", "uniq", "wc", "diff", "patch",
        "make", "cmake", "gcc", "g++", "clang", "rustc", "rustup",
        "cargo", "node", "npm", "npx", "yarn", "pnpm", "bun", "deno",
        "python", "python3", "pip", "pip3", "poetry", "uv",
        "go", "java", "javac", "mvn", "gradle",
        "ruby", "gem", "bundle", "rake",
        "docker", "docker-compose", "kubectl", "helm",
        "terraform", "ansible", "vagrant",
        "vi", "vim", "nvim", "nano", "emacs", "code",
        "tmux", "screen", "zsh", "bash", "sh", "fish",
        "xargs", "tee", "touch", "file", "stat", "ln", "readlink",
        "nmap", "nc", "netstat", "ss", "ip", "ifconfig", "ping",
        "traceroute", "dig", "nslookup", "host",
    ];

    BINARIES.contains(&word)
}

/// Match against project commands: build, test, run, lint, format.
fn match_project_commands(lower: &str, ctx: &ProjectContext) -> Option<ResolvedAction> {
    // Build
    if matches!(lower, "build" | "build it" | "build the project" | "compile") {
        return ctx
            .commands
            .build
            .as_ref()
            .map(|cmd| ResolvedAction::RunCommand(cmd.clone()));
    }

    // Test — compiled once via OnceLock — safe to call on the hot path
    let test_re = RE_TEST.get_or_init(|| {
        Regex::new(r"^(?:test|tests|run\s+(?:the\s+)?tests?)$")
            .expect("RE_TEST: invalid pattern")
    });
    if test_re.is_match(lower) {
        return ctx
            .commands
            .test
            .as_ref()
            .map(|cmd| ResolvedAction::RunCommand(cmd.clone()));
    }

    // Run / start — compiled once via OnceLock — safe to call on the hot path
    let run_re = RE_RUN.get_or_init(|| {
        Regex::new(
            r"^(?:run|start|run\s+it|start\s+it|run\s+the\s+(?:app|project|server)|start\s+the\s+(?:app|project|server))$",
        )
        .expect("RE_RUN: invalid pattern")
    });
    if run_re.is_match(lower) {
        return ctx
            .commands
            .run
            .as_ref()
            .map(|cmd| ResolvedAction::RunCommand(cmd.clone()));
    }

    // Lint / check — compiled once via OnceLock — safe to call on the hot path
    let lint_re = RE_LINT.get_or_init(|| {
        Regex::new(r"^(?:lint|check|run\s+(?:the\s+)?linter?)$")
            .expect("RE_LINT: invalid pattern")
    });
    if lint_re.is_match(lower) {
        return ctx
            .commands
            .lint
            .as_ref()
            .map(|cmd| ResolvedAction::RunCommand(cmd.clone()));
    }

    // Format — compiled once via OnceLock — safe to call on the hot path
    let fmt_re = RE_FMT.get_or_init(|| {
        Regex::new(r"^(?:format|fmt|format\s+(?:the\s+)?code|run\s+(?:the\s+)?formatter)$")
            .expect("RE_FMT: invalid pattern")
    });
    if fmt_re.is_match(lower) {
        return ctx
            .commands
            .format
            .as_ref()
            .map(|cmd| ResolvedAction::RunCommand(cmd.clone()));
    }

    None
}

/// Match git and VCS related patterns.
fn match_git_patterns(lower: &str) -> Option<ResolvedAction> {
    // "what changed" / "what changed today" / "recent changes"
    // Compiled once via OnceLock — safe to call on the hot path
    let changes_re = RE_CHANGES.get_or_init(|| {
        Regex::new(
            r"^(?:what(?:'s|\s+has)?\s+changed(?:\s+today|\s+recently|\s+lately)?|recent\s+changes|show\s+(?:recent\s+)?changes)$",
        )
        .expect("RE_CHANGES: invalid pattern")
    });
    if changes_re.is_match(lower) {
        return Some(ResolvedAction::RunCommand(
            "git log --oneline -10".to_string(),
        ));
    }

    // "status" / "git status" / "what's the status"
    // Compiled once via OnceLock — safe to call on the hot path
    let status_re = RE_STATUS.get_or_init(|| {
        Regex::new(r"^(?:status|git\s+status|what(?:'s|\s+is)\s+the\s+status)$")
            .expect("RE_STATUS: invalid pattern")
    });
    if status_re.is_match(lower) {
        return Some(ResolvedAction::RunCommand("git status".to_string()));
    }

    None
}

/// Match deploy patterns, returning Ambiguous when there's no clear target.
fn match_deploy_patterns(lower: &str, _original: &str) -> Option<ResolvedAction> {
    // Compiled once via OnceLock — safe to call on the hot path
    let deploy_re = RE_DEPLOY.get_or_init(|| {
        Regex::new(r"^deploy(?:\s+(?:to\s+)?(staging|production|prod|dev|preview))?$")
            .expect("RE_DEPLOY: invalid pattern")
    });

    if let Some(caps) = deploy_re.captures(lower) {
        if let Some(target) = caps.get(1) {
            let env = match target.as_str() {
                "prod" => "production",
                other => other,
            };
            return Some(ResolvedAction::RunCommand(format!("deploy {env}")));
        }
        // Bare "deploy" with no target — ambiguous.
        return Some(ResolvedAction::Ambiguous {
            input: lower.to_string(),
            options: vec![
                "deploy staging".to_string(),
                "deploy production".to_string(),
            ],
        });
    }

    None
}

/// Match patterns that should delegate to an AI agent.
fn match_agent_patterns(lower: &str) -> Option<ResolvedAction> {
    // "fix it" / "fix the error" / "fix this"
    // Compiled once via OnceLock — safe to call on the hot path
    let fix_re = RE_FIX.get_or_init(|| {
        Regex::new(r"^fix\s+(?:it|this|the\s+(?:error|bug|issue|problem|failure))$")
            .expect("RE_FIX: invalid pattern")
    });
    if fix_re.is_match(lower) {
        return Some(ResolvedAction::SpawnAgent(
            "fix the last error".to_string(),
        ));
    }

    // "explain" / "explain this" / "what happened"
    // Compiled once via OnceLock — safe to call on the hot path
    let explain_re = RE_EXPLAIN.get_or_init(|| {
        Regex::new(r"^(?:explain(?:\s+(?:this|it|that))?|what\s+happened)$")
            .expect("RE_EXPLAIN: invalid pattern")
    });
    if explain_re.is_match(lower) {
        return Some(ResolvedAction::SpawnAgent(
            "explain the last output".to_string(),
        ));
    }

    // "something feels slow" / "it's slow" / "performance" / "why is it slow"
    // Compiled once via OnceLock — safe to call on the hot path
    let perf_re = RE_PERF.get_or_init(|| {
        Regex::new(
            r"(?:feels?\s+slow|it(?:'s|\s+is)\s+slow|performance|why\s+is\s+it\s+slow|diagnose\s+performance)",
        )
        .expect("RE_PERF: invalid pattern")
    });
    if perf_re.is_match(lower) {
        return Some(ResolvedAction::SpawnAgent(
            "diagnose performance".to_string(),
        ));
    }

    // "show me X" / "what is X" — general agent delegation.
    // Compiled once via OnceLock — safe to call on the hot path
    let show_re = RE_SHOW.get_or_init(|| {
        Regex::new(r"^(?:show\s+me|what\s+is|tell\s+me\s+about)\s+(.+)$")
            .expect("RE_SHOW: invalid pattern")
    });
    if let Some(caps) = show_re.captures(lower) {
        let topic = caps.get(1).expect("RE_SHOW capture group 1").as_str().to_string();
        return Some(ResolvedAction::SpawnAgent(topic));
    }

    // "is CI green" / "CI status" / "check CI"
    // Compiled once via OnceLock — safe to call on the hot path
    let ci_re = RE_CI.get_or_init(|| {
        Regex::new(r"^(?:is\s+ci\s+green|ci\s+status|check\s+ci)$")
            .expect("RE_CI: invalid pattern")
    });
    if ci_re.is_match(lower) {
        return Some(ResolvedAction::ShowInfo("CI status check".to_string()));
    }

    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_context::{
        Framework, GitInfo, PackageManager, ProjectCommands, ProjectContext, ProjectType,
    };

    /// Build a Rust project context for testing.
    fn rust_ctx() -> ProjectContext {
        ProjectContext {
            root: "/tmp/my-rust-project".into(),
            name: "my-crate".into(),
            project_type: ProjectType::Rust,
            package_manager: PackageManager::Cargo,
            framework: Framework::None,
            commands: ProjectCommands {
                build: Some("cargo build".into()),
                test: Some("cargo test".into()),
                run: Some("cargo run".into()),
                lint: Some("cargo clippy".into()),
                format: Some("cargo fmt".into()),
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
            rust_version: Some("1.79.0".into()),
            node_version: None,
            python_version: None,
        }
    }

    /// Build a Node project context for testing.
    fn node_ctx() -> ProjectContext {
        ProjectContext {
            root: "/tmp/my-node-app".into(),
            name: "my-app".into(),
            project_type: ProjectType::Node,
            package_manager: PackageManager::Pnpm,
            framework: Framework::NextJs,
            commands: ProjectCommands {
                build: Some("pnpm build".into()),
                test: Some("pnpm test".into()),
                run: Some("pnpm dev".into()),
                lint: Some("pnpm lint".into()),
                format: Some("pnpm format".into()),
            },
            git: None,
            rust_version: None,
            node_version: Some("20.11.0".into()),
            python_version: None,
        }
    }

    // --- Project command tests -----------------------------------------------

    #[test]
    fn build_resolves_to_rust_command() {
        let ctx = rust_ctx();
        assert_eq!(
            NlpInterpreter::interpret("build", &ctx),
            ResolvedAction::RunCommand("cargo build".into()),
        );
    }

    #[test]
    fn build_resolves_to_node_command() {
        let ctx = node_ctx();
        assert_eq!(
            NlpInterpreter::interpret("build", &ctx),
            ResolvedAction::RunCommand("pnpm build".into()),
        );
    }

    #[test]
    fn test_keyword_resolves() {
        let ctx = rust_ctx();
        assert_eq!(
            NlpInterpreter::interpret("test", &ctx),
            ResolvedAction::RunCommand("cargo test".into()),
        );
    }

    #[test]
    fn run_the_tests_resolves() {
        let ctx = node_ctx();
        assert_eq!(
            NlpInterpreter::interpret("run the tests", &ctx),
            ResolvedAction::RunCommand("pnpm test".into()),
        );
    }

    #[test]
    fn run_tests_resolves() {
        let ctx = rust_ctx();
        assert_eq!(
            NlpInterpreter::interpret("run tests", &ctx),
            ResolvedAction::RunCommand("cargo test".into()),
        );
    }

    #[test]
    fn run_resolves_to_project_run() {
        let ctx = rust_ctx();
        assert_eq!(
            NlpInterpreter::interpret("run", &ctx),
            ResolvedAction::RunCommand("cargo run".into()),
        );
    }

    #[test]
    fn start_resolves_to_project_run() {
        let ctx = node_ctx();
        assert_eq!(
            NlpInterpreter::interpret("start", &ctx),
            ResolvedAction::RunCommand("pnpm dev".into()),
        );
    }

    #[test]
    fn lint_resolves() {
        let ctx = rust_ctx();
        assert_eq!(
            NlpInterpreter::interpret("lint", &ctx),
            ResolvedAction::RunCommand("cargo clippy".into()),
        );
    }

    #[test]
    fn format_resolves() {
        let ctx = node_ctx();
        assert_eq!(
            NlpInterpreter::interpret("format", &ctx),
            ResolvedAction::RunCommand("pnpm format".into()),
        );
    }

    #[test]
    fn fmt_resolves_to_format() {
        let ctx = rust_ctx();
        assert_eq!(
            NlpInterpreter::interpret("fmt", &ctx),
            ResolvedAction::RunCommand("cargo fmt".into()),
        );
    }

    // --- Git patterns --------------------------------------------------------

    #[test]
    fn what_changed_resolves_to_git_log() {
        let ctx = rust_ctx();
        assert_eq!(
            NlpInterpreter::interpret("what changed", &ctx),
            ResolvedAction::RunCommand("git log --oneline -10".into()),
        );
    }

    #[test]
    fn what_changed_today_resolves_to_git_log() {
        let ctx = rust_ctx();
        assert_eq!(
            NlpInterpreter::interpret("what changed today", &ctx),
            ResolvedAction::RunCommand("git log --oneline -10".into()),
        );
    }

    #[test]
    fn status_resolves_to_git_status() {
        let ctx = rust_ctx();
        assert_eq!(
            NlpInterpreter::interpret("status", &ctx),
            ResolvedAction::RunCommand("git status".into()),
        );
    }

    // --- Deploy patterns -----------------------------------------------------

    #[test]
    fn deploy_staging_resolves() {
        let ctx = rust_ctx();
        assert_eq!(
            NlpInterpreter::interpret("deploy staging", &ctx),
            ResolvedAction::RunCommand("deploy staging".into()),
        );
    }

    #[test]
    fn bare_deploy_is_ambiguous() {
        let ctx = rust_ctx();
        let result = NlpInterpreter::interpret("deploy", &ctx);
        match result {
            ResolvedAction::Ambiguous { options, .. } => {
                assert!(options.contains(&"deploy staging".to_string()));
                assert!(options.contains(&"deploy production".to_string()));
            }
            other => panic!("Expected Ambiguous, got {other:?}"),
        }
    }

    // --- Agent delegation patterns -------------------------------------------

    #[test]
    fn fix_it_spawns_agent() {
        let ctx = rust_ctx();
        assert_eq!(
            NlpInterpreter::interpret("fix the error", &ctx),
            ResolvedAction::SpawnAgent("fix the last error".into()),
        );
    }

    #[test]
    fn explain_spawns_agent() {
        let ctx = rust_ctx();
        assert_eq!(
            NlpInterpreter::interpret("explain", &ctx),
            ResolvedAction::SpawnAgent("explain the last output".into()),
        );
    }

    #[test]
    fn what_happened_spawns_agent() {
        let ctx = rust_ctx();
        assert_eq!(
            NlpInterpreter::interpret("what happened", &ctx),
            ResolvedAction::SpawnAgent("explain the last output".into()),
        );
    }

    #[test]
    fn something_feels_slow_spawns_agent() {
        let ctx = rust_ctx();
        assert_eq!(
            NlpInterpreter::interpret("something feels slow", &ctx),
            ResolvedAction::SpawnAgent("diagnose performance".into()),
        );
    }

    #[test]
    fn show_me_delegates_to_agent() {
        let ctx = rust_ctx();
        assert_eq!(
            NlpInterpreter::interpret("show me the config", &ctx),
            ResolvedAction::SpawnAgent("the config".into()),
        );
    }

    #[test]
    fn ci_status_shows_info() {
        let ctx = rust_ctx();
        assert_eq!(
            NlpInterpreter::interpret("is CI green", &ctx),
            ResolvedAction::ShowInfo("CI status check".into()),
        );
    }

    // --- PassThrough tests ---------------------------------------------------

    #[test]
    fn shell_pipe_passes_through() {
        let ctx = rust_ctx();
        assert_eq!(
            NlpInterpreter::interpret("cat foo.txt | grep bar", &ctx),
            ResolvedAction::PassThrough,
        );
    }

    #[test]
    fn shell_redirect_passes_through() {
        let ctx = rust_ctx();
        assert_eq!(
            NlpInterpreter::interpret("echo hello > out.txt", &ctx),
            ResolvedAction::PassThrough,
        );
    }

    #[test]
    fn shell_and_operator_passes_through() {
        let ctx = rust_ctx();
        assert_eq!(
            NlpInterpreter::interpret("make && make install", &ctx),
            ResolvedAction::PassThrough,
        );
    }

    #[test]
    fn path_command_passes_through() {
        let ctx = rust_ctx();
        assert_eq!(
            NlpInterpreter::interpret("./scripts/deploy.sh", &ctx),
            ResolvedAction::PassThrough,
        );
    }

    #[test]
    fn absolute_path_passes_through() {
        let ctx = rust_ctx();
        assert_eq!(
            NlpInterpreter::interpret("/usr/bin/env python3", &ctx),
            ResolvedAction::PassThrough,
        );
    }

    #[test]
    fn known_binary_passes_through() {
        let ctx = rust_ctx();
        assert_eq!(
            NlpInterpreter::interpret("git", &ctx),
            ResolvedAction::PassThrough,
        );
        assert_eq!(
            NlpInterpreter::interpret("ls", &ctx),
            ResolvedAction::PassThrough,
        );
        assert_eq!(
            NlpInterpreter::interpret("docker", &ctx),
            ResolvedAction::PassThrough,
        );
    }

    #[test]
    fn empty_input_passes_through() {
        let ctx = rust_ctx();
        assert_eq!(
            NlpInterpreter::interpret("", &ctx),
            ResolvedAction::PassThrough,
        );
    }

    // --- is_natural_language tests -------------------------------------------

    #[test]
    fn is_nl_detects_multi_word_phrases() {
        assert!(NlpInterpreter::is_natural_language("run the tests"));
        assert!(NlpInterpreter::is_natural_language("what changed today"));
        assert!(NlpInterpreter::is_natural_language("show me the logs"));
        assert!(NlpInterpreter::is_natural_language("fix the error"));
    }

    #[test]
    fn is_nl_detects_single_word_verbs() {
        assert!(NlpInterpreter::is_natural_language("build"));
        assert!(NlpInterpreter::is_natural_language("test"));
        assert!(NlpInterpreter::is_natural_language("deploy"));
    }

    #[test]
    fn is_nl_rejects_shell_commands() {
        assert!(!NlpInterpreter::is_natural_language("./deploy.sh"));
        assert!(!NlpInterpreter::is_natural_language("/usr/bin/env"));
        assert!(!NlpInterpreter::is_natural_language("cat foo | grep bar"));
        assert!(!NlpInterpreter::is_natural_language("echo > out.txt"));
        assert!(!NlpInterpreter::is_natural_language("a && b"));
    }

    #[test]
    fn is_nl_rejects_known_binaries() {
        assert!(!NlpInterpreter::is_natural_language("git"));
        assert!(!NlpInterpreter::is_natural_language("ls"));
        assert!(!NlpInterpreter::is_natural_language("docker"));
        assert!(!NlpInterpreter::is_natural_language("curl"));
    }

    // --- Context-aware resolution edge cases ---------------------------------

    #[test]
    fn no_build_command_returns_passthrough() {
        let mut ctx = rust_ctx();
        ctx.commands.build = None;
        // If the project has no build command, "build" should not resolve.
        assert_eq!(
            NlpInterpreter::interpret("build", &ctx),
            ResolvedAction::PassThrough,
        );
    }

    #[test]
    fn deploy_production_normalizes_prod() {
        let ctx = rust_ctx();
        assert_eq!(
            NlpInterpreter::interpret("deploy prod", &ctx),
            ResolvedAction::RunCommand("deploy production".into()),
        );
    }
}
