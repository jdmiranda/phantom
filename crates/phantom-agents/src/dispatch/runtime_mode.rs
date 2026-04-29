//! [`RuntimeMode`] — execution mode gate for the dispatch layer (Issue #105).

/// Runtime execution mode for the dispatch layer.
///
/// `SpawnOnly` is the mechanically-enforced harness mode required by issue #105.
/// When a [`DispatchContext`] is built with `runtime_mode: RuntimeMode::SpawnOnly`,
/// `dispatch_tool` denies every tool whose name is not `"spawn_subagent"` before
/// any capability gate or handler runs. The denial is logged to the event log so
/// the audit trail is complete.
///
/// Layer ordering: quarantine gate (layer-4) → SpawnOnly gate (layer-3) →
/// capability-class gate (layer-2) → handler.
///
/// [`DispatchContext`]: super::context::DispatchContext
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RuntimeMode {
    /// No extra restriction beyond role-manifest capability gating.
    #[default]
    Normal,
    /// Orchestrator harness mode: only `spawn_subagent` is permitted.
    ///
    /// All other tool calls return `"runtime denied: … only spawn_subagent is
    /// permitted in spawn_only mode"` without touching any handler.
    SpawnOnly,
}

impl RuntimeMode {
    /// Machine-readable label used in event-log payloads.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::SpawnOnly => "spawn_only",
        }
    }

    /// Returns `true` iff `tool_name` is permitted under this mode.
    ///
    /// In `Normal` mode every tool is permitted (capability gating applies
    /// separately). In `SpawnOnly` mode only `"spawn_subagent"` passes.
    pub fn permits(self, tool_name: &str) -> bool {
        match self {
            Self::Normal => true,
            Self::SpawnOnly => tool_name == "spawn_subagent",
        }
    }
}
