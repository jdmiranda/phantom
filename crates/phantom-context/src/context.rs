use std::collections::HashMap;
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
    ///
    /// When called via [`ContextAssembler`], a `## Crate Topology` section is
    /// appended for Rust projects.  When called directly on a bare
    /// [`ProjectContext`] there is no cached DAG, so the topology section is
    /// omitted.
    #[must_use]
    pub fn agent_context(&self) -> String {
        self.agent_context_with_dag(None)
    }

    /// Inner implementation that accepts an optional pre-built [`CodeDag`].
    #[must_use]
    pub(crate) fn agent_context_with_dag(&self, dag: Option<&CodeDag>) -> String {
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

        // Append crate topology when a DAG is available.
        if let Some(d) = dag
            && let Some(section) = topology_section(d)
        {
            lines.push(section);
        }

        lines.join("\n")
    }
}

// ---------------------------------------------------------------------------
// ContextAssembler
// ---------------------------------------------------------------------------

/// Builds [`ProjectContext`] with an optional cached [`CodeDag`].
///
/// For Rust projects, the assembler loads `.planning/dag.json` once and caches
/// the resulting [`CodeDag`].  The cache is invalidated whenever the `mtime` of
/// `Cargo.toml` in the workspace root changes.  If the file does not exist or
/// fails to parse, the topology section is silently omitted.
#[derive(Debug, Default)]
pub struct ContextAssembler {
    pub(crate) dag: Option<CodeDag>,
    /// mtime of `Cargo.toml` at the time the DAG was last built.
    cargo_toml_mtime: Option<SystemTime>,
}

impl ContextAssembler {
    /// Create a new, empty assembler.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a [`ProjectContext`] for `dir`, refreshing the DAG cache when
    /// `Cargo.toml` has changed.
    #[must_use]
    pub fn assemble(&mut self, dir: &Path) -> ProjectContext {
        let ctx = ProjectContext::detect(dir);

        if matches!(ctx.project_type, ProjectType::Rust) {
            self.refresh_dag_if_stale(dir);
        } else {
            // Non-Rust project: drop any stale cached DAG.
            self.dag = None;
            self.cargo_toml_mtime = None;
        }

        ctx
    }

    /// Produce the agent context string, including the topology section when a
    /// DAG is available.
    #[must_use]
    pub fn agent_context(&mut self, dir: &Path) -> String {
        let ctx = self.assemble(dir);
        ctx.agent_context_with_dag(self.dag.as_ref())
    }

    /// Return the upstream (dependencies) and downstream (dependents) neighbours
    /// of `crate_name` from the cached DAG as a short human-readable string.
    ///
    /// Returns `None` when no DAG has been built yet or the crate is not found.
    #[must_use]
    pub fn crate_summary(&self, crate_name: &str) -> Option<String> {
        let dag = self.dag.as_ref()?;

        // Check the crate exists in the DAG.
        dag.get_node(crate_name)?;

        // Upstream: edges where crate_name is the *from* end (crate depends on these).
        let upstream: Vec<&str> = dag
            .edges()
            .filter(|e| e.from() == crate_name)
            .map(|e| e.to())
            .collect();

        // Downstream: edges where crate_name is the *to* end (these depend on crate_name).
        let downstream: Vec<&str> = dag
            .edges()
            .filter(|e| e.to() == crate_name)
            .map(|e| e.from())
            .collect();

        let mut parts = Vec::new();
        if upstream.is_empty() {
            parts.push("  depends on: (none)".to_owned());
        } else {
            parts.push(format!("  depends on: {}", upstream.join(", ")));
        }
        if downstream.is_empty() {
            parts.push("  depended on by: (none)".to_owned());
        } else {
            parts.push(format!("  depended on by: {}", downstream.join(", ")));
        }

        Some(format!("{}:\n{}", crate_name, parts.join("\n")))
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
        if stale {
            // Load the DAG from the project's `.planning/dag.json` file if it
            // exists.  `phantom-dag` exposes `CodeDag::from_json`; a planning
            // tool is responsible for writing that file — the assembler only
            // consumes it.
            let dag_path = dir.join(".planning").join("dag.json");
            match std::fs::read_to_string(&dag_path)
                .map_err(anyhow::Error::from)
                .and_then(|s| CodeDag::from_json(&s))
            {
                Ok(dag) => {
                    self.dag = Some(dag);
                    self.cargo_toml_mtime = mtime;
                }
                Err(e) => {
                    log::debug!(
                        "phantom-context: DAG load from {} failed (skipping topology): {e}",
                        dag_path.display()
                    );
                    self.dag = None;
                    self.cargo_toml_mtime = None;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Topology section helper
// ---------------------------------------------------------------------------

/// Build the `## Crate Topology` markdown section from a [`CodeDag`].
///
/// Lists the top 5 crates by in-degree (most depended-upon) in descending
/// order.  Returns `None` when the DAG has no edges.
fn topology_section(dag: &CodeDag) -> Option<String> {
    // Count in-degree: how many edges point *to* each node.
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    for node in dag.nodes() {
        in_degree.entry(node.id()).or_insert(0);
    }
    for edge in dag.edges() {
        *in_degree.entry(edge.to()).or_insert(0) += 1;
    }

    if in_degree.is_empty() {
        return None;
    }

    // Sort by descending in-degree, then alphabetically for determinism.
    let mut ranked: Vec<(&&str, &usize)> = in_degree.iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));

    let top5: Vec<String> = ranked
        .into_iter()
        .take(5)
        .map(|(name, count)| format!("  {name} (in-degree: {count})"))
        .collect();

    Some(format!("## Crate Topology\nTop crates by dependents:\n{}", top5.join("\n")))
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

/// Spawn a git subprocess and wait up to `timeout_ms` milliseconds for it to
/// finish.  Returns trimmed stdout on success, or `None` on any failure
/// (including timeout, non-zero exit, or invalid UTF-8).
///
/// Kills the child process when the deadline expires so the caller never
/// blocks indefinitely on a slow or hung filesystem.
fn run_git_with_timeout(dir: &Path, args: &[&str], timeout_ms: u64) -> Option<String> {
    use std::io::Read;
    use std::time::{Duration, Instant};

    let mut child = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = child.stdout.take()?;
                let mut buf = String::new();
                stdout.read_to_string(&mut buf).ok()?;
                if !status.success() {
                    return None;
                }
                let s = buf.trim().to_string();
                return if s.is_empty() { None } else { Some(s) };
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    log::debug!(
                        "phantom-context: git {:?} timed out after {}ms — killed",
                        args,
                        timeout_ms
                    );
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => {
                let _ = child.kill();
                return None;
            }
        }
    }
}

/// Run a git command and return trimmed stdout, or `None` on any failure.
///
/// Uses a 2-second timeout so slow or hung git processes never stall the
/// OODA loop thread.
fn run_git(dir: &Path, args: &[&str]) -> Option<String> {
    run_git_with_timeout(dir, args, 2_000)
}

/// Collect git repository info, returning `None` only when not inside any git repo.
///
/// In detached HEAD state (`git branch --show-current` returns empty),
/// the branch is synthesised as `"HEAD@<short-hash>"` so agent prompts
/// keep git context even when bisecting, rebasing, or checking out a tag.
fn collect_git_info(dir: &Path) -> Option<GitInfo> {
    // `git branch --show-current` returns an empty string (and exit 0) in
    // detached HEAD state. We distinguish that from "not a git repo" (where
    // git exits non-zero / isn't on PATH) by falling through to `rev-parse`.
    let raw_branch = run_git_with_timeout(dir, &["branch", "--show-current"], 2_000);

    let branch = match raw_branch {
        Some(b) if !b.is_empty() => b,
        _ => {
            // Either detached HEAD (git succeeded but output was empty) or
            // not a repo at all.  rev-parse fails in the non-repo case,
            // propagating None and aborting the whole function.
            let hash = run_git_with_timeout(dir, &["rev-parse", "--short", "HEAD"], 2_000)?;
            format!("HEAD@{hash}")
        }
    };

    let remote_url = run_git(dir, &["remote", "get-url", "origin"]);

    // Use the timeout wrapper for `git status --porcelain` too so a frozen
    // NFS mount can't stall the OODA loop.  The wrapper returns None only
    // when git failed; an empty stdout means a clean working tree.
    let is_dirty = run_git_with_timeout(dir, &["status", "--porcelain"], 2_000)
        .map(|s| !s.is_empty())
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
    use phantom_dag::{DagEdge, DagNode, EdgeKind, NodeKind};
    use std::path::PathBuf;

    use super::*;
    use tempfile::TempDir;

    fn tmp() -> TempDir {
        TempDir::new().unwrap()
    }

    // -----------------------------------------------------------------------
    // ContextAssembler / topology tests
    // -----------------------------------------------------------------------

    /// Build a hand-constructed DAG with a known in-degree distribution and
    /// verify that the topology section appears in the agent context output.
    #[test]
    fn topology_section_included_for_rust_project() {
        let mut dag = CodeDag::new();
        let node = |name: &str| {
            DagNode::new(name.to_owned(), NodeKind::Module, PathBuf::from("Cargo.toml"), 1)
        };
        dag.add_node(node("phantom-core"));
        dag.add_node(node("phantom-app"));
        dag.add_node(node("phantom-brain"));
        dag.add_edge(DagEdge::new(
            "phantom-app".to_owned(),
            "phantom-core".to_owned(),
            EdgeKind::Uses,
        ));
        dag.add_edge(DagEdge::new(
            "phantom-brain".to_owned(),
            "phantom-core".to_owned(),
            EdgeKind::Uses,
        ));

        let ctx = ProjectContext {
            root: "/tmp/phantom".into(),
            name: "phantom".into(),
            project_type: ProjectType::Rust,
            package_manager: crate::detect::PackageManager::Cargo,
            framework: crate::detect::Framework::None,
            commands: ProjectCommands {
                build: None,
                test: None,
                run: None,
                lint: None,
                format: None,
            },
            git: None,
            rust_version: None,
            node_version: None,
            python_version: None,
        };

        let output = ctx.agent_context_with_dag(Some(&dag));
        assert!(
            output.contains("## Crate Topology"),
            "expected '## Crate Topology' in output:\n{output}"
        );
        assert!(
            output.contains("phantom-core"),
            "expected phantom-core (highest in-degree) in topology section:\n{output}"
        );
    }

    /// For a non-Rust project (or when no DAG is available), the topology
    /// section must be absent.
    #[test]
    fn topology_section_absent_for_non_rust_project() {
        let ctx = ProjectContext {
            root: "/tmp/myapp".into(),
            name: "myapp".into(),
            project_type: ProjectType::Node,
            package_manager: crate::detect::PackageManager::Npm,
            framework: crate::detect::Framework::None,
            commands: ProjectCommands {
                build: None,
                test: None,
                run: None,
                lint: None,
                format: None,
            },
            git: None,
            rust_version: None,
            node_version: None,
            python_version: None,
        };

        let output = ctx.agent_context_with_dag(None);
        assert!(
            !output.contains("## Crate Topology"),
            "unexpected '## Crate Topology' in non-Rust output:\n{output}"
        );
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
        let name = extract_project_name(dir.path(), &ProjectType::Unknown);
        assert!(!name.is_empty());
    }

    #[test]
    fn git_info_in_real_repo() {
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

        let info = collect_git_info(path).expect("collect_git_info on fresh repo");
        assert_eq!(info.branch, "phantom-test");
        assert!(!info.is_dirty);
        assert_eq!(info.ahead, 0);
        assert_eq!(info.behind, 0);
    }

    /// Detached HEAD now returns `Some(GitInfo)` with a synthetic branch name
    /// of the form `"HEAD@<short-hash>"` rather than `None`, so agents always
    /// receive git context even when bisecting or checking out a tag.
    #[test]
    fn collect_git_info_detached_head_returns_synthetic_branch() {
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

        Command::new("git")
            .args(["checkout", "-q", "--detach", "HEAD"])
            .current_dir(path)
            .status()
            .expect("git checkout --detach");

        // The new contract: detached HEAD returns Some with a synthetic branch.
        let info =
            collect_git_info(path).expect("collect_git_info must return Some on detached HEAD");
        assert!(
            info.branch.starts_with("HEAD@"),
            "synthetic branch must start with 'HEAD@', got: {}",
            info.branch
        );
        let hash_part = info.branch.trim_start_matches("HEAD@");
        assert!(hash_part.len() >= 4, "short hash too short: {}", hash_part);
    }

    /// Calling `assemble()` twice on the same dir reuses the cached DAG (no
    /// second load) and returns an identical context.
    #[test]
    fn context_assembler_caches_on_second_call() {
        let dir = tmp();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"cache-test\"\nversion = \"0.1.0\"",
        )
        .unwrap();

        let mut assembler = ContextAssembler::new();

        let ctx1 = assembler.assemble(dir.path());
        let dag_present_after_first = assembler.dag.is_some();

        let ctx2 = assembler.assemble(dir.path());

        // The dag presence flag should not change between calls with same mtime.
        assert_eq!(
            assembler.dag.is_some(),
            dag_present_after_first,
            "DAG cache should not flip between calls with the same mtime"
        );

        assert_eq!(ctx1.name, ctx2.name);
        assert_eq!(ctx1.project_type, ctx2.project_type);
    }

    /// A git command that would take longer than the timeout returns `None`
    /// instead of blocking.  The critical assertion is that the wall-clock
    /// time never exceeds 2 seconds — the timeout is always honoured.
    #[test]
    fn run_git_with_timeout_returns_none_on_slow_process() {
        use std::time::{Duration, Instant};

        let dir = tmp();
        let path = dir.path();

        // Only meaningful when git is available.
        let Ok(init) = Command::new("git")
            .args(["init", "-q"])
            .current_dir(path)
            .status()
        else {
            return;
        };
        if !init.success() {
            return;
        }

        let start = Instant::now();
        // 1 ms timeout — almost certainly will not complete, returning None.
        let _result = run_git_with_timeout(path, &["status", "--porcelain"], 1);
        let elapsed = start.elapsed();

        // The most critical assertion: timeout is always ≤ 2 seconds.
        assert!(
            elapsed < Duration::from_secs(2),
            "run_git_with_timeout blocked for {:?}, expected < 2s",
            elapsed
        );
    }
}
