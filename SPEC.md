# SPEC.md — Subagent reports-up-only isolation contract

## Goal

Implement Claude Code's subagent isolation contract inside Phantom. A subagent
is an agent spawned with a parent orchestrator. The contract: a subagent may
emit only upward report events. Lateral and internal classes are dropped at
the emit boundary, counted, and logged.

## Scope

- `crates/phantom-agents` — owner of `AgentSpawnOpts` and the dispatch gate.
- `crates/phantom-protocol` — owner of `Event` and `EventTopic`.

Out of scope for this slice:
- The full GUI wiring of `subagent=true` on `AgentPane`.
- Behaviour change for `subagent=false` agents.
- The 10 `AgentRole` variants (this contract is orthogonal to role).

## Contract

1. Every `Event` variant has a class: `UpwardReport`, `Lateral`, or `Internal`.
2. `AgentTaskComplete` and `AgentError` are `UpwardReport`. The brain and
   inspector consume those, which is exactly the parent-orchestrator surface.
3. When `AgentSpawnOpts::subagent == true`, the emit boundary blocks every
   non-`UpwardReport` event. The block path is: log at `warn`, drop the
   event, increment a counter on the emit guard.
4. `subagent == false` keeps the existing behaviour. No counter, no drop.
5. The block path never panics. A blocked emit is observable through the
   counter and the warn-log only.

## Non-goals

- Forward-compat for unclassified `Event::Custom` variants — they are
  classified as `Lateral` until a future revision needs finer granularity.
- Persistence of the suppressed-emit counter. It is in-memory only on the
  per-agent guard for the duration of the agent's life.
