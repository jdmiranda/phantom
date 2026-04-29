//! Capture sidecar, snapshot queue, and rollback helpers.

use log::warn;
use phantom_session::AgentSnapshot;

use super::AgentPane;

impl AgentPane {
    /// Push a snapshot of the current agent state into the shared queue.
    ///
    /// No-op when no sink is wired (test / legacy callers). Logs a warning
    /// and drops the snapshot if the mutex is poisoned.
    pub(super) fn push_snapshot(&self) {
        let Some(ref sink) = self.snapshot_sink else {
            return;
        };
        let snapshot = AgentSnapshot::from_agent(&self.agent);
        match sink.lock() {
            Ok(mut q) => q.push(snapshot),
            Err(_) => warn!("snapshot_sink mutex poisoned; dropping AgentSnapshot"),
        }
    }

    /// Revert file edits on failure (git checkout -- .).
    pub(super) fn rollback_if_dirty(&mut self) {
        if !self.has_file_edits {
            return;
        }
        self.output
            .push_str("\n⚠ Agent failed with uncommitted edits. Reverting...\n");
        let result = std::process::Command::new("git")
            .args(["checkout", "--", "."])
            .current_dir(&self.working_dir)
            .output();
        match result {
            Ok(out) if out.status.success() => {
                self.output.push_str("  ← Reverted to clean state.\n");
            }
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                self.output
                    .push_str(&format!("  ← Revert failed: {stderr}\n"));
            }
            Err(e) => {
                self.output.push_str(&format!("  ← Revert failed: {e}\n"));
            }
        }
    }

    /// Flush accumulated tool calls and output text to the capture sidecar.
    ///
    /// Called at `ApiEvent::Done` (agent finished) and `ApiEvent::Error`
    /// (agent failed). Drains `self.capture_tool_calls` and appends one
    /// `AgentRecord` to the sidecar. No-ops when no sidecar is configured.
    pub(super) fn flush_capture_record(&mut self) {
        let Some(ref capture) = self.agent_capture else {
            return;
        };
        // Cap text output to 4 096 chars so the sidecar stays bounded even
        // for chatty agents.  We take the tail (most-recent output) on the
        // assumption that the end of the run is more useful than the start.
        let text_output: String = if self.output.chars().count() > 4096 {
            let tail: String = self.output.chars().rev().take(4096).collect();
            tail.chars().rev().collect()
        } else {
            self.output.clone()
        };
        let tool_calls = std::mem::take(&mut self.capture_tool_calls);
        let session_uuid = self.capture_session_uuid;
        let agent_name = self.task.clone();
        if let Err(e) = capture.append(agent_name, session_uuid, tool_calls, text_output) {
            warn!("agent capture append failed: {e}");
        }
    }
}
