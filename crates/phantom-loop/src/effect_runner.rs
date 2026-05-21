//! Execution side of [`crate::LoopEffect`].
//!
//! Where [`crate::effect`] declares *what* effects exist, this module
//! declares *how* the runner applies them after a successful iteration.
//!
//! # Field-map semantics
//!
//! [`crate::effect::FieldMap::from`] is a dotted JSON path against the
//! iteration result. We support:
//!
//! - Top-level object lookup: `"pr_url"` → `result["pr_url"]`.
//! - Nested object lookup: `"result.pr_url"` → `result["result"]["pr_url"]`.
//! - Numeric array indexing: `"items.0.name"` → `result["items"][0]["name"]`.
//!
//! We do *not* support JSON-Pointer escapes (`~0` / `~1`) — the spec
//! examples in issue #650 all use simple dotted names, and the runtime
//! decision (proper JSON Pointer vs. dotted) is small enough to revisit
//! in a follow-up if a user spec ever needs an escape.

use serde_json::Value;

use crate::effect::{FieldMap, LoopEffect};
use crate::queue::{LoopMessage, LoopQueueRegistry};

/// Context handed to [`run_effects`].
///
/// Borrowed-only — the effect runner never extends a lifetime past the
/// call. `result` is the agent's `complete_task` output (or `Value::Null`
/// for agentless loops); `from_loop` is the producing loop's id (used to
/// stamp [`LoopMessage::from_loop`]); `queues` is the shared registry
/// every [`LoopEffect::EnqueueTo`] dispatches through.
pub struct EffectContext<'a> {
    pub result: &'a Value,
    pub from_loop: &'a str,
    pub queues: &'a LoopQueueRegistry,
}

/// Failure mode of [`run_effects`].
///
/// Today there is exactly one error variant: a [`FieldMap`] points at a
/// path that does not resolve in `result`. The runner treats this as
/// terminal — silently dropping a misconfigured mapping would mean
/// shipping malformed messages downstream, which is worse than stopping
/// the loop.
#[derive(Debug, thiserror::Error)]
pub enum EffectError {
    /// A [`FieldMap::from`] path did not resolve against `result`. The
    /// runner records `effect_idx` (position in `on_complete`) and the
    /// missing path for diagnostics.
    #[error("effect #{effect_idx}: field-map path `{path}` not found in result")]
    FieldMapMissing { effect_idx: usize, path: String },
}

/// What happened during effect dispatch.
///
/// Right now this only tracks whether any effect requested `stop_loop`,
/// but the type exists so future variants (counts, durations, structured
/// log events) can land without churning the runner.
#[derive(Debug, Default)]
pub struct EffectOutcome {
    /// `true` if a [`LoopEffect::StopLoop`] effect fired. The runner
    /// transitions to `Stopped` after the current iteration's effects
    /// complete.
    pub stop_requested: bool,
}

/// Apply all `effects` to `ctx`.
///
/// Effects run in the order declared by the spec. A failed
/// [`LoopEffect::EnqueueTo`] field-map short-circuits remaining effects
/// and propagates [`EffectError::FieldMapMissing`].
///
/// # Errors
///
/// Returns [`EffectError::FieldMapMissing`] when an `enqueue_to` effect's
/// `fields` references a path that does not exist in `ctx.result`.
pub fn run_effects(
    effects: &[LoopEffect],
    ctx: &EffectContext<'_>,
) -> Result<EffectOutcome, EffectError> {
    let mut outcome = EffectOutcome::default();
    for (idx, effect) in effects.iter().enumerate() {
        match effect {
            LoopEffect::EnqueueTo { queue, fields } => {
                let payload = build_payload(fields, ctx.result, idx)?;
                let msg = LoopMessage::new(ctx.from_loop.to_owned(), payload);
                ctx.queues.push(queue, msg);
                tracing::debug!(
                    effect_idx = idx,
                    from_loop = ctx.from_loop,
                    queue = %queue,
                    "enqueued message",
                );
            }
            LoopEffect::LogToBus { event_kind } => {
                // C2 stub: no bus reference yet. C3 wires this to the real
                // cross-loop event bus.
                tracing::info!(
                    effect_idx = idx,
                    from_loop = ctx.from_loop,
                    event_kind = %event_kind,
                    "log_to_bus (C2 stub — bus wiring lands in C3)",
                );
            }
            LoopEffect::StopLoop => {
                tracing::info!(effect_idx = idx, "stop_loop effect requested");
                outcome.stop_requested = true;
            }
        }
    }
    Ok(outcome)
}

/// Build a [`LoopMessage::payload`] from a list of [`FieldMap`]s.
///
/// Each mapping copies one value (resolved by dotted path) under its
/// `to` key. The destination is always a flat top-level object — no
/// nested `to` paths in this slice.
fn build_payload(
    fields: &[FieldMap],
    result: &Value,
    effect_idx: usize,
) -> Result<Value, EffectError> {
    if fields.is_empty() {
        // Empty mapping = forward the whole result verbatim. Callers
        // running agentless loops typically pass `Value::Null` here, which
        // becomes the message body for the receiving loop to interpret.
        return Ok(result.clone());
    }

    let mut payload = serde_json::Map::with_capacity(fields.len());
    for FieldMap { from, to } in fields {
        let v = resolve_dotted(result, from).ok_or_else(|| EffectError::FieldMapMissing {
            effect_idx,
            path: from.clone(),
        })?;
        payload.insert(to.clone(), v.clone());
    }
    Ok(Value::Object(payload))
}

/// Resolve a dotted path inside a JSON value.
///
/// Segments are split on `.`. Numeric segments are treated as array
/// indexes when the current value is an array; otherwise they index the
/// object as a string key (which is how serde_json would render them
/// anyway).
fn resolve_dotted<'v>(root: &'v Value, path: &str) -> Option<&'v Value> {
    let mut cur = root;
    for segment in path.split('.') {
        match cur {
            Value::Object(map) => {
                cur = map.get(segment)?;
            }
            Value::Array(arr) => {
                let idx: usize = segment.parse().ok()?;
                cur = arr.get(idx)?;
            }
            _ => return None,
        }
    }
    Some(cur)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolve_dotted_top_level() {
        let v = json!({"a": 1});
        assert_eq!(resolve_dotted(&v, "a"), Some(&json!(1)));
    }

    #[test]
    fn resolve_dotted_nested_object() {
        let v = json!({"result": {"pr_url": "https://x/1"}});
        assert_eq!(
            resolve_dotted(&v, "result.pr_url"),
            Some(&json!("https://x/1"))
        );
    }

    #[test]
    fn resolve_dotted_array_index() {
        let v = json!({"items": ["a", "b", "c"]});
        assert_eq!(resolve_dotted(&v, "items.1"), Some(&json!("b")));
    }

    #[test]
    fn resolve_dotted_missing_returns_none() {
        let v = json!({"a": 1});
        assert!(resolve_dotted(&v, "b").is_none());
        assert!(resolve_dotted(&v, "a.x").is_none());
    }

    #[test]
    fn enqueue_to_with_field_map_builds_flat_payload() {
        let reg = LoopQueueRegistry::new();
        let effects = vec![LoopEffect::EnqueueTo {
            queue: "review-queue".to_string(),
            fields: vec![FieldMap {
                from: "result.pr_url".to_string(),
                to: "target_pr".to_string(),
            }],
        }];
        let result = json!({"result": {"pr_url": "https://github.com/x/y/pull/1"}});
        let ctx = EffectContext {
            result: &result,
            from_loop: "pr-finder",
            queues: &reg,
        };
        let outcome = run_effects(&effects, &ctx).expect("ok");
        assert!(!outcome.stop_requested);

        let msg = reg.pop("review-queue").expect("one message enqueued");
        assert_eq!(msg.from_loop, "pr-finder");
        assert_eq!(
            msg.payload["target_pr"],
            "https://github.com/x/y/pull/1",
            "target_pr must hold the value resolved from result.pr_url"
        );
    }

    #[test]
    fn enqueue_to_with_empty_fields_forwards_whole_result() {
        let reg = LoopQueueRegistry::new();
        let effects = vec![LoopEffect::EnqueueTo {
            queue: "q".to_string(),
            fields: vec![],
        }];
        let result = json!({"a": 1, "b": 2});
        let ctx = EffectContext {
            result: &result,
            from_loop: "p",
            queues: &reg,
        };
        run_effects(&effects, &ctx).expect("ok");
        let msg = reg.pop("q").expect("enqueued");
        assert_eq!(msg.payload, json!({"a": 1, "b": 2}));
    }

    #[test]
    fn missing_field_map_path_errors() {
        let reg = LoopQueueRegistry::new();
        let effects = vec![LoopEffect::EnqueueTo {
            queue: "q".to_string(),
            fields: vec![FieldMap {
                from: "no.such.path".to_string(),
                to: "to".to_string(),
            }],
        }];
        let ctx = EffectContext {
            result: &json!({}),
            from_loop: "p",
            queues: &reg,
        };
        let err = run_effects(&effects, &ctx).expect_err("must error");
        match err {
            EffectError::FieldMapMissing { effect_idx, path } => {
                assert_eq!(effect_idx, 0);
                assert_eq!(path, "no.such.path");
            }
        }
    }

    #[test]
    fn stop_loop_effect_sets_outcome_flag() {
        let reg = LoopQueueRegistry::new();
        let effects = vec![LoopEffect::StopLoop];
        let ctx = EffectContext {
            result: &json!({}),
            from_loop: "p",
            queues: &reg,
        };
        let outcome = run_effects(&effects, &ctx).expect("ok");
        assert!(outcome.stop_requested);
    }

    #[test]
    fn log_to_bus_is_a_noop_today() {
        let reg = LoopQueueRegistry::new();
        let effects = vec![LoopEffect::LogToBus {
            event_kind: "pr_reviewed".to_string(),
        }];
        let ctx = EffectContext {
            result: &json!({}),
            from_loop: "p",
            queues: &reg,
        };
        let outcome = run_effects(&effects, &ctx).expect("ok");
        assert!(!outcome.stop_requested);
        // Bus stub: no enqueue, no error. The tracing line is best-effort.
    }
}
