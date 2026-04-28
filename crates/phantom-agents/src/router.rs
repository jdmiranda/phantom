//! Mention routing — parses user input for `@<label>` and `:role:` prefixes
//! and decides where to deliver the message.
//!
//! Default route is to the Conversational agent. An explicit `@<label>` at
//! the start of the input routes to a specific agent's inbox. A `:role:`
//! prefix broadcasts to all agents of that role.
//!
//! Unknown mentions fall through to Conversational with a warning hint so
//! the user gets feedback (rather than silent drop).

use crate::role::{AgentId, AgentRef, AgentRole};

// ---------------------------------------------------------------------------
// Routing target
// ---------------------------------------------------------------------------

/// Where a parsed user message should be delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MentionTarget {
    /// Deliver to the default conversational agent.
    DefaultConversational,
    /// Deliver to one specific agent by label / id.
    Agent(AgentId),
    /// Broadcast to all agents of the given role.
    Role(AgentRole),
    /// Caller used `@something` but no agent matched. Fall back to default,
    /// surfacing the unmatched label so the UI can warn.
    Unmatched { tried: String },
}

/// Result of parsing one user message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMessage {
    pub target: MentionTarget,
    /// Body of the message after the prefix (if any) is stripped.
    pub body: String,
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Parse a raw user message for an optional leading `@<label>` or
/// `:<role>:` mention. The remainder is the message body.
///
/// Look-up policy:
/// - `@foo` matches any agent with `label == "foo"`.
/// - `:watcher:` matches role `Watcher`. Role names are case-insensitive.
/// - No prefix → DefaultConversational.
/// - Prefix present but unmatched → Unmatched { tried: "foo" }, body kept.
pub fn parse_mention<'a>(
    input: &str,
    agents: impl IntoIterator<Item = &'a AgentRef>,
) -> ParsedMessage {
    let trimmed = input.trim_start();

    // :role: prefix.
    if trimmed.starts_with(':') {
        if let Some(end) = trimmed[1..].find(':') {
            let role_token = &trimmed[1..=end]; // exclusive of trailing ':'
            let body = trimmed[(end + 2)..].trim_start().to_owned();
            if let Some(role) = parse_role_token(role_token) {
                return ParsedMessage { target: MentionTarget::Role(role), body };
            } else {
                return ParsedMessage {
                    target: MentionTarget::Unmatched { tried: role_token.to_owned() },
                    body,
                };
            }
        }
    }

    // @label prefix.
    if let Some(rest) = trimmed.strip_prefix('@') {
        let (label, body) = split_first_token(rest);
        let body = body.trim_start().to_owned();
        let agent_match = agents.into_iter().find(|a| a.label == label);
        if let Some(a) = agent_match {
            return ParsedMessage { target: MentionTarget::Agent(a.id), body };
        } else {
            return ParsedMessage {
                target: MentionTarget::Unmatched { tried: label.to_owned() },
                body,
            };
        }
    }

    // No prefix.
    ParsedMessage {
        target: MentionTarget::DefaultConversational,
        body: input.to_owned(),
    }
}

fn split_first_token(s: &str) -> (&str, &str) {
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    }
}

fn parse_role_token(t: &str) -> Option<AgentRole> {
    match t.to_ascii_lowercase().as_str() {
        "conversational" | "conv" | "chat" => Some(AgentRole::Conversational),
        "watcher" | "watch" => Some(AgentRole::Watcher),
        "capturer" | "capture" => Some(AgentRole::Capturer),
        "transcriber" | "transcribe" => Some(AgentRole::Transcriber),
        "reflector" => Some(AgentRole::Reflector),
        "indexer" => Some(AgentRole::Indexer),
        "actor" => Some(AgentRole::Actor),
        "composer" => Some(AgentRole::Composer),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::role::SpawnSource;

    fn ref_for(id: AgentId, role: AgentRole, label: &str) -> AgentRef {
        AgentRef::new(id, role, label, SpawnSource::User)
    }

    #[test]
    fn no_prefix_routes_to_default_conversational() {
        let p = parse_mention("hello world", []);
        assert_eq!(p.target, MentionTarget::DefaultConversational);
        assert_eq!(p.body, "hello world");
    }

    #[test]
    fn at_label_routes_to_matching_agent() {
        let agents = vec![
            ref_for(7, AgentRole::Watcher, "contradictions"),
            ref_for(8, AgentRole::Watcher, "build-watcher"),
        ];
        let p = parse_mention("@contradictions what did you see?", agents.iter());
        assert_eq!(p.target, MentionTarget::Agent(7));
        assert_eq!(p.body, "what did you see?");
    }

    #[test]
    fn at_label_no_body_returns_empty_body() {
        let agents = vec![ref_for(1, AgentRole::Watcher, "alpha")];
        let p = parse_mention("@alpha", agents.iter());
        assert_eq!(p.target, MentionTarget::Agent(1));
        assert_eq!(p.body, "");
    }

    #[test]
    fn at_unknown_label_returns_unmatched_with_body_kept() {
        let agents = vec![ref_for(1, AgentRole::Watcher, "real")];
        let p = parse_mention("@ghost are you there?", agents.iter());
        assert_eq!(
            p.target,
            MentionTarget::Unmatched { tried: "ghost".to_owned() }
        );
        // Body is preserved so caller can still deliver to fallback.
        assert_eq!(p.body, "are you there?");
    }

    #[test]
    fn colon_role_colon_routes_to_role_broadcast() {
        let p = parse_mention(":watcher: who's listening?", []);
        assert_eq!(p.target, MentionTarget::Role(AgentRole::Watcher));
        assert_eq!(p.body, "who's listening?");
    }

    #[test]
    fn role_token_is_case_insensitive() {
        let p = parse_mention(":WATCHER: hi", []);
        assert_eq!(p.target, MentionTarget::Role(AgentRole::Watcher));
    }

    #[test]
    fn unknown_role_token_returns_unmatched() {
        let p = parse_mention(":platypus: hi", []);
        assert_eq!(
            p.target,
            MentionTarget::Unmatched { tried: "platypus".to_owned() }
        );
    }

    #[test]
    fn role_aliases_resolve() {
        assert_eq!(
            parse_mention(":conv: hi", []).target,
            MentionTarget::Role(AgentRole::Conversational)
        );
        assert_eq!(
            parse_mention(":watch: hi", []).target,
            MentionTarget::Role(AgentRole::Watcher)
        );
    }

    #[test]
    fn leading_whitespace_is_tolerated() {
        let agents = vec![ref_for(1, AgentRole::Watcher, "x")];
        let p = parse_mention("   @x hello", agents.iter());
        assert_eq!(p.target, MentionTarget::Agent(1));
        assert_eq!(p.body, "hello");
    }

    #[test]
    fn at_in_middle_is_not_a_mention() {
        // Plain text containing '@' but not at the start should route to default.
        let agents = vec![ref_for(1, AgentRole::Watcher, "label")];
        let p = parse_mention("email me @ a@b.com", agents.iter());
        assert_eq!(p.target, MentionTarget::DefaultConversational);
    }

    #[test]
    fn empty_input_routes_to_default() {
        let p = parse_mention("", []);
        assert_eq!(p.target, MentionTarget::DefaultConversational);
        assert_eq!(p.body, "");
    }

    #[test]
    fn unmatched_label_falls_through_to_useful_body() {
        // Important UX: even when the label is wrong, the body is kept so
        // caller can route to default with a "did you mean…?" hint.
        let agents = vec![ref_for(1, AgentRole::Watcher, "real")];
        let p = parse_mention("@typo do the thing", agents.iter());
        assert_eq!(p.body, "do the thing");
    }
}
