# PLAN.md — Subagent reports-up-only isolation contract

## Step 1: `phantom-protocol` — classify events

File: `crates/phantom-protocol/src/events.rs`.

Add a new `EventClass` enum next to `EventTopic` with three variants:

```rust
pub enum EventClass {
    UpwardReport,
    Lateral,
    Internal,
}
```

Add `Event::class(&self) -> EventClass`. The mapping:

- `AgentTaskComplete`, `AgentError`, `AgentProgress` → `UpwardReport`
  (agent-to-orchestrator status updates).
- `AgentSpawned`, `FastPathTaken` → `Internal` (lifecycle telemetry, not the
  agent's report-out surface).
- `CommandStarted`, `CommandComplete`, `TerminalOutput`,
  `SubprocessTakeoverDetected`, `SubprocessTakeoverCleared`,
  `BrainDecision`, `NlpInterpreted`, `SessionSwitched`, `FocusChanged`,
  `VideoPlaybackStateChanged`, `FrameCaptured`, `GlitchFxTriggered`,
  `Custom` → `Lateral` (peer-bus traffic).
- `MemoryPressure`, `JobCompleted`, `Shutdown` → `Internal` (host-level).

Re-export `EventClass` from `crates/phantom-protocol/src/lib.rs`.

## Step 2: `phantom-agents` — `AgentSpawnOpts` field

File: `crates/phantom-agents/src/agent.rs`.

Add `subagent: bool` to `AgentSpawnOpts`. Default `false` in `new`. Add the
builder `with_subagent(self, v: bool) -> Self` and the getter
`subagent(&self) -> bool`.

## Step 3: `phantom-agents` — subagent emit guard

New file: `crates/phantom-agents/src/subagent_emit.rs`.

Provides a small `SubagentEmitGuard` struct holding `subagent: bool` and
`suppressed_lateral_emits: u64`. One method:
`try_emit(&mut self, ev: &Event) -> bool`. When the agent is a subagent and
the event class is not `UpwardReport`, log at `warn`, increment the counter,
return `false`. Otherwise return `true`.

The guard is constructed from an `AgentSpawnOpts` via `SubagentEmitGuard::from_opts(opts)`.

## Step 4: Tests

Unit tests inside `subagent_emit.rs`:

1. Subagent emitting `AgentTaskComplete` is allowed and the counter stays 0.
2. Subagent emitting `CommandStarted` (Lateral) is blocked and the counter
   moves to 1.
3. Non-subagent agent emits any class. Counter remains 0 because
   non-subagents do not gate.

Round-trip tests on `Event::class()` in `crates/phantom-protocol/src/events.rs`:
- `AgentTaskComplete` and `AgentError` classify as `UpwardReport`.
- `CommandStarted` classifies as `Lateral`.
- `Shutdown` classifies as `Internal`.

## Step 5: Build and test

Run:
```
cargo build -p phantom-agents -p phantom-protocol
cargo test -p phantom-agents -p phantom-protocol
```

## Step 6: Draft PR

Title: `feat(phantom-agents): subagent reports-up-only isolation contract`.

PR body has a 3-bullet Summary and a Test plan checklist.
