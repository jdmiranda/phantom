# Phantom Vision — Where We're Going

**Last updated**: 2026-04-21

---

## What Phantom Is

Phantom is an **AI-native app platform** disguised as a terminal emulator.

The terminal is the interface. Intelligence is the substrate. Every command is understood. Every error is caught. Every app is an equal citizen that the AI can see and control.

## What Phantom Is NOT

- Not a terminal with a chatbot sidebar (Warp)
- Not a terminal with inline widgets (Wave)
- Not a terminal with autocomplete (Fig/Amazon Q)
- Not a reactive AI that waits for you to type (Claude Code)

## The Key Ideas

### 1. Everything Is An App
The terminal pane is an app. The agent pane is an app. A database browser is an app. A log viewer is an app. A speech-to-text service is an app. They all implement the same `AppAdapter` trait, run sandboxed in WASM, and the AI can see and control all of them equally.

### 2. Apps Compose Via Pub/Sub
Apps publish and subscribe to typed data streams. Terminal output flows to the semantic parser. The parser publishes structured errors. The error detector subscribes and triggers agent suggestions. Like Unix pipes, but for structured data between GUI apps. Inspired by Yahoo Pipes.

### 3. The AI Is Ambient
Not reactive — ambient. The AI brain runs on its own thread, observing everything continuously. It uses Utility AI scoring (from game AI) to decide when to suggest, when to act, when to stay quiet. The quiet score is 0.5 — the AI only speaks when it has something MORE useful than silence.

### 4. Spatial Intelligence
Apps negotiate for space. A database browser says "I need 2 stacked panes, at least 60 cols wide." The layout arbiter resolves this against other apps' claims. Apps query neighbors and negotiate. The scene graph tracks what's taken and what's available.

### 5. The Terminal Remembers
Per-project memory persists across sessions. "This project uses pnpm." "Port 3001." "The auth module is being refactored." Session restore puts you right back where you were. The terminal gets smarter every day.

### 6. Remote Control
MCP protocol (server + client) means any AI system can operate Phantom. Claude Code running inside Phantom gets superpowers. TCP listener enables remote automation. The AI can type keystrokes, read output, manage panes — indistinguishable from a human operator.

## Architecture Summary

```
                     ┌──────────────────┐
                     │    AI BRAIN      │ (OODA loop, utility scoring)
                     │   (own thread)   │
                     └────────┬─────────┘
                              │ events/actions
                     ┌────────▼─────────┐
                     │   APP REGISTRY   │ (lifecycle, discovery, gc)
                     ├──────────────────┤
                     │    EVENT BUS     │ (pub/sub between apps)
                     ├──────────────────┤
                     │  LAYOUT ARBITER  │ (spatial negotiation)
                     ├──────────────────┤
                     │   SCENE GRAPH    │ (dirty tracking, z-order)
                     └────────┬─────────┘
                              │
              ┌───────────────┼───────────────┐
              │               │               │
        ┌─────▼─────┐  ┌─────▼─────┐  ┌─────▼─────┐
        │TerminalApp│  │ AgentApp  │  │  WASMApp  │
        │  (PTY)    │  │ (Claude)  │  │ (sandbox) │
        └───────────┘  └───────────┘  └───────────┘
              │               │               │
        ┌─────▼─────┐  ┌─────▼─────┐  ┌─────▼─────┐
        │  Semantic  │  │  Memory   │  │  Plugin   │
        │  Parser    │  │  Store    │  │  Runtime  │
        └───────────┘  └───────────┘  └───────────┘
              │
        ┌─────▼──────────────────────────────────┐
        │         GPU RENDER PIPELINE             │
        │  Scene → Post-FX (CRT) → Overlay       │
        └────────────────────────────────────────┘
```

## The End State

You open Phantom. The skull glitch-reveals. System checks pass. "SYSTEM READY."

You start working. The AI watches. You run `cargo build`. It fails. Phantom says: "[PHANTOM]: 2 errors in auth.rs. Fix it?" You press Y. An agent spawns in a tethered pane, reads the code, writes the fix, runs the build — passes.

Meanwhile, a speech-to-text app is transcribing your pair programming call. A file watcher notices your coworker pushed to main. The status bar shows 3 new GitHub notifications. The database browser in the right pane auto-refreshes when the migration agent finishes.

You didn't configure any of this. It just works. Because the terminal isn't a dumb pipe anymore.

It's a cognitive interface.

That's Phantom.
