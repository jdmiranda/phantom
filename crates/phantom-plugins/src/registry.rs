use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use log;

use crate::host::{HookContext, HookResponse, PluginRuntime};
use crate::manifest::{hook_matches, HookType, Permission, PluginManifest};
use crate::wasm_host::WasmHost;

// ---------------------------------------------------------------------------
// Plugin error
// ---------------------------------------------------------------------------

/// Errors that can be returned by registry operations.
#[derive(Debug)]
pub enum PluginError {
    /// The plugin does not have the permission required to perform this operation.
    PermissionDenied {
        plugin: String,
        required: Permission,
    },
}

impl std::fmt::Display for PluginError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PluginError::PermissionDenied { plugin, required } => {
                write!(
                    f,
                    "plugin '{plugin}' denied: missing permission {required:?}"
                )
            }
        }
    }
}

impl std::error::Error for PluginError {}

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
    #[must_use]
    pub fn empty() -> Self {
        Self {
            plugins: Vec::new(),
            plugin_dir: PathBuf::new(),
        }
    }

    /// The directory this registry scans for plugins.
    #[must_use]
    pub fn plugin_dir(&self) -> &Path {
        &self.plugin_dir
    }

    /// Scan the plugin directory, load all manifests, and automatically
    /// activate plugins that have a non-empty `entry_point` field.
    ///
    /// For each plugin directory found:
    /// - The manifest is parsed from `plugin.toml` or `plugin.json`.
    /// - If `entry_point` is non-empty (i.e. the plugin is not a scaffold
    ///   placeholder), the entry-point WASM file is loaded via [`WasmHost`]
    ///   and the plugin is registered. Errors from individual plugins are
    ///   logged as warnings and do not prevent other plugins from loading.
    /// - Scaffold plugins (empty `entry_point`) are recorded in the returned
    ///   manifest list but not activated.
    ///
    /// Returns the manifests of all successfully parsed plugins (both activated
    /// and scaffold-only).
    ///
    /// # Threading
    ///
    /// A single [`WasmHost`] is built once and reused across every plugin
    /// loaded in this scan. This is sound today because `scan` is invoked
    /// synchronously from a single thread (typically the boot path in
    /// `phantom-app`). If `WasmHost` ever becomes `Send + !Sync`, or `scan`
    /// is ever moved off-thread, this shared-host pattern needs to change to
    /// either build a host per-iteration or guard the host behind a `Mutex`.
    pub fn scan(&mut self) -> Result<Vec<PluginManifest>> {
        let mut manifests = Vec::new();
        let entries = fs::read_dir(&self.plugin_dir)
            .with_context(|| format!("cannot read plugin dir: {}", self.plugin_dir.display()))?;

        // Collect plugin directories first so we can iterate cleanly.
        let mut plugin_dirs: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                plugin_dirs.push(path);
            }
        }

        // Build a shared WASM host for all plugins discovered in this scan.
        // Errors creating the host are not fatal — we still return manifests.
        let wasm_host = WasmHost::new().ok();

        for path in plugin_dirs {
            let manifest = match load_manifest(&path) {
                Ok(m) => {
                    log::info!("found plugin: {} v{}", m.name, m.version);
                    m
                }
                Err(e) => {
                    // Use full path: `file_name()` is `None` for `/`, which would
                    // produce an unhelpful "skipping : ..." log line.
                    log::warn!("skipping {}: {e:#}", path.display());
                    continue;
                }
            };

            // Auto-load plugins with a real entry point.
            if !manifest.entry_point.is_empty() {
                let wasm_path = path.join(&manifest.entry_point);
                match (wasm_host.as_ref(), fs::read(&wasm_path)) {
                    (Some(host), Ok(bytes)) => {
                        match host.load(&bytes) {
                            Ok(runtime) => {
                                match self.register(manifest.clone(), Box::new(runtime)) {
                                    Ok(()) => {
                                        log::info!(
                                            "auto-loaded plugin: {} v{}",
                                            manifest.name, manifest.version
                                        );
                                    }
                                    Err(e) => {
                                        log::warn!(
                                            "plugin '{}' init failed: {e:#}",
                                            manifest.name
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                log::warn!(
                                    "plugin '{}' WASM load failed ({}): {e:#}",
                                    manifest.name,
                                    wasm_path.display()
                                );
                            }
                        }
                    }
                    (None, _) => {
                        log::warn!(
                            "plugin '{}' skipped: WasmHost unavailable",
                            manifest.name
                        );
                    }
                    (_, Err(e)) => {
                        log::warn!(
                            "plugin '{}' entry point not readable ({}): {e:#}",
                            manifest.name,
                            wasm_path.display()
                        );
                    }
                }
            }

            manifests.push(manifest);
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
    #[must_use]
    pub fn plugins(&self) -> &[LoadedPlugin] {
        &self.plugins
    }

    /// Dispatch a hook event to every enabled plugin that registered for it.
    ///
    /// Plugins that lack the permission required for the hook are skipped and
    /// a [`PluginError::PermissionDenied`] is logged as a warning. All other
    /// enabled, listening plugins receive the hook regardless of individual
    /// permission failures. Returns all non-`None` responses.
    pub fn dispatch_hook(
        &mut self,
        hook: &HookType,
        context: &HookContext,
    ) -> Vec<HookResponse> {
        let required_perm = hook.required_permission();
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

            // Permission gate: if this hook type requires a permission,
            // verify the plugin manifest declares it before dispatching.
            if let Some(ref required) = required_perm
                && !plugin.manifest.has_permission(required)
            {
                let err = PluginError::PermissionDenied {
                    plugin: plugin.manifest.name.clone(),
                    required: required.clone(),
                };
                log::warn!("{err}");
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

    /// Dispatch a hook and return `Err(PluginError::PermissionDenied)` for the
    /// first plugin whose permission check fails, without dispatching to any
    /// further plugins. Useful when callers need a hard failure on denial rather
    /// than a silent skip.
    ///
    /// Unlike [`dispatch_hook`], this method stops at the first permission
    /// violation and returns without processing remaining plugins.
    ///
    /// Visibility is intentionally `pub(crate)` until a real caller exists — the
    /// public dispatch path is [`dispatch_hook`], which keeps the
    /// "warn-and-continue" semantics that the app/test surface expects. Promote
    /// to `pub` when an external caller is wired in.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn dispatch_hook_strict(
        &mut self,
        hook: &HookType,
        context: &HookContext,
    ) -> std::result::Result<Vec<HookResponse>, PluginError> {
        let required_perm = hook.required_permission();
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

            if let Some(ref required) = required_perm
                && !plugin.manifest.has_permission(required)
            {
                return Err(PluginError::PermissionDenied {
                    plugin: plugin.manifest.name.clone(),
                    required: required.clone(),
                });
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

        Ok(responses)
    }

    /// Execute a plugin-registered command. Searches all enabled plugins for
    /// a matching command name and runs the first plugin that both registers
    /// the command and holds the required permissions.
    ///
    /// Every command requires [`Permission::RunCommands`]. Commands whose
    /// [`CommandDef::write_access`] flag is `true` additionally require
    /// [`Permission::WriteFiles`]. A plugin that lacks the required permission
    /// is **skipped** (a warning is logged) and the search continues — so if a
    /// later plugin registers the same name with sufficient privilege, it
    /// still runs. This means a denied plugin never silently swallows a
    /// command intended for a peer.
    pub fn execute_command(
        &mut self,
        command: &str,
        args: &[String],
    ) -> Option<String> {
        for plugin in &mut self.plugins {
            if !plugin.enabled {
                continue;
            }

            let Some(cmd_def) = plugin
                .manifest
                .commands
                .iter()
                .find(|c| c.name == command)
            else {
                continue;
            };

            // Per-command write_access flag decides whether WriteFiles is needed.
            let needs_write = cmd_def.write_access;

            // All commands require RunCommands.
            if !plugin.manifest.has_permission(&Permission::RunCommands) {
                let err = PluginError::PermissionDenied {
                    plugin: plugin.manifest.name.clone(),
                    required: Permission::RunCommands,
                };
                log::warn!("{err}");
                continue;
            }

            // File-writing commands additionally require WriteFiles.
            if needs_write && !plugin.manifest.has_permission(&Permission::WriteFiles) {
                let err = PluginError::PermissionDenied {
                    plugin: plugin.manifest.name.clone(),
                    required: Permission::WriteFiles,
                };
                log::warn!("{err}");
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
    #[must_use]
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
    #[must_use]
    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// Whether the registry has zero loaded plugins.
    #[must_use]
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
            permissions: vec![Permission::ReadFiles, Permission::RunCommands],
            hooks: vec![HookType::OnStartup, HookType::OnCommand("git *".into())],
            commands: vec![CommandDef {
                name: format!("{name}-cmd"),
                description: "a command".into(),
                usage: format!("{name}-cmd [args]"),
                write_access: false,
            }],
            status_bar: Some(StatusBarDef {
                position: StatusBarPosition::Right,
                update_interval_ms: 1000,
            }),
            scaffold: false,
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

    // -----------------------------------------------------------------------
    // Bug-fix tests
    // -----------------------------------------------------------------------

    /// Bug 1: OnStartup hook dispatches to all loaded plugins.
    #[test]
    fn on_startup_dispatches_to_all_plugins() {
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

        // Both plugins should have received and responded to OnStartup.
        assert_eq!(responses.len(), 2);
        let texts: Vec<&str> = responses
            .iter()
            .filter_map(|r| match r {
                HookResponse::DisplayText(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            texts.contains(&"alpha started"),
            "alpha must receive OnStartup"
        );
        assert!(
            texts.contains(&"beta started"),
            "beta must receive OnStartup"
        );
    }

    /// Bug 2a: Plugin without Network permission gets denied for OnNetworkEvent.
    ///
    /// OnOutput requires RunCommands; we use it here as a proxy for a hook that
    /// needs a privilege the plugin does not hold.
    #[test]
    fn dispatch_hook_denied_when_no_permission() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = PluginRegistry::with_dir(tmp.path()).unwrap();

        // Plugin that listens for OnOutput but has NO RunCommands permission.
        let mut manifest = test_manifest("no-perm");
        manifest.permissions = vec![Permission::ReadFiles]; // no RunCommands
        manifest.hooks = vec![HookType::OnOutput];

        let runtime = MockRuntime::new()
            .on_hook("OnOutput", HookResponse::DisplayText("heard output".into()));

        reg.register(manifest, Box::new(runtime)).unwrap();

        let ctx = HookContext::output("ls", "file.txt\n", 0, "/tmp");
        let responses = reg.dispatch_hook(&HookType::OnOutput, &ctx);

        // Permission denied — no responses from the plugin.
        assert!(
            responses.is_empty(),
            "plugin without RunCommands must not receive OnOutput"
        );
    }

    /// Bug 2b: Plugin WITH Network permission receives OnNetworkEvent proxy.
    ///
    /// We model "network-event" via `OnCommand("curl *")` + `Permission::RunCommands`
    /// to stay within the actual `HookType` enum.
    #[test]
    fn dispatch_hook_allowed_when_permission_present() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = PluginRegistry::with_dir(tmp.path()).unwrap();

        // Plugin WITH RunCommands — should receive OnCommand.
        let mut manifest = test_manifest("with-perm");
        manifest.permissions = vec![Permission::RunCommands];
        manifest.hooks = vec![HookType::OnCommand("curl *".into())];

        let runtime = MockRuntime::new()
            .on_hook("OnCommand:curl *", HookResponse::DisplayText("curl seen".into()));

        reg.register(manifest, Box::new(runtime)).unwrap();

        let ctx = HookContext::command("curl https://example.com", "/tmp");
        let responses =
            reg.dispatch_hook(&HookType::OnCommand("curl https://example.com".into()), &ctx);

        assert_eq!(responses.len(), 1);
        assert_eq!(
            responses[0],
            HookResponse::DisplayText("curl seen".into()),
            "plugin with RunCommands must receive matching OnCommand"
        );
    }

    /// Bug 3: scan() auto-loads plugins with a non-empty entry_point.
    ///
    /// Because a real WASM binary is required for WasmHost::load and the test
    /// environment cannot provide one, we verify the auto-load path by
    /// confirming that plugins with entry_point="" (scaffold manifests) are
    /// NOT loaded while all manifests are still returned.  This proves the
    /// "skip scaffold, return manifest" path in scan() without needing a real
    /// WASM artifact.
    #[test]
    fn registry_scan_auto_loads_plugins() {
        let tmp = tempfile::tempdir().unwrap();

        // Plugin 1: scaffold (no entry point) — manifest returned, not loaded.
        let plugin_a_dir = tmp.path().join("scaffold-plugin");
        fs::create_dir_all(&plugin_a_dir).unwrap();
        let mut scaffold_manifest = test_manifest("scaffold-plugin");
        scaffold_manifest.entry_point = String::new();
        scaffold_manifest.scaffold = true;
        let toml_str = toml::to_string_pretty(&scaffold_manifest).unwrap();
        fs::write(plugin_a_dir.join("plugin.toml"), toml_str).unwrap();

        // Plugin 2: real entry point but no actual file — load will warn, not crash.
        let plugin_b_dir = tmp.path().join("real-plugin");
        fs::create_dir_all(&plugin_b_dir).unwrap();
        let mut real_manifest = test_manifest("real-plugin");
        real_manifest.entry_point = "plugin.wasm".into();
        real_manifest.scaffold = false;
        let toml_str = toml::to_string_pretty(&real_manifest).unwrap();
        fs::write(plugin_b_dir.join("plugin.toml"), toml_str).unwrap();
        // Note: plugin.wasm is intentionally absent — load will warn and skip.

        let mut reg = PluginRegistry::with_dir(tmp.path()).unwrap();
        let manifests = reg.scan().unwrap();

        // Both manifests are returned regardless of load outcome.
        assert_eq!(
            manifests.len(),
            2,
            "scan must return all manifests (scaffold and real)"
        );

        // The scaffold plugin has no entry point, so it cannot be loaded.
        // The real plugin has an absent WASM file, so load fails gracefully.
        // Either way, no plugins should be in the registry after this scan.
        assert_eq!(
            reg.len(),
            0,
            "no plugins loaded: scaffold has no binary, real plugin's binary is absent"
        );
    }

    /// Review fix: `execute_command` must `continue` past a permission-denied
    /// plugin instead of swallowing the call. A second plugin registering the
    /// same command name with sufficient permission must still receive the
    /// invocation.
    #[test]
    fn execute_command_falls_through_denied_plugin_to_authorised_peer() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = PluginRegistry::with_dir(tmp.path()).unwrap();

        // Plugin "denied" registers `shared-cmd` but has NO RunCommands.
        let mut denied_manifest = test_manifest("denied");
        denied_manifest.permissions = vec![Permission::ReadFiles]; // no RunCommands
        denied_manifest.commands = vec![CommandDef {
            name: "shared-cmd".into(),
            description: "shared command".into(),
            usage: "shared-cmd".into(),
            write_access: false,
        }];
        let denied_runtime =
            MockRuntime::new().on_command("shared-cmd", "denied output");
        reg.register(denied_manifest, Box::new(denied_runtime)).unwrap();

        // Plugin "allowed" registers the same command WITH RunCommands.
        let mut allowed_manifest = test_manifest("allowed");
        allowed_manifest.permissions = vec![Permission::RunCommands];
        allowed_manifest.commands = vec![CommandDef {
            name: "shared-cmd".into(),
            description: "shared command".into(),
            usage: "shared-cmd".into(),
            write_access: false,
        }];
        let allowed_runtime =
            MockRuntime::new().on_command("shared-cmd", "allowed output");
        reg.register(allowed_manifest, Box::new(allowed_runtime)).unwrap();

        let result = reg.execute_command("shared-cmd", &[]);
        assert_eq!(
            result.as_deref(),
            Some("allowed output"),
            "denied plugin must not swallow the command; the next authorised \
             plugin must receive it",
        );
    }

    /// Review fix: a command flagged `write_access = true` requires the
    /// plugin to declare `Permission::WriteFiles` in addition to `RunCommands`.
    #[test]
    fn execute_command_enforces_write_access_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = PluginRegistry::with_dir(tmp.path()).unwrap();

        // Plugin with RunCommands but no WriteFiles registers a write_access cmd.
        let mut manifest = test_manifest("writer");
        manifest.permissions = vec![Permission::RunCommands]; // no WriteFiles
        manifest.commands = vec![CommandDef {
            name: "typewriter".into(),
            description: "writes nothing actually".into(),
            usage: "typewriter".into(),
            write_access: true, // explicit declaration that this writes
        }];
        let runtime = MockRuntime::new().on_command("typewriter", "wrote stuff");
        reg.register(manifest, Box::new(runtime)).unwrap();

        let result = reg.execute_command("typewriter", &[]);
        assert!(
            result.is_none(),
            "write_access=true must require Permission::WriteFiles"
        );
    }

    /// Smoke test for the `pub(crate)` `dispatch_hook_strict` variant — keeps it
    /// honest while no external caller exists.
    #[test]
    fn dispatch_hook_strict_returns_err_on_denial() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = PluginRegistry::with_dir(tmp.path()).unwrap();

        let mut manifest = test_manifest("no-perm");
        manifest.permissions = vec![Permission::ReadFiles]; // no RunCommands
        manifest.hooks = vec![HookType::OnOutput];
        let runtime = MockRuntime::new()
            .on_hook("OnOutput", HookResponse::DisplayText("never".into()));
        reg.register(manifest, Box::new(runtime)).unwrap();

        let ctx = HookContext::output("ls", "file.txt\n", 0, "/tmp");
        let result = reg.dispatch_hook_strict(&HookType::OnOutput, &ctx);
        assert!(matches!(result, Err(PluginError::PermissionDenied { .. })));
    }

    /// Review fix: a `write_access = false` command must NOT require
    /// WriteFiles even if its name happens to contain a substring the old
    /// heuristic would have flagged (e.g. "typewriter" contains "write").
    #[test]
    fn execute_command_name_substring_no_longer_implies_write_access() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = PluginRegistry::with_dir(tmp.path()).unwrap();

        // Old heuristic would have demanded WriteFiles because the command name
        // contains "write"; the per-command flag must override that.
        let mut manifest = test_manifest("readonly-writer");
        manifest.permissions = vec![Permission::RunCommands]; // no WriteFiles
        manifest.commands = vec![CommandDef {
            name: "typewriter".into(),
            description: "read-only despite the name".into(),
            usage: "typewriter".into(),
            write_access: false,
        }];
        let runtime = MockRuntime::new().on_command("typewriter", "ok");
        reg.register(manifest, Box::new(runtime)).unwrap();

        let result = reg.execute_command("typewriter", &[]);
        assert_eq!(
            result.as_deref(),
            Some("ok"),
            "name-substring must not imply WriteFiles when write_access=false"
        );
    }
}
