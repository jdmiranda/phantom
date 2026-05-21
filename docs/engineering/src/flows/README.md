# Flows

Critical execution paths in Phantom, documented as sequence diagrams with
participants linked to component pages, walkthroughs, and inline gap
callouts.

| # | Flow | Honours | Gaps | What it shows |
|---|---|---|---|---|
| 1 | [Cold launch](01-cold-launch.md) | [ADR-001](../decisions/001-architecture.md), [ADR-003](../decisions/003-pubsub.md) | 3 | Boot → SetupAdapter → first agent · the "agent is king" path |
| 2 | [Agent spawn (Composer sub-agent)](02-agent-spawn.md) | [ADR-001](../decisions/001-architecture.md), [ADR-003](../decisions/003-pubsub.md) | 2 | Tool-driven adapter spawning · the capability gate in action |
| 3 | [Loop tick](03-loop-tick.md) | [ADR-001](../decisions/001-architecture.md) | 4 | `phantom loop run` → triager → implementer → reviewer · the autonomy pipeline |
| 4 | [Brain self-improvement](04-brain-self-improvement.md) | [ADR-001](../decisions/001-architecture.md) | 3 | Phantom-on-phantom · GoalSource → score → enqueue → Flow 3 |

## How each flow page is structured

- **Summary** — 3-5 lines of "what this is, why it matters."
- **Architecture decisions this flow honours** — links to the relevant ADRs.
- **Participants** — every named actor in the diagram, linked to its component page.
- **Sequence diagram** — mermaid, rendered client-side.
- **Walkthrough** — numbered steps mapping diagram edges to source file refs.
- **Gaps surfaced** — anchored callouts that link to [`gaps.md`](../gaps.md).
- **Source files** — copy-paste-ready `crates/<crate>/src/<file>.rs` pointers.

## Conventions in the diagrams

- `==>>` = sync call · `-->>` = async / event · `Note over A` = annotation.
- `[GAP]` notes inline mark unowned / underspecified seams; they link to
  [gaps.md](../gaps.md) by anchor.
- Cross-flow handoffs are NOT clickable inside mermaid (SVG text). Each
  diagram is followed by a `## Sub-flows referenced` markdown block with
  real `[Flow N](Flow-N.md)` links.
