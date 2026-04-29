//! Agent turn dispatch and tool execution logic.

use phantom_agents::agent::AgentMessage;
use phantom_agents::api::{ApiEvent, ApiHandle, ClaudeConfig, send_message};
use phantom_agents::chat::{ChatBackend, ChatRequest};
use phantom_agents::tools::{ToolCall, ToolDefinition, ToolResult, ToolType, available_tools};
use phantom_agents::agent::Agent;

use super::{AgentPane, AgentPaneStatus, MAX_TOOL_ROUNDS};

/// Format tool arguments as a compact, human-readable string.
pub(super) fn format_tool_args(tool: &ToolType, args: &serde_json::Value) -> String {
    match tool {
        ToolType::ReadFile | ToolType::EditFile | ToolType::ListFiles => args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string(),
        ToolType::WriteFile => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            let len = args
                .get("content")
                .and_then(|v| v.as_str())
                .map(|s| s.len())
                .unwrap_or(0);
            format!("{path} ({len} bytes)")
        }
        ToolType::RunCommand => args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string(),
        ToolType::SearchFiles => args
            .get("pattern")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string(),
        ToolType::GitStatus | ToolType::GitDiff => String::new(),
    }
}

impl AgentPane {
    /// Dispatch one turn of the chat conversation.
    ///
    /// Routes through [`ChatBackend::complete`] when a backend is configured;
    /// otherwise falls back to [`send_message`] directly so the legacy Claude
    /// path stays byte-for-byte identical when no `--model` was selected.
    pub(super) fn dispatch(
        backend: Option<&dyn ChatBackend>,
        claude_config: &ClaudeConfig,
        agent: &Agent,
        tools: &[ToolDefinition],
        tool_use_ids: &[String],
    ) -> ApiHandle {
        if let Some(backend) = backend {
            let request = ChatRequest {
                agent,
                tools,
                tool_use_ids,
                max_tokens: claude_config.max_tokens,
            };
            match backend.complete(request) {
                Ok(response) => response.into_handle(),
                Err(e) => {
                    // Surface the error through an ApiHandle so the existing
                    // poll() loop renders it consistently with network errors
                    // from send_message.
                    let (tx, rx) = std::sync::mpsc::channel();
                    let _ = tx.send(ApiEvent::Error(format!(
                        "chat backend ({}) error: {e}",
                        backend.name()
                    )));
                    ApiHandle::from_receiver(rx)
                }
            }
        } else {
            send_message(claude_config, agent, tools, tool_use_ids)
        }
    }

    /// Execute all pending tool calls, append results to the conversation,
    /// and re-invoke the Claude API for the next turn.
    pub(super) fn execute_pending_tools(&mut self) {
        use log::warn;

        if self.turn_count >= MAX_TOOL_ROUNDS {
            if let Some(ref mut j) = self.journal {
                if let Err(e) = j.record_flatline(
                    self.agent.id() as u64,
                    format!("iteration limit reached ({MAX_TOOL_ROUNDS} tool rounds)"),
                ) {
                    warn!("AgentJournal::record_flatline (limit) failed: {e}");
                }
            }
            self.output.push_str(&format!(
                "\n\n✗ Agent hit iteration limit ({MAX_TOOL_ROUNDS} tool rounds).\n"
            ));
            self.rollback_if_dirty();
            self.status = AgentPaneStatus::Failed;
            self.api_handle = None;
            self.push_snapshot();
            self.save_conversation();
            return;
        }
        self.turn_count += 1;

        // Append all tool calls to the agent's message history.
        for (_, call) in &self.pending_tools {
            self.agent
                .push_message(AgentMessage::ToolCall(call.clone()));
        }

        // Build the dispatch context once per turn so chat / composer tools
        // can fork-route by name through the same registry / event-log /
        // spawn-queue handles. When `set_substrate_handles` hasn't been
        // called (legacy / test path), we fall through to the per-tool
        // `execute_tool_with_provenance` path which only honors the
        // file/git surface.
        let working_dir = self.working_dir.clone();
        // Snapshot the substrate handles up front so we can drop the
        // immutable borrow on `self` before the body of the loop touches
        // mutable state (`tool_call_count`, `pending_tools.drain`, etc.).
        let calls: Vec<(String, ToolCall)> = self.pending_tools.drain(..).collect();

        // Execute each tool (with permission check) and append results.
        for (_, call) in calls {
            self.tool_call_count += 1;
            let start = std::time::Instant::now();
            let dispatch_ctx = self.build_dispatch_context();
            let result = if let Err(denied) = self.permissions.check_tool(&call.tool) {
                // Tag the synthetic permission-denied result with provenance
                // so source_chain_for_last_call() still finds it.
                ToolResult {
                    tool: call.tool,
                    success: false,
                    output: denied.to_string(),
                    ..ToolResult::default()
                }
                .with_provenance(phantom_agents::tools::ToolProvenance::from_call(
                    call.tool, &call.args, None,
                ))
            } else if let Some(ctx) = dispatch_ctx.as_ref() {
                // Substrate-aware fork: `dispatch_tool` routes by tool name
                // through file/git → chat → composer surfaces, enforcing
                // the role-class gate at a single check site. Provenance is
                // re-tagged on the way out so the audit log stays consistent.
                phantom_agents::dispatch::dispatch_tool(call.tool.api_name(), &call.args, ctx)
                    .with_provenance(phantom_agents::tools::ToolProvenance::from_call(
                        call.tool, &call.args, None,
                    ))
            } else {
                // Legacy path (no substrate handles wired): the file/git
                // surface only. The capability gate inside
                // `execute_tool_with_provenance` runs against
                // `DEFAULT_AGENT_PANE_ROLE` so the role manifest is honored
                // even without the wider dispatch context.
                phantom_agents::tools::execute_tool_with_provenance(
                    call.tool,
                    &call.args,
                    &working_dir,
                    &self.role,
                    None,
                )
            };
            // Capture source_event_id before dropping the dispatch context —
            // needed for Sec.2 provenance wiring in maybe_emit_capability_denied_event.
            let dispatch_source_event_id: Option<u64> =
                dispatch_ctx.as_ref().and_then(|c| c.source_event_id);
            // Drop the dispatch context borrow before mutating `self` below.
            drop(dispatch_ctx);
            let elapsed = start.elapsed();

            // Track file edits for rollback.
            if result.success && matches!(call.tool, ToolType::WriteFile | ToolType::EditFile) {
                self.has_file_edits = true;
            }

            // Display in pane.
            let status_char = if result.success { "✓" } else { "✗" };
            self.output
                .push_str(&format!("  {} {:.0}ms\n", status_char, elapsed.as_millis(),));

            // Show truncated output (max 200 chars for display).
            if result.output.len() > 200 {
                let truncated: String = result.output.chars().take(200).collect();
                self.output.push_str(&format!(
                    "  ← {}... ({} bytes)\n",
                    truncated,
                    result.output.len()
                ));
            } else if !result.output.is_empty() {
                self.output.push_str(&format!(
                    "  ← {}\n",
                    result.output.lines().next().unwrap_or("")
                ));
            }

            // Sec.1/2 capability-denial instrumentation: when the dispatch
            // gate refused the call, surface a `CapabilityDenied` substrate
            // event + matching audit-log entry. Sec.2 populates source_chain
            // via dispatch_source_event_id when the event log is wired.
            self.maybe_emit_capability_denied_event(
                call.tool,
                &call.args,
                &result,
                dispatch_source_event_id,
            );

            // Lars fix-thread instrumentation (Phase 2.E producer).
            //
            // Track consecutive tool-call failures so a stuck agent surfaces
            // an `EventKind::AgentBlocked` event into the substrate runtime,
            // which the Fixer spawn rule consumes. Successful results reset
            // the streak.
            if result.success {
                self.consecutive_tool_failures = 0;
                self.last_tool_error = None;
                self.last_failing_capability = None;
            } else {
                self.consecutive_tool_failures = self.consecutive_tool_failures.saturating_add(1);
                // Truncate the error excerpt so the eventual `reason` field
                // doesn't drag a multi-KB tool error into the spawn payload.
                let excerpt: String = result.output.chars().take(160).collect();
                self.last_tool_error = Some(excerpt);
                self.last_failing_capability = Some(call.tool.capability_class());
            }

            // Push structured semantic output into the agent's ring-buffer so
            // the model can reason about structured command results on the next
            // turn. Only `RunCommand` results carry a `semantic_output`; all
            // other tool types leave it `None`.
            if let Some(ref parsed) = result.semantic_output {
                self.agent.push_semantic_output(*parsed.clone());
            }

            self.agent.push_message(AgentMessage::ToolResult(result));
        }

        // After the per-turn batch settles, check whether the streak crossed
        // the block threshold. We check once per turn (not once per tool
        // result within the turn) so a single noisy turn with N failing calls
        // only emits one event — matching the spawn rule's
        // `SpawnIfNotRunning` idempotency.
        self.maybe_emit_blocked_event();

        // Re-invoke the chat backend with the updated conversation.
        let tools = available_tools();
        let handle = Self::dispatch(
            self.chat_backend.as_deref(),
            &self.claude_config,
            &self.agent,
            &tools,
            &self.tool_use_ids,
        );
        self.api_handle = Some(handle);

        self.output
            .push_str(&format!("\n● Continuing... (turn {})\n", self.turn_count));
    }
}
