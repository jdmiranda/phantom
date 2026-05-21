use serde::{Deserialize, Serialize};

/// Plugin manifest (loaded from plugin.toml or plugin.json).
///
/// Every plugin ships a manifest describing its identity, capabilities,
/// required permissions, and the events it reacts to.
///
/// ## Scaffold manifests
///
/// When a plugin is installed via the marketplace scaffold path (no real WASM
/// binary was downloaded), `scaffold` is `true`. A scaffold plugin can be
/// discovered and enumerated but **cannot be executed** — the WASM runtime will
/// reject it because no usable binary exists. Check `self.is_scaffold()` before
/// attempting to load a plugin into the runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub homepage: Option<String>,
    /// Relative path to the compiled WASM module (e.g. "plugin.wasm").
    /// For scaffold installs this is an empty string — no real binary exists.
    #[serde(default)]
    pub entry_point: String,
    #[serde(default)]
    pub permissions: Vec<Permission>,
    #[serde(default)]
    pub hooks: Vec<HookType>,
    #[serde(default)]
    pub commands: Vec<CommandDef>,
    #[serde(default)]
    pub status_bar: Option<StatusBarDef>,
    /// `true` when this manifest was produced by the marketplace scaffold path.
    /// No real WASM binary is present; the plugin cannot be executed until a
    /// real artifact is installed (tracked by issue #48).
    #[serde(default)]
    pub scaffold: bool,
}

/// Permission a plugin can request. The host must grant each permission
/// explicitly before the plugin can exercise it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Permission {
    ReadFiles,
    WriteFiles,
    RunCommands,
    Network,
    StatusBar,
    Notifications,
}

/// Events a plugin can hook into.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HookType {
    /// React to a specific command pattern (glob-style, e.g. "git *").
    OnCommand(String),
    /// React to any command output.
    OnOutput,
    /// React to command errors.
    OnError,
    /// Runs once at terminal startup.
    OnStartup,
    /// Runs once at terminal shutdown.
    OnShutdown,
    /// Periodic timer, interval in seconds.
    OnInterval(u64),
}

impl HookType {
    /// Returns the [`Permission`] required before a plugin may receive this
    /// hook, or `None` when the hook is always allowed.
    ///
    /// # Policy
    ///
    /// - Lifecycle hooks (`OnStartup`, `OnShutdown`, `OnInterval`) require no
    ///   privilege — they convey timing, not data.
    /// - Command/output hooks (`OnCommand`, `OnOutput`) require
    ///   [`Permission::RunCommands`] because they expose the user's command
    ///   stream and its output.
    /// - `OnError` is **intentionally** allowed without a permission. Error
    ///   hooks may include a command name + exit code (similar surface to
    ///   `OnOutput`), but Phantom treats failure reporting as essential
    ///   plumbing that all plugins should be able to observe (e.g. for
    ///   diagnostics widgets). If you tighten this in the future, prefer a new
    ///   `Permission::ObserveErrors` variant rather than reusing `RunCommands`,
    ///   so the privilege surface stays self-documenting.
    #[must_use]
    pub fn required_permission(&self) -> Option<Permission> {
        match self {
            HookType::OnStartup | HookType::OnShutdown | HookType::OnInterval(_) => None,
            HookType::OnCommand(_) | HookType::OnOutput => Some(Permission::RunCommands),
            HookType::OnError => None,
        }
    }
}

/// A command that a plugin registers with Phantom.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandDef {
    pub name: String,
    pub description: String,
    pub usage: String,
    /// `true` when this command may modify files on disk. When set, the plugin
    /// must declare `Permission::WriteFiles` in addition to `Permission::RunCommands`
    /// or the executor will reject the call. Defaults to `false` so existing
    /// manifests remain backward compatible.
    #[serde(default)]
    pub write_access: bool,
}

/// Configuration for a status-bar widget provided by a plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusBarDef {
    pub position: StatusBarPosition,
    pub update_interval_ms: u64,
}

/// Where a plugin's status-bar segment appears.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum StatusBarPosition {
    Left,
    Center,
    Right,
}

impl PluginManifest {
    /// Parse a manifest from TOML text.
    pub fn from_toml(s: &str) -> anyhow::Result<Self> {
        let manifest: Self = toml::from_str(s)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Parse a manifest from JSON text.
    pub fn from_json(s: &str) -> anyhow::Result<Self> {
        let manifest: Self = serde_json::from_str(s)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Basic validation — name non-empty, version non-empty.
    ///
    /// For scaffold manifests (`scaffold = true`) the `entry_point` is allowed
    /// to be empty because no real WASM binary has been downloaded yet.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.name.is_empty() {
            anyhow::bail!("plugin name must not be empty");
        }
        if self.version.is_empty() {
            anyhow::bail!("plugin version must not be empty");
        }
        if !self.scaffold && self.entry_point.is_empty() {
            anyhow::bail!(
                "plugin entry_point must not be empty (set scaffold = true if this is a \
                 placeholder install)"
            );
        }
        Ok(())
    }

    /// Returns `true` when this manifest was produced by the marketplace
    /// scaffold path and no real WASM binary is available. The plugin cannot
    /// be executed until a real artifact is installed (issue #48).
    #[must_use]
    pub fn is_scaffold(&self) -> bool {
        self.scaffold
    }

    /// Check whether this plugin requests a given permission.
    #[must_use]
    pub fn has_permission(&self, perm: &Permission) -> bool {
        self.permissions.contains(perm)
    }

    /// Returns true if this plugin defines a status-bar widget.
    #[must_use]
    pub fn has_status_bar(&self) -> bool {
        self.status_bar.is_some()
    }

    /// Check whether this plugin hooks into the given event type.
    #[must_use]
    pub fn listens_for(&self, hook: &HookType) -> bool {
        self.hooks.iter().any(|h| hook_matches(h, hook))
    }
}

/// Returns true when a plugin's registered hook matches an incoming event.
///
/// For `OnCommand`, matching uses a simple glob: `*` matches any substring.
/// All other variants match by discriminant (the inner data is ignored on the
/// registration side for `OnInterval`).
#[must_use]
pub fn hook_matches(registered: &HookType, incoming: &HookType) -> bool {
    match (registered, incoming) {
        (HookType::OnCommand(pattern), HookType::OnCommand(cmd)) => {
            simple_glob(pattern, cmd)
        }
        (HookType::OnOutput, HookType::OnOutput) => true,
        (HookType::OnError, HookType::OnError) => true,
        (HookType::OnStartup, HookType::OnStartup) => true,
        (HookType::OnShutdown, HookType::OnShutdown) => true,
        (HookType::OnInterval(_), HookType::OnInterval(_)) => true,
        _ => false,
    }
}

/// Minimal glob matching: supports `*` as a wildcard for any substring, and
/// literal characters otherwise. Good enough for command patterns like "git *".
fn simple_glob(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }

    let mut pos = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match text[pos..].find(part) {
            Some(idx) => {
                // First segment must match at start.
                if i == 0 && idx != 0 {
                    return false;
                }
                pos += idx + part.len();
            }
            None => return false,
        }
    }

    // If pattern doesn't end with `*`, text must end exactly.
    if !pattern.ends_with('*') {
        return text.ends_with(parts.last().unwrap_or(&""));
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> PluginManifest {
        PluginManifest {
            name: "git-insights".into(),
            version: "0.1.0".into(),
            description: "Git analytics plugin".into(),
            author: "phantom".into(),
            license: Some("MIT".into()),
            homepage: None,
            entry_point: "plugin.wasm".into(),
            permissions: vec![Permission::ReadFiles, Permission::RunCommands],
            hooks: vec![
                HookType::OnCommand("git *".into()),
                HookType::OnStartup,
            ],
            commands: vec![CommandDef {
                name: "git-stats".into(),
                description: "Show git statistics".into(),
                usage: "git-stats [--verbose]".into(),
                write_access: false,
            }],
            status_bar: Some(StatusBarDef {
                position: StatusBarPosition::Right,
                update_interval_ms: 5000,
            }),
            scaffold: false,
        }
    }

    #[test]
    fn manifest_roundtrip_json() {
        let m = sample_manifest();
        let json = serde_json::to_string_pretty(&m).unwrap();
        let parsed = PluginManifest::from_json(&json).unwrap();
        assert_eq!(parsed.name, "git-insights");
        assert_eq!(parsed.commands.len(), 1);
    }

    #[test]
    fn manifest_roundtrip_toml() {
        let m = sample_manifest();
        let toml_str = toml::to_string_pretty(&m).unwrap();
        let parsed = PluginManifest::from_toml(&toml_str).unwrap();
        assert_eq!(parsed.name, "git-insights");
        assert_eq!(parsed.permissions.len(), 2);
    }

    #[test]
    fn manifest_validation_rejects_empty_name() {
        let mut m = sample_manifest();
        m.name = String::new();
        assert!(m.validate().is_err());
    }

    #[test]
    fn manifest_validation_rejects_empty_version() {
        let mut m = sample_manifest();
        m.version = String::new();
        assert!(m.validate().is_err());
    }

    #[test]
    fn manifest_validation_rejects_empty_entry_point() {
        let mut m = sample_manifest();
        m.entry_point = String::new();
        assert!(m.validate().is_err());
    }

    #[test]
    fn manifest_validation_allows_empty_entry_point_when_scaffold() {
        let mut m = sample_manifest();
        m.scaffold = true;
        m.entry_point = String::new();
        assert!(m.validate().is_ok());
    }

    #[test]
    fn has_permission_positive() {
        let m = sample_manifest();
        assert!(m.has_permission(&Permission::ReadFiles));
    }

    #[test]
    fn has_permission_negative() {
        let m = sample_manifest();
        assert!(!m.has_permission(&Permission::Network));
    }

    #[test]
    fn has_status_bar() {
        let m = sample_manifest();
        assert!(m.has_status_bar());
    }

    #[test]
    fn listens_for_matching_command() {
        let m = sample_manifest();
        assert!(m.listens_for(&HookType::OnCommand("git commit".into())));
    }

    #[test]
    fn listens_for_non_matching_command() {
        let m = sample_manifest();
        assert!(!m.listens_for(&HookType::OnCommand("cargo build".into())));
    }

    #[test]
    fn simple_glob_exact() {
        assert!(simple_glob("hello", "hello"));
        assert!(!simple_glob("hello", "world"));
    }

    #[test]
    fn simple_glob_wildcard() {
        assert!(simple_glob("git *", "git commit"));
        assert!(simple_glob("git *", "git push --force"));
        assert!(!simple_glob("git *", "cargo build"));
    }

    #[test]
    fn simple_glob_star_only() {
        assert!(simple_glob("*", "anything at all"));
    }

    #[test]
    fn simple_glob_prefix_star() {
        assert!(simple_glob("*.rs", "main.rs"));
        assert!(!simple_glob("*.rs", "main.py"));
    }
}
