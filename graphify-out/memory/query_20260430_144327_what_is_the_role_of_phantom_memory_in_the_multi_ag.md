---
type: "explain"
date: "2026-04-30T14:43:27.180979+00:00"
question: "What is the role of phantom-memory in the multi-agent system?"
contributor: "graphify"
source_nodes: ["phantom-memory", "EventLog", "AgentJournal", "Brain", "OODA"]
---

# Q: What is the role of phantom-memory in the multi-agent system?

## Answer

The phantom-memory crate (Community 106/Thin) provides the persistent project knowledge store. It is designed to be a sink for all events (EventLog) and a source of truth for Agents (Journal). It connects to phantom-app via the bundle_store and captures per-pane context for RAG-based recall. It acts as the 'Long-term Memory' for the Brain's OODA loop.

## Source Nodes

- phantom-memory
- EventLog
- AgentJournal
- Brain
- OODA