# Terminal adapter rewrite — PLAN

1. Add a `tokens: Tokens` field to `TerminalAdapter`, defaulting to
   `Tokens::phosphor(RenderCtx::fallback())`. Add `set_tokens` and a
   command-handler arm `set_theme_name` that mirrors `LogsAdapter`.
2. Add a small private `chrome` module inside `terminal.rs` that
   holds the rewritten `render_chrome` function. The function takes a
   `Rect`, the `Tokens`, header strings, and returns appended quads /
   text segments. Use `AppHead::render_into_adapter` for the header
   row so the header is consistent with every other pane.
3. Rewrite `Renderable::render`:
   a. Emit outer card background (`surface_floating` fallback).
   b. Emit a 1 px border quad ring (4 hairlines around the rect) in
      `chrome_frame_dim`.
   c. Delegate the head row to `AppHead`.
   d. Emit a body background quad in `surface_recessed`.
   e. Apply 12 / 16 px inset to compute the inner grid rect.
   f. Emit a `GridData` whose origin is the inner rect's top-left and
      whose `cells` come from `output::extract_grid_themed`.
   g. Emit a glow quad behind the cursor cell when the cursor is
      visible.
4. Local fallback layer: a `const FALLBACK_GLOW_ALPHA: f32` and a
   `fn surface_floating(t: &Tokens) -> [f32; 4]` that returns
   `t.colors.surface_raised`. When sibling PR A lands, replace those
   two helpers with direct token accesses.
5. Add a unit test that calls `render_chrome` against a fixed `Rect`
   and asserts:
   - the outer quad is present;
   - the border quads are present (4 of them);
   - text segments contain `"TERMINAL"` and the meta `<cols>x<rows>`.
6. Keep the diff under 1000 lines including tests. No changes outside
   `crates/phantom-app/src/adapters/terminal.rs` and the spec docs.

## Order of operations during render

The render function emits in back-to-front order so that the GPU layer
gets a correct paint order:

1. card bg quad
2. card border quad ring
3. AppHead quads / text
4. body bg quad
5. cursor glow quad (when visible)
6. grid data (renderer draws cells over the body bg)
