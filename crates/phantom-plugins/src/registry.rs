use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use log;

use crate::host::{HookContext, HookResponse, PluginRuntime};
use crate::manifest::{hook_matches, HookType, PluginManifest};

// ---------------------------------------------------------------------------
// Loaded plugin
// ---------------------------------------------------------------------------

/// A plugin that has been loaded into the host, paired with its runtime.
pub struct LoadedPlugin {
    pub manifest: PluginManifest,
    pub runtime: Box<dyn PluginRuntime>,
    pub enabled: bool,
}

// ---------------------------------------------------------------------------
// Plugin info (read-only summary)
// ---------------------------------------------------------------------------

/// Lightweight summary of a plugin, suitable for display in a list view.
#[derive(Debug, Clone)]
pub struct PluginInfo {
    pub name: String,
    pub version: String,
    pub description: String,
    pub enabled: bool,
    pub hooks: usize,
    pub commands: usize,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Manages installed plugins — scanning, loading, dispatching hooks, running
/// commands, and querying status-bar widgets.
pub struct PluginRegistry {
    plugins: Vec<LoadedPlugin>,
    plugin_dir: PathBuf,
}

impl PluginRegistry {
    /// Create a new registry rooted at the default plugin directory
    /// (`~/.config/phantom/plugins/`). The directory is created if it does
    /// not exist.
    pub fn new() -> Result<Self> {
        let dir = dirs_or_default();
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create plugin dir: {}", dir.display()))?;
        Ok(Self {
            plugins: Vec::new(),
            plugin_dir: dir,
        })
    }

    /// Create a registry with an explicit plugin directory.
    pub fn with_dir(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create plugin dir: {}", dir.display()))?;
        Ok(Self {
            plugins: Vec::new(),
            plugin_dir: dir,
        })
    }

    /// Create an empty registry with no plugin directory. Plugins are disabled.
    pub fn empty() -> Self {
        Self {
            plugins: Vec::new(),
            plugin_dir: PathBuf::new(),
        }
    }

    /// The directory this registry scans for plugins.
    pub fn plugin_dir(&self) -> &Path {
        &self.plugin_dir
    }

    /// Scan the plugin directory and load all manifests. This does **not**
    /// initialise runtimes — call [`load_plugin`] for each plugin you want to
    /// activate.
    ///
    /// Returns the number of manifests successfully loaded.
    pub fn scan(&mut self) -> Result<Vec<PluginManifest>> {
        let mut manifests = Vec::new();
        let entries = fs::read_dir(&self.plugin_dir)
            .with_context(|| format!("cannot read plugin dir: {}", self.plugin_dir.display()))?;

        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            match load_manifest(&path) {
                Ok(manifest) => {
                    log::info!("found plugin: {} v{}", manifest.name, manifest.version);
                    manifests.push(manifest);
                }
                Err(e) => {
                    log::warn!(
                        "skipping {}: {e:#}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    );
                }
            }
        }

        Ok(manifests)
    }

    /// Load a specific plugin from a directory, pairing it with the given
    /// runtime. The runtime is initialised immediately.
    pub fn load_plugin(
        &mut self,
        path: &Path,
        mut runtime: Box<dyn PluginRuntime>,
    ) -> Result<()> {
        let manifest = load_manifest(path)?;
        runtime
            .init(&manifest)
            .with_context(|| format!("failed to init plugin '{}'", manifest.name))?;

        log::info!("loaded plugin: {} v{}", manifest.name, manifest.version);
        self.plugins.push(LoadedPlugin {
            manifest,
            runtime,
            enabled: true,
        });
        Ok(())
    }

    /// Load a plugin directly from a manifest and runtime (no filesystem
    /// lookup). Useful for testing and programmatic registration.
    pub fn register(
        &mut self,
        manifest: PluginManifest,
        mut runtime: Box<dyn PluginRuntime>,
    ) -> Result<()> {
        runtime
            .init(&manifest)
            .with_context(|| format!("failed to init plugin '{}'", manifest.name))?;

        self.plugins.push(LoadedPlugin {
            manifest,
            runtime,
            enabled: true,
        });
        Ok(())
    }

    /// All currently loaded plugins (enabled and disabled).
    pub fn plugins(&self) -> &[LoadedPlugin] {
        &self.plugins
    }

    /// Dispatch a hook event to every enabled plugin that registered for it.
    /// Returns all non-`None` responses.
    pub fn dispatch_hook(
        &mut self,
        hook: &HookType,
        context: &HookContext,
    ) -> Vec<HookResponse> {
        let mut responses = Vec::new();

        for plugin in &mut self.plugins {
            if !plugin.enabled {
                continue;
            }

            let listens = plugin
                .manifest
                .hooks
                .iter()
                .any(|h| hook_matches(h, hook));

            if !listens {
                continue;
            }

            match plugin.runtime.call_hook(hook, context) {
                Ok(Some(resp)) => responses.push(resp),
                Ok(None) => {}
                Err(e) => {
                    log::error!(
                        "plugin '{}' hook error: {e:#}",
                        plugin.manifest.name
                    );
                }
            }
        }

        responses
    }

    /// Execute a plugin-registered command. Searches all enabled plugins for
    /// a matching command name and runs the first match.
    pub fn execute_command(
        &mut self,
        command: &str,
        args: &[String],
    ) -> Option<String> {
        for plugin in &mut self.plugins {
            if !plugin.enabled {
                continue;
            }

            let has_cmd = plugin
                .manifest
                .commands
                .iter()
                .any(|c| c.name == command);

            if !has_cmd {
                continue;
            }

            match plugin.runtime.call_command(command, args) {
                Ok(output) => return Some(output),
                Err(e) => {
                    log::error!(
                        "plugin '{}' command '{command}' error: {e:#}",
                        plugin.manifest.name
                    );
                    return None;
                }
            }
        }

        None
    }

    /// Collect status-bar text from all enabled plugins that define a
    /// status-bar widget. Returns `(plugin_name, text)` pairs.
    pub fn status_bar_texts(&mut self) -> Vec<(String, String)> {
        let mut texts = Vec::new();

        for plugin in &mut self.plugins {
            if !plugin.enabled || !plugin.manifest.has_status_bar() {
                continue;
            }

            match plugin.runtime.get_status_text() {
                Ok(Some(text)) => {
                    texts.push((plugin.manifest.name.clone(), text));
                }
                Ok(None) => {}
                Err(e) => {
                    log::error!(
                        "plugin '{}' status text error: {e:#}",
                        plugin.manifest.name
                    );
                }
            }
        }

        texts
    }

    /// Enable or disable a plugin by name.
    pub fn set_enabled(&mut self, name: &str, enabled: bool) {
        for plugin in &mut self.plugins {
            if plugin.manifest.name == name {
                plugin.enabled = enabled;
                log::info!(
                    "plugin '{}' {}",
                    name,
                    if enabled { "enabled" } else { "disabled" }
                );
                return;
            }
        }
        log::warn!("plugin '{name}' not found");
    }

    /// List all loaded plugins with their summary info.
    pub fn list(&self) -> Vec<PluginInfo> {
        self.plugins
            .iter()
            .map(|p| PluginInfo {
                name: p.manifest.name.clone(),
                version: p.manifest.version.clone(),
                description: p.manifest.description.clone(),
                enabled: p.enabled,
                hooks: p.manifest.hooks.len(),
                commands: p.manifest.commands.len(),
            })
            .collect()
    }

    /// Shut down all loaded plugins gracefully.
    pub fn shutdown_all(&mut self) {
        for plugin in &mut self.plugins {
            if let Err(e) = plugin.runtime.shutdown() {
                log::error!(
                    "plugin '{}' shutdown error: {e:#}",
                    plugin.manifest.name
                );
            }
        }
    }

    /// Number of currently loaded plugins.
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// Whether the registry has zero loaded plugins.
    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Default plugin directory: `~/.config/phantom/plugins/`.
fn dirs_or_default() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join(".config")
        .join("phantom")
        .join("plugins")
}

/// Load a plugin manifest from a directory. Tries `plugin.toml` first, then
/// `plugin.json`.
fn load_manifest(dir: &Path) -> Result<PluginManifest> {
    let toml_path = dir.join("plugin.toml");
    if toml_path.exists() {
        let text = fs::read_to_string(&toml_path)
            .with_context(|| format!("cannot read {}", toml_path.display()))?;
        return PluginManifest::from_toml(&text);
    }

    let json_path = dir.join("plugin.json");
    if json_path.exists() {
        let text = fs::read_to_string(&json_path)
            .with_context(|| format!("cannot read {}", json_path.display()))?;
        return PluginManifest::from_json(&text);
    }

    anyhow::bail!(
        "no plugin.toml or plugin.json found in {}",
        dir.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::MockRuntime;
    use crate::manifest::*;
    use std::io::Write;

    fn test_manifest(name: &str) -> PluginManifest {
        PluginManifest {
            name: name.into(),
            version: "1.0.0".into(),
            description: format!("{name} plugin"),
            author: "test".into(),
            license: None,
            homepage: None,
            entry_point: "plugin.wasm".into(),
            permissions: vec![Permission::ReadFiles],
            hooks: vec![HookType::OnStartup, HookType::OnCommand("git *".into())],
            commands: vec![CommandDef {
                name: format!("{name}-cmd"),
                description: "a command".into(),
                usage: format!("{name}-cmd [args]"),
            }],
            status_bar: Some(StatusBarDef {
                position: StatusBarPosition::Right,
                update_interval_ms: 1000,
            }),
        }
    }

    fn mock_runtime_for(name: &str) -> MockRuntime {
        MockRuntime::new()
            .on_hook("OnStartup", HookResponse::DisplayText(format!("{name} started")))
            .on_hook(
                "OnCommand:git *",
                HookResponse::DisplayText(format!("{name} saw git")),
            )
            .on_command(&format!("{name}-cmd"), &format!("{name} output"))
            .with_status_text(&format!("{name}: ok"))
    }

    #[test]
    fn registry_register_and_list() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = PluginRegistry::with_dir(tmp.path()).unwrap();
        assert!(reg.is_empty());

        reg.register(
            test_manifest("alpha"),
            Box::new(mock_runtime_for("alpha")),
        )
        .unwrap();

        assert_eq!(reg.len(), 1);
        let list = reg.list();
        assert_eq!(list[0].name, "alpha");
        assert!(list[0].enabled);
        assert_eq!(list[0].hooks, 2);
        assert_eq!(list[0].commands, 1);
    }

    #[test]
    fn registry_dispatch_hook_to_matching_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = PluginRegistry::with_dir(tmp.path()).unwrap();
        reg.register(
            test_manifest("alpha"),
            Box::new(mock_runtime_for("alpha")),
        )
        .unwrap();

        let ctx = HookContext::startup("/tmp");
        let responses = reg.dispatch_hook(&HookType::OnStartup, &ctx);
        assert_eq!(responses.len(), 1);
        assert_eq!(
            responses[0],
            HookResponse::DisplayText("alpha started".into())
        );
    }

    #[test]
    fn registry_dispatch_hook_skips_disabled_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = PluginRegistry::with_dir(tmp.path()).unwrap();
        reg.register(
            test_manifest("alpha"),
            Box::new(mock_runtime_for("alpha")),
        )
        .unwrap();
        reg.set_enabled("alpha", false);

        let ctx = HookContext::startup("/tmp");
        let responses = reg.dispatch_hook(&HookType::OnStartup, &ctx);
        assert!(responses.is_empty());
    }

    #[test]
    fn registry_dispatch_hook_skips_non_listening_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = PluginRegistry::with_dir(tmp.path()).unwrap();
        reg.register(
            test_manifest("alpha"),
            Box::new(mock_runtime_for("alpha")),
        )
        .unwrap();

        // OnError not in the manifest hooks.
        let ctx = HookContext::error("oops", "/tmp");
        let responses = reg.dispatch_hook(&HookType::OnError, &ctx);
        assert!(responses.is_empty());
    }

    #[test]
    fn registry_dispatch_command_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = PluginRegistry::with_dir(tmp.path()).unwrap();
        reg.register(
            test_manifest("alpha"),
            Box::new(mock_runtime_for("alpha")),
        )
        .unwrap();

        let ctx = HookContext::command("git push", "/home");
        let responses =
            reg.dispatch_hook(&HookType::OnCommand("git push".into()), &ctx);
        assert_eq!(responses.len(), 1);
        assert_eq!(
            responses[0],
            HookResponse::DisplayText("alpha saw git".into())
        );
    }

    #[test]
    fn registry_execute_command() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = PluginRegistry::with_dir(tmp.path()).unwrap();
        reg.register(
            test_manifest("alpha"),
            Box::new(mock_runtime_for("alpha")),
        )
        .unwrap();

        let result = reg.execute_command("alpha-cmd", &[]);
        assert_eq!(result, Some("alpha output".into()));
    }

    #[test]
    fn registry_execute_unknown_command() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = PluginRegistry::with_dir(tmp.path()).unwrap();
        reg.register(
            test_manifest("alpha"),
            Box::new(mock_runtime_for("alpha")),
        )
        .unwrap();

        let result = reg.execute_command("nonexistent", &[]);
        assert_eq!(result, None);
    }

    #[test]
    fn registry_status_bar_texts() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = PluginRegistry::with_dir(tmp.path()).unwrap();
        reg.register(
            test_manifest("alpha"),
            Box::new(mock_runtime_for("alpha")),
        )
        .unwrap();
        reg.register(
            test_manifest("beta"),
            Box::new(mock_runtime_for("beta")),
        )
        .unwrap();

        let texts = reg.status_bar_texts();
        assert_eq!(texts.len(), 2);
        assert_eq!(texts[0], ("alpha".into(), "alpha: ok".into()));
        assert_eq!(texts[1], ("beta".into(), "beta: ok".into()));
    }

    #[test]
    fn registry_set_enabled_toggle() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = PluginRegistry::with_dir(tmp.path()).unwrap();
        reg.register(
            test_manifest("alpha"),
            Box::new(mock_runtime_for("alpha")),
        )
        .unwrap();

        assert!(reg.plugins()[0].enabled);
        reg.set_enabled("alpha", false);
        assert!(!reg.plugins()[0].enabled);
        reg.set_enabled("alpha", true);
        assert!(reg.plugins()[0].enabled);
    }

    #[test]
    fn registry_shutdown_all() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = PluginRegistry::with_dir(tmp.path()).unwrap();
        reg.register(
            test_manifest("alpha"),
            Box::new(mock_runtime_for("alpha")),
        )
        .unwrap();

        // Should not panic.
        reg.shutdown_all();
    }

    #[test]
    fn registry_scan_finds_toml_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join("my-plugin");
        fs::create_dir_all(&plugin_dir).unwrap();

        let manifest = test_manifest("my-plugin");
        let toml_str = toml::to_string_pretty(&manifest).unwrap();
        let mut f = fs::File::create(plugin_dir.join("plugin.toml")).unwrap();
        f.write_all(toml_str.as_bytes()).unwrap();

        let mut reg = PluginRegistry::with_dir(tmp.path()).unwrap();
        let manifests = reg.scan().unwrap();
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].name, "my-plugin");
    }

    #[test]
    fn registry_scan_finds_json_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join("json-plugin");
        fs::create_dir_all(&plugin_dir).unwrap();

        let manifest = test_manifest("json-plugin");
        let json_str = serde_json::to_string_pretty(&manifest).unwrap();
        let mut f = fs::File::create(plugin_dir.join("plugin.json")).unwrap();
        f.write_all(json_str.as_bytes()).unwrap();

        let mut reg = PluginRegistry::with_dir(tmp.path()).unwrap();
        let manifests = reg.scan().unwrap();
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].name, "json-plugin");
    }

    #[test]
    fn registry_load_plugin_from_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let plugin_dir = tmp.path().join("disk-plugin");
        fs::create_dir_all(&plugin_dir).unwrap();

        let manifest = test_manifest("disk-plugin");
        let toml_str = toml::to_string_pretty(&manifest).unwrap();
        fs::write(plugin_dir.join("plugin.toml"), toml_str).unwrap();

        let mut reg = PluginRegistry::with_dir(tmp.path()).unwrap();
        reg.load_plugin(&plugin_dir, Box::new(mock_runtime_for("disk-plugin")))
            .unwrap();

        assert_eq!(reg.len(), 1);
        assert_eq!(reg.plugins()[0].manifest.name, "disk-plugin");
    }

    #[test]
    fn registry_multiple_plugins_dispatch() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = PluginRegistry::with_dir(tmp.path()).unwrap();
        reg.register(
            test_manifest("alpha"),
            Box::new(mock_runtime_for("alpha")),
        )
        .unwrap();
        reg.register(
            test_manifest("beta"),
            Box::new(mock_runtime_for("beta")),
        )
        .unwrap();

        let ctx = HookContext::startup("/tmp");
        let responses = reg.dispatch_hook(&HookType::OnStartup, &ctx);
        assert_eq!(responses.len(), 2);
    }
}
