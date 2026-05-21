# Terminal

[← back to components index](README.md)

> PTY emulation + semantic command understanding.

## Status

<span class="chip ok">shipping</span> · `phantom-terminal`
<span class="chip warn">stubbed</span> · `phantom-semantic`

## What it does

Wraps a real PTY in an `AppAdapter`. Speaks VT100 / xterm / Kitty
protocols via `alacritty_terminal`. The semantic layer (stubbed today)
parses output streams into structured commands + results so the brain
can reason about "the user just ran `git status`; here are the modified
files."

## Crates

### `phantom-terminal` <span class="chip ok">shipping</span>

PTY-backed terminal emulator.

- `PhantomTerminal` — owns the PTY file descriptor, the
  `alacritty_terminal::Term`, the output buffer, the alt-screen state.
- `TerminalAdapter` (lives in `phantom-app/src/adapters/terminal.rs`) —
  the AppAdapter wrapper around `PhantomTerminal`. Drains stdout, emits
  `TerminalOutput` bus events, maintains the cursor + scrollback +
  selection state.
- `output::*` — theme colors + cursor state.
- `takeover` — `TakeoverDetector` for subprocess takeover (vim, htop,
  less) so the renderer can switch to alt-screen layout.
- `input` — mouse mode tracking.
- `process` — `foreground_process_name(pty_fd)` for the takeover label.

### `phantom-semantic` <span class="chip warn">stubbed</span>

Parsers for common command outputs. Currently a skeleton — types defined
but classification + parser implementations pending. When complete:

- `Command` enum — git, cargo, npm, kubectl, etc.
- `ParsedOutput` — structured form of the stdout stream.
- Pub-sub: subscribes to `terminal.output`, publishes `command.started`,
  `command.complete`, `command.error` topics.

## Owns

- PTY file descriptors (one per terminal pane)
- Alacritty terminal state (cell grid, cursor, scrollback)
- Bracketed-paste timeout state
- Mouse mode + selection state

## Reads from

| Source | What |
|---|---|
| PTY child process stdout | bytes |
| User keyboard | keystrokes (dispatched from `InputHandler`) |

## Writes to / publishes

| Target | What |
|---|---|
| PTY child stdin | keystrokes |
| `terminal.output` bus topic | byte counts |
| `command.*` bus topics (semantic layer, when shipping) | structured commands |

## Decisions honoured

- [ADR-005 · Keystroke glitch FX](../decisions/005-keystroke-fx.md) —
  per-keystroke shader overlay reads cursor position from the terminal
  state.

## Open gaps

(none currently surfaced from the 4 anchor flows — phantom-semantic's
stub state is a known incomplete-implementation rather than a flow gap)

## Source files

| Concept | File |
|---|---|
| PhantomTerminal | [`crates/phantom-terminal/src/terminal.rs`](../../../../crates/phantom-terminal/src/terminal.rs) |
| TerminalAdapter | [`crates/phantom-app/src/adapters/terminal.rs`](../../../../crates/phantom-app/src/adapters/terminal.rs) |
| Takeover detector | [`crates/phantom-terminal/src/takeover.rs`](../../../../crates/phantom-terminal/src/takeover.rs) |
| Semantic skeleton | [`crates/phantom-semantic/src/lib.rs`](../../../../crates/phantom-semantic/src/lib.rs) |
