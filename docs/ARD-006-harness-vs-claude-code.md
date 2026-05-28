# ARD-006: Phantom Agent Harness vs Claude Code — Structural Comparison

**Status**: Accepted
**Date**: 2026-05-28
**Authors**: Jeremy Miranda, Claude

---

## Status

Accepted. Phantom's 4-property harness is complete on `main` (commit `700b47f`). Four derived
implementation decisions are tracked in concurrent PRs — see Follow-on Work.

---

## Context

Phantom now has a closed-loop agent harness with four independently verifiable properties:

1. **Capability gate** — `phantom-agents::dispatch` routes every tool-use through
   `capability::check_capability(ctx.role, tool.class())` before the handler runs. Ten
   `AgentRole` variants each carry an explicit tool-class whitelist. Denials return a
   canonical `ToolResult` so the model self-corrects.

2. **External state machine** — `phantom-brain::TaskLedger` owns an external 9-state
   `AgentStatus` FSM (`Queued → Planning → AwaitingApproval → Working → WaitingForTool →
   Paused → Done → Failed → Flatline`). The `try_dispatch` guarded mutator returns
   `Result<&PlanStep, DispatchBlocked>`, preventing concurrent dispatch. `StepFailureCause`
   and `QuarantinePolicy` encode typed cascade semantics.

3. **Structured exit** — Agents spawned with `AgentSpawnOpts::with_requires_complete_task(true)`
   must call the `complete_task` lifecycle tool. The `LoopRunner` validates the payload against
   the per-loop `ExitSchema`. Three consecutive schema-invalid calls flatline the pane (3-strike
   `validation_failure_count`). The legacy stringly-typed "PARTIAL" exit path is removed.

4. **Typed inter-agent messaging** — `phantom-protocol::Event` bus carries 22 typed variants
   with `EventTopic` routing. `phantom-loop::LoopMessage` provides typed inter-loop routing
   through `LoopQueueRegistry`. The `Event::FastPathTaken` envelope makes every audit-traced
   auto-approval visible on the bus.

An architectural review of the Claude Code harness — an externally-built agentic system built
on the same Claude API — reveals a 6-layer stack that maps closely to Phantom's properties.
The key insight from the Anthropic team is that Claude Code is not a CLI that calls Claude;
it is an agentic harness where the model reasons and the harness mediates every action. That
separation is what makes it debuggable.

This ARD records how Phantom's design aligns with, diverges from, and can be strengthened by
the Claude Code model — specifically: context memory-budget discipline, the subagent
reports-upward-only contract, the worktree-per-task isolation primitive, and durable ledger
persistence.

---

## Decision

Adopt the Claude Code harness as a reference model for Phantom's agent infrastructure and
commit to closing the four gaps enumerated below via concurrent implementation work. The
4-property harness already in place maps cleanly onto the Claude Code 6-layer stack — no
fundamental redesign is required. Four targeted additions will bring the two models into
full alignment.

---

## Layer Mapping: Claude Code 6 Layers → Phantom Properties

| Claude Code Layer | Responsibility | Phantom Property | Phantom Crate(s) |
|---|---|---|---|
| **1. Interface surface** | Input routing, slash-command dispatch, keybindings | Capability gate (role routing) | `phantom-agents::dispatch`, `phantom-ui` |
| **2. Permission and safety** | Dangerousness scoring, user-approval flow, taint propagation | Capability gate (tool-class whitelisting) + taint levels | `phantom-agents::capability`, `phantom-agents::permissions` |
| **3. Tool** | ReadFile, WriteFile, RunCommand, BashTool, GitHub MCP, etc. | Tool execution layer inside each role's whitelist | `phantom-agents::tools`, `phantom-mcp` |
| **4. Memory and context** | Top-5 skill injection (5K tokens each, 25K budget), CLAUDE.md re-read after compaction | Context assembly for agent prompts — budget discipline **not yet enforced** | `phantom-context` |
| **5. Subagent and orchestration** | Subagents report upward only; Agent Teams share a task list across independent sessions | `LoopRunner` FSM + `SubstrateAgentDispatcher`; upward-only contract **not yet enforced** | `phantom-loop`, `phantom-brain` |
| **6. Execution environment** | Isolated git worktrees, one worktree per task, branch hygiene | Branch hygiene rule in CLAUDE.md; worktree primitive **not yet a typed API** | `phantom-app` (worktree surface), `phantom-loop` |

Column 4 entries marked **not yet enforced** are the four gaps being closed.

---

## Consequences

### Positive

- Phantom's 4-property harness already satisfies layers 1–3 and the state-machine portion
  of layer 5. No structural rework is required.
- Recording the layer mapping makes future debuggability claims testable: each Claude Code
  layer has a named Phantom counterpart, so regression is visible.
- The comparison validates the completeness of the `EventTopic` bus (layer 5) and the
  `complete_task` exit contract (layer 5 / structured exit).
- Adopting the context memory-budget primitive (layer 4) prevents agent context pollution
  as Phantom's skill library grows beyond the current set.

### Negative

- Four implementation gaps must be closed before the mapping is fully enforced rather than
  advisory. Until then, agents can exceed the context budget and subagents can write state
  sideways across sessions.
- The worktree primitive adds surface area to `phantom-app` and requires the loop CLI to
  manage worktree lifecycle alongside agent lifecycle.

### Neutral

- The Claude Code model uses CLAUDE.md re-injection after auto-compaction. Phantom's analog
  is `phantom-context` rebuilding the project context block. The mechanism differs but the
  intent is identical — grounding the model in project facts after a memory boundary.

---

## Follow-on Work

Four derived decisions are tracked in concurrent PRs against this branch.

**(a) Skills memory-budget primitive — `phantom-context`**

Context assembly currently concatenates all available context blocks without a token
ceiling. Introduce a `ContextBudget` struct capping injected skill/context blocks at a
configurable token limit (initial target: 25K tokens, matching Claude Code's discipline).
Blocks are ranked by recency-weighted utility and trimmed to fit. The 5-most-recent-skills
heuristic from Claude Code is the reference implementation.

**(b) Subagent reports-up-only contract — `phantom-agents`**

Subagents spawned via `SubstrateAgentDispatcher` currently have no structural barrier
preventing them from writing directly to a sibling agent's queue or the parent's internal
state. Encode the upward-only reporting contract as a type-level restriction: subagents
receive a `ReportHandle` that exposes only `complete_task` and `abort_task` plus one-way
message emission to their parent. Lateral writes become a compile-time error.

**(c) Worktree primitive API — `phantom-app`**

The branch-hygiene rule in CLAUDE.md (Orchestration Rule 6) describes the intended
behavior but has no runtime enforcement. Introduce a `WorktreeHandle` type in `phantom-app`
that wraps a `git worktree add` call with the baseline-tag-fallback logic, exposes the
worktree path, and implements `Drop` cleanup. `SubstrateAgentDispatcher` should require a
`WorktreeHandle` when spawning implementation agents.

**(d) Persistent TaskLedger JSONL log — `phantom-brain`**

`TaskLedger` state is currently in-memory only. A process restart loses all in-flight task
state, which is unrecoverable for multi-step plans. Append-only JSONL persistence (matching
the audit-log pattern already in `SelfImprovementConfig::audit_log_path`) provides crash
recovery and an offline audit trail. The log must record every FSM transition with a
timestamp, step index, `AgentStatus` before/after, and optional `StepFailureCause`.

---

## References

- `crates/phantom-agents/src/dispatch/mod.rs` — capability gate implementation
- `crates/phantom-agents/src/dispatch/capability.rs` — role-to-tool-class whitelist
- `crates/phantom-brain/src/task_ledger.rs` — `TaskLedger`, `try_dispatch`, `DispatchBlocked`
- `crates/phantom-brain/src/self_improvement.rs` — `AuditEntry` JSONL pattern (reference for gap d)
- `crates/phantom-loop/src/runner/fsm.rs` — `LoopRunner` async FSM
- `crates/phantom-loop/src/dispatcher/substrate.rs` — `SubstrateAgentDispatcher`
- `crates/phantom-context/src/lib.rs` — context assembly (target for gap a)
- `docs/design/brain-self-improvement.md` — `TrustBand` and `RateLimiter` design
- CLAUDE.md Orchestration Rule 6 — branch hygiene (reference for gap c)
- CLAUDE.md Orchestration Rule 8 — no question-mark sentences in agent prompts
- Anthropic: "Claude Code is not a CLI that calls Claude — it is an agentic harness where
  the model reasons and the harness mediates every action."
