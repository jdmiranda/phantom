# ARD-004: Rust Skills Integration & Codebase Audit

**Status**: Accepted
**Date**: 2026-04-22
**Authors**: Jeremy Miranda, Claude

---

## Decision

Adopt community Rust skills for Claude Code as persistent development guardrails. Run a full codebase audit against the skill rule sets to establish a baseline of technical debt and prioritize fixes.

## Context

Phantom is 31K lines across 19 crates. The codebase was built rapidly through AI-assisted sessions. A recurring crash (`assertion failed: self.is_char_boundary(end)` in `update.rs`) exposed that previous sessions were papering over bugs with `catch_unwind(AssertUnwindSafe(...))` instead of fixing root causes. This pointed to a broader pattern: the codebase needed systematic quality enforcement, not ad-hoc hardening.

## Skills Installed

All installed globally at `~/.claude/skills/` (available across all Rust projects).

### Source: davidbarsky (GitHub Gist)

| Skill | Scope | Mode |
|-------|-------|------|
| `rustdoc` | RFC 1574 doc conventions | Auto on doc comments |
| `rust-style` | for-loops, let-else, shadowing, newtypes, explicit matching | Auto on Rust code |
| `rust-analyzer-ssr` | Structural search & replace for semantic refactoring | On demand |

**Reference**: https://gist.github.com/davidbarsky/8fae6dc45c294297db582378284bd1f2

### Source: leonardomso/rust-skills

| Skill | Scope | Mode |
|-------|-------|------|
| `rust-skills` | 179 rules across 14 categories (ownership, errors, memory, API, async, optimization, naming, types, testing, docs, perf, project structure, clippy, anti-patterns) | Auto on Rust code |

**Reference**: https://github.com/leonardomso/rust-skills

### Source: actionbook/rust-skills

| Skill | Scope | Mode |
|-------|-------|------|
| `rust-coding-guidelines` | 50 core rules quick reference | Auto on style questions |
| `rust-m01-ownership` | Ownership/borrow/lifetime with error-to-design-question mapping | Auto on E0382, E0597, etc. |
| `rust-m04-zero-cost` | Generics, traits, static vs dynamic dispatch | Auto on E0277, E0308, etc. |
| `rust-m06-error-handling` | Result vs panic decision flowcharts, thiserror vs anyhow | Auto on error handling |
| `rust-m07-concurrency` | Send/Sync, thread safety, async, deadlock prevention | Auto on concurrency code |

**Reference**: https://github.com/actionbook/rust-skills

---

## Audit Findings

Scan performed 2026-04-22 against all 19 crates. Findings categorized by skill rule violations.

### CRITICAL: Error Handling

| # | Violation | Location | Rule | Impact |
|---|-----------|----------|------|--------|
| E1 | `.unwrap()` on 8 agent index lookups | `headless.rs:336,361,379,401,411,417,427,434` | `err-no-unwrap-prod` | Panic if agent ID invalid |
| E2 | `.unwrap()` on channel send in GPU callback | `screenshot.rs:96` | `err-no-unwrap-prod` | Panic if receiver dropped |
| E3 | Double-panic: `recv().unwrap().expect()` | `screenshot.rs:99` | `err-result-over-panic` | Unrecoverable on GPU buffer map failure |
| E4 | `.unwrap()` on PNG encode | `screenshot.rs:130-131` | `err-no-unwrap-prod` | Panic on encode failure |
| E5 | `.unwrap()` on `log_path.parent()` | `main.rs:206` | `err-no-unwrap-prod` | Panic if path has no parent |
| E6 | `.expect()` on window creation | `main.rs:50` | `err-expect-bugs-only` | Crash on display server failure |
| E7 | `panic!()` in plugin registry fallback | `app.rs:413` | `err-result-over-panic` | Crash if /tmp inaccessible |
| E8 | Silent `let _ =` on crash report write | `main.rs:266` | `anti-empty-catch` | Lost crash data |

### HIGH: Memory & Performance

| # | Violation | Location | Rule | Impact |
|---|-----------|----------|------|--------|
| M1 | `format!()` in render loop (per-frame) | `render.rs:424,550` | `anti-format-hot-path` | Allocation every frame |
| M2 | Vec alloc per text segment in render | `render.rs:601-608` | `mem-reuse-collections` | N allocations per frame |
| M3 | Missing `with_capacity()` on per-frame Vecs | `render.rs:298-299` | `mem-with-capacity` | Reallocation growth per frame |
| M4 | Pixel buffer `.clone()` in screenshot | `update.rs:382` | `anti-clone-excessive` | Full framebuffer copy |
| M5 | Image data `.clone()` in glyph atlas | `atlas.rs:206` | `anti-clone-excessive` | Unnecessary data duplication |

### HIGH: Concurrency

| # | Violation | Location | Rule | Impact |
|---|-----------|----------|------|--------|
| C1 | 9 thread spawns with discarded JoinHandle | `api.rs:369`, `main.rs:80,579`, `listener.rs:720+`, `update.rs:250` | fire-and-forget | Silent thread panics |
| C2 | 13 blocking `.recv()` without timeout | `listener.rs:333-648` (8x), `brain.rs:146` | deadlock risk | MCP/brain threads hang forever if app crashes |
| C3 | `Arc<Mutex<Vec>>` PTY write queue | `terminal.rs:37-38` | channel > shared state | Synchronous blocking on writes |

### MEDIUM: API Design

| # | Violation | Location | Rule | Impact |
|---|-----------|----------|------|--------|
| A1 | 6 bool params where enums clearer | `sysmon.rs:72`, `agent.rs:131`, `router.rs:224,267`, `registry.rs:260`, `tree.rs:174` | `type-enum-states` | Caller confusion |
| A2 | 4 stringly-typed APIs | `host.rs:78,111`, `protocol.rs:257`, `marketplace.rs:86` | `type-no-stringly` | Type safety gap |
| A3 | 0 `#[must_use]` on 59+ Result functions | workspace-wide | `api-must-use` | Silently ignored errors |
| A4 | 35+ wildcard `_ =>` matches on enums | 12+ files | explicit matching | Missing future variants |

### CLEAN

| Area | Result |
|------|--------|
| `unsafe` blocks (9 total) | All correct and necessary (libc FFI) |
| `&String` / `&Vec<T>` parameters | None found — correctly uses `&str` / `&[T]` |
| Lifetime annotations | Appropriate elision used throughout |

---

## Remediation Priority

### Phase 1: Crash Prevention (immediate)
- Fix E1-E7: Replace all production `unwrap()`/`expect()`/`panic!()` with `?`, `let-else`, or graceful degradation
- Already done: `update.rs:46` char boundary fix + `catch_unwind` removal (this session)

### Phase 2: Render Performance (next)
- Fix M1-M3: Pre-allocate buffers, use `write!()` instead of `format!()`, reuse Vecs with `clear()`
- Fix M4-M5: Move instead of clone where possible

### Phase 3: Concurrency Safety (next)
- Fix C2: Add `recv_timeout()` to all MCP listener and brain channel reads
- Fix C1: Collect JoinHandles or use scoped threads for error propagation

### Phase 4: API Hardening (ongoing)
- Fix A3: Add `#[must_use]` to all Result-returning public functions (zero-risk, high-value)
- Fix A4: Replace wildcard matches with explicit variants
- Fix A1-A2: Introduce enums and newtypes at API boundaries

---

## Alternatives Considered

| Option | Pros | Cons |
|--------|------|------|
| **No skills, manual review** | No dependencies | Inconsistent enforcement, bugs slip through between sessions |
| **Clippy only** | Built-in, fast | Doesn't cover API design, architecture, concurrency patterns |
| **Skills (chosen)** | Persistent across sessions, covers full spectrum (179 rules), auto-triggers on relevant code | Adds prompt weight, some rules may conflict with project conventions |
| **Project-specific CLAUDE.md only** | Tailored to Phantom | Doesn't benefit from community knowledge, must be maintained manually |

## Decision Rationale

Skills are complementary to project-specific instructions. They encode community-proven Rust patterns that apply universally. The auto-trigger mechanism means they activate when relevant (ownership errors, error handling, concurrency) without manual invocation. The audit baseline lets us track improvement over time.
