# Flow 2 · Agent spawn (Composer sub-agent)

[← back to flows index](README.md)

A running Composer-role agent emits the `spawn_subagent` tool call.
Phantom's dispatch surface routes the request through the capability
gate, queues it for the App thread, and the App constructs a new
`AgentAdapter` peer to the Composer. This is the substrate primitive
that lets one agent delegate work to another without bypassing the
harness.

## Architecture decisions this flow honours

- [ADR-001 · Architecture decisions](../decisions/001-architecture.md) — the
  capability gate at `dispatch/mod.rs`, the per-role tool whitelist.
- [ADR-003 · App lifecycle + pub-sub](../decisions/003-pubsub.md) — agent
  spawn / completion events flow on the bus; subscribers (brain,
  inspector, reconciler) react.

## Participants

- **Composer agent** — a running `AgentAdapter` whose role is `Composer`.
  Receives a streamed `tool_use` block from the Claude API. See
  [agents](../components/agents.md).
- **Tool dispatcher** — `phantom_agents::dispatch::dispatch_tool`, the
  capability-gated entry point.
- **Capability gate** — `phantom_agents::dispatch::capability::check_capability(role, class)`.
- **Spawn queue** — `App::pending_spawn_subagent`, an
  `Arc<Mutex<VecDeque<SpawnSubagentRequest>>>`.
- **App** — drains the queue each update tick on the GUI thread (so the
  coordinator + scene + layout are touched without contention).
- **AgentPane** — the new child agent, constructed with a fresh
  `ChatModel`, role, and the parent's `AgentRef` correlation.
- **Coordinator** — registers the new adapter at a fresh pane (split path)
  or in place (replace path).
- **Event bus** — receives `AgentSpawned`, optionally `FastPathTaken`.

## Sequence

```mermaid
sequenceDiagram
    autonumber
    participant LLM as Claude API
    participant Composer as Composer agent
    participant Disp as Tool dispatcher
    participant Cap as Capability gate
    participant Queue as Spawn queue
    participant App as App
    participant Coord as Coordinator
    participant Child as Child AgentPane
    participant Bus as Event bus

    LLM-->>Composer: tool_use { name: "spawn_subagent", input: {…} }
    Composer->>Disp: dispatch_tool(ctx, tool)
    Disp->>Cap: check_capability(Composer, Coordinate)
    alt allowed
        Cap-->>Disp: OK
        Disp->>Disp: try_auto_approve_with_audit(ctx, tool)
        alt fast-path hit
            Disp->>Bus: emit FastPathTaken { agent_id, kind, reason }
        end
        Disp->>Queue: push SpawnSubagentRequest { role, label, task, chat_model, parent, assigned_id }
        Disp-->>Composer: ToolResult { ok: queued, child_id }
        Composer-->>LLM: tool_result
    else denied
        Cap-->>Disp: capability denied: Coordinate not in Conversational manifest
        Disp-->>Composer: ToolResult { error: "capability denied: …" }
        Composer-->>LLM: tool_result (model self-corrects on next turn)
    end

    rect rgba(102, 221, 255, 0.08)
        Note over App: Next App update tick · GUI thread
    end

    App->>Queue: drain into local Vec
    Queue-->>App: [SpawnSubagentRequest …]
    loop for each request
        App->>App: spawn_agent_pane_with_opts(opts)
        App->>App: resolve_api_config(chat_model)
        App->>Child: AgentPane::spawn_with_opts(opts, claude_config, …)
        App->>Child: set_substrate_handles(runtime, snapshot_queue, mcp_registry, …)
        App->>Coord: split_vertical(parent_pane_id)
        Coord-->>App: (existing_child, new_child)
        App->>Coord: register_adapter_at_pane(AgentAdapter, new_child, scene_node)
        Coord->>Bus: emit AgentSpawned { agent_id, role, parent_id }
        Bus-->>Composer: (subscribed)
        Bus-->>Coord: (brain observer subscribed; inspector subscribed)
    end
```

**GAP** · [capability-class-propagation](../gaps.md#gap-capability-class-propagation) —
the canonical `CapabilityClass` enum lives in
`phantom-agents::role` but `phantom-relay::grant` and `phantom-hub::auth`
each define their own independent copy.

**GAP** · [fast-path-audit-trail](../gaps.md#gap-fast-path-audit-trail) —
`Event::FastPathTaken` is emitted on the bus when the dispatcher's
auto-approve fires, but Inspector has no dedicated view to summarise
fast-path activity per agent.

## Walkthrough

1. **Composer receives a tool_use** — the Claude API streams a tool block;
   the Composer's `AgentPane` parses lifecycle signals and routes the
   request to the dispatch surface.
2. **Capability gate** —
   `phantom_agents::dispatch::capability::check_capability(role, class)`
   checks the role-class matrix. For `spawn_subagent`,
   `class_for(tool) == Coordinate`. Composer has Coordinate in its
   manifest; Conversational does not. Denials return the canonical string
   `"capability denied: <Class> not in <Role> manifest"` as the
   `ToolResult` so the model can self-correct.
3. **Fast-path audit** — `try_auto_approve_with_audit` is consulted; if it
   fires, an `Event::FastPathTaken` lands on the bus so the audit trail
   captures "this dispatch skipped the typical approval flow."
4. **Queue push** — `SpawnSubagentRequest` is pushed onto
   `App::pending_spawn_subagent`. The dispatch returns synchronously with
   "queued" so the Composer's LLM call can continue.
5. **App drains** — on the next update tick (GUI thread), the App locks
   the queue briefly, takes the snapshot into a local `Vec`, releases the
   lock, and processes each request. This serialises GUI mutations
   without holding the queue lock across coordinator / scene / layout
   mutations.
6. **spawn_agent_pane_with_opts** — builds an `AgentSpawnOpts` from the
   request (role, label, chat model). `resolve_api_config` resolves the
   key for the requested model.
7. **Substrate handles wired** — `set_substrate_handles` clones the
   runtime registry, event log, snapshot queue, MCP registry, blocked-
   event sink, and quarantine registry into the new `AgentPane`.
8. **Split pane** — `layout.split_vertical(parent_pane_id)` returns two
   children; the existing adapter (Composer) remaps to the top, the
   child agent claims the bottom. Both get `flex_grow: 1.0`.
9. **Register + focus** —
   `coordinator::register_adapter_at_pane(AgentAdapter, new_child, scene_node)`
   binds the adapter to the new pane slot; `run_arbiter_negotiation`
   refreshes spatial allocations; the new agent receives focus.
10. **Bus event** — `Event::AgentSpawned { agent_id, role, parent_id }`
    fires on the agent-event topic. Inspector renders a row in its event
    log; the brain reconciler tracks the dispatch for completion
    reconciliation later.

## Source files

| Concept | File |
|---|---|
| Tool dispatch | [`crates/phantom-agents/src/dispatch/mod.rs`](../../../../crates/phantom-agents/src/dispatch/mod.rs) |
| Capability gate | [`crates/phantom-agents/src/dispatch/capability.rs`](../../../../crates/phantom-agents/src/dispatch/capability.rs) |
| Role / class enum | [`crates/phantom-agents/src/role.rs`](../../../../crates/phantom-agents/src/role.rs) |
| spawn_subagent tool | [`crates/phantom-agents/src/tools.rs`](../../../../crates/phantom-agents/src/tools.rs) |
| App spawn drain | [`crates/phantom-app/src/update.rs`](../../../../crates/phantom-app/src/update.rs) |
| spawn_agent_pane_with_opts | [`crates/phantom-app/src/agent_pane/spawn.rs`](../../../../crates/phantom-app/src/agent_pane/spawn.rs) |
| Layout split | [`crates/phantom-ui/src/layout.rs`](../../../../crates/phantom-ui/src/layout.rs) |
| Event bus topics | [`crates/phantom-protocol/src/events.rs`](../../../../crates/phantom-protocol/src/events.rs) |
