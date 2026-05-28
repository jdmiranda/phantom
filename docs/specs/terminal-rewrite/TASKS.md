# Terminal adapter rewrite — TASKS

- [x] Read mockup and tokens.
- [ ] Add `tokens: Tokens` field + `set_tokens` + `set_theme_name`
      command arm on `TerminalAdapter`.
- [ ] Add `render_chrome` private helper that emits card bg, 1 px
      border ring, head row, and body bg.
- [ ] Replace `Renderable::render` body so the order is: card bg ->
      border -> head -> body bg -> cursor glow -> grid.
- [ ] Add fallback helpers `surface_floating` and
      `glow_color_for` so the file compiles against current main.
- [ ] Add a unit test `render_chrome_emits_card_head_and_body` that
      builds a fixed-rect chrome render and asserts the quads / text
      shape.
- [ ] `cargo build -p phantom-app` clean.
- [ ] Open draft PR with Summary / Test plan / Dependencies sections.
