> Canonical source: [`docs/ARD-002-wasm-app-adapter.md`](../../../ARD-002-wasm-app-adapter.md). Edits go there, not here.

{{#include ../../../ARD-002-wasm-app-adapter.md}}

---

## Components affected

- [Substrate](../components/substrate.md) — the `AppAdapter` trait family is the WASM contract.
- [Extensibility + Capture](../components/extensibility.md) — `phantom-plugins` will host the WASM runtime.

## Flows that honour this

_(No anchor flow currently exercises the WASM path; this is Phase 2+ work.)_
