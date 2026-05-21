# Brain

[← back to components index](README.md)

> OODA loop + the autonomy pipeline.

## Status

<span class="chip ok">shipping</span> — both crates ship; the
self-improvement pipeline is wired but `enabled = false` by default.

## What it does

The brain is Phantom's executive: it observes the bus, scores its
options, and dispatches actions. Self-improvement (Flow 4) sits on top
of the OODA loop; the loop runner (Flow 3) is the headless execution
substrate that consumes brain-emitted queue messages.

## Crates

### `phantom-brain` <span class="chip ok">shipping</span>

The OODA loop + scoring + self-improvement reconciler.

- `Brain` — the per-process thread + action channel + event log handle.
  Constructed in `App::with_config_scaled`; runs on its own thread.
- `WorldState` — the snapshot the brain orients on (focused adapter,
  idle time, suggestions, recent errors, agent state, etc.).
- OODA tick — `Observe` (build WorldState) → `Orient` (utility score) →
  `Decide` (best action) → `Act` (emit `AiAction` onto the channel).
  Quiet score is 0.5 — the brain stays silent unless it has something
  more useful.
- `AiAction` enum — typed actions: `EnqueueLoopMessage`, `Suggest`,
  `SpawnAgent`, etc.
- `TaskLedger` — the external state machine. Owns agent dispatch state
  per the harness control invariant (ADR-001). `try_dispatch(idx)`
  returns `Result<&PlanStep, DispatchBlocked>`.
- `self_improvement::*` — Flow 4 implementation. `score_candidate`,
  `HardExclusions`, `TrustBand` (4 bands), `RateLimiter`, `AuditEntry`.
- `goal_source::*` — `GoalSource` trait + `GhIssueGoalSource` +
  `GhCiFailureGoalSource`.
- `reconciler::*` — completion reconciliation: when an
  `AgentTaskComplete` fires on the bus, the reconciler matches it
  against active dispatches, advances the TaskLedger, and routes
  quarantine-coincident failures to `record_quarantine_failure`.

### `phantom-loop` <span class="chip ok">shipping</span>

The headless autonomy pipeline (Flow 3).

- `LoopRegistry` — owns named `LoopRunner` instances.
- `LoopRunner` — per-loop async FSM.
- `LoopSource` trait + 4 production impls (`CronSource`,
  `LoopMessageQueueSource`, `GhIssueQueueSource`, `GhPrReviewQueueSource`).
- `SubstrateAgentDispatcher` — translates `LoopInput` to
  `AgentSpawnOpts` with `requires_complete_task=true`.
- `SubstrateDriver` — the headless GUI-less driver.
- `SubstrateBackend` trait + `ChatBackedSubstrateBackend` (production) +
  `MockSubstrateBackend` (tests).
- `ExitSchema` — per-loop schema for `complete_task` payload validation.
- `LoopQueueRegistry` — typed `LoopMessage` fan-out.
- `RunLock` — exclusive file lock at `<repo>/.phantom/loops/.runlock`.
- Preflight: `check_gh_binary`, `check_gh_auth`, `check_mcp_collisions`.

## Owns

- `WorldState` shape (single source for "what does the brain see").
- `AiAction` enum (the executive's action vocabulary).
- `TaskLedger` (external state machine for agent dispatch).
- `score_candidate` weighted-sum (Flow 4's scoring math).
- `TrustBand` ramp logic.
- `RateLimiter` ceilings (per-hour, per-day, cooldown).
- `LoopRunner` FSM transitions.
- `ExitSchema` validation contract (per-loop schemas live in TOML).
- `RunLock` lifecycle.

## Reads from

| Source | What |
|---|---|
| Event bus | every topic (the brain observes all) |
| `gh` CLI (via `GoalSource`) | open issues + failing CI runs |
| Loop spec TOMLs | source config, role, ExitSchema |
| Audit log path | TrustBand persisted state (between sessions) |

## Writes to / publishes

| Target | What |
|---|---|
| `AiAction` channel | actions for `App` to execute |
| `LoopQueueRegistry` | typed `LoopMessage` to downstream loops |
| Audit log JSONL | every brain decision |
| Inspector view | snapshot of agents, events, denials |
| Spawn queue (via `EnqueueLoopMessage` → bridge) | agent spawn requests |

## Decisions honoured

- [ADR-001 · Architecture decisions](../decisions/001-architecture.md) — the
  harness control properties (per-role tool whitelist, external state
  machine, structured exit, typed inter-agent messaging) are all owned
  by the brain ring.
- [ADR-003 · App lifecycle + pub-sub](../decisions/003-pubsub.md) — the
  brain subscribes to every topic; `AiAction::EnqueueLoopMessage` and
  `LoopMessage` are the typed channels.

## Open gaps

- [gap-loop-mid-flight-cancel](../gaps.md#gap-loop-mid-flight-cancel) —
  no graceful loop-stop CLI.
- [gap-loop-exit-schema-error-uplift](../gaps.md#gap-loop-exit-schema-error-uplift) —
  schema errors flatline silently.
- [gap-loop-quarantine-cascade-ux](../gaps.md#gap-loop-quarantine-cascade-ux) —
  quarantine state isn't shown in Inspector.
- [gap-loop-watchdog-vs-supervisor](../gaps.md#gap-loop-watchdog-vs-supervisor) —
  two unrelated restart loops.
- [gap-brain-trust-band-ramp-ux](../gaps.md#gap-brain-trust-band-ramp-ux) —
  TrustBand ramps are invisible.
- [gap-brain-self-improve-opt-in](../gaps.md#gap-brain-self-improve-opt-in) —
  opt-in requires hand-editing config.
- [gap-brain-goal-source-rate-limit](../gaps.md#gap-brain-goal-source-rate-limit) —
  unauthenticated `gh` blows the 60/hr quota silently.

## Source files

| Concept | File |
|---|---|
| Brain thread | [`crates/phantom-brain/src/brain.rs`](../../../../crates/phantom-brain/src/brain.rs) |
| OODA tick | [`crates/phantom-brain/src/ooda.rs`](../../../../crates/phantom-brain/src/ooda.rs) |
| TaskLedger | [`crates/phantom-brain/src/orchestrator.rs`](../../../../crates/phantom-brain/src/orchestrator.rs) |
| Self-improvement | [`crates/phantom-brain/src/self_improvement.rs`](../../../../crates/phantom-brain/src/self_improvement.rs) |
| GoalSource impls | [`crates/phantom-brain/src/goal_source/mod.rs`](../../../../crates/phantom-brain/src/goal_source/mod.rs) |
| LoopRunner FSM | [`crates/phantom-loop/src/runner/fsm.rs`](../../../../crates/phantom-loop/src/runner/fsm.rs) |
| SubstrateDriver | [`crates/phantom-loop/src/dispatcher/driver.rs`](../../../../crates/phantom-loop/src/dispatcher/driver.rs) |
| ExitSchema | [`crates/phantom-loop/src/exit.rs`](../../../../crates/phantom-loop/src/exit.rs) |
