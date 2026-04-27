# Phase 7 Research: Goal-Directed Autonomy

Deep research synthesis from 3 parallel research agents. 50+ references
across autonomous agents, multi-agent orchestration, and intrinsic motivation.

## Key Architectural Patterns to Steal

### 1. Magentic-One Task Ledger (Microsoft Research, 2024)
A single Orchestrator maintains a structured ledger: planned, in-progress,
completed, failed. Re-plans on failure. Specialists are stateless — they
receive everything in their instructions.
- [Paper](https://arxiv.org/abs/2411.04468)
- **Steal**: Parent agent with explicit task ledger, not just prompts.

### 2. Anthropic Orchestrator-Worker (2025)
Lead agent spawns 3-5 subagents with: objective, output format spec, tool
guidance, task boundaries. Each gets isolated context. 90% latency reduction
vs sequential.
- [Blog](https://www.anthropic.com/engineering/multi-agent-research-system)
- **Steal**: Output format specs make the reduce step programmatic.

### 3. Handoffs-as-Tools (OpenAI Swarm → Agents SDK, 2024)
Agent delegation modeled as tool calls: `delegate_to(specialist, task)`.
No complex orchestration protocol needed.
- [GitHub](https://github.com/openai/swarm)
- **Steal**: Add `delegate` as a tool in the agent toolkit.

### 4. Reducer-Driven Shared State (LangGraph, 2024)
Typed state schema. Each agent returns a delta. Reducers merge
deterministically. Eliminates race conditions in parallel execution.
- [LangGraph](https://www.langchain.com/langgraph)
- **Steal**: Typed artifacts between agents, not prose.

### 5. Event Sourcing (OpenHands, 2024-2025)
Every agent action is an immutable, replayable event. Enables audit,
rollback, replay, and debugging.
- [OpenHands](https://openhands.dev/)
- **Steal**: Already have event bus — make events persistent.

### 6. Conflict Detection Before Dispatch (Clash, 2025)
Before spawning parallel agents, analyze task decomposition for file/module
overlap. Cheaper than discovering conflicts at merge time.
- [GitHub](https://github.com/clash-sh/clash)
- **Steal**: `check_isolation(tasks) -> Vec<Conflict>` in orchestrator.

### 7. Structured Artifact Protocol (MetaGPT, ICLR 2024)
Agents produce typed documents (TaskDecomposition, CodePatch, TestResult),
not prose. Pub-sub communication.
- [Paper](https://arxiv.org/abs/2308.00352)
- **Steal**: Define artifact types in phantom-protocol.

## Intrinsic Motivation — How the Brain Decides What's Worth Doing

### 8. ICM Curiosity (Pathak et al., ICML 2017)
Prediction error = reward. High error = "I don't understand this yet."
- [Paper](https://arxiv.org/abs/1705.05363)
- **Phantom**: `novelty_score()` based on how surprising command output is.

### 9. Empowerment (Salge et al., 2014)
Maximize mutual information between actions and future states. Prefer
actions that keep options open.
- **Phantom**: Fixing a blocking error has high empowerment (unlocks everything).
  Docs have low empowerment. Weight actions by downstream value.

### 10. RICE Scoring (Intercom, 2018)
Score = (Reach × Impact × Confidence) / Effort.
- [Blog](https://www.intercom.com/blog/rice-simple-prioritization-for-product-managers/)
- **Phantom**: Replace hardcoded scores with:
  `score = (surprise × empowerment × confidence × context_alignment) / effort + exploration_bonus`

### 11. UCB Bandits (Auer et al., 2002)
Exploration bonus for under-tried actions. Decays with experience.
- **Phantom**: Track (attempts, accepted, dismissed) per action type in memory.
  Under-tried actions get exploration bonus.

### 12. Value of Information (Raiffa, 1968)
Before acting: "Would more information change my decision?"
- **Phantom**: Unambiguous error → suggest immediately. Ambiguous crash →
  investigate first. Replaces hardcoded "3+ errors = Complex" threshold.

### 13. Flow State Research (Gloria Mark, CHI 2008)
23 minutes to refocus after interruption. Developers self-interrupt as
often as external interruptions.
- [Paper](https://ics.uci.edu/~gmark/chi08-mark.pdf)
- **Phantom**: Track typing patterns → detect flow state → raise quiet
  threshold to 0.9. Queue suggestions for natural breaks.

### 14. Hotspot Analysis (Tornhill, "Your Code as a Crime Scene")
Files with high complexity AND high change frequency = highest-value targets.
- [CodeScene](https://codescene.com/)
- **Phantom**: Track change frequency per file from git log. Weight
  proactive actions by hotspot rank.

### 15. Reflexion (Shinn et al., NeurIPS 2023)
Store natural-language reflections after each attempt. Prepend to future
prompts. 91% on HumanEval with no fine-tuning.
- [Paper](https://arxiv.org/abs/2303.11366)
- **Phantom**: After every action: "Suggested clippy fix during build —
  user dismissed because they were in flow. Next time, wait for idle."

### 16. Voyager Automatic Curriculum (Wang et al., 2023)
Agent proposes increasingly complex tasks based on current capabilities.
Skill library stores verified solutions.
- [Paper](https://arxiv.org/abs/2305.16291)
- **Phantom**: Start with "run clippy" → "fix warnings" → "add tests" →
  "refactor hotspot." Track capability level in memory.

### 17. PDCA Check-Act (Deming)
Plan-Do-Check-Act. The Check phase measures whether your intervention
actually improved things.
- **Phantom**: After fix, observe next build. Did errors decrease?
  Record outcome. Brain learns what actually works.

### 18. Self-Instruct (Wang et al., ACL 2023)
Model generates its own task types from seeds.
- [Paper](https://arxiv.org/abs/2212.10560)
- **Phantom**: During idle, ask LLM: "What proactive checks would be
  valuable for this project?" Validate and store.

## Multi-Agent Orchestration

### 19. CrewAI Hierarchical Process (2023-present)
Manager agent decomposes → delegates → validates. Task dependency graph.
- [CrewAI](https://crewai.com/)
- **Steal**: Manager should be a cheap model call (planning, not working).

### 20. AutoGen Group Chat (Microsoft, 2023)
Agents talk in group chats. GroupChatManager selects who speaks.
Runtime separates agent definition from execution.
- [GitHub](https://github.com/microsoft/autogen)
- **Steal**: Agents portable across local and distributed execution.

### 21. Devin Interactive Planning (Cognition, 2025)
Show decomposition plan to user, let them edit, then execute.
- [Devin](https://cognition.ai/blog/devin-2)
- **Steal**: Interactive planning before dispatch. Critical for trust.

### 22. Claude Code Agent Teams (Anthropic, 2026)
2-4 parallel agents is the sweet spot. Spec-driven decomposition prerequisite.
Mailbox for peer-to-peer communication.
- [Docs](https://code.claude.com/docs/en/sub-agents)
- **Steal**: 2-4 agents, spec-first, worktree isolation.

### 23. Blackboard Architecture (2025)
Shared data structure. Agents volunteer based on capability, not assignment.
13-57% improvement over master-slave patterns.
- [Paper](https://arxiv.org/abs/2507.01701)
- **Steal**: Volunteer activation via event bus monitoring.

### 24. A2A Agent Cards (Google, 2025)
Machine-readable capability descriptors per agent. Task lifecycle states.
- [a2a-protocol.org](https://a2a-protocol.org/latest/)
- **Steal**: Agent Cards + InputRequired status state.

### 25. Temporal Durable Execution (2020-2025)
Workflows survive crashes via event sourcing + deterministic replay.
Checkpoint after each side effect. Resume from last checkpoint.
- [temporal.io](https://temporal.io/solutions/ai)
- **Steal**: Checkpoint agent state after each tool call.

### 26. HTN Reusable Decomposition (Classical AI + 2024 LLM integration)
Save successful task decompositions as reusable templates.
- [GPT-HTN-Planner](https://github.com/DaemonIB/GPT-HTN-Planner)
- **Steal**: Store decomposition templates in memory. Learning planner.

### 27. MapReduce for Agents (2024-2025)
Scatter → Execute in parallel → Gather → Reduce. The reduce step should
be pluggable: concatenate, LLM synthesize, or git merge.
- [AWS Pattern](https://docs.aws.amazon.com/prescriptive-guidance/latest/agentic-ai-patterns/parallelization-and-scatter-gather-patterns.html)

## Software Engineering Agents

### 28. SWE-agent (NeurIPS 2024)
Agent-Computer Interface design matters as much as the model.
- [Paper](https://arxiv.org/abs/2405.15793)

### 29. AutoCodeRover (ISSTA 2024)
Program-structure-aware search + spectrum-based fault localization.
$0.43/issue, 4 minutes vs 2.68 developer-days.
- [Paper](https://arxiv.org/abs/2404.05427)

### 30. Agentless (ICLR 2025)
Localize → Repair → Validate pipeline. No autonomous loop needed for
most fixes. Generate N candidates, validate all.
- [Paper](https://arxiv.org/abs/2407.01489)
- **Steal**: Localize-repair-validate for FixError tasks.

### 31. ACE (FSE 2025)
LLM refactorings validated by static analysis. 37% correct raw, 98%
after validation filtering.
- [Paper](https://arxiv.org/abs/2507.03536)

### 32. SPRING (NeurIPS 2023)
Read the game's paper to understand rules, then build a knowledge DAG.
Reading docs is more efficient than trial-and-error.
- [Paper](https://arxiv.org/abs/2305.15486)
- **Phantom**: Read README, Cargo.toml, CI config during idle. Build
  project knowledge DAG in memory.

## The Unified Scoring Formula

Replace hardcoded scores with:

```
score = (surprise × empowerment × confidence × context_alignment) / effort + exploration_bonus
```

Where:
- surprise = prediction error (ICM, Ref 8)
- empowerment = downstream value unlocked (Ref 9)
- confidence = diagnosis certainty (RICE, Ref 10)
- context_alignment = is this in user's current focus (Ref 13, 14)
- effort = estimated token cost + time (RICE)
- exploration_bonus = UCB for under-tried actions (Ref 11)

## The Goal Loop

```
IDLE (>60s) → SCAN (TODOs, clippy, test gaps, hotspots)
            → SCORE (unified formula)
            → GATE (flow state? VoI worth it?)
            → ACT (suggest or auto-fix based on confidence)
            → CHECK (did next build improve?)
            → LEARN (reflexion, skill library, bandit update)
```

This is the brain deciding what to work on. Not reacting. Choosing.
