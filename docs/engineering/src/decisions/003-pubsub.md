> Canonical source: [`docs/ARD-003-app-lifecycle-pubsub.md`](../../../ARD-003-app-lifecycle-pubsub.md). Edits go there, not here.

{{#include ../../../ARD-003-app-lifecycle-pubsub.md}}

---

## Components affected

- [Substrate](../components/substrate.md) — `phantom-protocol::Event` enum + the bus topics live here.
- [Agents](../components/agents.md) — emit `agent.*` topics.
- [Brain](../components/brain.md) — subscribes to every topic for OODA observation.

## Flows that honour this

- [Flow 1 · Cold launch](../flows/01-cold-launch.md) — adapter registration emits bus events.
- [Flow 2 · Agent spawn](../flows/02-agent-spawn.md) — the canonical example of typed pub-sub between coordinator and bus subscribers.
