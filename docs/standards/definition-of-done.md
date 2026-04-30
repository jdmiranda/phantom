# Definition of Done — Phantom

How the orchestrator routes a reviewed PR. The reviewer has run [review-rubric.md](review-rubric.md) and produced one of three verdicts. This document tells the orchestrator what to do with each verdict, and exactly when a finding is "merge-with-issue" eligible vs. "send back to fix."

---

## Verdict → orchestrator action

```
pass               → spawn merge agent
pass-with-issues   → spawn issue agent for each follow-up + spawn merge agent
fix                → spawn fix agent with the FAIL list
```

The orchestrator does not merge, does not write issues, and does not fix code. The orchestrator only spawns.

---

## When a finding is merge-with-issue eligible

A finding from the rubric may be carved out into a follow-up issue (rather than blocking the merge) when **all four** are true:

1. **Narrow** — fits in a single agent's worktree, touches < 5 files, can be specified in < 200 words.
2. **Scoped outside the changed code path** — the bug exists in code the PR did not modify, OR it pre-existed on main and the PR didn't make it worse.
3. **Not a regression** — running the tests on this PR's branch is no worse than on main.
4. **Not a safety issue** — does not introduce: privilege escalation, plaintext credentials, panic in the agent loop, memory leak in a hot path, deadlock, supervisor heartbeat break, or anything in §12 of the rubric.

If any one of those is false, the finding blocks the merge. Send to fix.

### Examples — merge-with-issue

- A `// TODO: optimize` in a slow path that the PR didn't touch.
- A misleading log message in a sibling module.
- A doc comment on an existing public function that's slightly stale after this PR.
- A clippy hint on a pre-existing line that just got renumbered by the diff.

### Examples — send to fix

- A `todo!()` reachable from the new code path the PR is supposed to deliver.
- A test failure introduced by this PR (even one).
- An `unwrap()` added by this PR in a non-startup path.
- A public API change that breaks another open PR in the round.
- The PR claims to "wire X end-to-end" but X is reachable only via test code.
- A new mock/placeholder type leaking into a public signature.
- Any concurrency, memory, or supervisor-protocol break.

---

## Definition of done — what `pass` actually requires

A PR is `pass` when **every** check in [review-rubric.md](review-rubric.md) is PASS or N/A and the following ground truths hold:

| # | Ground truth |
|---|---|
| 1 | `cargo build --workspace` produces zero warnings |
| 2 | `cargo test --workspace` produces zero failures and zero new ignored tests |
| 3 | `cargo clippy --workspace --all-targets -- -D warnings` exits 0 |
| 4 | `./scripts/pre-pr-check.sh <crate>` exits 0 for every touched crate (or the script is missing — note in handoff) |
| 5 | The closing issue's acceptance criteria are satisfied by code in this PR (not promised in a follow-up) |
| 6 | The PR's "Files Touched" handoff field matches `git diff origin/main...HEAD --name-only` |
| 7 | New code is reachable from the live binary, not just from tests |
| 8 | No new direct calls to cloud APIs that bypass the privacy-mode gate (rubric §12.6) |
| 9 | The handoff for this PR exists at `~/.wolf/handoffs/phantom/$(date +%Y-%m-%d)/...` and follows the schema |

If 1–4 are red, that's a fix. If 5–9 are red, also fix.

---

## Two-hop pipeline (this round's flow)

```
[steward / fix agent]
   ↓ (handoff: ready-for-review)
[review agent]  ← runs review-rubric.md
   ↓ (handoff: pass | pass-with-issues | fix)
[orchestrator]
   ↓
[merge agent]   or   [fix agent + back to review]   or   [issue agent(s) + merge agent]
```

Two hops, never collapsed. The reviewer never merges. The merge agent never reviews. The fix agent never reviews its own work.

---

## Rebase policy

- Steward / fix agents rebase as their **final action** before handoff so the reviewer sees a tip already on top of `origin/main`.
- The flow inside a fix agent: pull → make code changes → commit → rebase onto `origin/main` → resolve conflicts → push `--force-with-lease` → write handoff.
- If a rebase produces non-trivial conflicts (>50 lines or semantic ambiguity), the agent **must abort** (`git rebase --abort`) and write a `needs-fix` handoff explaining the conflict surface. Conflicts are an orchestrator decision, not a fix-agent shortcut.
- Force-pushes use `--force-with-lease`. Plain `--force` is forbidden.

---

## Merge agent constraints

A merge agent receives a `pass` (or `pass-with-issues` after issue agent confirms issues are filed) handoff and:

1. Runs `gh pr checks <PR>` — all required checks must be green.
2. Runs `cargo build --workspace && cargo test --workspace --no-run` on the PR's tip one last time. If red, refuses to merge and writes a `merge-blocked` handoff back to the orchestrator.
3. `gh pr merge <PR> --squash --delete-branch` (or `--rebase` if the round's branches must preserve commits — orchestrator specifies in the spawn prompt).
4. Removes the local worktree: `git worktree remove .claude/worktrees/pr-<N>`.
5. Writes a handoff with status `merged` and the merge commit SHA.

After every merge, the orchestrator MUST verify main builds clean before spawning the next merge agent (CLAUDE.md orchestration rule #1). The merge agent does not run this check on main itself — the orchestrator spawns a separate post-merge verifier if confidence is needed.

---

## Issue agent constraints

When a `pass-with-issues` verdict needs N follow-up issues filed:

1. Issue agent receives the list of findings + their file:line citations.
2. For each finding, it creates one GitHub issue with: title, repro/observation, file:line, suggested fix surface, label (e.g., `tech-debt`, `bug`), wave label if applicable.
3. Issues link back to the PR they were carved out of: "Carved out of #<PR> during review."
4. The issue agent writes a handoff with the list of created issue numbers, then the orchestrator dispatches the merge agent.

The merge agent's spawn prompt includes the carved-out issue numbers so the squash commit message can reference them.

---

## Out of scope for this document

- The MAST rubric (issue-scoring before spec gate) — see CLAUDE.md orchestration rule #7.
- Merge ordering across a wave — orchestrator decides per-wave based on file overlap.
- Wave definitions — see issue labels on `jdmiranda/phantom`.
