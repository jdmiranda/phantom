# Phantom · Engineering Reference

> "Phantom IS the AI, not a terminal that hosts one."

This site is the **engineering reference** for Phantom. It's contributor-facing
documentation — flow-level sequence diagrams, component pages aggregated from
the 34 workspace crates, the 5 architecture decisions (ADRs), and a
cross-cutting gap inventory surfaced by the flows.

## How to navigate

| You want to… | Go to… |
|---|---|
| Understand a critical execution path | [Flows](flows/README.md) |
| Look up what a crate does | [Components](components/README.md) |
| Find why a design choice was made | [Decisions (ADRs)](decisions/README.md) |
| See what's broken / unowned | [Gap inventory](gaps.md) |
| Search across all pages | top-bar `/` icon (mdBook search) |
| Switch theme (light/dark or 6 palettes) | top-bar paint-brush + the 6 palette dots |

## North star

Phantom is an **AI-native app platform** disguised as a terminal emulator. The
terminal is the interface; intelligence is the substrate; every command is
understood; every error is caught; every "app" (terminal, agent, inspector,
monitor, video, …) is an equal citizen the AI can see and control.

See [`docs/VISION.md`](../../VISION.md) for the full vision document and
[`CLAUDE.md`](../../../CLAUDE.md) for build/run + orchestration rules.

## Five key ideas (from VISION.md)

1. **Everything is an app.** Terminal pane, agent pane, database browser, log
   viewer — all implement the same `AppAdapter` trait, run sandboxed (WASM in
   future), and the AI sees and controls all of them equally.
2. **Apps compose via pub/sub.** Typed event streams between adapters — Unix
   pipes for structured GUI data. Terminal output flows to the semantic
   parser; the parser publishes structured errors; the error detector
   subscribes and triggers agent suggestions. See [ADR-003](decisions/003-pubsub.md).
3. **The AI is ambient.** A brain thread runs continuously, observes
   everything, scores its options with Utility AI (game-AI style), and only
   acts when it has something more useful than silence. See
   [component-brain](components/brain.md).
4. **Spatial intelligence.** Apps declare a `SpatialPreference`; the arbiter
   resolves competing claims; the adapter renders inside whatever rect it gets.
   See [Flow 1 · Cold launch](flows/01-cold-launch.md) for the canonical example.
5. **Phantom remembers.** Per-project memory, session restore, command history,
   semantic command understanding. See [memory + history](components/memory-history.md).

## Status (2026-05-21)

Phantom is a Rust workspace with **34 crates**, all wired into the top-level
`Cargo.toml`. Most are shipping; some are stubbed; a few are planned. The
component pages mark each crate with one of:

- <span class="chip ok">shipping</span> &nbsp; real code, used by other crates
- <span class="chip warn">stubbed</span> &nbsp; types defined; behaviour is a no-op
- <span class="chip info">planned</span> &nbsp; named in `Cargo.toml`, not yet populated

## How this site is built

```
docs/engineering/
├── book.toml              # mdBook config — committed
├── theme/
│   ├── custom.css         # token vocab + 6 palettes
│   └── theme-switch.js    # 6-palette switcher
├── src/                   # source markdown — committed
└── book/                  # rendered output — gitignored, run `mdbook build`
```

Build locally:

```bash
cargo install mdbook mdbook-mermaid mdbook-linkcheck
cd docs/engineering && mdbook build
open book/html/index.html
```

A CI workflow (Phase 1b) publishes `book/` to GitHub Pages so non-contributors
can read it without installing the build tool.

## Conventions

- **Flow pages** end with a `## Files` block of `crates/<crate>/src/<file>.rs`
  pointers so a reader can jump from the diagram straight to the code.
- **Component pages** group crates by ring (substrate / rendering / terminal /
  agents / brain / memory+history / extensibility+capture / federation+speech).
- **ADR pages** `{{#include}}` the canonical `docs/ARD-NNN-*.md` source verbatim
  — edits go to the source file, never to the wrapper.
- **Gaps** have anchors (`<a id="gap-foo"></a>`) so flow pages can link to a
  specific row in [gaps.md](gaps.md).

## What this isn't

- Not the operator manual (that's [`docs/html/`](../../html/index.html) —
  user-facing pitch and feature tour). The engineering reference cross-links
  to it; the two have different audiences.
- Not auto-generated from code. Hand-maintained markdown, intentional prose.
- Not exhaustive. Phase 1 covers 4 anchor flows + 8 component groupings +
  5 ADRs. New flows and components get added as Phantom grows. When the
  corpus crosses ~30 pages, the deferred Phase 2 (DB-backed) plan revives.
