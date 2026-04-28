//! MCP tools as first-class agent tools.
//!
//! Phantom already speaks MCP to external clients (see `phantom-mcp::server`
//! and `phantom-mcp::listener`). Internally, the agent runtime owns its own
//! `tools.rs` flat list (read_file, run_command, …). When an agent is asked
//! "screenshot the terminal" it currently hallucinates "I don't have access
//! to screenshots" because the MCP tool surface was never reflected into the
//! agent's tool registry.
//!
//! This module bridges that gap. It declares a [`McpToolDef`] for every
//! externally-advertised MCP method and tags each with a [`CapabilityClass`].
//! [`mcp_tool_definitions`] returns the subset a given role's manifest
//! permits — Sense+Reflect for Watcher/Capturer, all-but-Act for
//! Conversational, the full set for Actor.
//!
//! **Phase 1.D scope:** definitions only. No live wiring to `AppCommand`.
//! Dispatch lands in Phase 2 alongside the `MockMcpDispatcher` shape we
//! sketch here for tests.

use serde::Serialize;

use crate::role::{AgentRole, CapabilityClass};
use crate::tools::{DispatchError, check_capability};

// ---------------------------------------------------------------------------
// McpToolDef
// ---------------------------------------------------------------------------

/// A single MCP method, packaged as something the agent runtime can hand to
/// the model in an API request and (eventually) dispatch over the live
/// command channel.
///
/// Mirrors the `ToolDefinition` shape in [`crate::tools`] so the two lists
/// can be concatenated by callers — the agent presents a flat tool surface
/// to the model regardless of where each tool came from. The extra `class`
/// field is what role-filtering gates on; it isn't serialized to the wire.
///
/// We intentionally do NOT derive `Deserialize` here. The model never sends
/// us tool definitions — it only sends tool *calls*. Round-tripping a
/// definition would force `CapabilityClass: Default`, which would silently
/// turn an unknown tool into an Act-class one (or whatever the default is).
/// Better to keep the type one-way: code constructs it, serde sends it.
#[derive(Debug, Clone, Serialize)]
pub struct McpToolDef {
    /// Wire name sent to the model. Matches the MCP method exactly
    /// (e.g. `phantom.screenshot`) so the listener can round-trip it.
    pub name: String,
    /// Human-readable description copied from the MCP server's catalog.
    pub description: String,
    /// JSON-Schema for the tool's arguments — same shape MCP advertises.
    pub parameters: serde_json::Value,
    /// Capability class used for role-manifest gating. Not serialized to
    /// the model.
    #[serde(skip)]
    pub class: CapabilityClass,
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

/// Every MCP tool currently advertised by `phantom-mcp::server::builtin_tools`,
/// tagged with its capability class. Order is stable.
pub fn all_mcp_tools() -> Vec<McpToolDef> {
    vec![
        // -- Sense ---------------------------------------------------------
        McpToolDef {
            name: "phantom.screenshot".into(),
            description: "Capture the terminal state as a PNG. Returns the saved path.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pane_id": {"type": "string", "description": "Pane to capture (optional)"},
                },
            }),
            class: CapabilityClass::Sense,
        },
        McpToolDef {
            name: "phantom.read_output".into(),
            description: "Read recent lines from the focused pane's scrollback.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pane_id": {"type": "string", "description": "Pane to read from (optional)"},
                    "lines": {"type": "integer", "description": "Number of lines to read"},
                },
            }),
            class: CapabilityClass::Sense,
        },
        McpToolDef {
            name: "phantom.get_context".into(),
            description: "Get project context (language, framework, git state).".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
            }),
            class: CapabilityClass::Sense,
        },
        McpToolDef {
            name: "phantom.get_memory".into(),
            description: "Read a value from project memory.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string", "description": "Memory key to read"},
                },
                "required": ["key"],
            }),
            class: CapabilityClass::Sense,
        },
        // -- Reflect -------------------------------------------------------
        McpToolDef {
            name: "phantom.set_memory".into(),
            description: "Write a value to project memory.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string", "description": "Memory key"},
                    "value": {"type": "string", "description": "Value to store"},
                },
                "required": ["key", "value"],
            }),
            class: CapabilityClass::Reflect,
        },
        // -- Act -----------------------------------------------------------
        McpToolDef {
            name: "phantom.send_key".into(),
            description: "Send a keypress to the focused pane (or dismiss the boot screen). \
                          Named keys: Enter, Tab, Escape, Space, Backspace, Up, Down, Left, Right."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string", "description": "Named key or literal character(s)"},
                },
                "required": ["key"],
            }),
            class: CapabilityClass::Act,
        },
        McpToolDef {
            name: "phantom.command".into(),
            description: "Execute a Phantom command (backtick mode): theme, debug, plain, \
                          boot, agent <prompt>, reload, quit."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The Phantom command to execute"},
                },
                "required": ["command"],
            }),
            class: CapabilityClass::Act,
        },
        McpToolDef {
            name: "phantom.split_pane".into(),
            description: "Create a new pane by splitting an existing one.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "direction": {
                        "type": "string",
                        "enum": ["horizontal", "vertical"],
                        "description": "Split direction",
                    },
                    "pane_id": {"type": "string", "description": "Pane to split (optional)"},
                },
            }),
            class: CapabilityClass::Act,
        },
        McpToolDef {
            name: "phantom.run_command".into(),
            description: "Execute a shell command in a pane.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The shell command to run"},
                    "pane_id": {"type": "string", "description": "Target pane (optional)"},
                },
                "required": ["command"],
            }),
            class: CapabilityClass::Act,
        },
        // -- Inter-agent chat (chat_tools.rs) ------------------------------
        //
        // Three peer-to-peer messaging tools. Sense gates `send_to_agent` /
        // `read_from_agent` (every agent that can observe peers gets them);
        // Coordinate gates `broadcast_to_role` so Watcher / Capturer / Actor
        // cannot mass-broadcast.
        McpToolDef {
            name: "send_to_agent".into(),
            description: "Send a text body to another running agent by label. \
                          Looks up the recipient in the live agent registry; \
                          delivers an AgentSpeak message to their inbox."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "label": {"type": "string", "description": "Target agent's label (e.g. 'planner-1')"},
                    "body": {"type": "string", "description": "Plain-text body of the message"},
                },
                "required": ["label", "body"],
            }),
            class: CapabilityClass::Sense,
        },
        McpToolDef {
            name: "read_from_agent".into(),
            description: "Read recent messages another agent (by label) has \
                          spoken. Returns up to 20 most-recent agent.speak \
                          envelopes whose id > since_event_id."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "label": {"type": "string", "description": "Source agent's label"},
                    "since_event_id": {
                        "type": "integer",
                        "description": "Cursor: only return envelopes with id > this. Use 0 to read everything."
                    },
                },
                "required": ["label"],
            }),
            class: CapabilityClass::Sense,
        },
        McpToolDef {
            name: "broadcast_to_role".into(),
            description: "Broadcast a text body to every running agent of a \
                          given role. Returns the number of recipients that \
                          accepted the message."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "role": {
                        "type": "string",
                        "description": "Role name (case-insensitive): conversational, watcher, capturer, transcriber, reflector, indexer, actor, composer, fixer."
                    },
                    "body": {"type": "string", "description": "Plain-text body of the message"},
                },
                "required": ["role", "body"],
            }),
            class: CapabilityClass::Coordinate,
        },
    ]
}

// ---------------------------------------------------------------------------
// Role filtering
// ---------------------------------------------------------------------------

/// Return the subset of MCP tools the given role's manifest permits.
///
/// Filtering is the intersection of each tool's [`CapabilityClass`] against
/// the role's allowed classes. A `Watcher` (Sense + Reflect + Compute) gets
/// all the read tools and `set_memory`; an `Actor` (Sense + Reflect +
/// Compute + Act) gets the full catalog; `Conversational` (no Act) gets
/// everything except the world-mutating tools — it must spawn an Actor with
/// consent to perform `Act` operations.
///
/// This is the planning artifact for Phase 1.D. The agent runtime can
/// concatenate the result with [`crate::tools::available_tools`] to present
/// a single flat tool list to the model, with security gating already
/// applied at the source.
pub fn mcp_tool_definitions(role: &AgentRole) -> Vec<McpToolDef> {
    let manifest = role.manifest();
    all_mcp_tools()
        .into_iter()
        .filter(|t| manifest.classes.contains(&t.class))
        .collect()
}

// ---------------------------------------------------------------------------
// Dispatcher trait — Phase 2 will plug `phantom-mcp::AppCommand` in here.
// ---------------------------------------------------------------------------

/// Look up the [`CapabilityClass`] for an MCP tool by wire name.
///
/// Returns `None` for names not in the global catalog (i.e. hallucinated
/// tool names). Dispatchers map this into [`DispatchError::UnknownTool`].
fn mcp_tool_class(tool_name: &str) -> Option<CapabilityClass> {
    all_mcp_tools()
        .into_iter()
        .find(|t| t.name == tool_name)
        .map(|t| t.class)
}

/// Boundary the agent runtime uses to invoke an MCP tool by name.
///
/// **Default-deny at dispatch.** Every implementation MUST gate on the
/// caller's role manifest before doing anything else: a `Watcher`
/// (Sense + Reflect + Compute) calling `phantom.run_command` (Act) MUST
/// be refused with [`DispatchError::CapabilityDenied`], even if the model
/// somehow hallucinated the call past the role-filtered tool list.
/// Names not in the global catalog return [`DispatchError::UnknownTool`]
/// instead of being silently dropped or treated as Act.
///
/// In production this will forward to `phantom_mcp::listener::AppCommand`'s
/// reply-channel pattern; for tests we use [`MockMcpDispatcher`] which
/// returns a canned response after the gate passes.
pub trait McpDispatcher: Send + Sync {
    /// Invoke the tool on behalf of `role`. The implementation is
    /// responsible for any blocking or async I/O. A capability denial
    /// returns [`DispatchError::CapabilityDenied`]; a name not in the
    /// catalog returns [`DispatchError::UnknownTool`].
    fn dispatch(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        role: &AgentRole,
    ) -> Result<serde_json::Value, DispatchError>;
}

/// Test double that returns the same JSON for every call that passes the
/// capability gate. Lets us exercise role-filtering and the agent runtime's
/// dispatch loop without standing up a live `PhantomMcpServer` or socket.
///
/// The mock implements the same default-deny semantics as the production
/// dispatcher: it looks up the tool's [`CapabilityClass`] from
/// [`all_mcp_tools`] and refuses the call when the role's manifest doesn't
/// allow it. This is what makes the test suite a meaningful safety check —
/// a misuse of the trait surface fails the same way in tests as in prod.
pub struct MockMcpDispatcher {
    response: serde_json::Value,
}

impl MockMcpDispatcher {
    /// Construct a dispatcher that returns `response` for every call.
    pub fn new(response: serde_json::Value) -> Self {
        Self { response }
    }
}

impl McpDispatcher for MockMcpDispatcher {
    fn dispatch(
        &self,
        tool_name: &str,
        _args: &serde_json::Value,
        role: &AgentRole,
    ) -> Result<serde_json::Value, DispatchError> {
        // 1. Reject names not in the global catalog. Hallucinated tool
        //    names land here — better an `UnknownTool` than a silent
        //    pass-through that gets treated as Act-class by default.
        let class = mcp_tool_class(tool_name).ok_or_else(|| DispatchError::UnknownTool {
            name: tool_name.to_string(),
        })?;
        // 2. Default-deny: confirm the role's manifest allows the tool's
        //    class. This is the core security property — it doesn't matter
        //    whether the name was filtered out of the model's tool list at
        //    advertise time.
        check_capability(role, class)?;
        Ok(self.response.clone())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Names of the MCP tools the externally-facing server advertises.
    /// Keep this list in sync with `phantom-mcp::server::builtin_tools`.
    const ALL_NAMES: &[&str] = &[
        "phantom.screenshot",
        "phantom.read_output",
        "phantom.get_context",
        "phantom.get_memory",
        "phantom.set_memory",
        "phantom.send_key",
        "phantom.command",
        "phantom.split_pane",
        "phantom.run_command",
    ];

    /// Inter-agent chat tool names (chat_tools.rs). These live in the same
    /// MCP catalog so the manifest filter applies the same role gates.
    const CHAT_NAMES: &[&str] = &[
        "send_to_agent",
        "read_from_agent",
        "broadcast_to_role",
    ];

    fn names(defs: &[McpToolDef]) -> HashSet<String> {
        defs.iter().map(|d| d.name.clone()).collect()
    }

    // -- Catalog completeness ----------------------------------------------

    #[test]
    fn all_mcp_tools_covers_every_advertised_method() {
        let n = names(&all_mcp_tools());
        for expected in ALL_NAMES {
            assert!(n.contains(*expected), "missing MCP tool: {expected}");
        }
        for expected in CHAT_NAMES {
            assert!(n.contains(*expected), "missing chat tool: {expected}");
        }
        assert_eq!(
            n.len(),
            ALL_NAMES.len() + CHAT_NAMES.len(),
            "extra tools in catalog",
        );
    }

    #[test]
    fn every_tool_has_object_schema() {
        for tool in all_mcp_tools() {
            assert!(
                tool.parameters.is_object(),
                "tool {} has non-object params",
                tool.name
            );
            assert_eq!(
                tool.parameters.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "tool {} schema type != object",
                tool.name
            );
        }
    }

    // -- Role filtering ----------------------------------------------------

    #[test]
    fn watcher_gets_sense_and_reflect_tools() {
        let n = names(&mcp_tool_definitions(&AgentRole::Watcher));
        // Sense
        assert!(n.contains("phantom.screenshot"));
        assert!(n.contains("phantom.read_output"));
        assert!(n.contains("phantom.get_context"));
        assert!(n.contains("phantom.get_memory"));
        // Reflect
        assert!(n.contains("phantom.set_memory"));
        // No Act
        assert!(!n.contains("phantom.send_key"));
        assert!(!n.contains("phantom.command"));
        assert!(!n.contains("phantom.split_pane"));
        assert!(!n.contains("phantom.run_command"));
        // Sense-class chat tools yes; Coordinate no.
        assert!(n.contains("send_to_agent"));
        assert!(n.contains("read_from_agent"));
        assert!(!n.contains("broadcast_to_role"));
        // 5 phantom.* + 2 chat (send + read).
        assert_eq!(n.len(), 7);
    }

    #[test]
    fn capturer_gets_sense_and_reflect_tools() {
        // Capturer has Sense + Reflect (no Compute, no Act). Its tool
        // surface is the same as Watcher's at the MCP level.
        let n = names(&mcp_tool_definitions(&AgentRole::Capturer));
        assert!(n.contains("phantom.screenshot"));
        assert!(n.contains("phantom.read_output"));
        assert!(n.contains("phantom.get_context"));
        assert!(n.contains("phantom.get_memory"));
        assert!(n.contains("phantom.set_memory"));
        assert!(!n.contains("phantom.run_command"));
        assert!(!n.contains("phantom.send_key"));
        // Capturer holds Sense → gets the two read/send chat tools but not
        // broadcast (Coordinate).
        assert!(n.contains("send_to_agent"));
        assert!(n.contains("read_from_agent"));
        assert!(!n.contains("broadcast_to_role"));
        assert_eq!(n.len(), 7);
    }

    #[test]
    fn actor_gets_all_tools_including_act() {
        let n = names(&mcp_tool_definitions(&AgentRole::Actor));
        for expected in ALL_NAMES {
            assert!(n.contains(*expected), "Actor missing {expected}");
        }
        // Actor holds Sense (so chat send/read) but not Coordinate.
        assert!(n.contains("send_to_agent"));
        assert!(n.contains("read_from_agent"));
        assert!(
            !n.contains("broadcast_to_role"),
            "Actor must not have broadcast (Coordinate)",
        );
        assert_eq!(n.len(), ALL_NAMES.len() + 2);
    }

    #[test]
    fn conversational_gets_all_minus_act() {
        let n = names(&mcp_tool_definitions(&AgentRole::Conversational));
        // Sense
        assert!(n.contains("phantom.screenshot"));
        assert!(n.contains("phantom.read_output"));
        assert!(n.contains("phantom.get_context"));
        assert!(n.contains("phantom.get_memory"));
        // Reflect
        assert!(n.contains("phantom.set_memory"));
        // No Act — must spawn an Actor with consent for these.
        assert!(!n.contains("phantom.send_key"));
        assert!(!n.contains("phantom.command"));
        assert!(!n.contains("phantom.split_pane"));
        assert!(!n.contains("phantom.run_command"));
        // Conversational holds Coordinate → gets all three chat tools.
        assert!(n.contains("send_to_agent"));
        assert!(n.contains("read_from_agent"));
        assert!(n.contains("broadcast_to_role"));
        // 5 phantom.* + 3 chat.
        assert_eq!(n.len(), 8);
    }

    #[test]
    fn capturer_and_watcher_filter_to_same_mcp_set() {
        // Both roles share Sense + Reflect at the MCP layer. Watcher's
        // extra Compute capability gates LLM use, not MCP tools.
        assert_eq!(
            names(&mcp_tool_definitions(&AgentRole::Watcher)),
            names(&mcp_tool_definitions(&AgentRole::Capturer)),
        );
    }

    #[test]
    fn filter_is_subset_of_full_catalog_for_every_role() {
        let full = names(&all_mcp_tools());
        for role in [
            AgentRole::Conversational,
            AgentRole::Watcher,
            AgentRole::Capturer,
            AgentRole::Transcriber,
            AgentRole::Reflector,
            AgentRole::Indexer,
            AgentRole::Actor,
            AgentRole::Composer,
        ] {
            let got = names(&mcp_tool_definitions(&role));
            assert!(
                got.is_subset(&full),
                "{role:?} produced tools outside the catalog: {got:?}"
            );
        }
    }

    #[test]
    fn transcriber_sees_no_sense_tools() {
        // Transcriber holds Compute + Reflect — no Sense. So it gets
        // `set_memory` and nothing else from this catalog.
        let n = names(&mcp_tool_definitions(&AgentRole::Transcriber));
        assert!(n.contains("phantom.set_memory"));
        assert!(!n.contains("phantom.screenshot"));
        assert!(!n.contains("phantom.read_output"));
        assert_eq!(n.len(), 1);
    }

    // -- MockMcpDispatcher -------------------------------------------------

    #[test]
    fn mock_dispatcher_returns_canned_response() {
        let payload = serde_json::json!({
            "content": [{"type": "text", "text": "ok"}],
        });
        let mock = MockMcpDispatcher::new(payload.clone());
        let got = mock
            .dispatch("phantom.screenshot", &serde_json::json!({}), &AgentRole::Actor)
            .expect("mock should succeed");
        assert_eq!(got, payload);
    }

    #[test]
    fn mock_dispatcher_is_callable_for_every_tool_name() {
        // Actor's manifest covers Sense + Reflect + Compute + Act — but NOT
        // Coordinate, so the Coordinate-class chat tool (`broadcast_to_role`)
        // is correctly default-denied for Actor. Use Composer (which holds
        // Coordinate) to assert that every catalog tool's class is reachable
        // by *some* role; the per-role gating is exercised in dedicated tests
        // above.
        let mock = MockMcpDispatcher::new(serde_json::json!({"content": []}));
        for tool in all_mcp_tools() {
            let role = if tool.class == CapabilityClass::Act {
                AgentRole::Actor
            } else {
                AgentRole::Composer
            };
            let res = mock.dispatch(&tool.name, &serde_json::json!({}), &role);
            assert!(res.is_ok(), "dispatch failed for {} as {role:?}", tool.name);
        }
    }

    #[test]
    fn mock_dispatcher_is_object_safe() {
        // Constructable as `dyn McpDispatcher` — required so the agent
        // runtime can hold whichever implementation is wired in.
        let mock: Box<dyn McpDispatcher> =
            Box::new(MockMcpDispatcher::new(serde_json::json!({"ok": true})));
        let res = mock
            .dispatch(
                "phantom.get_context",
                &serde_json::json!({}),
                &AgentRole::Actor,
            )
            .expect("dispatch should succeed");
        assert_eq!(res["ok"], true);
    }

    // -- Default-deny dispatch gate ----------------------------------------

    #[test]
    fn mock_dispatcher_enforces_role_gate() {
        // Capturer holds Sense + Reflect — no Act. Calling an Act-class
        // MCP tool (`phantom.command`) MUST be refused with
        // CapabilityDenied { role: Capturer, tool_class: Act }, regardless
        // of the canned response the mock would otherwise return.
        let mock = MockMcpDispatcher::new(serde_json::json!({"content": []}));
        let err = mock
            .dispatch(
                "phantom.command",
                &serde_json::json!({"command": "should_not_run"}),
                &AgentRole::Capturer,
            )
            .expect_err("Capturer must not be allowed to invoke Act-class tools");
        assert_eq!(
            err,
            DispatchError::CapabilityDenied {
                role: AgentRole::Capturer,
                tool_class: CapabilityClass::Act,
            }
        );
    }

    #[test]
    fn unknown_tool_name_returns_unknown_tool_error() {
        // A name that doesn't exist in the global catalog must surface as
        // UnknownTool, NOT CapabilityDenied. This distinguishes "the model
        // hallucinated a tool that doesn't exist" from "the model named a
        // real tool its role can't invoke" — the agent runtime can react
        // differently to each (e.g. tell the model to stop hallucinating
        // vs. tell it to spawn an Actor with consent).
        let mock = MockMcpDispatcher::new(serde_json::json!({"content": []}));
        let err = mock
            .dispatch("fake_tool", &serde_json::json!({}), &AgentRole::Actor)
            .expect_err("fake_tool is not in the catalog");
        assert!(
            matches!(err, DispatchError::UnknownTool { ref name } if name == "fake_tool"),
            "expected UnknownTool, got {err:?}",
        );
    }

    #[test]
    fn watcher_act_class_mcp_tool_returns_capability_denied() {
        // Watcher's manifest is Sense + Reflect + Compute. Trying to call
        // `phantom.run_command` (Act) must be refused, even though Watcher
        // is a long-lived role that *might* have leaked the tool name from
        // a prompt-injected context.
        let mock = MockMcpDispatcher::new(serde_json::json!({"ok": true}));
        let err = mock
            .dispatch(
                "phantom.run_command",
                &serde_json::json!({"command": "ls"}),
                &AgentRole::Watcher,
            )
            .expect_err("Watcher must not be allowed Act-class tools");
        assert_eq!(
            err,
            DispatchError::CapabilityDenied {
                role: AgentRole::Watcher,
                tool_class: CapabilityClass::Act,
            }
        );
    }

    #[test]
    fn unknown_tool_takes_precedence_over_capability_check() {
        // For a name not in the catalog, we have no class to check against
        // — UnknownTool is the right answer for *any* role, including ones
        // with no Act capability. This keeps the dispatcher's failure modes
        // disjoint and unambiguous.
        let mock = MockMcpDispatcher::new(serde_json::json!({"content": []}));
        let err = mock
            .dispatch(
                "definitely.not.a.tool",
                &serde_json::json!({}),
                &AgentRole::Watcher,
            )
            .expect_err("name is not in the catalog");
        assert!(matches!(err, DispatchError::UnknownTool { .. }));
    }

    #[test]
    fn mcp_tool_class_returns_class_for_real_tool() {
        // Sanity check on the helper used by the dispatcher. Every tool in
        // the catalog must have its declared class returned here.
        for tool in all_mcp_tools() {
            assert_eq!(
                mcp_tool_class(&tool.name),
                Some(tool.class),
                "{} class lookup failed",
                tool.name
            );
        }
    }

    #[test]
    fn mcp_tool_class_returns_none_for_unknown_name() {
        assert_eq!(mcp_tool_class("phantom.nonexistent"), None);
        assert_eq!(mcp_tool_class(""), None);
    }

    // -- Inter-agent chat tool catalog gating ------------------------------

    #[test]
    fn watcher_role_does_not_have_broadcast_tool() {
        // Capability gate: a Watcher's manifest is Sense + Reflect + Compute.
        // `broadcast_to_role` is Coordinate-class — must NOT appear in the
        // Watcher's filtered catalog. Same property covers Capturer / Actor.
        let n = names(&mcp_tool_definitions(&AgentRole::Watcher));
        assert!(
            !n.contains("broadcast_to_role"),
            "Watcher must not see broadcast_to_role; got {n:?}",
        );
        // Sense-class chat tools still appear.
        assert!(n.contains("send_to_agent"));
        assert!(n.contains("read_from_agent"));
    }

    #[test]
    fn mcp_tool_definitions_include_chat_tools_for_conversational() {
        // Conversational holds Sense + Reflect + Compute + Coordinate, so
        // every chat tool must surface in its filtered manifest. This is the
        // load-bearing check that the role-filter actually reaches into the
        // catalog and emits the new tools to the model.
        let n = names(&mcp_tool_definitions(&AgentRole::Conversational));
        for tool in CHAT_NAMES {
            assert!(
                n.contains(*tool),
                "Conversational missing chat tool: {tool}",
            );
        }
    }

    #[test]
    fn composer_also_has_broadcast_tool() {
        // Composer holds Coordinate (it spawns / steers other agents). It
        // must therefore have access to broadcast_to_role for fan-out
        // coordination.
        let n = names(&mcp_tool_definitions(&AgentRole::Composer));
        assert!(n.contains("send_to_agent"));
        assert!(n.contains("read_from_agent"));
        assert!(n.contains("broadcast_to_role"));
    }

    #[test]
    fn capturer_does_not_have_broadcast_tool() {
        // Capturer is Sense + Reflect only. Same property as Watcher.
        let n = names(&mcp_tool_definitions(&AgentRole::Capturer));
        assert!(!n.contains("broadcast_to_role"));
        assert!(n.contains("send_to_agent"));
        assert!(n.contains("read_from_agent"));
    }

    #[test]
    fn actor_does_not_have_broadcast_tool() {
        // Actor is Sense + Reflect + Compute + Act. No Coordinate — so
        // broadcast is gated even though Actor is the highest-privilege
        // role for world-mutating tools.
        let n = names(&mcp_tool_definitions(&AgentRole::Actor));
        assert!(!n.contains("broadcast_to_role"));
        assert!(n.contains("send_to_agent"));
        assert!(n.contains("read_from_agent"));
    }
}
