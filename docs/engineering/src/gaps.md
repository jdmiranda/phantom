# Gap Inventory

Cross-cutting gaps surfaced by the engineering flows. Each row has an
anchor; flow pages link to specific rows via fragment IDs (e.g.
`gaps.md#gap-arbiter-leftover`).

**Severity legend**:
- <span class="chip danger">blocking</span> — the flow can't complete correctly without it
- <span class="chip warn">degraded</span> — flow completes but with a UX or correctness penalty
- <span class="chip info">in-design</span> — a draft design proposal exists in `docs/design/`
- <span class="chip">cosmetic</span> — minor

## Surfaced gaps

<a id="gap-arbiter-leftover"></a>

### gap-arbiter-leftover · Arbiter Phase 3 doesn't redistribute leftover height to unbounded adapters

**Surfaced in**: [Flow 1 · Cold launch](flows/01-cold-launch.md)
**Severity**: <span class="chip warn">degraded</span>
**Owner**: `phantom-ui`
**Status**: <span class="chip warn">workaround shipped</span>

The layout arbiter's Phase 3 (redistribute leftover space) only considers
adapters whose `allocated_h < preferred_h`. An adapter with no `max_size`
that has already reached its `preferred_h` does NOT eat the remaining space,
leaving a black void below the content. Worked around in
`crates/phantom-app/src/adapters/agent.rs::spatial_preference` by setting
`preferred_size: (500, 200)` — a "larger than any monitor" sentinel — but the
underlying behaviour is wrong; an unbounded `max_size: None` should grow.

**Fix sketch**: in `arbiter.rs::negotiate` Phase 3, change the eligibility
filter from `allocated_h < preferred_h` to `allocated_h < preferred_h OR max_h.is_none()`.

---

<a id="gap-silent-spawn-failure"></a>

### gap-silent-spawn-failure · `spawn_agent_pane` silently no-ops on missing API key

**Surfaced in**: [Flow 1 · Cold launch](flows/01-cold-launch.md)
**Severity**: <span class="chip warn">degraded</span> (was blocking until fix)
**Owner**: `phantom-app::agent_pane::spawn`
**Status**: <span class="chip ok">fixed on this branch</span>

When `resolve_api_config` returns `None` (no `ANTHROPIC_API_KEY` /
`OPENAI_API_KEY`), the spawn path returned `None` with a single `warn!` line.
The user saw a SetupAdapter forever and no idea what was wrong. Now emits
`log::error!` + `console.system(...)` line so the failure is visible in the
in-app console.

**Permanent fix**: a small surface in `phantom auth login` (CLI subcommand
already exists) plus a clickable "set API key" affordance on the
SetupAdapter pane.

---

<a id="gap-cmd-t-from-setup"></a>

### gap-cmd-t-from-setup · `Cmd+T` from SetupAdapter splits instead of swaps

**Surfaced in**: [Flow 1 · Cold launch](flows/01-cold-launch.md)
**Severity**: <span class="chip">cosmetic</span>
**Owner**: `phantom-app::pane`
**Status**: <span class="chip ok">fixed on this branch</span>

`Action::NewTab` → `split_focused_pane(true)` unconditionally spawned a
TerminalAdapter into the split CHILD. With a SetupAdapter as the only
adapter, the user got a half-screen Setup beside a new half-screen terminal.
Now special-cased: from a SetupAdapter, the keybind uses
`kill_keeping_pane` to swap a full-window TerminalAdapter in-place.

---

<a id="gap-capability-class-propagation"></a>

### gap-capability-class-propagation · CapabilityClass enum exists in three independent forms

**Surfaced in**: [Flow 2 · Agent spawn](flows/02-agent-spawn.md)
**Severity**: <span class="chip warn">degraded</span>
**Owner**: cross-cutting (`phantom-agents`, `phantom-relay`, `phantom-hub`)
**Status**: <span class="chip info">in-design</span>

`phantom-agents::role::CapabilityClass` (the canonical 5-variant Sense /
Reflect / Compute / Act / Coordinate enum) is duplicated by independent
enums in `phantom-relay::grant` and `phantom-hub::auth`. Adding a new class
to one does NOT extend the others. A single shared definition (likely
hoisted to `phantom-protocol` or `phantom-adapter`) is the right shape.

**Fix sketch**: hoist `CapabilityClass` to `phantom-protocol`; re-export
from the three current sites; mark deprecated; remove in a follow-up.

---

<a id="gap-fast-path-audit-trail"></a>

### gap-fast-path-audit-trail · Fast-path auto-approve events have no UI surface

**Surfaced in**: [Flow 2 · Agent spawn](flows/02-agent-spawn.md)
**Severity**: <span class="chip warn">degraded</span>
**Owner**: `phantom-app::inspector`
**Status**: <span class="chip">open</span>

`Event::FastPathTaken { agent_id, kind, reason }` is emitted on the bus when
an agent hits `try_auto_approve_with_audit` (the dispatch-time fast path).
Inspector's event log shows it as a one-line entry but there's no dedicated
view summarising fast-path activity per agent, nor a way to revoke an
auto-approval. Important for debugging "why did the agent skip the
capability gate".

---

<a id="gap-loop-mid-flight-cancel"></a>

### gap-loop-mid-flight-cancel · `phantom loop run` has no documented abort path

**Surfaced in**: [Flow 3 · Loop tick](flows/03-loop-tick.md)
**Severity**: <span class="chip warn">degraded</span>
**Owner**: `phantom-loop`
**Status**: <span class="chip">open</span>

Once `phantom loop run --repo X --loops a,b,c` is in flight, the only abort
path is Ctrl-C (which triggers the `Drop`-based RunLock release). There's no
graceful "drain in-flight, then stop" surface; no `phantom loop stop` CLI
command. Long-running implementer agents get SIGTERM'd mid-thought.

---

<a id="gap-loop-exit-schema-error-uplift"></a>

### gap-loop-exit-schema-error-uplift · ExitSchema validation failure surfaces as opaque flatline

**Surfaced in**: [Flow 3 · Loop tick](flows/03-loop-tick.md)
**Severity**: <span class="chip warn">degraded</span>
**Owner**: `phantom-loop::runner::fsm`
**Status**: <span class="chip">open</span>

Three consecutive `complete_task` calls with payloads that don't validate
against the loop's `ExitSchema` flatlines the pane. The user sees an agent
that "stopped working" with no inline reason — the validation error lives
in the event log but isn't surfaced on the failed pane.

---

<a id="gap-loop-quarantine-cascade-ux"></a>

### gap-loop-quarantine-cascade-ux · Quarantine cascade is invisible

**Surfaced in**: [Flow 3 · Loop tick](flows/03-loop-tick.md)
**Severity**: <span class="chip warn">degraded</span>
**Owner**: `phantom-brain::reconciler` + `phantom-app::inspector`
**Status**: <span class="chip">open</span>

`QuarantineRegistry` tags failed agents as `Quarantined`; the brain
reconciler routes their completions through `record_quarantine_failure`.
But the Inspector pane doesn't show "this agent is quarantined" inline —
the user has to look at the event log and infer the state.

---

<a id="gap-loop-watchdog-vs-supervisor"></a>

### gap-loop-watchdog-vs-supervisor · Loop watchdog and Supervisor are separate restart loops

**Surfaced in**: [Flow 3 · Loop tick](flows/03-loop-tick.md)
**Severity**: <span class="chip">cosmetic</span>
**Owner**: cross-cutting (`phantom-loop` + `phantom-supervisor`)
**Status**: <span class="chip">open</span>

`scripts/phantom-loop-forever.sh` is the de-facto loop watchdog (passes
`--max-runtime-min` so loops restart on a timer). `phantom-supervisor` is
the GUI-process supervisor. Two unrelated restart concepts with different
log surfaces. Not broken, just confusing.

---

<a id="gap-brain-trust-band-ramp-ux"></a>

### gap-brain-trust-band-ramp-ux · TrustBand ramps are invisible to the user

**Surfaced in**: [Flow 4 · Brain self-improvement](flows/04-brain-self-improvement.md)
**Severity**: <span class="chip warn">degraded</span>
**Owner**: `phantom-brain::self_improvement`
**Status**: <span class="chip">open</span>

The brain's `TrustBand` ramps up on enqueue success and down on failure
(SuggestionOnly → Conservative → Standard → Aggressive). The user sees
agents being spawned but has no visibility into "we just earned trust"
or "we're in cooldown." The audit log captures it; the Inspector pane
doesn't.

---

<a id="gap-brain-self-improve-opt-in"></a>

### gap-brain-self-improve-opt-in · Self-improvement is opt-in but has no UI affordance

**Surfaced in**: [Flow 4 · Brain self-improvement](flows/04-brain-self-improvement.md)
**Severity**: <span class="chip warn">degraded</span>
**Owner**: `phantom-brain::self_improvement` + `phantom-app::settings_ui`
**Status**: <span class="chip">open</span>

`SelfImprovementConfig::enabled` defaults to `false`. Operator must opt in
via config file edit. No `phantom self-improve enable` CLI; no settings
toggle. The opt-in friction is intentional (per
[`docs/design/brain-self-improvement.md`](../../design/brain-self-improvement.md))
but the path is undocumented.

---

<a id="gap-brain-goal-source-rate-limit"></a>

### gap-brain-goal-source-rate-limit · GoalSource has no GitHub-API rate-limit handling

**Surfaced in**: [Flow 4 · Brain self-improvement](flows/04-brain-self-improvement.md)
**Severity**: <span class="chip warn">degraded</span>
**Owner**: `phantom-brain::goal_source`
**Status**: <span class="chip">open</span>

`GhIssueGoalSource` polls `gh issue list` on a tick. On a fresh clone with
no `GITHUB_TOKEN`, the `gh` CLI uses the unauthenticated 60/hr rate limit;
polling every few minutes blows through it. The source returns an empty
list silently — the brain thinks nothing is happening.

---

## Closed gaps (recently fixed; kept for context)

- `gap-silent-spawn-failure` (above) — fixed on `feat/fleet-builder-integration-shim`.
- `gap-cmd-t-from-setup` (above) — fixed on `feat/fleet-builder-integration-shim`.

---

## Triggering Phase 2 (the DB-backed engineering docs)

When this page crosses ~30 gaps, or when multiple agents start authoring
docs concurrently and stepping on each other, the deferred Phase 2 plan
in `~/.claude/plans/silly-chasing-feather.md` revives — see "Phase 2
(DEFERRED — not in this PR)" in that file. Until then, this hand-maintained
markdown table is the source of truth.
