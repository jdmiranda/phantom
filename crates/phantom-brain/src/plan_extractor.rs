//! Plan extraction from sentinel headings in agent output.
//!
//! When an agent produces output containing a `## Plan` (or similar sentinel
//! heading) followed by a numbered list, this module extracts the list items
//! as [`PlanStep`] values that the brain can submit to the [`TaskLedger`].
//!
//! # Protocol
//!
//! Agents self-describe their work by including a sentinel section:
//!
//! ```text
//! ## Plan
//! 1. Read the failing test [tool: ReadFile]
//! 2. Fix the bug [tool: WriteFile] [depends: 1]
//! 3. Run tests [tool: RunCommand] [depends: 2]
//! ```
//!
//! This degrades gracefully (the heading is human-readable) and is far more
//! reliable than JSON schema enforcement for LLM output.
//!
//! # Sentinel headings
//!
//! The following headings are recognised, case-insensitively:
//!
//! - `## Plan`
//! - `## Steps`
//! - `## Task Plan`
//! - `## Implementation Plan`
//! - `## Tech Spec` *(for planning-disposition agents)*
//!
//! # Annotations
//!
//! Items may carry optional inline annotations:
//!
//! - `[tool: X]` — maps to `tool_hint` on the extracted step.
//! - `[depends: N,M]` — 1-based step numbers that must complete first; stored
//!   as 0-based indices in `dependencies`.

use phantom_agents::AgentTask;

use crate::orchestrator::PlanStep;

// ---------------------------------------------------------------------------
// Extracted step (intermediate representation)
// ---------------------------------------------------------------------------

/// An intermediate representation produced by the parser before it is
/// converted into a [`PlanStep`] for the [`crate::orchestrator::TaskLedger`].
///
/// Kept private to this module — callers receive `Vec<PlanStep>` directly.
struct RawStep {
    /// The human-readable description (text portion of the list item).
    description: String,
    /// Optional `[tool: X]` annotation.
    tool_hint: Option<String>,
    /// 0-based indices of prerequisite steps (`[depends: N,M]` is 1-based in
    /// the source; converted here).
    dependencies: Vec<usize>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Extract a plan from agent output containing a sentinel heading.
///
/// Scans `text` for any of the recognised sentinel headings (`## Plan`,
/// `## Steps`, `## Task Plan`, `## Implementation Plan`, `## Tech Spec`),
/// then parses the numbered list that follows it.  Items may carry optional
/// `[tool: X]` and `[depends: N,M]` annotations.
///
/// Returns `None` when:
/// - no sentinel heading is found, or
/// - the heading exists but is not followed by at least one numbered item.
///
/// # Example
///
/// ```rust
/// use phantom_brain::plan_extractor::extract_plan_from_text;
///
/// let text = "## Plan\n1. Read file [tool: ReadFile]\n2. Write fix [depends: 1]";
/// let steps = extract_plan_from_text(text).unwrap();
/// assert_eq!(steps.len(), 2);
/// ```
pub fn extract_plan_from_text(text: &str) -> Option<Vec<PlanStep>> {
    let body = find_sentinel_body(text)?;
    let raw = parse_numbered_list(body);
    if raw.is_empty() {
        return None;
    }
    let steps = raw.into_iter().map(into_plan_step).collect();
    Some(steps)
}

// ---------------------------------------------------------------------------
// Sentinel detection
// ---------------------------------------------------------------------------

/// Recognised sentinel headings (matched case-insensitively after `## `).
const SENTINELS: &[&str] = &[
    "plan",
    "steps",
    "task plan",
    "implementation plan",
    "tech spec",
];

/// Find the text *after* the first recognised sentinel heading.
///
/// Returns the slice starting immediately after the heading line.  The slice
/// ends at the next `##`-level heading or at the end of the string.
fn find_sentinel_body(text: &str) -> Option<&str> {
    let lower = text.to_ascii_lowercase();

    // Walk each line looking for "## <sentinel>".
    let mut pos = 0usize;
    for line in text.lines() {
        let trimmed = line.trim();
        let trimmed_lower = trimmed.to_ascii_lowercase();

        if let Some(rest_lower) = trimmed_lower.strip_prefix("## ") {
            let rest_lower = rest_lower.trim();
            if SENTINELS.contains(&rest_lower) {
                // Advance `pos` past this line (including newline).
                let line_end = pos + line.len();
                // Skip the newline character(s).
                let after = if text[line_end..].starts_with("\r\n") {
                    line_end + 2
                } else if text[line_end..].starts_with('\n') {
                    line_end + 1
                } else {
                    line_end
                };

                // The body runs until the next `## ` heading.
                let body = &text[after..];
                let end = find_next_heading(body).unwrap_or(body.len());
                return Some(&body[..end]);
            }
        }

        pos += line.len() + 1; // +1 for '\n'; accurate enough for index math
        let _ = &lower; // suppress unused-variable warning
    }

    None
}

/// Return the byte offset of the next `##`-level heading in `text`, or `None`.
fn find_next_heading(text: &str) -> Option<usize> {
    let mut offset = 0usize;
    for line in text.lines() {
        if line.trim_start().starts_with("## ") {
            return Some(offset);
        }
        offset += line.len() + 1;
    }
    None
}

// ---------------------------------------------------------------------------
// Numbered-list parser
// ---------------------------------------------------------------------------

/// Parse numbered list items from `body`.
///
/// Recognises lines of the form `N. text`, `N) text`, or `N: text` where
/// `N` is a decimal integer.  Blank lines and non-numbered lines are ignored.
fn parse_numbered_list(body: &str) -> Vec<RawStep> {
    let mut steps: Vec<RawStep> = Vec::new();

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Match `N.`, `N)`, or `N:` prefix.
        let item_text = match strip_list_number(trimmed) {
            Some(t) => t,
            None => continue,
        };

        let (description, tool_hint, dependencies) = parse_annotations(item_text);
        steps.push(RawStep {
            description,
            tool_hint,
            dependencies,
        });
    }

    steps
}

/// Strip a leading `N.`, `N)`, or `N:` from the line.
///
/// Returns the text after the separator + any leading whitespace, or `None`
/// if the line doesn't start with a decimal number followed by a separator.
fn strip_list_number(line: &str) -> Option<&str> {
    // Find the end of the digit run.
    let digit_end = line.find(|c: char| !c.is_ascii_digit())?;
    if digit_end == 0 {
        return None; // Line didn't start with a digit.
    }

    let sep = line[digit_end..].chars().next()?;
    if !matches!(sep, '.' | ')' | ':') {
        return None;
    }

    Some(line[digit_end + sep.len_utf8()..].trim_start())
}

// ---------------------------------------------------------------------------
// Annotation parser
// ---------------------------------------------------------------------------

/// Extract `[tool: X]` and `[depends: N,M,…]` annotations from an item line.
///
/// Returns `(description_without_annotations, tool_hint, 0-based-deps)`.
fn parse_annotations(text: &str) -> (String, Option<String>, Vec<usize>) {
    let mut tool_hint: Option<String> = None;
    let mut dependencies: Vec<usize> = Vec::new();
    let mut remaining = text;
    let mut description_parts: Vec<&str> = Vec::new();

    // Walk the text, consuming `[…]` blocks.
    loop {
        match remaining.find('[') {
            None => {
                description_parts.push(remaining);
                break;
            }
            Some(open) => {
                description_parts.push(remaining[..open].trim_end());
                remaining = &remaining[open + 1..];

                // Find matching `]`.
                let close = match remaining.find(']') {
                    Some(i) => i,
                    None => {
                        // Malformed annotation — treat the rest as description.
                        description_parts.push(remaining);
                        break;
                    }
                };

                let inner = &remaining[..close];
                remaining = &remaining[close + 1..].trim_start();

                let lower = inner.to_ascii_lowercase();
                if lower.strip_prefix("tool:").is_some() {
                    tool_hint = Some(inner[5..].trim().to_string());
                } else if lower.strip_prefix("depends:").is_some() {
                    let nums = &inner[8..];
                    for part in nums.split(',') {
                        let part = part.trim();
                        if let Ok(n) = part.parse::<usize>() {
                            if n > 0 {
                                dependencies.push(n - 1); // convert to 0-based
                            }
                        }
                    }
                }
                // Unknown annotations are silently dropped.
            }
        }
    }

    let description = description_parts
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    (description, tool_hint, dependencies)
}

// ---------------------------------------------------------------------------
// Conversion to PlanStep
// ---------------------------------------------------------------------------

/// Convert a [`RawStep`] into a [`PlanStep`].
///
/// The `tool_hint` is stored as the preferred provider when present; the
/// [`AgentTask`] is always `FreeForm` because at extraction time we don't have
/// enough context to pick a specialised task type.
fn into_plan_step(raw: RawStep) -> PlanStep {
    let task = AgentTask::FreeForm {
        prompt: raw.description.clone(),
    };

    let mut step = if raw.dependencies.is_empty() {
        PlanStep::new(raw.description, task)
    } else {
        PlanStep::with_deps(raw.description, task, raw.dependencies)
    };

    if let Some(hint) = raw.tool_hint {
        step = step.with_provider(hint);
    }

    step
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn descriptions(steps: &[PlanStep]) -> Vec<&str> {
        steps.iter().map(|s| s.description.as_str()).collect()
    }

    // -----------------------------------------------------------------------
    // 1. extract_basic_numbered_list — 3 steps, no annotations
    // -----------------------------------------------------------------------

    #[test]
    fn extract_basic_numbered_list() {
        let text = "## Plan\n1. Read the file\n2. Edit the code\n3. Run tests\n";
        let steps = extract_plan_from_text(text).unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(
            descriptions(&steps),
            vec!["Read the file", "Edit the code", "Run tests"]
        );
    }

    // -----------------------------------------------------------------------
    // 2. extract_with_tool_annotation — [tool: X] parsed into preferred_provider
    // -----------------------------------------------------------------------

    #[test]
    fn extract_with_tool_annotation() {
        let text = "## Plan\n1. Read failing test [tool: ReadFile]\n2. Fix the bug [tool: WriteFile]\n";
        let steps = extract_plan_from_text(text).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].preferred_provider(), Some("ReadFile"));
        assert_eq!(steps[1].preferred_provider(), Some("WriteFile"));
    }

    // -----------------------------------------------------------------------
    // 3. extract_with_depends_annotation — [depends: N,M] → 0-based dependencies
    // -----------------------------------------------------------------------

    #[test]
    fn extract_with_depends_annotation() {
        let text = "## Plan\n\
                    1. Read the failing test [tool: ReadFile]\n\
                    2. Fix the bug [tool: WriteFile] [depends: 1]\n\
                    3. Run tests [tool: RunCommand] [depends: 2]\n";
        let steps = extract_plan_from_text(text).unwrap();
        assert_eq!(steps.len(), 3);
        assert!(steps[0].depends_on().is_empty());
        assert_eq!(steps[1].depends_on(), &[0usize]);
        assert_eq!(steps[2].depends_on(), &[1usize]);
    }

    // -----------------------------------------------------------------------
    // 4. extract_returns_none_when_no_sentinel
    // -----------------------------------------------------------------------

    #[test]
    fn extract_returns_none_when_no_sentinel() {
        let text = "Here is my work:\n1. Do thing A\n2. Do thing B\n";
        assert!(extract_plan_from_text(text).is_none());
    }

    // -----------------------------------------------------------------------
    // 5. extract_case_insensitive_sentinel — ## PLAN, ## Steps, etc.
    // -----------------------------------------------------------------------

    #[test]
    fn extract_case_insensitive_sentinel() {
        for heading in &["## PLAN", "## Plan", "## plan", "## pLaN"] {
            let text = format!("{heading}\n1. Step one\n2. Step two\n");
            let steps = extract_plan_from_text(&text)
                .unwrap_or_else(|| panic!("expected Some for heading `{heading}`"));
            assert_eq!(steps.len(), 2, "failed for heading `{heading}`");
        }
    }

    // -----------------------------------------------------------------------
    // 6. Additional sentinels recognised
    // -----------------------------------------------------------------------

    #[test]
    fn extract_steps_heading() {
        let text = "## Steps\n1. Alpha\n2. Beta\n";
        let steps = extract_plan_from_text(text).unwrap();
        assert_eq!(steps.len(), 2);
    }

    #[test]
    fn extract_task_plan_heading() {
        let text = "## Task Plan\n1. Phase one\n2. Phase two\n";
        let steps = extract_plan_from_text(text).unwrap();
        assert_eq!(steps.len(), 2);
    }

    #[test]
    fn extract_implementation_plan_heading() {
        let text = "## Implementation Plan\n1. Design\n2. Code\n3. Review\n";
        let steps = extract_plan_from_text(text).unwrap();
        assert_eq!(steps.len(), 3);
    }

    #[test]
    fn extract_tech_spec_heading() {
        let text = "## Tech Spec\n1. Define API\n2. Write tests\n";
        let steps = extract_plan_from_text(text).unwrap();
        assert_eq!(steps.len(), 2);
    }

    // -----------------------------------------------------------------------
    // 7. Returns None when sentinel found but list is empty
    // -----------------------------------------------------------------------

    #[test]
    fn extract_returns_none_on_empty_list() {
        let text = "## Plan\n\nNo numbered items here.\n";
        assert!(extract_plan_from_text(text).is_none());
    }

    // -----------------------------------------------------------------------
    // 8. Body stops at next ## heading
    // -----------------------------------------------------------------------

    #[test]
    fn extract_stops_at_next_heading() {
        let text = "## Plan\n1. Step A\n2. Step B\n\n## Notes\n3. Not a step\n";
        let steps = extract_plan_from_text(text).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(descriptions(&steps), vec!["Step A", "Step B"]);
    }

    // -----------------------------------------------------------------------
    // 9. Multiple depends
    // -----------------------------------------------------------------------

    #[test]
    fn extract_multi_depends() {
        let text = "## Plan\n1. Prep A\n2. Prep B\n3. Merge [depends: 1,2]\n";
        let steps = extract_plan_from_text(text).unwrap();
        assert_eq!(steps[2].depends_on(), &[0usize, 1usize]);
    }

    // -----------------------------------------------------------------------
    // 10. Surrounding prose is ignored
    // -----------------------------------------------------------------------

    #[test]
    fn extract_ignores_surrounding_prose() {
        let text = "I will now describe my plan.\n\n\
                    ## Plan\n\
                    1. First step\n\
                    2. Second step\n\n\
                    After this I will proceed.";
        let steps = extract_plan_from_text(text).unwrap();
        assert_eq!(steps.len(), 2);
    }

    // -----------------------------------------------------------------------
    // 11. Description text is stripped of annotation residue
    // -----------------------------------------------------------------------

    #[test]
    fn extract_description_clean() {
        // Two-step plan so step 1 can declare [depends: 1].
        let text = "## Plan\n1. Prep work\n2. Fix the bug [tool: WriteFile] [depends: 1]\n";
        let steps = extract_plan_from_text(text).unwrap();
        assert_eq!(steps[1].description, "Fix the bug");
        assert_eq!(steps[1].preferred_provider(), Some("WriteFile"));
        assert_eq!(steps[1].depends_on(), &[0usize]);
    }
}
