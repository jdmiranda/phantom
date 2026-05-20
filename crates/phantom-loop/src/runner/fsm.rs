//! The [`LoopRunner`] state machine.
//!
//! One runner drives one [`crate::LoopSpec`] to completion. It owns the
//! source, the dispatcher, the queue registry, and the compiled exit
//! schema; per-iteration state lives in [`LoopState`].
//!
//! # State diagram
//!
//! ```text
//!     ┌──────────────────────────┐
//!     │           Idle           │ ◄────────────── (initial)
//!     └─────────────┬────────────┘
//!                   │ enter loop
//!                   ▼
//!     ┌──────────────────────────┐
//!     │         Pulling          │
//!     └─────────────┬────────────┘
//!         Empty │   │ Available           │ Done/Error
//!     (back off)│   ▼                     ▼
//!     ◄─────────┘ ┌──────────────────┐  ┌──────────┐
//!                 │   Dispatching    │  │ Stopped  │ (terminal)
//!                 └──────┬───────────┘  └──────────┘
//!                        │  agent? yes
//!                        ▼
//!                 ┌──────────────────┐
//!                 │     Awaiting     │ ── result ──► Validating
//!                 └──────────────────┘
//!                                            invalid + Park / FailAndStop ──► Stopped
//!                                            invalid + SkipAndContinue    ──► Pulling
//!                                            valid                        ──► (run effects, then Pulling)
//! ```
//!
//! Agentless specs (no `[agent]` in TOML) skip `Awaiting` / `Validating`:
//! `Dispatching` runs the effects directly with a `null` result payload.
//!
//! # Async shape
//!
//! [`LoopRunner::run`] is an async fn that drives the FSM via straight-line
//! control flow. The only real `.await` is on
//! [`crate::runner::dispatcher::DispatchHandle::completion_rx`] inside the
//! `Awaiting` step and on `tokio::time::sleep` inside the `Empty` backoff.
//! Tests pause the tokio clock to deterministically fast-forward the
//! latter.

use std::sync::Arc;
use std::time::{Duration, Instant};

use phantom_agents::agent::AgentId;
use serde_json::Value;

use crate::effect::LoopEffect;
use crate::effect_runner::{EffectContext, EffectError, run_effects};
use crate::exit::ExitSchema;
use crate::queue::LoopQueueRegistry;
use crate::runner::dispatcher::{AgentDispatcher, DispatchError};
use crate::runner::source::{LoopContext, LoopInput, LoopPullResult, LoopSource};
use crate::spec::{LoopQuarantinePolicy, LoopSpec};

/// How long the runner sleeps when [`LoopSource::next`] returns
/// [`LoopPullResult::Empty`] before re-polling.
///
/// Deliberately short — the loop overseer is a foreground CLI in MVP, not a
/// long-lived daemon; we'd rather burn a few extra polls than sit on
/// available work. C3's CLI can promote this to a config knob if needed.
const IDLE_BACKOFF: Duration = Duration::from_millis(100);

/// Max consecutive transient source errors before the loop gives up.
/// GitHub's GraphQL search occasionally returns HTTP 504; we don't want one
/// flaky poll to kill a long-running discovery loop.
const MAX_SOURCE_ERROR_STREAK: u32 = 5;

/// Exponential backoff cap when retrying after a source error.
const SOURCE_ERROR_BACKOFF_CAP: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// LoopState
// ---------------------------------------------------------------------------

/// Per-iteration state of a running loop.
///
/// `Stopped` is terminal — [`LoopRunner::run`] returns once the FSM reaches
/// it. The other variants form a cycle:
/// `Idle → Pulling → Dispatching → (Awaiting → Validating)? → Pulling → …`.
#[derive(Debug)]
pub enum LoopState {
    /// Fresh runner, not yet pulled.
    Idle { since: Instant },

    /// Polling the source for the next input.
    Pulling,

    /// Holding an input and about to invoke the dispatcher (or effects for
    /// agentless loops).
    Dispatching { input: LoopInput },

    /// Dispatched; waiting for the agent's `complete_task` result.
    Awaiting {
        input: LoopInput,
        agent_id: AgentId,
        started: Instant,
    },

    /// Got a result; validating against [`ExitSchema`] and (if valid)
    /// running effects.
    Validating { input: LoopInput, result: Value },

    /// Terminal state. The reason is recorded for observability.
    Stopped { reason: String },
}

impl LoopState {
    /// Convenience: `true` if this is the terminal [`LoopState::Stopped`].
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        matches!(self, Self::Stopped { .. })
    }

    /// Borrow the stop reason if terminal. `None` otherwise.
    #[must_use]
    pub fn stopped_reason(&self) -> Option<&str> {
        match self {
            Self::Stopped { reason } => Some(reason),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// LoopRunner
// ---------------------------------------------------------------------------

/// One runner instance for one [`LoopSpec`].
///
/// Construct with [`LoopRunner::new`] and drive with [`LoopRunner::run`].
/// The runner is consumed by `run` — restarts mean construct a fresh
/// runner with a fresh source.
pub struct LoopRunner {
    spec: Arc<LoopSpec>,
    exit_schema: Option<ExitSchema>,
    source: Box<dyn LoopSource>,
    queues: Arc<LoopQueueRegistry>,
    dispatcher: Arc<dyn AgentDispatcher>,
    state: LoopState,
    /// Receiver carried *outside* [`LoopState`] because
    /// [`tokio::sync::oneshot::Receiver`] does not implement `Debug` in a
    /// way that's pleasant to thread through a public state enum. Held
    /// `Some` during [`LoopState::Awaiting`], `None` otherwise. Taken on
    /// the `Awaiting → Validating` edge.
    pending_completion:
        Option<tokio::sync::oneshot::Receiver<Result<serde_json::Value, DispatchError>>>,
    /// Consecutive transient source errors. Reset on any non-error pull.
    /// When this exceeds [`MAX_SOURCE_ERROR_STREAK`] the loop stops.
    pull_error_streak: u32,
}

impl LoopRunner {
    /// Construct a runner. Validation of `(spec, exit_schema)` agreement
    /// (agent present iff schema present) is the caller's job — typically
    /// it comes for free from [`crate::parse_spec_str`].
    #[must_use]
    pub fn new(
        spec: Arc<LoopSpec>,
        exit_schema: Option<ExitSchema>,
        source: Box<dyn LoopSource>,
        queues: Arc<LoopQueueRegistry>,
        dispatcher: Arc<dyn AgentDispatcher>,
    ) -> Self {
        Self {
            spec,
            exit_schema,
            source,
            queues,
            dispatcher,
            state: LoopState::Idle {
                since: Instant::now(),
            },
            pending_completion: None,
            pull_error_streak: 0,
        }
    }

    /// Borrow the current state. Stable across ticks; useful for tests.
    #[must_use]
    pub fn state(&self) -> &LoopState {
        &self.state
    }

    /// Borrow the spec the runner is driving.
    #[must_use]
    pub fn spec(&self) -> &LoopSpec {
        &self.spec
    }

    /// Build the per-pull [`LoopContext`] handed to the source.
    fn ctx(&self) -> LoopContext {
        LoopContext {
            loop_id: self.spec.id.clone(),
        }
    }

    /// Drive the FSM until it reaches [`LoopState::Stopped`].
    ///
    /// Returns the stop reason. Consumes the runner.
    pub async fn run(mut self) -> String {
        loop {
            // Single-step the FSM. `step` returns once the state has been
            // transitioned (or the runner stopped); the loop here re-enters
            // for the next transition.
            self.step().await;
            if let LoopState::Stopped { reason } = &self.state {
                return reason.clone();
            }
        }
    }

    /// Single-step the FSM. Public so tests can drive the runner
    /// transition-by-transition without spinning the full `run` loop.
    ///
    /// Returns once exactly one state transition has occurred (or the
    /// runner reached `Stopped`).
    pub async fn step(&mut self) {
        // Replace `self.state` with a sentinel so we can match on the owned
        // value and then assign the new state without borrow-conflict.
        let current = std::mem::replace(
            &mut self.state,
            LoopState::Stopped {
                reason: "<<in-flight>>".to_string(),
            },
        );

        let next = match current {
            LoopState::Idle { .. } => {
                tracing::debug!(loop_id = %self.spec.id, "transition Idle → Pulling");
                LoopState::Pulling
            }
            LoopState::Pulling => self.pull().await,
            LoopState::Dispatching { input } => self.dispatch(input).await,
            LoopState::Awaiting {
                input,
                agent_id,
                started,
            } => self.await_completion(input, agent_id, started).await,
            LoopState::Validating { input, result } => self.validate(input, result).await,
            stopped @ LoopState::Stopped { .. } => stopped,
        };
        self.state = next;
    }

    // -----------------------------------------------------------------------
    // Individual transitions
    // -----------------------------------------------------------------------

    async fn pull(&mut self) -> LoopState {
        let ctx = self.ctx();
        match self.source.next(&ctx) {
            LoopPullResult::Available(input) => {
                self.pull_error_streak = 0;
                tracing::debug!(
                    loop_id = %self.spec.id,
                    key = %input.key,
                    correlation = %input.correlation_id,
                    "transition Pulling → Dispatching",
                );
                LoopState::Dispatching { input }
            }
            LoopPullResult::Empty => {
                self.pull_error_streak = 0;
                tracing::trace!(loop_id = %self.spec.id, "source empty; backing off");
                tokio::time::sleep(IDLE_BACKOFF).await;
                LoopState::Pulling
            }
            LoopPullResult::Done => {
                tracing::info!(loop_id = %self.spec.id, "source exhausted; stopping");
                LoopState::Stopped {
                    reason: "source exhausted".to_string(),
                }
            }
            LoopPullResult::Error(e) => {
                self.pull_error_streak += 1;
                if self.pull_error_streak > MAX_SOURCE_ERROR_STREAK {
                    tracing::warn!(
                        loop_id = %self.spec.id,
                        error = %e,
                        streak = self.pull_error_streak,
                        "source error streak exceeded; stopping",
                    );
                    return LoopState::Stopped {
                        reason: format!("source error: {e}"),
                    };
                }
                let backoff = std::cmp::min(
                    Duration::from_secs(1u64 << (self.pull_error_streak - 1).min(6)),
                    SOURCE_ERROR_BACKOFF_CAP,
                );
                tracing::warn!(
                    loop_id = %self.spec.id,
                    error = %e,
                    streak = self.pull_error_streak,
                    backoff_secs = backoff.as_secs(),
                    "source error; retrying after backoff",
                );
                tokio::time::sleep(backoff).await;
                LoopState::Pulling
            }
        }
    }

    async fn dispatch(&mut self, input: LoopInput) -> LoopState {
        // Agentless path: no agent, so the source's `LoopInput` *is* the
        // iteration result. Synthesise a JSON object that exposes both
        // `key` and `payload` to the effect runner so `EnqueueTo.fields`
        // mappings like `from = "key"` or `from = "payload.<...>"` resolve
        // against the input the source just produced. See issue #665.
        let Some(agent_spec) = self.spec.agent.as_ref() else {
            tracing::debug!(
                loop_id = %self.spec.id,
                key = %input.key,
                "agentless dispatch; running effects with synthesised input result",
            );
            let synthetic_result = serde_json::json!({
                "key": input.key,
                "payload": input.payload,
            });
            return self.run_effects_and_continue(&input, &synthetic_result);
        };

        // Agent path: ask the dispatcher to spawn.
        match self.dispatcher.dispatch(agent_spec, &input) {
            Ok(handle) => {
                tracing::debug!(
                    loop_id = %self.spec.id,
                    key = %input.key,
                    agent_id = handle.agent_id,
                    "transition Dispatching → Awaiting",
                );
                self.pending_completion = Some(handle.completion_rx);
                LoopState::Awaiting {
                    input,
                    agent_id: handle.agent_id,
                    started: Instant::now(),
                }
            }
            Err(e) => {
                tracing::warn!(
                    loop_id = %self.spec.id,
                    key = %input.key,
                    error = %e,
                    "dispatch failed; stopping",
                );
                LoopState::Stopped {
                    reason: format!("dispatch failed: {e}"),
                }
            }
        }
    }

    async fn await_completion(
        &mut self,
        input: LoopInput,
        agent_id: AgentId,
        started: Instant,
    ) -> LoopState {
        let Some(rx) = self.pending_completion.take() else {
            // Defensive: `await_completion` invoked without a receiver in
            // flight — treat as a stop-class invariant violation.
            return LoopState::Stopped {
                reason: "internal error: Awaiting state without pending completion".to_string(),
            };
        };

        match rx.await {
            Ok(Ok(result)) => {
                tracing::debug!(
                    loop_id = %self.spec.id,
                    key = %input.key,
                    agent_id,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "agent completed; transition Awaiting → Validating",
                );
                LoopState::Validating { input, result }
            }
            Ok(Err(e)) => self.handle_dispatch_failure(input, e),
            Err(_recv_err) => self.handle_dispatch_failure(input, DispatchError::Cancelled),
        }
    }

    async fn validate(&mut self, input: LoopInput, result: Value) -> LoopState {
        // Validate against the compiled schema if present.
        if let Some(schema) = self.exit_schema.as_ref()
            && let Err(errors) = schema.validate(&result)
        {
            tracing::warn!(
                loop_id = %self.spec.id,
                key = %input.key,
                errors = ?errors,
                "exit schema validation failed",
            );
            return self.apply_quarantine_policy(errors);
        }

        // Schema-OK (or no schema): run effects with the result.
        self.run_effects_and_continue(&input, &result)
    }

    /// Apply [`LoopQuarantinePolicy`] when an iteration result fails to
    /// validate. Returns the next state.
    fn apply_quarantine_policy(&self, errors: Vec<String>) -> LoopState {
        let reason = format!(
            "exit schema validation failed: {}",
            errors.join("; ")
        );
        match self.spec.on_quarantine {
            LoopQuarantinePolicy::FailAndStop => LoopState::Stopped { reason },
            LoopQuarantinePolicy::Park => LoopState::Stopped {
                reason: format!("parked: {reason}"),
            },
            LoopQuarantinePolicy::SkipAndContinue => {
                tracing::warn!("skipping invalid iteration and continuing");
                LoopState::Pulling
            }
        }
    }

    /// Translate a [`DispatchError`] into the runner's next state.
    ///
    /// `DispatchError::Execution` (agent thrashed past iteration limit,
    /// agent timed out, agent panicked on a tool) is a PER-ITERATION
    /// failure, not substrate breakage — under
    /// [`LoopQuarantinePolicy::SkipAndContinue`] we log it, skip the
    /// iteration, and resume pulling. Without that, a single bad agent
    /// prompt kills the whole loop and the autonomous chain dies (the
    /// 2026-05-20 triager → implementer chain hit this exactly once and
    /// took the implementer down).
    ///
    /// `DispatchError::Spawn` and `DispatchError::Cancelled` remain
    /// terminal — those indicate the substrate itself is broken (queue
    /// poisoned, completion channel closed prematurely) and retrying
    /// past them is futile until the substrate is rebuilt.
    fn handle_dispatch_failure(
        &self,
        input: LoopInput,
        err: DispatchError,
    ) -> LoopState {
        tracing::warn!(
            loop_id = %self.spec.id,
            key = %input.key,
            error = %err,
            on_quarantine = ?self.spec.on_quarantine,
            "dispatch/await failed",
        );
        match (&err, self.spec.on_quarantine) {
            (DispatchError::Execution(_), LoopQuarantinePolicy::SkipAndContinue) => {
                tracing::warn!(
                    loop_id = %self.spec.id,
                    key = %input.key,
                    "agent execution failure — skipping iteration under SkipAndContinue policy",
                );
                LoopState::Pulling
            }
            _ => LoopState::Stopped {
                reason: format!("dispatch failure: {err}"),
            },
        }
    }

    /// Run all configured [`LoopEffect`]s for the iteration that just
    /// produced `result`, then decide whether to continue or stop based on
    /// the presence of [`LoopEffect::StopLoop`].
    fn run_effects_and_continue(&self, input: &LoopInput, result: &Value) -> LoopState {
        let effects: &[LoopEffect] = &self.spec.on_complete;
        let ctx = EffectContext {
            result,
            from_loop: &self.spec.id,
            queues: &self.queues,
        };

        match run_effects(effects, &ctx) {
            Ok(outcome) => {
                if outcome.stop_requested {
                    tracing::info!(loop_id = %self.spec.id, "stop_loop effect fired; stopping");
                    LoopState::Stopped {
                        reason: "stop_loop effect".to_string(),
                    }
                } else {
                    tracing::debug!(
                        loop_id = %self.spec.id,
                        key = %input.key,
                        "effects ran; transition Validating → Pulling",
                    );
                    LoopState::Pulling
                }
            }
            Err(EffectError::FieldMapMissing { effect_idx, path }) => {
                tracing::warn!(
                    loop_id = %self.spec.id,
                    effect_idx,
                    path = %path,
                    "effect field-map path missing in result; stopping",
                );
                LoopState::Stopped {
                    reason: format!(
                        "effect[{effect_idx}] field-map path `{path}` missing in result"
                    ),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests — unit tests that don't require the integration harness.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::LoopQueueRegistry;
    use crate::runner::dispatcher::DispatchHandle;
    use crate::runner::source::{CorrelationId, LoopSourceError};
    use crate::spec::{LoopAgentSpec, LoopPolicy};
    use serde_json::json;
    use tokio::sync::oneshot;

    /// Source that emits one input then `Done`.
    struct OneShotSource {
        input: Option<LoopInput>,
    }
    impl LoopSource for OneShotSource {
        fn next(&mut self, _ctx: &LoopContext) -> LoopPullResult {
            match self.input.take() {
                Some(i) => LoopPullResult::Available(i),
                None => LoopPullResult::Done,
            }
        }
    }

    /// Source that immediately errors.
    struct ErrorSource;
    impl LoopSource for ErrorSource {
        fn next(&mut self, _ctx: &LoopContext) -> LoopPullResult {
            LoopPullResult::Error(LoopSourceError::Transport("boom".to_string()))
        }
    }

    /// Dispatcher that immediately resolves the oneshot with `result`.
    struct StubOk {
        result: Value,
    }
    impl AgentDispatcher for StubOk {
        fn dispatch(
            &self,
            _spec: &LoopAgentSpec,
            _input: &LoopInput,
        ) -> Result<DispatchHandle, DispatchError> {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(Ok(self.result.clone()));
            Ok(DispatchHandle::new(99, rx))
        }
    }

    fn make_spec_with_agent(exit_schema: Value) -> LoopSpec {
        LoopSpec {
            id: "unit-test".to_string(),
            agent: Some(LoopAgentSpec {
                role: phantom_agents::role::AgentRole::Actor,
                allow_tools: None,
                system_prompt: "noop".to_string(),
                exit_schema,
                policy: LoopPolicy::default(),
            }),
            source: crate::source::LoopSourceSpec::Cron {
                interval_seconds: 60,
            },
            on_complete: vec![],
            max_concurrent: 1,
            on_quarantine: LoopQuarantinePolicy::SkipAndContinue,
        }
    }

    #[tokio::test]
    async fn idle_transitions_to_pulling_on_first_step() {
        let spec = make_spec_with_agent(json!({"type": "object"}));
        let schema = ExitSchema::compile(&spec.agent.as_ref().unwrap().exit_schema).ok();
        let source = Box::new(OneShotSource {
            input: Some(LoopInput {
                key: "k1".to_string(),
                payload: json!({}),
                correlation_id: CorrelationId::new("c1"),
            }),
        });
        let mut runner = LoopRunner::new(
            Arc::new(spec),
            schema,
            source,
            Arc::new(LoopQueueRegistry::new()),
            Arc::new(StubOk { result: json!({}) }),
        );
        assert!(matches!(runner.state(), LoopState::Idle { .. }));
        runner.step().await;
        assert!(matches!(runner.state(), LoopState::Pulling));
    }

    #[tokio::test]
    async fn error_source_stops_runner_with_error_reason() {
        let spec = make_spec_with_agent(json!({"type": "object"}));
        let schema = ExitSchema::compile(&spec.agent.as_ref().unwrap().exit_schema).ok();
        let runner = LoopRunner::new(
            Arc::new(spec),
            schema,
            Box::new(ErrorSource),
            Arc::new(LoopQueueRegistry::new()),
            Arc::new(StubOk { result: json!({}) }),
        );
        let reason = runner.run().await;
        assert!(
            reason.starts_with("source error:"),
            "expected source-error reason, got `{reason}`"
        );
    }

    #[tokio::test]
    async fn agentless_dispatch_forwards_loop_input_to_effects() {
        // Regression for issue #665. Agentless loops must surface the
        // source-produced `LoopInput` as the effect-runner result so
        // field maps like `from = "key"` or `from = "payload.<x>"`
        // resolve. With an empty `fields` list the whole synthetic
        // result is forwarded as the message payload.
        let mut spec = make_spec_with_agent(json!({"type": "object"}));
        spec.agent = None;
        spec.on_complete = vec![LoopEffect::EnqueueTo {
            queue: "out".to_string(),
            fields: vec![],
        }];

        let source = Box::new(OneShotSource {
            input: Some(LoopInput {
                key: "k".to_string(),
                payload: json!({"surfaced": true}),
                correlation_id: CorrelationId::new("c"),
            }),
        });
        let queues = Arc::new(LoopQueueRegistry::new());
        let runner = LoopRunner::new(
            Arc::new(spec),
            None,
            source,
            queues.clone(),
            Arc::new(StubOk { result: json!({}) }),
        );
        let reason = runner.run().await;
        // After processing one input, source returns Done → stop.
        assert_eq!(reason, "source exhausted");
        // The synthetic result wraps the LoopInput verbatim.
        let msg = queues.get_or_create("out").pop().expect("one enqueued message");
        assert_eq!(msg.payload, json!({"key": "k", "payload": {"surfaced": true}}));
    }
}
