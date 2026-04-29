//! Dispatch context construction and related helpers.

use super::AgentPane;

/// Lowercase string label for a `CapabilityClass`. Used in the audit log's
/// `class` field and the substrate-event payload's `attempted_class` so
/// scrapers can treat both as the same vocabulary.
pub(super) fn class_label(class: phantom_agents::role::CapabilityClass) -> &'static str {
    match class {
        phantom_agents::role::CapabilityClass::Sense => "Sense",
        phantom_agents::role::CapabilityClass::Reflect => "Reflect",
        phantom_agents::role::CapabilityClass::Compute => "Compute",
        phantom_agents::role::CapabilityClass::Act => "Act",
        phantom_agents::role::CapabilityClass::Coordinate => "Coordinate",
    }
}

/// Wall-clock millis since epoch. Best-effort: returns 0 if the system clock
/// is somehow before the epoch.
pub(super) fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Build project context for agent system prompts.
/// Reads CLAUDE.md if it exists, and provides a crate map.
pub(super) fn build_codebase_context() -> String {
    let working_dir = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".into());

    let mut ctx = String::from(
        "CODEBASE CONTEXT:\n\
         You are an agent inside Phantom, an AI-native terminal emulator.\n\
         Written in Rust. 19 crates. ~100K lines. deny(warnings) is enforced.\n\
         Always run `cargo check --workspace` after edits.\n\n\
         Key crates:\n\
         - phantom (binary entry point)\n\
         - phantom-app (GUI: render, input, mouse, coordinator, agent_pane)\n\
         - phantom-brain (OODA loop, scoring, goals, proactive, orchestrator)\n\
         - phantom-agents (tools, API client, permissions, agent lifecycle)\n\
         - phantom-adapter (AppAdapter trait, spatial preferences, event bus)\n\
         - phantom-ui (layout engine, arbiter, themes, keybinds)\n\
         - phantom-terminal (PTY, VTE, SGR mouse encoding)\n\
         - phantom-scene (scene graph, z-order, dirty flags, render layers)\n\
         - phantom-semantic (output parsing, error detection)\n\
         - phantom-context (project detection, git state)\n\
         - phantom-memory (persistent key-value store)\n\
         - phantom-mcp (MCP protocol, Unix socket server/client)\n\n",
    );

    // Try to read CLAUDE.md for project-specific instructions.
    let claude_md = std::path::Path::new(&working_dir).join("CLAUDE.md");
    if let Ok(content) = std::fs::read_to_string(&claude_md) {
        let truncated = if content.len() > 2000 {
            format!("{}...(truncated)", &content[..2000])
        } else {
            content
        };
        ctx.push_str(&format!("CLAUDE.md:\n{truncated}\n\n"));
    }

    ctx
}

impl AgentPane {
    /// Build a [`phantom_agents::dispatch::DispatchContext`] from the
    /// pane's current substrate handles, if all required pieces are wired.
    ///
    /// Returns `None` when the pane was constructed without a runtime
    /// connection (legacy / test fixtures) — callers fall back to the file
    /// /git-only path. Borrows `self.working_dir` as `&Path` so the
    /// returned context's lifetime is tied to `self`'s borrow scope.
    pub(super) fn build_dispatch_context(
        &self,
    ) -> Option<phantom_agents::dispatch::DispatchContext<'_>> {
        let registry = self.registry.clone()?;
        let pending_spawn = self.pending_spawn.clone()?;
        let self_ref = self.self_ref.clone()?;
        // Issue #235: inject the ticket dispatcher only for Dispatcher-role
        // panes. Non-Dispatcher agents receive `None` so the three Dispatcher
        // tools remain unreachable to them (capability gate catches first, but
        // defence-in-depth keeps the `None` path as the safe fallback).
        let ticket_dispatcher = if self.role == phantom_agents::role::AgentRole::Dispatcher {
            self.ticket_dispatcher.clone()
        } else {
            None
        };

        Some(phantom_agents::dispatch::DispatchContext {
            self_ref,
            role: self.role,
            working_dir: std::path::Path::new(self.working_dir.as_str()),
            registry,
            event_log: self.event_log.clone(),
            pending_spawn,
            source_event_id: None,
            // Sec.7.3 fix (#225): pass the real quarantine registry so the
            // dispatch gate can block quarantined agents before any capability
            // check or handler runs. `None` keeps legacy / test paths open.
            quarantine: self.quarantine.clone(),
            correlation_id: None,
            ticket_dispatcher,
            runtime_mode: phantom_agents::dispatch::RuntimeMode::Normal,
        })
    }
}
