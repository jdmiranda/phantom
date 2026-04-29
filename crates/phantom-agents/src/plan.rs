//! Plan extraction helpers for LLM output parsing.
//!
//! Agents running in planning dispositions (Feature, BugFix, Refactor) are
//! instructed to emit sentinel markdown sections — `## Tech Spec` and
//! `## Implementation Plan` — inside their response text. This module
//! provides [`extract_section`] to pull those bodies out for downstream
//! processing (approval gates, display, archiving).

/// Extract the body of a sentinel markdown section from LLM output.
///
/// Scans `text` for a line exactly matching `## {heading}` and returns
/// everything until the next `## ` heading or end of string, trimmed.
/// Returns `None` if the heading is not found.
///
/// # Examples
///
/// ```
/// use phantom_agents::plan::extract_section;
///
/// let text = "## Tech Spec\nUse Rust.\n\n## Implementation Plan\nStep 1.";
/// assert_eq!(extract_section(text, "Tech Spec"), Some("Use Rust.".to_string()));
/// assert_eq!(extract_section(text, "Implementation Plan"), Some("Step 1.".to_string()));
/// assert_eq!(extract_section(text, "Missing"), None);
/// ```
pub fn extract_section(text: &str, heading: &str) -> Option<String> {
    let marker = format!("## {heading}");
    let start = text.find(&marker)? + marker.len();
    let rest = &text[start..];
    let end = rest.find("\n## ").unwrap_or(rest.len());
    Some(rest[..end].trim().to_string())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- present -----------------------------------------------------------

    #[test]
    fn present_section_returns_body() {
        let text = "## Tech Spec\nUse Rust.\n\n## Implementation Plan\nStep 1.\nStep 2.";
        assert_eq!(
            extract_section(text, "Tech Spec"),
            Some("Use Rust.".to_string())
        );
    }

    #[test]
    fn second_section_returns_correct_body() {
        let text = "## Tech Spec\nUse Rust.\n\n## Implementation Plan\nStep 1.\nStep 2.";
        assert_eq!(
            extract_section(text, "Implementation Plan"),
            Some("Step 1.\nStep 2.".to_string())
        );
    }

    // --- absent -----------------------------------------------------------

    #[test]
    fn absent_heading_returns_none() {
        let text = "## Tech Spec\nSome content here.";
        assert_eq!(extract_section(text, "Missing"), None);
    }

    #[test]
    fn empty_text_returns_none() {
        assert_eq!(extract_section("", "Tech Spec"), None);
    }

    // --- last section (no following ##) -----------------------------------

    #[test]
    fn last_section_no_following_heading_returns_body() {
        let text = "## Preamble\nIgnore this.\n\n## Implementation Plan\nFinal step here.";
        assert_eq!(
            extract_section(text, "Implementation Plan"),
            Some("Final step here.".to_string())
        );
    }

    #[test]
    fn heading_at_end_of_string_returns_empty_string() {
        // Heading exists but has no body after it.
        let text = "## Tech Spec";
        assert_eq!(
            extract_section(text, "Tech Spec"),
            Some(String::new())
        );
    }

    // --- leading / trailing whitespace ------------------------------------

    #[test]
    fn body_is_trimmed_of_leading_trailing_whitespace() {
        let text = "## Tech Spec\n\n   Use Rust.   \n\n## Implementation Plan\nStep 1.";
        assert_eq!(
            extract_section(text, "Tech Spec"),
            Some("Use Rust.".to_string())
        );
    }

    #[test]
    fn body_with_only_whitespace_returns_empty_string() {
        let text = "## Tech Spec\n   \n\n## Next\nOther content.";
        assert_eq!(
            extract_section(text, "Tech Spec"),
            Some(String::new())
        );
    }

    // --- multiple sections ------------------------------------------------

    #[test]
    fn multiple_sections_each_returns_own_body() {
        let text = "\
## Overview\n\
Background here.\n\
\n\
## Tech Spec\n\
Use async Rust.\n\
\n\
## Implementation Plan\n\
1. Write code.\n\
2. Test it.\n\
\n\
## Risks\n\
Low risk.";

        assert_eq!(
            extract_section(text, "Overview"),
            Some("Background here.".to_string())
        );
        assert_eq!(
            extract_section(text, "Tech Spec"),
            Some("Use async Rust.".to_string())
        );
        assert_eq!(
            extract_section(text, "Implementation Plan"),
            Some("1. Write code.\n2. Test it.".to_string())
        );
        assert_eq!(
            extract_section(text, "Risks"),
            Some("Low risk.".to_string())
        );
    }

    #[test]
    fn first_occurrence_is_used_when_heading_duplicated() {
        // find() returns the first match — this is documented behaviour.
        let text = "## Tech Spec\nFirst.\n\n## Tech Spec\nSecond.";
        assert_eq!(
            extract_section(text, "Tech Spec"),
            Some("First.".to_string())
        );
    }
}
