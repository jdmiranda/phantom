# ARD-005: Per-Keystroke Glitch Animation

**Status**: Accepted
**Date**: 2026-04-22
**Authors**: Jeremy Miranda, Claude

---

## Decision

Add a per-keystroke glitch animation that briefly replaces the typed character with cycling block-element glitch characters before locking in to the real character. The effect is a purely visual overlay on the `GridCell` array and never modifies terminal state.

## Context

Phantom's boot sequence (`boot.rs`) already establishes a signature aesthetic: CRT noise, scan beams, and a logo reveal where characters glitch in through randomized block elements before settling. Once the boot sequence finishes and the user starts typing, the terminal feels flat by comparison. The transition from cinematic startup to a static terminal is jarring.

The goal is to extend the boot sequence's "glitch reveal" feel into regular typing, so every keystroke carries a hint of the CRT-era interference pattern. This makes the terminal feel alive at every moment, not just during startup.

## How It Works

1. **Trigger**: On every printable keypress, `KeystrokeFx::trigger(col, row)` records the cursor position and the current `Instant`.
2. **Tick**: Each frame, `KeystrokeFx::tick()` increments a wrapping frame counter and expires any glitch cells older than 120ms (`GLITCH_DURATION_SECS = 0.12`).
3. **Apply**: `KeystrokeFx::apply()` walks the active glitch cells and, for each one still in the "glitch phase" (first 75% of the duration), replaces the character in the grid with a cycling glitch character. The last 25% is the "lock-in" phase where the real character shows through.
4. **Glitch selection**: A deterministic noise hash (`noise_hash(row, col, frame)`) selects from the same 27-character `GLITCH_CHARS` palette used in the boot sequence. The hash uses Knuth multiplicative constants and wrapping arithmetic, producing a different glyph each frame without any randomness source.
5. **Space skip**: Spaces are never glitched (prevents visual noise in empty regions).
6. **Bounds safety**: `grid.get_mut(idx)` returns `Option`, so out-of-bounds cursor positions are silently ignored via `let-else`.

### Timeline Per Keystroke

| Phase | Window | Visual |
|-------|--------|--------|
| Glitch | 0 -- 90ms (75% of 120ms) | Cell shows cycling block elements (new glyph each frame) |
| Lock-in | 90 -- 120ms | Real character shows through |
| Expired | >120ms | Cell removed from tracking |

## Architecture

The `KeystrokeFx` struct is a pure visual overlay. It never reads from or writes to the terminal's internal state (PTY buffer, cursor model, scrollback). The pipeline is:

```
Keypress event
  -> trigger(col, row) records position + timestamp
  -> terminal processes the key normally (PTY write, grid update)
  -> next frame: tick() expires old cells, apply() mutates GridCell.ch in the render-ready grid
  -> GPU renders the modified grid
  -> real character is already in terminal state, untouched
```

This separation means:
- **No terminal corruption**: The glitch only touches the render-time copy of the grid. If the effect is disabled or panics, the terminal is unaffected.
- **No scrollback pollution**: Glitch characters never enter the scrollback buffer or PTY stream.
- **Focused pane only**: `apply()` is called only for the currently focused pane's grid, so background panes render without glitch overhead.

## Performance Characteristics

| Metric | Value | Rationale |
|--------|-------|-----------|
| Vec initial capacity | 16 | Covers fast typing bursts without reallocation. Even 120 WPM only produces ~10 keypresses per second; at 120ms expiry, the Vec holds ~1-2 entries typically. |
| Expiry time | 120ms | Fast enough to feel instant, long enough to be visible. Matches the boot logo reveal timing. |
| Per-frame cost | O(n) where n = active glitch cells | Typically n <= 2. Walk + hash + index into grid. No allocations. |
| Hash function | 3 wrapping multiplies + 1 XOR shift | Same cost as the boot noise. No PRNG state, no branches. |
| Memory | 24 bytes per `GlitchCell` (2 usize + Instant) | At peak typing, <384 bytes total. |

## Alternatives Considered

| Option | Pros | Cons |
|--------|------|------|
| **Shader-based per-cell glitch** | GPU-native, zero CPU cost for the effect itself | Requires passing per-cell timing data to the shader (new uniform buffer or texture). Adds GPU pipeline complexity. Hard to synchronize with cursor position. Overkill for a sparse, short-lived effect. |
| **Per-cell timer map (HashMap<(col,row), Instant>)** | O(1) lookup per cell | HashMap overhead (hashing, buckets) is heavier than linear scan of 1-2 entries. Expiry requires periodic full scan anyway. Over-engineered for the typical cardinality. |
| **Ring buffer of fixed size** | No allocation ever, cache-friendly | Fixed cap means either wasting memory (large ring) or dropping animations (small ring). Vec with capacity 16 is effectively the same thing with dynamic fallback. |
| **Modify terminal state directly** | Simpler pipeline (no separate apply step) | Corrupts scrollback, breaks copy-paste, breaks screen readers, breaks search. Fundamentally wrong — the effect is cosmetic, not semantic. |
| **Vec with capacity 16 (chosen)** | Minimal memory, O(n) on tiny n, no allocations in steady state, trivially correct expiry via `retain()` | Linear scan is O(n), but n is bounded by typing speed * expiry window, making it effectively O(1). |

## Decision Rationale

The boot sequence proved that character-level glitch effects create Phantom's signature feel. Extending this to regular typing with a lightweight, non-invasive overlay was the natural next step. The `Vec<GlitchCell>` approach was chosen because it is the simplest implementation that is correct: no allocations in steady state, no terminal state corruption, no GPU pipeline changes, and trivially testable (4 unit tests cover the full lifecycle, grid mutation, space skipping, and bounds safety).
