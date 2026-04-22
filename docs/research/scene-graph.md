# Research: Scene Graph Architecture for Phantom

**Date**: 2026-04-21
**Status**: Queued (Task #43)
**Priority**: High — performance + architecture foundation

---

## Problem

Phantom currently re-uploads the entire terminal grid, all quads, and all glyph instances to the GPU **every frame**, even when nothing has changed. For a fullscreen terminal (300+ cols × 90+ rows = 27,000+ cells), this is ~27K glyph lookups + ~27K quad instances rebuilt from scratch 60 times per second.

This gets worse with:
- Multiple panes (each with its own grid)
- Inline images (large textures re-bound every frame)
- Agent panes with streaming output
- Animated tethers/borders (force full re-render of everything, not just the animated parts)

## Current Architecture (Flat)

```
taffy layout → compute rects → for each pane: extract_grid() → GridRenderData::prepare() → upload all quads + glyphs → draw
```

No hierarchy. No dirty tracking. No retained state. Every frame is a full rebuild.

## Target Architecture (Retained Scene Graph)

```
SceneGraph
├── TabBarNode (dirty: false → skip)
├── ContentNode
│   ├── PaneNode[0] (dirty: true → re-upload)
│   │   ├── CellGridNode (retained row textures, only re-upload changed rows)
│   │   ├── CursorNode (animated → always dirty)
│   │   ├── ImageNode[] (retained GPU textures → never re-upload)
│   │   └── BorderNode (static unless detached)
│   ├── TetherNode[0→1] (bezier quads, animated)
│   └── PaneNode[1] (dirty: false → skip)
├── StatusBarNode (dirty: every 1s for clock)
├── PostFxNode (whole-screen CRT shader)
└── SystemOverlayNode (post-CRT layer)
    ├── CommandBarNode
    ├── DebugHudNode
    └── AgentSuggestionNode
```

### Node Properties
- **Local transform**: position, size relative to parent
- **World transform**: computed by walking parent chain (cached, invalidated by dirty flag)
- **Dirty flag**: set when content changes, cleared after GPU upload
- **Visibility**: hidden nodes skip entirely (no GPU work)
- **Z-order**: explicit layer ordering within siblings
- **Render data**: optional retained GPU buffers (quads, glyphs, textures)

### Dirty Propagation
- A node marks itself dirty when its content changes
- Dirty propagates UP (parent knows a child changed → needs to re-composite)
- World transform recomputation propagates DOWN (parent moved → children's world positions change)
- Unchanged subtrees are skipped entirely — no extraction, no upload, no draw calls

## Research: Prior Art

### FrankenTUI (Terminal-Specific)
- **Source**: https://github.com/Dicklesworthstone/frankentui
- **Approach**: Diff-based rendering with dirty-region tracking. Only recompute what changed. When a single text cell changes, only that cell's wrapping is recomputed. Uses the "materialized view" database pattern adapted for frame-rate rendering.
- **Relevance**: Directly applicable — built for terminal UIs. Their RenderPlan where only dirty nodes are recomputed is the pattern we want.

### ori-term (GPU Terminal Emulator in Rust)
- **Source**: https://github.com/upstat-io/ori-term
- **Approach**: Combines terminal emulation, pane multiplexing, and a custom GPU-rendered UI framework in one Rust application.
- **Relevance**: Someone built exactly what we're building. Study their UI framework layer for scene management patterns.

### Bevy Retained Render World (Game Engine)
- **Source**: https://bevy.org/news/bevy-0-15/
- **Approach**: Bevy 0.15 switched from clearing the render world every frame to a retained model where entities persist. Relationships stored as components. This eliminated massive per-frame allocation overhead.
- **Relevance**: The retained-mode pattern is proven at scale. Bevy renders complex 3D scenes; we render 2D terminal content — much simpler, same pattern.
- **Discussion**: https://github.com/bevyengine/bevy/discussions/14437 — Bevy's next-gen scene/UI system design decisions.

### Dirty Flag Pattern (Game Programming Patterns)
- **Source**: https://gameprogrammingpatterns.com/dirty-flag.html
- **Approach**: Classic pattern — add a dirty flag to each object. When local transform changes, set flag. When world transform is needed, check flag: if set, recalculate and clear. Avoids redundant computation in hierarchies.
- **Relevance**: The foundational pattern for our scene graph. Simple, proven, zero overhead when nothing changes.

### Vello (GPU Compute 2D Renderer)
- **Source**: https://github.com/linebender/vello
- **Approach**: 2D graphics engine focused on GPU compute. Draws large 2D scenes with interactive performance. Compute-centric architecture.
- **Relevance**: Their approach to batching 2D draw calls and managing scene state could inform our quad/glyph batching strategy.

### Scene Graph Fundamentals
- **Source**: https://en.wikipedia.org/wiki/Scene_graph
- **Key insight**: "A scene graph is a general data structure that arranges the logical and often spatial representation of a graphical scene." The hierarchy enables: grouping (move a pane and everything inside moves), instancing (reuse the same border style), and culling (off-screen panes don't render).

## Design Decisions

### Why Not ECS (Bevy-style)?
ECS is powerful but overkill for our use case. We have ~50-100 nodes max (panes, bars, overlays), not millions of entities. A simple tree with dirty flags is simpler, faster to implement, and easier to reason about. ECS adds query overhead and architectural complexity we don't need.

### Why Not Immediate Mode (egui-style)?
Immediate mode rebuilds the entire UI every frame — exactly what we're trying to stop doing. It's great for tools/debug UIs but wrong for a terminal emulator where 95% of the screen is static between keystrokes.

### Chosen: Retained Tree with Dirty Flags
- Simple `Vec<Node>` arena with parent/child indices
- Each node owns its GPU buffer handles (optional — leaf nodes only)
- Dirty flag per node, propagation up/down
- Render traversal skips clean subtrees
- Matches FrankenTUI's approach, Bevy's retained world, and the classic game pattern

## Performance Impact Estimate

Current: ~27,000 cells × 60fps = 1.62M glyph lookups/sec + full quad rebuild

With scene graph:
- Idle terminal (no output): 0 glyph lookups, 0 quad rebuilds (cursor blink only)
- Single character typed: 1 row re-uploaded (~300 cells), rest retained
- Pane resize: full re-upload of resized pane only
- Status bar clock: 1 node re-rendered every 1s

Estimated reduction: **95%+ GPU upload work** for typical terminal usage.

## Implementation Plan

1. `phantom-scene` crate with `SceneNode`, `SceneTree`, `DirtyFlags`
2. Integrate between taffy layout output and renderer input
3. Pane nodes own retained glyph/quad buffers
4. PTY read marks pane node dirty
5. Render traversal: walk tree, skip clean subtrees, upload only dirty nodes
6. System overlay nodes render in post-CRT pass (existing architecture)
