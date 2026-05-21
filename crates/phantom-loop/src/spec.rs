//! The top-level [`LoopSpec`] and its sub-types.
//!
//! A `LoopSpec` is the TOML-deserialised description of one repo-scoped loop:
//! a (possibly absent) agent, a source, an exit schema, post-iteration
//! effects, and a quarantine policy. The runner (C2) consumes the spec; this
//! slice does no execution.
//!
//! # Why [`LoopPolicy`] mirrors [`phantom_agents::policy::AgentPolicy`]
//!
//! [`phantom_agents::policy::AgentPolicy`] is the policy the agent crate
//! enforces at runtime, but it does *not* derive `serde`. Since C1 is
//! deliberately scoped to *the new crate plus the workspace `Cargo.toml`*,
//! we cannot bolt serde derives onto `AgentPolicy` from here. The clean
//! workaround is to keep a TOML-shaped mirror inside `phantom-loop` and
//! convert at the boundary the runner will own in C2.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::effect::LoopEffect;
use crate::error::LoopSpecError;
use crate::exit::ExitSchema;
use crate::source::LoopSourceSpec;

// ---------------------------------------------------------------------------
// Top-level spec
// ---------------------------------------------------------------------------

/// Deserialised view of a `<repo>/.phantom/loops/<name>.toml` file.
///
/// This struct is intentionally *data only*. The runtime LoopRunner that
/// consumes it lives in a future slice (C2) and will hold an [`ExitSchema`]
/// alongside this spec rather than embedding the compiled validator inside
/// the spec value itself (so the spec stays cheap to clone and inspect).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoopSpec {
    /// User-chosen identifier — `reviewer`, `pr-finder`, `implementer`.
    /// Distinct from the runtime [`crate::LoopId`] which the runner assigns.
    pub id: String,

    /// The agent that drives each iteration. `None` for agentless loops
    /// like PR-finder, which just transform inputs into queue messages
    /// without any LLM in the loop.
    #[serde(default)]
    pub agent: Option<LoopAgentSpec>,

    /// Where iteration inputs come from. Always present.
    pub source: LoopSourceSpec,

    /// Effects fired after each iteration completes with a valid exit.
    /// Defaults to no effects — perfectly fine for a watcher-style loop
    /// that only logs.
    #[serde(default)]
    pub on_complete: Vec<LoopEffect>,

    /// Maximum number of in-flight iterations. Loops are sequential by
    /// default (1) so the user must opt in to parallelism explicitly.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: u8,

    /// What to do when an iteration's exit fails to validate against the
    /// schema. Defaults to [`LoopQuarantinePolicy::SkipAndContinue`] —
    /// matching the loop-overseer expectation that one bad PR shouldn't
    /// stop the entire reviewer.
    #[serde(default)]
    pub on_quarantine: LoopQuarantinePolicy,
}

const fn default_max_concurrent() -> u8 {
    1
}

// ---------------------------------------------------------------------------
// Agent spec — TOML-friendly view of the per-iteration agent config
// ---------------------------------------------------------------------------

/// Per-iteration agent configuration.
///
/// The runner (C2) spawns a fresh agent of the given role for each iteration,
/// optionally narrowing the role's default tool whitelist via `allow_tools`,
/// installing the `system_prompt`, and tagging the iteration result with the
/// compiled [`ExitSchema`].
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoopAgentSpec {
    /// Which agent role to spawn — typically [`phantom_agents::role::AgentRole::Actor`]
    /// for review/implementer loops since those need `Act` capability.
    pub role: phantom_agents::role::AgentRole,

    /// Optional intersection on top of the role's default tool whitelist.
    /// `None` means "no narrowing — use the role's full default whitelist
    /// as-is". An empty `Some(vec![])` means "deny everything", which is
    /// almost never what the user wants but is allowed for completeness.
    #[serde(default)]
    pub allow_tools: Option<Vec<String>>,

    /// Prompt installed at agent spawn. Carries the loop's task description
    /// and any role-specific framing.
    pub system_prompt: String,

    /// Raw JSON Schema (verbatim from TOML) describing the exit payload.
    /// `load_spec` compiles this into an [`ExitSchema`] at parse time;
    /// the runner uses the compiled form.
    pub exit_schema: serde_json::Value,

    /// Per-agent runtime policy (timeouts, retries, auto-approve, planning).
    /// Mirrors [`phantom_agents::policy::AgentPolicy`] — see the module-level
    /// note on why this is a mirror rather than a direct reuse.
    #[serde(default)]
    pub policy: LoopPolicy,
}

// ---------------------------------------------------------------------------
// LoopPolicy — TOML-shaped mirror of phantom_agents::policy::AgentPolicy
// ---------------------------------------------------------------------------

/// Serde-friendly mirror of [`phantom_agents::policy::AgentPolicy`].
///
/// Field semantics are identical to the upstream type — see its rustdoc for
/// the canonical description. The wrapper exists only because `AgentPolicy`
/// does not derive `serde` and this C1 slice is scoped strictly to the new
/// crate plus the workspace `Cargo.toml`.
///
/// The [`From`] impl below converts to the upstream type for runtime use.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoopPolicy {
    /// Maps to [`phantom_agents::policy::AgentPolicy::max_attempts`].
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    /// Maps to [`phantom_agents::policy::AgentPolicy::timeout_seconds`].
    #[serde(default = "default_timeout_seconds")]
    pub timeout_seconds: u64,
    /// Maps to [`phantom_agents::policy::AgentPolicy::auto_approve`].
    #[serde(default)]
    pub auto_approve: bool,
    /// Maps to [`phantom_agents::policy::AgentPolicy::skip_planning`].
    #[serde(default)]
    pub skip_planning: bool,
}

const fn default_max_attempts() -> u32 {
    3
}

const fn default_timeout_seconds() -> u64 {
    1800
}

impl Default for LoopPolicy {
    fn default() -> Self {
        Self {
            max_attempts: default_max_attempts(),
            timeout_seconds: default_timeout_seconds(),
            auto_approve: false,
            skip_planning: false,
        }
    }
}

impl From<LoopPolicy> for phantom_agents::policy::AgentPolicy {
    fn from(p: LoopPolicy) -> Self {
        Self {
            max_attempts: p.max_attempts,
            timeout_seconds: p.timeout_seconds,
            auto_approve: p.auto_approve,
            skip_planning: p.skip_planning,
        }
    }
}

// ---------------------------------------------------------------------------
// Quarantine policy
// ---------------------------------------------------------------------------

/// How the runner should react when an iteration's exit payload fails to
/// validate against the [`ExitSchema`].
///
/// The default is [`Self::SkipAndContinue`] — for the typical loop-overseer
/// use case (reviewer, implementer) you want to move past one bad iteration
/// rather than halting the entire loop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopQuarantinePolicy {
    /// Treat the failure as terminal and stop the loop.
    FailAndStop,
    /// Drop the offending input and pick up the next one.
    #[default]
    SkipAndContinue,
    /// Halt the loop and wait for external manual clearance.
    Park,
}

// ---------------------------------------------------------------------------
// Spec loading
// ---------------------------------------------------------------------------

/// Read a TOML file at `path`, parse it into a [`LoopSpec`], and compile the
/// agent's exit schema into an [`ExitSchema`].
///
/// Returns both values so the runner can hold the compiled validator
/// alongside the spec data. For agentless specs (no `[agent]` section), the
/// returned `Option<ExitSchema>` is `None`.
///
/// # Errors
///
/// - [`LoopSpecError::Io`] if `path` cannot be read.
/// - [`LoopSpecError::TomlParse`] if the TOML is malformed or fails type
///   constraints.
/// - [`LoopSpecError::SchemaCompile`] if the embedded `exit_schema` is not a
///   valid JSON Schema.
/// - [`LoopSpecError::InvalidField`] for structural issues serde alone
///   cannot catch (e.g. an empty `id`).
pub fn load_spec(path: &Path) -> Result<(LoopSpec, Option<ExitSchema>), LoopSpecError> {
    let raw = std::fs::read_to_string(path).map_err(|source| LoopSpecError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    parse_spec_str(&raw).map_err(|e| match e {
        // Re-annotate the bare TomlParseRaw with the file path so end users
        // see *which* spec is malformed.
        LoopSpecError::TomlParseRaw(source) => LoopSpecError::TomlParse {
            path: path.to_path_buf(),
            source,
        },
        other => other,
    })
}

/// Parse a [`LoopSpec`] from an in-memory TOML string.
///
/// Used by tests and by callers that load specs from a non-filesystem
/// source. Identical semantics to [`load_spec`] minus the IO step.
///
/// # Errors
///
/// Same as [`load_spec`] minus the [`LoopSpecError::Io`] variant.
pub fn parse_spec_str(raw: &str) -> Result<(LoopSpec, Option<ExitSchema>), LoopSpecError> {
    let spec: LoopSpec = toml::from_str(raw)?;

    if spec.id.trim().is_empty() {
        return Err(LoopSpecError::InvalidField {
            field: "id",
            reason: "must not be empty".to_string(),
        });
    }

    let schema = match spec.agent.as_ref() {
        Some(agent) => Some(ExitSchema::compile(&agent.exit_schema)?),
        None => None,
    };

    Ok((spec, schema))
}

// ---------------------------------------------------------------------------
// Tests — module-local, unit-only. End-to-end TOML round-trip lives in
// `tests/spec_roundtrip.rs`.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loop_policy_default_matches_agent_policy_default() {
        let lp = LoopPolicy::default();
        let ap = phantom_agents::policy::AgentPolicy::default();
        assert_eq!(lp.max_attempts, ap.max_attempts);
        assert_eq!(lp.timeout_seconds, ap.timeout_seconds);
        assert_eq!(lp.auto_approve, ap.auto_approve);
        assert_eq!(lp.skip_planning, ap.skip_planning);
    }

    #[test]
    fn loop_policy_converts_to_agent_policy() {
        let lp = LoopPolicy {
            max_attempts: 5,
            timeout_seconds: 600,
            auto_approve: true,
            skip_planning: true,
        };
        let ap: phantom_agents::policy::AgentPolicy = lp.into();
        assert_eq!(ap.max_attempts, 5);
        assert_eq!(ap.timeout_seconds, 600);
        assert!(ap.auto_approve);
        assert!(ap.skip_planning);
    }

    #[test]
    fn empty_id_is_rejected() {
        let toml_src = r#"
            id = ""

            [source]
            kind = "cron"
            interval_seconds = 60
        "#;
        let err = parse_spec_str(toml_src).expect_err("empty id must error");
        match err {
            LoopSpecError::InvalidField { field, .. } => assert_eq!(field, "id"),
            other => panic!("expected InvalidField, got {other:?}"),
        }
    }

    #[test]
    fn quarantine_policy_defaults_to_skip_and_continue() {
        assert_eq!(LoopQuarantinePolicy::default(), LoopQuarantinePolicy::SkipAndContinue);
    }

    #[test]
    fn max_concurrent_defaults_to_one() {
        assert_eq!(default_max_concurrent(), 1);
    }
}
