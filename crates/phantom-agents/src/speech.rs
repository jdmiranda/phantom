//! Speech events: how agents emit utterances toward the user, peer agents,
//! or the broader fabric.
//!
//! Every spoken thing an agent produces is a [`SpeakEvent`]. The event is
//! self-describing about who spoke, who they spoke to, what they said, and
//! how loudly they wanted it heard. The downstream renderer / router uses
//! [`SpeakEvent::audience`] to decide whether the line bubbles up to the
//! user-visible chrome or stays internal between agents.

use crate::role::{AgentId, AgentRef};
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Targets, bodies, audiences
// ---------------------------------------------------------------------------

/// Who an utterance is addressed to.
#[derive(Debug, Clone)]
pub enum SpeakTarget {
    /// The user. Always renders in user-visible chrome.
    User,
    /// A specific peer agent. Internal coordination only.
    Agent(AgentId),
    /// Anyone listening. Visible to the user iff important enough.
    Broadcast,
}

/// What was actually said.
#[derive(Debug, Clone)]
pub enum SpeakBody {
    /// Plain text utterance.
    Text(String),
    /// Heartbeat / liveness ping. Carries no content.
    StatusPing,
    /// A structured notification, e.g. "memory.bundle_written".
    Notification {
        kind: String,
        ref_uri: Option<String>,
    },
    /// A reference to an in-flight stream (token stream, audio chunk feed).
    Stream { stream_id: u64 },
}

/// Whether an event should surface in the user-visible chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeechAudience {
    UserVisible,
    InternalOnly,
}

// ---------------------------------------------------------------------------
// SpeakEvent
// ---------------------------------------------------------------------------

/// A single utterance from an agent. The atomic unit of agent speech.
#[derive(Debug, Clone)]
pub struct SpeakEvent {
    pub from: AgentRef,
    pub to: SpeakTarget,
    pub body: SpeakBody,
    /// Saliency in [0.0, 1.0]. Constructors clamp out-of-range values.
    pub importance: f32,
    pub at_unix_ms: u64,
}

impl SpeakEvent {
    /// Construct a text utterance addressed to the user. Importance is
    /// clamped to `[0.0, 1.0]`.
    pub fn text_to_user(from: AgentRef, body: impl Into<String>, importance: f32) -> Self {
        Self {
            from,
            to: SpeakTarget::User,
            body: SpeakBody::Text(body.into()),
            importance: clamp_importance(importance),
            at_unix_ms: now_unix_ms(),
        }
    }

    /// Construct a text utterance addressed to a specific peer agent.
    /// Internal-only by definition; importance is fixed at 0.0 since the
    /// user-visible saliency channel doesn't apply.
    pub fn text_to_agent(from: AgentRef, target: AgentId, body: impl Into<String>) -> Self {
        Self {
            from,
            to: SpeakTarget::Agent(target),
            body: SpeakBody::Text(body.into()),
            importance: 0.0,
            at_unix_ms: now_unix_ms(),
        }
    }

    /// Construct a heartbeat ping. Carries no payload and is never
    /// individually salient.
    pub fn status_ping(from: AgentRef) -> Self {
        Self {
            from,
            to: SpeakTarget::Broadcast,
            body: SpeakBody::StatusPing,
            importance: 0.0,
            at_unix_ms: now_unix_ms(),
        }
    }

    /// Whether this event should render in the user-visible chrome.
    ///
    /// - `User` → always [`SpeechAudience::UserVisible`].
    /// - `Agent(_)` → always [`SpeechAudience::InternalOnly`].
    /// - `Broadcast` → user-visible iff `importance >= 0.5`.
    pub fn audience(&self) -> SpeechAudience {
        match self.to {
            SpeakTarget::User => SpeechAudience::UserVisible,
            SpeakTarget::Agent(_) => SpeechAudience::InternalOnly,
            SpeakTarget::Broadcast => {
                if self.importance >= 0.5 {
                    SpeechAudience::UserVisible
                } else {
                    SpeechAudience::InternalOnly
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn clamp_importance(v: f32) -> f32 {
    if v.is_nan() {
        0.0
    } else {
        v.clamp(0.0, 1.0)
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::role::{AgentRole, SpawnSource};

    fn fixture_ref() -> AgentRef {
        AgentRef::new(1, AgentRole::Conversational, "talker", SpawnSource::User)
    }

    #[test]
    fn text_to_user_targets_user_and_clamps_importance() {
        let ev = SpeakEvent::text_to_user(fixture_ref(), "hello", 0.7);
        assert!(matches!(ev.to, SpeakTarget::User));
        match &ev.body {
            SpeakBody::Text(s) => assert_eq!(s, "hello"),
            other => panic!("expected Text, got {other:?}"),
        }
        assert!((ev.importance - 0.7).abs() < f32::EPSILON);
    }

    #[test]
    fn text_to_agent_targets_specific_peer() {
        let ev = SpeakEvent::text_to_agent(fixture_ref(), 99, "ack");
        match ev.to {
            SpeakTarget::Agent(id) => assert_eq!(id, 99),
            other => panic!("expected Agent(99), got {other:?}"),
        }
    }

    #[test]
    fn status_ping_body_and_importance() {
        let ev = SpeakEvent::status_ping(fixture_ref());
        assert!(matches!(ev.body, SpeakBody::StatusPing));
        assert_eq!(ev.importance, 0.0);
    }

    #[test]
    fn audience_user_is_visible() {
        let ev = SpeakEvent::text_to_user(fixture_ref(), "hi", 0.0);
        assert_eq!(ev.audience(), SpeechAudience::UserVisible);
    }

    #[test]
    fn audience_agent_is_internal() {
        let ev = SpeakEvent::text_to_agent(fixture_ref(), 7, "internal");
        assert_eq!(ev.audience(), SpeechAudience::InternalOnly);
    }

    #[test]
    fn audience_broadcast_high_importance_is_visible() {
        let ev = SpeakEvent {
            from: fixture_ref(),
            to: SpeakTarget::Broadcast,
            body: SpeakBody::Text("loud".into()),
            importance: 0.6,
            at_unix_ms: now_unix_ms(),
        };
        assert_eq!(ev.audience(), SpeechAudience::UserVisible);
    }

    #[test]
    fn audience_broadcast_low_importance_is_internal() {
        let ev = SpeakEvent {
            from: fixture_ref(),
            to: SpeakTarget::Broadcast,
            body: SpeakBody::Text("quiet".into()),
            importance: 0.3,
            at_unix_ms: now_unix_ms(),
        };
        assert_eq!(ev.audience(), SpeechAudience::InternalOnly);
    }

    #[test]
    fn audience_broadcast_threshold_inclusive_at_half() {
        // The rule is `>= 0.5`. Boundary case must be UserVisible.
        let ev = SpeakEvent {
            from: fixture_ref(),
            to: SpeakTarget::Broadcast,
            body: SpeakBody::StatusPing,
            importance: 0.5,
            at_unix_ms: now_unix_ms(),
        };
        assert_eq!(ev.audience(), SpeechAudience::UserVisible);
    }

    #[test]
    fn at_unix_ms_is_nonzero_for_fresh_events() {
        let ev = SpeakEvent::text_to_user(fixture_ref(), "now", 0.1);
        assert!(ev.at_unix_ms > 0);
    }

    #[test]
    fn importance_above_one_clamps_to_one() {
        let ev = SpeakEvent::text_to_user(fixture_ref(), "loud", 1.5);
        assert_eq!(ev.importance, 1.0);
    }

    #[test]
    fn importance_below_zero_clamps_to_zero() {
        let ev = SpeakEvent::text_to_user(fixture_ref(), "negative", -0.2);
        assert_eq!(ev.importance, 0.0);
    }
}
