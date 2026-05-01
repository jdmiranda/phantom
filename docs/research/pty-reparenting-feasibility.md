# PTY Reparenting Feasibility — Research Spike

**Issue**: #370  
**Date**: 2026-04-30  
**Author**: research agent  
**Status**: DEFER (with narrow GO path for Linux-only v2)

---

## 1. What "True" Detach Means

The alt-screen split shipped in #346 is *visual* detach: Phantom detects that a
subprocess entered alt-screen mode and renders a snapshot of it in a secondary
pane. The child process is still running inside the same PTY it was born in. If
Phantom (the GPU process) is killed, the child dies too because the PTY master
closes, delivering SIGHUP to the foreground process group.

True PTY reparenting means:

1. **The subprocess survives Phantom's death** — it keeps running after the
   parent terminal closes.
2. **Phantom (or a new instance) can re-attach to it later** — the PTY master
   can be handed to a different process and re-connected to the terminal
   emulator state machine.
3. **Job control still works inside the session** — the subprocess sees a
   controlling terminal, can call `tcgetpgrp`/`tcsetpgrp`, and signals like
   `SIGTSTP` route correctly.

This is exactly what `tmux` and `screen` do: they are a PTY broker that owns
the master end and outlives the client that opened it.

---

## 2. Mechanisms Survey

### 2.1 forkpty + setsid (POSIX baseline)

`forkpty(3)` allocates a PTY pair, forks, and makes the slave the child's
controlling terminal. If the parent calls `setsid(2)` *in the child before
exec*, the child becomes a session leader with no controlling terminal —
and then `open(slave_fd)` gives it one. This is the standard shell startup
sequence.

To survive parent death the child must:

- Call `setsid()` to start a new session.
- Hold the slave fd open (so SIGHUP from master close doesn't arrive).
- **Not** be in the foreground process group of the original session.

The key invariant: SIGHUP fires when the *session leader* exits **or** when the
last process holding the master fd closes it. If we can keep the master alive in
a different process, SIGHUP never fires.

Relevant syscalls: `posix_openpt`, `grantpt`, `unlockpt`, `open(pts_name)`,
`setsid`, `ioctl(TIOCSCTTY)`, `ioctl(TIOCNOTTY)`.

### 2.2 Screen/tmux PTY Layer Model

Both `tmux` and GNU `screen` implement a session daemon:

1. A long-lived **server process** calls `forkpty` or `posix_openpt` and holds
   the master fd for the lifetime of the session.
2. A short-lived **client process** connects to the server over a Unix socket
   and streams terminal input/output.
3. When the client disconnects (user closes the window), the server keeps the
   master fd open. The subprocess (e.g. `vim`) never sees SIGHUP.
4. A new client can attach later. The server sends the server-side screen buffer
   snapshot and resumes I/O.

This is the architecture Phantom would need to replicate for true detach.

### 2.3 Linux pidfd_send_signal + New-PTY Patterns

Linux 5.3 introduced `pidfd_open(2)` and `pidfd_send_signal(2)`. These give
race-free process identity by file descriptor, which helps when trying to send
signals to a re-parented process — you're not racing against PID reuse. However,
`pidfd` does **not** help with PTY transfer. The PTY master fd is just a file
descriptor; it must be transferred via `SCM_RIGHTS` over a Unix domain socket
(standard POSIX ancillary data).

Linux also has `TIOCSPGRP` (set foreground process group) and
`TIOCGPGRP` (already used in `phantom-terminal/src/process.rs:19`) which are
needed after re-attach to restore job control. Linux 5.6+ `pidfd_getfd` can
duplicate a file descriptor out of another process, which theoretically enables
"stealing" a PTY master from a running process — but requires `CAP_SYS_PTRACE`
or a ptrace relationship. Not a general-user mechanism.

### 2.4 macOS Constraints

macOS diverges from Linux in ways that matter:

- **No pidfd, no pidfd_getfd.** File descriptor transfer requires the two ends
  to be related (parent/child at fork time) or both cooperating via a socket.
- **System Integrity Protection (SIP).** Injecting into running processes (for
  PTY theft via Mach task port) requires entitlements Phantom won't have in
  production.
- **`TIOCNOTTY`** (drop controlling terminal) and `TIOCSCTTY` (acquire one) are
  available on macOS, but only within the same process/session.
- **`posix_openpt` + `grantpt` + `unlockpt`** work the same as Linux for
  creating PTY pairs.
- **Controlling terminal**: on macOS, `TIOCSCTTY` requires the calling process
  to have no controlling terminal and be a session leader — same as Linux, and
  it works — but **transferring the master fd to an unrelated process** still
  requires `SCM_RIGHTS`, which requires a socket between the two processes at
  the time of transfer.

The bottom line on macOS: the mechanism is architecturally possible but requires
the infrastructure to be in place *before* the subprocess is spawned. You cannot
retrofit an already-running subprocess.

### 2.5 Windows ConPTY

Windows 10 1809+ exposes `CreatePseudoConsole` / `ResizePseudoConsole` /
`ClosePseudoConsole` (the ConPTY API). There is no PTY transfer mechanism
analogous to SCM_RIGHTS. A ConPTY handle is tightly bound to the process that
created it. Re-attach would require re-creating the ConPTY and re-connecting the
subprocess via a new inherited handle at spawn time — only possible if the
subprocess was designed for it. There is no runtime PTY reparenting on Windows.

**Windows verdict for this spike: NOT FEASIBLE without redesigning subprocess
spawning entirely and requiring subprocess cooperation.**

---

## 3. Cross-Platform Feasibility Summary

| Platform | Mechanism available | Retrofit existing processes | Requires new infra |
|---|---|---|---|
| Linux | `SCM_RIGHTS` fd pass + `setsid` + `TIOCSCTTY` | No | Session broker daemon |
| macOS | `SCM_RIGHTS` + `setsid` + `TIOCSCTTY` | No | Same as Linux |
| Windows | None (ConPTY API) | No | ConPTY ownership redesign |

"Retrofit" means: take a subprocess that was already spawned under a normal PTY
and transfer it without its cooperation. This is not possible on any platform
without OS-level ptrace/debug privileges.

---

## 4. Rust Crate Landscape

### `portable-pty` (wezterm crate, v0.8)

The most complete cross-platform PTY abstraction in Rust. Provides:
- `PtySystem` trait with Unix and Windows backends.
- `openpty`, `spawn_command` with proper slave setup.
- Does **not** provide PTY transfer/reparenting — it's a creation API.

### `nix` (v0.29)

Bindings to nearly all relevant POSIX APIs:
- `nix::pty::{posix_openpt, grantpt, unlockpt, ptsname, openpty}` — PTY creation.
- `nix::unistd::{setsid, dup, dup2, close}` — session manipulation.
- `nix::sys::socket::{sendmsg, recvmsg, ControlMessage::ScmRights}` — fd passing.
- `nix::sys::ioctl` macros — `TIOCNOTTY`, `TIOCSCTTY`, `TIOCGPGRP`, `TIOCSPGRP`.

This is the right crate for a Unix implementation of a PTY broker.

### `libc` (already in phantom-terminal)

Lower-level than `nix`, already available. `libc::forkpty`, `libc::setsid`,
`libc::ioctl` are all usable. The code in `phantom-terminal/src/process.rs` already
uses `libc::ioctl` with `TIOCGPGRP` — the raw syscall layer is already present.

### `alacritty_terminal::tty` (current dependency)

The `tty::new` call in `phantom-terminal/src/terminal.rs:191` delegates PTY
creation to alacritty's implementation. Internally `tty::new` (Unix backend at
`alacritty_terminal-0.26.0/src/tty/unix.rs:195`) calls `openpty` to allocate the
PTY pair and then immediately delegates to a public sibling, `tty::from_fd`:

```rust
pub fn new(config: &Options, window_size: WindowSize, window_id: u64) -> Result<Pty> {
    let pty = openpty(None, Some(&window_size.to_winsize()))?;
    let (master, slave) = (pty.controller, pty.user);
    from_fd(config, window_id, master, slave)
}

pub fn from_fd(
    config: &Options,
    window_id: u64,
    master: OwnedFd,
    slave: OwnedFd,
) -> Result<Pty> { /* ... fork + execvp + setsid + TIOCSCTTY ... */ }
```

This means the public surface is *less opaque* than originally framed:

- **Master fd is reachable.** `tty::Pty` exposes `pub fn file(&self) -> &File`
  (`unix.rs:114`). `phantom-terminal/src/terminal.rs:196` already calls
  `pty.file().try_clone()`. Combined with `AsRawFd`/`AsFd` on `&File`, the master
  fd is trivially available for `SCM_RIGHTS` transfer with no fork of upstream.
- **PTY pair can be supplied externally.** `tty::from_fd(config, window_id,
  master: OwnedFd, slave: OwnedFd) -> Result<Pty>` is `pub`. A PTY broker can
  call `nix::pty::openpty` (or `posix_openpt` + `grantpt` + `unlockpt` +
  `open(pts_name)`) to allocate the pair externally, then hand the `OwnedFd`s to
  `tty::from_fd`. The fork/exec, `setsid`, and `TIOCSCTTY` plumbing inside
  alacritty stay intact — Phantom is not on the hook for re-implementing them.

The remaining Pty internals (`child: Child`, `signals: UnixStream`, `sig_id:
SigId`) are private and recreated per `from_fd` call. That is fine for the
broker model: on re-attach the broker passes the existing master fd back, and
the new `Pty` is *not* the goal — the goal is for `phantom-terminal` to read /
write the master fd directly. Reconstructing a full `tty::Pty` is only required
on the spawn path, which already takes `from_fd`.

---

## 5. Risks: Signal Delivery and Job Control

### SIGHUP

SIGHUP is delivered to the foreground process group when the *session leader*
dies **or** when the last holder of the master fd calls `close`. If Phantom's
`PhantomTerminal` drops the `_pty` field (which holds the master fd), every
foreground process in that PTY session receives SIGHUP. Default disposition:
terminal death. Most shells exit. `vim` and other programs may save or crash.

To prevent this, the master fd must be kept alive in a process that outlives
Phantom. **The supervisor process is the natural candidate** — it already
outlives the phantom app process.

### Foreground Process Group (TIOCGPGRP / TIOCSPGRP)

After re-attach, the new PTY master holder must restore the foreground process
group. If the subprocess did a `setpgid(0, 0)` (making itself a new process
group), the re-attaching process must call `ioctl(master_fd, TIOCSPGRP, &pgid)`
to restore job control so `SIGINT` (`Ctrl-C`) routes correctly. Getting this
wrong causes silent signal loss.

### Signal Mask Inheritance

Signals blocked in the phantom process before `forkpty` are inherited by the
child. If phantom blocks `SIGINT` for UI purposes, the child inherits the mask.
The current code in `phantom-supervisor/src/main.rs:574–598` blocks SIGINT and
SIGTERM for signal-waiter thread routing. If phantom inherits a similar mask and
forks a PTY child, that child may have a broken signal mask. **Audit required.**

### Shell Expectations for Controlling Terminal

Shells (zsh, bash, fish) call `tcgetattr` and assume they have a controlling
terminal via `/dev/tty`. If a re-attached session's slave is closed and reopened
in a new session, the shell's internal state may be inconsistent. tmux works
around this by never detaching the slave — it holds the slave fd open in the
server process even when no client is attached, so the shell never observes
terminal loss.

### `TIOCNOTTY` Hazards

Calling `ioctl(slave_fd, TIOCNOTTY)` drops the controlling terminal from the
calling process. If done incorrectly in the context of a broker handoff, the
subprocess loses its terminal unexpectedly. This must be called in the *old
owner* before transfer, not in the subprocess.

---

## 6. Phantom-Specific Interactions

### 6.1 Supervisor Process Boundary

phantom-supervisor is already the right architectural location for a PTY broker.
It outlives the phantom app process, has a Unix socket to phantom, and already
has the orphan PID file mechanism (`phantom-supervisor/src/orphan.rs`) for
tracking child PIDs across restarts.

**If** a PTY broker were built into phantom-supervisor:
1. Supervisor opens the PTY pair directly (instead of phantom-terminal doing it).
2. Supervisor passes the slave fd to phantom via `SCM_RIGHTS` at spawn time.
3. Phantom uses the slave to run the shell.
4. When phantom crashes, supervisor retains the master fd — no SIGHUP to the
   child.
5. On restart, supervisor passes the master fd back to the new phantom instance.

This requires phantom-protocol to gain a new message type for fd passing (Unix
sockets support ancillary data). The current protocol is line-based text
(`phantom-protocol` crate) — it would need a binary ancillary-data path.

### 6.2 Panic Recovery Model

Phantom already survives GPU panics via the supervisor restart loop
(`supervisor/src/main.rs:192–219`, `restart_phantom`). Today, when phantom
restarts, the user loses their shell session. With a supervisor-held PTY, the
shell session would survive the restart. This aligns with the panic recovery
model's intent and would be a genuine improvement to the restart story.

### 6.3 Alt-Screen Rendering Already in Main

The alt-screen split in #346 (`phantom-app/src/adapters/terminal.rs:52–264`) is
a *rendering* concept: when `is_detached` flips true, the app creates a sibling
pane with an `AltScreenViewAdapter` snapshot. The PTY continues to run in the
original `TerminalAdapter`. **True PTY reparenting is orthogonal to this** —
you could ship true detach without touching the alt-screen rendering path, or
combine them. The rendering layer doesn't need to change until the PTY
reparenting infrastructure is working and can deliver a PTY fd to a new adapter.

### 6.4 alacritty_terminal Integration Surface

An earlier draft of this spike framed `alacritty_terminal::tty::Pty` as
opaque — claiming the master fd could not be reached and that a broker would
require either forking alacritty or bypassing it entirely with a raw `nix` +
`fork` + `execvp` sequence. That framing is wrong against the actual public API
of `alacritty_terminal 0.26.0`.

The relevant public surface is, on Unix
(`alacritty_terminal-0.26.0/src/tty/unix.rs`):

| Item | Visibility | Use in broker model |
|---|---|---|
| `tty::Pty` | `pub struct` | Returned by both `new` and `from_fd` |
| `tty::Pty::file(&self) -> &File` | `pub fn` (line 114) | Already called from `phantom-terminal/src/terminal.rs:196`; `as_raw_fd()` is one method away |
| `tty::new(config, window_size, window_id) -> Result<Pty>` | `pub fn` (line 195) | Today's spawn path; allocates the PTY pair internally |
| `tty::from_fd(config, window_id, master: OwnedFd, slave: OwnedFd) -> Result<Pty>` | `pub fn` (line 202) | **The minimal broker integration point**: caller supplies the pair |

`tty::new` is implemented as `openpty(...)` followed by `from_fd(...)` — there
is no behaviour in `new` that is not also reachable via `from_fd`. This makes
the broker integration a one-line swap rather than a rewrite:

```rust
// Today (phantom-terminal/src/terminal.rs:191):
let pty = tty::new(&pty_options, size.window_size(), 0)?;

// Phase 2 broker re-attach path:
let (master, slave) = supervisor_handoff.take_pty_pair()?; // OwnedFd, OwnedFd
let pty = tty::from_fd(&pty_options, 0, master, slave)?;
```

This means:
- **No fork of alacritty_terminal** is required.
- **No raw `nix::pty::openpty` + `fork` + `execvp` bypass** is required inside
  `phantom-terminal`. The broker calls `openpty` (in the supervisor); the
  `phantom-terminal` side keeps using alacritty for `setsid` / `TIOCSCTTY` /
  child spawn.
- **Existing `pty.file()` plumbing is reusable** for the SCM_RIGHTS send path
  on detach: `pty.file().as_fd()` (or `.try_clone()` for an `OwnedFd`) is
  already what the supervisor needs in order to receive the master back when
  Phantom shuts down cleanly.

The non-trivial work moves out of `phantom-terminal` and into the supervisor:
opening the PTY pair, holding the master across phantom restarts, and serving it
back over `SCM_RIGHTS`. Inside `phantom-terminal`, the change is mechanical —
swap `tty::new` for `tty::from_fd` on the re-attach branch and add an
`OwnedFd` parameter to `PhantomTerminal::new`.

---

## 7. Recommendation: DEFER (with a narrow GO path)

### Why DEFER

1. **No retrofit possible.** Already-running PTY sessions (those born under
   today's `tty::new` path with no broker present) cannot be transferred to a
   supervisor-held master after the fact — fd transfer requires a cooperating
   process at the time of allocation. New sessions can be brokered cleanly via
   `tty::from_fd` (see §6.4), but every existing session must still be born
   through the supervisor's broker. The transition cost is real even though the
   per-call code change in `phantom-terminal` is small.

2. **Platform asymmetry.** A Linux-only implementation is feasible via
   `SCM_RIGHTS`. macOS is feasible but requires the same infrastructure.
   Windows is not feasible without a fundamentally different ConPTY ownership
   model. Phantom targets all three.

3. **Scope exceeds the current phase.** Phase 4 (this wave) is
   hardening the alt-screen visual detach path (#364–369). Implementing a PTY
   broker — which touches phantom-supervisor, phantom-protocol, phantom-terminal,
   and phantom-app — is a multi-sprint system project that would be premature
   before the visual layer is stable.

4. **The alt-screen approach delivers ~80% of the user value.** For the common
   case (user runs `vim` or `htop`, it visually floats into its own pane), the
   current implementation is already shipping. True PTY reparenting is only
   needed for the edge cases (Phantom crash while vim is running, or the user
   genuinely wants to detach a shell session, not just an alt-screen program).

### The Narrow GO Path (Linux + macOS, v2)

If the project decides to invest, the approach is viable on Unix systems with
this sequencing:

**Phase 1 — PTY broker in supervisor (new issue)**
- Add `nix` as a dependency to `phantom-supervisor`.
- Supervisor gains a `PtyBroker` module: calls `nix::pty::openpty` before
  spawning phantom, stores master fds keyed by session ID.
- Add ancillary-data (`SCM_RIGHTS`) support to `phantom-protocol`'s Unix socket
  transport so phantom can request and receive master fds on restart.

**Phase 2 — PhantomTerminal switch from `tty::new` to `tty::from_fd` (new issue)**
- In `phantom-terminal/src/terminal.rs:181–224`, change `PhantomTerminal::new`
  to accept an optional pre-opened PTY pair from the supervisor:
  ```rust
  pub fn new(
      cols: u16,
      rows: u16,
      handoff: Option<PtyHandoff>, // (master: OwnedFd, slave: OwnedFd) from broker
  ) -> Result<Self> { ... }
  ```
- Spawn path branches on `handoff`:
  - `Some(handoff)` → `alacritty_terminal::tty::from_fd(&pty_options, 0,
    handoff.master, handoff.slave)` — broker-allocated pair, child still spawned
    by alacritty with its existing `setsid` / `TIOCSCTTY` plumbing.
  - `None` → `alacritty_terminal::tty::new(...)` (today's path) so `phantom`
    can still run standalone without the supervisor.
- No fork of `alacritty_terminal`, no raw `nix` + `fork` + `execvp` bypass.
  alacritty's `pre_exec` hook (`unix.rs:248–276`) already handles `setsid`,
  `set_controlling_terminal(slave_fd)`, master/slave fd close in child, and
  signal-handler reset to defaults.
- On clean shutdown, send the master fd back to the supervisor via SCM_RIGHTS
  using `pty.file().try_clone()` (already used at line 196 for the read/write
  handles). The remaining `tty::Pty` (containing `child` + `signals` + `sig_id`)
  is dropped — its `Drop` impl sends `SIGHUP` to the child (`unix.rs:309–321`),
  so on intentional handoff the broker must be the one holding the live master
  before drop, otherwise the child is killed. Sequencing: broker receives master
  → phantom-terminal then drops `Pty`. Order matters; this is the single most
  delicate step in Phase 2 and warrants a focused test.
- Signal mask audit: alacritty's `pre_exec` resets `SIGCHLD`/`SIGHUP`/`SIGINT`/
  `SIGQUIT`/`SIGTERM`/`SIGALRM` dispositions but does not reset the *signal
  mask* inherited from phantom. If phantom blocks signals (see
  `phantom-supervisor/src/main.rs:574–598` for precedent), add a
  `pthread_sigmask(SIG_SETMASK, empty_set, NULL)` call inside a phantom-side
  `unsafe { builder.pre_exec(...) }` registered *before* alacritty's own
  `pre_exec` registration (or upstream a PR adding mask reset to alacritty).

**Phase 3 — App integration and re-attach protocol (new issue)**
- `phantom-app/src/adapters/terminal.rs` gets a `reattach(master_fd: RawFd)`
  method.
- On supervisor-triggered restart, the app receives the session list and master
  fds from the supervisor via the existing socket, creating `TerminalAdapter`
  instances for each surviving session.
- Re-attach calls `ioctl(master_fd, TIOCSPGRP, &pgid)` to restore foreground
  process group.

**Phase 4 — User-facing detach command (new issue)**
- Expose a keybind or command-mode command (e.g. `` ` detach ``) that
  disconnects the current session from its pane without killing it.
- The session stays in the supervisor's broker table; the pane closes.
- A new `` ` attach <session-id> `` command re-connects it.

**Windows**: Defer indefinitely. ConPTY reparenting requires subprocess
cooperation from birth; a Windows-specific design would be a separate spike.

---

## 8. Code-Level Pointers for a Future Implementer

| Location | Relevant to |
|---|---|
| `crates/phantom-terminal/src/terminal.rs:181–224` (`PhantomTerminal::new`) | Add optional `handoff: Option<PtyHandoff>`; branch to `alacritty_terminal::tty::from_fd` when present, retain `tty::new` for standalone path |
| `crates/phantom-terminal/src/terminal.rs:155–173` (`PhantomTerminal` struct fields) | Add `session_id: Uuid` field; master fd stays inside `_pty: tty::Pty` (reachable via `pty.file()`) |
| `crates/phantom-terminal/src/process.rs:13–25` (`foreground_process_name`) | Already uses `TIOCGPGRP` — extend for `TIOCSPGRP` on re-attach |
| `crates/phantom-supervisor/src/main.rs:129–148` (`spawn_phantom`) | Open PTY pair via `nix::pty::openpty` before spawn; pass `(master, slave)` via `SCM_RIGHTS` |
| `crates/phantom-supervisor/src/orphan.rs:43–51` (`PidFileData`) | Extend to track session IDs alongside child PIDs |
| `crates/phantom-protocol/src/lib.rs` | Add binary ancillary-data message type for fd passing |
| `crates/phantom-app/src/adapters/terminal.rs:79–101` (`TerminalAdapter::new`) | Add re-attach constructor path that threads `PtyHandoff` to `PhantomTerminal::new` |

Key syscalls in order of use during a broker handoff:
1. `nix::pty::openpty` — create PTY pair in **broker (supervisor)**.
2. `sendmsg` with `ControlMessage::ScmRights(&[master, slave])` — pass pair to phantom.
3. `alacritty_terminal::tty::from_fd(config, 0, master, slave)` — phantom hands the pair to alacritty, which runs its existing `setsid` + `TIOCSCTTY` + `execvp` `pre_exec` block on the child.
4. On detach: phantom sends `pty.file().try_clone()?` back to broker via `SCM_RIGHTS`, then drops `tty::Pty` *after* broker confirms receipt (otherwise `Pty::drop` SIGHUPs the child).
5. `ioctl(master_fd, TIOCSPGRP, &pgid)` — restore foreground process group on re-attach.

---

## 9. Parent Issue #2 Narrowing

Based on this spike, the acceptance criteria for `#2` should be updated:

- "PTY reparenting is either proven viable with a concrete follow-up plan or
  rejected" → **DEFER with a Unix GO path**: viable on macOS and Linux if
  phantom-supervisor becomes a PTY broker. Windows requires a separate design.
  Concrete follow-up issues: four sequenced issues as described in §7 above.
- The current alt-screen visual detach path covers the high-frequency user
  journey (UJ-004 "interactive subprocess floats into its own pane"). True detach
  ("subprocess survives Phantom restart") is a separate, lower-frequency need
  that can be deferred past Phase 4 hardening.
