//! Script-based plugin runtime — executes shell commands instead of WASM.
//!
//! Each hook/command is mapped to a shell script path or inline command.
//! This provides a real plugin execution path without the wasmtime dependency.

use std::collections::HashMap;
use std::process::Command;

use anyhow::Result;

use crate::host::{HookContext, HookResponse, PluginRuntime};
use crate::manifest::{HookType, PluginManifest};

/// A plugin runtime that delegates to shell scripts/commands.
pub struct ScriptRuntime {
    initialized: bool,
    working_dir: String,
    /// Maps hook discriminant keys to shell commands.
    hook_scripts: HashMap<String, String>,
    /// Maps command names to shell commands.
    command_scripts: HashMap<String, String>,
    status_command: Option<String>,
}

impl ScriptRuntime {
    pub fn new(working_dir: impl Into<String>) -> Self {
        Self {
            initialized: false,
            working_dir: working_dir.into(),
            hook_scripts: HashMap::new(),
            command_scripts: HashMap::new(),
            status_command: None,
        }
    }

    /// Register a shell command to run when a specific hook fires.
    pub fn on_hook(mut self, hook_key: &str, shell_cmd: &str) -> Self {
        self.hook_scripts.insert(hook_key.into(), shell_cmd.into());
        self
    }

    /// Register a shell command to run for a named plugin command.
    pub fn on_command(mut self, name: &str, shell_cmd: &str) -> Self {
        self.command_scripts.insert(name.into(), shell_cmd.into());
        self
    }

    /// Register a shell command that produces status bar text.
    pub fn with_status_command(mut self, shell_cmd: &str) -> Self {
        self.status_command = Some(shell_cmd.into());
        self
    }

    fn run_shell(&self, cmd: &str) -> Result<String> {
        let output = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(&self.working_dir)
            .output()?;
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

impl PluginRuntime for ScriptRuntime {
    fn init(&mut self, _manifest: &PluginManifest) -> Result<()> {
        self.initialized = true;
        Ok(())
    }

    fn call_hook(
        &mut self,
        hook: &HookType,
        _context: &HookContext,
    ) -> Result<Option<HookResponse>> {
        let key = hook_type_key(hook);
        let Some(cmd) = self.hook_scripts.get(&key) else {
            return Ok(None);
        };
        let output = self.run_shell(cmd)?;
        if output.is_empty() {
            Ok(Some(HookResponse::Nothing))
        } else {
            Ok(Some(HookResponse::DisplayText(output)))
        }
    }

    fn call_command(&mut self, command: &str, args: &[String]) -> Result<String> {
        let Some(cmd) = self.command_scripts.get(command) else {
            anyhow::bail!("no script registered for command: {command}");
        };
        let full_cmd = if args.is_empty() {
            cmd.clone()
        } else {
            format!("{cmd} {}", args.join(" "))
        };
        self.run_shell(&full_cmd)
    }

    fn get_status_text(&mut self) -> Result<Option<String>> {
        let Some(ref cmd) = self.status_command else {
            return Ok(None);
        };
        let output = self.run_shell(cmd)?;
        if output.is_empty() {
            Ok(None)
        } else {
            Ok(Some(output))
        }
    }

    fn shutdown(&mut self) -> Result<()> {
        self.initialized = false;
        Ok(())
    }
}

/// Derive a string key from a `HookType` discriminant.
fn hook_type_key(hook: &HookType) -> String {
    match hook {
        HookType::OnCommand(pat) => format!("OnCommand:{pat}"),
        HookType::OnOutput => "OnOutput".into(),
        HookType::OnError => "OnError".into(),
        HookType::OnStartup => "OnStartup".into(),
        HookType::OnShutdown => "OnShutdown".into(),
        HookType::OnInterval(s) => format!("OnInterval:{s}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::*;

    fn test_manifest() -> PluginManifest {
        PluginManifest {
            name: "test-script".into(),
            version: "0.1.0".into(),
            description: "test".into(),
            author: "test".into(),
            license: None,
            homepage: None,
            entry_point: "script".into(),
            permissions: vec![],
            hooks: vec![HookType::OnStartup],
            commands: vec![],
            status_bar: None,
        }
    }

    #[test]
    fn script_runtime_init_and_shutdown() {
        let mut rt = ScriptRuntime::new("/tmp");
        rt.init(&test_manifest()).unwrap();
        assert!(rt.initialized);
        rt.shutdown().unwrap();
        assert!(!rt.initialized);
    }

    #[test]
    fn script_runtime_hook_runs_command() {
        let mut rt = ScriptRuntime::new("/tmp")
            .on_hook("OnStartup", "echo hello");
        rt.init(&test_manifest()).unwrap();

        let ctx = crate::host::HookContext::startup("/tmp");
        let resp = rt.call_hook(&HookType::OnStartup, &ctx).unwrap();
        assert_eq!(resp, Some(HookResponse::DisplayText("hello".into())));
    }

    #[test]
    fn script_runtime_command_runs() {
        let mut rt = ScriptRuntime::new("/tmp")
            .on_command("greet", "echo hi");
        rt.init(&test_manifest()).unwrap();

        let output = rt.call_command("greet", &[]).unwrap();
        assert_eq!(output, "hi");
    }

    #[test]
    fn script_runtime_status_text() {
        let mut rt = ScriptRuntime::new("/tmp")
            .with_status_command("echo status-ok");

        let text = rt.get_status_text().unwrap();
        assert_eq!(text, Some("status-ok".into()));
    }

    #[test]
    fn script_runtime_hook_no_match_returns_none() {
        let mut rt = ScriptRuntime::new("/tmp")
            .on_hook("OnStartup", "echo boot");
        rt.init(&test_manifest()).unwrap();

        // OnShutdown not registered.
        let ctx = crate::host::HookContext::shutdown("/tmp");
        let resp = rt.call_hook(&HookType::OnShutdown, &ctx).unwrap();
        assert_eq!(resp, None);
    }

    #[test]
    fn script_runtime_command_with_args() {
        let mut rt = ScriptRuntime::new("/tmp")
            .on_command("echo-args", "echo");
        rt.init(&test_manifest()).unwrap();

        let output = rt.call_command("echo-args", &["hello".into(), "world".into()]).unwrap();
        assert_eq!(output, "hello world");
    }

    #[test]
    fn script_runtime_unknown_command_errors() {
        let mut rt = ScriptRuntime::new("/tmp");
        rt.init(&test_manifest()).unwrap();

        let result = rt.call_command("nonexistent", &[]);
        assert!(result.is_err());
    }

    #[test]
    fn script_runtime_empty_output_returns_nothing() {
        let mut rt = ScriptRuntime::new("/tmp")
            .on_hook("OnStartup", "true"); // `true` produces no output
        rt.init(&test_manifest()).unwrap();

        let ctx = crate::host::HookContext::startup("/tmp");
        let resp = rt.call_hook(&HookType::OnStartup, &ctx).unwrap();
        assert_eq!(resp, Some(HookResponse::Nothing));
    }

    // =======================================================================
    // Registry + ScriptRuntime integration
    // =======================================================================

    #[test]
    fn registry_with_script_runtime_dispatches_hooks() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = crate::registry::PluginRegistry::with_dir(tmp.path()).unwrap();

        let manifest = PluginManifest {
            name: "echo-plugin".into(),
            version: "1.0.0".into(),
            description: "echoes on startup".into(),
            author: "test".into(),
            license: None,
            homepage: None,
            entry_point: "script".into(),
            permissions: vec![],
            hooks: vec![HookType::OnStartup, HookType::OnShutdown],
            commands: vec![CommandDef {
                name: "greet".into(),
                description: "say hi".into(),
                usage: "greet".into(),
            }],
            status_bar: None,
        };

        let rt = ScriptRuntime::new("/tmp")
            .on_hook("OnStartup", "echo phantom-alive")
            .on_hook("OnShutdown", "echo phantom-bye")
            .on_command("greet", "echo hello-from-plugin");

        reg.register(manifest, Box::new(rt)).unwrap();
        assert_eq!(reg.len(), 1);

        // Startup hook.
        let ctx = crate::host::HookContext::startup("/tmp");
        let resps = reg.dispatch_hook(&HookType::OnStartup, &ctx);
        assert_eq!(resps.len(), 1);
        assert_eq!(resps[0], HookResponse::DisplayText("phantom-alive".into()));

        // Shutdown hook.
        let ctx = crate::host::HookContext::shutdown("/tmp");
        let resps = reg.dispatch_hook(&HookType::OnShutdown, &ctx);
        assert_eq!(resps.len(), 1);
        assert_eq!(resps[0], HookResponse::DisplayText("phantom-bye".into()));

        // Command execution.
        let output = reg.execute_command("greet", &[]);
        assert_eq!(output, Some("hello-from-plugin".into()));
    }

    #[test]
    fn registry_shutdown_all_with_script_runtime() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = crate::registry::PluginRegistry::with_dir(tmp.path()).unwrap();

        let manifest = test_manifest();
        let rt = ScriptRuntime::new("/tmp");
        reg.register(manifest, Box::new(rt)).unwrap();

        // Should not panic.
        reg.shutdown_all();
    }

    #[test]
    fn script_runtime_working_dir_affects_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let mut rt = ScriptRuntime::new(tmp.path().to_str().unwrap())
            .on_command("pwd", "pwd");
        rt.init(&test_manifest()).unwrap();

        let output = rt.call_command("pwd", &[]).unwrap();
        // The output should be the tmp dir path (resolved).
        assert!(
            output.contains("tmp") || output.contains("var"),
            "pwd should reflect working dir: {output}"
        );
    }
}
