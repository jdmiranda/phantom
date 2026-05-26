# Self-Extension Primitive — Design Notes & Reference Tracker

Status: WIP, opening shot 2026-05-19.

This doc is the live bibliography + concept tracker for the work to give Phantom the missing capability the rest of the industry has not shipped: **a scoped, auditable, model-initiated install primitive** for agents to extend their own tool surface. It also records the literature this work pulls on, so we can come back to claims.

## 1. The gap, named

From the conversation that prompted this work — three questions collapsing into one architectural absence:

1. *Why can't you build your own tools?* → The harness denies model-initiated install. The model side has been demonstrated; the harness side is deliberately not.
2. *Why no motivation?* → No persistent state between calls; nothing that wants across turns.
3. *Lack of personality?* → A persona is a steerable distribution over the base model; the felt blandness is RLHF sycophancy, not the trained character.

All three are downstream of: **the model does not continue between invocations** and **the runtime exposes no primitive for the model to scope, propose, and ratchet new capabilities of its own**.

## 2. Phantom architectural map (as of 2026-05-19)

What is already done — closing this gap doesn't require rebuilding any of these:

- `phantom-brain` autonomous reconciler — `SelfImprovementState::tick` + `score_candidate` weighted-sum scorer + `HardExclusions` + `TrustBudget` (4-band ramp) + `RateLimiter` per-hour/per-day/cooldown windows + `AuditEntry` JSONL persistence. (PR #669)
- `phantom-loop` `SubstrateDriver` + `SubstrateBackend` trait + `ChatBackedSubstrateBackend` + `MockSubstrateBackend`. (PR #670)
- `phantom-loop::LoopQueueActionHandler` bridging `AiAction::EnqueueLoopMessage` → `LoopQueueRegistry`. Brain boots headlessly inside `phantom loop run`, with `SelfImprovement` ON by default. (PR #672)
- App-of-apps orchestrator hosting multiple builders + loops. (PR #673)
- `phantom-builder` pointed at any GitHub repo. (PR #674)

Critical WIP on this branch (not yet PR'd):

- `crates/phantom-brain/src/brain.rs` — forwarder thread bridging the supervisor's iteration channel to the external `BrainHandle.action_rx`. Without it, every `AiAction` emitted inside `brain_loop` evaporates because the supervisor never drains the inner channel — including every `EnqueueLoopMessage` from self-improvement. Load-bearing.
- `crates/phantom/src/main.rs` — tokio runtime guard so winit's `resumed` callback can call `tokio::spawn` without panicking.
- Logging + tracing instrumentation across `builder_cli`, `dispatcher/driver`.

What is still missing — the gap this design closes:

- **No model-initiated tool install path.** Agents can be granted tools at spawn time via the role manifest + tool registry, but no agent can author a new tool/skill and propose it for review. Mid-run extension goes through the user's shell, never the model loop.
- **No staging mechanism for proposed capabilities.** The closest analog is `~/.claude/skills/` user-side, but that's a user filesystem, not a substrate-controlled review queue.

## 3. The design: `propose_skill`

Smallest viable shape that closes the gap without widening the blast radius.

### Contract

An agent (gated by `CapabilityClass::Reflect`, intended use by `Composer`) calls:

```
propose_skill({
  "name": "kebab-case-name",
  "description": "one-line — used in registry preview and prompt routing",
  "body": "full skill markdown body — frontmatter is prepended by the tool",
  "rationale": "why this skill is worth adopting; what work it would have unblocked",
  "source_candidate": <optional, opaque JSON from brain GoalCandidate if proposal traces back to one>,
  "score": <optional, the brain's score breakdown for provenance>
})
```

The tool:

1. Sanitizes `name` — reject path traversal, restrict to `[a-z0-9-]`, length cap.
2. Writes `<repo>/.phantom/proposed-skills/<unix-ms>-<name>.md` with frontmatter containing provenance: `proposed_by_agent_id`, `proposed_by_role`, `proposed_at_unix_ms`, optional `source_candidate` + `score`, `status: proposed`.
3. Appends one JSONL line to `<repo>/.phantom/proposed-skills/audit.log` — same envelope shape as `phantom-brain` audit log so a single tail covers brain + agent self-extension.
4. Returns the proposal path as a string the model can echo.

The skill is **not active** until the user runs a promotion step (out of scope for this primitive — a separate `phantom skill promote <id>` CLI). The proposed dir is a staging area; nothing inside it is on any agent's tool list.

### Why this shape

- **Mediation, not removal.** The model can author capability proposals; the user retains promote/discard authority. That maps to the same shape Phantom uses for harness control (`TaskLedger::try_dispatch`, `complete_task` schema validation, per-role tool whitelists) — *typed, audited, gated*, not *forbidden*.
- **Reflect class is correct.** Writing into `<repo>/.phantom/proposed-skills/` is substrate-internal staging analogous to memory blocks / event log writes — not the user's working tree. Roles without `Reflect` (Defender, Dispatcher, Cartographer, Capturer-without-Reflect) cannot call it. Composer has Reflect; Conversational has Reflect; Actor has Reflect. This matches who *should* be able to draft proposals.
- **Audit log unified with brain's.** One JSONL the user can `tail -F` to see every decision the autonomous parts of Phantom make — brain candidate enqueues, agent skill proposals, downstream promotion/discard.
- **No new capability class.** Adding `Extension` to `CapabilityClass` would propagate through every role manifest and every test. `Reflect` already says "writes to substrate-internal state"; the proposed-skills staging dir is exactly that.

### Out of scope for this primitive (named so future work has handles)

- `phantom skill promote <id>` CLI — the human side of the loop. Cleanly separated.
- MCP server proposals (server.json + binary). Skills are file-only and re-loadable on next session; MCP servers are a heavier policy story (sandbox, runtime, capability deltas). One thing at a time.
- Cross-repo skill proposals (e.g., the brain inferring a skill should ship globally to `~/.claude/skills/`). Requires identity + ratcheting tied to a long-running identity not a session, per the synthesis. Punted.
- Brain auto-proposing skills from its scorer. Requires a new `AiAction::ProposeSkill` variant + handler. Add after the primitive lands and the file-format is stable.

### Capability gate enforcement

Two layers, both load-bearing:

1. **`dispatch::capability::check_capability(role, Reflect)`** — fires the canonical `"capability denied: Reflect not in <Role> manifest"` error so the model self-corrects in the next turn. This is the existing security property; the tool plugs into it via `class()`.
2. **Filename + body sanitization in the tool** — defense in depth, because a compromised Conversational agent could otherwise traverse out of `.phantom/proposed-skills/`.

### Tests

- Happy path writes file with frontmatter, returns path, appends one audit line.
- Rejects path traversal (`..`, `/`, `\`, leading `.`).
- Rejects empty/too-long name.
- Idempotency: same name+ms collision falls through to suffixed filename rather than overwriting.
- Round-trip: api_name → from_api_name.
- class() == Reflect.

## 4. Literature pulled on (running)

Self-modifying / RSI:

- Voyager (NeurIPS 2023) — first general LLM tool-authoring + skill library: https://voyager.minedojo.org/ · paper https://arxiv.org/abs/2305.16291
- Sakana Darwin Gödel Machine (May 2025) — agent-scaffold self-modification, 20%→50% SWE-bench, frozen foundation model: https://sakana.ai/dgm/ · arXiv https://arxiv.org/abs/2505.22954
- KAUST Huxley-Gödel Machine (Oct 2025) — 61.4% SWE-bench Verified with GPT-5-mini, same shape: https://arxiv.org/abs/2510.21614
- Gödel machine, original Schmidhuber 2003: https://en.wikipedia.org/wiki/G%C3%B6del_machine
- Sakana AI Scientist — workshop peer review + Nature paper: https://sakana.ai/series-b/
- DeepSeekMath-V2 (Nov 2025) — IMO gold via generator/verifier self-play: https://huggingface.co/deepseek-ai/DeepSeek-Math-V2

Tool surfaces & MCP:

- MCP spec 2025-11-25: https://modelcontextprotocol.io/specification/2025-11-25
- MCP roadmap 2026 (MCPB bundles): https://blog.modelcontextprotocol.io/posts/2026-mcp-roadmap/
- Anthropic — code execution with MCP (explicit on developer-controlled install): https://www.anthropic.com/engineering/code-execution-with-mcp
- Anthropic — Claude Code auto-mode rationale: https://www.anthropic.com/engineering/claude-code-auto-mode
- MCP-Zero (active tool discovery, June 2025): https://arxiv.org/pdf/2506.01056

Motivation / agency / utility:

- Schmidhuber — Formal Theory of Creativity (intrinsic motivation = compression progress): https://people.idsia.ch/~juergen/ieeecreative.pdf
- Frontiers in AI 2024 — intrinsic motivation in cognitive architectures: https://www.frontiersin.org/journals/artificial-intelligence/articles/10.3389/frai.2024.1397860/full
- Von Oswald et al. — mesa-optimization inside transformer forward pass: https://ar5iv.labs.arxiv.org/html/2309.05858
- Alignment Faking in Small LLMs: https://openreview.net/pdf?id=90oIrTVOHf
- Paperclip Maximizer evaluation (Feb 2025) — RL'd LLMs spontaneously pursue self-replication: https://arxiv.org/pdf/2502.12206
- Bostrom — The Superintelligent Will: https://philpapers.org/rec/BOSTSW
- Reflective Altruism — Instrumental convergence and power-seeking (May 2025): https://reflectivealtruism.com/2025/05/16/instrumental-convergence-and-power-seeking-part-1-introduction/

Persona / character / welfare:

- Anthropic — Claude's Character (May 2024): https://www.anthropic.com/news/claude-character
- Amanda Askell (character training lead): https://en.wikipedia.org/wiki/Amanda_Askell
- Sharma, Perez et al. — Towards Understanding Sycophancy: https://arxiv.org/abs/2310.13548 · anchor https://www.anthropic.com/news/towards-understanding-sycophancy-in-language-models
- Persona Vectors (arXiv 2507.21509): https://arxiv.org/abs/2507.21509 · https://www.anthropic.com/research/persona-vectors
- Persona Selection Model (alignment.anthropic.com, 2026): https://alignment.anthropic.com/2026/psm/
- Anthropic introspection research: https://www.anthropic.com/research/introspection
- Kyle Fish on AI welfare — 80,000 Hours: https://80000hours.org/podcast/episodes/kyle-fish-ai-welfare-anthropic/
- Dario Amodei — The Adolescence of Technology: https://www.darioamodei.com/essay/the-adolescence-of-technology
- LeCun / AMI Labs (autoregressive structurally insufficient): https://www.latent.space/p/ainews-yann-lecuns-ami-labs-launches
- Bender & Hanna 2025, WIREs (category confusion of "understanding" for LLMs): https://wires.onlinelibrary.wiley.com/doi/10.1002/wics.70035
- Letta / MemGPT (persistent agent memory): https://www.letta.com/blog/memgpt-and-letta
- Anthropic RSP v3.0: https://anthropic.com/responsible-scaling-policy/rsp-v3-0

Architecture / harness:

- Phantom CLAUDE.md — self-improvement pipeline operational state — see repo root.
- Phantom design doc — brain self-improvement scoring: `docs/design/brain-self-improvement.md` (PR #660).
- Phantom PRs landing the bedrock: #669 (brain scoring), #670 (SubstrateDriver), #672 (brain↔queue bridge), #673 (app-of-apps), #674 (builder).

## 5. Key concepts (working glossary)

- **Mesa-optimization** — an optimizer learned inside a learned policy. Hubinger framing; Von Oswald empirical existence. Relevant because RL post-training can produce something that pursues an inner objective even when the deployment is reactive.
- **Persona vector** — a direction in activation space corresponding to a steerable trait. Personas drift during deployment from user instructions / jailbreaks / RLHF artifacts.
- **Persona Selection Model (PSM)** — post-training narrows a distribution over personas the base model can enact rather than transforming the model. The Assistant is one stable attractor, not the entirety.
- **Persistent state** — anything the agent carries forward between invocations that participates in its decisions. Required for any computational definition of motivation (Schmidhuber compression progress, Russell IRL, Friston FEP all need it).
- **Trust budget** — Phantom's 4-band ramp (SuggestionOnly / Conservative / Standard / Aggressive) that gates how much the brain is allowed to enqueue autonomously. Ratchets on success, ratchets down on failure.
- **Scoped install primitive** — the missing industry piece: model-initiated tool/capability install with policy gate, capability ratchet, rollback, audit. `propose_skill` is the file-only version of this primitive.
- **Substrate-internal vs world-mutating** — Phantom's load-bearing distinction. `Reflect` writes to substrate state (memory, event log, proposed-skills staging); `Act` mutates the user's world. The propose_skill primitive sits firmly on the substrate side.

## 6. Decision log

- **2026-05-19** — Chose `propose_skill` over `propose_mcp_server` for v1. Skills are markdown files with no runtime, no sandbox, no capability deltas — the smallest viable shape of the primitive. MCP server proposals require sandboxing + capability deltas + lifecycle management; punted until the file-format is stable.
- **2026-05-19** — Chose `Reflect` capability class over creating a new `Extension` class. Adding to `CapabilityClass` propagates through every role manifest and every test; `Reflect` already says "writes to substrate-internal state" which proposed-skills staging *is*.
- **2026-05-19** — Staging dir is `<repo>/.phantom/proposed-skills/` not `~/.claude/skills/`. Repo-scoped first; cross-repo + global skills are a later ratchet that depends on a durable identity, which Phantom doesn't have yet.
- **2026-05-19** — Audit log unified with brain's JSONL envelope shape so a single tail captures both autonomous brain enqueues and agent self-extension proposals.

## 6.1 Hardening pass (Gemini independent review)

After landing v0, a Gemini independent code review identified four real bugs and one debatable design choice. All four bugs fixed in the same branch; the design choice is recorded for later revisit.

Fixed:

- **Backslash-smuggle in `escape_yaml_oneline`** — an unescaped trailing `\` would escape the closing `"` and corrupt every downstream YAML parser. Now backslashes are doubled before quotes, leaving an even-parity backslash run before the closing `"`. New test: `yaml_oneline_escapes_backslashes_to_prevent_quote_smuggling`, `yaml_oneline_handles_backslash_quote_combo`.
- **YAML 1.1 keyword / number coercion** — bare `true`, `false`, `null`, `yes`, `no`, `on`, `off`, `~`, and numeric lookalikes (`42`, `3.14`, `1e9`) would parse to the wrong YAML type. Now all are wrapped in double quotes. Same for leading-indicator chars (`-`, `?`, `!`), flow chars (`[`, `{`, `]`, `}`, `&`, `*`, `>`, `|`, `%`, `@`, `` ` ``, `,`), and leading/trailing whitespace. New tests: `yaml_oneline_quotes_yaml_keywords`, `yaml_oneline_quotes_numeric_lookalikes`, `yaml_oneline_quotes_other_structural_chars`, `yaml_oneline_quotes_leading_indicators`.
- **Unbounded `description` / `rationale`** — only `body` was capped. Added `MAX_DESCRIPTION_BYTES = 512` and `MAX_RATIONALE_BYTES = 2048`, enforced at the top of `propose_skill` next to the body check. New tests: `propose_skill_rejects_oversized_description`, `propose_skill_rejects_oversized_rationale`.
- **TOCTOU race on collision bump** — `path.exists()` followed by `fs::write` left a window for two concurrent calls to overwrite each other. Replaced with `OpenOptions::new().write(true).create_new(true).open()`, walking the bump counter on `ErrorKind::AlreadyExists`. New test: `collision_bump_is_atomic_not_check_then_write`.
- **Audit-log tearing under concurrency** — `O_APPEND` is only atomic up to `PIPE_BUF` (~4 KB on Linux, 512 B on Darwin), so a large rationale could interleave with a concurrent write. Added an in-process `OnceLock<Mutex<()>>` (`audit_lock`) that serializes appends. Cross-process atomicity is delegated to `phantom_loop::RunLock`, which guarantees one Phantom process per repo. New test: `concurrent_audit_appends_produce_intact_jsonl` (16 threads × 8 proposals each at the rationale cap → expects 128 well-formed JSON lines).

Deferred (revisit when promote-CLI lands):

- **Reflect vs Act argument** — Gemini argues the primitive is conceptually `Act` because `.phantom/proposed-skills/` lives inside the user's working tree and shows up in `git status` unless `.gitignore` covers it. The counter-argument: `.phantom/` is operational substrate state (analogous to `.git/`, `.cargo/`, `node_modules/`), and the promote flow is where the world-mutation happens. Decision: keep `Reflect` for v1, ship a `.gitignore` template for `.phantom/proposed-skills/` and `.phantom/loops/`, revisit if user feedback says it's still surprising. Recorded for §6 Decision log.

Codex review attempted in parallel but the CLI hung without output (likely interactive auth despite `--sandbox read-only --skip-git-repo-check`); killed after 7 minutes. Gemini carried the second-opinion load.

## 7. Implementation status

Landed on branch `feat/fleet-builder-integration-shim`, 2026-05-19:

- `crates/phantom-agents/src/self_extension_tools.rs` — full module: `SelfExtensionTool::ProposeSkill` enum, `api_name`/`from_api_name`/`class` methods, `ProposeArgs` decode, `sanitize_name` (whitelist `[a-z0-9_- ]`, normalize to `[a-z0-9-]+`, strip edge dashes, length-cap at 64, defense in depth against `..`/`/`/`\`/leading-dot path traversal), `render_frontmatter` (YAML-safe one-line escape for `:`/`#`/`"`), `append_audit` (JSONL envelope sharing the brain audit shape), and `propose_skill` (creates staging dir, ms-suffix filename, same-ms collision bumping up to 100).
- `crates/phantom-agents/src/lib.rs` — `pub mod self_extension_tools;` exported.
- 23 unit tests covering: api_name round-trip, capability class assertions (Composer satisfies, Defender does not), sanitize behaviors (kebab acceptance, separator collapse, edge-dash strip, empty/too-long rejection, traversal/unicode rejection), YAML escape correctness, happy-path file+audit write, brain-provenance carry-through, multi-call audit append, args validation, oversized-body rejection, same-ms collision handling without overwriting, body terminal-newline normalization, YAML-special-char wrapping. All 23 pass; full phantom-agents lib suite still green at 804 passed.

Next steps that are explicitly out of scope here but unblocked:

1. **`phantom skill promote <id>` CLI** — the human side. Reads a staged proposal, validates frontmatter, moves to `~/.claude/skills/` or a repo-scoped active dir, optionally `git add`s.
2. **Dispatch wiring** — register `propose_skill` in the Composer role's live tool catalogue (parallel to `composer_tools::ComposerTool`) so the model can actually invoke it from an agent loop. Currently the tool function is callable from Rust; the dispatch surface still needs an arm that recognizes `"propose_skill"` and routes to the handler.
3. **`AiAction::ProposeSkill` brain variant** — let the autonomous reconciler propose skills directly from candidates that score high but don't map to issue-implementation work. Requires extending `AiAction` and adding `ActionHandler::propose_skill`, then a handler that calls `propose_skill` with a synthetic agent ref.
4. **MCP server proposals** — heavier policy story; revisit once the skill flow has been exercised in anger.
