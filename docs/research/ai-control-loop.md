# Research: AI Control Loop Architecture for Phantom

**Date**: 2026-04-21
**Status**: Design Phase
**Priority**: Critical — this is what makes Phantom an AI system, not a terminal with AI bolted on

---

## The Core Question

Claude Code and every other AI coding tool today is **reactive** — it sleeps until you type something. Phantom should be **ambient** — always observing, always ready, proactively surfacing insights without being asked.

The difference:
- **Reactive (Claude Code)**: User types → AI thinks → AI acts → sleeps
- **Ambient (Phantom)**: AI observes continuously → notices pattern → surfaces suggestion → user accepts/rejects → AI learns

This is the difference between a tool you use and a system that thinks alongside you.

---

## Research: How Others Solve This

### 1. OODA Loop (Military → Agentic AI)

**Observe → Orient → Decide → Act**, originally designed for fighter pilots to outmaneuver opponents through faster decision cycles.

Applied to AI agents ([Harvard/Schneier](https://cyber.harvard.edu/story/2025-10/agentic-ais-ooda-loop-problem), [Sogeti](https://labs.sogeti.com/harnessing-the-ooda-loop-for-agentic-ai-from-generative-foundations-to-proactive-intelligence/)):

```
OBSERVE: Read terminal output, PTY data, file changes, git state
ORIENT:  Parse semantically, compare to project context, check memory
DECIDE:  Should I suggest? Should I act? Should I stay quiet?
ACT:     Show suggestion, spawn agent, update memory, do nothing
```

**Key insight from Schneier**: "AI's OODA loops must observe untrusted sources to be useful, and the competitive advantage of accessing web-scale information is identical to the attack surface." This is why we need the permission sandbox — the AI observes everything but acts within boundaries.

**Key insight from Sogeti**: "Agentic AI achieves faster response times through continuous cycling, improved decision-making through thorough orientation, and enhanced adaptability through feedback mechanisms."

Sources:
- [Agentic AI's OODA Loop Problem](https://cyber.harvard.edu/story/2025-10/agentic-ais-ooda-loop-problem)
- [Harnessing the OODA Loop for Agentic AI](https://labs.sogeti.com/harnessing-the-ooda-loop-for-agentic-ai-from-generative-foundations-to-proactive-intelligence/)
- [OODA Loop Pattern for Autonomous AI Agents](https://dev.to/yedanyagamiaicmd/the-ooda-loop-pattern-for-autonomous-ai-agents-how-i-built-a-self-improving-system-2ap3)
- [Oracle: What Is the AI Agent Loop?](https://blogs.oracle.com/developers/what-is-the-ai-agent-loop-the-core-architecture-behind-autonomous-ai-systems)

### 2. Event-Driven vs Polling

**Critical finding**: [Event-driven reduces agent latency by 70-90%](https://fast.io/resources/ai-agent-event-driven-architecture/) compared to polling. An event-driven agent incurs zero cost until a relevant event triggers it, while a polling agent wastes tokens checking repeatedly.

[Confluent's position](https://www.confluent.io/blog/the-future-of-ai-agents-is-event-driven/): "The future of AI agents is event-driven." Agents subscribe to event streams and react to changes, not poll for them.

For Phantom this means:
- The AI thread SLEEPS on a channel
- PTY output → event → AI wakes, parses, decides
- File change → event → AI notices, updates context
- Timer → event → AI checks long-running tasks
- User `!` key → interrupt event → AI responds immediately

**NOT**: AI polls terminal state every 100ms burning CPU.

Sources:
- [Event-Driven AI Agent Architecture Guide (2026)](https://fast.io/resources/ai-agent-event-driven-architecture/)
- [Event-Driven Architecture for AI Agents](https://atlan.com/know/event-driven-architecture-for-ai-agents/)
- [AWS: Event-Driven Architecture for Agentic AI](https://docs.aws.amazon.com/prescriptive-guidance/latest/agentic-ai-serverless/event-driven-architecture.html)
- [Event-Driven vs Poll-Based](https://bugfree.ai/knowledge-hub/event-driven-vs-poll-based-task-execution)

### 3. Claude Code's Architecture (Reverse-Engineered)

From [source analysis](https://gist.github.com/yanchuk/0c47dd351c2805236e44ec3935e9095d) and [community deep dives](https://github.com/VILA-Lab/Dive-into-Claude-Code):

**Agent Loop**: Simple while-loop. Model produces text + tool calls → agent executes tools → feeds results back → model continues. Loops until model stops issuing tool calls.

**What Phantom should KEEP from Claude Code**:
- Tool use loop (we built this in phantom-agents/api.rs)
- Speculative execution (start read-only tools while model is still streaming)
- Context compaction tiers (clear old tool outputs → summarize → extract to storage → truncate)
- Sub-agent spawning with task-specific prompts

**What Phantom should do DIFFERENTLY**:
- Claude Code is reactive (waits for user input). Phantom is ambient (observes continuously).
- Claude Code runs in YOUR terminal. Phantom IS the terminal — it sees everything, always.
- Claude Code has no memory between sessions. Phantom has persistent per-project memory.
- Claude Code has no semantic understanding of output. Phantom parses every command result.

Sources:
- [Claude Code Agent Architecture Deep Dive](https://gist.github.com/yanchuk/0c47dd351c2805236e44ec3935e9095d)
- [Dive into Claude Code — Systematic Analysis](https://github.com/VILA-Lab/Dive-into-Claude-Code)
- [Claude Code from Source — Reverse Engineered](https://github.com/alejandrobalderas/claude-code-from-source)
- [Claude Code Sub-agents](https://github.com/gregpriday/claude-code-docs/blob/main/claude-code-subagents.md)
- [Anthropic Claude Code GitHub](https://github.com/anthropics/claude-code)

### 4. Game AI Decision Systems

Games solve exactly our problem: autonomous entities that observe their environment, make decisions, and act — all in real-time, all proactively.

**Behavior Trees** ([survey](https://www.sciencedirect.com/science/article/pii/S0921889022000513)): Hierarchical decision trees. Root → selector/sequence nodes → leaf actions. Tick every frame. Good for structured behavior (patrol → detect → chase → attack). **Limitation**: rigid, doesn't handle nuance well.

**Utility AI** ([Game AI Pro](http://www.gameaipro.com/GameAIPro/GameAIPro_Chapter10_Building_Utility_Decisions_into_Your_Existing_Behavior_Tree.pdf)): Score every possible action by utility (how useful is it right now?). Pick the highest-scoring action. **Strength**: emergent behavior, graceful degradation, easy to add new actions. "If the AI is under fire, prioritize finding cover" — natural language maps directly to utility scorers.

**Key insight**: [Game developers are moving from Behavior Trees to Utility AI](https://www.gamedeveloper.com/programming/are-behavior-trees-a-thing-of-the-past-) because Utility AI produces more naturalistic, emergent behavior. Rules can be designed in natural language.

**For Phantom**: Utility AI is the right model. Each possible AI action gets a score:
- "Suggest fixing this error" → high score if error just appeared, low if user already started fixing
- "Show git status reminder" → medium score if many unstaged changes, low if just committed
- "Offer to explain" → high score if user seems stuck (no commands for 30s after an error)
- "Stay quiet" → default high score (don't be annoying)

Sources:
- [Behavior Trees vs FSMs](https://queenofsquiggles.github.io/guides/fsm-vs-bt/)
- [Are Behavior Trees a Thing of the Past?](https://www.gamedeveloper.com/programming/are-behavior-trees-a-thing-of-the-past-)
- [Utility Decisions in Behavior Trees](http://www.gameaipro.com/GameAIPro/GameAIPro_Chapter10_Building_Utility_Decisions_into_Your_Existing_Behavior_Tree.pdf)
- [BT vs FSM comparison (arxiv)](https://arxiv.org/html/2405.16137v1)
- [State Machines vs BTs for Robotics](https://www.polymathrobotics.com/blog/state-machines-vs-behavior-trees)

### 5. Ambient Agents (2026 Industry Direction)

**This is where the industry is heading**. [DigitalOcean](https://www.digitalocean.com/community/tutorials/ambient-agents-context-aware-ai): "Ambient agents are proactive AI agents that live in the background, continuously monitoring signals from systems you already use, and then autonomously advancing workflows."

Key properties of ambient agents:
- **Continuous** (not episodic) — always running
- **Environmental** (not device-bound) — aware of everything in the workspace
- **Proactive** (acting on their own) — don't wait to be asked
- **Multimodal** — across terminal output, file changes, git state, CI status

[Proactive AI in 2026](https://www.alpha-sense.com/resources/research-articles/proactive-ai/): "Moving beyond the prompt." The next generation of AI doesn't wait for instructions — it observes context and acts.

[Lenovo Qira (CES 2026)](https://news.lenovo.com/pressroom/press-releases/lenovo-unveils-lenovo-and-motorola-qira/): "A shift from app-based AI to ambient system-level intelligence — context-aware and available without requiring users to actively invoke it."

**Phantom IS an ambient agent platform.** The terminal is the environment. The AI observes everything that happens in it.

Sources:
- [Ambient Agents: Context-Aware AI](https://www.digitalocean.com/community/tutorials/ambient-agents-context-aware-ai)
- [What Is an Ambient Agent?](https://www.moveworks.com/us/en/resources/blog/what-is-an-ambient-agent)
- [Proactive AI in 2026](https://www.alpha-sense.com/resources/research-articles/proactive-ai/)
- [Proactive AI Agent Guide](https://www.emilingemarkarlsson.com/blog/proactive-ai-agents-guide-2025/)
- [Ambient Agents: Proactive AI for Smarter Automation](https://earlybirdlabs.com/insights/what-are-ambient-agents)
- [State of AI Agents 2026](https://www.prosus.com/news-insights/2026/state-of-ai-agents-2026-autonomy-is-here)

---

## Phantom's AI Architecture: The Design

### The Three Loops

Phantom runs three concurrent loops on separate threads:

```
┌─────────────────────────────────────────────────────────┐
│ LOOP 1: RENDER (main thread, 60fps)                     │
│   winit events → update UI → render frame → present     │
│   Owns: GPU, window, input dispatch                     │
│   Communicates via: channels to Loop 2 and 3            │
└─────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────┐
│ LOOP 2: TERMINAL I/O (dedicated thread)                 │
│   PTY read → semantic parse → emit events               │
│   Owns: PTY file descriptors, semantic parser           │
│   Emits: CommandComplete, ErrorDetected, OutputChanged  │
│   Communicates via: event channel to Loop 3             │
└─────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────┐
│ LOOP 3: AI BRAIN (dedicated thread) ← THE NEW THING    │
│   Event-driven OODA loop with Utility AI scoring        │
│   Owns: agent manager, memory, context, Claude API      │
│   Sleeps until: event arrives OR interrupt               │
│   Cycle: Observe → Orient → Score → Decide → Act        │
└─────────────────────────────────────────────────────────┘
```

### Loop 3: The AI Brain (Detail)

```rust
// Pseudocode for the AI brain thread

fn ai_brain_loop(event_rx: Receiver<AiEvent>, action_tx: Sender<AiAction>) {
    let mut context = ProjectContext::detect(".");
    let mut memory = MemoryStore::open(".");
    let mut agents = AgentManager::new(5);
    let mut scorer = UtilityScorer::new();
    
    loop {
        // OBSERVE: wait for an event (blocks — zero CPU when idle)
        let event = event_rx.recv();  // or recv_timeout for timer events
        
        // ORIENT: update world model
        match &event {
            AiEvent::CommandComplete(parsed) => {
                history.append(parsed);
                context.refresh_git();
                if parsed.errors.len() > 0 {
                    memory.set("last_error", &error_summary(parsed));
                }
            }
            AiEvent::FileChanged(path) => {
                context.note_file_change(path);
            }
            AiEvent::UserIdle(duration) => {
                // User hasn't typed in a while after an error
            }
            AiEvent::AgentComplete(id, result) => {
                // Process agent result, update memory
            }
            AiEvent::Interrupt(cmd) => {
                // User pressed !, handle immediately
            }
        }
        
        // SCORE: evaluate every possible action using Utility AI
        let candidates = vec![
            Action::SuggestFix { score: scorer.fix_score(&context, &memory) },
            Action::OfferExplanation { score: scorer.explain_score(&context) },
            Action::UpdateMemory { score: scorer.memory_score(&event) },
            Action::SpawnWatcher { score: scorer.watcher_score(&context) },
            Action::StayQuiet { score: 0.5 },  // default: don't be annoying
        ];
        
        // DECIDE: pick the highest-scoring action
        let best = candidates.iter().max_by(|a, b| a.score.partial_cmp(&b.score));
        
        // ACT: execute the decision
        match best {
            Action::SuggestFix { .. } => {
                action_tx.send(AiAction::ShowSuggestion(suggestion));
            }
            Action::SpawnAgent(task) => {
                let id = agents.spawn(task);
                // Start Claude API call on agent thread pool
            }
            Action::StayQuiet => {
                // Do nothing. This is often the right choice.
            }
        }
    }
}
```

### Utility Scorers

Each scorer considers context to produce a 0.0-1.0 score:

| Scorer | High Score When | Low Score When |
|--------|----------------|----------------|
| `fix_score` | Error just appeared, user hasn't started fixing | Error is old, user already editing the file |
| `explain_score` | Complex error, user seems stuck (idle after error) | Simple error, user immediately started fixing |
| `memory_score` | New pattern detected (e.g. first time seeing this error) | Already memorized |
| `watcher_score` | Long-running process expected (CI, deploy) | Nothing pending |
| `quiet_score` | User is actively typing, flow state | Always baseline 0.5 |

The `quiet_score` baseline of 0.5 means the AI only acts when it's MORE useful than staying silent. This prevents the #1 failure mode of proactive AI: being annoying.

### Event Types

```rust
enum AiEvent {
    // From Terminal I/O loop
    CommandComplete(ParsedOutput),
    OutputChunk(String),           // streaming output, partial
    ErrorDetected(DetectedError),
    
    // From file watcher (future)
    FileChanged(PathBuf),
    GitStateChanged,
    
    // From user
    Interrupt(String),             // ! command
    AgentRequest(AgentTask),       // explicit agent spawn
    
    // From agents
    AgentComplete(AgentId, AgentResult),
    AgentNeedsInput(AgentId),
    
    // Timers
    UserIdle(Duration),            // no input for N seconds
    WatcherTick(AgentId),          // periodic watcher check
    
    // System
    Shutdown,
}
```

### How It Differs From Claude Code

| | Claude Code | Phantom |
|---|-----------|---------|
| **Trigger** | User types a message | Events from terminal, files, git, timers |
| **Decision** | Always respond to user | Utility scoring — often stays quiet |
| **Context** | Reads files on demand | Continuously maintains world model |
| **Memory** | None between sessions | Persistent per-project memory |
| **Observation** | None — blind between prompts | Sees every command and output |
| **Proactivity** | Zero — purely reactive | Ambient — monitors and suggests |
| **Architecture** | Single async generator loop | Three-loop system (render, I/O, brain) |

---

## Implementation Plan

1. **AI Event System** — define AiEvent enum, channels between loops
2. **AI Brain Thread** — spawn dedicated thread, event-driven OODA loop
3. **Utility Scorers** — scoring functions for each possible action
4. **Terminal I/O Thread** — move PTY reads to dedicated thread, emit parsed events
5. **Integration** — wire AI actions back to render loop (show suggestions, spawn panes)
6. **Memory Integration** — AI brain reads/writes project memory on every cycle
7. **Watcher Agents** — long-running agents that periodically check things (CI, deploy)
