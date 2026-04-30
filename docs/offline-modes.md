# Offline Modes — Phantom Terminal

This document describes what Phantom can and cannot do without a network
connection. Three distinct operating modes exist: **terminal-only**,
**local-AI**, and **cloud-AI**. Each has a different prerequisite and a
different set of capabilities.

---

## Quick reference

| Mode | Network required? | API key required? | Local model runtime required? | Status bar |
|---|:---:|:---:|:---:|---|
| Terminal-only | No | No | No | (none) |
| Local-AI | No | No | Yes (Ollama) | — |
| Cloud-AI | Yes | Yes | No | — |
| Privacy mode | No | No | Recommended | `[P]` |

---

## Terminal-only (unconditional offline guarantee)

The following features work with **zero API keys, zero network access, and no
local model runtime**:

- GPU-accelerated terminal emulation (PTY, VT100/xterm, Kitty protocol)
- All five CRT shader themes, live shader parameter tweaks
- Tmux-style pane splitting, focus cycling, pane close
- Process-detach detection (vim, htop, ssh alternate-screen mode)
- Session save and restore (working directories, pane layout, git branch)
- Structured command history (JSONL store, cross-session search)
- Per-project memory (key-value store, atomic saves)
- Supervisor heartbeat and auto-restart (two-process model)
- Semantic output parsing (git status, cargo errors, JSON, tabular data)
- Natural-language command heuristics — 100+ built-in passthrough rules
  (e.g. "build" → `cargo build`) run entirely locally
- Project context detection (language, framework, package manager, git state)
- MCP server exposure — other tools can drive Phantom over the local Unix socket

**None of these features make network calls.** You can work offline without
configuring anything.

---

## Local-AI (offline AI — optional, requires Ollama)

A subset of AI features can operate without cloud access when a local model
runtime is running on the same machine. Phantom's brain router is pre-wired
for Ollama but the runtime is **not bundled** — you must install it separately.

### What is required

1. **Ollama** running locally (`http://localhost:11434`) with at least one
   compatible model pulled (e.g. `ollama run phi3.5`).
2. No API key and no network access are required once the model is downloaded.

### What works in local-AI mode

- AI brain ambient scoring (OODA loop utility scoring, event triage) —
  Trivial and Simple complexity tasks are routed to the local Ollama backend.
- Natural-language commands that fall through the heuristic interpreter can be
  classified by the local model instead of Claude.
- Privacy mode (see below) routes all AI work exclusively to local backends.

### What does NOT work without a network-capable backend

- Complex multi-step agent tasks (code generation, tool use, long-form
  reasoning) — these are `TaskComplexity::Complex` and require a frontier
  model. Heuristic and Ollama backends handle `Trivial` and `Simple` tasks
  only. With no cloud backend available, complex agent requests will fail with
  a "no backend available" error.
- The NLP LLM fallback (`nlp_llm = true` in config) requires the Claude API.
  Disable it with `nlp_llm = false` if you are working offline without Ollama;
  the heuristic interpreter still handles the majority of common commands.

### How to tell if Ollama is being used

The brain router health-checks Ollama at startup and after each failure.
If Ollama is unreachable at startup, the Ollama backend is marked unavailable
and tasks cascade to the next available backend (heuristic or cloud). There is
no persistent status bar indicator for Ollama availability in the current
release.

---

## Cloud-AI (default, requires network + API key)

By default Phantom routes complex AI tasks to the Anthropic Claude API.

### Prerequisites

- Active network connection.
- `ANTHROPIC_API_KEY` environment variable set, or the key configured via your
  shell environment before launching Phantom.

### What works in cloud-AI mode

Everything in terminal-only mode, plus:

- Full AI agent system (7 sandboxed tools, multi-step reasoning, code
  generation)
- AI brain ambient intelligence for all task complexity tiers
- Error-to-agent pipeline (build fails → Phantom offers to fix it via Claude)
- NLP LLM fallback for unrecognized natural-language commands

### What happens when the network drops mid-task

The brain router marks a backend unavailable after a failed call and cascades
to the next backend in priority order (cheapest first: heuristic → Ollama →
Claude). If no backend can handle the task, the router returns an empty
candidate list and the brain logs a "no backend available" event — no crash,
no retry loop. The terminal itself keeps running normally; only the AI features
that required network access degrade.

---

## Privacy mode

Privacy mode is a hard block on all cloud API calls. It is implemented as a
dual gate: the `PrivacyGuard` interceptor in `phantom-agents` and the routing
filter in `phantom-brain`'s `BrainRouter`. Together they guarantee that no
network socket is opened to a cloud provider regardless of which code path
triggers a routing decision.

### What privacy mode does

- Blocks all calls to Anthropic Claude, OpenAI-compatible endpoints, and any
  backend whose `is_cloud_provider()` returns `true`.
- Local backends (Ollama, heuristic rule-engine) continue to work normally.
- The status bar shows a `[P]` lock indicator while the mode is active.

### What privacy mode does NOT guarantee without Ollama

Privacy mode alone does not make AI features work offline — it only prevents
cloud calls. If Ollama is not running, AI tasks that need more than heuristic
scoring will fail because there is no available backend to handle them. Install
and run Ollama to use AI features in privacy mode.

### Enabling privacy mode

**Config file** (`~/.config/phantom/config.toml`):

```toml
privacy_mode = true
```

**Runtime toggle** (command mode, press `` ` ``):

```
privacy on
privacy off
```

The `[P]` indicator in the status bar confirms the mode is active.

---

## Summary: what works with zero API keys

| Feature | Works? | Notes |
|---|:---:|---|
| Terminal emulation | Yes | Unconditional |
| Pane splitting / session restore | Yes | Unconditional |
| Semantic output parsing | Yes | Local, no AI needed |
| Heuristic NLP (100+ commands) | Yes | Local, no AI needed |
| Project context detection | Yes | Local, reads fs/git |
| Per-project memory | Yes | Local JSONL |
| Ollama ambient AI (Trivial/Simple) | Optional | Requires Ollama running |
| Complex agent tasks (code gen) | No | Requires cloud backend |
| NLP LLM fallback | No | Requires `ANTHROPIC_API_KEY` |

---

## Related

- **Privacy mode implementation**: PR [#350](https://github.com/jdmiranda/phantom/pull/350) (open, closes issue #322)
- **Offline mode / graceful disconnect**: PR [#353](https://github.com/jdmiranda/phantom/pull/353) (open, closes issue #1)
- **Provider/backend capability matrix**: [`docs/providers.md`](providers.md)
- **Config reference**: `~/.config/phantom/config.toml` — run `phantom --write-config` to generate a commented default
- **Issue #363**: this doc closes the documentation gap identified there
