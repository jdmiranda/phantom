# Federation + Speech

[← back to components index](README.md)

> Multi-instance + relays + speech I/O.

## Status

<span class="chip warn">stubbed</span> for most crates. Federation
infrastructure is wired (handshakes, message envelopes) but no
production deployment exists yet. Speech is similarly stubbed —
backend abstractions exist; live STT/TTS integration is pending.

## What it does

Two adjacent concerns:

1. **Federation** — Phantom-to-Phantom communication. Phase 9 of the
   roadmap. Identity + relay + hub + fleet.
2. **Speech** — STT (microphone → text → NLP) + TTS (agent reply →
   audio).

## Crates

### `phantom-net` <span class="chip warn">stubbed</span>

Identity bootstrap, relay handshake, opaque message envelope, and
heartbeat keepalive for Phantom federation (issue #5).

- `PeerId` — typed identity.
- `Envelope` — opaque encrypted payload routed by `PeerId`.
- Handshake protocol skeleton.

### `phantom-relay` <span class="chip warn">stubbed</span>

Stateless WebSocket message broker routing opaque envelopes by `PeerId`
(issue #4).

- `Relay` — broker process (planned).
- `CapabilityClass` — local copy (see
  [gap-capability-class-propagation](../gaps.md#gap-capability-class-propagation)).
- Grant types for inter-instance authorization.

### `phantom-hub` <span class="chip warn">stubbed</span>

Railway-hosted MCP fleet control hub: connection broker, auth, and
JSON-RPC router (issue #394).

- `Hub` — hosted broker for "make Phantom remotely controllable by any
  AI."
- `auth::CapabilityClass` — local copy (see same gap).
- JWT-based identity.

### `phantom-fleet` <span class="chip warn">stubbed</span>

Multi-instance orchestration. The "run Phantom on 10 nodes, have them
collaborate" layer. Spec defined; not yet running.

### `phantom-builder` <span class="chip warn">stubbed</span>

Workspace assembly / provisioning. Recently wired (PR #675 — "wire
builder integration shim — AppKind::Builder now active"). Owns
`AppKind::Builder`; converts a `BuilderManifest` into a running
workspace.

### `phantom-stt` <span class="chip warn">stubbed</span>

Speech-to-text backend abstraction (Whisper / Deepgram traits + mock).
Streaming Whisper + OpenAI integration pending (issues #56, #68).

- `SttBackend` trait — `transcribe(audio) -> Stream<Result<Transcript>>`.
- `MockSttBackend` for tests.
- `WhisperBackend` skeleton.

### `phantom-voice` <span class="chip warn">stubbed</span>

Text-to-speech backend abstraction (ElevenLabs / Piper / system-TTS
traits + mock). Real TTS pending (issue #69).

- `TtsBackend` trait — `synthesize(text) -> Stream<AudioFrame>`.
- `MockTtsBackend` for tests.
- `OpenAiTtsBackend` ships in `phantom-app::tts` (calls OpenAI's TTS
  endpoint) but the abstraction is centralized here.

### `phantom-audio` <span class="chip info">future-shipping</span>

> **Note:** `phantom-audio` exists in the working tree at
> [`crates/phantom-audio/Cargo.toml`](../../../../crates/phantom-audio/Cargo.toml) but is
> **NOT** a workspace member (absent from root `Cargo.toml`'s
> `[workspace.members]`). It's a scratch directory. When it lands as a
> workspace member, it'll provide the OS-level audio device abstraction
> that STT (microphone capture) and TTS (speaker output) currently get
> via ad-hoc per-backend code.

## Owns

- Phantom-to-Phantom identity (`PeerId`) + envelope routing
- Hub auth + JSON-RPC routing
- STT / TTS backend trait abstractions
- Builder workspace assembly pipeline
- (Future) OS audio device abstraction (`phantom-audio`)

## Reads from

| Source | What |
|---|---|
| OS audio devices | microphone PCM frames (when wired) |
| External STT API (Whisper / Deepgram) | transcripts |
| External TTS API (ElevenLabs / OpenAI / Piper) | audio frames |
| Hub WebSocket | inter-instance messages |
| Relay WebSocket | per-peer routing |

## Writes to / publishes

| Target | What |
|---|---|
| OS audio output | TTS frames |
| `nlp.intent` topic | STT-derived intent |
| `agent.*` topic | TTS playback state |
| Hub | identity + capability advertisements |
| Other Phantom instances (via relay) | typed `Envelope`s |

## Decisions honoured

(none yet specific to federation — Phase 9 work)

## Open gaps

- [gap-capability-class-propagation](../gaps.md#gap-capability-class-propagation)
  — `phantom-relay::CapabilityClass` + `phantom-hub::CapabilityClass`
  are independent copies of the `phantom-agents::role::CapabilityClass`
  shape.

## Source files

| Concept | File |
|---|---|
| Net identity + envelope | [`crates/phantom-net/src/lib.rs`](../../../../crates/phantom-net/src/lib.rs) |
| Relay broker | [`crates/phantom-relay/src/lib.rs`](../../../../crates/phantom-relay/src/lib.rs) |
| Hub broker | [`crates/phantom-hub/src/lib.rs`](../../../../crates/phantom-hub/src/lib.rs) |
| Fleet orchestration | [`crates/phantom-fleet/src/lib.rs`](../../../../crates/phantom-fleet/src/lib.rs) |
| Builder | [`crates/phantom-builder/src/lib.rs`](../../../../crates/phantom-builder/src/lib.rs) |
| STT backend abstraction | [`crates/phantom-stt/src/lib.rs`](../../../../crates/phantom-stt/src/lib.rs) |
| TTS backend abstraction | [`crates/phantom-voice/src/lib.rs`](../../../../crates/phantom-voice/src/lib.rs) |
