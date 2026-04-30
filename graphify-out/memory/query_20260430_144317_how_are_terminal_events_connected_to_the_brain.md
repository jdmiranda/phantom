---
type: "explain"
date: "2026-04-30T14:43:17.573491+00:00"
question: "How are Terminal events connected to the Brain?"
contributor: "graphify"
source_nodes: ["TerminalAdapter", "App", "drain_bus_to_brain", "Brain"]
---

# Q: How are Terminal events connected to the Brain?

## Answer

TerminalAdapter (Community 8) publishes events like TerminalOutput and CommandComplete to the EventBus. The App::update loop (Community 19) calls drain_bus_to_brain, which subscribes to these topics and forwards them to the Brain (Community 5) as AiEvents. This forms the primary sensory pipeline for the AI.

## Source Nodes

- TerminalAdapter
- App
- drain_bus_to_brain
- Brain