# Research: Spatial Negotiation Protocol for Scene Graph

**Date**: 2026-04-21
**Status**: Design Phase
**Priority**: High — enables the "everything is an app" architecture

---

## The Problem

Current layout is dumb — user manually splits panes, each gets 50% of the space. Apps can't express preferences. An app that needs a tall narrow sidebar gets the same square pane as a full-width terminal. The scene graph knows positions but not intent.

## What We Want

Apps declare spatial **preferences**, a **layout arbiter** resolves conflicts, and apps can **negotiate** with neighbors:

```
DatabaseApp: "I want 2 panes, stacked vertically, minimum 60 cols wide"
TerminalApp: "I want at least 80 cols"
Available:   200 cols total

Arbiter: DatabaseApp gets 80 cols (2 stacked panes of 80×45)
         TerminalApp gets 120 cols
         Both above their minimums. Resolved.
```

## Research: How Others Solve This

### Wayland's xdg-toplevel Protocol

**Source**: [Wayland Protocol Book](https://wayland-book.com/xdg-shell-in-depth/configuration.html), [XDG Shell Protocol](https://wayland.app/protocols/xdg-shell)

The state-of-the-art in window/compositor negotiation:

- **Two-phase negotiation**: compositor sends `configure` event (suggested size), client sends `ack_configure` (accepted). Neither side dictates — it's a conversation.
- **Size hints**: apps declare `min_size` and `max_size`. Compositor uses these to make layout decisions. "A tiling window manager may use this information to place and resize client windows in a more effective way."
- **Configure with zero dimensions**: "if width or height arguments are zero, it means the client should decide its own window dimension" — the compositor is saying "you choose."
- **Key insight**: the compositor has the final word, but informed by client preferences.

### Cassowary Constraint Solver

**Source**: [Cassowary Documentation](https://cassowary.readthedocs.io/en/latest/topics/theory.html), [Wikipedia](https://en.wikipedia.org/wiki/Cassowary_(software))

The algorithm behind Apple's Auto Layout (iOS/macOS):

- **Linear constraints**: `view.width >= 200`, `view.left == neighbor.right + 8`, `view.width == parent.width * 0.5`
- **Required vs preferred**: constraints have strength levels. Required constraints must be satisfied. Preferred constraints are satisfied "as much as possible."
- **Incremental solving**: when one constraint changes, the solver doesn't start from scratch — it incrementally updates. Perfect for interactive resize.
- **Key insight**: apps express relationships, not absolute positions. "My width should be at least 200 but preferably 400, and I should be to the right of the sidebar."

### Constraint-Based Tiling (Academic)

**Source**: [ScienceDirect](https://www.sciencedirect.com/science/article/pii/S2352220817300238), [IEEE](https://ieeexplore.ieee.org/document/4056889/)

- **Tiling algebra**: mathematical framework for describing tiling layouts with constraints. Minimum sizes can be guaranteed and layouts remain solvable through inequality constraints.
- **Key insight**: tiling + constraints = apps that always fit, never overlap, and respect minimums.

### i3/Sway Tree-Based Tiling

**Source**: [i3 User Guide](https://i3wm.org/docs/userguide.html), [i3 IPC](https://i3wm.org/)

- **Tree structure**: i3 organizes windows in a tree where each non-leaf node has an orientation (horizontal/vertical) and layout mode.
- **IPC protocol**: third-party programs can query the tree, get window info, and send layout commands.
- **Size hints ignored in tiling**: i3 v4.3+ ignores X11 size increment hints for tiled windows. The tiler decides.
- **Key insight**: the compositor is the authority, not the app. Apps can request but not demand.

### Game Engine Spatial Partitioning

**Source**: [Game Programming Patterns](https://gameprogrammingpatterns.com/spatial-partition.html)

- **Spatial queries**: "what objects are near this location?" Quadtrees, BSP trees, spatial hashes.
- **For Phantom**: apps query the scene graph — "who's my left neighbor?" "is there 200px of free space below me?"
- **Key insight**: the scene graph becomes queryable, not just a render tree.

## Phantom's Design: Spatial Negotiation Protocol

### App Spatial Preferences

```rust
/// What an app wants from the layout system.
struct SpatialPreference {
    /// Minimum dimensions to function.
    min_size: (u32, u32),          // (cols, rows)
    /// Preferred dimensions.
    preferred_size: (u32, u32),
    /// Maximum useful dimensions (beyond this, wasted space).
    max_size: Option<(u32, u32)>,
    /// Aspect ratio preference (width:height). None = flexible.
    aspect_ratio: Option<f32>,
    /// How many sub-panes this app manages internally.
    internal_panes: u32,           // 1 = single, 2 = split, etc.
    /// Preferred internal layout.
    internal_layout: InternalLayout,
    /// How important is it that preferences are met? (0.0-1.0)
    priority: f32,
}

enum InternalLayout {
    Single,
    VerticalStack(u32),   // N panes stacked
    HorizontalStack(u32), // N panes side by side
    Grid(u32, u32),       // rows × cols
    Custom,               // app manages its own sub-layout
}
```

### Neighbor Queries

```rust
/// Apps can query the scene graph about their spatial context.
trait SpatialQuery {
    /// Who is my immediate neighbor in this direction?
    fn neighbor(&self, direction: Direction) -> Option<NodeId>;
    
    /// How much free space is available in this direction?
    fn available_space(&self, direction: Direction) -> f32;
    
    /// Request a resize of self (arbiter decides).
    fn request_resize(&self, new_size: (f32, f32)) -> ResizeResult;
    
    /// Request that a neighbor shrink to make room.
    fn negotiate_with_neighbor(&self, neighbor: NodeId, request: SpaceRequest) -> NegotiationResult;
}

enum Direction { Up, Down, Left, Right }

enum ResizeResult { 
    Granted(f32, f32),           // got exactly what was asked
    Partial(f32, f32),           // got less than requested
    Denied { reason: String },   // can't do it
}

enum NegotiationResult {
    Accepted,                    // neighbor agreed
    CounterOffer(f32, f32),     // neighbor offers different size
    Rejected,                    // neighbor at minimum, can't shrink
}
```

### The Layout Arbiter

```rust
/// Makes the final call on spatial allocation.
struct LayoutArbiter {
    scene: SceneTree,
    constraints: Vec<Constraint>,
}

impl LayoutArbiter {
    /// An app requests space. Arbiter resolves against all constraints.
    fn allocate(&mut self, app_id: NodeId, pref: &SpatialPreference) -> AllocationResult;
    
    /// Resolve all pending requests at once (batch mode).
    fn resolve_all(&mut self) -> Vec<(NodeId, Rect)>;
    
    /// Can this preference be satisfied without violating any minimums?
    fn can_satisfy(&self, pref: &SpatialPreference) -> bool;
}
```

The arbiter's algorithm:
1. Collect all spatial preferences from all apps
2. Sort by priority (higher priority = first pick)
3. Allocate preferred sizes where possible
4. If space is tight, fall back to minimum sizes
5. If minimums can't all be met, deny the lowest-priority request
6. Apply Cassowary-style constraint solving for complex layouts
7. Notify all affected apps of their final size

### Example: Database App Requesting 2 Panes

```
1. DatabaseApp calls: allocate(pref: {
       min_size: (60, 20),
       preferred_size: (80, 50),
       internal_panes: 2,
       internal_layout: VerticalStack(2),
       priority: 0.7
   })

2. Arbiter checks scene graph:
   - Total available: 200 cols × 90 rows
   - TerminalApp claims: min 80 cols, currently using 200 cols
   - Free space: 0 (TerminalApp has it all)

3. Arbiter negotiates:
   - TerminalApp priority: 0.5 (lower than DatabaseApp's 0.7)
   - TerminalApp min: 80 cols
   - Can shrink TerminalApp to 120 cols → frees 80 cols

4. Arbiter allocates:
   - TerminalApp: 120 × 90
   - DatabaseApp: 80 × 90 (internally: 2 panes of 80 × 45)

5. Both apps notified of new sizes via configure event.
```

## Integration with Existing Architecture

- **Scene graph** (phantom-scene): already has nodes with transforms. Add `SpatialPreference` to nodes.
- **Taffy layout**: currently used for chrome (tab/status bar) + pane grid. The arbiter wraps taffy, feeding it constraints derived from spatial preferences.
- **AppAdapter trait**: add `fn spatial_preference(&self) -> SpatialPreference` method.
- **AI brain**: the arbiter's decisions are observable events. The brain can learn layout patterns: "this user always puts the terminal on the left and the agent on the right."

## References

- [Wayland xdg-shell: Configuration & Lifecycle](https://wayland-book.com/xdg-shell-in-depth/configuration.html)
- [XDG Shell Protocol Spec](https://wayland.app/protocols/xdg-shell)
- [Cassowary Constraint Solver](https://cassowary.readthedocs.io/en/latest/topics/theory.html)
- [Cassowary — Wikipedia](https://en.wikipedia.org/wiki/Cassowary_(software))
- [Constraint-Based Tiled Windows (IEEE)](https://ieeexplore.ieee.org/document/4056889/)
- [Tiling Algebra for Constraint-Based Layout](https://www.sciencedirect.com/science/article/pii/S2352220817300238)
- [i3 Window Manager User Guide](https://i3wm.org/docs/userguide.html)
- [Game Programming Patterns: Spatial Partition](https://gameprogrammingpatterns.com/spatial-partition.html)
- [Tiling Window Manager — Wikipedia](https://en.wikipedia.org/wiki/Tiling_window_manager)
