# Phase 2 Plan: Scrollbars, Mouse, Fullscreen, Settings

## Architecture Context

**Key files:**
- `crates/phantom/src/main.rs` — winit event loop, currently only handles KeyboardInput/Resized/ModifiersChanged
- `crates/phantom-app/src/app.rs` — App struct, owns all subsystems
- `crates/phantom-app/src/pane.rs` — Pane struct (no scroll state), geometry helpers
- `crates/phantom-app/src/render.rs` — 3-pass GPU render: scene → postfx → overlay
- `crates/phantom-app/src/input.rs` — keyboard handling, keybind dispatch
- `crates/phantom-terminal/src/terminal.rs` — PhantomTerminal wraps alacritty Term
- `crates/phantom-terminal/src/output.rs` — extract_grid_themed() extracts visible viewport
- `crates/phantom-terminal/src/input.rs` — encode_key() for PTY input encoding
- `crates/phantom-adapter/src/adapter.rs` — InputHandler trait (keyboard only)
- `crates/phantom-ui/src/keybinds.rs` — KeybindRegistry, Action enum

**Alacritty API available:**
- `term.scroll_display(Scroll::Delta(n))` — scroll by n lines (negative = up)
- `term.scroll_display(Scroll::PageUp)` / `Scroll::PageDown` / `Scroll::Top` / `Scroll::Bottom`
- `term.grid().display_offset()` — current scroll position (0 = bottom)
- `term.grid().history_size()` — total scrollback lines available
- `term.grid().screen_lines()` — visible viewport rows
- Grid indexing already respects display_offset — extract_grid_themed works after scroll_display

**Render architecture:**
- Scene pass: terminal grid + chrome quads rendered into PostFx texture (gets CRT)
- Overlay pass: container borders, titles, console, HUD rendered post-CRT (stays crisp)
- Scrollbar should be in overlay pass (crisp, not CRT-warped)

---

## Task Decomposition

### Wave 0: Terminal Scroll API (foundation, no deps)

#### T1: Expose scroll methods on PhantomTerminal
**File:** `crates/phantom-terminal/src/terminal.rs`
**What:** Add methods to PhantomTerminal:
- `scroll_up(&mut self, lines: usize)` → calls `self.term.scroll_display(Scroll::Delta(-(lines as i32)))`
- `scroll_down(&mut self, lines: usize)` → calls `self.term.scroll_display(Scroll::Delta(lines as i32))`
- `scroll_page_up(&mut self)` → calls `self.term.scroll_display(Scroll::PageUp)`
- `scroll_page_down(&mut self)` → calls `self.term.scroll_display(Scroll::PageDown)`
- `scroll_to_bottom(&mut self)` → calls `self.term.scroll_display(Scroll::Bottom)`
- `scroll_to_top(&mut self)` → calls `self.term.scroll_display(Scroll::Top)`
- `display_offset(&self) -> usize` → returns `self.term.grid().display_offset()`
- `history_size(&self) -> usize` → returns `self.term.grid().history_size()`
**Tests:** Unit tests that create a PhantomTerminal, write enough data to create scrollback, then verify scroll methods change display_offset.
**Blocked by:** nothing

#### T2: Add mouse state tracking to App
**File:** `crates/phantom-app/src/app.rs`
**What:** Add fields to App struct:
- `cursor_position: (f64, f64)` — current mouse position in window coords
- `mouse_buttons: u8` — bitfield for pressed buttons (left=1, right=2, middle=4)
- `cursor_over_pane: Option<usize>` — which pane the cursor is over (updated each CursorMoved)
Initialize in `with_config_scaled()`.
**Tests:** N/A (struct fields, tested through integration)
**Blocked by:** nothing

#### T3: Add scroll keybind actions to Action enum
**File:** `crates/phantom-ui/src/keybinds.rs`
**What:** Add variants to Action enum:
- `ScrollUp` — scroll focused pane up 3 lines
- `ScrollDown` — scroll focused pane down 3 lines  
- `ScrollPageUp` — scroll focused pane up one page
- `ScrollPageDown` — scroll focused pane down one page
- `ScrollToTop` — scroll to top of history
- `ScrollToBottom` — scroll to bottom
Register default keybinds: Shift+PageUp → ScrollPageUp, Shift+PageDown → ScrollPageDown, Shift+Home → ScrollToTop, Shift+End → ScrollToBottom
**Tests:** Test that keybind lookup resolves the new combos.
**Blocked by:** nothing

### Wave 1: Wire Scroll + Capture Mouse (deps: Wave 0)

#### T4: Wire scroll actions in input dispatch
**File:** `crates/phantom-app/src/input.rs`
**What:** Add cases for ScrollUp/Down/PageUp/PageDown/ToTop/ToBottom in `dispatch_action()`:
```
Action::ScrollPageUp => { if let Some(p) = self.panes.get_mut(self.focused_pane) { p.terminal.scroll_page_up(); } }
```
And similar for each variant. When scrolling up, also prevent the scroll keybinds from being forwarded to PTY.
**Tests:** N/A (integration tested through keybind → scroll → display_offset)
**Blocked by:** T1, T3

#### T5: Capture mouse events in main.rs
**File:** `crates/phantom/src/main.rs`
**What:** Add three new arms to `window_event()` match:
- `WindowEvent::CursorMoved { position, .. }` → call `app.handle_cursor_moved(position.x, position.y)`
- `WindowEvent::MouseInput { state, button, .. }` → call `app.handle_mouse_click(state, button)`
- `WindowEvent::MouseWheel { delta, .. }` → call `app.handle_mouse_scroll(delta)`
**Tests:** N/A (event forwarding, tested through integration)
**Blocked by:** T2

#### T6: Add mouse handler stubs on App
**Files:** `crates/phantom-app/src/mouse.rs` (new file), `crates/phantom-app/src/app.rs` (add mod)
**What:** Create new `mouse.rs` module with impl App methods:
- `handle_cursor_moved(&mut self, x: f64, y: f64)` — update self.cursor_position, compute cursor_over_pane by hit-testing against pane rects
- `handle_mouse_click(&mut self, state: winit::event::ElementState, button: winit::event::MouseButton)` — update mouse_buttons, if left click on pane → focus that pane
- `handle_mouse_scroll(&mut self, delta: winit::event::MouseScrollDelta)` — scroll focused pane (delta lines or pixels→lines conversion)
Hit-testing: iterate self.panes, get layout rect for each, check if (x,y) is inside pane_inner_rect. Also check scrollbar region.
**Tests:** Can test hit-testing logic with mock rects.
**Blocked by:** T1, T2, T5

### Wave 2: Scrollbar Rendering (deps: T1)

#### T7: Render scrollbar in overlay pass
**File:** `crates/phantom-app/src/render.rs`
**What:** After rendering each pane's grid, compute scrollbar geometry and add quads to `chrome_quads` (overlay pass, crisp):
- Track: thin vertical quad on right edge of pane inner rect (width: 6px, color: theme bg + slight tint, alpha 0.3)
- Thumb: proportional height = viewport_rows / (viewport_rows + history_size), position based on display_offset
- Only show scrollbar when history_size > 0 (there IS scrollback)
- Thumb color: theme foreground at 40% opacity, brighter when hovered
- Scrollbar drawn in chrome_quads (overlay pass) so it stays crisp through CRT
**Tests:** Test scrollbar geometry calculation (thumb height, position) as a pure function.
**Blocked by:** T1

#### T8: Auto-scroll-to-bottom on new PTY output
**File:** `crates/phantom-app/src/update.rs`
**What:** In the per-frame update loop where pty_read() is called, after reading new data: if pane.terminal.display_offset() > 0, call pane.terminal.scroll_to_bottom(). This ensures that when new output arrives while the user has scrolled up, it snaps back to the bottom (standard terminal behavior). Only do this if the pane is NOT in "scroll lock" mode (we can add a flag later if needed — for now, always snap).
**Tests:** N/A (behavior test: scroll up, produce output, verify offset returns to 0)
**Blocked by:** T1

### Wave 3: Mouse → PTY (SGR encoding)

#### T9: Add mouse SGR encoding to phantom-terminal
**File:** `crates/phantom-terminal/src/input.rs`
**What:** Add function `encode_mouse_sgr(button: u8, x: usize, y: usize, pressed: bool) -> Vec<u8>` that produces SGR 1006 mouse sequences: `\x1b[<{button};{x+1};{y+1}{M|m}` where M=press, m=release. Also add `encode_mouse_motion_sgr(button: u8, x: usize, y: usize) -> Vec<u8>` for motion events (button + 32).
**Tests:** Unit tests verifying correct SGR byte sequences for various button/position combos.
**Blocked by:** nothing

#### T10: Add mouse mode tracking to PhantomTerminal
**File:** `crates/phantom-terminal/src/terminal.rs`
**What:** Add method `mouse_mode(&self) -> MouseMode` that checks alacritty's terminal mode flags to determine if the running program has enabled mouse tracking. Check `term.mode()` for `TermMode::MOUSE_REPORT_CLICK`, `MOUSE_DRAG`, `MOUSE_MOTION`, `SGR_MOUSE`. Return an enum: `MouseMode::None`, `MouseMode::Click`, `MouseMode::Drag`, `MouseMode::Motion`.
**Tests:** Unit test creating terminal and checking default mode is None.
**Blocked by:** nothing

#### T11: Wire mouse clicks to PTY via SGR when mouse mode active
**File:** `crates/phantom-app/src/mouse.rs`
**What:** In `handle_mouse_click()`, after focusing the pane: if `pane.terminal.mouse_mode() != MouseMode::None`, compute (col, row) from click position relative to pane inner rect, encode SGR mouse event, write to PTY. Similarly for motion events in `handle_cursor_moved()` when in drag/motion mode.
**Tests:** Integration test: verify that click coordinates produce correct SGR bytes written to PTY.
**Blocked by:** T6, T9, T10

### Wave 4: Scrollbar Mouse Interaction

#### T12: Mouse click on scrollbar → jump scroll
**File:** `crates/phantom-app/src/mouse.rs`
**What:** In `handle_mouse_click()`, check if click is in scrollbar region (right 8px of pane rect). If so, compute scroll position from y coordinate: `target_offset = (1.0 - (click_y - track_top) / track_height) * history_size`. Call `pane.terminal.scroll_to_position(target_offset)` (needs a new method on PhantomTerminal that calls `scroll_display(Scroll::Delta(...))`).
**Tests:** Test position → offset calculation as pure function.
**Blocked by:** T6, T7

### Wave 5: Fullscreen Pane Toggle

#### T13: Add fullscreen state + keybind
**Files:** `crates/phantom-app/src/app.rs`, `crates/phantom-ui/src/keybinds.rs`, `crates/phantom-app/src/input.rs`
**What:** 
- Add `fullscreen_pane: Option<usize>` to App struct (None = normal, Some(idx) = that pane is fullscreen)
- Add `ToggleFullscreen` variant to Action enum
- Register keybind: Ctrl+Shift+F → ToggleFullscreen (also F11)
- In dispatch_action: toggle fullscreen_pane between None and Some(focused_pane)
- Escape should exit fullscreen (add check in handle_key_with_mods before other handling)
**Tests:** Test that toggling sets/clears the field.
**Blocked by:** nothing

#### T14: Render fullscreen pane
**File:** `crates/phantom-app/src/render.rs`
**What:** In `render_terminal()`, if `self.fullscreen_pane.is_some()`:
- Only render the fullscreen pane (skip all others)
- Use full screen_size for the pane rect (minus status bar)
- Skip tab bar, skip panels
- Temporarily resize the terminal to match the fullscreen dimensions
- Draw a small "ESC to exit" hint in the corner
**Tests:** N/A (visual, needs manual testing)
**Blocked by:** T13

### Wave 6: Settings System

#### T15: Settings data model + TOML file
**File:** `crates/phantom-app/src/settings.rs` (new)
**What:** Define `PhantomSettings` struct with sections:
- `theme: String`
- `font_size: f32`
- `keybinds: HashMap<String, String>` (combo → action)
- `crt: CrtSettings` (all shader params)
- `scroll: ScrollSettings` (history lines, scroll amount)
Load from `~/.config/phantom/settings.toml`, merge with PhantomConfig. Save on change.
**Tests:** Round-trip test: create settings, save to TOML, reload, verify equal.
**Blocked by:** nothing

#### T16: Settings overlay UI
**Files:** `crates/phantom-app/src/settings_ui.rs` (new), wire into render.rs + input.rs
**What:** Render a settings panel overlay (similar to debug HUD but more structured):
- Toggle with keybind (Ctrl+,)
- Sections: Theme, Font, CRT Effects, Keybinds
- Arrow keys to navigate, Enter to edit, Escape to close
- Changes apply live (like debug HUD) and auto-save
**Tests:** N/A (visual UI, manual testing)
**Blocked by:** T15

---

## Dependency Graph

```
T1 (scroll API) ──┬── T4 (wire keybinds) 
                   ├── T7 (scrollbar render) ── T12 (scrollbar click)
                   ├── T8 (auto-scroll-bottom)
                   └── T6 (mouse handlers) ──┬── T11 (mouse→PTY)
                                              └── T12 (scrollbar click)
T2 (mouse state) ──── T5 (capture events) ── T6 (mouse handlers)
T3 (keybind actions) ── T4 (wire keybinds)
T9 (SGR encoding) ──── T11 (mouse→PTY)
T10 (mouse mode) ──── T11 (mouse→PTY)
T13 (fullscreen state) ── T14 (fullscreen render)
T15 (settings model) ── T16 (settings UI)
```

## Parallel Execution Strategy

**Batch A (immediate, no deps):** T1, T2, T3, T9, T10, T13, T15 — all 7 can run in parallel
**Batch B (after Batch A):** T4, T5, T6, T7, T8, T14, T16 — run after their deps complete
**Batch C (after Batch B):** T11, T12 — final wiring

## Risk Assessment

1. **alacritty Scroll import** — need to verify `use alacritty_terminal::grid::Scroll` compiles
2. **Mouse mode flags** — need to verify alacritty exposes TermMode bits publicly
3. **Overlay pass ordering** — scrollbar quads must render AFTER CRT pass but BEFORE console overlay
4. **Fullscreen resize** — temporary resize during fullscreen must not corrupt the PTY's size state on exit
5. **Settings file conflicts** — settings.toml vs config.toml vs PhantomConfig — need clear precedence
