# Review Rubric — Phantom

The checklist a review agent runs against every PR. Each item is **PASS / FAIL / N/A**. The reviewer's handoff must call out every FAIL with a file:line citation and a one-line "why this fails".

A PR passes review only when every applicable item is PASS. See [definition-of-done.md](definition-of-done.md) for what the orchestrator does with the verdict.

---

## 1. Build & test integrity

| # | Check | How to verify |
|---|---|---|
| 1.1 | `cargo build --workspace` clean (no warnings — `deny(warnings)` is enforced) | run it |
| 1.2 | `cargo test --workspace --no-run` clean (test code compiles) | run it |
| 1.3 | `cargo test --workspace` passes (no failing tests, no flaky skips) | run it |
| 1.4 | `cargo clippy --workspace --all-targets -- -D warnings` clean | run it |
| 1.5 | `./scripts/pre-pr-check.sh <crate>` passes for each touched crate | run it |
| 1.6 | No new `#[ignore]` / `#[cfg(disabled)]` on tests without a linked issue + reason | grep diff |

## 2. YAGNI / scope

| # | Check |
|---|---|
| 2.1 | Diff matches the closing issue's scope — no drive-by features, no opportunistic refactors |
| 2.2 | No new abstractions introduced "for future use" (traits, generics, type params) without a current caller |
| 2.3 | No feature flags / config knobs added without a current consumer |
| 2.4 | No backwards-compatibility shims (renamed `_var`s, deprecated re-exports, "// removed" comments) |
| 2.5 | If three similar lines exist, three lines is fine — don't extract a helper unless 4+ uses or non-trivial logic |

## 3. Wiring / completeness

| # | Check |
|---|---|
| 3.1 | New code is reachable from the live app (no orphan types/functions/modules) |
| 3.2 | New crate features are actually toggled by something (config, CLI flag, runtime branch) |
| 3.3 | New trait impls are registered with the relevant registry/dispatcher |
| 3.4 | New events/messages have both producer AND consumer in the diff |
| 3.5 | If the PR title says "wire X", X is end-to-end callable — not just declared |

## 4. Stubs / dead code / placeholders

| # | Check (FAIL if any present in shipping code paths) |
|---|---|
| 4.1 | No `todo!()`, `unimplemented!()`, `unreachable!()` outside test/example code |
| 4.2 | No `Mock*` / `Fake*` / `Placeholder*` types reaching public API |
| 4.3 | No `// TODO:` comments without a linked issue number |
| 4.4 | No `let _ = result;` swallowing errors silently |
| 4.5 | No empty match arms or function bodies that should do something |
| 4.6 | No commented-out code blocks (kill them) |
| 4.7 | No unused `pub` items, unused imports, unused modules |
| 4.8 | No dead code paths behind `if false` / `#[cfg(any())]` |

## 5. Error handling

| # | Check |
|---|---|
| 5.1 | No new `.unwrap()` / `.expect()` in non-test, non-startup code (if needed, must have inline `// SAFETY:` style justification) |
| 5.2 | No new `panic!()` in library code |
| 5.3 | Errors propagated with `?`, not converted to `Option` or swallowed |
| 5.4 | `Result` types use the crate's canonical error, not ad-hoc `String` errors |
| 5.5 | External I/O (network, fs, IPC) is wrapped in timeout or cancellation context |

## 6. Memory / resources

| # | Check |
|---|---|
| 6.1 | No `Box::leak`, `mem::forget`, raw `static mut` introduced |
| 6.2 | No `Arc<Mutex<...>>` held across `.await` (deadlock risk) |
| 6.3 | No unbounded `Vec` / `HashMap` growth on a hot loop without bounds or eviction |
| 6.4 | Spawned tasks (`tokio::spawn`, threads) are tracked — owner can cancel/join them |
| 6.5 | File handles, sockets, GPU resources have explicit drop semantics or RAII guards |
| 6.6 | No reference cycles (`Rc<RefCell<Rc<...>>>`) without a `Weak` somewhere |

## 7. Concurrency

| # | Check |
|---|---|
| 7.1 | `Send` + `Sync` correctness — types crossing thread boundaries are actually safe |
| 7.2 | No `block_on` inside an async context |
| 7.3 | No detached `tokio::spawn` whose result/panic nobody observes |
| 7.4 | Locks held for the minimum scope; no I/O while holding a lock |
| 7.5 | Channel back-pressure is bounded (no unbounded `mpsc` on a hot path) |

## 8. API surface

| # | Check |
|---|---|
| 8.1 | New `pub` items are intentional — internal helpers are `pub(crate)` |
| 8.2 | Public API changes (signatures, return types, schema) are flagged in the handoff Warnings |
| 8.3 | Doc comments on new public items per [rustdoc skill](~/.claude/skills/rustdoc/SKILL.md) |
| 8.4 | No semver-breaking changes to crates that have downstream consumers in this round's other PRs without coordination |

## 9. Style

| # | Check |
|---|---|
| 9.1 | Conforms to [rust-style skill](~/.claude/skills/rust-style/SKILL.md) — for-loops over iterator chains, let-else, shadowing, newtypes, explicit matching |
| 9.2 | No comments narrating the WHAT (well-named identifiers do that). Comments only when WHY is non-obvious |
| 9.3 | No multi-paragraph docstrings on internal items |
| 9.4 | Identifiers descriptive — no single-letter names outside math/idiom |

## 10. CLAUDE.md compliance

| # | Check |
|---|---|
| 10.1 | Diff respects the crate's stated purpose (per CLAUDE.md crate list) — no logic in wrong crate |
| 10.2 | Orchestration rules respected: branch from main, `--force-with-lease`, pre-PR check ran |
| 10.3 | No edits to `ACTIVE.md` from inside the PR (handoff system manages that) |
| 10.4 | No commits authored as the user without justification |

## 11. Tests

| # | Check |
|---|---|
| 11.1 | New behavior has at least one test exercising it |
| 11.2 | Bug fixes have a regression test that fails on `main` and passes with the fix |
| 11.3 | Tests don't sleep — they use deterministic synchronization |
| 11.4 | Tests don't depend on network/filesystem state outside the test fixture |
| 11.5 | Mocks (where used) match the real impl's contract — no mock-only behaviors |

## 12. Phantom-specific

| # | Check |
|---|---|
| 12.1 | If touching `phantom-renderer`: no shader changes that break Metal AND Vulkan AND D3D12 |
| 12.2 | If touching `phantom-agents`: capability/permission model respected (no privilege escalation paths) |
| 12.3 | If touching `phantom-brain`: OODA loop tick budget respected (work bounded per tick) |
| 12.4 | If touching `phantom-supervisor`/`phantom-protocol`: heartbeat compatibility preserved |
| 12.5 | If touching `phantom-net`/peer code: no plaintext credentials, identity verification on every message |
| 12.6 | If adding API calls to Anthropic/OpenAI/cloud services: respects privacy-mode gate |

---

## Reviewer verdict format

The reviewer writes a handoff with **Status** set to one of:

- **`pass`** — every applicable check is PASS. Recommend merge.
- **`pass-with-issues`** — every check is PASS *except* a small number of follow-up-issue-eligible items (per [definition-of-done.md](definition-of-done.md)). Lists them.
- **`fix`** — at least one FAIL that blocks merge. Lists each FAIL with file:line + reason.

The handoff "Next Agent Should" field maps directly:
- `pass` → "spawn merge agent"
- `pass-with-issues` → "spawn issue agent for: [list], then spawn merge agent"
- `fix` → "spawn fix agent to address: [list]"
