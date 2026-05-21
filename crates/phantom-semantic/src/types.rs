use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Command classification
// ---------------------------------------------------------------------------

/// What kind of command was executed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CommandType {
    Git(GitCommand),
    Cargo(CargoCommand),
    Docker(DockerCommand),
    Npm(NpmCommand),
    Http(HttpCommand),
    Shell,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum GitCommand {
    Status,
    Log,
    Diff,
    Push,
    Pull,
    Commit,
    Branch,
    Checkout,
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CargoCommand {
    Build,
    Test,
    Run,
    Check,
    Clippy,
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum DockerCommand {
    Ps,
    Images,
    Logs,
    Build,
    Compose,
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum NpmCommand {
    Install,
    Test,
    Run,
    Build,
    Other(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum HttpCommand {
    Get,
    Post,
    Put,
    Delete,
    Other(String),
}

// ---------------------------------------------------------------------------
// Error detection
// ---------------------------------------------------------------------------

/// A detected error in command output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DetectedError {
    pub message: String,
    pub error_type: ErrorType,
    /// Source file path, if extractable.
    pub file: Option<String>,
    /// Line number in source.
    pub line: Option<usize>,
    /// Column number in source.
    pub column: Option<usize>,
    /// Error code (e.g. `E0308`).
    pub code: Option<String>,
    pub severity: Severity,
    /// The original output line that triggered detection.
    pub raw_line: String,
    /// Compiler/tool suggestion, if available.
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ErrorType {
    Compiler,
    Runtime,
    Test,
    Http,
    Permission,
    NotFound,
    Syntax,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Severity {
    Error,
    Warning,
    Info,
}

// ---------------------------------------------------------------------------
// Content types (for rich rendering)
// ---------------------------------------------------------------------------

/// What kind of content the output represents.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ContentType {
    PlainText,
    Json,
    /// Tabular data (TSV/CSV detected).
    Table,
    GitStatus(GitStatusData),
    GitLog(Vec<GitLogEntry>),
    GitDiff,
    /// Compiler output with structured errors.
    CompilerOutput,
    TestResults(TestSummary),
    HttpResponse(HttpResponseData),
    /// Parsed `docker ps` or `docker build` output.
    DockerOutput(DockerOutputData),
    /// Parsed `npm install` or `npm test` output.
    NpmOutput(NpmOutputData),
}

/// Parsed `git status` output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitStatusData {
    pub branch: String,
    pub upstream: Option<String>,
    pub modified: Vec<String>,
    pub staged: Vec<String>,
    pub untracked: Vec<String>,
    pub ahead: u32,
    pub behind: u32,
}

/// A single entry from `git log` output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitLogEntry {
    pub hash: String,
    pub author: String,
    pub date: String,
    pub message: String,
}

/// Aggregated test results.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TestSummary {
    pub passed: u32,
    pub failed: u32,
    pub ignored: u32,
    pub total: u32,
    pub failures: Vec<String>,
}

/// Parsed HTTP response metadata (e.g. from curl -i).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HttpResponseData {
    pub status: u16,
    pub status_text: String,
    pub content_type: Option<String>,
    pub body_preview: String,
}

/// A single container row from `docker ps`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DockerContainer {
    pub id: String,
    pub name: String,
    pub status: String,
    pub ports: String,
}

/// Parsed Docker command output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DockerOutputData {
    /// Containers listed by `docker ps`.
    pub containers: Vec<DockerContainer>,
    /// Image hash from a successful `docker build` (e.g. `sha256:abc123`).
    pub built_image_hash: Option<String>,
    /// True when the output contains a build error.
    pub build_failed: bool,
}

/// Parsed npm command output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NpmOutputData {
    /// Number of packages added/updated during `npm install`.
    pub package_count: Option<u32>,
    /// Number of security warnings from the audit summary.
    pub audit_warnings: Option<u32>,
    /// Test pass count from `npm test`.
    pub tests_passed: Option<u32>,
    /// Test fail count from `npm test`.
    pub tests_failed: Option<u32>,
}

// ---------------------------------------------------------------------------
// The top-level parsed output
// ---------------------------------------------------------------------------

/// The fully parsed output of a command execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedOutput {
    /// Raw command string as typed by the user.
    pub command: String,
    pub command_type: CommandType,
    pub exit_code: Option<i32>,
    pub content_type: ContentType,
    pub errors: Vec<DetectedError>,
    pub warnings: Vec<DetectedError>,
    pub duration_ms: Option<u64>,
    /// Full raw stdout+stderr concatenated.
    pub raw_output: String,
}
