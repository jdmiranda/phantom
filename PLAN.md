# PLAN: Implementation Sequence

## Phase 1 — Type plumbing

1. Add `Serialize` + `Deserialize` derives to `StepFailureCause`,
   `QuarantinePolicy`, and `PlanStep` (and any transitive types they
   compose, like `AgentTask`, which already derives both).
2. Verify `StepStatus` already derives `Serialize` / `Deserialize`; if
   not, add them. (Inspection: it currently derives only `Debug, Clone,
   Copy, PartialEq, Eq` — add serde derives.)

## Phase 2 — Module: ledger_log

Add a new file `crates/phantom-brain/src/orchestrator/ledger_log.rs`
(or, to keep the diff small, inline the new types at the bottom of
`orchestrator.rs`). Inline keeps diff under 800 lines; pick inline.

The module contains:

- `LedgerError` enum wrapping `io::Error` and `serde_json::Error`.
- `LedgerConfig { path: PathBuf }` with a helper `LedgerConfig::default_for_repo(&Path) -> Self` that resolves `<repo>/.phantom/ledger.jsonl`.
- `LedgerEvent` enum (variants per SPEC).
- A private `LedgerWriter` struct holding an open `BufWriter<File>` for the JSONL stream. The writer flushes after every line.

## Phase 3 — Wire into TaskLedger

1. Add an `Option<LedgerWriter>` field on `TaskLedger`.
2. Add `bind_persistence(&mut self, cfg) -> Result<(), LedgerError>`
   that opens the file in append+create mode, parent-dir-creates as
   needed, and stores the writer.
3. Add `append_event(&mut self, ev) -> Result<(), LedgerError>` that
   serializes one line and flushes. No-op when no writer is bound.
4. Hook into mutators:
   - `set_plan` -> `PlanInitialized { steps }` event.
   - `try_dispatch` success path -> `StepDispatched { idx }`.
   - `record_quarantine_failure` -> `Quarantined { idx, agent_id, since_ms, policy }`.
   - `approve_checkpoint` success path -> `Approved { idx }`.
   - Add new `record_step_success(idx, summary)` and `record_step_failure(idx, summary)` methods that delegate to `PlanStep::record_*` and append `StepCompleted` / `StepFailed` events.
5. Add `replay_from(path) -> Result<TaskLedger, LedgerError>` that:
   - Reads the file line-by-line via `BufReader`.
   - Parses each line as `LedgerEvent`; on error logs a warn and
     continues.
   - Dispatches each event to a private `apply_event(&mut self, ev)`
     that mutates the ledger without re-appending.
   - Returns the reconstructed ledger with `writer = None` so the
     caller decides whether to rebind for continued writes.

## Phase 4 — Tests (inline in orchestrator.rs `mod tests`)

1. `ledger_persist_roundtrip` — write a plan, dispatch, complete; tear
   down; replay; assert plan state equality.
2. `ledger_persist_corrupt_line_tolerated` — write a valid event, then
   garbage, then another valid event; replay; assert both valid
   events were applied and no panic.
3. `ledger_persist_empty_file` — replay from a path that does not
   exist; assert an empty default ledger comes back.
4. `ledger_persist_quarantine_cascade_replayed` — exercise the
   `Quarantined { policy: FailAndCascade }` path and replay, asserting
   downstream steps are skipped after replay.

## Phase 5 — Build & verify

- `cargo build -p phantom-brain`
- `cargo test -p phantom-brain`

## Phase 6 — PR

- Commit on the preconfigured branch.
- `gh pr create --draft --title "feat(phantom-brain): persist TaskLedger as JSONL event log for crash survivability"` with the HEREDOC body.

## Non-goals checked off the list

- No `tokio` async path — the existing ledger is sync-only and the
  reconciler calls it from a sync context.
- No `tracing` dep — `log` is already in the workspace and the rest
  of `phantom-brain` uses it.
- No new dep edges; `serde_json` + `serde` + `log` are already in the
  crate's `Cargo.toml`.
