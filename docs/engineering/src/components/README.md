# Components

Phantom is a Rust workspace with **34 members** grouped into 8 functional
rings. Each ring is one page; each page enumerates its crates with status,
ownership, pub/sub topology, and source-file pointers.

| Page | Crates | Concern |
|---|---|---|
| [Substrate](substrate.md) | `phantom` + `phantom-app` + `phantom-supervisor` + `phantom-protocol` + `phantom-adapter` | Plumbing — the binary, IPC, the AppAdapter trait |
| [Rendering](rendering.md) | `phantom-renderer` + `phantom-ui` + `phantom-scene` | GPU pipeline + Taffy layout + design tokens |
| [Terminal](terminal.md) | `phantom-terminal` + `phantom-semantic` | PTY + semantic command understanding |
| [Agents](agents.md) | `phantom-agents` + `phantom-context` + `phantom-nlp` | LLM-driven actors + project context + NLP intent |
| [Brain](brain.md) | `phantom-brain` + `phantom-loop` | OODA loop + the autonomy pipeline |
| [Memory + History](memory-history.md) | `phantom-memory` + `phantom-dag` + `phantom-recall` + `phantom-history` + `phantom-session` | What Phantom remembers + records + restores |
| [Extensibility + Capture](extensibility.md) | `phantom-plugins` + `phantom-mcp` + `phantom-skill-host` + `phantom-vision` + `phantom-bundles` + `phantom-bundle-store` + `phantom-embeddings` | Adding capabilities + frame capture + vectors |
| [Federation + Speech](federation.md) | `phantom-net` + `phantom-relay` + `phantom-hub` + `phantom-fleet` + `phantom-builder` + `phantom-stt` + `phantom-voice` | Multi-instance + relays + speech I/O |

## Reading a component page

Each page has the same shape:

1. **Status** — chip per crate: <span class="chip ok">shipping</span> · <span class="chip warn">stubbed</span> · <span class="chip info">planned</span>
2. **What it does** — 3-5 lines.
3. **Crates** — one paragraph per crate in the group, with key public types.
4. **Owns** — types / traits / state that this component is the source of truth for.
5. **Reads from** — other components + bus topics it subscribes to.
6. **Writes to / publishes** — other components + bus topics it publishes.
7. **Decisions honoured** — links to ADRs that apply.
8. **Open gaps** — links to anchors in [`gaps.md`](../gaps.md).
9. **Source files** — file pointers for the major concepts.

## Note on `phantom-audio`

`phantom-audio` exists in the working tree but is **NOT a workspace member**
(absent from root `Cargo.toml`'s `[workspace.members]`). The
[Federation + Speech](federation.md) page flags it as 🚧 future-shipping with
a callout — when it lands as a workspace member, the chip flips to shipping.

## Note on `phantom-substrate`

No such crate on the current reference branch. The name was reserved in
[`docs/PHASE1-EXECUTION.md`](../../../PHASE1-EXECUTION.md) for an
unstarted layer; if it lands, it'll join [Substrate](substrate.md).
