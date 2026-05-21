# Flow 4 · Brain self-improvement (phantom-on-phantom)

[← back to flows index](README.md)

The brain pulls candidate work from GitHub (its own repository,
`jdmiranda/phantom`), scores each candidate, applies hard exclusions +
rate limits + a trust budget, and enqueues the highest-scoring item onto
a downstream loop queue. From there, Flow 3 takes over.

## Architecture decisions this flow honours

- [ADR-001 · Architecture decisions](../decisions/001-architecture.md) — the
  brain runs on its own thread, scores with utility AI.

See the long-form design at
[`docs/design/brain-self-improvement.md`](../../../design/brain-self-improvement.md).

## Participants

- **GoalSource** — trait with two production impls:
  - `GhIssueGoalSource` — polls `gh issue list` for open issues on
    `jdmiranda/phantom`.
  - `GhCiFailureGoalSource` — polls `gh run list` for failing workflow runs.
- **score_candidate** — weighted-sum scorer.
- **HardExclusions** — keyword filter.
- **TrustBand** — 4 autonomy bands (SuggestionOnly → Conservative →
  Standard → Aggressive).
- **RateLimiter** — per-hour / per-day ceilings + cooldown.
- **AuditEntry** — JSONL audit log.
- **AiAction::EnqueueLoopMessage** — the brain's action variant.
- **LoopQueueActionHandler** — bridge between brain actions and
  `LoopQueueRegistry`.

Downstream: [Flow 3](03-loop-tick.md) picks up from there.

## Sequence

```mermaid
sequenceDiagram
    autonumber
    participant Brain as Brain thread
    participant Src as GoalSource (gh issues + ci)
    participant Score as score_candidate
    participant Excl as HardExclusions
    participant Trust as TrustBand
    participant Rate as RateLimiter
    participant Audit as AuditEntry log
    participant Action as AiAction::EnqueueLoopMessage
    participant Bridge as LoopQueueActionHandler
    participant QReg as LoopQueueRegistry

    Brain->>Brain: OODA tick (Observe → Orient → Decide → Act)
    Brain->>Src: poll candidates
    Src-->>Brain: [issue#NNN, ci_run#MMM, …]

    loop per candidate
        Brain->>Excl: matches hard-exclusion keywords?
        alt excluded
            Excl-->>Brain: skip
            Brain->>Audit: AuditEntry { decision: "skip", reason: "excluded" }
        else not excluded
            Excl-->>Brain: pass
            Brain->>Score: weighted-sum (severity, recency, blast radius, attempts)
            Score-->>Brain: score ∈ [0.0..1.0]
            Brain->>Brain: floor at 0.85 if label ∈ {critical, regression, blocker}

            Brain->>Trust: current TrustBand?
            Trust-->>Brain: band → threshold
            alt score >= threshold
                Brain->>Rate: within per-hour + per-day + cooldown windows?
                alt allowed
                    Rate-->>Brain: ok
                    Brain->>Action: AiAction::EnqueueLoopMessage { queue, payload }
                    Action->>Bridge: enqueue_loop_message(queue, msg)
                    Bridge->>QReg: push(implementer-queue, LoopMessage{external_id, score, …})
                    QReg-->>Bridge: ok
                    Brain->>Audit: AuditEntry { decision: "enqueue", score, breakdown, queue }
                    Brain->>Trust: record success (ramp up)
                else rate-limited
                    Rate-->>Brain: blocked
                    Brain->>Audit: AuditEntry { decision: "skip", reason: "rate-limit" }
                end
            else below threshold
                Brain->>Audit: AuditEntry { decision: "skip", reason: "below-threshold", score, band }
            end
        end
    end

    Note over QReg: → Flow 3 LoopMessageQueueSource picks up the message
```

**GAP** · [brain-trust-band-ramp-ux](../gaps.md#gap-brain-trust-band-ramp-ux) —
TrustBand ramps are invisible to the operator.

**GAP** · [brain-self-improve-opt-in](../gaps.md#gap-brain-self-improve-opt-in) —
`SelfImprovementConfig::enabled = false` by default; opt-in requires
hand-editing the config file.

**GAP** · [brain-goal-source-rate-limit](../gaps.md#gap-brain-goal-source-rate-limit) —
unauthenticated `gh` API gives 60/hr; the source silently returns empty
when rate-limited.

## Walkthrough

1. **OODA tick** — the brain runs on its own thread. Each tick runs the
   Observe / Orient / Decide / Act loop. Self-improvement is one of
   several "Orient → Decide" branches.
2. **GoalSource polls** — `GhIssueGoalSource` runs `gh issue list -R
   jdmiranda/phantom --state open --json …`; `GhCiFailureGoalSource` runs
   `gh run list --status failure --json …`.
3. **HardExclusions** — keyword filter against title / labels / body.
   Issues labelled `auto-triage-skip` are filtered.
4. **score_candidate** — weighted-sum of severity, recency, blast radius,
   prior agent attempts.
5. **CRITICAL_LABEL_FLOOR** — issues labelled `critical` / `regression` /
   `blocker` get score floored at 0.85.
6. **TrustBand** — current band determines the score threshold. Banding
   starts at Standard and ramps based on outcomes.
7. **RateLimiter** — per-hour, per-day, and per-candidate cooldown.
8. **AiAction::EnqueueLoopMessage** —
   `LoopQueueActionHandler` translates the action into a typed
   `LoopMessage` and pushes onto `LoopQueueRegistry`.
9. **AuditEntry** — every decision (enqueue OR skip) appends a JSONL row
   to the audit log path.
10. **Hand-off to Flow 3** — the loop runner pulls the message via
    `LoopMessageQueueSource` and the rest is [Flow 3](03-loop-tick.md)
    from there.

## Source files

| Concept | File |
|---|---|
| Self-improvement reconciler | [`crates/phantom-brain/src/self_improvement.rs`](../../../../crates/phantom-brain/src/self_improvement.rs) |
| GoalSource trait | [`crates/phantom-brain/src/goal_source/mod.rs`](../../../../crates/phantom-brain/src/goal_source/mod.rs) (+ `gh_issues.rs`, `gh_ci.rs`) |
| AiAction variant | [`crates/phantom-brain/src/events.rs`](../../../../crates/phantom-brain/src/events.rs) |
| Design doc | [`docs/design/brain-self-improvement.md`](../../../design/brain-self-improvement.md) |
| Loop CLI entry | [`crates/phantom/src/loop_cli.rs`](../../../../crates/phantom/src/loop_cli.rs) |
