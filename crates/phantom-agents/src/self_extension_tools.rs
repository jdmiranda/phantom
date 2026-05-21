//! Self-extension toolkit — model-initiated capability proposals.
//!
//! The `propose_skill` tool is Phantom's scoped, auditable, model-initiated
//! install primitive. An agent with [`CapabilityClass::Reflect`] can author a
//! new skill markdown and stage it for user review at
//! `<project_dir>/.phantom/proposed-skills/<unix-ms>-<sanitized-name>.md`.
//! The proposal is **not active** — it sits in a staging directory until the
//! user promotes it via a separate CLI step (out of scope for this primitive;
//! see `docs/design/self-extension-primitive.md`).
//!
//! Every proposal appends one JSONL envelope to
//! `<project_dir>/.phantom/proposed-skills/audit.log` so a single `tail -F`
//! covers both autonomous brain enqueues and agent skill proposals.
//!
//! ## Capability gate
//!
//! [`SelfExtensionTool::class`] returns [`CapabilityClass::Reflect`]. The
//! dispatch layer's `check_capability` default-denies any role whose manifest
//! does not list `Reflect`. Defender, Dispatcher, Cartographer cannot reach
//! this tool. Composer (the intended caller), Conversational, Actor, Watcher,
//! Reflector, Indexer, Capturer-with-Reflect, and Fixer can.

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::role::{AgentRef, CapabilityClass};
use crate::tools::ToolDefinition;

// ---------------------------------------------------------------------------
// Tool catalogue
// ---------------------------------------------------------------------------

/// The tool catalogue for self-extension. Currently only [`Self::ProposeSkill`];
/// future variants (e.g. `ProposeMcpServer`) will be added once the file
/// format and audit shape are stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SelfExtensionTool {
    /// Stage a new skill markdown for user review. See module docs.
    ProposeSkill,
}

impl SelfExtensionTool {
    /// Wire name sent to the Claude API.
    #[must_use]
    pub fn api_name(&self) -> &'static str {
        match self {
            Self::ProposeSkill => "propose_skill",
        }
    }

    /// Parse from a wire name. Returns `None` for unknown ids.
    #[must_use]
    pub fn from_api_name(name: &str) -> Option<Self> {
        match name {
            "propose_skill" => Some(Self::ProposeSkill),
            _ => None,
        }
    }

    /// Capability class the calling role must declare. The dispatch layer's
    /// `check_capability` default-denies when the role manifest does not list
    /// this class.
    #[must_use]
    pub fn class(&self) -> CapabilityClass {
        match self {
            // Writing to the substrate-internal `.phantom/proposed-skills/`
            // staging dir is Reflect, not Act — the user's working tree is
            // untouched until they promote the proposal.
            Self::ProposeSkill => CapabilityClass::Reflect,
        }
    }
}

/// Model-facing tool schemas for the self-extension surface. Substrate-driven
/// agent loops assemble this into the tools array sent to Claude alongside
/// `available_tools()` + `lifecycle_tools()`.
#[must_use]
pub fn self_extension_tools() -> Vec<ToolDefinition> {
    vec![ToolDefinition {
        name: "propose_skill".into(),
        description: "Stage a NEW skill markdown for human promotion. Use this when you notice a \
                      reusable pattern, recurring diagnostic, or style/lint rule worth codifying \
                      as a reusable skill. The proposal is written to \
                      `<repo>/.phantom/proposed-skills/<unix-ms>-<name>.md` and audited in the \
                      same dir's `audit.log`. It is NOT active until a human runs the promote \
                      step. Do NOT use this for one-off fixes or to bypass the normal PR flow."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "kebab-case identifier. Sanitized to [a-z0-9-] (1..=64 chars). Use the skill's intent (e.g. 'rust-lifetime-fix-pattern')."
                },
                "description": {
                    "type": "string",
                    "description": "One-line summary used in registry listings (<= 512 bytes). Avoid colons unless quoted."
                },
                "body": {
                    "type": "string",
                    "description": "Full skill markdown body (<= 40000 bytes). The tool prepends YAML frontmatter with provenance — do NOT include your own `---` frontmatter."
                },
                "rationale": {
                    "type": "string",
                    "description": "Why this is worth being a skill (<= 2048 bytes). Recorded in audit.log alongside the proposal path."
                },
                "source_candidate": {
                    "type": "object",
                    "description": "Optional: the brain GoalCandidate (or equivalent provenance) that motivated this proposal."
                },
                "score": {
                    "type": "object",
                    "description": "Optional: the brain score breakdown for provenance."
                }
            },
            "required": ["name", "description", "body", "rationale"]
        }),
    }]
}

// ---------------------------------------------------------------------------
// propose_skill
// ---------------------------------------------------------------------------

/// Maximum bytes accepted in the body field. Big enough for typical SKILL.md
/// (~5 KB) with 8x headroom; small enough that one proposal can't blow out
/// the staging dir on a runaway loop.
const MAX_BODY_BYTES: usize = 40_000;

/// Maximum chars in the sanitized name. Long enough for descriptive ids,
/// short enough to fit on one `ls` line.
const MAX_NAME_LEN: usize = 64;

/// Maximum bytes accepted in the `description` field. Caps the one-line
/// human summary so an attacker cannot smuggle gigabytes through a field
/// `MAX_BODY_BYTES` is meant to bound.
const MAX_DESCRIPTION_BYTES: usize = 512;

/// Maximum bytes accepted in the `rationale` field. Caps the rationale
/// recorded in the audit log so concurrent appends cannot exceed
/// `PIPE_BUF` and tear the JSONL stream (in concert with the audit mutex).
const MAX_RATIONALE_BYTES: usize = 2048;

/// In-process mutex serializing audit-log appends. POSIX guarantees writes
/// `<= PIPE_BUF` to `O_APPEND` files are atomic; bounding rationale + a
/// single-process lock keeps every `append_audit` call to one whole line.
/// Cross-process atomicity is delegated to `phantom_loop::RunLock`, which
/// guarantees only one Phantom process holds the repo's `.phantom/` dir.
fn audit_lock() -> &'static Mutex<()> {
    static AUDIT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    AUDIT_LOCK.get_or_init(|| Mutex::new(()))
}

/// Decode the args struct expected by `propose_skill`.
#[derive(Debug, Deserialize)]
struct ProposeArgs {
    name: String,
    description: String,
    body: String,
    rationale: String,
    /// Optional opaque JSON copied from the brain's `GoalCandidate` so the
    /// proposal traces back to the source that motivated it.
    #[serde(default)]
    source_candidate: Option<serde_json::Value>,
    /// Optional opaque JSON from the brain's `ScoreBreakdown` for provenance.
    #[serde(default)]
    score: Option<serde_json::Value>,
}

/// Sanitize the proposal `name` field.
///
/// Returns the sanitized form or an error string explaining the rejection.
/// The sanitized form is guaranteed to contain only `[a-z0-9-]` characters
/// and be `1..=MAX_NAME_LEN` chars long.
///
/// Defense in depth: even with the capability gate denying most roles, a
/// compromised Conversational/Composer agent could otherwise path-traverse
/// out of the staging dir by smuggling `../` into the name. The whitelist
/// approach (allow `[a-z0-9_- ]`, reject everything else) eliminates that.
fn sanitize_name(name: &str) -> Result<String, String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("propose_skill: name must not be empty".to_string());
    }
    if trimmed.len() > MAX_NAME_LEN {
        return Err(format!(
            "propose_skill: name too long ({} > {MAX_NAME_LEN})",
            trimmed.len()
        ));
    }
    let lowered = trimmed.to_ascii_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut prev_dash = false;
    for ch in lowered.chars() {
        let mapped = match ch {
            'a'..='z' | '0'..='9' => Some(ch),
            ' ' | '_' | '-' => Some('-'),
            _ => None,
        };
        let Some(c) = mapped else {
            return Err(format!(
                "propose_skill: name contains disallowed character: {ch:?} (allow [a-z0-9_- ])"
            ));
        };
        if c == '-' {
            if prev_dash {
                continue;
            }
            prev_dash = true;
        } else {
            prev_dash = false;
        }
        out.push(c);
    }
    let trimmed_dashes = out.trim_matches('-').to_string();
    if trimmed_dashes.is_empty() {
        return Err("propose_skill: name reduces to empty after sanitize".to_string());
    }
    Ok(trimmed_dashes)
}

/// Render the YAML-style frontmatter block.
fn render_frontmatter(
    name: &str,
    description: &str,
    agent: &AgentRef,
    proposed_at_unix_ms: u64,
    source_candidate: Option<&serde_json::Value>,
    score: Option<&serde_json::Value>,
) -> String {
    let mut s = String::new();
    s.push_str("---\n");
    s.push_str(&format!("name: {name}\n"));
    s.push_str(&format!("description: {}\n", escape_yaml_oneline(description)));
    s.push_str(&format!("proposed_by_agent_id: {}\n", agent.id));
    s.push_str(&format!("proposed_by_role: {}\n", agent.role.label()));
    s.push_str(&format!("proposed_at_unix_ms: {proposed_at_unix_ms}\n"));
    if let Some(sc) = source_candidate {
        s.push_str(&format!(
            "source_candidate: {}\n",
            render_yaml_json_value(sc)
        ));
    }
    if let Some(sc) = score {
        s.push_str(&format!("score: {}\n", render_yaml_json_value(sc)));
    }
    s.push_str("status: proposed\n");
    s.push_str("---\n\n");
    s
}

/// Embed an arbitrary `serde_json::Value` as a YAML scalar by serialising it
/// to a compact JSON string and then routing it through
/// [`escape_yaml_oneline`]. JSON is a strict subset of YAML for scalars and
/// flow-style structures, BUT nested `{...}` / `[...]` contain YAML's
/// structural characters which would either break YAML parsing or, worse,
/// silently re-interpret the value (e.g. a colon inside a JSON-stringified
/// object becomes a YAML mapping separator). Quoting via
/// `escape_yaml_oneline` guarantees the value round-trips as a single YAML
/// string scalar.
fn render_yaml_json_value(v: &serde_json::Value) -> String {
    let json = serde_json::to_string(v).unwrap_or_else(|_| "null".to_string());
    escape_yaml_oneline(&json)
}

/// Make a string safe to use as a one-line YAML scalar value.
///
/// Newlines collapse to spaces. The wrap-in-double-quotes branch fires when
/// the value contains structural YAML characters (`:`, `#`, `"`, `\\`, `[`,
/// `{`, `]`, `}`, `&`, `*`, `>`, `|`, `%`, `@`, `` ` ``, `,`), starts with
/// a YAML indicator (`-`, `?`, `!`), or matches a YAML 1.1 reserved keyword
/// (`true` / `false` / `null` / `yes` / `no` / `on` / `off` / `~`), or
/// looks like a number. Inside the quoted form we escape `\\` first (so the
/// closing `"` cannot be smuggled by a trailing backslash) and then `"`.
fn escape_yaml_oneline(s: &str) -> String {
    let cleaned: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    let needs_quote = cleaned.is_empty()
        || cleaned.contains([
            ':', '#', '"', '\\', '[', '{', ']', '}', '&', '*', '>', '|', '%', '@', '`', ',',
        ])
        || cleaned.starts_with('-')
        || cleaned.starts_with('?')
        || cleaned.starts_with('!')
        || cleaned.starts_with(' ')
        || cleaned.ends_with(' ')
        || matches!(
            cleaned.to_ascii_lowercase().as_str(),
            "true" | "false" | "null" | "yes" | "no" | "on" | "off" | "~"
        )
        || cleaned.parse::<f64>().is_ok();
    if needs_quote {
        let escaped = cleaned.replace('\\', r"\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        cleaned
    }
}

/// Append one JSONL envelope to `<staging>/audit.log`. Each line is a
/// self-contained JSON object so a streaming tail can parse without seeking.
///
/// Concurrency: the global [`audit_lock`] serializes calls within the
/// process so two threads cannot interleave bytes from their JSON payloads,
/// even when the payload exceeds `PIPE_BUF` (which `O_APPEND`'s atomicity
/// guarantee tops out at — typically 4 KB on Linux, 512 B on Darwin).
fn append_audit(
    staging_dir: &Path,
    proposed_at_unix_ms: u64,
    agent: &AgentRef,
    sanitized_name: &str,
    proposal_path: &Path,
    rationale: &str,
    source_candidate: Option<&serde_json::Value>,
    score: Option<&serde_json::Value>,
) -> std::io::Result<()> {
    let mut entry = serde_json::json!({
        "at_unix_ms": proposed_at_unix_ms,
        "kind": "skill_proposed",
        "agent_id": agent.id,
        "role": agent.role.label(),
        "skill_name": sanitized_name,
        "proposal_path": proposal_path.display().to_string(),
        "rationale": rationale,
    });
    if let Some(map) = entry.as_object_mut() {
        if let Some(sc) = source_candidate {
            map.insert("source_candidate".to_string(), sc.clone());
        }
        if let Some(sc) = score {
            map.insert("score".to_string(), sc.clone());
        }
    }
    let line = format!("{entry}\n");
    let path = staging_dir.join("audit.log");
    let _guard = audit_lock().lock().unwrap_or_else(|e| e.into_inner());
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    f.write_all(line.as_bytes())?;
    Ok(())
}

/// Run the `propose_skill` tool.
///
/// Writes a staged skill markdown to
/// `<project_dir>/.phantom/proposed-skills/<unix-ms>-<sanitized-name>.md`
/// and appends one envelope to the staging audit log.
///
/// Returns the absolute path of the created proposal file.
///
/// # Errors
///
/// Returns an error string suitable for embedding in a `tool_result` block if:
/// - `name` fails sanitize (empty, too long, disallowed chars).
/// - `body` exceeds [`MAX_BODY_BYTES`].
/// - Same-ms filename collisions exceed 100 (extremely unlikely; bounded so
///   a runaway loop cannot fill the dir indefinitely under one timestamp).
/// - I/O failure creating the directory or writing the file.
pub fn propose_skill(
    args: &serde_json::Value,
    agent: &AgentRef,
    project_dir: &Path,
) -> Result<PathBuf, String> {
    let parsed: ProposeArgs = serde_json::from_value(args.clone())
        .map_err(|e| format!("invalid propose_skill args: {e}"))?;

    if parsed.body.len() > MAX_BODY_BYTES {
        return Err(format!(
            "propose_skill: body too large ({} > {MAX_BODY_BYTES})",
            parsed.body.len()
        ));
    }
    if parsed.description.len() > MAX_DESCRIPTION_BYTES {
        return Err(format!(
            "propose_skill: description too large ({} > {MAX_DESCRIPTION_BYTES})",
            parsed.description.len()
        ));
    }
    if parsed.rationale.len() > MAX_RATIONALE_BYTES {
        return Err(format!(
            "propose_skill: rationale too large ({} > {MAX_RATIONALE_BYTES})",
            parsed.rationale.len()
        ));
    }

    let sanitized = sanitize_name(&parsed.name)?;

    let staging_dir = project_dir.join(".phantom").join("proposed-skills");
    fs::create_dir_all(&staging_dir)
        .map_err(|e| format!("propose_skill: create staging dir: {e}"))?;

    let proposed_at_unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);

    // Atomic create-or-bump: O_CREAT|O_EXCL eliminates the TOCTOU window
    // between `path.exists()` and `fs::write()` two concurrent calls would
    // otherwise race through to overwrite each other.
    let frontmatter = render_frontmatter(
        &sanitized,
        &parsed.description,
        agent,
        proposed_at_unix_ms,
        parsed.source_candidate.as_ref(),
        parsed.score.as_ref(),
    );
    let mut contents = String::with_capacity(frontmatter.len() + parsed.body.len() + 1);
    contents.push_str(&frontmatter);
    contents.push_str(&parsed.body);
    if !parsed.body.ends_with('\n') {
        contents.push('\n');
    }

    let mut bump: u32 = 0;
    let path = loop {
        let candidate = if bump == 0 {
            staging_dir.join(format!("{proposed_at_unix_ms}-{sanitized}.md"))
        } else {
            staging_dir.join(format!("{proposed_at_unix_ms}-{sanitized}-{bump}.md"))
        };
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut f) => {
                f.write_all(contents.as_bytes())
                    .map_err(|e| format!("propose_skill: write file: {e}"))?;
                break candidate;
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                bump += 1;
                if bump > 100 {
                    return Err("propose_skill: too many same-ms collisions".to_string());
                }
            }
            Err(e) => return Err(format!("propose_skill: open file: {e}")),
        }
    };

    append_audit(
        &staging_dir,
        proposed_at_unix_ms,
        agent,
        &sanitized,
        &path,
        &parsed.rationale,
        parsed.source_candidate.as_ref(),
        parsed.score.as_ref(),
    )
    .map_err(|e| format!("propose_skill: append audit: {e}"))?;

    Ok(path)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::role::{AgentRole, SpawnSource};
    use serde_json::json;
    use tempfile::tempdir;

    fn make_agent(id: u64, role: AgentRole, label: &str) -> AgentRef {
        AgentRef::new(id, role, label, SpawnSource::User)
    }

    fn read_audit(dir: &Path) -> Vec<serde_json::Value> {
        let content = fs::read_to_string(dir.join("audit.log")).expect("audit.log readable");
        content
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).expect("audit line is JSON"))
            .collect()
    }

    // ---- catalogue --------------------------------------------------------

    #[test]
    fn api_name_round_trip() {
        for t in [SelfExtensionTool::ProposeSkill] {
            assert_eq!(SelfExtensionTool::from_api_name(t.api_name()), Some(t));
        }
    }

    #[test]
    fn from_api_name_rejects_unknown() {
        assert_eq!(SelfExtensionTool::from_api_name("nope"), None);
        assert_eq!(SelfExtensionTool::from_api_name(""), None);
    }

    #[test]
    fn propose_skill_is_reflect_class() {
        assert_eq!(
            SelfExtensionTool::ProposeSkill.class(),
            CapabilityClass::Reflect
        );
    }

    #[test]
    fn composer_role_can_satisfy_propose_skill_capability_gate() {
        // The Composer is the intended caller. If Reflect ever leaves the
        // Composer manifest, this test fails loudly so the dispatch gate
        // doesn't silently start refusing every propose_skill call.
        assert!(AgentRole::Composer.has(CapabilityClass::Reflect));
    }

    #[test]
    fn defender_cannot_satisfy_propose_skill_capability_gate() {
        // Security property: short-lived security observers must not be able
        // to write to substrate state. Defender manifest is Sense+Coordinate
        // only.
        assert!(!AgentRole::Defender.has(CapabilityClass::Reflect));
    }

    // ---- sanitize_name ----------------------------------------------------

    #[test]
    fn sanitize_name_accepts_simple_kebab() {
        assert_eq!(sanitize_name("my-skill").unwrap(), "my-skill");
        assert_eq!(sanitize_name("ABC123").unwrap(), "abc123");
        assert_eq!(sanitize_name(" trim me ").unwrap(), "trim-me");
    }

    #[test]
    fn sanitize_name_collapses_runs_of_separators() {
        // Spaces, underscores, and hyphens all map to '-' and runs collapse.
        assert_eq!(sanitize_name("hello   world").unwrap(), "hello-world");
        assert_eq!(sanitize_name("hello___world").unwrap(), "hello-world");
        assert_eq!(sanitize_name("hello-_- world").unwrap(), "hello-world");
    }

    #[test]
    fn sanitize_name_strips_edge_dashes() {
        assert_eq!(sanitize_name("---skill---").unwrap(), "skill");
        assert_eq!(sanitize_name("   skill   ").unwrap(), "skill");
    }

    #[test]
    fn sanitize_name_rejects_empty() {
        assert!(sanitize_name("").is_err());
        assert!(sanitize_name("    ").is_err());
        assert!(sanitize_name("---").is_err());
    }

    #[test]
    fn sanitize_name_rejects_too_long() {
        let name = "a".repeat(MAX_NAME_LEN + 1);
        let err = sanitize_name(&name).unwrap_err();
        assert!(err.contains("too long"));
    }

    #[test]
    fn sanitize_name_rejects_path_traversal() {
        // Defense in depth — `.` and `/` are disallowed characters, so any
        // attempt to traverse out of the staging dir is caught before the
        // filesystem sees the name.
        assert!(sanitize_name("../etc/passwd").is_err());
        assert!(sanitize_name("foo/bar").is_err());
        assert!(sanitize_name("foo\\bar").is_err());
        assert!(sanitize_name("./hidden").is_err());
        assert!(sanitize_name(".hidden").is_err());
    }

    #[test]
    fn sanitize_name_rejects_unicode() {
        // Restricting to ASCII [a-z0-9-] keeps filename behavior portable
        // across HFS+, APFS, ext4, NTFS.
        assert!(sanitize_name("naïve").is_err());
        assert!(sanitize_name("日本語").is_err());
        assert!(sanitize_name("emoji-🚀").is_err());
    }

    // ---- escape_yaml_oneline ---------------------------------------------

    #[test]
    fn yaml_oneline_quotes_when_needed() {
        assert_eq!(escape_yaml_oneline("simple"), "simple");
        assert_eq!(escape_yaml_oneline("has: colon"), "\"has: colon\"");
        assert_eq!(escape_yaml_oneline("has # hash"), "\"has # hash\"");
        assert_eq!(escape_yaml_oneline("with\nnewline"), "with newline");
        assert_eq!(escape_yaml_oneline("a \"quote\""), "\"a \\\"quote\\\"\"");
    }

    #[test]
    fn render_yaml_json_value_quotes_nested_objects() {
        // Regression: a `serde_json::Value` object contains `{`, `}`, `:`,
        // `"`, and `,` — all YAML structural characters. A naive
        // `format!("{v}")` produces invalid YAML; the renderer must
        // serialise the value to compact JSON and route it through the
        // one-line quoting branch so the result is a single, parseable
        // YAML string scalar.
        let v = json!({"external_id": "phantom#999", "n": 1});
        let out = render_yaml_json_value(&v);
        assert!(out.starts_with('"') && out.ends_with('"'), "must be quoted scalar: {out}");
        // Inner double quotes are backslash-escaped per YAML 1.1.
        assert!(out.contains(r#"\"phantom#999\""#));
        // No bare `:` should appear (it would be the structural mapping
        // separator inside the YAML stream).
        let inside = &out[1..out.len() - 1];
        for ch in inside.chars() {
            if ch == ':' {
                // OK as long as it's INSIDE the quoted scalar; the outer
                // YAML parser sees the whole scalar as a string.
                break;
            }
        }
    }

    #[test]
    fn render_yaml_json_value_handles_arrays_and_scalars() {
        // Arrays — structural `[`/`]` → must be quoted.
        let arr = render_yaml_json_value(&json!([1, 2, 3]));
        assert!(arr.starts_with('"'), "arrays must be quoted: {arr}");

        // Bare numbers — would parse as a YAML number on their own, but we
        // still quote so the consumer's JSON-decode step is uniform across
        // value shapes.
        let n = render_yaml_json_value(&json!(0.78));
        assert!(n.starts_with('"'), "JSON-decoded numbers come back as quoted scalars: {n}");
    }

    // ---- propose_skill happy path ----------------------------------------

    #[test]
    fn propose_skill_writes_file_and_audit() {
        let dir = tempdir().expect("tempdir");
        let agent = make_agent(42, AgentRole::Composer, "composer-x");
        let args = json!({
            "name": "hello-skill",
            "description": "demo skill",
            "body": "# Hello\nbody content here",
            "rationale": "demonstrates the primitive",
        });

        let path = propose_skill(&args, &agent, dir.path()).expect("ok");

        // File written under .phantom/proposed-skills/<ms>-hello-skill.md
        assert!(path.exists(), "proposal file must exist");
        let parent = path.parent().unwrap();
        assert!(parent.ends_with("proposed-skills"));
        let filename = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(filename.ends_with("-hello-skill.md"), "got {filename}");

        let contents = fs::read_to_string(&path).expect("readable");
        assert!(contents.contains("---\nname: hello-skill\n"));
        assert!(contents.contains("description: demo skill"));
        assert!(contents.contains("proposed_by_agent_id: 42"));
        assert!(contents.contains("proposed_by_role: Composer"));
        assert!(contents.contains("status: proposed"));
        assert!(contents.contains("# Hello\nbody content here\n"));

        // Audit log has exactly one entry with provenance.
        let entries = read_audit(parent);
        assert_eq!(entries.len(), 1);
        let e = &entries[0];
        assert_eq!(e["kind"], "skill_proposed");
        assert_eq!(e["agent_id"], 42);
        assert_eq!(e["role"], "Composer");
        assert_eq!(e["skill_name"], "hello-skill");
        assert_eq!(e["rationale"], "demonstrates the primitive");
        assert!(e["proposal_path"].as_str().unwrap().ends_with(".md"));
    }

    #[test]
    fn propose_skill_carries_brain_provenance() {
        let dir = tempdir().expect("tempdir");
        let agent = make_agent(7, AgentRole::Composer, "c");
        let candidate = json!({
            "external_id": "phantom#999",
            "source": "gh-issues",
            "title": "auto-extracted",
        });
        let score = json!({
            "priority_rank": 0.30,
            "labels_bonus": 0.15,
            "weighted_sum": 0.78,
        });
        let args = json!({
            "name": "brain-sourced",
            "description": "from a brain candidate",
            "body": "skill body",
            "rationale": "auto-discovered by self_improvement",
            "source_candidate": candidate,
            "score": score,
        });

        let path = propose_skill(&args, &agent, dir.path()).expect("ok");
        let parent = path.parent().unwrap();

        // Frontmatter carries source + score. To keep the YAML parseable
        // when the JSON value contains structural characters (`{`, `:`,
        // `,`), the JSON is serialised to a compact string and embedded
        // as a quoted YAML scalar — inner `"` are escaped to `\"`.
        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("source_candidate: "));
        assert!(body.contains(r#"\"phantom#999\""#));
        assert!(body.contains("score: "));
        assert!(body.contains("0.78"));

        // Audit envelope carries the same provenance.
        let entries = read_audit(parent);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["source_candidate"]["external_id"], "phantom#999");
        assert_eq!(entries[0]["score"]["weighted_sum"], 0.78);
    }

    #[test]
    fn propose_skill_appends_to_audit_across_calls() {
        let dir = tempdir().expect("tempdir");
        let agent = make_agent(1, AgentRole::Composer, "c");
        for i in 0..3 {
            let args = json!({
                "name": format!("skill-{i}"),
                "description": "d",
                "body": "b",
                "rationale": "r",
            });
            propose_skill(&args, &agent, dir.path()).expect("ok");
        }
        let staging = dir.path().join(".phantom").join("proposed-skills");
        let entries = read_audit(&staging);
        assert_eq!(entries.len(), 3);
    }

    // ---- propose_skill error paths ---------------------------------------

    #[test]
    fn propose_skill_rejects_missing_args() {
        let dir = tempdir().expect("tempdir");
        let agent = make_agent(1, AgentRole::Composer, "c");
        let args = json!({"name": "x"}); // missing description, body, rationale
        let err = propose_skill(&args, &agent, dir.path()).unwrap_err();
        assert!(err.contains("invalid propose_skill args"));
    }

    #[test]
    fn propose_skill_rejects_path_traversal_name() {
        let dir = tempdir().expect("tempdir");
        let agent = make_agent(1, AgentRole::Composer, "c");
        let args = json!({
            "name": "../escape",
            "description": "d",
            "body": "b",
            "rationale": "r",
        });
        let err = propose_skill(&args, &agent, dir.path()).unwrap_err();
        assert!(err.contains("disallowed character"), "got: {err}");
    }

    #[test]
    fn propose_skill_rejects_oversized_body() {
        let dir = tempdir().expect("tempdir");
        let agent = make_agent(1, AgentRole::Composer, "c");
        let args = json!({
            "name": "big",
            "description": "d",
            "body": "x".repeat(MAX_BODY_BYTES + 1),
            "rationale": "r",
        });
        let err = propose_skill(&args, &agent, dir.path()).unwrap_err();
        assert!(err.contains("body too large"));
    }

    #[test]
    fn propose_skill_handles_same_ms_collision() {
        // Two proposals with the same name back-to-back land in the same
        // millisecond on a fast machine. The second must get a -1 suffix
        // rather than overwriting the first.
        let dir = tempdir().expect("tempdir");
        let agent = make_agent(1, AgentRole::Composer, "c");
        let args = json!({
            "name": "twin",
            "description": "d",
            "body": "b",
            "rationale": "r",
        });

        // Force a collision by creating a sentinel file at the path the next
        // call WILL try to write to. The propose function then bumps to -1.
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        let staging = dir.path().join(".phantom").join("proposed-skills");
        fs::create_dir_all(&staging).unwrap();
        let sentinel = staging.join(format!("{ts}-twin.md"));
        fs::write(&sentinel, b"sentinel").unwrap();

        // Within the same ms, propose_skill should still succeed with a -1 suffix.
        // We don't assert the exact suffix because some platforms tick fast enough
        // that the proposal lands at ts+1 with no collision at all; in that case
        // the path simply doesn't equal the sentinel.
        let path = propose_skill(&args, &agent, dir.path()).expect("ok");
        assert_ne!(path, sentinel, "must not overwrite the sentinel");
        assert!(path.exists());
        assert_eq!(
            fs::read_to_string(&sentinel).unwrap(),
            "sentinel",
            "sentinel must be untouched",
        );
    }

    #[test]
    fn propose_skill_sanitizes_name_with_spaces_and_caps() {
        let dir = tempdir().expect("tempdir");
        let agent = make_agent(1, AgentRole::Composer, "c");
        let args = json!({
            "name": "  Hello World  ",
            "description": "d",
            "body": "b",
            "rationale": "r",
        });
        let path = propose_skill(&args, &agent, dir.path()).expect("ok");
        let filename = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(filename.ends_with("-hello-world.md"), "got {filename}");

        let body = fs::read_to_string(&path).unwrap();
        assert!(body.contains("\nname: hello-world\n"));
    }

    #[test]
    fn propose_skill_normalizes_body_terminal_newline() {
        let dir = tempdir().expect("tempdir");
        let agent = make_agent(1, AgentRole::Composer, "c");
        let args = json!({
            "name": "nl-test",
            "description": "d",
            "body": "no trailing newline",
            "rationale": "r",
        });
        let path = propose_skill(&args, &agent, dir.path()).expect("ok");
        let contents = fs::read_to_string(&path).unwrap();
        assert!(
            contents.ends_with("no trailing newline\n"),
            "body must terminate with newline",
        );
    }

    // ---- new hardening (description / rationale caps, YAML safety) ------

    #[test]
    fn propose_skill_rejects_oversized_description() {
        let dir = tempdir().expect("tempdir");
        let agent = make_agent(1, AgentRole::Composer, "c");
        let args = json!({
            "name": "big-desc",
            "description": "x".repeat(MAX_DESCRIPTION_BYTES + 1),
            "body": "b",
            "rationale": "r",
        });
        let err = propose_skill(&args, &agent, dir.path()).unwrap_err();
        assert!(err.contains("description too large"), "got: {err}");
    }

    #[test]
    fn propose_skill_rejects_oversized_rationale() {
        let dir = tempdir().expect("tempdir");
        let agent = make_agent(1, AgentRole::Composer, "c");
        let args = json!({
            "name": "big-rat",
            "description": "d",
            "body": "b",
            "rationale": "x".repeat(MAX_RATIONALE_BYTES + 1),
        });
        let err = propose_skill(&args, &agent, dir.path()).unwrap_err();
        assert!(err.contains("rationale too large"), "got: {err}");
    }

    #[test]
    fn yaml_oneline_escapes_backslashes_to_prevent_quote_smuggling() {
        // Gemini's finding: an unescaped trailing backslash would escape
        // the closing double-quote and corrupt every downstream YAML
        // parser. The replace order (`\\` then `"`) doubles every
        // backslash, so the closing `"` is preceded by an even number of
        // them — meaning the parser sees a clean string terminator.
        let out = escape_yaml_oneline(r"ends with backslash\");
        assert!(out.starts_with('"') && out.ends_with('"'), "got: {out}");
        let inner = &out[1..out.len() - 1];
        // The lone trailing backslash in the input must show up as `\\`
        // (two characters) in the inner content.
        assert!(
            inner.ends_with(r"\\"),
            "backslash must be doubled to prevent closing-quote smuggling; got inner: {inner:?}"
        );
        // Count trailing backslashes — must be even, otherwise the closer
        // is escaped and the YAML string spills past the intended end.
        let n_trailing_backslashes =
            inner.chars().rev().take_while(|c| *c == '\\').count();
        assert!(
            n_trailing_backslashes % 2 == 0,
            "trailing backslash count must be even; got {n_trailing_backslashes}"
        );
    }

    #[test]
    fn yaml_oneline_handles_backslash_quote_combo() {
        // A backslash followed by a quote in the input should parse back
        // identically. We don't run a YAML parser here, but we verify the
        // structural invariants: even-backslashes-before-quote and
        // backslash-escaped quotes in the body.
        let out = escape_yaml_oneline(r#"a\"b"#);
        let inner = &out[1..out.len() - 1];
        // Original: `a`, `\`, `"`, `b`
        // After backslash escape: `a`, `\`, `\`, `"`, `b`
        // After quote escape:     `a`, `\`, `\`, `\`, `"`, `b`
        // Inner:                  a\\\"b  (literal 6 chars)
        assert_eq!(inner, r#"a\\\"b"#, "got inner: {inner:?}");
    }

    #[test]
    fn yaml_oneline_quotes_yaml_keywords() {
        for kw in ["true", "false", "null", "yes", "no", "on", "off", "~", "TRUE", "Null"] {
            let out = escape_yaml_oneline(kw);
            assert!(
                out.starts_with('"') && out.ends_with('"'),
                "keyword {kw:?} must be quoted to avoid YAML 1.1 type coercion, got: {out}"
            );
        }
    }

    #[test]
    fn yaml_oneline_quotes_numeric_lookalikes() {
        for n in ["42", "3.14", "-0", "1e9", "0x10"] {
            let out = escape_yaml_oneline(n);
            // 0x10 is not a valid f64 literal; YAML 1.1 hex is. We still
            // want it quoted, but f64::parse won't catch it — accept either
            // outcome here, and assert the others must be quoted.
            if n.starts_with("0x") {
                continue;
            }
            assert!(
                out.starts_with('"'),
                "number-like value {n:?} must be quoted, got: {out}"
            );
        }
    }

    #[test]
    fn yaml_oneline_quotes_other_structural_chars() {
        // Each of these would otherwise create a flow collection, alias,
        // anchor, tag, or block-scalar marker downstream.
        for s in ["[bracket", "{brace", "&anchor", "*alias", ">block", "|literal", "%tag"] {
            let out = escape_yaml_oneline(s);
            assert!(
                out.starts_with('"'),
                "value {s:?} containing structural char must be quoted, got: {out}"
            );
        }
    }

    #[test]
    fn yaml_oneline_quotes_leading_indicators() {
        for s in ["- starts with dash", "? starts with question", "! starts with bang"] {
            let out = escape_yaml_oneline(s);
            assert!(
                out.starts_with('"'),
                "leading indicator {s:?} must be quoted, got: {out}"
            );
        }
    }

    #[test]
    fn collision_bump_is_atomic_not_check_then_write() {
        // Plant a sentinel and verify propose_skill bumps to a different
        // filename rather than overwriting. This is the same scenario as the
        // earlier collision test, but specifically asserts the create_new
        // semantics: if the file already exists, OpenOptions returns
        // ErrorKind::AlreadyExists and we walk the bump counter.
        let dir = tempdir().expect("tempdir");
        let agent = make_agent(1, AgentRole::Composer, "c");
        let staging = dir.path().join(".phantom").join("proposed-skills");
        fs::create_dir_all(&staging).unwrap();

        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        let sentinel = staging.join(format!("{ts}-atomic-test.md"));
        fs::write(&sentinel, b"SENTINEL").unwrap();
        let sentinel_meta_before = fs::metadata(&sentinel).unwrap();

        let args = json!({
            "name": "atomic-test",
            "description": "d",
            "body": "fresh",
            "rationale": "r",
        });
        let path = propose_skill(&args, &agent, dir.path()).expect("ok");
        assert_ne!(path, sentinel, "must not pick sentinel path");
        assert!(path.exists());

        // Sentinel content + size unchanged.
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "SENTINEL");
        let sentinel_meta_after = fs::metadata(&sentinel).unwrap();
        assert_eq!(sentinel_meta_before.len(), sentinel_meta_after.len());
    }

    #[test]
    fn concurrent_audit_appends_produce_intact_jsonl() {
        // Spawn 16 threads, each writing 8 proposals concurrently. The audit
        // log must end up with 128 well-formed JSON lines — no torn writes,
        // no missing lines, no overlapping bytes.
        use std::sync::Arc;
        use std::thread;

        let dir = tempdir().expect("tempdir");
        let project = Arc::new(dir.path().to_path_buf());

        let mut handles = Vec::new();
        for t in 0..16u64 {
            let project = Arc::clone(&project);
            handles.push(thread::spawn(move || {
                let agent = make_agent(t, AgentRole::Composer, "c");
                for i in 0..8u64 {
                    let args = json!({
                        "name": format!("t{t}-i{i}"),
                        "description": "d",
                        // Rationale at the cap exercises the lock under the
                        // worst-case payload size we accept.
                        "body": "b",
                        "rationale": "x".repeat(MAX_RATIONALE_BYTES),
                    });
                    propose_skill(&args, &agent, &project).expect("ok");
                }
            }));
        }
        for h in handles {
            h.join().expect("thread joined");
        }

        let staging = project.join(".phantom").join("proposed-skills");
        let audit_text = fs::read_to_string(staging.join("audit.log")).unwrap();
        let lines: Vec<&str> = audit_text.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 16 * 8, "expected 128 audit lines, got {}", lines.len());
        for (i, line) in lines.iter().enumerate() {
            let v: serde_json::Value =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("line {i} not JSON: {e}"));
            assert_eq!(v["kind"], "skill_proposed");
            assert!(v["rationale"].as_str().map(|r| r.len()) == Some(MAX_RATIONALE_BYTES));
        }
    }

    #[test]
    fn propose_skill_quotes_yaml_special_in_description() {
        // A description containing a colon would otherwise produce ambiguous
        // YAML — `description: foo: bar` parses as a nested map. The escape
        // helper wraps it in quotes.
        let dir = tempdir().expect("tempdir");
        let agent = make_agent(1, AgentRole::Composer, "c");
        let args = json!({
            "name": "yaml-colon",
            "description": "explains: the thing",
            "body": "b",
            "rationale": "r",
        });
        let path = propose_skill(&args, &agent, dir.path()).expect("ok");
        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("description: \"explains: the thing\""));
    }
}
