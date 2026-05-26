# Decisions (ADRs)

Architecture Decision Records. The canonical source for each decision is
`docs/ARD-NNN-*.md` in the repo root; pages here pull the source verbatim via
mdBook's `{{#include}}` directive and add backlinks to the components and
flows that honour the decision.

> **Editing rule:** edits go to the canonical source file
> (`docs/ARD-NNN-*.md`), never to the wrapper page here.

| # | Title | Components affected | Flows that honour it |
|---|---|---|---|
| [001](001-architecture.md) | Architecture decisions | [substrate](../components/substrate.md), [rendering](../components/rendering.md), [agents](../components/agents.md), [brain](../components/brain.md) | [Flow 1](../flows/01-cold-launch.md), [Flow 2](../flows/02-agent-spawn.md), [Flow 3](../flows/03-loop-tick.md), [Flow 4](../flows/04-brain-self-improvement.md) |
| [002](002-wasm-adapter.md) | WASM app adapter | [substrate](../components/substrate.md), [extensibility](../components/extensibility.md) | (none yet — Phase 2+ work) |
| [003](003-pubsub.md) | App lifecycle + pub-sub | [substrate](../components/substrate.md), [agents](../components/agents.md) | [Flow 1](../flows/01-cold-launch.md), [Flow 2](../flows/02-agent-spawn.md) |
| [004](004-rust-skills.md) | Rust skills audit | (project-wide convention) | (project-wide) |
| [005](005-keystroke-fx.md) | Keystroke glitch FX | [rendering](../components/rendering.md), [terminal](../components/terminal.md) | (cross-cutting) |

## What's missing

The ADRs above cover the architecture decisions made up to 2026-05. New
decisions should land as `docs/ARD-006-…md` and get a wrapper here.
Candidates currently undocumented:

- "Agent is king" cold-launch invariant (`SetupAdapter` → agent swap) — see
  [Flow 1](../flows/01-cold-launch.md).
- D2-over-mermaid migration trigger conditions — see the deferred Phase 2
  sketch in the plan file.
- Themable token system as a workspace-wide convention — see
  [`docs/engineering/theme/custom.css`](../../theme/custom.css).
