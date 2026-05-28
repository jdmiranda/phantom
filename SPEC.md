# SPEC: TaskLedger JSONL Persistence for Crash Survivability

## Problem

`phantom_brain::orchestrator::TaskLedger` is currently in-memory only. A
main-process crash (SIGSEGV, panic, OOM) loses every in-flight goal, plan
step state, agent assignment, and quarantine policy decision the brain
made up to that point. The supervisor restarts the main process but the
brain wakes up empty, which silently aborts whatever multi-step task the
operator was running.

## Goal

Append every state transition on `TaskLedger` to an on-disk JSONL event
log so a restarted process can replay the log and reconstruct ledger
state without operator intervention.

## In Scope

- `LedgerEvent` enum with one variant per observable transition.
- `LedgerConfig` carrying the on-disk path (default
  `<repo>/.phantom/ledger.jsonl`).
- `TaskLedger::append_event` that writes one line, flushes, and
  surfaces I/O failures up to the caller.
- `TaskLedger::replay_from` that reconstructs a ledger from an existing
  log file, tolerating corrupt lines with a `log::warn!` skip.
- Hooks at the existing `try_dispatch`, `record_quarantine_failure`,
  `approve_checkpoint`, and `record_success` / `record_failure`
  forwarding paths so each transition produces exactly one event when
  persistence is enabled.
- Unit tests in `crates/phantom-brain/src/orchestrator.rs` covering
  round-trip equality, corrupt-line tolerance, and empty-file replay.

## Out of Scope

- Concurrent-writer locking. Only one `TaskLedger` may be bound to a
  given JSONL path at a time. v1 explicitly assumes a single-process
  writer. A future revision will gate this on a lockfile.
- Log compaction / rotation.
- Encrypting the log at rest.
- Persisting `Fact`s, `recent_outputs`, or the loop-detection sliding
  window. Only the FSM-relevant state is persisted; soft caches are
  rebuilt empty on replay.
- Migrating any other crate's state (agents, loops, brain action
  queue) to disk. Those crates own their own persistence.

## Public API

`TaskLedger`'s existing public methods (`new`, `set_plan`,
`try_dispatch`, `record_quarantine_failure`, `approve_checkpoint`,
`record_success` via `PlanStep`, `record_failure` via `PlanStep`)
remain source-compatible. Persistence is opt-in via a new
`bind_persistence(cfg)` method; callers that never bind keep the
current in-memory-only behavior. Three additions:

- `pub struct LedgerConfig { pub path: PathBuf }`
- `pub enum LedgerError { Io(io::Error), Json(serde_json::Error) }`
- `pub fn TaskLedger::bind_persistence(&mut self, cfg: LedgerConfig) -> Result<(), LedgerError>`
- `pub fn TaskLedger::append_event(&mut self, ev: LedgerEvent) -> Result<(), LedgerError>`
- `pub fn TaskLedger::replay_from(path: &Path) -> Result<TaskLedger, LedgerError>`

Existing mutators internally call `append_event` when a writer is
bound and bubble the error up via the same `Result` channel they
already use (`try_dispatch` already returns `Result<&PlanStep, DispatchBlocked>`,
so a new `DispatchBlocked::Persistence` variant carries the I/O error).
`approve_checkpoint` already returns `Result<(), String>`; persistence
failure formats into that string.
`PlanStep::record_success` / `record_failure` are pure data mutators
with no ledger handle, so the corresponding events are emitted at the
*reconciler call site* via a thin helper on `TaskLedger`
(`record_step_success(idx, summary)`, `record_step_failure(idx, summary)`).

## Event Schema

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LedgerEvent {
    StepDispatched { idx: usize },
    StepCompleted { idx: usize, summary: String },
    StepFailed { idx: usize, summary: String, cause: StepFailureCause },
    Quarantined { idx: usize, agent_id: u64, since_ms: u64, policy: QuarantinePolicy },
    Approved { idx: usize },
    AgentStatusChanged { agent_id: u64, from: AgentStatus, to: AgentStatus },
}
```

`StepFailureCause` and `QuarantinePolicy` already exist in
`orchestrator.rs` and get `Serialize` / `Deserialize` derives added.
`AgentStatus` already derives both in `phantom-agents`.

## Replay Semantics

- An empty or absent file yields `TaskLedger::new("")` with empty
  plan, facts, history.
- Each event is applied in order; corrupt JSON lines log a
  `warn!("ledger replay: skipping corrupt line {n}: {err}")` and
  continue.
- Replay reconstructs only the FSM state listed above. Plan structure
  (the `Vec<PlanStep>` topology) is recovered from a separate
  `PlanInitialized { steps: Vec<PlanStep> }` event emitted by
  `set_plan`. Replaying `StepDispatched { idx }` against an unknown
  index logs a warning and continues so corrupt logs cannot panic the
  replay.

## Risks Acknowledged

- **Stale lock without a lockfile**: a crashed process holds no OS
  resource on the log; a second process opening the same path will
  interleave writes. Single-writer is documented as a caller
  invariant for v1.
- **Disk-full mid-append** surfaces as `LedgerError::Io` to the
  caller. The ledger does *not* roll back the in-memory transition
  on append failure; callers see the I/O error and the in-memory
  state is consistent with what the next append would have recorded.
- **`fs::OpenOptions` append-mode atomicity** holds for write sizes
  under PIPE_BUF (4096 bytes on Linux/macOS). Most `LedgerEvent`
  variants serialize to well under that threshold for typical plans.
  The one exception is `PlanInitialized { steps: Vec<PlanStep> }`,
  which carries the full plan topology and can exceed PIPE_BUF on
  large plans. POSIX append semantics do NOT guarantee atomicity
  above PIPE_BUF, so a concurrent writer (forbidden by the
  single-writer invariant above) could interleave such a write.
  Within the single-writer model this is not an interleaving risk,
  but a crash mid-write of a large `PlanInitialized` line will leave
  a truncated final line on disk. The corrupt-line skip-on-replay
  path (`warn!("ledger replay: skipping corrupt line ...")`)
  mitigates: the partial line is discarded and replay continues,
  yielding a ledger missing only the plan topology, which the
  reconciler will treat as an empty plan and re-bootstrap.
- **`BufWriter::flush` is not `fsync`.** `TaskLedger::append_event`
  flushes the user-space buffer to the kernel page cache; it does
  NOT issue an `fsync(2)`. The log survives a user-space process
  crash (panic, SIGSEGV, OOM kill) because the kernel still holds
  the bytes and will write them out, but it does NOT survive a
  kernel panic or sudden power loss — the most recent appends may
  be lost. This is acceptable for v1 because the supervisor-driven
  restart loop targets only user-space crashes; durability against
  hardware loss is out of scope.

## Future Work

- **`fsync` upgrade**: add an opt-in `LedgerConfig::sync_on_append`
  flag that calls `File::sync_data` after each flush, trading
  throughput for kernel-panic / power-loss durability. Suitable
  defaults will depend on measured append rates.
- **Lockfile-based single-writer enforcement**: replace the
  documented invariant with an OS file lock (`fcntl(F_SETLK)` /
  `LockFileEx`) so a second process opening the same path errors
  out instead of silently interleaving large appends.
- **Log compaction**: snapshot the in-memory ledger to a sibling
  file and truncate the JSONL once it exceeds a size threshold,
  bounding replay time on long-running processes.
