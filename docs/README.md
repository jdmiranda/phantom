# Phantom · docs

Three audiences, three layouts:

| For… | Open… |
|---|---|
| Contributors who want the engineering reference (flows, components, ADRs, gaps) | [`engineering/`](engineering/) — mdBook source. Run `cd engineering && mdbook build && open book/html/index.html` for the rendered site. |
| Users / operators who want the polished pitch + feature tour | `html/` — the operator manual. (Currently uncommitted on this branch; will land in a parallel PR.) |
| The vision + plan + execution-state-of-the-union | [`VISION.md`](VISION.md), [`PLAN.md`](PLAN.md), [`PHASE1-EXECUTION.md`](PHASE1-EXECUTION.md) |

## What lives where

```
docs/
├── README.md                       ← you are here
├── VISION.md                       ← the north star
├── PLAN.md                         ← strategic plan
├── PHASE1-EXECUTION.md             ← execution journal
├── HANDOFF.md                      ← inter-agent coordination notes
├── ARD-001..005-*.md               ← architecture decision records (canonical source)
├── engineering/                    ← mdBook engineering reference (this PR)
│   ├── book.toml
│   ├── src/                        ← markdown sources (committed)
│   │   ├── README.md               ← landing
│   │   ├── SUMMARY.md              ← sidebar nav
│   │   ├── flows/                  ← 4 anchor flows (cold-launch, agent-spawn,
│   │   │                              loop-tick, brain-self-improvement)
│   │   ├── components/             ← 8 component pages covering 34 crates
│   │   ├── decisions/              ← 5 ADR wrappers ({{#include}} the source)
│   │   └── gaps.md                 ← cross-cutting gap inventory
│   ├── theme/                      ← custom.css + theme-switch.js (committed)
│   └── book/                       ← rendered HTML output (gitignored)
├── html/                           ← operator manual (uncommitted on this branch)
├── design/                         ← long-form design docs
├── research/                       ← research notes
├── references/                     ← PDFs (gitignored)
└── standards/                      ← coding + review + handoff standards
```

## Building the engineering reference

```bash
cargo install mdbook mdbook-mermaid mdbook-linkcheck2
cd docs/engineering
mdbook build
open book/html/index.html
```

Output lands in `book/`, which is gitignored. The build is reproducible
from `src/` + `theme/` + `book.toml`.

## Why two sites

Different audiences. The operator manual (`html/`) is the polished
user-facing pitch — what Phantom is, what it does for you. The
engineering reference (`engineering/`) is contributor-facing detail —
sequence diagrams, component cross-refs, gap inventory, file-path
pointers into the source tree. Cross-links between the two land when
the operator-manual PR lands.

A future Phase 1b workflow publishes `engineering/book/` to GitHub
Pages so non-contributors can read it without installing mdBook.
