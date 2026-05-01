# Operator Guide — Provisioning Narrow-Capability `phantom-hub` API Keys

> Audience: hub operators deploying `phantom-hub` to Railway (or any other
> environment). This guide explains how to move a deployment from the legacy
> "every key has every capability" default to fail-closed, narrow-grant keys
> backed by [`ApiKeyStore::from_entries`].
>
> Source of truth for the runtime behaviour described here is
> [`crates/phantom-hub/src/auth.rs`](../src/auth.rs). When the code and this
> document disagree, the code wins.

---

## TL;DR

1. Today, every API key loaded from the `HUB_API_KEYS` environment variable is
   granted **all capabilities** (`Sense`, `Compute`, `Coordinate`). This is the
   v1 backwards-compat default — a single leaked key can call every MCP tool.
2. The runtime already supports per-key capability scoping via
   [`ApiKeyStore::from_entries`]. New keys can be provisioned with a narrow,
   explicit `HashSet<CapabilityClass>`.
3. Recommended migration: provision narrow keys via `from_entries`, rotate the
   `HUB_API_KEYS`-loaded keys out, and end up with a deployment where every key
   in the store has an explicit grant.

---

## Background — the data model

The hub identifies an MCP caller by API key. Each stored key carries the set of
operations it is allowed to invoke. Operations are grouped into three
[`CapabilityClass`] variants:

| Variant       | What it grants                                                                    |
|---------------|-----------------------------------------------------------------------------------|
| `Sense`       | Read / observe — `phantom.list_phantoms`, `phantom.read_output`                   |
| `Compute`     | Execute shell commands — `phantom.run_command`                                    |
| `Coordinate`  | Spawn or steer agents — `phantom.spawn_agent`, `phantom.list_panes`, `phantom.get_agent_status` |

Internally the store holds:

```rust
struct ApiKeyEntry {
    hash: [u8; 32],                       // SHA-256 of the raw key
    capabilities: HashSet<CapabilityClass>,
}
```

Raw key strings are hashed once at construction and discarded. Validation walks
every entry in constant time (`subtle::ConstantTimeEq`) and, on a match,
returns a [`SessionIdentity`] that exposes the granted capability set:

```rust
pub struct SessionIdentity {
    pub key_hash: [u8; 32],
    pub capabilities: HashSet<CapabilityClass>,
}

impl SessionIdentity {
    pub fn has(&self, class: CapabilityClass) -> bool { /* ... */ }
}
```

Each MCP tool handler is expected to call `session.has(CapabilityClass::X)`
before it forwards a frame to the target Phantom. A capability that is not in
the set means the handler returns `403 Forbidden` (or the JSON-RPC equivalent)
without dispatching.

See [`crates/phantom-hub/src/auth.rs`](../src/auth.rs) for the full type
definitions, doc-comments, and round-trip tests.

---

## Why `HUB_API_KEYS` keys default to `ALL_CAPABILITIES`

PR #522 added per-key capability scoping. Prior to that PR, every accepted
key could call every tool — there was no notion of per-key authorisation. To
avoid breaking existing Railway deployments that already had keys in
`HUB_API_KEYS`, the env-var loader explicitly grants the full set:

```rust
// crates/phantom-hub/src/auth.rs — ApiKeyStore::from_raw_keys
pub fn from_raw_keys<'a>(keys: impl Iterator<Item = &'a str>) -> Self {
    let all: HashSet<CapabilityClass> = ALL_CAPABILITIES.iter().copied().collect();
    // ... each entry receives `all.clone()` ...
}
```

`ALL_CAPABILITIES` is the single constant that defines this default:

```rust
pub const ALL_CAPABILITIES: &[CapabilityClass] = &[
    CapabilityClass::Sense,
    CapabilityClass::Compute,
    CapabilityClass::Coordinate,
];
```

Operators should treat this default as a transitional safety net, not a final
state. The next two sections explain how to leave it behind.

---

## Provisioning a narrow-capability key

`ApiKeyStore::from_entries` accepts an iterator of `(raw_key, capabilities)`
pairs and is the supported entry point for fail-closed provisioning:

```rust
use std::collections::HashSet;
use phantom_hub::auth::{ApiKeyStore, CapabilityClass};

// A read-only key — can list phantoms and read output, nothing else.
let read_only_caps = HashSet::from([CapabilityClass::Sense]);

// A coordination key for an agent-orchestrator session — can spawn and steer
// agents but cannot execute arbitrary shell commands.
let orchestrator_caps = HashSet::from([
    CapabilityClass::Sense,
    CapabilityClass::Coordinate,
]);

let store = ApiKeyStore::from_entries(
    [
        ("phk_read_only_session_xyz",  read_only_caps),
        ("phk_orchestrator_session_a", orchestrator_caps),
    ]
    .into_iter()
    // `from_entries` takes an iterator of `(&str, HashSet<CapabilityClass>)`.
    .map(|(k, c)| (k, c)),
);
```

Hand the `store` to `AppState` exactly as you would a store built by
`ApiKeyStore::from_env`. From there, every `validate(...)` call returns a
`SessionIdentity` whose `capabilities` field is *only* the set you provisioned.

### Operational notes

- Pick keys that match the `phk_<base64url>` shape used by the rest of the
  codebase. The store does not enforce a prefix, but tooling and log filters
  expect `phk_`.
- Generate at least 32 random bytes per key, base64url-encode without padding,
  and prefix with `phk_`. Treat the raw value like a credential — log only the
  SHA-256 hash, never the plaintext.
- The hashed comparison is constant-time across the whole store, so storing
  many keys (dozens to low-hundreds) has no security cost. It does cost a tiny
  amount of CPU per validate; this is irrelevant at expected request rates.

---

## Recommended migration plan for production deployments

Goal: every accepted key has an explicit, narrow capability grant. No key in
the running store carries `ALL_CAPABILITIES` by accident.

### Phase 1 — inventory

1. List every consumer of the hub today (Claude session, automation script,
   internal tool). Group them by what they actually call:
   - Sessions that only read fleet state → `Sense`.
   - Sessions that run shell commands → `Sense + Compute`.
   - Sessions that drive agents → `Sense + Coordinate`.
   - Sessions that do all three → `Sense + Compute + Coordinate`
     (equivalent to `ALL_CAPABILITIES`, but issued explicitly).
2. Decide a key-naming scheme that encodes the role (e.g.
   `phk_role_readonly_<id>`, `phk_role_runner_<id>`). The store does not parse
   names; this is purely operator hygiene.

### Phase 2 — provision narrow keys side-by-side

1. Generate one new key per consumer with the minimal capability set.
2. Wire those keys into the hub via `ApiKeyStore::from_entries` at startup.
   Today the simplest path is to extend the hub's startup wiring (`AppState`
   construction) to merge:
   - the legacy `from_env()` store (still loaded for compat), and
   - an explicit `from_entries(...)` block built from a sealed config file
     (Railway secret, Kubernetes Secret, etc.).
3. Roll the new key out to one consumer at a time. Confirm in logs that the
   capability check passes for the operations the consumer needs and fails
   (with `403`) for everything else. Adjust the capability set if a legitimate
   call fails.

### Phase 3 — retire the legacy keys

1. Once every consumer has been moved to a narrow-grant key, remove the old
   `phk_...` values from `HUB_API_KEYS`.
2. Either set `HUB_API_KEYS=""` (so `from_env()` returns an empty store and the
   merge becomes a no-op) or delete the variable entirely. With no env-loaded
   keys, every accepted key is one provisioned via `from_entries` with a
   narrow set. Fail-closed by construction.
3. Document the active set of keys + capability grants in your runbook so the
   on-call rotation knows what each key can do.

### Phase 4 — ongoing

- Treat capability widening as a privileged operation: it requires a new key,
  not an in-place edit, so audit logs always show a key-rotation event.
- Rotate keys on a fixed cadence (e.g. quarterly) and on every personnel
  change. Rotation is just "issue a new entry, deploy, retire the old one".

---

## See also

- `crates/phantom-hub/src/auth.rs` — `CapabilityClass`, `ApiKeyEntry`,
  `ApiKeyStore`, `SessionIdentity`, with inline rustdoc.
- Tests in the same file —
  `api_key_from_raw_keys_receives_all_capabilities`,
  `api_key_from_entries_with_narrow_caps_only_allows_granted_caps`,
  `session_identity_has_matches_stored_capabilities` — exercise the contract
  this document relies on.
- Issue [#511](https://github.com/jdmiranda/phantom/issues/511) — original
  capability-scoping design.
- Issue [#529](https://github.com/jdmiranda/phantom/issues/529) — this
  operator guide.

[`ApiKeyStore::from_entries`]: ../src/auth.rs
[`CapabilityClass`]: ../src/auth.rs
[`SessionIdentity`]: ../src/auth.rs
