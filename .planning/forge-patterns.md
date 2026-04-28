# Forge Patterns → Phantom Implementation Plan

Mined from deep analysis of `forge-gmh`. Every pattern listed here is a
**universal abstraction** that predates software — the implementation target
is Phantom, but the principle applies to any domain with autonomous agents,
sequential work, and human oversight gates.

---

## Pattern Index

| # | Pattern | Universal Abstraction | Phantom Target |
|---|---|---|---|
| 01 | [Background Reconciler](#01-background-reconciler) | OODA loop | `phantom-brain` |
| 02 | [Finite State Machine](#02-finite-state-machine) | Mealy FSM | `phantom-agents` |
| 03 | [2-Loop Decomposition](#03-2-loop-decomposition) | Stepwise refinement | `phantom-agents` |
| 04 | [Handoff Context Flow](#04-handoff-context-flow) | Baton relay / token passing | `phantom-agents` |
| 05 | [Journal as Event Log](#05-journal-as-event-log) | Write-ahead log / ledger | `phantom-memory` |
| 06 | [Flatline + Manual Retry](#06-flatline--manual-retry) | Circuit breaker / dead letter queue | `phantom-agents` |
| 07 | [Task DAG + Cycle Detection](#07-task-dag--cycle-detection) | Topological sort | `phantom-agents` |
| 08 | [Provider Catalog](#08-provider-catalog) | Strategy pattern / dependency injection | `phantom-brain` |
| 09 | [Skill Injection + Tracking](#09-skill-injection--tracking) | Instruction composition | `phantom-agents` |
| 10 | [Correlation ID Chaining](#10-correlation-id-chaining) | Causality token / Lamport clock | `phantom-agents` |
| 11 | [Orphan Process Recovery](#11-orphan-process-recovery) | Watchdog / dead man's switch | `phantom-supervisor` |
| 12 | [Plan Gate (Human Checkpoint)](#12-plan-gate-human-checkpoint) | Human-in-the-loop / CRM checklist | `phantom-app` |
| 13 | [Failure Preservation Branch](#13-failure-preservation-branch) | Forensic snapshot / black box | `phantom-agents` |
| 14 | [Auto-Approve Fast Path](#14-auto-approve-fast-path) | Trusted path / fast lane | `phantom-agents` |
| 15 | [Notification-as-Record](#15-notification-as-record) | Durable event / append-only log | `phantom-memory` |
| 16 | [Monotonic Sequence Clock](#16-monotonic-sequence-clock) | Logical clock / Lamport timestamp | `phantom-memory` |
| 17 | [Policy-per-Entity](#17-policy-per-entity) | Strategy-at-dispatch / rules engine | `phantom-agents` |
| 18 | [Lifecycle Hooks](#18-lifecycle-hooks) | Inversion of control / extension points | `phantom-agents` |
| 19 | [Prompt Persistence (DEBUG-01)](#19-prompt-persistence-debug-01) | Audit trail / reproducibility record | `phantom-agents` |
| 20 | [Desktop PATH Resolution](#20-desktop-path-resolution) | Environment normalization | `phantom` binary |
| 21 | [Plan Extraction via Sentinel Heading](#21-plan-extraction-via-sentinel-heading) | Structured output protocol | `phantom-agents` |
| 22 | [Disposition-Driven Behavior](#22-disposition-driven-behavior) | Role/intent tagging | `phantom-agents` |

---

## 01 — Background Reconciler

**Universal abstraction:** OODA loop (Observe-Orient-Decide-Act). Boyd, 1976.
Any autonomous system that must respond to changing world state without being
explicitly poked uses this pattern: observe current state, compute what should
happen, act, repeat.

**What Forge does:**
A background thread wakes every 2 seconds inside a `catch_panic` wrapper.
It reads DB state and drives all loop lifecycle transitions: dispatch unresolved
loops, check on running agents, recover orphans, chain pipeline tasks. Nothing
in the system waits to be called — the reconciler finds work and does it.

**Why the catch-panic wrapper matters:**
Any panic in the reconciler body is caught, logged, and the loop continues.
This is the same principle as Erlang supervisors: the loop itself must never die.

**Phantom implementation target:** `phantom-brain`

Current state: `phantom-brain` has an OODA loop concept but it's coupled to
scoring utility functions, not lifecycle management. The reconciler should be
a separate, dedicated background task in `phantom-brain` or a new
`phantom-orchestrator` crate.

**Implementation spec:**
```rust
// phantom-brain/src/reconciler.rs
pub async fn run_reconciler(state: Arc<AppState>) {
    loop {
        let result = std::panic::catch_unwind(|| {
            tick(&state);
        });
        if result.is_err() {
            tracing::error!("reconciler panic, continuing");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

fn tick(state: &AppState) {
    recover_orphans(state);       // stale PIDs after restart
    dispatch_unresolved(state);   // new agents waiting to run
    check_running(state);         // poll active agent exit status
    chain_pipeline(state);        // auto-dispatch next eligible task
}
```

---

## 02 — Finite State Machine

**Universal abstraction:** Mealy FSM. Every agent lifecycle has defined states
and legal transitions. Invalid transitions are rejected at the model level, not
at the caller level. This is the same principle used in TCP state machines,
order fulfillment systems, and patient care pathways.

**What Forge does:**
```
unresolved → planning → awaiting_review → approved → running → synced
                                                        ↓
                                                     flatline
```
`LoopState::can_transition_to()` is the single enforcement point. No caller
can skip states or go backwards except via `flatline → unresolved` (retry).

**Phantom implementation target:** `phantom-agents`

Current state: agent states exist but transitions are implicit in match arms
scattered across the codebase.

**Implementation spec:**
```rust
// phantom-agents/src/agent.rs
#[derive(Debug, PartialEq)]
pub enum AgentState {
    Idle,
    Planning,
    AwaitingApproval,
    Approved,
    Running,
    Synced,
    Flatline { reason: String },
}

impl AgentState {
    pub fn can_transition_to(&self, next: &AgentState) -> bool {
        matches!(
            (self, next),
            (Idle, Planning)
            | (Planning, AwaitingApproval)
            | (AwaitingApproval, Approved)
            | (AwaitingApproval, Planning)  // revision
            | (Approved, Running)
            | (Running, Synced)
            | (Running, Flatline { .. })
            | (Flatline { .. }, Idle)       // manual retry
        )
    }
}
```

---

## 03 — 2-Loop Decomposition

**Universal abstraction:** Stepwise refinement (Dijkstra/Wirth, 1971). Complex
problems are solved by first enriching the problem statement, then decomposing
the enriched statement — not by attempting decomposition on the raw input.

Applied to non-code domains: a law firm uses this when a partner first annotates
a client brief with legal constraints (synthesis), then an associate decomposes
it into billable tasks (decomposition). A surgeon does this: enrich the patient
chart with imaging findings, then plan the procedure steps.

**What Forge does:**
Instead of one "PRD → task list" prompt:
1. **Synthesis loop** (`disposition = synthesize`): AI enriches PRD with
   implementation hints, file paths, constraints → outputs **markdown**.
2. **Decompose loop** (`disposition = decompose`): AI reads enriched markdown
   → outputs **JSON task array**.

The reconciler auto-chains them: synthesis syncs → reconciler reads
`correlation_id` → creates decompose loop with enriched PRD as description.

**Why two loops are better than one:**
A single large decomposition prompt produces hallucinated JSON. Splitting into
two focused loops with explicit output formats (markdown, then JSON) is
dramatically more reliable. Each loop has a narrow mandate.

**Phantom implementation target:** `phantom-agents`

**Implementation spec:**
```rust
// phantom-agents/src/dispatch.rs
pub enum Disposition {
    Chat,
    Feature,
    BugFix,
    Refactor,
    Synthesize,   // enrichment pass
    Decompose,    // decomposition pass
    Audit,
}

// reconciler chains synthesize → decompose via correlation_id
fn chain_pipeline(state: &AppState) {
    let synced = state.agents.synced_with_disposition(Disposition::Synthesize);
    for agent in synced {
        if state.agents.decompose_pending_for(&agent.correlation_id).is_none() {
            let enriched_prd = agent.final_output();
            state.agents.spawn(AgentRequest {
                disposition: Disposition::Decompose,
                description: enriched_prd,
                correlation_id: agent.correlation_id.clone(),
                auto_approve: true,
            });
        }
    }
}
```

---

## 04 — Handoff Context Flow

**Universal abstraction:** Baton relay. Token passing in actor systems. Patient
handoff protocol in hospitals (SBAR: Situation-Background-Assessment-Recommendation).
When sequential agents do work, downstream agents receive structured context
from upstream agents — not raw output, but curated next-steps.

**What Forge does:**
When loop A completes:
1. `completion.rs` records a `handoffs` row: `final_diff_summary`, `next_steps`,
   `artifact_refs` (git branch, etc.)
2. Before dispatching loop B, reconciler reads `next_steps` from A's handoff
3. Writes those notes to B's `handoff_notes` column
4. B's prompt is prefixed with the handoff context

**Phantom implementation target:** `phantom-agents`, `phantom-memory`

**Implementation spec:**
```rust
// phantom-memory/src/lib.rs
pub struct Handoff {
    pub from_agent_id: AgentId,
    pub to_agent_id: Option<AgentId>,
    pub summary: String,
    pub next_steps: Vec<String>,
    pub artifacts: Vec<Artifact>,  // file paths, git refs, etc.
    pub timestamp: DateTime<Utc>,
}

// phantom-agents/src/completion.rs
pub fn record_handoff(agent: &Agent, output: &AgentOutput) -> Handoff {
    Handoff {
        from_agent_id: agent.id,
        to_agent_id: None,  // filled by reconciler when chaining
        summary: extract_summary(&output),
        next_steps: extract_next_steps(&output),
        artifacts: collect_artifacts(agent),
        timestamp: Utc::now(),
    }
}
```

---

## 05 — Journal as Event Log

**Universal abstraction:** Write-ahead log / double-entry bookkeeping / ship's
log. Every significant event is appended to an immutable, ordered record with
timestamp, phase, level, and actor. State is derived from the current snapshot
but the journal tells you *how you got there*.

Non-code: a surgical team writes every instrument in/out to an OR log. A
courtroom produces a transcript. A nuclear plant logs every valve position.

**What Forge does:**
Every agent output line is appended to `journal_entries`:
```
(loop_id, sequence, timestamp, phase, level, actor_type, message)
```
- `sequence` is monotonically increasing per loop (from `sequences` table)
- `phase` distinguishes planning/execution/completion/lifecycle events
- The full journal output is used to extract the plan via heading parsing

**Phantom implementation target:** `phantom-memory`

Files already exist: `phantom-memory/src/event_log.rs` — wire this up as the
authoritative journal for all agent activity.

**Implementation spec:**
```rust
// phantom-memory/src/event_log.rs
pub struct JournalEntry {
    pub agent_id: AgentId,
    pub sequence: u64,         // monotonic per-agent
    pub timestamp: DateTime<Utc>,
    pub phase: Phase,          // Planning | Execution | Completion | Lifecycle
    pub level: Level,          // Info | Warn | Error
    pub message: String,
}

pub enum Phase { Planning, Execution, Completion, Lifecycle }
```

---

## 06 — Flatline + Manual Retry

**Universal abstraction:** Circuit breaker + dead letter queue. When a system
hits a terminal failure, it stops automatically and waits for human judgment.
No automatic backoff loop that wastes resources or makes things worse.

Non-code: a circuit breaker in an electrical panel trips and stays off until
someone manually resets it. A hospital codes a patient and calls a code — it
doesn't auto-restart resuscitation attempts without a physician decision.

**What Forge does:**
`flatline` is a terminal state. Triggers:
- Agent exit code != 0 after max_attempts exhausted
- Spawn failure
- Pre/post hook failure
- Git merge conflict
- Stale orphan process (see Pattern 11)

Recovery: user explicitly calls `retry_loop()` → state = `unresolved`,
attempt = 0.

**Critical nuance:** failed work is preserved on `{branch}-failed-{exit_code}`
branch (Pattern 13). The flatline record tells you *why*, the branch tells you
*what was attempted*.

**Phantom implementation target:** `phantom-agents`

**Implementation spec:**
```rust
pub enum FlatlineReason {
    MaxAttemptsExceeded,
    SpawnFailed(String),
    PreHookFailed { exit_code: i32 },
    PostHookFailed { exit_code: i32 },
    GitConflict(String),
    OrphanRecovered,
    Timeout,
}

impl Agent {
    pub fn flatline(&mut self, reason: FlatlineReason) {
        self.state = AgentState::Flatline { reason: reason.to_string() };
        self.preserve_failure_artifacts();
        self.emit_notification(Notification::AgentFlatlined { agent_id: self.id });
    }

    pub fn retry(&mut self) {
        assert!(matches!(self.state, AgentState::Flatline { .. }));
        self.state = AgentState::Idle;
        self.attempt = 0;
    }
}
```

---

## 07 — Task DAG + Cycle Detection

**Universal abstraction:** Topological sort (Kahn's algorithm, 1962). Any
ordered dependency graph — build systems (Make, Bazel), package managers (npm,
cargo), CI pipelines, project management (predecessors in MS Project) — uses
this. Cycle detection prevents deadlock.

**What Forge does:**
Tasks store `depends_on` as comma-separated titles. Before dispatching a task,
`is_eligible()` checks all dependencies are `completed`. `has_cycle()` runs DFS
before any task group is executed. Cyclic task groups are immediately `blocked`.

**Phantom implementation target:** `phantom-agents`

**Implementation spec:**
```rust
// phantom-agents/src/spawn_rules.rs
pub struct TaskGraph {
    pub tasks: Vec<Task>,
}

impl TaskGraph {
    pub fn eligible_next(&self) -> Vec<&Task> {
        let completed: HashSet<_> = self.tasks.iter()
            .filter(|t| t.status == TaskStatus::Completed)
            .map(|t| t.id)
            .collect();

        self.tasks.iter()
            .filter(|t| t.status == TaskStatus::Ready)
            .filter(|t| t.depends_on.iter().all(|d| completed.contains(d)))
            .collect()
    }

    pub fn has_cycle(&self) -> bool {
        // DFS from each node, track path set
        let mut visited = HashSet::new();
        let mut path = HashSet::new();
        for task in &self.tasks {
            if dfs(task.id, &self.tasks, &mut visited, &mut path) {
                return true;
            }
        }
        false
    }
}
```

---

## 08 — Provider Catalog

**Universal abstraction:** Strategy pattern (GoF, 1994) + dependency injection.
The concrete algorithm (which AI model to use, how to invoke it) is selected
at runtime from a registry, not hardcoded. The caller only knows the interface.

Non-code: a hospital's formulary — doctors prescribe "anticoagulant" and the
pharmacy selects the specific drug based on patient insurance, availability, and
contraindications.

**What Forge does:**
`provider_profiles` table rows with `runtime_command`, `default_model`,
`models_json`, `api_key_setting`. Projects reference a `provider_id`.
Switching from Claude to Codex = update one FK.

Built-in profiles seeded in migration #2. Users can INSERT new rows.

**Phantom implementation target:** `phantom-brain`

Current state: model selection logic is in `phantom-brain/src/claude.rs` —
needs to become a general provider registry.

**Implementation spec:**
```rust
// phantom-brain/src/router.rs
pub struct ProviderProfile {
    pub id: String,
    pub name: String,
    pub runtime_command: String,   // e.g., "claude -p --dangerously-skip-permissions"
    pub default_model: String,
    pub available_models: Vec<String>,
}

pub struct ProviderCatalog {
    profiles: HashMap<String, ProviderProfile>,
}

impl ProviderCatalog {
    pub fn resolve(&self, provider_id: &str) -> &ProviderProfile {
        self.profiles.get(provider_id)
            .unwrap_or_else(|| self.profiles.get("claude-default").unwrap())
    }
}
```

---

## 09 — Skill Injection + Tracking

**Universal abstraction:** Instruction composition / operational briefing.
Military operations orders inject specific standing orders (ROE, comms protocols)
into mission briefings. Surgical checklists inject safety checks into procedure
starts. The instructions are modular, composable, and auditable.

**What Forge does:**
`build_launch_params()` assembles the final prompt by composing:
1. TDD skill (always injected)
2. Disposition-specific skill (feature/bug-fix/synthesize/decompose)
3. Knowledge graph reference (if available)
4. Guardrails + build instructions from Forgefile

Every injection is logged to `skill_injections` table:
```sql
(loop_id, skill_name, injected_at)
```
This decouples skills from prompts — you can inspect exactly what instructions
any agent received, even if the skill definition changes later.

**Phantom implementation target:** `phantom-agents`

**Implementation spec:**
```rust
// phantom-agents/src/system_prompt.rs
pub struct PromptBuilder {
    pub base: String,
    pub injections: Vec<SkillInjection>,
}

pub struct SkillInjection {
    pub name: String,
    pub content: String,
}

impl PromptBuilder {
    pub fn inject(&mut self, skill_name: &str, registry: &SkillRegistry) {
        if let Some(content) = registry.get(skill_name) {
            self.injections.push(SkillInjection {
                name: skill_name.to_string(),
                content: content.to_string(),
            });
        }
    }

    pub fn build(&self) -> String {
        let skills = self.injections.iter()
            .map(|i| i.content.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        format!("{}\n\n{}", skills, self.base)
    }

    // Persist injection record for audit
    pub fn record_injections(&self, agent_id: AgentId, memory: &Memory) {
        for injection in &self.injections {
            memory.log_skill_injection(agent_id, &injection.name);
        }
    }
}
```

---

## 10 — Correlation ID Chaining

**Universal abstraction:** Causality token / distributed tracing (Lamport, 1978).
A correlation ID threads through multiple independent agents that are logically
related. You can reconstruct the full causal chain from any agent's record by
following the token.

Non-code: a hospital encounter number follows a patient through ER → surgery →
ICU → discharge. Every record, every note, every order links to the same number.

**What Forge does:**
`anomalies.correlation_id` links the synthesize loop to the decompose loop.
When synthesis completes, the reconciler looks for a pending decompose loop
with the same `correlation_id`. If none exists, it creates one.

**Phantom implementation target:** `phantom-agents`

**Implementation spec:**
```rust
pub struct AgentRequest {
    pub id: AgentId,
    pub correlation_id: CorrelationId,  // UUID, same for all agents in a chain
    pub parent_id: Option<AgentId>,     // direct parent (for tree structure)
    pub disposition: Disposition,
    pub description: String,
}

// Enables: find all agents in this pipeline run
// query: WHERE correlation_id = ?
// Enables: find what spawned this agent  
// query: WHERE id = parent_id
```

---

## 11 — Orphan Process Recovery

**Universal abstraction:** Watchdog / dead man's switch. If a system crashes
mid-operation, the next startup must detect and triage in-progress work. Don't
pretend it completed; don't pretend it never started. Classify it.

Non-code: railway dead man's switch — if the engineer stops pressing the pedal
(crashes), the train brakes automatically. Aircraft black box records state so
the crash can be reconstructed.

**What Forge does:**
On reconciler tick, check all loops in `planning` or `running` state with a
`worker_pid`. If the PID no longer corresponds to a live process → orphan.
Transition to `flatline` with reason `"stale process after restart"`.

**Phantom implementation target:** `phantom-supervisor`

The supervisor already monitors Phantom's main process. Extend it to also
recover agent orphans when the main process restarts.

**Implementation spec:**
```rust
// phantom-agents/src/supervisor.rs
pub fn recover_orphans(state: &AppState) {
    let active = state.agents.in_states(&[
        AgentState::Planning, AgentState::Running
    ]);
    
    for agent in active {
        if let Some(pid) = agent.worker_pid {
            if !process_alive(pid) {
                agent.flatline(FlatlineReason::OrphanRecovered);
                tracing::warn!(
                    agent_id = %agent.id,
                    pid,
                    "orphaned agent recovered"
                );
            }
        }
    }
}
```

---

## 12 — Plan Gate (Human Checkpoint)

**Universal abstraction:** Human-in-the-loop / crew resource management (CRM)
checkpoint. Before committing to irreversible action, a structured review gate
requires human sign-off. Surgical time-out. Pre-flight checklist. Code review
before merge.

**What Forge does:**
Planning agent produces a plan with `## Tech Spec` and `## Implementation Plan`
markdown sections (Pattern 21). These are extracted into a `plans` row with
`review_status = 'pending'`. The reconciler halts until:
- Human approves → dispatch implementation
- Human requests revision → re-run planning agent with feedback appended
- Human rejects → cancel

**Auto-approve bypass:** synthesis/decompose dispositions skip the gate
(Pattern 14). The gate only fires for work that modifies user's codebase.

**Phantom implementation target:** `phantom-app`

The agent pane already has input/output areas. Add an approval widget that
surfaces the plan and a decision (approve/revise/reject).

---

## 13 — Failure Preservation Branch

**Universal abstraction:** Forensic snapshot / black box recorder. When a
system fails, preserve the exact state at time of failure. Don't discard it.
The failure artifact is the most valuable debugging artifact.

Non-code: NTSB preserves crash site before cleanup. Surgeons document
intraoperative complications in the op note, not just the outcome.

**What Forge does:**
On agent failure, before transitioning to flatline:
```
git add -A
git commit -m "wip(forge): failed loop {loop_id} (exit code {exit_code})"
```
Branch name: `{original_branch}-failed-{exit_code}`

The failed work is never discarded. Developer can `git checkout` to inspect
exactly what the agent produced before failing.

**Phantom implementation target:** `phantom-agents`

**Implementation spec:**
```rust
// phantom-agents/src/completion.rs
pub fn preserve_failure(agent: &Agent, exit_code: i32) {
    let failure_branch = format!("{}-failed-{}", agent.branch, exit_code);
    git::commit_all(&format!(
        "wip(phantom): failed agent {} (exit {})", agent.id, exit_code
    ));
    // branch exists for forensic inspection, not merged
    tracing::info!(branch = %failure_branch, "failure preserved");
}
```

---

## 14 — Auto-Approve Fast Path

**Universal abstraction:** Trusted path / fast lane. For operations that are
inherently safe or internal pipeline steps, skip the human gate. This isn't
bypassing safety — it's recognizing that not all decisions are equally
consequential.

Non-code: a surgeon's pre-op checklist requires attending sign-off; the
post-op wound care order does not. TSA PreCheck skips the full screening for
pre-vetted travelers.

**What Forge does:**
`disposition = synthesize` or `disposition = decompose` → auto_approve = true.
These loops manipulate the PRD document, not the user's codebase. The
consequential decision (approving the task group for execution) is a separate
gate.

**Phantom implementation target:** `phantom-agents`

**Implementation spec:**
```rust
impl Disposition {
    pub fn requires_human_review(&self) -> bool {
        !matches!(self, Disposition::Synthesize | Disposition::Decompose)
    }
}
```

---

## 15 — Notification-as-Record

**Universal abstraction:** Durable event / append-only notification log.
Notifications are not ephemeral toasts — they are records of significant state
transitions. You can query them, mark them read, trace what happened when.

Non-code: a bank statement is a durable notification record. Medical discharge
summary is a durable notification that the encounter ended.

**What Forge does:**
Every significant transition INSERT into `notifications` table, then
`emit("notification:new", payload)` to the UI. Notifications persist
(soft-delete via `read` flag). Frontend subscribes and refreshes on event.

**Phantom implementation target:** `phantom-memory`, `phantom-app`

**Implementation spec:**
```rust
// phantom-memory/src/lib.rs
pub struct Notification {
    pub id: NotificationId,
    pub kind: NotificationKind,
    pub title: String,
    pub message: String,
    pub agent_id: Option<AgentId>,
    pub read: bool,
    pub created_at: DateTime<Utc>,
}

pub enum NotificationKind {
    PlanReady,
    AgentRunning,
    AgentSynced,
    AgentFlatlined,
    PipelineCompleted,
    PipelineBlocked,
}
```

---

## 16 — Monotonic Sequence Clock

**Universal abstraction:** Logical clock (Lamport, 1978). Within any causal
chain, events must be orderable. A wall-clock timestamp can collide or be
out-of-order due to NTP drift. A per-entity monotonically incrementing sequence
number is always correct.

**What Forge does:**
`sequences` table: `(prefix TEXT, value INTEGER)`. Before inserting a journal
entry, increment `sequences` where `prefix = 'journal:{loop_id}'`. Store the
returned value as the entry's `sequence`. Entries are always orderable.

**Phantom implementation target:** `phantom-memory`

**Implementation spec:**
```rust
// phantom-memory/src/lib.rs
pub fn next_sequence(prefix: &str) -> u64 {
    // atomic increment, returns new value
    db.execute("INSERT INTO sequences (prefix, value) VALUES (?1, 1)
                ON CONFLICT(prefix) DO UPDATE SET value = value + 1", [prefix]);
    db.query_row("SELECT value FROM sequences WHERE prefix = ?1", [prefix])
}
```

---

## 17 — Policy-per-Entity

**Universal abstraction:** Strategy-at-dispatch / rules engine. Each unit of
work carries its own behavioral policy — how many retries, what timeout, which
hooks. This is not global config. It's per-entity, set at creation time,
immutable during execution.

Non-code: a hospital patient has a code status (DNR, full code) documented
at admission. That policy travels with the patient and governs every downstream
decision without requiring re-query.

**What Forge does:**
`anomalies.policy_json: {"max_attempts": 3, "timeout_seconds": 1800}`.
Set when the loop is created. Reconciler reads `max_attempts` before each
retry decision. `timeout_seconds` is stored but not yet enforced (known gap).

**Phantom implementation target:** `phantom-agents`

**Implementation spec:**
```rust
pub struct AgentPolicy {
    pub max_attempts: u32,
    pub timeout_seconds: u64,    // enforce this — Forge doesn't
    pub auto_approve: bool,
    pub skip_planning: bool,
}

impl Default for AgentPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            timeout_seconds: 1800,
            auto_approve: false,
            skip_planning: false,
        }
    }
}
```

**Phantom can go further:** enforce `timeout_seconds` via `tokio::time::timeout`.
Forge stores it but never checks it — this is a known gap we can close.

---

## 18 — Lifecycle Hooks

**Universal abstraction:** Inversion of control / Hollywood Principle ("don't
call us, we'll call you"). The orchestrator calls user-defined extension points
at predetermined moments. Users extend behavior without modifying core logic.

Non-code: a wedding ceremony has fixed slots where the officiant says "if
anyone objects, speak now." The ceremony doesn't change; the extension point
is always there.

**What Forge does:**
`projects.lifecycle_json`:
- `startup` — before anything runs
- `pre_workflow` — before agent spawns
- `post_workflow` — after agent succeeds (validation gate)
- `on_failure` — after agent fails

Any of these failing → loop flatlines. `post_workflow` is the validation gate:
if tests fail, the loop does not sync.

**Phantom implementation target:** `phantom-agents`

**Implementation spec:**
```rust
pub struct LifecycleHooks {
    pub startup: Option<String>,
    pub pre_workflow: Option<String>,
    pub post_workflow: Option<String>,
    pub on_failure: Option<String>,
}

impl Agent {
    async fn run_hook(&self, hook: &str) -> Result<()> {
        let status = Command::new("sh").arg("-c").arg(hook)
            .current_dir(&self.project_path)
            .status().await?;
        if !status.success() {
            return Err(HookFailed(hook.to_string(), status.code()));
        }
        Ok(())
    }
}
```

---

## 19 — Prompt Persistence (DEBUG-01)

**Universal abstraction:** Audit trail / reproducibility record. The exact
inputs that produced an output must be preserved. Without this, you cannot
reproduce failures, compare prompt variations, or audit what an agent was told.

Non-code: a pharmaceutical trial logs the exact drug batch number, dose, and
administration time for every patient. "We gave them the drug" is not sufficient.

**What Forge does:**
After `build_launch_params()` assembles the full prompt, it's written to
`anomalies.prompt_text`. Named "DEBUG-01" in the codebase. Can be retrieved
via `get_loop_prompt(loop_id)` for inspection.

**Phantom implementation target:** `phantom-agents`, `phantom-memory`

The final assembled prompt (base + all skill injections + handoff context +
planning phase wrapper) should be stored on the agent record before execution.

---

## 20 — Desktop PATH Resolution

**Universal abstraction:** Environment normalization. A process's runtime
environment is not guaranteed to match the user's interactive shell environment.
Normalize it before spawning any subprocess.

**What Forge does:**
On startup, `fix_path()` runs `zsh -ilc "echo $PATH"` to get the login shell's
real PATH. Replaces the process `PATH` env var. On Windows, manually appends
`%APPDATA%\npm` and `%LOCALAPPDATA%\npm` to handle global npm installs.

**Why this matters for Phantom:** When launched from a GUI (macOS app bundle,
Finder), Phantom inherits a minimal launchd PATH, not the user's zsh PATH.
`claude`, `node`, `python`, `cargo` may not resolve.

**Phantom implementation target:** `phantom` binary (startup code)

**Implementation spec:**
```rust
// phantom/src/main.rs (startup)
#[cfg(target_os = "macos")]
fn normalize_path() {
    if let Ok(output) = Command::new("zsh")
        .args(["-ilc", "echo $PATH"])
        .output() {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        std::env::set_var("PATH", path);
    }
}
```

---

## 21 — Plan Extraction via Sentinel Heading

**Universal abstraction:** Structured output protocol / output framing contract.
When an AI agent must produce structured output, don't use JSON schema
enforcement (brittle, breaks on large outputs). Instead, use a simple sentinel
pattern: specific markdown headings that the parser looks for.

**What Forge does:**
Planning agent is instructed to produce `## Tech Spec` and `## Implementation Plan`
sections in its response. The host parses journal output looking for these exact
headings. Content between headings becomes the plan record.

**Why this is better than JSON output:**
- Markdown is natural for LLMs; they rarely produce malformed headings
- JSON with embedded code blocks frequently breaks parsers
- The sentinel approach degrades gracefully (partial content is still useful)

**Phantom implementation target:** `phantom-agents`

**Implementation spec:**
```rust
pub fn extract_section(text: &str, heading: &str) -> Option<String> {
    let marker = format!("## {}", heading);
    let start = text.find(&marker)? + marker.len();
    let rest = &text[start..];
    // next ## heading terminates the section, or end of string
    let end = rest.find("\n## ").unwrap_or(rest.len());
    Some(rest[..end].trim().to_string())
}

pub fn extract_plan(journal_output: &str) -> Option<Plan> {
    Some(Plan {
        tech_spec: extract_section(journal_output, "Tech Spec")?,
        implementation_plan: extract_section(journal_output, "Implementation Plan")?,
    })
}
```

---

## 22 — Disposition-Driven Behavior

**Universal abstraction:** Role/intent tagging / semantic typing. Each unit of
work carries an intent label that governs which tools, skills, permissions, and
behaviors are activated. The same infrastructure (an agent loop) behaves
completely differently based on its disposition.

Non-code: a hospital patient's "level of care" designation (outpatient, inpatient,
ICU) governs which protocols apply, which staff are assigned, which equipment
is available — without changing the underlying hospital infrastructure.

**What Forge does:**
`anomalies.disposition` = `feature | bug-fix | refactor | chore | synthesize | decompose`

Disposition controls:
- Which skill is injected (Pattern 09)
- Whether human review gate fires (Pattern 14)
- Whether lifecycle hooks run (Pattern 18)
- Whether git branch is created
- Which post-workflow validation commands run

**Phantom implementation target:** `phantom-agents`

**Implementation spec:**
```rust
pub enum Disposition {
    Chat,       // no git, no planning gate, streaming response
    Feature,    // full lifecycle: plan → approve → implement → validate → merge
    BugFix,     // full lifecycle + diagnostic phase
    Refactor,   // full lifecycle, no new feature skills
    Chore,      // minimal lifecycle, no planning gate
    Synthesize, // enrichment only, no git, auto-approve
    Decompose,  // JSON output, no git, auto-approve
    Audit,      // read-only analysis, no git
    Inspect,    // screenshot + analysis, no git
}

impl Disposition {
    pub fn creates_branch(&self) -> bool {
        matches!(self, Feature | BugFix | Refactor | Chore)
    }
    pub fn requires_plan_gate(&self) -> bool {
        matches!(self, Feature | BugFix | Refactor)
    }
    pub fn runs_hooks(&self) -> bool {
        matches!(self, Feature | BugFix | Refactor | Chore)
    }
    pub fn skill(&self) -> &'static str {
        match self {
            Feature => FEATURE_SKILL,
            BugFix => BUGFIX_SKILL,
            Refactor => REFACTOR_SKILL,
            Synthesize => SYNTHESIZE_SKILL,
            Decompose => DECOMPOSE_SKILL,
            _ => BASE_SKILL,
        }
    }
}
```

---

## Implementation Sequence

These 22 patterns are not independent. Build in dependency order:

### Phase 1 — Foundation (no patterns work without these)
1. **Pattern 16** — Monotonic Sequence Clock (`phantom-memory`)
2. **Pattern 05** — Journal as Event Log (wires to sequence clock)
3. **Pattern 02** — Finite State Machine (all other patterns depend on valid states)
4. **Pattern 17** — Policy-per-Entity (governs retry/timeout throughout)

### Phase 2 — Agent Lifecycle
5. **Pattern 22** — Disposition-Driven Behavior
6. **Pattern 09** — Skill Injection + Tracking
7. **Pattern 19** — Prompt Persistence
8. **Pattern 21** — Plan Extraction via Sentinel Heading
9. **Pattern 12** — Plan Gate (Human Checkpoint)
10. **Pattern 14** — Auto-Approve Fast Path

### Phase 3 — Execution + Recovery
11. **Pattern 18** — Lifecycle Hooks
12. **Pattern 06** — Flatline + Manual Retry
13. **Pattern 13** — Failure Preservation Branch
14. **Pattern 11** — Orphan Process Recovery
15. **Pattern 01** — Background Reconciler (ties Phase 2+3 together)

### Phase 4 — Multi-Agent Coordination
16. **Pattern 10** — Correlation ID Chaining
17. **Pattern 04** — Handoff Context Flow
18. **Pattern 03** — 2-Loop Decomposition
19. **Pattern 07** — Task DAG + Cycle Detection

### Phase 5 — Infrastructure + Observability
20. **Pattern 08** — Provider Catalog
21. **Pattern 15** — Notification-as-Record
22. **Pattern 20** — Desktop PATH Resolution

---

## The Universal Principle Underneath All 22

Every pattern here is an answer to the same question:

> **How do you build a system that does complex, risky work autonomously while
> remaining debuggable, recoverable, and trustworthy?**

The answers cluster into four themes:
- **State integrity** (02, 17, 22) — never be in an undefined state
- **Observability** (05, 09, 15, 19, 16) — always know what happened and why
- **Recovery** (06, 11, 13) — every failure mode has a known recovery path
- **Coordination** (01, 03, 04, 07, 10) — sequential agents pass context, not
  just outputs

These themes are not software engineering inventions. They are engineering
principles that appear in aviation, medicine, nuclear power, and manufacturing
anywhere the cost of failure is high and the system must be trusted by humans
who cannot observe every internal decision.
