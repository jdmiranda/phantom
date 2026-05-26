> Canonical source: [`docs/ARD-005-keystroke-glitch-fx.md`](../../../ARD-005-keystroke-glitch-fx.md). Edits go there, not here.

{{#include ../../../ARD-005-keystroke-glitch-fx.md}}

---

## Components affected

- [Rendering](../components/rendering.md) — `phantom-renderer`'s PostFx pipeline hosts the glitch shader pass.
- [Terminal](../components/terminal.md) — the trigger source (per-keystroke cursor position from `TerminalAdapter`).

## Flows that honour this

_(Cross-cutting visual effect; not exercised by a single anchor flow.)_
