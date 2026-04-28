# Phantom Visual Design System

**Status:** v0.1 — first articulation
**Owner:** Jeremy Miranda + Claude
**Scope:** User-facing surfaces only. Agent-facing structured data is a separate document.

## 1. North Star

Phantom is not "a terminal with a green palette." It is a **diegetic computing surface** — a piece of in-world hardware the user is operating. The reference is not Pinterest moodboards of phosphor green; it is **RobCo Pip-Boy 3000**, **Severance Lumon**, **Alien: Isolation MU/TH/UR**, and **NieR: Automata** menus: chunky framed panels, deliberate chrome, hierarchy carried by structure not just color.

The current build fails this on one axis: it has the *palette* (Pip-Boy phosphor green) but the *layout* of `println!()` debug output. Every surface is left-aligned monospace text on a near-black field, with 1px lines pretending to be borders.

The work below is to install the missing primitives — tokens, components, patterns — so a single global change (theme swap, density change, focus state) propagates correctly, and so each surface reads as a *piece of designed hardware* rather than stdout.

## 2. Principles

1. **Diegetic, not decorative.** Every chrome element pretends to be physical: bracketed corners, gauge frames, riveted seams, status pills. No element exists "just to look cool" — each carries a role.
2. **Structure before color.** Hierarchy comes from spacing, framing, and weight first; color is the last 10%. The UI must read on a desaturated screenshot.
3. **Density is a choice, not an accident.** Information-dense panels are fine — but density requires *more* structure, not less. Crowding without rhythm is a bug.
4. **No flat debug lines.** Strings like `terminal 202x43  40:32 on tty008` are prohibited as final UI. They become labeled fields inside framed components.
5. **Active state is loud, inactive state is quiet.** A focused pane is unmistakable from across the room. Unfocused panes recede, they don't disappear.
6. **One source of truth per token.** No hardcoded RGB or pixel padding outside `tokens.rs`. Themes mutate tokens; components consume them.
7. **The renderer is not a decorator.** If a component needs a primitive (corner glyphs, gauges, dividers) we add the primitive — we do not fake it with concatenated text.

## 3. Tokens (canonical)

These live in a new `phantom-ui/src/tokens.rs` and are consumed by every component. Theme files override the value table; nothing else does.

### 3.1 Spacing scale (px)

Based on a 4px base unit. All padding, margins, and gaps pick from this scale:

```
SPACE_0 =  0   SPACE_1 =  4   SPACE_2 =  8
SPACE_3 = 12   SPACE_4 = 16   SPACE_5 = 24
SPACE_6 = 32   SPACE_7 = 48   SPACE_8 = 64
```

Magic numbers like `10.0` for panel padding are deleted.

### 3.2 Type ramp

Currently a single monospace size. We introduce **logical sizes** that all map to the same font but different *cell scales* (or, when we add a UI font, different sizes):

```
type.body      = 1.00x  (terminal cells, primary)
type.label     = 0.85x  (status bar, tab labels, meta)
type.caption   = 0.75x  (timestamps, hints)
type.heading   = 1.15x  (panel headers — bolded via brighter color until we have weight)
```

Until we wire a second font, `type.heading` and `type.label` are achieved with **color/intensity contrast**, not size.

### 3.3 Color roles

Roles, not literals. Each theme defines the palette; components reference roles:

```
color.surface.base       — pane interior background
color.surface.recessed   — content area inside chrome
color.surface.raised     — title strip, hovered tab
color.chrome.frame       — panel borders, corner glyphs
color.chrome.frame_active   — focused pane border
color.chrome.frame_dim   — unfocused pane border
color.chrome.divider     — horizontal/vertical hairlines
color.text.primary       — body text
color.text.secondary     — labels, meta
color.text.dim           — captions, idle status
color.text.accent        — heading, active label
color.status.ok          — green success
color.status.warn        — amber
color.status.danger      — red
color.status.info        — cyan/blue
```

Phosphor theme today maps everything to ~3 actual values. After roles, themes can add subtle differentiation without losing the look.

### 3.4 Radius / line

```
radius.sm   = 2     (input bars, pills)
radius.md   = 4     (panels)
radius.lg   = 6     (outer container)
hair        = 1     (dividers, 1px lines)
frame       = 2     (panel borders — thicker than hair, deliberate)
```

## 4. Components

Each component is a function in `phantom-ui/src/components/` that emits `(quads, glyphs)` given a layout rect, theme, and state. No component reads global state; everything passes through props.

### 4.1 `Panel`

The most important new primitive. A framed container with a header strip. Replaces the current "1px border + overlay text title" approach.

```
+— [ TITLE ]————————————————————— meta ——+
|                                          |
|  content                                 |
|                                          |
+——————————————————————————————————————————+
```

- **Border:** `frame` thickness, color from `chrome.frame_active|frame_dim`
- **Corner glyphs:** ASCII brackets `[` `]` or unicode box-draw `┏ ┓ ┗ ┛` rendered in `chrome.frame`
- **Header strip:** `SPACE_3` tall, `surface.raised` background, with title left-aligned and meta (right-aligned, `text.dim`)
- **Title format:** ALL CAPS, `text.accent` if focused, `text.secondary` if not. Surrounded by ` [ ` and ` ] ` to make it diegetic.
- **Content area:** `SPACE_3` padding inset from frame.

### 4.2 `Header` (within panel)

The header is built from the panel; not a standalone component, but it has rules:
- Title: `[ TITLE ]` form, never bare text.
- Meta slots (right side): `key=value` pairs separated by ` · `. Examples: `pid=12345`, `cwd=~/code`, `pty=tty008`.
- No frame width labels (`202x43`) ever — they belong in a developer overlay, not the chrome.

### 4.3 `MessageBlock` (agent pane)

```
┌ user ─────────────────────────────────── 19:42 ┐
  fix the failing tests
└─────────────────────────────────────────────────┘

┌ phantom ───────────── claude-opus-4-7 · 6 tools ┐
  Looking at the test output, the issue is in
  src/foo.rs:42 — the assertion expects…
└─────────────────────────────────────────────────┘
```

- **Two roles:** `user` (warmer, `text.primary` on `surface.recessed`) and `agent` (cooler, `text.primary` on `surface.base`).
- **Each block has a mini-header strip:** role label + meta (timestamp for user, model+tool count for agent).
- **Vertical rhythm:** `SPACE_4` between blocks. No more concatenated wall of text.
- **Tool calls** appear as inline pills inside the agent block: ` ▸ read_file(…)`.

### 4.4 `InputBar`

Replaces the bare `>` prompt. A bordered, padded field with a left affordance, the prompt text, and a right affordance.

```
┌─ ❯ ─────────────────────────────────────── send ─┐
│   build the project                              │
└──────────────────────────────────────────────────┘
```

- **Active cursor:** block cursor, blinks at `text.accent`.
- **Left glyph:** `❯` for agent input, `$` for shell.
- **Right hint:** `[Enter]` or `[ ⏎ send ]`.

### 4.5 `StatusStrip` (bottom bar)

Replaces the three free-floating words.

```
┌── PROJECT ──────── BRANCH ────── PTY ──── TIME ──┐
│  badass-cli   │   main      │  tty008  │ 19:42 │
└──────────────────────────────────────────────────┘
```

- **Sectioned**, with vertical hairline dividers.
- Each section has a tiny ALL-CAPS label above the value (`type.caption`, `text.dim`), and the value below (`type.label`, `text.primary`).
- Right-most section has a small status dot (●) showing connection / agent state.

### 4.6 `TabStrip`

The current tab strip has the right *idea* (active vs inactive bg) but no chrome. Add:

- **Active tab:** dropped seam — the bottom border is missing where the tab meets the active panel, so the tab "merges" into the panel below. Diegetic.
- **Inactive tab:** full bottom border, dim text, `surface.recessed` bg.
- **Tab format:** `▸ shell` or `▸ agent · 2 turns`. The `▸` is the focus arrow on the active tab; on inactive it becomes `·`.
- **Close affordance:** `×` only on hover (post-mouse-input work).

### 4.7 `Divider`

A primitive: emits a thin quad of `hair` thickness in `color.chrome.divider`. Used inside panels to separate sections (e.g. message blocks, status sections).

## 5. Patterns

### 5.1 Pane focus

Focused pane:
- `frame_active` border (1.5–2px wider than dim)
- Title `text.accent`
- Subtle inner glow (matched to `chrome.frame_active`, very low alpha) — already supported by the bloom shader, just needs tuning.

Unfocused pane:
- `frame_dim` border, ~50% alpha
- Title `text.secondary`
- Slight surface darken (multiply by 0.92 on `surface.base`) — hard to see but communicates state.

### 5.2 Pane seams

Two adjacent panes share their inner border. Today they each draw a separate 1px line, which makes the seam look 2px and slightly off. Fix: **shared seam**, drawn once, in `chrome.divider` (not `chrome.frame`), so split panes feel like sections of one chassis, not separate boxes glued together.

### 5.3 Empty terminal

The current top pane shows just `% ` on a giant teal void. Replace with:
- **Idle state placeholder** (centered, `text.dim`):
  ```
  shell · tty008 · ~/code/badass-cli
  press any key to begin
  ```
- The `% ` prompt sits at the bottom-left as it always does, but the placeholder above keeps the panel from looking broken.

### 5.4 Diegetic chrome details

These small touches separate "Pip-Boy palette" from "actually feels like a Pip-Boy":

- **Riveted corners:** small `▪` dots at panel corners, just inside the frame.
- **Section markers:** between status strip sections, a `║` instead of plain `|`.
- **Power LED:** a single `●` somewhere persistent (top-right of outer chassis), pulsing gently — communicates "system is on."
- **Scanline density variation:** title strips slightly more scanline-heavy than content (already feasible via shader uniform per-region; future iteration).

## 6. Implementation order

We will implement and screenshot after each step.

1. **`tokens.rs` + theme refactor.** Centralize the values. Make existing UI consume them (no visible change, but unblocks everything else).
2. **`Panel` component.** Replace per-pane chrome (`render.rs:437–452`) with `Panel`. This alone changes ~60% of the UI.
3. **`StatusStrip`.** Replace `widgets/mod.rs:160–217` with sectioned strip.
4. **`TabStrip` v2.** Add diegetic active-tab seam.
5. **`MessageBlock` + `InputBar`.** Refactor agent pane interior (`render_overlay.rs:498–622`).
6. **Polish:** seams, riveted corners, idle-state placeholder, focus glow tuning.

After each step: `cargo build --release`, MCP screenshot, evaluate against this document. If a step's output doesn't *clearly* feel like an upgrade, we stop and fix before moving on. We do not stack iterations on a broken base.

## 7. What we are explicitly **not** doing yet

- A second font (UI sans). Stays monospace until tokens & components are in place.
- Animations / transitions. Static state first.
- Theme-switcher UI. Themes still hot-load via config; the *settings* surface is later work.
- Mouse hover states (no mouse input yet).
- Image / sprite chrome. Quads + text + corner glyphs only — no PNG decorations.

## 8. Acceptance criteria

The redesign is "done enough" to ship when, on a screenshot:

- A new viewer can identify the active pane in <1 second.
- Each pane reads as a discrete *piece of hardware*, not a div.
- The status strip and tab strip have visible internal structure (sections, not free words).
- An agent message and a user message are visually distinguishable from across the room.
- No string in the UI is a raw debug line (`202x43`, `tty008` shown bare). Every value lives inside a labeled field.
- A desaturated screenshot still reads correctly — hierarchy survives without color.

When all six pass, this document gets a v0.2 with whatever we learned, and the next pass (animations, second font, gauges) begins.
