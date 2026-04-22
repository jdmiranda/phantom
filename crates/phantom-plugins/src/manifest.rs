use serde::{Deserialize, Serialize};

/// Plugin manifest (loaded from plugin.toml or plugin.json).
///
/// Every plugin ships a manifest describing its identity, capabilities,
/// required permissions, and the events it reacts to.
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
    pub entry_point: String,
    #[serde(default)]
    pub permissions: Vec<Permission>,
    #[serde(default)]
    pub hooks: Vec<HookType>,
    #[serde(default)]
    pub commands: Vec<CommandDef>,
    #[serde(default)]
    pub status_bar: Option<StatusBarDef>,
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

/// A command that a plugin registers with Phantom.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandDef {
    pub name: String,
    pub description: String,
    pub usage: String,
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

    /// Basic validation — name non-empty, version non-empty, entry_point non-empty.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.name.is_empty() {
            anyhow::bail!("plugin name must not be empty");
        }
        if self.version.is_empty() {
            anyhow::bail!("plugin version must not be empty");
        }
        if self.entry_point.is_empty() {
            anyhow::bail!("plugin entry_point must not be empty");
        }
        Ok(())
    }

    /// Check whether this plugin requests a given permission.
    pub fn has_permission(&self, perm: &Permission) -> bool {
        self.permissions.contains(perm)
    }

    /// Returns true if this plugin defines a status-bar widget.
    pub fn has_status_bar(&self) -> bool {
        self.status_bar.is_some()
    }

    /// Check whether this plugin hooks into the given event type.
    pub fn listens_for(&self, hook: &HookType) -> bool {
        self.hooks.iter().any(|h| hook_matches(h, hook))
    }
}

/// Returns true when a plugin's registered hook matches an incoming event.
///
/// For `OnCommand`, matching uses a simple glob: `*` matches any substring.
/// All other variants match by discriminant (the inner data is ignored on the
/// registration side for `OnInterval`).
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
            }],
            status_bar: Some(StatusBarDef {
                position: StatusBarPosition::Right,
                update_interval_ms: 5000,
            }),
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
