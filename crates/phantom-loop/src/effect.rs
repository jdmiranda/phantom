//! Side effects fired after a loop iteration emits a valid exit payload.
//!
//! Each [`LoopEffect`] is declarative — it describes *what* should happen
//! after a successful iteration, not *how* the runner accomplishes it. The
//! C2 slice (the runner) is what dispatches these.

use serde::{Deserialize, Serialize};

/// A declarative post-iteration action.
///
/// `LoopEffect` is encoded as a tagged enum in TOML so the user can list
/// multiple effects against the same exit:
///
/// ```toml
/// [[on_complete]]
/// kind = "enqueue_to"
/// queue = "implementer-queue"
///
/// [[on_complete.fields]]
/// from = "result.pr_url"
/// to = "target_pr"
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoopEffect {
    /// Push a typed message onto a named queue. `fields` maps slots of the
    /// iteration result onto slots of the outgoing `LoopMessage` payload.
    EnqueueTo {
        queue: String,
        #[serde(default)]
        fields: Vec<FieldMap>,
    },

    /// Emit a custom event on the cross-loop bus. The runner will forward
    /// the iteration result verbatim under the given `event_kind`.
    LogToBus { event_kind: String },

    /// Halt the loop entirely. No further inputs are consumed. The runner
    /// transitions to a terminal `Stopped` state — restarting requires a
    /// fresh `phantom loop run`.
    StopLoop,
}

/// Maps a dotted path inside an iteration result onto a target field on the
/// outgoing message payload.
///
/// `from` is a JSON pointer-like dotted path (`result.pr_url`); `to` is a
/// flat field name on the `LoopMessage` payload. Future slices will decide
/// the exact resolution semantics for the dotted path; for C1 we only
/// preserve the strings verbatim.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FieldMap {
    pub from: String,
    pub to: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enqueue_to_with_field_map_roundtrips_through_toml() {
        let input = r#"
            kind = "enqueue_to"
            queue = "implementer-queue"

            [[fields]]
            from = "result.pr_url"
            to = "target_pr"
        "#;
        let effect: LoopEffect = toml::from_str(input).expect("parse enqueue_to");
        match effect {
            LoopEffect::EnqueueTo { queue, fields } => {
                assert_eq!(queue, "implementer-queue");
                assert_eq!(fields.len(), 1);
                assert_eq!(fields[0].from, "result.pr_url");
                assert_eq!(fields[0].to, "target_pr");
            }
            other => panic!("expected EnqueueTo, got {other:?}"),
        }
    }

    #[test]
    fn log_to_bus_parses_with_only_event_kind() {
        let input = r#"
            kind = "log_to_bus"
            event_kind = "pr_reviewed"
        "#;
        let effect: LoopEffect = toml::from_str(input).expect("parse log_to_bus");
        match effect {
            LoopEffect::LogToBus { event_kind } => assert_eq!(event_kind, "pr_reviewed"),
            other => panic!("expected LogToBus, got {other:?}"),
        }
    }

    #[test]
    fn stop_loop_parses_with_no_fields() {
        let input = r#"kind = "stop_loop""#;
        let effect: LoopEffect = toml::from_str(input).expect("parse stop_loop");
        assert!(matches!(effect, LoopEffect::StopLoop));
    }

    #[test]
    fn enqueue_to_with_no_fields_defaults_to_empty_vec() {
        let input = r#"
            kind = "enqueue_to"
            queue = "review-queue"
        "#;
        let effect: LoopEffect = toml::from_str(input).expect("parse enqueue_to no-fields");
        match effect {
            LoopEffect::EnqueueTo { queue, fields } => {
                assert_eq!(queue, "review-queue");
                assert!(fields.is_empty());
            }
            other => panic!("expected EnqueueTo, got {other:?}"),
        }
    }
}
