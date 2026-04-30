---
type: "explain"
date: "2026-04-30T14:43:21.803645+00:00"
question: "How does the Brain receive feedback from AI Agents?"
contributor: "graphify"
source_nodes: ["AgentAdapter", "EventBus", "drain_bus_to_brain", "Brain", "TaskLedger"]
---

# Q: How does the Brain receive feedback from AI Agents?

## Answer

AgentAdapter (Community 1) publishes AgentTaskComplete and AgentError events to the EventBus. These are drained in update.rs by drain_bus_to_brain and converted into AiEvent::AgentComplete, which is then sent to the Brain's reconciler (Community 63). This allows the Brain to track task success/failure and update the TaskLedger.

## Source Nodes

- AgentAdapter
- EventBus
- drain_bus_to_brain
- Brain
- TaskLedger