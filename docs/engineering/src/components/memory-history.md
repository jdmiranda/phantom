# Memory + History

[← back to components index](README.md)

> What Phantom remembers + records + restores.

## Status

<span class="chip warn">stubbed</span> for most crates · the persistence
plumbing ships but the high-level read/write APIs aren't fully wired
into the brain yet.

## What it does

Five crates collectively manage three concerns:

1. **Memory** — per-project knowledge that persists across sessions
   ("this project uses pnpm", "port 3001", "the auth module is being
   refactored").
2. **History** — what happened: command history, agent message rows,
   tool call sidecar.
3. **Sessions** — save / restore of agent + goal state across launches.

## Crates

### `phantom-memory` <span class="chip warn">stubbed</span>

Per-project knowledge store. Event log + memory blocks. Schema +
read/write API pending (issues #28, #33, #62, #78).

- `MemoryStore::open(project_dir)` — per-project store.
- `NotificationStore::open(project_dir)` — persistent notification feed
  (denials, suggestions).
- Today: stores notifications. Memory blocks (the "what I remember about
  this project" concept) are still skeletal.

### `phantom-dag` <span class="chip warn">stubbed</span>

Code dependency graph. `.planning/dag.json` schema for agent navigation.

- `DagNode` — code symbol (Function / Struct / Trait / Module / Test).
- `DagEdge` — REFERENCES / DEPENDS_ON / etc.
- Used by the Cartographer agent role for "find me code related to X."
- DAG extraction pipeline is pending; the types ship.

### `phantom-recall` <span class="chip warn">stubbed</span>

Intent-anchored retrieval API. Query rewriting, score fusion, ANN
routing — types defined, backend wiring pending (issue #72).

- `RecallQuery` — typed retrieval request.
- `RecallResult` — scored hits.
- Will fuse memory + history + DAG + embeddings.

### `phantom-history` <span class="chip warn">stubbed</span>

Structured JSONL command history store. Read/write + agent output
capture pending (issue #75).

- `HistoryStore::open(session_uuid)` — opens
  `~/.local/share/phantom/history/<uuid>.jsonl`.
- `AgentCapture` — sidecar at
  `~/.local/share/phantom/history/<uuid>-agents.jsonl` capturing tool
  calls + outputs per agent.
- Write API ships (the sidecar IS populated each session); read /
  search API stubbed.

### `phantom-session` <span class="chip warn">stubbed</span>

Session save / restore. Agent + goal state restore pending (issues #76,
#77).

- `SessionManager` — discovers the latest session JSON per project at
  `~/.local/share/phantom/sessions/`.
- `AgentStatePersister` — sidecar for `AgentSnapshot` rows.
- `GoalStatePersister` — sidecar for `GoalSnapshot` rows.
- `SessionRestorer` — combines the two, returns `RestoredSession`.
- `welcome_message(session)` — the "Resume previous session?" prompt
  source. Restore prompt itself is wired in `App::update` (Flow 1's
  Step 5-ish branch).

## Owns

- Per-project memory store
- Per-project notification store
- Per-session history JSONL
- Per-session agent capture sidecar
- Per-session agent + goal snapshot sidecars
- Code DAG schema

## Reads from

| Source | What |
|---|---|
| Filesystem (`~/.local/share/phantom/`, `~/.config/phantom/`) | persisted state |
| Bus topics | agent lifecycle events, terminal output, notifications |

## Writes to / publishes

| Target | What |
|---|---|
| Filesystem | session sidecars, history JSONL, agent capture |
| Inspector | notification banner data, history list |
| Brain | restored session signal on cold launch |

## Decisions honoured

- [ADR-001 · Architecture decisions](../decisions/001-architecture.md) —
  the per-project memory + session restore is part of the "Phantom
  remembers" key idea.

## Open gaps

(none currently surfaced from the 4 anchor flows — the persistence
plumbing isn't yet exercised by a flow page)

## Source files

| Concept | File |
|---|---|
| MemoryStore | [`crates/phantom-memory/src/lib.rs`](../../../../crates/phantom-memory/src/lib.rs) |
| NotificationStore | [`crates/phantom-memory/src/notifications.rs`](../../../../crates/phantom-memory/src/notifications.rs) |
| Code DAG | [`crates/phantom-dag/src/lib.rs`](../../../../crates/phantom-dag/src/lib.rs) |
| Recall API | [`crates/phantom-recall/src/lib.rs`](../../../../crates/phantom-recall/src/lib.rs) |
| HistoryStore | [`crates/phantom-history/src/lib.rs`](../../../../crates/phantom-history/src/lib.rs) |
| SessionManager | [`crates/phantom-session/src/lib.rs`](../../../../crates/phantom-session/src/lib.rs) |
