---
type: "explain"
date: "2026-04-30T14:43:27.164905+00:00"
question: "Where does phantom-semantic fit into the architecture?"
contributor: "graphify"
source_nodes: ["SemanticParser", "TerminalOutput", "EventBus", "Brain"]
---

# Q: Where does phantom-semantic fit into the architecture?

## Answer

The phantom-semantic crate (Community 123/Thin) is designed to be the parser for the EventBus 'TerminalOutput' topic. Based on current stubs in crates/phantom-semantic/src/lib.rs, it will classify commands (git, cargo, npm) and parse their output into structured types. It will eventually sit between the TerminalAdapter and the Brain, turning raw bytes into semantic insights.

## Source Nodes

- SemanticParser
- TerminalOutput
- EventBus
- Brain