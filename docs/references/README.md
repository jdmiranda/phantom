# References

Architecture and design references for Phantom. Mine these for patterns,
not prose.

## Agent Architecture
- [ReAct: Synergizing Reasoning and Acting in Language Models](https://arxiv.org/abs/2210.03629) — Yao et al. 2023. The observe-think-act loop that agent systems are built on. Phantom's OODA cycle is a real-time variant.
- [Toolformer: Language Models Can Teach Themselves to Use Tools](https://arxiv.org/abs/2302.04761) — Schick et al. 2023. Tool selection as a learned behavior. Relevant for the brain's utility scoring of when to invoke tools vs when to stay quiet.
- [Voyager: An Open-Ended Embodied Agent with Large Language Models](https://arxiv.org/abs/2305.16291) — Wang et al. 2023. Minecraft agent that writes its own code, verifies it, and builds a skill library. Direct analog to selftest/selfheal.
- [SWE-agent: Agent-Computer Interfaces for Software Engineering](https://arxiv.org/abs/2405.15793) — Yang et al. 2024. How the interface between agent and computer matters more than the model. Phantom's adapter system is this interface.

## Systems Architecture
- [A Philosophy of Software Design](https://web.stanford.edu/~ouster/cgi-bin/book.php) — John Ousterhout. Deep modules over shallow. Every trait in phantom-adapter should have a deep implementation behind a narrow interface.
- [Game Programming Patterns](https://gameprogrammingpatterns.com/) — Robert Nystrom. State machines, component pattern, update method, event queue. Phantom's coordinator is the Update Method pattern; AgentPane is a State Machine; the event bus is the Event Queue.
- [Designing Data-Intensive Applications](https://dataintensive.net/) — Martin Kleppmann. Event sourcing, exactly-once semantics, backpressure. The bus→brain pipeline is an event-driven system.
- [The Art of UNIX Programming](http://www.catb.org/esr/writings/taup/html/) — Eric Raymond. Small composable tools. The adapter trait system is Unix pipes for AI.

## Latency & Real-Time
- [John Carmack on latency](https://web.archive.org/web/20140719085550/http://www.altdevblogaday.com/2013/02/22/latency-mitigation-strategies/) — frame budgets, prediction, pipeline design. Tool execution must not block the render loop.
- [OODA Loop](https://en.wikipedia.org/wiki/OODA_loop) — John Boyd. Observe-Orient-Decide-Act. The brain's core cycle. Speed of the loop is the competitive advantage.

## Protocol & Tool Use
- [Model Context Protocol Specification](https://spec.modelcontextprotocol.io/) — Anthropic. The tool/resource federation protocol Phantom's MCP crate implements.
- [Claude Tool Use Documentation](https://docs.anthropic.com/en/docs/build-with-claude/tool-use) — Anthropic. The API contract for tool_use blocks, tool_result responses, and multi-turn execution.
- [JSON-RPC 2.0 Specification](https://www.jsonrpc.org/specification) — The wire protocol under MCP.

## GPU & Rendering
- [Learn Wgpu](https://sotrh.github.io/learn-wgpu/) — wgpu patterns. Phantom's 3-pass rendering (scene → postfx → overlay) follows the render pass architecture.
- [Bevy ECS](https://bevyengine.org/) — entity-component-system patterns. The coordinator's adapter registry is a simplified ECS.
