# Terminal adapter rewrite — SPEC

## Goal

Make `crates/phantom-app/src/adapters/terminal.rs::Renderable::render` emit
the exact visual shape that `docs/mockups/apps.html` `#terminal` shows.

## Reference

- Mockup: `docs/mockups/apps.html` lines 379-397.
- Style: `docs/mockups/apps.html` lines 69-90 (`.term-body`) plus
  `docs/mockups/system.css` lines 314-344 (`.app`, `.app-head`,
  `.app-body`).

## Visual contract (per frame)

The pane is a single rounded card containing two regions stacked
vertically.

1. **Outer card**
   - background `surface_floating` (fallback `surface_raised`)
   - border 1 px, color `chrome_frame_dim`
   - corner radius 10 px (approximated where the renderer cannot draw a
     true rounded rect — see Dependencies)

2. **App head strip**
   - height around 44 px
   - background `surface_floating`
   - bottom hairline 1 px `chrome_divider`
   - icon glyph `▶` in `text_bright` (mapped to `text_accent`) at the
     left
   - name `TERMINAL`, uppercase, `text_bright` (`text_accent`)
   - separator dot `·` in `text_dim`
   - title text `<shell> · <cwd>` in `text_secondary`
   - meta `<cols>x<rows>` right-aligned in `text_dim`

3. **App body**
   - inner padding 12 / 16 px
   - holds the terminal grid (cells from
     `output::extract_grid_themed`)
   - body bg fills with `surface_recessed` so the grid sits on its
     own surface
   - cursor is drawn by the grid pipeline; an extra glow quad is
     emitted behind it when tokens carry a glow color

## Out of scope

- Cursor blink driver wiring (the renderer already runs its own clock;
  this PR does not touch that path).
- PTY IO, alt-screen routing, takeover detection — left untouched.
- Per-glyph letter-spacing for the `TERMINAL` label — the renderer is
  monospace and we accept the standard cell advance.

## Dependencies

This PR is the third in a three-PR series.

- Sibling A — `phantom-ui::tokens` sync with `system.css`: adds
  `surface_floating`, `glow_color`, role colors, scanline color.
- Sibling B — `phantom-renderer` primitives:
  `draw_rounded_rect(rect, radius, color)`,
  `draw_glow(rect, color, radius)`,
  `draw_gradient_rect(rect, top, bottom)`.

Until A and B land, this PR uses local fallbacks:

- `surface_floating` -> `surface_raised`
- `glow_color` -> `text_accent` with reduced alpha
- rounded corners -> a single straight quad (the renderer cannot draw
  rounded rects until B lands)
- glow -> an oversize quad behind the cursor at low alpha

Once A and B are on `main`, a follow-up PR can replace the fallbacks
with real calls. Nothing in this PR's surface area changes when that
happens.
