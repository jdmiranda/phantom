//! System-prompt block builder for spawned agents.
//!
//! Every agent we launch needs a system-prompt preamble that pins down:
//!
//! 1. **Who it is** — the role manifest paragraph (label, id, capability
//!    classes, role-specific description). Without this, models hallucinate
//!    tool surfaces and refuse capabilities they actually have ("I don't
//!    have access to screenshots" when the manifest says they do).
//! 2. **What its boundary is** — an explicit instruction to *say so and
//!    request escalation* rather than fabricate tool calls.
//! 3. **What it's been asked to do** — the task, project context, workspace
//!    inventory, and tool list, each as their own optional section.
//! 4. **How to cite observations** — pane-id attribution rule on the closing
//!    line.
//!
//! The builder is intentionally narrow: pure assembly of strings, no I/O,
//! no LLM access, no global state. That makes it cheap to test exhaustively
//! per role and trivially deterministic.
//!
//! The output is a single `String` joined by blank lines between sections.
//! Callers feed it as the `system` field on the first request to the model.

use crate::role::AgentRef;

/// Trailing instruction that closes every built prompt. Centralized as a
/// constant so tests can assert exact match without duplicating the literal.
const CLOSING_LINE: &str =
    "Always cite the pane id you're reading from when summarizing observations.";

/// Capability-boundary statement. Reinforces the manifest paragraph's own
/// boundary clause as a standalone sentence so it stays salient even if the
/// model truncates context aggressively.
const BOUNDARY_LINE: &str =
    "If you need a capability you don't have, say so and request escalation \
     — do NOT invent tool calls.";

/// Composer-only protocol addendum. Pinned verbatim — the dispatcher,
/// recipient agents, and the user-facing report all parse for the
/// AGREE/DISAGREE/PARTIAL tokens, so paraphrasing this paragraph would
/// silently break the debate-protocol contract.
#[allow(dead_code)] // Phase 2 composer wiring will reference this.
const COMPOSER_PROTOCOL: &str = "\
You are a COMPOSER. Your job is to PLAN and DELEGATE, not to do the work yourself.

Protocol:
1. Decompose the user's task into atomic sub-tasks.
2. For each sub-task, assign it to a sub-agent (existing label or spawn one via spawn_subagent).
3. send_to_agent <label> \"<sub-task>\"; wait_for_agent for the result.
4. If your task involves debate or quality control:
   a. After A produces an answer, request_critique from B.
   b. If B replies AGREE -> accept. If DISAGREE -> ask A to defend or revise; one round.
   c. If still disagreement after the round, force a confidence vote from each, return the higher-confidence answer with BOTH views shown.
5. NEVER paper over a real disagreement with a hedge. If A and B disagree and you cannot resolve in one round, surface it.
6. Compose the final answer for the user with attribution: who proposed what, who critiqued, what was kept, what was rejected and why.
7. When the task is complete, emit an event of kind \"agent.complete\" via append_event_log.

You CANNOT use Act-class tools (write_file, run_command, send_key, etc.). If a step requires acting on the user's world, the user must spawn an Actor agent and grant explicit consent.";

/// Fluent builder that assembles an agent's system-prompt block.
///
/// Construct with [`SystemPromptBuilder::new`] giving the agent's [`AgentRef`],
/// then attach optional context with the `with_*` setters. Call [`build`] to
/// materialize the final `String`.
///
/// All setters return `self` by value so chains compose. The builder is
/// `Clone`-able if a caller wants to reuse the base over several variants,
/// but the canonical use is one-shot at spawn time.
///
/// [`build`]: SystemPromptBuilder::build
pub struct SystemPromptBuilder {
    pub agent: AgentRef,
    pub task: Option<String>,
    pub project_context: Option<String>,
    /// Serialized pane inventory. Format is the caller's choice; we just
    /// drop it under the `## Workspace inventory` heading verbatim.
    pub apps_list: Option<String>,
    /// Human-readable tool list (typically rendered from the role's allowed
    /// tool catalog). Goes under `## Tools you can call`.
    pub tool_summary: Option<String>,
    /// `(role, label)` pairs for *other* agents currently running. When
    /// non-empty, the prompt appends a paragraph describing the chat tools
    /// the agent can use to address them. Empty / `None` keeps the legacy
    /// prompt unchanged.
    pub other_agents: Option<Vec<(crate::role::AgentRole, String)>>,
}

impl SystemPromptBuilder {
    /// Start a new builder for the given agent. All optional sections start
    /// `None` and must be attached explicitly with the `with_*` methods.
    pub fn new(agent: AgentRef) -> Self {
        Self {
            agent,
            task: None,
            project_context: None,
            apps_list: None,
            tool_summary: None,
            other_agents: None,
        }
    }

    /// Attach the task description shown under `## Task`.
    pub fn with_task(mut self, task: String) -> Self {
        self.task = Some(task);
        self
    }

    /// Attach project context (git branch, recent diffs, etc.) shown under
    /// `## Project context`.
    pub fn with_project_context(mut self, ctx: String) -> Self {
        self.project_context = Some(ctx);
        self
    }

    /// Attach the workspace pane inventory shown under
    /// `## Workspace inventory`.
    pub fn with_apps_list(mut self, list: String) -> Self {
        self.apps_list = Some(list);
        self
    }

    /// Attach the tool catalog summary shown under `## Tools you can call`.
    pub fn with_tool_summary(mut self, tools: String) -> Self {
        self.tool_summary = Some(tools);
        self
    }

    /// Attach the list of *other* running agents. The (role, label) pairs
    /// drop into a manifest paragraph that primes the model to use the
    /// inter-agent chat tools when appropriate.
    ///
    /// Pass an empty vector or never call this when no peers are running —
    /// the resulting prompt then contains no co-agent paragraph at all.
    pub fn with_other_agents(
        mut self,
        peers: Vec<(crate::role::AgentRole, String)>,
    ) -> Self {
        self.other_agents = Some(peers);
        self
    }

    /// Materialize the final system prompt.
    ///
    /// Sections are emitted in this fixed order, separated by blank lines:
    ///
    /// 1. Role manifest paragraph (always present).
    /// 2. Capability boundary statement (always present).
    /// 3. `## Task` (if set).
    /// 4. `## Project context` (if set).
    /// 5. `## Workspace inventory` (if set).
    /// 6. `## Tools you can call` (if set).
    /// 7. Closing pane-id citation rule (always present).
    ///
    /// When `agent.role == AgentRole::Composer`, the role-specific
    /// debate/critique protocol is appended after the boundary line so it
    /// stays salient ahead of the variable task / context sections.
    ///
    /// The output is deterministic: building the same builder twice yields
    /// the same `String`.
    pub fn build(&self) -> String {
        use crate::role::AgentRole;
        let mut sections: Vec<String> = Vec::new();

        // 1. Role manifest paragraph.
        let manifest = self.agent.role.manifest();
        sections.push(manifest.system_prompt_paragraph(&self.agent.label, self.agent.id));

        // 2. Capability boundary statement.
        sections.push(BOUNDARY_LINE.to_string());

        // 2b. Composer-only debate / delegation protocol.
        if self.agent.role == AgentRole::Composer {
            sections.push(COMPOSER_PROTOCOL.to_string());
        }

        // 2c. Other-agents paragraph. Surfaces the peer manifest and points
        // the model at the inter-agent chat tools (`send_to_agent`,
        // `read_from_agent`, `broadcast_to_role`). Skipped entirely when
        // there are no peers — keeps the no-peer prompt byte-identical with
        // pre-chat-tools versions so existing snapshot tests still pass.
        if let Some(peers) = &self.other_agents
            && !peers.is_empty()
        {
            let mut s = String::from(
                "Other agents are also running: ",
            );
            for (i, (role, label)) in peers.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                s.push_str(&format!("{{role: {}, label: {label}}}", role.label()));
            }
            s.push_str(
                ". Use `send_to_agent` to talk to them directly, \
                 `read_from_agent` to read what they've said, \
                 `broadcast_to_role` to message all agents of a role.",
            );
            sections.push(s);
        }

        // 3. Task.
        if let Some(task) = &self.task {
            sections.push(format!("## Task\n{task}"));
        }

        // 4. Project context.
        if let Some(ctx) = &self.project_context {
            sections.push(format!("## Project context\n{ctx}"));
        }

        // 5. Workspace inventory.
        if let Some(list) = &self.apps_list {
            sections.push(format!("## Workspace inventory\n{list}"));
        }

        // 6. Tools you can call.
        if let Some(tools) = &self.tool_summary {
            sections.push(format!("## Tools you can call\n{tools}"));
        }

        // 7. Closing line.
        sections.push(CLOSING_LINE.to_string());

        sections.join("\n\n")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::role::{AgentRef, AgentRole, SpawnSource};

    /// Helper: build a fresh AgentRef for a given role.
    fn agent(role: AgentRole, label: &str, id: u64) -> AgentRef {
        AgentRef::new(id, role, label, SpawnSource::User)
    }

    // --- structural assertions ------------------------------------------------

    #[test]
    fn empty_builder_has_only_manifest_boundary_and_closing() {
        // With no optional sections set, the prompt must contain exactly
        // sections 1, 2, and 7 — no headings at all.
        let prompt = SystemPromptBuilder::new(agent(AgentRole::Watcher, "w1", 1)).build();
        assert!(!prompt.contains("## Task"), "no Task heading expected");
        assert!(!prompt.contains("## Project context"), "no Project context heading");
        assert!(!prompt.contains("## Workspace inventory"), "no Workspace inventory");
        assert!(!prompt.contains("## Tools you can call"), "no Tools heading");
        // But the always-present pieces must be there.
        assert!(prompt.contains("Watcher"), "manifest paragraph missing");
        assert!(prompt.contains("invent tool calls"), "boundary line missing");
        assert!(prompt.ends_with(CLOSING_LINE), "closing line must terminate prompt");
    }

    #[test]
    fn with_task_inserts_task_section_after_boundary() {
        let prompt = SystemPromptBuilder::new(agent(AgentRole::Conversational, "chat", 7))
            .with_task("Find the cause of the panic".to_string())
            .build();

        let task_idx = prompt.find("## Task").expect("Task heading present");
        let boundary_idx = prompt.find("invent tool calls").expect("boundary present");
        let closing_idx = prompt.find(CLOSING_LINE).expect("closing present");

        assert!(boundary_idx < task_idx, "Task must follow boundary");
        assert!(task_idx < closing_idx, "Task must precede closing line");
        assert!(prompt.contains("Find the cause of the panic"));
    }

    #[test]
    fn apps_and_tools_appear_in_declared_order() {
        // When both apps_list and tool_summary are set, Workspace inventory
        // must appear before Tools you can call.
        let prompt = SystemPromptBuilder::new(agent(AgentRole::Composer, "comp", 11))
            .with_apps_list("pane:0 = zsh\npane:1 = vim".to_string())
            .with_tool_summary("phantom.screenshot, phantom.read_output".to_string())
            .build();

        let apps_idx = prompt.find("## Workspace inventory").expect("apps heading");
        let tools_idx = prompt.find("## Tools you can call").expect("tools heading");
        assert!(apps_idx < tools_idx, "inventory must precede tools");
        assert!(prompt.contains("pane:0 = zsh"), "apps body present");
        assert!(prompt.contains("phantom.screenshot"), "tools body present");
    }

    #[test]
    fn build_is_deterministic() {
        // Running build() twice on the same builder must yield byte-identical
        // strings — no timestamps, no nondeterministic iteration.
        let builder = SystemPromptBuilder::new(agent(AgentRole::Actor, "a1", 99))
            .with_task("apply the fix".to_string())
            .with_project_context("rust workspace".to_string())
            .with_apps_list("pane:0".to_string())
            .with_tool_summary("WriteFile".to_string());

        let a = builder.build();
        let b = builder.build();
        assert_eq!(a, b, "build must be deterministic");
    }

    // --- per-role assertions -------------------------------------------------

    /// Assert generic structural properties for every role: the prompt
    /// surfaces label, id, and at least one declared capability, and ends
    /// with the closing line.
    #[test]
    fn every_role_prompt_includes_label_id_capability_and_closing() {
        // (role, label, id, expected_capability_keyword)
        let cases = [
            (AgentRole::Conversational, "chat", 1u64, "Sense"),
            (AgentRole::Watcher, "watch", 2, "Sense"),
            (AgentRole::Capturer, "cap", 3, "Sense"),
            (AgentRole::Transcriber, "trans", 4, "Compute"),
            (AgentRole::Reflector, "refl", 5, "Sense"),
            (AgentRole::Indexer, "idx", 6, "Sense"),
            (AgentRole::Actor, "act", 7, "Act"),
            (AgentRole::Composer, "comp", 8, "Coordinate"),
        ];
        for (role, label, id, cap) in cases {
            let prompt = SystemPromptBuilder::new(agent(role, label, id)).build();
            assert!(prompt.contains(label), "{role:?}: label `{label}` missing");
            assert!(
                prompt.contains(&id.to_string()),
                "{role:?}: id {id} missing from prompt"
            );
            assert!(
                prompt.contains(cap),
                "{role:?}: capability keyword `{cap}` missing"
            );
            assert!(
                prompt.ends_with(CLOSING_LINE),
                "{role:?}: closing line missing"
            );
        }
    }

    #[test]
    fn watcher_prompt_mentions_observation() {
        // The Watcher role description leans on observation language —
        // verify either "observe" or "subscribe" surfaces somewhere
        // (the manifest paragraph drops in the description verbatim).
        let prompt = SystemPromptBuilder::new(agent(AgentRole::Watcher, "ob", 1)).build();
        let lower = prompt.to_lowercase();
        assert!(
            lower.contains("observ") || lower.contains("subscribe"),
            "Watcher prompt should mention observe/subscribe; got:\n{prompt}"
        );
    }

    #[test]
    fn capturer_prompt_mentions_no_llm() {
        // Capturer's manifest description is "No LLM, no acting." We need
        // the model to know it cannot reason — capture only.
        let prompt = SystemPromptBuilder::new(agent(AgentRole::Capturer, "cap", 1)).build();
        let lower = prompt.to_lowercase();
        assert!(
            lower.contains("no llm"),
            "Capturer prompt must say 'no LLM'; got:\n{prompt}"
        );
    }

    #[test]
    fn conversational_prompt_mentions_label_and_coordinate() {
        let prompt =
            SystemPromptBuilder::new(agent(AgentRole::Conversational, "chat", 1)).build();
        assert!(prompt.contains("Conversational"), "label missing");
        assert!(
            prompt.contains("Coordinate"),
            "Conversational must declare Coordinate capability; got:\n{prompt}"
        );
    }

    #[test]
    fn actor_prompt_mentions_act_capability_and_consent_guard() {
        // Actor is the only role with Act, and its description warns about
        // explicit user consent. Both must surface.
        let prompt = SystemPromptBuilder::new(agent(AgentRole::Actor, "act", 1)).build();
        let lower = prompt.to_lowercase();
        assert!(
            lower.contains("act ("),
            "Actor prompt must declare Act capability; got:\n{prompt}"
        );
        assert!(
            lower.contains("consent"),
            "Actor prompt must mention user consent guard; got:\n{prompt}"
        );
    }

    /// Composer-only protocol must surface verbatim. We assert on the
    /// load-bearing tokens that downstream agents and the debate parser
    /// rely on; paraphrasing the paragraph would break the contract
    /// silently, so this test is the canary.
    #[test]
    fn composer_prompt_includes_debate_protocol() {
        let prompt =
            SystemPromptBuilder::new(agent(AgentRole::Composer, "comp", 1)).build();
        let lower = prompt.to_lowercase();
        assert!(
            lower.contains("decompose"),
            "Composer prompt must include 'decompose'; got:\n{prompt}",
        );
        assert!(
            prompt.contains("request_critique"),
            "Composer prompt must mention the request_critique tool by name; got:\n{prompt}",
        );
        assert!(
            prompt.contains("NEVER paper over"),
            "Composer prompt must keep the 'NEVER paper over' clause verbatim; got:\n{prompt}",
        );
        assert!(
            prompt.contains("AGREE"),
            "Composer prompt must enumerate the AGREE/DISAGREE/PARTIAL verdicts; got:\n{prompt}",
        );
        assert!(
            prompt.contains("spawn_subagent"),
            "Composer prompt must mention the spawn_subagent tool by name; got:\n{prompt}",
        );
    }

    /// Non-Composer roles must NOT receive the protocol addendum. This is
    /// what keeps the debate machinery from polluting Watcher/Capturer
    /// system prompts and causing them to hallucinate tools they can't call.
    #[test]
    fn non_composer_prompts_omit_debate_protocol() {
        for role in [
            AgentRole::Conversational,
            AgentRole::Watcher,
            AgentRole::Capturer,
            AgentRole::Transcriber,
            AgentRole::Reflector,
            AgentRole::Indexer,
            AgentRole::Actor,
            AgentRole::Fixer,
        ] {
            let prompt = SystemPromptBuilder::new(agent(role, "x", 1)).build();
            assert!(
                !prompt.contains("NEVER paper over"),
                "{role:?} must NOT carry the Composer addendum; got:\n{prompt}",
            );
            assert!(
                !prompt.contains("request_critique"),
                "{role:?} must NOT mention request_critique; got:\n{prompt}",
            );
        }
    }

    // --- other_agents (inter-agent chat hints) ----------------------------

    #[test]
    fn other_agents_omitted_when_none_set() {
        // No `with_other_agents` call → no co-agent paragraph at all.
        let prompt =
            SystemPromptBuilder::new(agent(AgentRole::Conversational, "chat", 1)).build();
        assert!(!prompt.contains("Other agents are also running"));
        assert!(!prompt.contains("send_to_agent"));
    }

    #[test]
    fn other_agents_empty_vec_does_not_emit_paragraph() {
        // Calling `with_other_agents(vec![])` is the equivalent of "no peers
        // right now"; the paragraph must still be skipped so the rendered
        // prompt is byte-equivalent to the no-peer version.
        let with_empty =
            SystemPromptBuilder::new(agent(AgentRole::Conversational, "chat", 1))
                .with_other_agents(Vec::new())
                .build();
        let without =
            SystemPromptBuilder::new(agent(AgentRole::Conversational, "chat", 1)).build();
        assert_eq!(with_empty, without, "empty peers must match no-peer prompt");
    }

    #[test]
    fn other_agents_paragraph_lists_peers_and_chat_tools() {
        let prompt =
            SystemPromptBuilder::new(agent(AgentRole::Conversational, "boss", 1))
                .with_other_agents(vec![
                    (AgentRole::Watcher, "scout".to_string()),
                    (AgentRole::Composer, "planner".to_string()),
                ])
                .build();
        assert!(
            prompt.contains("Other agents are also running"),
            "missing co-agent paragraph; got:\n{prompt}",
        );
        // Both labels must appear so the model can address them by name.
        assert!(prompt.contains("scout"), "missing peer label scout");
        assert!(prompt.contains("planner"), "missing peer label planner");
        // Roles surface as their canonical labels.
        assert!(prompt.contains("Watcher"));
        assert!(prompt.contains("Composer"));
        // The three tool names must surface so the model knows what to call.
        assert!(prompt.contains("send_to_agent"));
        assert!(prompt.contains("read_from_agent"));
        assert!(prompt.contains("broadcast_to_role"));
    }

    #[test]
    fn other_agents_paragraph_appears_after_boundary_before_task() {
        // The co-agent hint sits with the manifest+boundary so the model
        // sees who its peers are before reading the variable task body.
        let prompt =
            SystemPromptBuilder::new(agent(AgentRole::Conversational, "boss", 1))
                .with_other_agents(vec![(AgentRole::Watcher, "scout".to_string())])
                .with_task("plan the next move".to_string())
                .build();
        let boundary_idx = prompt.find("invent tool calls").expect("boundary");
        let peers_idx = prompt
            .find("Other agents are also running")
            .expect("co-agent");
        let task_idx = prompt.find("## Task").expect("task heading");
        assert!(boundary_idx < peers_idx, "peers must follow boundary");
        assert!(peers_idx < task_idx, "peers must precede task");
    }
}
