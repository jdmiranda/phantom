use std::collections::HashMap;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::manifest::{HookType, PluginManifest};

// ---------------------------------------------------------------------------
// Plugin runtime trait
// ---------------------------------------------------------------------------

/// The interface a plugin runtime must implement.
///
/// This abstracts over WASM, native, or mock backends. Phantom ships without
/// a concrete WASM runtime — that gets wired in later via this trait — keeping
/// compile times fast and the plugin API stable.
pub trait PluginRuntime: Send {
    /// Initialize the plugin with its manifest. Called once after loading.
    fn init(&mut self, manifest: &PluginManifest) -> Result<()>;

    /// Call a plugin hook. Returns `None` if the plugin has nothing to say.
    fn call_hook(&mut self, hook: &HookType, context: &HookContext) -> Result<Option<HookResponse>>;

    /// Execute a registered command, returning its output text.
    fn call_command(&mut self, command: &str, args: &[String]) -> Result<String>;

    /// Get the current status-bar text (for plugins with `status_bar` defined).
    fn get_status_text(&mut self) -> Result<Option<String>>;

    /// Graceful shutdown. Release resources, flush state.
    fn shutdown(&mut self) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Hook context & response
// ---------------------------------------------------------------------------

/// Context passed to a plugin when a hook fires.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookContext {
    pub event: HookEvent,
    pub working_dir: String,
    pub command: Option<String>,
    pub output: Option<String>,
    pub exit_code: Option<i32>,
}

/// Concrete event data accompanying a hook invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HookEvent {
    Command { cmd: String },
    Output { cmd: String, stdout: String },
    Error { message: String },
    Startup,
    Shutdown,
    Timer,
}

/// What a plugin can do in response to a hook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookResponse {
    /// Display text in the terminal output area.
    DisplayText(String),
    /// Replace the command's output entirely.
    ModifyOutput(String),
    /// Execute a follow-up command.
    RunCommand(String),
    /// Show a desktop/in-terminal notification.
    Notification(String),
    /// Update this plugin's status-bar segment.
    StatusUpdate(String),
    /// The plugin handled the hook but has nothing to emit.
    Nothing,
}

impl HookContext {
    /// Convenience: build a context for a command event.
    pub fn command(cmd: impl Into<String>, working_dir: impl Into<String>) -> Self {
        let cmd = cmd.into();
        Self {
            event: HookEvent::Command { cmd: cmd.clone() },
            working_dir: working_dir.into(),
            command: Some(cmd),
            output: None,
            exit_code: None,
        }
    }

    /// Convenience: build a context for command output.
    pub fn output(
        cmd: impl Into<String>,
        stdout: impl Into<String>,
        exit_code: i32,
        working_dir: impl Into<String>,
    ) -> Self {
        let cmd = cmd.into();
        let stdout = stdout.into();
        Self {
            event: HookEvent::Output {
                cmd: cmd.clone(),
                stdout: stdout.clone(),
            },
            working_dir: working_dir.into(),
            command: Some(cmd),
            output: Some(stdout),
            exit_code: Some(exit_code),
        }
    }

    /// Convenience: build a context for an error event.
    pub fn error(message: impl Into<String>, working_dir: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            event: HookEvent::Error {
                message: message.clone(),
            },
            working_dir: working_dir.into(),
            command: None,
            output: None,
            exit_code: None,
        }
    }

    /// Convenience: startup event.
    pub fn startup(working_dir: impl Into<String>) -> Self {
        Self {
            event: HookEvent::Startup,
            working_dir: working_dir.into(),
            command: None,
            output: None,
            exit_code: None,
        }
    }

    /// Convenience: shutdown event.
    pub fn shutdown(working_dir: impl Into<String>) -> Self {
        Self {
            event: HookEvent::Shutdown,
            working_dir: working_dir.into(),
            command: None,
            output: None,
            exit_code: None,
        }
    }

    /// Convenience: timer event.
    pub fn timer(working_dir: impl Into<String>) -> Self {
        Self {
            event: HookEvent::Timer,
            working_dir: working_dir.into(),
            command: None,
            output: None,
            exit_code: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Mock runtime — for testing without any WASM engine
// ---------------------------------------------------------------------------

/// A configurable mock runtime for testing the plugin system end-to-end
/// without compiling or loading any WASM.
pub struct MockRuntime {
    initialized: bool,
    hook_responses: HashMap<String, HookResponse>,
    command_responses: HashMap<String, String>,
    status_text: Option<String>,
    shutdown_called: bool,
}

impl MockRuntime {
    pub fn new() -> Self {
        Self {
            initialized: false,
            hook_responses: HashMap::new(),
            command_responses: HashMap::new(),
            status_text: None,
            shutdown_called: false,
        }
    }

    /// Pre-configure a response for a specific hook discriminant name (e.g. "OnStartup").
    pub fn on_hook(mut self, hook_key: &str, response: HookResponse) -> Self {
        self.hook_responses.insert(hook_key.into(), response);
        self
    }

    /// Pre-configure a response for a command invocation.
    pub fn on_command(mut self, cmd: &str, output: &str) -> Self {
        self.command_responses.insert(cmd.into(), output.into());
        self
    }

    /// Pre-configure the status-bar text.
    pub fn with_status_text(mut self, text: &str) -> Self {
        self.status_text = Some(text.into());
        self
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub fn is_shutdown(&self) -> bool {
        self.shutdown_called
    }
}

impl Default for MockRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginRuntime for MockRuntime {
    fn init(&mut self, _manifest: &PluginManifest) -> Result<()> {
        self.initialized = true;
        log::info!("MockRuntime initialized");
        Ok(())
    }

    fn call_hook(
        &mut self,
        hook: &HookType,
        _context: &HookContext,
    ) -> Result<Option<HookResponse>> {
        // Try exact key match first.
        let key = hook_type_key(hook);
        if let Some(resp) = self.hook_responses.get(&key) {
            return Ok(Some(resp.clone()));
        }

        // For OnCommand hooks, fall back to glob-pattern matching against
        // registered patterns (e.g. incoming "OnCommand:git push" matches
        // stored "OnCommand:git *").
        if let HookType::OnCommand(cmd) = hook {
            for (stored_key, resp) in &self.hook_responses {
                if let Some(pattern) = stored_key.strip_prefix("OnCommand:") {
                    if crate::manifest::hook_matches(
                        &HookType::OnCommand(pattern.to_string()),
                        &HookType::OnCommand(cmd.clone()),
                    ) {
                        return Ok(Some(resp.clone()));
                    }
                }
            }
        }

        Ok(None)
    }

    fn call_command(&mut self, command: &str, _args: &[String]) -> Result<String> {
        self.command_responses
            .get(command)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown command: {command}"))
    }

    fn get_status_text(&mut self) -> Result<Option<String>> {
        Ok(self.status_text.clone())
    }

    fn shutdown(&mut self) -> Result<()> {
        self.shutdown_called = true;
        log::info!("MockRuntime shut down");
        Ok(())
    }
}

/// Derive a string key from a `HookType` discriminant for the mock lookup table.
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
            name: "test-plugin".into(),
            version: "0.1.0".into(),
            description: "a test plugin".into(),
            author: "test".into(),
            license: None,
            homepage: None,
            entry_point: "plugin.wasm".into(),
            permissions: vec![],
            hooks: vec![HookType::OnStartup],
            commands: vec![CommandDef {
                name: "hello".into(),
                description: "say hello".into(),
                usage: "hello".into(),
            }],
            status_bar: None,
        }
    }

    #[test]
    fn mock_runtime_init() {
        let mut rt = MockRuntime::new();
        assert!(!rt.is_initialized());
        rt.init(&test_manifest()).unwrap();
        assert!(rt.is_initialized());
    }

    #[test]
    fn mock_runtime_hook_response() {
        let mut rt = MockRuntime::new()
            .on_hook("OnStartup", HookResponse::DisplayText("booted!".into()));
        rt.init(&test_manifest()).unwrap();

        let ctx = HookContext::startup("/tmp");
        let resp = rt.call_hook(&HookType::OnStartup, &ctx).unwrap();
        assert_eq!(resp, Some(HookResponse::DisplayText("booted!".into())));
    }

    #[test]
    fn mock_runtime_hook_no_response() {
        let mut rt = MockRuntime::new();
        rt.init(&test_manifest()).unwrap();
        let ctx = HookContext::startup("/tmp");
        let resp = rt.call_hook(&HookType::OnStartup, &ctx).unwrap();
        assert_eq!(resp, None);
    }

    #[test]
    fn mock_runtime_command() {
        let mut rt = MockRuntime::new().on_command("hello", "Hello, world!");
        rt.init(&test_manifest()).unwrap();
        let out = rt.call_command("hello", &[]).unwrap();
        assert_eq!(out, "Hello, world!");
    }

    #[test]
    fn mock_runtime_unknown_command() {
        let mut rt = MockRuntime::new();
        rt.init(&test_manifest()).unwrap();
        assert!(rt.call_command("nope", &[]).is_err());
    }

    #[test]
    fn mock_runtime_status_text() {
        let mut rt = MockRuntime::new().with_status_text("main [3 ahead]");
        let text = rt.get_status_text().unwrap();
        assert_eq!(text, Some("main [3 ahead]".into()));
    }

    #[test]
    fn mock_runtime_shutdown() {
        let mut rt = MockRuntime::new();
        assert!(!rt.is_shutdown());
        rt.shutdown().unwrap();
        assert!(rt.is_shutdown());
    }

    #[test]
    fn hook_context_command_builder() {
        let ctx = HookContext::command("git status", "/home/user/project");
        assert_eq!(ctx.command, Some("git status".into()));
        assert_eq!(ctx.working_dir, "/home/user/project");
        assert!(matches!(ctx.event, HookEvent::Command { .. }));
    }

    #[test]
    fn hook_context_output_builder() {
        let ctx = HookContext::output("ls", "file.txt\n", 0, "/tmp");
        assert_eq!(ctx.exit_code, Some(0));
        assert_eq!(ctx.output, Some("file.txt\n".into()));
    }

    #[test]
    fn hook_context_error_builder() {
        let ctx = HookContext::error("segfault", "/tmp");
        assert!(matches!(ctx.event, HookEvent::Error { .. }));
    }
}
