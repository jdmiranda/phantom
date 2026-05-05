use std::path::Path;
use std::process::Command;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use phantom_dag::CodeDag;

use crate::detect::{
    detect_commands, detect_framework, detect_package_manager, detect_project, Framework,
    PackageManager, ProjectType,
};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Commands for common project operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectCommands {
    pub build: Option<String>,
    pub test: Option<String>,
    pub run: Option<String>,
    pub lint: Option<String>,
    pub format: Option<String>,
}

/// Git repository information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitInfo {
    pub branch: String,
    pub remote_url: Option<String>,
    pub is_dirty: bool,
    pub ahead: u32,
    pub behind: u32,
    pub last_commit_message: Option<String>,
    /// Human-readable relative time, e.g. "2 hours ago".
    pub last_commit_age: Option<String>,
}

/// Full project context assembled from filesystem detection and tool output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectContext {
    /// Absolute path to the project root.
    pub root: String,
    /// Project name (from manifest or directory name).
    pub name: String,
    pub project_type: ProjectType,
    pub package_manager: PackageManager,
    pub framework: Framework,
    pub commands: ProjectCommands,
    pub git: Option<GitInfo>,
    pub rust_version: Option<String>,
    pub node_version: Option<String>,
    pub python_version: Option<String>,
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

impl ProjectContext {
    /// Scan a directory and build the full project context.
    #[must_use]
    pub fn detect(dir: &Path) -> Self {
        let project_type = detect_project(dir);
        let package_manager = detect_package_manager(dir);
        let framework = detect_framework(dir, &project_type);
        let commands = detect_commands(dir, &project_type, &package_manager);
        let name = extract_project_name(dir, &project_type);
        let git = collect_git_info(dir);

        let rust_version = if matches!(project_type, ProjectType::Rust) {
            version_from_cmd("rustc", &["--version"])
        } else {
            None
        };
        let node_version = if matches!(project_type, ProjectType::Node) {
            version_from_cmd("node", &["--version"])
        } else {
            None
        };
        let python_version = if matches!(project_type, ProjectType::Python) {
            version_from_cmd("python3", &["--version"])
        } else {
            None
        };

        let root = dir
            .canonicalize()
            .unwrap_or_else(|_| dir.to_path_buf())
            .to_string_lossy()
            .into_owned();

        Self {
            root,
            name,
            project_type,
            package_manager,
            framework,
            commands,
            git,
            rust_version,
            node_version,
            python_version,
        }
    }

    /// Refresh only the git info (cheap operation, safe to call frequently).
    pub fn refresh_git(&mut self) {
        self.git = collect_git_info(Path::new(&self.root));
    }

    /// One-line summary suitable for a status bar.
    #[must_use]
    pub fn status_line(&self) -> String {
        let icon = match self.project_type {
            ProjectType::Rust => "rs",
            ProjectType::Node => "js",
            ProjectType::Python => "py",
            ProjectType::Go => "go",
            ProjectType::Java => "java",
            ProjectType::Ruby => "rb",
            ProjectType::Elixir => "ex",
            ProjectType::Cpp => "c++",
            ProjectType::CSharp => "c#",
            ProjectType::Swift => "swift",
            ProjectType::Unknown => "?",
        };

        let fw = match self.framework {
            Framework::None => String::new(),
            ref f => format!(" [{f:?}]"),
        };

        let branch = self
            .git
            .as_ref()
            .map(|g| format!(" ({})", g.branch))
            .unwrap_or_default();

        format!("[{icon}] {}{fw}{branch}", self.name)
    }

    /// Multi-line context string for feeding into an agent or LLM.
    #[must_use]
    pub fn agent_context(&self) -> String {
        let mut lines = Vec::new();
        lines.push(format!("Project: {}", self.name));
        lines.push(format!("Root: {}", self.root));
        lines.push(format!("Type: {:?}", self.project_type));
        lines.push(format!("Package Manager: {:?}", self.package_manager));

        if self.framework != Framework::None {
            lines.push(format!("Framework: {:?}", self.framework));
        }

        if let Some(ref v) = self.rust_version {
            lines.push(format!("Rust: {v}"));
        }
        if let Some(ref v) = self.node_version {
            lines.push(format!("Node: {v}"));
        }
        if let Some(ref v) = self.python_version {
            lines.push(format!("Python: {v}"));
        }

        let cmd_line = |label: &str, val: &Option<String>| {
            val.as_ref().map(|v| format!("  {label}: {v}"))
        };

        lines.push("Commands:".into());
        if let Some(l) = cmd_line("build", &self.commands.build) {
            lines.push(l);
        }
        if let Some(l) = cmd_line("test", &self.commands.test) {
            lines.push(l);
        }
        if let Some(l) = cmd_line("run", &self.commands.run) {
            lines.push(l);
        }
        if let Some(l) = cmd_line("lint", &self.commands.lint) {
            lines.push(l);
        }
        if let Some(l) = cmd_line("format", &self.commands.format) {
            lines.push(l);
        }

        if let Some(ref git) = self.git {
            lines.push(format!("Git branch: {}", git.branch));
            if let Some(ref url) = git.remote_url {
                lines.push(format!("Git remote: {url}"));
            }
            if git.is_dirty {
                lines.push("Git: uncommitted changes".into());
            }
            if git.ahead > 0 {
                lines.push(format!("Git: {} commit(s) ahead", git.ahead));
            }
            if git.behind > 0 {
                lines.push(format!("Git: {} commit(s) behind", git.behind));
            }
            if let Some(ref msg) = git.last_commit_message {
                let age = git
                    .last_commit_age
                    .as_deref()
                    .unwrap_or("unknown time ago");
                lines.push(format!("Last commit: {msg} ({age})"));
            }
        }

        lines.join("\n")
    }
}

// ---------------------------------------------------------------------------
// ContextAssembler
// ---------------------------------------------------------------------------

/// Builds [`ProjectContext`] with an optional cached [`CodeDag`].
///
/// For Rust projects the assembler calls [`CodeDag::from_cargo_metadata`] and
/// caches the result in `.planning/dag.json`.  On subsequent calls within the
/// same 5-minute window the file is loaded directly, avoiding a shell-out.
/// The cache is invalidated when `Cargo.toml` mtime changes.  If
/// `cargo metadata` fails the topology section is silently omitted.
#[derive(Debug, Default)]
pub struct ContextAssembler {
    dag: Option<CodeDag>,
    /// mtime of `Cargo.toml` at the time the DAG was last built.
    cargo_toml_mtime: Option<SystemTime>,
}

impl ContextAssembler {
    /// Create a new, empty assembler.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build and return a [`ProjectContext`] for `dir`, refreshing the DAG
    /// when `Cargo.toml` has changed.
    #[must_use]
    pub fn assemble(&mut self, dir: &Path) -> ProjectContext {
        let ctx = ProjectContext::detect(dir);
        if matches!(ctx.project_type, ProjectType::Rust) {
            self.refresh_dag_if_stale(dir);
        } else {
            self.dag = None;
            self.cargo_toml_mtime = None;
        }
        ctx
    }

    /// Produce the agent context string, appending a crate topology section
    /// when a DAG is available.
    #[must_use]
    pub fn agent_context(&mut self, dir: &Path) -> String {
        let ctx = self.assemble(dir);
        match &self.dag {
            Some(dag) => {
                let base = ctx.agent_context();
                match topology_section(dag) {
                    Some(t) => format!("{base}\n{t}"),
                    None => base,
                }
            }
            None => ctx.agent_context(),
        }
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    fn refresh_dag_if_stale(&mut self, dir: &Path) {
        let mtime = cargo_toml_mtime(dir);
        let stale = match (mtime, self.cargo_toml_mtime) {
            (Some(m), Some(cached)) => m != cached,
            _ => true,
        };
        if !stale {
            return;
        }

        let dag_path = dir.join(".planning").join("dag.json");

        // Serve the cache if it was written within the last 5 minutes.
        let dag_age = std::fs::metadata(&dag_path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|mt| SystemTime::now().duration_since(mt).ok());
        let cache_fresh = dag_age
            .map(|age| age < std::time::Duration::from_secs(300))
            .unwrap_or(false);

        if cache_fresh {
            match std::fs::read_to_string(&dag_path)
                .map_err(anyhow::Error::from)
                .and_then(|s| CodeDag::from_json(&s))
            {
                Ok(dag) => {
                    self.dag = Some(dag);
                    self.cargo_toml_mtime = mtime;
                    return;
                }
                Err(e) => {
                    log::debug!(
                        "phantom-context: DAG cache load failed (will rebuild): {e}"
                    );
                }
            }
        }

        // Cache miss or stale — rebuild from cargo metadata.
        match CodeDag::from_cargo_metadata() {
            Ok(dag) => {
                // Persist for future fast loads.
                if let Ok(json) = dag.to_json() {
                    let _ = std::fs::create_dir_all(dir.join(".planning"));
                    let _ = std::fs::write(&dag_path, json);
                }
                self.dag = Some(dag);
                self.cargo_toml_mtime = mtime;
            }
            Err(e) => {
                log::debug!(
                    "phantom-context: from_cargo_metadata failed (skipping topology): {e}"
                );
                // Fall back to any on-disk file even if it's stale.
                match std::fs::read_to_string(&dag_path)
                    .map_err(anyhow::Error::from)
                    .and_then(|s| CodeDag::from_json(&s))
                {
                    Ok(dag) => {
                        self.dag = Some(dag);
                        self.cargo_toml_mtime = mtime;
                    }
                    Err(_) => {
                        self.dag = None;
                        self.cargo_toml_mtime = None;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Topology section helper
// ---------------------------------------------------------------------------

/// Build a `## Crate Topology` section from the top-5 most-depended-upon
/// crates (highest in-degree).  Returns `None` when the graph has no edges.
fn topology_section(dag: &CodeDag) -> Option<String> {
    use std::collections::HashMap as Map;
    let mut in_degree: Map<&str, usize> = Map::new();
    for node in dag.nodes() {
        in_degree.entry(node.id()).or_insert(0);
    }
    for edge in dag.edges() {
        *in_degree.entry(edge.to()).or_insert(0) += 1;
    }
    if in_degree.is_empty() {
        return None;
    }
    let mut ranked: Vec<(&&str, &usize)> = in_degree.iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let top5: Vec<String> = ranked
        .into_iter()
        .take(5)
        .map(|(name, count)| format!("  {name} (in-degree: {count})"))
        .collect();
    Some(format!(
        "## Crate Topology\nTop crates by dependents:\n{}",
        top5.join("\n")
    ))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Return the `mtime` of `Cargo.toml` in `dir`, or `None` on any I/O error.
fn cargo_toml_mtime(dir: &Path) -> Option<SystemTime> {
    std::fs::metadata(dir.join("Cargo.toml"))
        .ok()
        .and_then(|m| m.modified().ok())
}

/// Extract the project name from the manifest file, falling back to dir name.
fn extract_project_name(dir: &Path, project_type: &ProjectType) -> String {
    match project_type {
        ProjectType::Rust => {
            if let Some(name) = read_toml_name(dir, "Cargo.toml") {
                return name;
            }
        }
        ProjectType::Node => {
            if let Ok(contents) = std::fs::read_to_string(dir.join("package.json"))
                && let Ok(json) = serde_json::from_str::<serde_json::Value>(&contents)
                && let Some(name) = json.get("name").and_then(|v| v.as_str())
            {
                return name.to_string();
            }
        }
        ProjectType::Python => {
            if let Some(name) = read_toml_name(dir, "pyproject.toml") {
                return name;
            }
        }
        ProjectType::Go => {
            if let Ok(contents) = std::fs::read_to_string(dir.join("go.mod"))
                && let Some(line) = contents.lines().next()
                && let Some(module) = line.strip_prefix("module ")
            {
                return module.trim().to_string();
            }
        }
        ProjectType::Elixir => {
            if let Ok(contents) = std::fs::read_to_string(dir.join("mix.exs")) {
                // Look for `app: :name`
                for line in contents.lines() {
                    let trimmed = line.trim();
                    if let Some(rest) = trimmed.strip_prefix("app:") {
                        let name = rest.trim().trim_start_matches(':').trim_matches(',');
                        if !name.is_empty() {
                            return name.to_string();
                        }
                    }
                }
            }
        }
        _ => {}
    }

    // Fallback: directory name.
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".into())
}

/// Simple TOML `name` extraction without pulling in a TOML parser.
/// Looks for `name = "..."` in the first section (before any `[` header
/// after the first).
fn read_toml_name(dir: &Path, filename: &str) -> Option<String> {
    let contents = std::fs::read_to_string(dir.join(filename)).ok()?;
    for line in contents.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("name") {
            let rest = rest.trim();
            if let Some(rest) = rest.strip_prefix('=') {
                let rest = rest.trim().trim_matches('"');
                if !rest.is_empty() {
                    return Some(rest.to_string());
                }
            }
        }
    }
    None
}

/// Run a command and return trimmed stdout, or `None` on any failure.
fn run_git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Collect git repository info, returning `None` if not inside a repo.
fn collect_git_info(dir: &Path) -> Option<GitInfo> {
    let branch = run_git(dir, &["branch", "--show-current"])?;

    let remote_url = run_git(dir, &["remote", "get-url", "origin"]);

    let is_dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    let ahead = run_git(dir, &["rev-list", "--count", "@{u}..HEAD"])
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);

    let behind = run_git(dir, &["rev-list", "--count", "HEAD..@{u}"])
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);

    let last_commit_message = run_git(dir, &["log", "-1", "--format=%s"]);
    let last_commit_age = run_git(dir, &["log", "-1", "--format=%cr"]);

    Some(GitInfo {
        branch,
        remote_url,
        is_dirty,
        ahead,
        behind,
        last_commit_message,
        last_commit_age,
    })
}

/// Run a version command and extract the trimmed output.
fn version_from_cmd(cmd: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tmp() -> TempDir {
        TempDir::new().unwrap()
    }

    #[test]
    fn project_context_for_rust_project() {
        let dir = tmp();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"my-crate\"\nversion = \"0.1.0\"",
        )
        .unwrap();

        let ctx = ProjectContext::detect(dir.path());

        assert_eq!(ctx.name, "my-crate");
        assert_eq!(ctx.project_type, ProjectType::Rust);
        assert_eq!(ctx.package_manager, PackageManager::Cargo);
        assert_eq!(ctx.commands.build.as_deref(), Some("cargo build"));
    }

    #[test]
    fn project_context_for_node_nextjs() {
        let dir = tmp();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"my-app","dependencies":{"next":"14","react":"18"},"scripts":{"build":"next build","dev":"next dev"}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("pnpm-lock.yaml"), "").unwrap();

        let ctx = ProjectContext::detect(dir.path());

        assert_eq!(ctx.name, "my-app");
        assert_eq!(ctx.project_type, ProjectType::Node);
        assert_eq!(ctx.package_manager, PackageManager::Pnpm);
        assert_eq!(ctx.framework, Framework::NextJs);
        assert_eq!(ctx.commands.build.as_deref(), Some("pnpm build"));
        assert_eq!(ctx.commands.run.as_deref(), Some("pnpm dev"));
    }

    #[test]
    fn project_context_for_python_django() {
        let dir = tmp();
        std::fs::write(dir.path().join("requirements.txt"), "django==4.2\ncelery\n").unwrap();
        std::fs::write(dir.path().join("manage.py"), "#!/usr/bin/env python").unwrap();

        let ctx = ProjectContext::detect(dir.path());

        assert_eq!(ctx.project_type, ProjectType::Python);
        assert_eq!(ctx.framework, Framework::Django);
        assert_eq!(
            ctx.commands.run.as_deref(),
            Some("python manage.py runserver")
        );
    }

    #[test]
    fn status_line_format() {
        let ctx = ProjectContext {
            root: "/tmp/test".into(),
            name: "phantom".into(),
            project_type: ProjectType::Rust,
            package_manager: PackageManager::Cargo,
            framework: Framework::Axum,
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
        };

        assert_eq!(ctx.status_line(), "[rs] phantom [Axum] (main)");
    }

    #[test]
    fn agent_context_contains_key_fields() {
        let ctx = ProjectContext {
            root: "/home/dev/myapp".into(),
            name: "myapp".into(),
            project_type: ProjectType::Go,
            package_manager: PackageManager::GoMod,
            framework: Framework::None,
            commands: ProjectCommands {
                build: Some("go build ./...".into()),
                test: Some("go test ./...".into()),
                run: Some("go run .".into()),
                lint: None,
                format: None,
            },
            git: None,
            rust_version: None,
            node_version: None,
            python_version: None,
        };

        let s = ctx.agent_context();
        assert!(s.contains("Project: myapp"));
        assert!(s.contains("Type: Go"));
        assert!(s.contains("Package Manager: GoMod"));
        assert!(s.contains("build: go build ./..."));
    }

    #[test]
    fn extract_name_from_package_json() {
        let dir = tmp();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"cool-app","version":"1.0"}"#,
        )
        .unwrap();

        let name = extract_project_name(dir.path(), &ProjectType::Node);
        assert_eq!(name, "cool-app");
    }

    #[test]
    fn extract_name_falls_back_to_dir() {
        let dir = tmp();
        // No manifest — should use the directory name.
        let name = extract_project_name(dir.path(), &ProjectType::Unknown);
        // TempDir names are random, just ensure it's non-empty.
        assert!(!name.is_empty());
    }

    #[test]
    fn git_info_in_real_repo() {
        // Build a fresh repo in a tempdir so the test is hermetic and works in
        // CI / agent worktrees where the host repo may have a detached HEAD.
        let dir = tmp();
        let path = dir.path();

        // Skip cleanly if `git` isn't on PATH (e.g. minimal sandbox).
        let Ok(init) = Command::new("git")
            .args(["init", "-q", "-b", "phantom-test"])
            .current_dir(path)
            .status()
        else {
            return;
        };
        if !init.success() {
            return;
        }

        // Configure a local identity so `git commit` works without global config.
        for args in [
            ["config", "user.email", "test@phantom.local"].as_slice(),
            ["config", "user.name", "Phantom Test"].as_slice(),
        ] {
            Command::new("git")
                .args(args)
                .current_dir(path)
                .status()
                .expect("git config");
        }

        std::fs::write(path.join("README.md"), "hello").unwrap();
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(path)
            .status()
            .expect("git add");
        Command::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(path)
            .status()
            .expect("git commit");

        let info = collect_git_info(path).expect("collect_git_info on fresh repo");
        assert_eq!(info.branch, "phantom-test");
        assert!(!info.is_dirty);
        assert_eq!(info.ahead, 0);
        assert_eq!(info.behind, 0);
    }

    #[test]
    fn git_info_returns_none_on_detached_head() {
        // Detached HEAD has no current branch — `collect_git_info` should bail
        // out cleanly rather than panicking or returning a bogus branch name.
        let dir = tmp();
        let path = dir.path();

        let Ok(init) = Command::new("git")
            .args(["init", "-q", "-b", "phantom-test"])
            .current_dir(path)
            .status()
        else {
            return;
        };
        if !init.success() {
            return;
        }

        for args in [
            ["config", "user.email", "test@phantom.local"].as_slice(),
            ["config", "user.name", "Phantom Test"].as_slice(),
        ] {
            Command::new("git")
                .args(args)
                .current_dir(path)
                .status()
                .expect("git config");
        }

        std::fs::write(path.join("README.md"), "hello").unwrap();
        Command::new("git")
            .args(["add", "README.md"])
            .current_dir(path)
            .status()
            .expect("git add");
        Command::new("git")
            .args(["commit", "-q", "-m", "init"])
            .current_dir(path)
            .status()
            .expect("git commit");

        // Detach HEAD at the current commit.
        Command::new("git")
            .args(["checkout", "-q", "--detach", "HEAD"])
            .current_dir(path)
            .status()
            .expect("git checkout --detach");

        // `git branch --show-current` is empty on detached HEAD, so we currently
        // return None. The contract: don't panic, don't fabricate a branch.
        assert!(collect_git_info(path).is_none());
    }
}
