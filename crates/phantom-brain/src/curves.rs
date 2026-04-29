//! DA:I-inspired response curves, consideration composition, and hysteresis.
//!
//! This module provides the building blocks for a configurable utility scoring
//! system inspired by Dragon Age: Inquisition's Behavior Decision System (BDS).
//!
//! # DA:I architecture mapping
//!
//! | DA:I concept            | Phantom equivalent                          |
//! |-------------------------|---------------------------------------------|
//! | BehaviorSnippet         | `Behavior` (evaluation + action + class)    |
//! | EvaluationTree          | `EvalTree` (filter chain + scoring nodes)   |
//! | FilterNode              | `Filter` (boolean gate)                     |
//! | ScoringNode             | `ResponseCurve` applied to a `Consideration`|
//! | TargetSelector          | Not needed (no NPCs to target)              |
//! | ExecutionTree           | Maps to `AiAction` directly                 |
//! | Scoring convention      | `ActionClass` with point ranges             |
//! | Summary table           | `EvalSnapshot` for debug overlay            |
//!
//! # Key design decisions from DA:I
//!
//! - Scores are **additive** within a behavior, not multiplicative.
//!   Each consideration that passes its filter adds its curve output to the
//!   running total. This avoids the "many considerations collapse to zero"
//!   problem that plagues multiplicative systems.
//!
//! - Actions are grouped into **classes** with fixed score ranges:
//!   - Basic (0-10): default/idle behaviors
//!   - Proactive (20-40): suggestions, memory updates
//!   - Support (25-45): explanations, monitoring
//!   - Reaction (50-70): error fixes, urgent notifications
//!
//! - **Momentum/hysteresis**: the currently-executing action gets a bonus
//!   that prevents rapid switching (thrashing). The bonus decays over time
//!   so stale actions can eventually be preempted.

use std::time::Instant;

// ---------------------------------------------------------------------------
// ResponseCurve — configurable scoring functions
// ---------------------------------------------------------------------------

/// A response curve maps a normalized input [0.0, 1.0] to a score.
///
/// These are the fundamental building blocks of the scoring system.
/// Each curve type produces different decision-making characteristics:
/// - Linear: proportional response (good for idle time, health)
/// - Polynomial: accelerating/decelerating response (urgency ramps)
/// - Logistic: S-curve with sharp transition (threshold triggers)
/// - Step: binary on/off (hard filters that still contribute score)
/// - Constant: fixed score (baseline behaviors)
#[derive(Debug, Clone)]
pub enum ResponseCurve {
    /// `score = slope * input + intercept`, clamped to [0, max_score].
    ///
    /// Use for: idle time scoring, linear urgency.
    Linear {
        slope: f32,
        intercept: f32,
        max_score: f32,
    },

    /// `score = coefficient * input^exponent`, clamped to [0, max_score].
    ///
    /// Exponent < 1.0: diminishing returns (log-like).
    /// Exponent > 1.0: accelerating urgency (exponential-like).
    Polynomial {
        exponent: f32,
        coefficient: f32,
        max_score: f32,
    },

    /// `score = max_score / (1 + e^(-steepness * (input - midpoint)))`.
    ///
    /// S-curve: low output below midpoint, rapid transition, high above.
    /// Use for: threshold-based triggers (error count, idle duration).
    Logistic {
        midpoint: f32,
        steepness: f32,
        max_score: f32,
    },

    /// `score = if input >= threshold { on_value } else { off_value }`.
    ///
    /// Binary switch. Use for: hard preconditions that also contribute score.
    Step {
        threshold: f32,
        on_value: f32,
        off_value: f32,
    },

    /// `score = value` regardless of input.
    ///
    /// Use for: baseline behaviors (DA:I's "follow the leader" at score 0).
    Constant { value: f32 },
}

impl ResponseCurve {
    /// Evaluate the curve for a given input in [0.0, 1.0].
    pub fn evaluate(&self, input: f32) -> f32 {
        let input = input.clamp(0.0, 1.0);
        match self {
            Self::Linear {
                slope,
                intercept,
                max_score,
            } => (slope * input + intercept).clamp(0.0, *max_score),
            Self::Polynomial {
                exponent,
                coefficient,
                max_score,
            } => (coefficient * input.powf(*exponent)).clamp(0.0, *max_score),
            Self::Logistic {
                midpoint,
                steepness,
                max_score,
            } => {
                let exp = (-steepness * (input - midpoint)).exp();
                (max_score / (1.0 + exp)).clamp(0.0, *max_score)
            }
            Self::Step {
                threshold,
                on_value,
                off_value,
            } => {
                if input >= *threshold {
                    *on_value
                } else {
                    *off_value
                }
            }
            Self::Constant { value } => *value,
        }
    }

    // -- Convenience constructors --

    /// Linear curve: 0 at input=0, `max_score` at input=1.
    pub fn linear(max_score: f32) -> Self {
        Self::Linear {
            slope: max_score,
            intercept: 0.0,
            max_score,
        }
    }

    /// Polynomial with exponent 2 (quadratic ramp).
    pub fn quadratic(max_score: f32) -> Self {
        Self::Polynomial {
            exponent: 2.0,
            coefficient: max_score,
            max_score,
        }
    }

    /// Logistic S-curve centered at 0.5 with moderate steepness.
    pub fn logistic(max_score: f32) -> Self {
        Self::Logistic {
            midpoint: 0.5,
            steepness: 10.0,
            max_score,
        }
    }

    /// Step function that returns `score` when input >= threshold.
    pub fn step(threshold: f32, score: f32) -> Self {
        Self::Step {
            threshold,
            on_value: score,
            off_value: 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Filter — boolean precondition gate (DA:I FilterNode)
// ---------------------------------------------------------------------------

/// A boolean precondition that gates whether a consideration contributes.
///
/// In DA:I, filter nodes are binary: they either allow the tree to continue
/// (returning true) or stop evaluation of that branch. Here, a filter is a
/// closure that examines the scoring context and returns pass/fail.
pub struct Filter {
    /// Human-readable name for debugging.
    pub name: String,
    /// The gate function. Returns true if the consideration should contribute.
    pub gate: Box<dyn Fn(&ScoringContext) -> bool + Send>,
}

impl std::fmt::Debug for Filter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Filter({})", self.name)
    }
}

// ---------------------------------------------------------------------------
// Consideration — a single factor in the evaluation (DA:I ScoringNode)
// ---------------------------------------------------------------------------

/// A single scoring factor: an input accessor, an optional filter gate,
/// and a response curve that maps the input to a score contribution.
///
/// In DA:I, scoring nodes are embedded in evaluation trees. Here, we
/// flatten the tree into a list of considerations per behavior. Each
/// consideration:
/// 1. Checks its filter (if present) -- if it fails, contributes 0.
/// 2. Reads its input from the scoring context.
/// 3. Applies its response curve to get a score contribution.
/// 4. Adds the result to the behavior's running total (additive composition).
pub struct Consideration {
    /// Human-readable name for debugging.
    pub name: String,
    /// Optional gate -- if it fails, this consideration contributes 0.
    pub filter: Option<Filter>,
    /// Extracts a normalized [0.0, 1.0] input from the scoring context.
    pub input_fn: Box<dyn Fn(&ScoringContext) -> f32 + Send>,
    /// Maps the input to a score contribution.
    pub curve: ResponseCurve,
}

impl std::fmt::Debug for Consideration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Consideration({}, {:?})", self.name, self.curve)
    }
}

impl Consideration {
    /// Evaluate this consideration against a scoring context.
    /// Returns the score contribution (0.0 if filtered out).
    pub fn evaluate(&self, ctx: &ScoringContext) -> f32 {
        // Check filter gate.
        if let Some(ref filter) = self.filter {
            if !(filter.gate)(ctx) {
                return 0.0;
            }
        }
        // Read input and apply curve.
        let input = (self.input_fn)(ctx);
        self.curve.evaluate(input)
    }
}

// ---------------------------------------------------------------------------
// ActionClass — DA:I scoring convention (Table 31.1)
// ---------------------------------------------------------------------------

/// Action class determines the score range convention.
///
/// From DA:I's Table 31.1: each class has a designated point range.
/// Higher classes always beat lower classes when their conditions fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionClass {
    /// Score range: 0-10. Default/idle. "Follow the leader."
    Basic,
    /// Score range: 20-40. Proactive actions (suggestions, memory updates).
    Proactive,
    /// Score range: 25-45. Preparatory/support (explanations, monitoring).
    Support,
    /// Score range: 50-70. Immediate response to threats/errors.
    Reaction,
}

impl ActionClass {
    /// The base score for this class (minimum when the behavior fires).
    pub fn base_score(self) -> f32 {
        match self {
            Self::Basic => 0.0,
            Self::Proactive => 20.0,
            Self::Support => 25.0,
            Self::Reaction => 50.0,
        }
    }

    /// The maximum dynamic range within this class.
    pub fn dynamic_range(self) -> f32 {
        match self {
            Self::Basic => 10.0,
            Self::Proactive => 20.0,
            Self::Support => 20.0,
            Self::Reaction => 20.0,
        }
    }
}

// ---------------------------------------------------------------------------
// ScoringContext — the "world state" snapshot fed to considerations
// ---------------------------------------------------------------------------

/// Snapshot of the brain's state, used as input to considerations.
///
/// This replaces the scattered parameters in the current scorer methods.
/// All considerations read from the same context, making it easy to add
/// new inputs without changing function signatures.
#[derive(Debug, Clone)]
pub struct ScoringContext {
    /// Seconds since the user's last input.
    pub idle_secs: f32,
    /// Whether the last command had errors.
    pub has_errors: bool,
    /// Number of errors in the last command (0 if no errors).
    pub error_count: u32,
    /// Whether a long-running process is active.
    pub has_active_process: bool,
    /// Whether a new memory pattern was detected.
    pub new_pattern_detected: bool,
    /// Whether an agent just completed.
    pub agent_just_completed: bool,
    /// Whether a file/git change was detected.
    pub file_or_git_changed: bool,
    /// Whether the user is in a REPL session.
    pub in_repl: bool,
    /// Chattiness level (0.0 = fresh, 0.5 = very chatty).
    pub chattiness: f32,
    /// Number of suggestions since last user input.
    pub suggestions_since_input: u32,
}

impl Default for ScoringContext {
    fn default() -> Self {
        Self {
            idle_secs: 0.0,
            has_errors: false,
            error_count: 0,
            has_active_process: false,
            new_pattern_detected: false,
            agent_just_completed: false,
            file_or_git_changed: false,
            in_repl: false,
            chattiness: 0.0,
            suggestions_since_input: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Behavior — a complete DA:I BehaviorSnippet
// ---------------------------------------------------------------------------

/// A complete behavior: a named action with its evaluation logic.
///
/// Mirrors DA:I's BehaviorSnippet which contains both an evaluation tree
/// (for scoring) and an execution tree (for performing the action).
/// In Phantom, the "execution tree" is just the action_id that maps
/// to an AiAction.
pub struct Behavior {
    /// Unique identifier for this behavior.
    pub id: String,
    /// The action class (determines base score range).
    pub class: ActionClass,
    /// Optional top-level viability gate.
    ///
    /// If present and the gate returns `false` for the current context, the
    /// entire behavior is treated as non-viable and returns 0.0 (the behavior
    /// cannot even earn its class base score). This is the DA:I root-filter
    /// that prevents non-applicable behaviors from polluting the score table.
    pub viable: Option<Box<dyn Fn(&ScoringContext) -> bool + Send>>,
    /// The considerations that compose this behavior's score.
    /// Score = base_score + sum(consideration.evaluate()).
    pub considerations: Vec<Consideration>,
}

impl Behavior {
    /// Evaluate this behavior: run all considerations and sum their outputs,
    /// added to the class base score.
    ///
    /// Returns 0.0 immediately if the `viable` gate is present and fails.
    /// This is the DA:I additive composition: start at base_score for the
    /// action class, then add each consideration's curve output.
    pub fn evaluate(&self, ctx: &ScoringContext) -> f32 {
        // Root viability check — if the behavior can't possibly apply, score 0.
        if let Some(ref gate) = self.viable {
            if !gate(ctx) {
                return 0.0;
            }
        }

        let base = self.class.base_score();
        let max_add = self.class.dynamic_range();

        let consideration_total: f32 = self.considerations.iter().map(|c| c.evaluate(ctx)).sum();

        // Clamp the consideration contribution to the class dynamic range.
        base + consideration_total.min(max_add)
    }
}

impl std::fmt::Debug for Behavior {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Behavior({}, {:?}, {} considerations)",
            self.id,
            self.class,
            self.considerations.len()
        )
    }
}

// ---------------------------------------------------------------------------
// Momentum — hysteresis to prevent action thrashing
// ---------------------------------------------------------------------------

/// Tracks the currently active behavior and applies a bonus to prevent
/// rapid switching (thrashing).
///
/// DA:I handles this implicitly through continuous re-evaluation with
/// execution tree guards. We make it explicit: the currently active
/// behavior gets a score bonus that decays over time. This means a new
/// behavior must score significantly higher to preempt the current one.
#[derive(Debug)]
pub struct Momentum {
    /// The behavior ID that is currently "active" (executing or recently chosen).
    pub active_behavior: Option<String>,
    /// When the active behavior was last selected.
    pub selected_at: Option<Instant>,
    /// The initial bonus applied to the active behavior.
    pub initial_bonus: f32,
    /// How many seconds until the bonus fully decays to zero.
    pub decay_secs: f32,
}

impl Momentum {
    pub fn new() -> Self {
        Self {
            active_behavior: None,
            selected_at: None,
            initial_bonus: 10.0, // 10 points of hysteresis
            decay_secs: 30.0,    // fully decays after 30s
        }
    }

    /// Set the currently active behavior.
    pub fn set_active(&mut self, behavior_id: &str) {
        self.active_behavior = Some(behavior_id.to_owned());
        self.selected_at = Some(Instant::now());
    }

    /// Clear the active behavior (e.g., on user action).
    pub fn clear(&mut self) {
        self.active_behavior = None;
        self.selected_at = None;
    }

    /// Get the momentum bonus for a given behavior ID.
    ///
    /// Returns > 0.0 if this behavior is the currently active one and the
    /// bonus hasn't fully decayed. Returns 0.0 otherwise.
    pub fn bonus_for(&self, behavior_id: &str) -> f32 {
        let Some(ref active) = self.active_behavior else {
            return 0.0;
        };
        if active != behavior_id {
            return 0.0;
        }
        let Some(selected_at) = self.selected_at else {
            return 0.0;
        };

        let elapsed = selected_at.elapsed().as_secs_f32();
        if elapsed >= self.decay_secs {
            return 0.0;
        }

        // Linear decay from initial_bonus to 0 over decay_secs.
        self.initial_bonus * (1.0 - elapsed / self.decay_secs)
    }
}

impl Default for Momentum {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// EvalSnapshot — debug output for the DA:I "summary table"
// ---------------------------------------------------------------------------

/// A snapshot of one behavior's evaluation result.
///
/// DA:I stores these in a "debug-viewable summary table" for each
/// character's AI update. We do the same for the brain debug overlay.
#[derive(Debug, Clone)]
pub struct BehaviorScore {
    pub behavior_id: String,
    pub class: ActionClass,
    pub raw_score: f32,
    pub momentum_bonus: f32,
    pub final_score: f32,
}

/// Full evaluation snapshot for one brain tick.
#[derive(Debug, Clone)]
pub struct EvalSnapshot {
    pub scores: Vec<BehaviorScore>,
    pub winner_id: String,
    pub winner_score: f32,
}

// ---------------------------------------------------------------------------
// BehaviorDecisionSystem — the full DA:I BDS
// ---------------------------------------------------------------------------

/// The complete Behavior Decision System: evaluates all registered behaviors
/// and picks the highest-scoring one, with momentum applied.
///
/// This is the DA:I equivalent of the main BDS evaluation pass that runs
/// on every AI update. It replaces the flat list of scorer methods in the
/// current UtilityScorer with a data-driven, composable system.
pub struct BehaviorDecisionSystem {
    /// All registered behaviors.
    pub behaviors: Vec<Behavior>,
    /// Momentum tracker for hysteresis.
    pub momentum: Momentum,
}

impl BehaviorDecisionSystem {
    pub fn new() -> Self {
        Self {
            behaviors: Vec::new(),
            momentum: Momentum::new(),
        }
    }

    /// Register a behavior.
    pub fn register(&mut self, behavior: Behavior) {
        self.behaviors.push(behavior);
    }

    /// Evaluate all behaviors and return the full snapshot + winner ID.
    ///
    /// This is the core loop from DA:I Listing 31.1:
    /// ```text
    /// for each registered behavior:
    ///     score = behavior.evaluate(context)
    ///     score += momentum.bonus_for(behavior.id)
    ///     record (behavior, score)
    /// pick highest score
    /// ```
    pub fn evaluate(&mut self, ctx: &ScoringContext) -> EvalSnapshot {
        let mut scores: Vec<BehaviorScore> = self
            .behaviors
            .iter()
            .map(|b| {
                let raw = b.evaluate(ctx);
                let bonus = self.momentum.bonus_for(&b.id);
                BehaviorScore {
                    behavior_id: b.id.clone(),
                    class: b.class,
                    raw_score: raw,
                    momentum_bonus: bonus,
                    final_score: raw + bonus,
                }
            })
            .collect();

        scores.sort_by(|a, b| {
            b.final_score
                .partial_cmp(&a.final_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let (winner_id, winner_score) = scores
            .first()
            .map(|s| (s.behavior_id.clone(), s.final_score))
            .unwrap_or_else(|| ("quiet".into(), 0.0));

        // Update momentum to the new winner.
        if self.momentum.active_behavior.as_deref() != Some(&winner_id) {
            self.momentum.set_active(&winner_id);
        }

        EvalSnapshot {
            scores,
            winner_id,
            winner_score,
        }
    }
}

impl Default for BehaviorDecisionSystem {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Factory: build the default Phantom BDS behaviors
// ---------------------------------------------------------------------------

/// Build the default set of behaviors for Phantom's brain.
///
/// This maps the current hardcoded scorers in `scoring.rs` to the DA:I
/// data-driven system. Each former scorer method becomes a `Behavior`
/// with explicit considerations and response curves.
pub fn build_default_behaviors() -> Vec<Behavior> {
    vec![
        // -- Quiet baseline (Basic class, always viable) --
        Behavior {
            id: "quiet".into(),
            class: ActionClass::Basic,
            viable: None, // always eligible
            considerations: vec![
                Consideration {
                    name: "baseline".into(),
                    filter: None,
                    input_fn: Box::new(|_| 1.0),
                    curve: ResponseCurve::Constant { value: 1.0 },
                },
                // Chattiness raises the quiet bar.
                Consideration {
                    name: "chattiness_penalty".into(),
                    filter: None,
                    input_fn: Box::new(|ctx| ctx.chattiness / 0.5), // normalize to [0, 1]
                    curve: ResponseCurve::linear(9.0),              // up to 9 more points
                },
            ],
        },
        // -- Fix error (Reaction class: 50-70) --
        // Viable only when errors are present and user is not actively typing.
        Behavior {
            id: "fix_error".into(),
            class: ActionClass::Reaction,
            viable: Some(Box::new(|ctx| ctx.has_errors && ctx.idle_secs >= 2.0)),
            considerations: vec![
                // Must have errors.
                Consideration {
                    name: "has_errors".into(),
                    filter: Some(Filter {
                        name: "errors_present".into(),
                        gate: Box::new(|ctx| ctx.has_errors),
                    }),
                    input_fn: Box::new(|_| 1.0),
                    curve: ResponseCurve::Constant { value: 5.0 },
                },
                // More errors = more urgency (up to 10 more points).
                Consideration {
                    name: "error_count_urgency".into(),
                    filter: Some(Filter {
                        name: "errors_present".into(),
                        gate: Box::new(|ctx| ctx.has_errors),
                    }),
                    input_fn: Box::new(|ctx| (ctx.error_count as f32 / 5.0).min(1.0)),
                    curve: ResponseCurve::linear(10.0),
                },
                // User must be idle enough (not actively typing).
                Consideration {
                    name: "user_idle_enough".into(),
                    filter: Some(Filter {
                        name: "idle_gt_2s".into(),
                        gate: Box::new(|ctx| ctx.idle_secs >= 2.0),
                    }),
                    input_fn: Box::new(|ctx| (ctx.idle_secs / 30.0).min(1.0)),
                    curve: ResponseCurve::Logistic {
                        midpoint: 0.1, // fires quickly after 2s
                        steepness: 15.0,
                        max_score: 5.0,
                    },
                },
            ],
        },
        // -- Explain error (Support class: 25-45) --
        // Viable only when there are errors and the user has been idle > 5s.
        Behavior {
            id: "explain_error".into(),
            class: ActionClass::Support,
            viable: Some(Box::new(|ctx| {
                ctx.has_errors && ctx.idle_secs > 5.0 && !ctx.in_repl
            })),
            considerations: vec![
                // User must have been idle after an error.
                Consideration {
                    name: "idle_after_error".into(),
                    filter: Some(Filter {
                        name: "has_errors_and_idle".into(),
                        gate: Box::new(|ctx| ctx.has_errors && ctx.idle_secs > 5.0 && !ctx.in_repl),
                    }),
                    input_fn: Box::new(|ctx| ((ctx.idle_secs - 5.0) / 25.0).clamp(0.0, 1.0)),
                    curve: ResponseCurve::Logistic {
                        midpoint: 0.3, // transition around 10-12s idle
                        steepness: 8.0,
                        max_score: 15.0,
                    },
                },
            ],
        },
        // -- Offer help (Support class: 25-45, lower than explain) --
        // Viable only after a long idle with no errors.
        Behavior {
            id: "offer_help".into(),
            class: ActionClass::Support,
            viable: Some(Box::new(|ctx| {
                !ctx.has_errors && ctx.idle_secs > 30.0 && !ctx.in_repl
            })),
            considerations: vec![Consideration {
                name: "long_idle_no_errors".into(),
                filter: Some(Filter {
                    name: "idle_gt_30_no_errors".into(),
                    gate: Box::new(|ctx| !ctx.has_errors && ctx.idle_secs > 30.0 && !ctx.in_repl),
                }),
                input_fn: Box::new(|ctx| ((ctx.idle_secs - 30.0) / 60.0).clamp(0.0, 1.0)),
                curve: ResponseCurve::linear(10.0),
            }],
        },
        // -- Update memory (Proactive class: 20-40) --
        // Viable only when a new pattern was detected.
        Behavior {
            id: "update_memory".into(),
            class: ActionClass::Proactive,
            viable: Some(Box::new(|ctx| ctx.new_pattern_detected)),
            considerations: vec![Consideration {
                name: "new_pattern".into(),
                filter: Some(Filter {
                    name: "new_pattern_detected".into(),
                    gate: Box::new(|ctx| ctx.new_pattern_detected),
                }),
                input_fn: Box::new(|_| 1.0),
                curve: ResponseCurve::Constant { value: 15.0 },
            }],
        },
        // -- Watch build (Proactive class: 20-40) --
        // Viable only while an active process is running.
        Behavior {
            id: "watch_build".into(),
            class: ActionClass::Proactive,
            viable: Some(Box::new(|ctx| ctx.has_active_process)),
            considerations: vec![Consideration {
                name: "active_process".into(),
                filter: Some(Filter {
                    name: "has_active_process".into(),
                    gate: Box::new(|ctx| ctx.has_active_process),
                }),
                input_fn: Box::new(|_| 1.0),
                curve: ResponseCurve::Constant { value: 10.0 },
            }],
        },
        // -- Notification: agent complete (Reaction class: 50-70) --
        // Viable only when an agent just completed.
        Behavior {
            id: "notify_agent_complete".into(),
            class: ActionClass::Reaction,
            viable: Some(Box::new(|ctx| ctx.agent_just_completed)),
            considerations: vec![Consideration {
                name: "agent_completed".into(),
                filter: Some(Filter {
                    name: "agent_just_completed".into(),
                    gate: Box::new(|ctx| ctx.agent_just_completed),
                }),
                input_fn: Box::new(|_| 1.0),
                curve: ResponseCurve::Constant { value: 15.0 },
            }],
        },
        // -- Notification: file/git change (Proactive class: 20-40) --
        // Viable only when a file or git change was detected this frame.
        Behavior {
            id: "notify_change".into(),
            class: ActionClass::Proactive,
            viable: Some(Box::new(|ctx| ctx.file_or_git_changed)),
            considerations: vec![Consideration {
                name: "file_or_git_changed".into(),
                filter: Some(Filter {
                    name: "change_detected".into(),
                    gate: Box::new(|ctx| ctx.file_or_git_changed),
                }),
                input_fn: Box::new(|_| 1.0),
                curve: ResponseCurve::Constant { value: 8.0 },
            }],
        },
    ]
}

// ---------------------------------------------------------------------------
// UtilityCurve — normalized [0,1] → [0,1] scoring primitives (issue #40)
// ---------------------------------------------------------------------------

/// A scoring curve that maps a normalized input `x ∈ [0.0, 1.0]` to a
/// normalized output score `∈ [0.0, 1.0]`.
///
/// These are the raw mathematical building blocks used by the BDS system's
/// [`ResponseCurve`] variants (which add action-specific max_score scaling
/// on top). Use `UtilityCurve` implementations directly when you need pure
/// normalized scoring without the DA:I class/range machinery.
///
/// # Contract
///
/// - Input `x` outside `[0.0, 1.0]` is clamped before evaluation.
/// - Output is always clamped to `[0.0, 1.0]`.
/// - Implementations must be `Send + Sync` (curves are shared across threads).
pub trait UtilityCurve: Send + Sync {
    /// Evaluate the curve at normalized input `x`.
    ///
    /// `x` is clamped to `[0.0, 1.0]` before use; output is guaranteed in
    /// `[0.0, 1.0]`.
    fn score(&self, x: f32) -> f32;
}

// ---------------------------------------------------------------------------
// LinearCurve
// ---------------------------------------------------------------------------

/// A linear ramp: `score = m * x + b`, clamped to `[0.0, 1.0]`.
///
/// Produces a proportional response. Use for inputs where the scoring
/// contribution should scale evenly (idle time, file count, etc.).
///
/// # Builder
///
/// ```rust
/// use phantom_brain::curves::{LinearCurve, UtilityCurve};
///
/// // Rising from 0 at x=0 to 1 at x=1.
/// let ramp = LinearCurve::rising();
/// assert_eq!(ramp.score(0.0), 0.0);
/// assert_eq!(ramp.score(1.0), 1.0);
/// ```
#[derive(Debug, Clone, Copy)]
pub struct LinearCurve {
    /// Slope (rise per unit input).
    m: f32,
    /// Y-intercept (score at x=0 before clamping).
    b: f32,
}

impl LinearCurve {
    /// Create a custom linear curve `score = m * x + b`.
    pub fn new(m: f32, b: f32) -> Self {
        Self { m, b }
    }

    /// Rising ramp from 0 to 1 (`m=1, b=0`).
    pub fn rising() -> Self {
        Self { m: 1.0, b: 0.0 }
    }

    /// Falling ramp from 1 to 0 (`m=-1, b=1`).
    pub fn falling() -> Self {
        Self { m: -1.0, b: 1.0 }
    }
}

impl UtilityCurve for LinearCurve {
    fn score(&self, x: f32) -> f32 {
        let x = x.clamp(0.0, 1.0);
        (self.m * x + self.b).clamp(0.0, 1.0)
    }
}

// ---------------------------------------------------------------------------
// ExponentialCurve
// ---------------------------------------------------------------------------

/// A power-law curve: `score = x^k`, clamped to `[0.0, 1.0]`.
///
/// - `k > 1.0`: accelerating (convex) — slow start, fast finish.
/// - `k = 1.0`: linear (same as [`LinearCurve::rising`]).
/// - `0 < k < 1.0`: decelerating (concave) — fast start, slow finish.
///
/// # Builder
///
/// ```rust
/// use phantom_brain::curves::{ExponentialCurve, UtilityCurve};
///
/// let quadratic = ExponentialCurve::quadratic();
/// assert!(quadratic.score(0.5) < 0.5); // accelerating: scores below linear
/// ```
#[derive(Debug, Clone, Copy)]
pub struct ExponentialCurve {
    /// The exponent `k`. Must be positive.
    k: f32,
}

impl ExponentialCurve {
    /// Create a power-law curve with exponent `k`.
    pub fn new(k: f32) -> Self {
        Self {
            k: k.max(f32::EPSILON),
        }
    }

    /// Quadratic ramp (`k = 2`): accelerating urgency.
    pub fn quadratic() -> Self {
        Self { k: 2.0 }
    }

    /// Square-root ramp (`k = 0.5`): fast initial gain, diminishing returns.
    pub fn sqrt() -> Self {
        Self { k: 0.5 }
    }
}

impl UtilityCurve for ExponentialCurve {
    fn score(&self, x: f32) -> f32 {
        let x = x.clamp(0.0, 1.0);
        x.powf(self.k).clamp(0.0, 1.0)
    }
}

// ---------------------------------------------------------------------------
// LogisticCurve (sigmoid)
// ---------------------------------------------------------------------------

/// A logistic (sigmoid) S-curve: low below midpoint, rapid transition, high above.
///
/// `score = 1 / (1 + exp(-steepness * (x - midpoint)))`, clamped to `[0.0, 1.0]`.
///
/// Use for threshold-triggered scoring — inputs below the midpoint score
/// near 0, inputs above score near 1, with a sharp transition width
/// controlled by `steepness`.
///
/// # Builder
///
/// ```rust
/// use phantom_brain::curves::{LogisticCurve, UtilityCurve};
///
/// let sigmoid = LogisticCurve::standard();
/// assert!(sigmoid.score(0.0) < 0.1);
/// assert!((sigmoid.score(0.5) - 0.5).abs() < 0.01);
/// assert!(sigmoid.score(1.0) > 0.9);
/// ```
#[derive(Debug, Clone, Copy)]
pub struct LogisticCurve {
    /// The x-value at the inflection point (score = 0.5).
    midpoint: f32,
    /// Controls the transition sharpness. Higher = steeper S.
    steepness: f32,
}

impl LogisticCurve {
    /// Create a logistic curve with custom midpoint and steepness.
    pub fn new(midpoint: f32, steepness: f32) -> Self {
        Self {
            midpoint,
            steepness,
        }
    }

    /// Standard sigmoid centered at 0.5 with moderate steepness (10).
    pub fn standard() -> Self {
        Self {
            midpoint: 0.5,
            steepness: 10.0,
        }
    }

    /// Sharp transition (steepness = 20) centered at 0.5.
    pub fn sharp() -> Self {
        Self {
            midpoint: 0.5,
            steepness: 20.0,
        }
    }
}

impl UtilityCurve for LogisticCurve {
    fn score(&self, x: f32) -> f32 {
        let x = x.clamp(0.0, 1.0);
        let exp = (-self.steepness * (x - self.midpoint)).exp();
        (1.0 / (1.0 + exp)).clamp(0.0, 1.0)
    }
}

// ---------------------------------------------------------------------------
// StepCurve
// ---------------------------------------------------------------------------

/// A binary step: `score = on_value` when `x >= threshold`, else `off_value`.
///
/// Use for hard on/off conditions that still participate in the normalized
/// scoring pipeline (rather than being a boolean filter gate).
///
/// # Builder
///
/// ```rust
/// use phantom_brain::curves::{StepCurve, UtilityCurve};
///
/// let gate = StepCurve::on_at(0.5);
/// assert_eq!(gate.score(0.49), 0.0);
/// assert_eq!(gate.score(0.5),  1.0);
/// ```
#[derive(Debug, Clone, Copy)]
pub struct StepCurve {
    /// The input threshold. At or above this value, output is `on_value`.
    threshold: f32,
    /// Output when `x >= threshold`.
    on_value: f32,
    /// Output when `x < threshold`.
    off_value: f32,
}

impl StepCurve {
    /// Create a step with custom threshold and values.
    pub fn new(threshold: f32, on_value: f32, off_value: f32) -> Self {
        Self {
            threshold,
            on_value: on_value.clamp(0.0, 1.0),
            off_value: off_value.clamp(0.0, 1.0),
        }
    }

    /// Binary 0/1 step that activates at `threshold`.
    pub fn on_at(threshold: f32) -> Self {
        Self {
            threshold,
            on_value: 1.0,
            off_value: 0.0,
        }
    }

    /// Always-on constant (threshold = 0).
    pub fn always_on() -> Self {
        Self {
            threshold: 0.0,
            on_value: 1.0,
            off_value: 1.0,
        }
    }
}

impl UtilityCurve for StepCurve {
    fn score(&self, x: f32) -> f32 {
        let x = x.clamp(0.0, 1.0);
        if x >= self.threshold {
            self.on_value
        } else {
            self.off_value
        }
        .clamp(0.0, 1.0)
    }
}

// ---------------------------------------------------------------------------
// InvertedCurve
// ---------------------------------------------------------------------------

/// Wraps another curve and inverts its output: `score = 1 - inner.score(x)`.
///
/// Use to express "less is more" semantics — e.g., high chattiness should
/// *lower* the intervention score, which is the inversion of the raw curve.
///
/// # Builder
///
/// ```rust
/// use phantom_brain::curves::{InvertedCurve, LinearCurve, UtilityCurve};
///
/// let falling = InvertedCurve::of(LinearCurve::rising());
/// assert_eq!(falling.score(0.0), 1.0);
/// assert_eq!(falling.score(1.0), 0.0);
/// ```
pub struct InvertedCurve {
    inner: Box<dyn UtilityCurve>,
}

impl InvertedCurve {
    /// Wrap `curve` and invert its output.
    pub fn of<C: UtilityCurve + 'static>(curve: C) -> Self {
        Self {
            inner: Box::new(curve),
        }
    }
}

impl std::fmt::Debug for InvertedCurve {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "InvertedCurve(..)")
    }
}

impl UtilityCurve for InvertedCurve {
    fn score(&self, x: f32) -> f32 {
        (1.0 - self.inner.score(x)).clamp(0.0, 1.0)
    }
}

// ---------------------------------------------------------------------------
// CompositeCurve
// ---------------------------------------------------------------------------

/// Combines multiple [`UtilityCurve`]s with explicit weights.
///
/// The output is the weighted average of all component curves:
///
/// ```text
/// score = Σ(weight_i * curve_i.score(x)) / Σ(weight_i)
/// ```
///
/// Weights need not sum to 1.0 — they are normalized internally. An empty
/// composite scores 0.0.
///
/// # Validation
///
/// [`CompositeCurve::build`] returns an error if any weight is negative.
/// Use [`CompositeCurve::new`] if you want to bypass the guard (weights are
/// clamped to 0.0 at evaluation time in that case).
///
/// # Example
///
/// ```rust
/// use phantom_brain::curves::{CompositeCurve, LinearCurve, LogisticCurve, UtilityCurve};
///
/// let composite = CompositeCurve::build(vec![
///     (Box::new(LinearCurve::rising()),    0.6),
///     (Box::new(LogisticCurve::standard()), 0.4),
/// ]).unwrap();
///
/// let score = composite.score(0.5);
/// assert!(score >= 0.0 && score <= 1.0);
/// ```
pub struct CompositeCurve {
    components: Vec<(Box<dyn UtilityCurve>, f32)>,
}

impl CompositeCurve {
    /// Create without validation (weights clamped to ≥ 0 at evaluation).
    pub fn new(components: Vec<(Box<dyn UtilityCurve>, f32)>) -> Self {
        Self { components }
    }

    /// Create with validation: returns `Err` if any weight is negative.
    pub fn build(components: Vec<(Box<dyn UtilityCurve>, f32)>) -> Result<Self, &'static str> {
        if components.iter().any(|(_, w)| *w < 0.0) {
            return Err("CompositeCurve: all weights must be non-negative");
        }
        Ok(Self { components })
    }

    /// Number of component curves.
    pub fn len(&self) -> usize {
        self.components.len()
    }

    /// True if no component curves are registered.
    pub fn is_empty(&self) -> bool {
        self.components.is_empty()
    }
}

impl std::fmt::Debug for CompositeCurve {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CompositeCurve({} components)", self.components.len())
    }
}

impl UtilityCurve for CompositeCurve {
    fn score(&self, x: f32) -> f32 {
        if self.components.is_empty() {
            return 0.0;
        }

        let x = x.clamp(0.0, 1.0);
        let total_weight: f32 = self.components.iter().map(|(_, w)| w.max(0.0)).sum();

        if total_weight <= 0.0 {
            return 0.0;
        }

        let weighted_sum: f32 = self
            .components
            .iter()
            .map(|(curve, w)| curve.score(x) * w.max(0.0))
            .sum();

        (weighted_sum / total_weight).clamp(0.0, 1.0)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // ResponseCurve tests
    // -----------------------------------------------------------------------

    #[test]
    fn linear_curve_at_endpoints() {
        let curve = ResponseCurve::linear(10.0);
        assert!((curve.evaluate(0.0) - 0.0).abs() < f32::EPSILON);
        assert!((curve.evaluate(1.0) - 10.0).abs() < f32::EPSILON);
    }

    #[test]
    fn linear_curve_midpoint() {
        let curve = ResponseCurve::linear(10.0);
        assert!((curve.evaluate(0.5) - 5.0).abs() < f32::EPSILON);
    }

    #[test]
    fn quadratic_curve_accelerates() {
        let curve = ResponseCurve::quadratic(10.0);
        let at_quarter = curve.evaluate(0.25);
        let at_half = curve.evaluate(0.5);
        let at_three_quarters = curve.evaluate(0.75);
        // Quadratic: f(0.25)=0.625, f(0.5)=2.5, f(0.75)=5.625
        assert!(at_quarter < at_half);
        assert!(at_half < at_three_quarters);
        // Should accelerate: gap between 0.5->0.75 > gap between 0.25->0.5
        assert!((at_three_quarters - at_half) > (at_half - at_quarter));
    }

    #[test]
    fn logistic_curve_s_shape() {
        let curve = ResponseCurve::logistic(10.0);
        let low = curve.evaluate(0.1);
        let mid = curve.evaluate(0.5);
        let high = curve.evaluate(0.9);
        // Should be S-shaped: low at 0.1, ~5 at 0.5, high at 0.9.
        assert!(low < 3.0, "low end should be < 3, got {low}");
        assert!((mid - 5.0).abs() < 1.0, "midpoint should be ~5, got {mid}");
        assert!(high > 7.0, "high end should be > 7, got {high}");
    }

    #[test]
    fn step_curve_binary() {
        let curve = ResponseCurve::step(0.5, 10.0);
        assert!((curve.evaluate(0.4) - 0.0).abs() < f32::EPSILON);
        assert!((curve.evaluate(0.5) - 10.0).abs() < f32::EPSILON);
        assert!((curve.evaluate(0.9) - 10.0).abs() < f32::EPSILON);
    }

    #[test]
    fn constant_curve_always_same() {
        let curve = ResponseCurve::Constant { value: 7.0 };
        assert!((curve.evaluate(0.0) - 7.0).abs() < f32::EPSILON);
        assert!((curve.evaluate(0.5) - 7.0).abs() < f32::EPSILON);
        assert!((curve.evaluate(1.0) - 7.0).abs() < f32::EPSILON);
    }

    #[test]
    fn input_clamped_to_0_1() {
        let curve = ResponseCurve::linear(10.0);
        // Negative input should clamp to 0.
        assert!((curve.evaluate(-1.0) - 0.0).abs() < f32::EPSILON);
        // Input > 1 should clamp to 1.
        assert!((curve.evaluate(2.0) - 10.0).abs() < f32::EPSILON);
    }

    // -----------------------------------------------------------------------
    // ActionClass tests
    // -----------------------------------------------------------------------

    #[test]
    fn action_class_ordering() {
        // Reaction base > Support base > Proactive base > Basic base.
        assert!(ActionClass::Reaction.base_score() > ActionClass::Support.base_score());
        assert!(ActionClass::Support.base_score() > ActionClass::Proactive.base_score());
        assert!(ActionClass::Proactive.base_score() > ActionClass::Basic.base_score());
    }

    #[test]
    fn reaction_always_beats_proactive_max() {
        let reaction_min = ActionClass::Reaction.base_score();
        let proactive_max =
            ActionClass::Proactive.base_score() + ActionClass::Proactive.dynamic_range();
        assert!(
            reaction_min > proactive_max,
            "reaction minimum ({reaction_min}) should beat proactive maximum ({proactive_max})"
        );
    }

    // -----------------------------------------------------------------------
    // Consideration tests
    // -----------------------------------------------------------------------

    #[test]
    fn consideration_respects_filter() {
        let consideration = Consideration {
            name: "test".into(),
            filter: Some(Filter {
                name: "always_fail".into(),
                gate: Box::new(|_| false),
            }),
            input_fn: Box::new(|_| 1.0),
            curve: ResponseCurve::Constant { value: 100.0 },
        };
        let ctx = ScoringContext::default();
        assert!((consideration.evaluate(&ctx) - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn consideration_without_filter_always_fires() {
        let consideration = Consideration {
            name: "test".into(),
            filter: None,
            input_fn: Box::new(|_| 0.5),
            curve: ResponseCurve::linear(10.0),
        };
        let ctx = ScoringContext::default();
        assert!((consideration.evaluate(&ctx) - 5.0).abs() < f32::EPSILON);
    }

    // -----------------------------------------------------------------------
    // Behavior tests
    // -----------------------------------------------------------------------

    #[test]
    fn behavior_additive_composition() {
        let behavior = Behavior {
            id: "test".into(),
            class: ActionClass::Proactive, // base = 20
            viable: None,
            considerations: vec![
                Consideration {
                    name: "a".into(),
                    filter: None,
                    input_fn: Box::new(|_| 1.0),
                    curve: ResponseCurve::Constant { value: 5.0 },
                },
                Consideration {
                    name: "b".into(),
                    filter: None,
                    input_fn: Box::new(|_| 1.0),
                    curve: ResponseCurve::Constant { value: 7.0 },
                },
            ],
        };
        let ctx = ScoringContext::default();
        // base(20) + 5 + 7 = 32. But clamped to base + dynamic_range = 20 + 20 = 40. 32 < 40, so 32.
        assert!((behavior.evaluate(&ctx) - 32.0).abs() < f32::EPSILON);
    }

    #[test]
    fn behavior_clamped_to_class_range() {
        let behavior = Behavior {
            id: "test".into(),
            class: ActionClass::Proactive, // base=20, range=20, max=40
            viable: None,
            considerations: vec![Consideration {
                name: "huge".into(),
                filter: None,
                input_fn: Box::new(|_| 1.0),
                curve: ResponseCurve::Constant { value: 100.0 }, // way over range
            }],
        };
        let ctx = ScoringContext::default();
        // Clamped to 20 + 20 = 40.
        assert!((behavior.evaluate(&ctx) - 40.0).abs() < f32::EPSILON);
    }

    // -----------------------------------------------------------------------
    // Momentum tests
    // -----------------------------------------------------------------------

    #[test]
    fn momentum_bonus_for_active_behavior() {
        let mut momentum = Momentum::new();
        momentum.set_active("fix_error");
        let bonus = momentum.bonus_for("fix_error");
        // Should be close to initial_bonus (10.0) since we just set it.
        assert!(bonus > 9.0, "expected ~10, got {bonus}");
    }

    #[test]
    fn momentum_zero_for_different_behavior() {
        let mut momentum = Momentum::new();
        momentum.set_active("fix_error");
        let bonus = momentum.bonus_for("explain_error");
        assert!((bonus - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn momentum_zero_when_cleared() {
        let mut momentum = Momentum::new();
        momentum.set_active("fix_error");
        momentum.clear();
        let bonus = momentum.bonus_for("fix_error");
        assert!((bonus - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn momentum_zero_when_no_active() {
        let momentum = Momentum::new();
        let bonus = momentum.bonus_for("anything");
        assert!((bonus - 0.0).abs() < f32::EPSILON);
    }

    // -----------------------------------------------------------------------
    // BDS integration tests
    // -----------------------------------------------------------------------

    #[test]
    fn bds_picks_highest_scorer() {
        let mut bds = BehaviorDecisionSystem::new();

        // Register a Basic behavior (max score ~10).
        bds.register(Behavior {
            id: "idle".into(),
            class: ActionClass::Basic,
            viable: None,
            considerations: vec![Consideration {
                name: "always".into(),
                filter: None,
                input_fn: Box::new(|_| 1.0),
                curve: ResponseCurve::Constant { value: 5.0 },
            }],
        });

        // Register a Reaction behavior that fires on errors.
        bds.register(Behavior {
            id: "fix_error".into(),
            class: ActionClass::Reaction,
            viable: None,
            considerations: vec![Consideration {
                name: "has_errors".into(),
                filter: Some(Filter {
                    name: "errors".into(),
                    gate: Box::new(|ctx| ctx.has_errors),
                }),
                input_fn: Box::new(|_| 1.0),
                curve: ResponseCurve::Constant { value: 10.0 },
            }],
        });

        // Context with errors.
        let ctx = ScoringContext {
            has_errors: true,
            idle_secs: 5.0,
            ..Default::default()
        };

        let snapshot = bds.evaluate(&ctx);
        assert_eq!(snapshot.winner_id, "fix_error");
        // fix_error: base(50) + 10 = 60. idle: base(0) + 5 = 5.
        assert!(snapshot.winner_score > 50.0);
    }

    #[test]
    fn bds_idle_wins_when_no_errors() {
        let mut bds = BehaviorDecisionSystem::new();

        bds.register(Behavior {
            id: "idle".into(),
            class: ActionClass::Basic,
            viable: None,
            considerations: vec![Consideration {
                name: "always".into(),
                filter: None,
                input_fn: Box::new(|_| 1.0),
                curve: ResponseCurve::Constant { value: 5.0 },
            }],
        });

        // fix_error now has a viability gate: only applicable when has_errors.
        bds.register(Behavior {
            id: "fix_error".into(),
            class: ActionClass::Reaction,
            viable: Some(Box::new(|ctx| ctx.has_errors)),
            considerations: vec![Consideration {
                name: "has_errors".into(),
                filter: Some(Filter {
                    name: "errors".into(),
                    gate: Box::new(|ctx| ctx.has_errors),
                }),
                input_fn: Box::new(|_| 1.0),
                curve: ResponseCurve::Constant { value: 10.0 },
            }],
        });

        // No errors — fix_error.viable returns false → scores 0.
        // idle: base(0) + 5 = 5. idle wins.
        let ctx = ScoringContext::default();
        let snapshot = bds.evaluate(&ctx);
        assert_eq!(snapshot.winner_id, "idle", "idle should win when no errors");
        assert!((snapshot.winner_score - 5.0).abs() < f32::EPSILON);
    }

    #[test]
    fn bds_momentum_prevents_thrashing() {
        let mut bds = BehaviorDecisionSystem::new();

        bds.register(Behavior {
            id: "a".into(),
            class: ActionClass::Proactive,
            viable: None,
            considerations: vec![Consideration {
                name: "score_a".into(),
                filter: None,
                input_fn: Box::new(|_| 1.0),
                curve: ResponseCurve::Constant { value: 15.0 },
            }],
        });

        bds.register(Behavior {
            id: "b".into(),
            class: ActionClass::Proactive,
            viable: None,
            considerations: vec![Consideration {
                name: "score_b".into(),
                filter: None,
                input_fn: Box::new(|_| 1.0),
                curve: ResponseCurve::Constant { value: 16.0 }, // 1 point higher than a
            }],
        });

        let ctx = ScoringContext::default();

        // First eval: b wins (36 vs 35).
        let snap1 = bds.evaluate(&ctx);
        assert_eq!(snap1.winner_id, "b");

        // Second eval immediately after: b has momentum bonus ~10.
        // b: 36 + 10 = 46. a: 35 + 0 = 35. b still wins.
        let snap2 = bds.evaluate(&ctx);
        assert_eq!(snap2.winner_id, "b");

        // If we force a to have a much higher score, it should still overcome momentum.
        // (Can't easily test time-based decay in a unit test without sleeping.)
    }

    #[test]
    fn default_behaviors_build_successfully() {
        let behaviors = build_default_behaviors();
        assert!(
            behaviors.len() >= 7,
            "expected at least 7 behaviors, got {}",
            behaviors.len()
        );

        // Verify all behaviors have at least one consideration.
        for b in &behaviors {
            assert!(
                !b.considerations.is_empty() || b.id == "quiet",
                "behavior {} has no considerations",
                b.id
            );
        }
    }

    #[test]
    fn default_fix_error_beats_quiet_on_errors() {
        let behaviors = build_default_behaviors();
        let ctx = ScoringContext {
            has_errors: true,
            error_count: 3,
            idle_secs: 5.0,
            ..Default::default()
        };

        let quiet_score = behaviors
            .iter()
            .find(|b| b.id == "quiet")
            .unwrap()
            .evaluate(&ctx);

        let fix_score = behaviors
            .iter()
            .find(|b| b.id == "fix_error")
            .unwrap()
            .evaluate(&ctx);

        assert!(
            fix_score > quiet_score,
            "fix_error ({fix_score}) should beat quiet ({quiet_score}) when errors present"
        );
    }

    // -----------------------------------------------------------------------
    // UtilityCurve trait tests
    // -----------------------------------------------------------------------

    // -- LinearCurve ---------------------------------------------------------

    #[test]
    fn linear_curve_at_zero() {
        let c = LinearCurve::rising();
        assert_eq!(c.score(0.0), 0.0);
    }

    #[test]
    fn linear_curve_at_half() {
        let c = LinearCurve::rising();
        assert!((c.score(0.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn linear_curve_at_one() {
        let c = LinearCurve::rising();
        assert_eq!(c.score(1.0), 1.0);
    }

    #[test]
    fn linear_falling_is_inverse_of_rising() {
        let rising = LinearCurve::rising();
        let falling = LinearCurve::falling();
        for x in [0.0f32, 0.25, 0.5, 0.75, 1.0] {
            let sum = rising.score(x) + falling.score(x);
            assert!(
                (sum - 1.0).abs() < 1e-6,
                "rising + falling should sum to 1 at x={x}, got {sum}"
            );
        }
    }

    #[test]
    fn linear_clamps_input_below_zero() {
        let c = LinearCurve::rising();
        assert_eq!(c.score(-5.0), 0.0);
    }

    #[test]
    fn linear_clamps_input_above_one() {
        let c = LinearCurve::rising();
        assert_eq!(c.score(2.0), 1.0);
    }

    // -- ExponentialCurve ----------------------------------------------------

    #[test]
    fn exponential_quadratic_at_zero() {
        let c = ExponentialCurve::quadratic();
        assert_eq!(c.score(0.0), 0.0);
    }

    #[test]
    fn exponential_quadratic_at_one() {
        let c = ExponentialCurve::quadratic();
        assert_eq!(c.score(1.0), 1.0);
    }

    #[test]
    fn exponential_quadratic_at_half_is_below_linear() {
        let c = ExponentialCurve::quadratic();
        // x^2 at 0.5 = 0.25, which is less than linear 0.5
        assert!(
            c.score(0.5) < 0.5,
            "quadratic should score below linear midpoint"
        );
        assert!((c.score(0.5) - 0.25).abs() < 1e-6);
    }

    #[test]
    fn exponential_sqrt_at_half_is_above_linear() {
        let c = ExponentialCurve::sqrt();
        // sqrt(0.5) ≈ 0.707 > 0.5
        assert!(
            c.score(0.5) > 0.5,
            "sqrt should score above linear midpoint"
        );
    }

    #[test]
    fn exponential_monotonically_increasing() {
        let c = ExponentialCurve::quadratic();
        let samples = [0.0, 0.1, 0.3, 0.5, 0.7, 0.9, 1.0];
        for window in samples.windows(2) {
            let (lo, hi) = (window[0], window[1]);
            assert!(
                c.score(hi) >= c.score(lo),
                "ExponentialCurve should be monotonic: score({hi}) < score({lo})"
            );
        }
    }

    // -- LogisticCurve -------------------------------------------------------

    #[test]
    fn logistic_at_zero_is_low() {
        let c = LogisticCurve::standard();
        assert!(
            c.score(0.0) < 0.1,
            "logistic at x=0 should be < 0.1, got {}",
            c.score(0.0)
        );
    }

    #[test]
    fn logistic_at_half_is_midpoint() {
        let c = LogisticCurve::standard();
        // Sigmoid(0) = 0.5 exactly.
        assert!(
            (c.score(0.5) - 0.5).abs() < 1e-5,
            "logistic at midpoint should be 0.5, got {}",
            c.score(0.5)
        );
    }

    #[test]
    fn logistic_at_one_is_high() {
        let c = LogisticCurve::standard();
        assert!(
            c.score(1.0) > 0.9,
            "logistic at x=1 should be > 0.9, got {}",
            c.score(1.0)
        );
    }

    #[test]
    fn logistic_is_monotonically_increasing() {
        let c = LogisticCurve::standard();
        let samples = [0.0, 0.1, 0.2, 0.4, 0.5, 0.6, 0.8, 1.0];
        for window in samples.windows(2) {
            let (lo, hi) = (window[0], window[1]);
            assert!(
                c.score(hi) > c.score(lo),
                "LogisticCurve should be strictly increasing: score({hi})={} < score({lo})={}",
                c.score(hi),
                c.score(lo)
            );
        }
    }

    #[test]
    fn logistic_output_always_in_range() {
        let c = LogisticCurve::new(0.5, 50.0); // very steep
        for x in [0.0f32, 0.499, 0.5, 0.501, 1.0] {
            let s = c.score(x);
            assert!(
                s >= 0.0 && s <= 1.0,
                "logistic output {s} out of [0,1] at x={x}"
            );
        }
    }

    // -- StepCurve -----------------------------------------------------------

    #[test]
    fn step_below_threshold_returns_off() {
        let c = StepCurve::on_at(0.5);
        assert_eq!(c.score(0.0), 0.0);
        assert_eq!(c.score(0.49), 0.0);
    }

    #[test]
    fn step_at_threshold_returns_on() {
        let c = StepCurve::on_at(0.5);
        assert_eq!(c.score(0.5), 1.0);
    }

    #[test]
    fn step_above_threshold_returns_on() {
        let c = StepCurve::on_at(0.5);
        assert_eq!(c.score(1.0), 1.0);
    }

    #[test]
    fn step_custom_on_off_values() {
        let c = StepCurve::new(0.7, 0.8, 0.2);
        assert!((c.score(0.5) - 0.2).abs() < 1e-6);
        assert!((c.score(0.7) - 0.8).abs() < 1e-6);
        assert!((c.score(1.0) - 0.8).abs() < 1e-6);
    }

    // -- InvertedCurve -------------------------------------------------------

    #[test]
    fn inverted_at_zero_is_one() {
        let c = InvertedCurve::of(LinearCurve::rising());
        assert_eq!(c.score(0.0), 1.0);
    }

    #[test]
    fn inverted_at_half_is_half() {
        let c = InvertedCurve::of(LinearCurve::rising());
        assert!((c.score(0.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn inverted_at_one_is_zero() {
        let c = InvertedCurve::of(LinearCurve::rising());
        assert_eq!(c.score(1.0), 0.0);
    }

    #[test]
    fn inverted_output_always_in_range() {
        let c = InvertedCurve::of(LogisticCurve::sharp());
        for x in [0.0f32, 0.25, 0.5, 0.75, 1.0] {
            let s = c.score(x);
            assert!(
                s >= 0.0 && s <= 1.0,
                "inverted output {s} out of [0,1] at x={x}"
            );
        }
    }

    // -- CompositeCurve ------------------------------------------------------

    #[test]
    fn composite_at_zero() {
        let c = CompositeCurve::build(vec![
            (Box::new(LinearCurve::rising()), 0.5),
            (Box::new(LogisticCurve::standard()), 0.5),
        ])
        .unwrap();
        // Both curves score ~0 at x=0; weighted avg is also ~0.
        assert!(c.score(0.0) < 0.1);
    }

    #[test]
    fn composite_at_half() {
        let c = CompositeCurve::build(vec![
            (Box::new(LinearCurve::rising()), 1.0), // 0.5
            (Box::new(LinearCurve::rising()), 1.0), // 0.5
        ])
        .unwrap();
        assert!((c.score(0.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn composite_at_one() {
        let c = CompositeCurve::build(vec![
            (Box::new(LinearCurve::rising()), 0.6),
            (Box::new(ExponentialCurve::quadratic()), 0.4),
        ])
        .unwrap();
        // Both score 1.0 at x=1; composite should also be 1.0.
        assert!((c.score(1.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn composite_weights_sum_guard_rejects_negative() {
        let result = CompositeCurve::build(vec![
            (Box::new(LinearCurve::rising()), 0.6),
            (Box::new(LinearCurve::rising()), -0.1), // negative weight
        ]);
        assert!(
            result.is_err(),
            "CompositeCurve::build should reject negative weights"
        );
    }

    #[test]
    fn composite_output_always_in_range() {
        let c = CompositeCurve::build(vec![
            (Box::new(LinearCurve::rising()), 0.3),
            (Box::new(LogisticCurve::standard()), 0.4),
            (Box::new(ExponentialCurve::sqrt()), 0.3),
        ])
        .unwrap();
        for x in [0.0f32, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0] {
            let s = c.score(x);
            assert!(
                s >= 0.0 && s <= 1.0,
                "composite output {s} out of [0,1] at x={x}"
            );
        }
    }

    #[test]
    fn composite_unequal_weights_bias_toward_heavier_curve() {
        // Heavy curve: step that's always 1.0. Light curve: linear rising (0.5 at x=0.5).
        // With weights 0.9 / 0.1, result should be much closer to 1.0 than 0.5.
        let c = CompositeCurve::build(vec![
            (Box::new(StepCurve::always_on()), 0.9),
            (Box::new(LinearCurve::rising()), 0.1),
        ])
        .unwrap();
        let score = c.score(0.5);
        // weighted avg = (0.9 * 1.0 + 0.1 * 0.5) / 1.0 = 0.95
        assert!((score - 0.95).abs() < 1e-6, "expected 0.95, got {score}");
    }

    #[test]
    fn composite_empty_returns_zero() {
        let c = CompositeCurve::new(vec![]);
        assert_eq!(c.score(0.5), 0.0);
    }

    #[test]
    fn composite_monotonic_when_all_components_monotonic() {
        let c = CompositeCurve::build(vec![
            (Box::new(LinearCurve::rising()), 0.5),
            (Box::new(ExponentialCurve::quadratic()), 0.5),
        ])
        .unwrap();
        let samples = [0.0, 0.1, 0.3, 0.5, 0.7, 0.9, 1.0];
        for window in samples.windows(2) {
            let (lo, hi) = (window[0], window[1]);
            assert!(
                c.score(hi) >= c.score(lo),
                "CompositeCurve should be monotonic: score({hi}) < score({lo})"
            );
        }
    }

    // -- Send + Sync (compile-time checks) -----------------------------------

    #[test]
    fn utility_curve_implementations_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LinearCurve>();
        assert_send_sync::<ExponentialCurve>();
        assert_send_sync::<LogisticCurve>();
        assert_send_sync::<StepCurve>();
        // InvertedCurve and CompositeCurve hold Box<dyn UtilityCurve>
        // which is Send+Sync because UtilityCurve: Send + Sync.
        assert_send_sync::<InvertedCurve>();
        assert_send_sync::<CompositeCurve>();
    }
}
