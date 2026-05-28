# TASKS

## T1 — Serde derives on existing types

In `crates/phantom-brain/src/orchestrator.rs`, add
`Serialize, Deserialize` derives to:

- `StepStatus`
- `StepFailureCause`
- `QuarantinePolicy`
- `PlanStep`

Verify `AgentTask` and `Disposition` already derive both (they do).
Add `use serde::{Deserialize, Serialize};` at the top of the file.

## T2 — Ledger persistence types

At the bottom of `crates/phantom-brain/src/orchestrator.rs`, append:

- `pub enum LedgerError { Io(io::Error), Json(serde_json::Error) }` with
  `Display` + `Debug` and `From<io::Error>` + `From<serde_json::Error>`.
- `pub struct LedgerConfig { pub path: PathBuf }` with a constructor
  `LedgerConfig::new(path)` and a convenience
  `LedgerConfig::default_for_repo(repo: &Path)` that returns
  `<repo>/.phantom/ledger.jsonl`.
- `pub enum LedgerEvent` with the six variants listed in SPEC plus
  `PlanInitialized { steps: Vec<PlanStep> }`, deriving
  `Serialize, Deserialize, Debug, Clone`.
- A private `LedgerWriter` struct holding `BufWriter<File>` plus the
  bound path.

## T3 — TaskLedger persistence methods

In `crates/phantom-brain/src/orchestrator.rs`:

- Add a private `writer: Option<LedgerWriter>` field on `TaskLedger`.
- Initialize it to `None` in `TaskLedger::new`.
- Add `pub fn bind_persistence(&mut self, cfg: LedgerConfig) -> Result<(), LedgerError>`.
  The implementation creates `cfg.path`'s parent directory if missing,
  opens the file with `OpenOptions::new().create(true).append(true)`,
  and stores a `LedgerWriter`.
- Add `pub fn append_event(&mut self, ev: LedgerEvent) -> Result<(), LedgerError>`
  that no-ops when no writer is bound, otherwise serializes the event
  to JSON, writes one line plus `\n`, and `flush()`es before returning.
- Add `pub fn replay_from(path: &Path) -> Result<TaskLedger, LedgerError>`
  that returns a default ledger when the file does not exist, otherwise
  opens with `BufReader`, parses each line as `LedgerEvent`, applies
  it via a private `apply_event`, logs a warning on parse failure, and
  continues. The returned ledger has `writer = None`.
- Add a private `fn apply_event(&mut self, ev: LedgerEvent)` that
  mutates in-memory state without appending. Use this from `replay_from`.

## T4 — Mutator hooks

In `crates/phantom-brain/src/orchestrator.rs`:

- After `set_plan` mutates the plan, append a
  `LedgerEvent::PlanInitialized { steps: self.plan.clone() }`. Log
  any append error via `log::error!` and continue (set_plan is
  infallible by signature; persistence failure on plan-init is logged
  rather than swallowed).
- In `try_dispatch`, on the success branch immediately before
  returning `Ok`, append `LedgerEvent::StepDispatched { idx }`. Surface
  any error by mapping it into a new
  `DispatchBlocked::Persistence(String)` variant so the caller sees
  the I/O failure. (This is an additive variant; existing
  `match`/`if let` arms continue to compile.)
- In `record_quarantine_failure`, after applying the policy, append
  `LedgerEvent::Quarantined { idx, agent_id, since_ms, policy }` and
  map the error to `DispatchBlocked::Persistence`.
- In `approve_checkpoint`, on the success branch, append
  `LedgerEvent::Approved { idx }`; map any error to the existing
  `Err(String)` return type via `format!`.
- Add `pub fn record_step_success(&mut self, idx, summary)` and
  `pub fn record_step_failure(&mut self, idx, summary)` that delegate
  to `PlanStep::record_success` / `record_failure` and append the
  corresponding event. Both return `Result<(), LedgerError>` so
  reconciler call sites can see persistence failures.

## T5 — Tests

Inside the existing `#[cfg(test)] mod tests` in `orchestrator.rs`, add:

- `ledger_roundtrip_state_equality`
- `ledger_replay_skips_corrupt_lines`
- `ledger_replay_missing_file_is_empty`
- `ledger_replay_reconstructs_quarantine_cascade`

Use `tempfile::TempDir` (already a dev-dep of `phantom-brain`) for
disposable log paths.

## T6 — Build, test, PR

- `cargo build -p phantom-brain` (run from worktree root).
- `cargo test -p phantom-brain`.
- Commit on the existing branch.
- Open a draft PR via `gh pr create --draft --title "feat(phantom-brain): persist TaskLedger as JSONL event log for crash survivability"` with a HEREDOC body containing a 3-bullet Summary and a Test plan checklist.
