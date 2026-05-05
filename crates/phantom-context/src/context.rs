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
/// For Rust projects, the assembler runs `cargo metadata` once and caches the
/// resulting [`CodeDag`].  The cache is invalidated whenever the `mtime` of
/// `Cargo.toml` in the workspace root changes.  If `cargo metadata` fails (not
/// a Rust project, `cargo` not on `PATH`, etc.) the topology section is
/// silently omitted.
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
            let dag_path = dir.join(".planning/dag.json");
            match std::fs::read_to_string(&dag_path).map_err(anyhow::Error::from).and_then(|s| CodeDag::from_json(&s)) {
                Ok(dag) => {
                    self.dag = Some(dag);
                    self.cargo_toml_mtime = mtime;
                }
                Err(e) => {
                    log::debug!("phantom-context: DAG load failed (skipping topology): {e}");
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
        // Build a small hand-constructed DAG:
        //   phantom-core depended on by phantom-app and phantom-brain (in-degree 2)
        //   phantom-app  in-degree 0
        //   phantom-brain in-degree 0
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

        // Build a ProjectContext directly (no subprocess) and call the inner method.
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

        // Pass no DAG — simulates DAG build failure or non-Rust project.
        let output = ctx.agent_context_with_dag(None);
        assert!(
            !output.contains("## Crate Topology"),
            "unexpected '## Crate Topology' in non-Rust output:\n{output}"
        );
    }

    /// The top-5 list must be sorted by descending in-degree.
    #[test]
    fn top_five_depended_crates_sorted_by_in_degree() {
        let mut dag = CodeDag::new();
        let node = |name: &str| {
            DagNode::new(name.to_owned(), NodeKind::Module, PathBuf::from("Cargo.toml"), 1)
        };

        // Create 7 crates.
        let crates = ["a", "b", "c", "d", "e", "f", "g"];
        for name in crates {
            dag.add_node(node(name));
        }

        // Give 'a' in-degree 6, 'b' in-degree 5, ..., 'g' in-degree 0.
        // "a" is depended on by b, c, d, e, f, g  → in-degree 6
        // "b" is depended on by c, d, e, f, g       → in-degree 5
        // "c" is depended on by d, e, f, g            → in-degree 4
        // "d" is depended on by e, f, g               → in-degree 3
        // "e" is depended on by f, g                  → in-degree 2
        // "f" is depended on by g                     → in-degree 1
        // "g" has no dependents                        → in-degree 0
        let depends_on = [
            ("b", "a"),
            ("c", "a"),
            ("d", "a"),
            ("e", "a"),
            ("f", "a"),
            ("g", "a"),
            ("c", "b"),
            ("d", "b"),
            ("e", "b"),
            ("f", "b"),
            ("g", "b"),
            ("d", "c"),
            ("e", "c"),
            ("f", "c"),
            ("g", "c"),
            ("e", "d"),
            ("f", "d"),
            ("g", "d"),
            ("f", "e"),
            ("g", "e"),
            ("g", "f"),
        ];
        for (from, to) in depends_on {
            dag.add_edge(DagEdge::new(from.to_owned(), to.to_owned(), EdgeKind::Uses));
        }

        let section = topology_section(&dag).expect("topology_section must return Some");

        // The section must list the crates in order a, b, c, d, e (top 5).
        // Verify positional ordering by finding each name's byte offset.
        let pos_a = section.find("a (in-degree").expect("'a' must appear");
        let pos_b = section.find("b (in-degree").expect("'b' must appear");
        let pos_c = section.find("c (in-degree").expect("'c' must appear");
        let pos_d = section.find("d (in-degree").expect("'d' must appear");
        let pos_e = section.find("e (in-degree").expect("'e' must appear");

        assert!(
            pos_a < pos_b && pos_b < pos_c && pos_c < pos_d && pos_d < pos_e,
            "crates not in descending in-degree order in:\n{section}"
        );

        // 'f' and 'g' must NOT appear (only top 5).
        assert!(!section.contains("f (in-degree"), "f must not appear (outside top 5)");
        assert!(!section.contains("g (in-degree"), "g must not appear (outside top 5)");
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
