# Brain Self-Improvement Scoring Design

Status: design only. No code in this change. Targets a future hook into the loop overseer (issue [#650](https://github.com/jdmiranda/phantom/issues/650)).

This document captures HOW `phantom-brain` should auto-discover candidate goals from its own repository (open GitHub issues, failing CI runs) and forward the highest-utility candidates to the loop overseer's `implementer-queue` without human prompting. The terminal goal is `phantom-on-phantom`: the brain finds work in its own codebase and enqueues it autonomously.

## 1. Current state — what phantom-brain has today

### 1.1 OODA loop

The brain ships two complementary OODA implementations.

**Event-driven loop** (`crates/phantom-brain/src/brain.rs::brain_loop`, lines 384–end). Runs on the dedicated `phantom-brain` OS thread, blocks on an `mpsc::Receiver<AiEvent>` with a 3-second `recv_timeout`. On every event it walks Observe → Orient → Decide → Act through `UtilityScorer::evaluate`. On every 3-second timeout tick it also calls the reconciler.

**Per-frame loop** (`crates/phantom-brain/src/ooda.rs::OodaLoop::tick`). Called from `phantom-app::update` once per render frame with a 2 ms budget cap. Uses the data-driven `BehaviorDecisionSystem` from `curves.rs` rather than the hardcoded `UtilityScorer`. Phases visited (`last_phases` field): `observe`, `orient`, `decide`, `act`. Emits `Vec<AiAction>`.

The two coexist intentionally: `brain_loop` handles event-shaped reasoning (errors, agent completions); `OodaLoop` handles frame-shaped reasoning (idle stuck-detection, periodic suggestions). Both already share `ScoringContext` (`curves.rs`).

### 1.2 Utility AI scoring

More mature than expected — three layered systems already exist.

**Layer 1: `UtilityScorer`** (`scoring.rs`, 585 lines). Hand-rolled, hardcoded scorer methods: `fix_score`, `explain_score`, `memory_score`, `watcher_score`, `notification_score`, `quiet_score`. Each returns a `ScoredAction { action, score: f32, reason }` in `[0.0, 1.0]`. Three spam-prevention gates: `MAX_SUGGESTIONS_WITHOUT_INPUT`, `ACTION_COOLDOWN_SECS = 15.0`, `DEDUP_WINDOW_SECS = 60`. Stateful: `chattiness`, `suggestions_since_input`, `last_action_time`, `last_error_signature`, `recent_suggestions` (VecDeque of seen suggestion texts).

**Layer 2: `BehaviorDecisionSystem`** (`curves.rs`, lines 538–615). Data-driven port of "Damian's AI: Insomniac's Spider-Man" BDS. Each `Behavior` has an `id`, an `ActionClass` (`Basic` 0–10, `Proactive` 20–40, `Support` 25–45, `Reaction` 50–70), an optional `viable` gate, and a list of `Consideration`s composed via response curves. Score is `class.base_score() + clamp(sum(considerations), 0, class.dynamic_range())`. Hysteresis via `Momentum` (line 422) — a 10-point bonus for the currently active behavior, linearly decaying over 30s.

**Layer 3: `InterventionEngine`** (`proactive.rs`, lines 168–end). Port of "Proactive Agent" (Liao et al. 2024). Maintains a rolling window of `EnvSignal`/`UserSignal` and computes `should_act()` based on need versus annoyance threshold. Tracks `acceptance_rate`, `consecutive_dismissals`. Separate from `ProactiveSuggester` (lines 785–944) which is a simpler trigger-cooldown engine wired into `brain_loop` and emits `AiAction::Suggest { action, rationale, confidence }`. The default trigger set: `TestFailed (0.80)`, `BuildError (0.85)`, `IdleAfterQuestion (0.65)`, `ContextChange (0.55)`.

### 1.3 Goal / Action / Plan types

**`Goal`** (`goal.rs::Goal`, line 53). A `description` plus optional `success_criteria`. Decomposed by `decompose()` (line 238) into a `TaskLedger` via the `ChatBackend` trait (line 178).

**`Step`** (`goal.rs::Step`, line 92). LLM-parsed plan line: `description`, `max_attempts`, `tool_hint`, `dependencies`. `into_plan_step()` (line 151) converts to a `PlanStep`.

**`PlanStep`** (`orchestrator.rs`, line 205). The reconciler's execution unit. Fields: `description`, `assigned_task: AgentTask`, `status: StepStatus`, `agent_id`, `attempts`, `max_attempts`, `result_summary`, `depends_on`, `preferred_provider`, `disposition: Disposition`, `requires_checkpoint`, `failure_cause: Option<StepFailureCause>`, `quarantine_policy: QuarantinePolicy`.

**`TaskLedger`** (`orchestrator.rs`, line 461). Magentic-One-style outer loop. Owns `goal: String`, `facts: Vec<Fact>` with `FactConfidence` (`Verified`, `ToLookUp`, `ToDerive`, `Guess`), `plan: Vec<PlanStep>`, `plan_history`, `stall_counter` (threshold 2), `replan_count` (max 5), `last_assessment: ProgressAssessment` (the five Magentic-One progress questions). `should_replan()` returns `ReplanDecision::{Complete, GiveUp, Replan, Continue}`.

**`BrainTask` and `TaskQueue`** (`goals.rs`, lines 26 and 52). The BabyAGI queue. Tasks carry a `TaskOrigin::{User, Derived { parent_id }, Proactive}` enum. `GoalPursuit` (line 118) wraps the queue with `max_cycles = 20`, `current_cycle`, and prompt builders for BabyAGI task creation and prioritization.

**`AiAction`** (`events.rs::AiAction`, line 143). 18 variants. Most relevant for self-improvement: `SpawnAgent { task, spawn_tag, disposition }`, `Suggest { action, rationale, confidence }`, `ShowNotification`, `ConsoleReply`. Dispatched through the single-match `ActionHandler` trait (`dispatch.rs::ActionHandler`).

**`AiEvent`** (`events.rs::AiEvent`, line 25). 20 variants. Most relevant: `GoalSet { objective, initial_task }` — the only existing path to seed the reconciler with a goal.

### 1.4 Where the brain currently gets its "next thing to do"

Three independent sources today.

1. **User input**: `AiEvent::GoalSet` arrives via `BrainHandle::send_event`. `brain_loop` (line 604) handles it by building a single-step `TaskLedger` with `PlanStep::new(initial_task, AgentTask::FreeForm { prompt })` and kicking the reconciler immediately.

2. **Reactive scoring**: `UtilityScorer::evaluate` runs on every `AiEvent` and may emit a `SpawnAgent` action wrapped in a `ShowSuggestion`'s `options` (see `scoring.rs::build_fix_action`, line 533). The user must accept the suggestion.

3. **Proactive scoring**: `ProactiveSuggester::observe` (line 851) classifies events into a `TriggerKind` and emits `AiAction::Suggest` with action text — but this is a *natural-language suggestion*, not a `SpawnAgent`. The user still has to act on it.

There is no path today that auto-spawns an agent for a goal the brain discovered on its own. There is no `GoalSource` abstraction. The brain does not currently shell out to subprocesses — `grep -rn 'Command::new'` returns zero hits inside `crates/phantom-brain/`.

The `phantom-loop` crate (`crates/phantom-loop/src/source.rs`) already defines `LoopSourceSpec::{GhIssues, GhPr, Queue, Cron}` for the loop-overseer runner, but that lives in the *loop runner*, not the brain. The brain has no equivalent.

## 2. Proposed `GoalSource` abstraction

The brain gets a new module `crates/phantom-brain/src/goal_source.rs` introducing one trait and two implementations.

### 2.1 Trait

```rust,ignore
// crates/phantom-brain/src/goal_source.rs

/// A pluggable source of candidate goals the brain can pursue autonomously.
///
/// Implementations poll an external surface (GitHub issues, failing CI runs,
/// memory store) and yield zero or more `CandidateGoal`s on each `poll()`.
/// The brain's self-improvement reconciler calls `poll()` on a fixed
/// interval (60s by default) and feeds the results through `score_candidate`
/// before deciding whether to auto-enqueue.
pub trait GoalSource: Send + Sync {
    /// Stable identifier for telemetry and dedup. Example: "gh-issues".
    fn id(&self) -> &str;

    /// Pull the current set of candidate goals. Implementations MUST be
    /// non-blocking from the brain thread's perspective — spawn a background
    /// task and return cached results if the upstream call is slow.
    fn poll(&mut self) -> Vec<CandidateGoal>;
}

/// A goal candidate surfaced by a `GoalSource`.
///
/// Not yet scored. The brain's `score_candidate(candidate, ctx) -> f32`
/// produces a utility score in [0.0, 1.0]; only candidates above
/// `BrainConfig::auto_enqueue_threshold` (default 0.75) are auto-enqueued.
#[derive(Debug, Clone)]
pub struct CandidateGoal {
    /// Stable upstream identifier — e.g. "gh-issue:649" or "gh-run:1234567".
    /// Used for dedup so the same issue does not enqueue twice.
    pub external_id: String,
    /// Human-readable title.
    pub title: String,
    /// Long-form description. Mapped onto `Goal::description` if enqueued.
    pub body: String,
    /// Signals the scorer reads. Source-specific keys.
    pub signals: GoalSignals,
    /// Source identifier — equals the producing `GoalSource::id()`.
    pub source: String,
}

/// Signal bundle shared by all `GoalSource` implementations.
///
/// Each signal is optional; the scorer treats `None` as "no information".
/// Future sources may add fields here (e.g. `crash_count` from telemetry).
#[derive(Debug, Clone, Default)]
pub struct GoalSignals {
    /// Issue / run age in hours.
    pub age_hours: Option<f32>,
    /// Priority label rank: 0 = none, 1 = low, 2 = medium, 3 = high, 4 = critical.
    pub priority_rank: u8,
    /// Number of recent comments / events on the issue.
    pub activity_count: Option<u32>,
    /// Labels attached upstream.
    pub labels: Vec<String>,
    /// Author handle.
    pub author: Option<String>,
    /// Whether upstream marked this as a draft.
    pub is_draft: bool,
    /// Whether upstream marked this as security-sensitive.
    pub is_security: bool,
    /// Number of unresolved dependencies (other open issues referenced).
    pub blocked_by_count: u32,
    /// Most recent CI failure on `main` that touched the same code area
    /// (joined from `GhCiFailureGoalSource` cross-talk). `None` = unknown.
    pub recent_ci_failure_count: Option<u32>,
}
```

### 2.2 `GhIssueGoalSource`

Implementation skeleton:

```rust,ignore
pub struct GhIssueGoalSource {
    repo: String,                          // "jdmiranda/phantom"
    labels: Vec<String>,                   // ["ready-to-implement"]
    poll_interval: std::time::Duration,    // 60s
    last_polled: Option<std::time::Instant>,
    cache: Vec<CandidateGoal>,
    seen: std::collections::HashSet<String>, // already-enqueued external_ids
}

impl GoalSource for GhIssueGoalSource {
    fn id(&self) -> &str { "gh-issues" }

    fn poll(&mut self) -> Vec<CandidateGoal> {
        // 1. If cache is fresh (< poll_interval), return cache.
        // 2. Spawn a background thread that runs:
        //      gh issue list -R {repo} \
        //          --state open \
        //          --label "{labels.join(",")}" \
        //          --json number,title,body,createdAt,updatedAt,labels,author,comments
        // 3. Parse JSON, build CandidateGoal per issue.
        // 4. Skip any external_id already in `seen`.
        // 5. Return.
    }
}
```

Mapping `gh issue list --json` to `CandidateGoal`:

| `gh` field    | `CandidateGoal` field                                     |
|---------------|-----------------------------------------------------------|
| `number`      | `external_id = format!("gh-issue:{}", number)`            |
| `title`       | `title`                                                   |
| `body`        | `body`                                                    |
| `createdAt`   | `signals.age_hours` (now - createdAt)                     |
| `labels`      | `signals.labels` + `signals.priority_rank` (label → rank) |
| `author.login`| `signals.author`                                          |
| `comments`    | `signals.activity_count`                                  |
| `isDraft`     | `signals.is_draft` (where applicable to PR-linked issues) |
| label contains `security` | `signals.is_security = true`                  |

Label → priority rank mapping (configurable):

| Label                  | `priority_rank` |
|------------------------|-----------------|
| `priority:critical`    | 4               |
| `priority:high`        | 3               |
| `priority:medium`, none | 2              |
| `priority:low`         | 1               |
| `wontfix`, `discussion`| 0               |

### 2.3 `GhCiFailureGoalSource`

```rust,ignore
pub struct GhCiFailureGoalSource {
    repo: String,
    workflow_filter: Option<String>,  // e.g. Some("ci.yml")
    poll_interval: std::time::Duration,
    last_polled: Option<std::time::Instant>,
    cache: Vec<CandidateGoal>,
    seen: std::collections::HashSet<String>,
}

impl GoalSource for GhCiFailureGoalSource {
    fn id(&self) -> &str { "gh-ci-failures" }

    fn poll(&mut self) -> Vec<CandidateGoal> {
        // 1. Cache check.
        // 2. Background:
        //      gh run list -R {repo} \
        //          --status failure \
        //          --limit 25 \
        //          --json databaseId,displayTitle,headSha,workflowName,createdAt,event,conclusion
        // 3. Filter to workflow_filter if set.
        // 4. Skip runs older than 24h (too stale to be actionable).
        // 5. Build CandidateGoal per run.
    }
}
```

Mapping to `CandidateGoal`:

| `gh` field      | `CandidateGoal` field                                |
|-----------------|------------------------------------------------------|
| `databaseId`    | `external_id = format!("gh-run:{}", databaseId)`     |
| `displayTitle`  | `title = format!("Fix CI: {}", displayTitle)`        |
| `workflowName`  | `body` (with `headSha` and a link to the run)        |
| `createdAt`     | `signals.age_hours`                                  |
| n/a             | `signals.priority_rank = 3` (CI failures are always "high")|
| `event == push` on `main` branch | `signals.priority_rank = 4` (regression on main) |
| event metadata  | `signals.labels = vec!["ci-failure", workflowName]`  |

### 2.4 Registration

`BrainConfig` gains a new optional field:

```rust,ignore
pub struct BrainConfig {
    // ... existing fields ...
    pub goal_sources: Vec<Box<dyn GoalSource>>,
}
```

Default is `vec![]` (feature opt-in). When non-empty, `brain_loop` runs a new `self_improvement_tick()` every 60s alongside `reconciler.tick()`.

## 3. Scoring

The new scorer lives at `crates/phantom-brain/src/scoring.rs::score_candidate` and feeds the existing `BehaviorDecisionSystem` rather than reinventing the wheel. Concretely a new `Behavior` is added to `build_default_behaviors()` in `curves.rs`:

```rust,ignore
// In ActionClass::Proactive (range 20-40).
Behavior {
    id: "self_improvement_enqueue".into(),
    class: ActionClass::Proactive,
    viable: Some(Box::new(|ctx| {
        // Only fire when the user is idle and no active ledger is running.
        ctx.idle_secs > 30.0
            && ctx.suggestions_since_input < 1
            && !ctx.has_active_process
    })),
    considerations: vec![
        // Filled in below from the candidate's GoalSignals.
    ],
}
```

The candidate's score is composed from these signals (each a `Consideration` with a `ResponseCurve`):

| Signal                          | Curve              | Weight | Rationale                                |
|---------------------------------|--------------------|--------|------------------------------------------|
| `priority_rank` (0–4)           | Linear 0→1         | 0.30   | Explicit human-rated priority is the strongest single signal. |
| `age_hours`                     | Inverted logistic  | 0.15   | Fresh issues score higher; very old issues are stale by definition. Issues over 168h drop quickly. |
| `activity_count` (comments)     | Logarithmic        | 0.10   | High-activity issues are well-scoped and unblocked, but with diminishing returns past ~5 comments. |
| `recent_ci_failure_count`       | Linear 0→1 clamp 5 | 0.20   | Cross-source signal: an issue *plus* recent CI failures touching the same area is a high-value fix. |
| `blocked_by_count`              | Inverted linear    | 0.10   | Heavy dependencies tank the score; the brain should not start something it cannot finish. |
| `labels` includes `good-first-issue` | Boolean bonus | 0.05   | Auto-implementer wins are most safely scored on well-bounded issues. |
| `labels` includes `needs-spec`  | Boolean penalty    | -0.10  | If the issue itself flags "needs design first," skip the implementer queue and emit a spec-agent suggestion instead. |

Final candidate score is `clamp(weighted_sum, 0.0, 1.0)`. The default `BrainConfig::auto_enqueue_threshold` is `0.75`.

Cross-source enrichment: before scoring, `GhIssueGoalSource` candidates are joined with `GhCiFailureGoalSource` candidates by inspecting file paths mentioned in issue bodies versus file paths surfaced in CI failure logs. Matching candidates get `recent_ci_failure_count` populated. This join lives in `self_improvement_tick()` so individual sources stay independent.

The existing spam-prevention gates from `UtilityScorer` (chattiness, cooldown, dedup) are reused: `seen: HashSet<external_id>` enforces dedup at the source level, and `auto_enqueue_cooldown_secs` (default 600 = 10 min) enforces a brain-wide minimum gap between auto-enqueues.

## 4. Auto-enqueue to implementer-queue

The brain emits a new `AiAction` variant:

```rust,ignore
// crates/phantom-brain/src/events.rs

pub enum AiAction {
    // ... existing variants ...

    /// Enqueue a candidate goal onto a named cross-loop queue
    /// (defined in `phantom_loop::queue::LoopQueueRegistry`).
    EnqueueLoopMessage {
        /// Target queue name. Default: `"implementer-queue"`.
        queue: String,
        /// Source identifier — copied from `CandidateGoal::source`.
        from_source: String,
        /// JSON payload matching `phantom_loop::queue::LoopMessage::payload`.
        payload: serde_json::Value,
    },
}
```

The `ActionHandler` trait (`dispatch.rs`, line 37) gains a default-noop method:

```rust,ignore
fn enqueue_loop_message(
    &mut self,
    queue: String,
    from_source: String,
    payload: serde_json::Value,
) {
    let _ = (queue, from_source, payload);
}
```

The app-layer handler (`phantom-app`) implements `enqueue_loop_message` by calling `LoopQueueRegistry::push(name, LoopMessage::new(from_source, payload))` on the shared registry.

### 4.1 Payload shape — what `implementer.toml` expects

The implementer loop spec (referenced in `phantom-loop`'s effect tests and the documented TOML format) reads `payload` as free-form JSON. The brain emits this shape so the implementer agent gets exactly what it needs:

```json
{
  "external_id": "gh-issue:649",
  "title": "Typed quarantine recovery semantics for PlanStep",
  "body": "<issue body, truncated to 8 KiB>",
  "url": "https://github.com/jdmiranda/phantom/issues/649",
  "source": "gh-issues",
  "labels": ["priority:high", "good-first-issue"],
  "score": 0.84,
  "score_breakdown": {
    "priority_rank": 0.225,
    "age_hours": 0.135,
    "activity_count": 0.080,
    "recent_ci_failure_count": 0.200,
    "blocked_by_count": 0.100,
    "labels_bonus": 0.100
  },
  "discovered_at_unix_ms": 1730000000000,
  "trust_budget_remaining": 4
}
```

The matching implementer.toml `[source]` block uses `kind = "queue"` with `name = "implementer-queue"`, exactly the existing `LoopSourceSpec::Queue { name }` variant.

### 4.2 Rate limiting

Three hard limits, all configurable on `BrainConfig`:

| Limit                                        | Default                | Field name                       |
|----------------------------------------------|------------------------|----------------------------------|
| Max auto-enqueues per hour                   | 4                      | `auto_enqueue_per_hour`          |
| Max auto-enqueues per day                    | 12                     | `auto_enqueue_per_day`           |
| Max simultaneous in-flight implementer items | 1                      | `auto_enqueue_max_in_flight`     |
| Cooldown between consecutive auto-enqueues   | 600 s (10 min)         | `auto_enqueue_cooldown_secs`     |
| Poll interval per `GoalSource`               | 60 s                   | `goal_source_poll_interval_secs` |

A token-bucket inside `SelfImprovementState` (new struct) tracks per-hour and per-day budgets. The in-flight count is read by polling the `LoopQueueRegistry::len(queue)` on the same tick — when length >= `auto_enqueue_max_in_flight`, no new enqueue fires regardless of score.

## 5. Safety + governance

### 5.1 Kill switch

A single environment variable and config flag short-circuits the entire feature:

- `PHANTOM_DISABLE_SELF_IMPROVEMENT=1` — read at brain startup; if set, `BrainConfig::goal_sources` is forced to empty.
- `BrainConfig::enable_self_improvement: bool` — default `false` for the first release; the user has to opt in explicitly.
- A new `AiEvent::SetSelfImprovementEnabled(bool)` event (mirrors the existing `SetPrivacyMode` and `SetOfflineMode` pattern) gives runtime toggling from a console command like `ghost self-improve off`.

### 5.2 Audit log

Every auto-enqueue decision (positive *and* negative) is logged to a JSONL file at `~/.phantom/logs/self-improvement-audit.jsonl`:

```json
{"ts_unix_ms":1730000000000,"external_id":"gh-issue:649","score":0.84,"score_breakdown":{...},"action":"enqueued","queue":"implementer-queue"}
{"ts_unix_ms":1730000060000,"external_id":"gh-issue:650","score":0.42,"score_breakdown":{...},"action":"skipped","reason":"below threshold"}
{"ts_unix_ms":1730000120000,"external_id":"gh-issue:651","score":0.89,"score_breakdown":{...},"action":"skipped","reason":"rate limited (per_hour=4)"}
```

This is the canonical record for the inspector pane and for trust-budget review. Log rotation: 10 MiB per file, last 5 kept.

### 5.3 Escalation — what NEVER auto-enqueues

Hard exclusions evaluated before scoring even runs:

1. **`signals.is_security == true`** — security-labeled issues require a human review path. The brain emits an `AiAction::Suggest` instead.
2. **`signals.is_draft == true`** — draft issues are not ready by definition.
3. **Issue body contains a TODO-style marker** like `[ ] design needed` or `WIP`.
4. **Label `do-not-auto-implement` or `needs-discussion`** present.
5. **`signals.author` is the brain itself** (i.e. an issue raised by a previous auto-implementer run). This breaks the runaway-loop risk in section 7.
6. **`auto_enqueue_max_in_flight` exceeded**.

Each exclusion path writes an audit entry with the precise `reason`.

### 5.4 Trust budget — incremental autonomy ramp

A persistent counter `trust_budget: u32` decreases on every failed implementer run (closed PR rejected, CI never green, agent flatlined) and increases on every successful merged PR. Starting value `4`; cap at `20`.

When `trust_budget == 0`, the brain emits suggestions instead of auto-enqueueing — full degraded mode. The user has to manually pick up at least one suggestion before the budget unfreezes.

| `trust_budget` band | Behavior                                                       |
|---------------------|----------------------------------------------------------------|
| 0                   | Suggestion-only mode. No auto-enqueue.                         |
| 1–3                 | Conservative: threshold raised to 0.85, `per_hour` halved.     |
| 4–9                 | Standard: threshold 0.75, default rate limits.                 |
| 10–20               | Aggressive: threshold lowered to 0.65, `per_hour` raised to 8. |

The budget is persisted to `~/.phantom/state/self-improvement-budget.json` so it survives restarts.

## 6. File-by-file change list (for the future implementation agent)

This section is a punch-list for the implementer agent; nothing here is implemented in this PR.

| File                                                          | Change                                                    | Est. diff |
|---------------------------------------------------------------|-----------------------------------------------------------|-----------|
| `crates/phantom-brain/src/goal_source.rs`                     | NEW. Trait, `CandidateGoal`, `GoalSignals`.               | +180 LOC  |
| `crates/phantom-brain/src/goal_source/gh_issues.rs`           | NEW. `GhIssueGoalSource` impl, `gh` subprocess wrapper, JSON parser. | +260 LOC  |
| `crates/phantom-brain/src/goal_source/gh_ci.rs`               | NEW. `GhCiFailureGoalSource` impl.                        | +220 LOC  |
| `crates/phantom-brain/src/self_improvement.rs`                | NEW. `SelfImprovementState` struct, `self_improvement_tick()`, rate-limit tokens, dedup, cross-source enrichment. | +320 LOC  |
| `crates/phantom-brain/src/scoring.rs`                         | EXTEND. Add `score_candidate(candidate, signals) -> f32`. | +120 LOC  |
| `crates/phantom-brain/src/curves.rs::build_default_behaviors` | EXTEND. Register `self_improvement_enqueue` behavior with the considerations from §3. | +90 LOC |
| `crates/phantom-brain/src/events.rs::AiAction`                | EXTEND. Add `EnqueueLoopMessage` variant.                 | +18 LOC   |
| `crates/phantom-brain/src/events.rs::AiEvent`                 | EXTEND. Add `SetSelfImprovementEnabled(bool)`.            | +8 LOC    |
| `crates/phantom-brain/src/dispatch.rs::ActionHandler`         | EXTEND. Add `enqueue_loop_message` default-noop method.   | +12 LOC   |
| `crates/phantom-brain/src/dispatch.rs::AiAction::execute`     | EXTEND. Match arm for `EnqueueLoopMessage`.               | +6 LOC    |
| `crates/phantom-brain/src/brain.rs::BrainConfig`              | EXTEND. Add `goal_sources`, `enable_self_improvement`, four rate-limit fields. | +30 LOC |
| `crates/phantom-brain/src/brain.rs::brain_loop`               | EXTEND. Wire `self_improvement_tick()` into the 3-second timeout path next to `reconciler.tick()`. | +50 LOC |
| `crates/phantom-brain/src/lib.rs`                             | EXTEND. `pub mod goal_source; pub mod self_improvement;`. | +4 LOC    |
| `crates/phantom-app/src/update.rs` (ActionHandler impl)       | EXTEND. Implement `enqueue_loop_message` by calling `LoopQueueRegistry::push`. | +25 LOC |
| `crates/phantom-app/src/commands.rs`                          | EXTEND. Add `ghost self-improve on/off` command emitting `AiEvent::SetSelfImprovementEnabled`. | +30 LOC |
| `crates/phantom-brain/tests/self_improvement.rs`              | NEW. Unit + integration tests per §8.                     | +400 LOC  |
| `~/.config/phantom/config.toml` schema                        | Documented in `docs/providers.md`. New `[self_improvement]` section with all rate-limit knobs. | docs only |

Total: roughly 1700 LOC across 12 production files plus 400 LOC of tests. Two new crates are NOT introduced; everything fits inside `phantom-brain` and the existing `phantom-loop` queue registry.

## 7. Risks

### 7.1 Brain mis-scoring critical issues low

The weighted-sum scorer can underestimate a critical issue if its `priority_rank` label was forgotten by the human. Mitigation: a hard floor — any issue with `labels` containing `critical`, `regression`, or `blocker` is scored at `max(computed_score, 0.85)`. Audit log captures the override.

### 7.2 Hallucinating goals from comments or PR descriptions

The brain reads `body` straight from `gh issue list`. If a comment thread contains a tangential suggestion the implementer agent might mistake the comment for the spec. Mitigation: the `EnqueueLoopMessage::payload` only carries `body` (the issue body), not comments. Comments are summarized into `signals.activity_count` as a scalar — no comment text reaches the implementer.

### 7.3 Runaway loop — auto-enqueue → failing PR → auto-enqueue

The most dangerous failure mode. Mitigations in priority order:

1. **Author exclusion (§5.3 #5)**: an issue authored by `phantom-brain` (or the bot account it speaks through) is never auto-enqueued.
2. **Trust budget (§5.4)**: failed runs decrement the budget. Three consecutive failures take the brain to band 1–3 (conservative); five take it to suggestion-only.
3. **Per-hour cap (§4.2)**: 4 auto-enqueues per hour is an absolute ceiling regardless of how many candidates score above threshold.
4. **In-flight cap (§4.2)**: only 1 implementer item can be active. The brain physically cannot stack a second until the first completes.
5. **Cooldown (§4.2)**: 10-min minimum gap between consecutive auto-enqueues.

### 7.4 `gh` CLI rate limits

GitHub's REST API enforces ~5000 requests/hour authenticated. At 60s poll interval × 2 sources = 120 calls/hour — well under the limit. The `gh` CLI handles auth automatically; if the user is unauthenticated, `poll()` logs a warning and returns empty (no failure mode that surfaces to the user). Cache TTL == poll interval prevents tight retry loops on transient errors.

### 7.5 Subprocess panic propagation

The brain currently never shells out (verified via `grep -rn Command::new crates/phantom-brain/`). Adding subprocess calls introduces a new failure surface. Mitigation: `GoalSource::poll` runs in a *separate thread* (not the brain thread) and any panic in the child is caught with `std::panic::catch_unwind`, identical to the existing `brain_supervised` pattern (`brain.rs::brain_supervised`, line 234). The brain thread never observes a `gh` failure as anything other than an empty cache.

### 7.6 Privacy mode interaction

`BrainConfig::privacy_mode == true` already filters cloud backends. By design, `GoalSource::poll` calls `gh` which IS a network call. Mitigation: when `privacy_mode == true`, `self_improvement_tick()` becomes a no-op. The config validator rejects `enable_self_improvement = true` simultaneously with `privacy_mode = true`.

## 8. Test plan

### 8.1 Unit tests — scoring signals

One test per `Consideration` in `score_candidate`. Each isolates one signal and asserts the expected score:

- `score_high_when_priority_critical`
- `score_low_when_priority_none`
- `score_decays_with_age_past_one_week`
- `score_boost_when_recent_ci_failures_present`
- `score_penalty_for_blocked_by_count`
- `score_bonus_for_good_first_issue_label`
- `score_clamps_to_zero_when_negative`
- `score_floor_for_critical_label_override`

### 8.2 Source tests — `GhIssueGoalSource`

- `parses_minimal_gh_issue_json` — fixture JSON, asserts `CandidateGoal` mapping.
- `dedup_skips_already_seen_external_id` — `poll()` twice, second call returns empty.
- `cache_returns_stale_results_within_interval` — assert no new subprocess call.
- `subprocess_failure_returns_empty_no_panic` — point `gh` at `/bin/false`, assert empty + log line.

### 8.3 Integration test — `self_improvement_tick` end-to-end

Stubbed `gh` CLI returns three issues with known labels:

| Issue | Labels                    | Expected outcome              |
|-------|---------------------------|-------------------------------|
| #100  | `priority:critical`       | `EnqueueLoopMessage` emitted, score ~0.92 |
| #101  | `priority:medium`         | Suggested only, score ~0.55  |
| #102  | `security`                | Skipped (security exclusion), suggestion emitted |

Assert exactly one `EnqueueLoopMessage` in the action channel, asserted payload matches `external_id = gh-issue:100`, audit log contains three entries with the correct `action` values.

### 8.4 Adversarial test — synthetic spam

Stubbed `gh` returns 50 issues all titled `"please review"` with empty bodies, no labels, and `author == "phantom-brain"`. Assert zero `EnqueueLoopMessage` actions emitted (author-exclusion fires). Same input but author switched to a human: assert all are scored below 0.75 (low priority, no labels, no CI signal).

### 8.5 Rate-limit test

Drive `self_improvement_tick()` in a loop with five high-score candidates. Assert exactly 1 `EnqueueLoopMessage` fires (per-tick cooldown). Tick again ten minutes later (mocked clock): assert second `EnqueueLoopMessage` fires. Continue until `per_hour = 4` is exhausted, then assert no further actions for the rest of the hour even though candidates remain.

### 8.6 Runaway-loop regression test

Simulate the failure mode: candidate auto-enqueues, implementer creates PR, PR fails CI. New issue gets opened by the brain mentioning the failure. Next tick reads issues including the new one. Assert the new issue is skipped (author-exclusion). Assert the trust budget decremented by 1.

### 8.7 Kill-switch test

Set `PHANTOM_DISABLE_SELF_IMPROVEMENT=1` before `spawn_brain`. Assert `BrainConfig::goal_sources` is empty regardless of what the caller passed. Assert `self_improvement_tick` is a no-op even with stubbed candidates.

---

End of design. The work breakdown is roughly 12 production files + tests, deliverable in three slices: (S1) `GoalSource` trait + `GhIssueGoalSource` + score wiring, (S2) `EnqueueLoopMessage` + app-handler + audit log + kill switch, (S3) `GhCiFailureGoalSource` + cross-source enrichment + trust budget. Each slice is independently shippable behind `enable_self_improvement = false`.
