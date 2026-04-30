# Agent Prompt Phrasing — Phantom

Standard for how instructions are phrased in any text an agent reads as its directive: spawn prompts, hook-injected instructions, standards docs, README sections referenced in prompts.

---

## The rule

**Every actionable line in an agent prompt must be an imperative or a declarative constraint. Never a question.**

A sentence ending in `?` is a question. Agents answer questions — they write analysis, consider options, produce commentary. Agents execute imperatives and constraints. The difference is not cosmetic: in the 2026-04-30 multi-agent round, prompts containing question-mark sentences routinely produced commentary instead of routed verdicts. The root cause is that the model is a next-token predictor — a question primes it to produce an answer, not an action.

---

## Why this matters

Observed failure mode from the 2026-04-30 multi-agent pipeline run:

- Prompt: "Is this a blocking FAIL or pass-with-issues?"
- Agent output: "This appears to be a blocking FAIL because the test suite fails on the new code path. It is worth considering whether the scope of the fix falls within the original issue. In contrast, pass-with-issues would apply if…"
- Expected output: a `fix` status in the handoff and a file:line citation.

The agent answered the question correctly. It never cleared the checkpoint. The orchestrator had no routable verdict.

**Questions invite deliberation. Imperatives trigger execution.**

---

## Wrong vs. right — canonical examples (from issue #428)

| Wrong (question) | Right (imperative or declarative constraint) |
|---|---|
| "Should you fix it?" | "Fix it." |
| "Is this a blocking FAIL or pass-with-issues?" | "Decide: blocking FAIL or pass-with-issues. Write your verdict in the handoff." |
| "Does the registry consume the data?" | "Verify the registry consumes the data. If not, list it as a wiring gap." |
| "Question for you: is this partial resolution acceptable?" | "Acceptance criterion: if X holds, mark resolved; otherwise mark deferred. State which applies." |

---

## Additional examples — patterns observed in this codebase's prompt templates

The following patterns appear in or near existing spawn prompt templates and standards docs. They are provided as further illustration of the rewrite rule; no existing file currently contains question-mark instruction lines.

| Wrong (question) | Right (imperative or declarative constraint) |
|---|---|
| "Should the steward rebase or abort if there are conflicts?" | "If conflicts exceed 50 lines or are semantically ambiguous, abort the rebase and write a `needs-fix` handoff." |
| "Does the PR title match the closing issue?" | "Confirm the PR title references the closing issue number. FAIL this check if it does not." |
| "Is the agent running for more than 30 minutes?" | "Check agent run time. If it exceeds 30 minutes without an open PR, send a status message." |

---

## Where this rule applies

Apply this rule in every file an agent may read as a directive:

- **Agent spawn prompts** — any string passed to an agent at spawn time.
- **Hook scripts** — text emitted by `.claude/hooks/*.sh` that becomes part of an agent's input context.
- **Standards docs** — `docs/standards/*.md`, which agents read via CLAUDE.md references.
- **CLAUDE.md orchestration rules** — agents read these as behavioral constraints.
- **README sections that agents reference** — any section cited in a spawn prompt or used in context assembly.

**Exempt from this rule:** rationale paragraphs, design-discussion sections, and section headers used for human readers (e.g., "Why does this matter?" as a heading is prose structure, not an instruction to an agent). The heuristic: if the sentence is inside a numbered rule, a bullet in a "Next Agent Should" list, or an instruction block, it is an actionable line and must be a directive.

---

## Three rewrite patterns

### Pattern A — Question → imperative

Use when the question has one obvious answer that the agent must act on.

| Before | After |
|---|---|
| "Should you fix it?" | "Fix it." |
| "Should the agent rebase before pushing?" | "Rebase before pushing." |

### Pattern B — Question → decision rule with verdict

Use when the question is a branching gate. The agent must choose a path and record its choice.

| Before | After |
|---|---|
| "Is this a blocking FAIL or pass-with-issues?" | "Decide: blocking FAIL or pass-with-issues. Write your verdict in the handoff `Status` field." |
| "Is this partial resolution acceptable?" | "Acceptance criterion: if the acceptance criteria in the closing issue are fully met, mark `resolved`; otherwise mark `deferred`. State which applies in the handoff." |

### Pattern C — Question → verification step

Use when the question is a check that may reveal a gap. The agent must run the check and report findings.

| Before | After |
|---|---|
| "Does the registry consume the data?" | "Verify the registry consumes the data. List wiring gaps in the handoff Warnings field if any." |
| "Does the PR title match the closing issue?" | "Confirm the PR title references the closing issue number. FAIL this check if it does not." |

---

## Audit checklist for existing prompts

When authoring or reviewing a spawn prompt, a standards doc update, or a hook script body:

1. Run `rg -n '\?\s*$' <file>` on every prompt-bearing file.
2. For each hit, determine whether it is an actionable instruction line or prose structure (section header, rationale paragraph).
3. For every actionable instruction line that ends in `?`, apply Pattern A, B, or C above.
4. Files to scan at minimum:
   - `docs/standards/*.md`
   - `CLAUDE.md`
   - `.claude/hooks/*.sh`
   - Any prompt strings embedded in orchestration scripts or agent-spawn code.

Run the audit before opening a PR that touches any of these files.

---

## Cross-references

- [handoff-schema.md](handoff-schema.md) — "Next Agent Should" instructions go directly into spawn prompts. Apply this rule there.
- [review-rubric.md](review-rubric.md) — reviewer routing instructions must be imperatives.
- [definition-of-done.md](definition-of-done.md) — orchestrator routing logic must be declarative constraints.
- CLAUDE.md orchestration rule #9.
