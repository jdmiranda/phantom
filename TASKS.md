# TASKS.md — Subagent reports-up-only isolation contract

- [ ] Add `EventClass` enum with `UpwardReport / Lateral / Internal` variants in `crates/phantom-protocol/src/events.rs`.
- [ ] Add `Event::class(&self) -> EventClass` covering all 22 existing variants.
- [ ] Re-export `EventClass` from `crates/phantom-protocol/src/lib.rs`.
- [ ] Add `subagent: bool` field to `AgentSpawnOpts` in `crates/phantom-agents/src/agent.rs`. Default `false`.
- [ ] Add `with_subagent(self, v: bool) -> Self` builder on `AgentSpawnOpts`.
- [ ] Add `subagent(&self) -> bool` getter.
- [ ] Create `crates/phantom-agents/src/subagent_emit.rs` with `SubagentEmitGuard` struct and `try_emit` method.
- [ ] Wire `pub mod subagent_emit;` into `crates/phantom-agents/src/lib.rs`.
- [ ] Unit tests in `subagent_emit.rs`: subagent allows `UpwardReport`, subagent blocks `Lateral` and increments counter, non-subagent passes all classes.
- [ ] Tests in `events.rs` for `Event::class()` mapping.
- [ ] Run `cargo build -p phantom-agents -p phantom-protocol` clean.
- [ ] Run `cargo test -p phantom-agents -p phantom-protocol` clean.
- [ ] Open draft PR with required title and HEREDOC body.
