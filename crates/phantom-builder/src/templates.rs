//! Embedded loop spec templates.
//!
//! At build time the four canonical loop specs from `crates/phantom-builder/
//! templates/` are baked into the binary via `include_str!`. At runtime the
//! builder rewrites the `repo` field on every `gh_pr` / `gh_issues` source so
//! the seeded specs target the user-supplied slug, then writes one TOML file
//! per loop into `<repo>/.phantom/loops/`.
//!
//! Files that already exist on disk are skipped — the builder is idempotent;
//! a second `phantom builder run` invocation does not overwrite a spec the
//! operator has hand-customized.

use std::path::{Path, PathBuf};

use crate::BuilderError;

/// Filename → embedded TOML body for every default spec.
const TEMPLATES: &[(&str, &str)] = &[
    (
        "pr_finder_review.toml",
        include_str!("../templates/pr_finder_review.toml"),
    ),
    (
        "pr_finder_impl.toml",
        include_str!("../templates/pr_finder_impl.toml"),
    ),
    (
        "reviewer.toml",
        include_str!("../templates/reviewer.toml"),
    ),
    (
        "implementer.toml",
        include_str!("../templates/implementer.toml"),
    ),
];

/// Write the default four-loop pipeline into `<repo_path>/.phantom/loops/`.
///
/// Substitutes the target slug into every `repo = "jdmiranda/phantom"` line so
/// the seeded specs poll the user's target rather than the upstream repo. The
/// substitution is intentionally string-based — TOML round-tripping through a
/// real parser would lose the careful comments in each template.
///
/// # Errors
///
/// - [`BuilderError::Io`] when the spec directory cannot be created or one of
///   the TOML files cannot be written.
pub fn write_default_specs(
    repo_path: &Path,
    target_slug: &str,
) -> Result<Vec<PathBuf>, BuilderError> {
    let dir = repo_path.join(".phantom").join("loops");
    std::fs::create_dir_all(&dir).map_err(|source| BuilderError::Io {
        path: dir.clone(),
        source,
    })?;

    let mut written = Vec::with_capacity(TEMPLATES.len());
    for (filename, body) in TEMPLATES {
        let dst = dir.join(filename);
        if dst.exists() {
            tracing::info!(
                file = %dst.display(),
                "skipping existing loop spec (idempotent re-run)",
            );
            // Even when skipped we include the path in the return list so
            // callers see the full set of seeded specs.
            written.push(dst);
            continue;
        }
        let rewritten = rewrite_repo_field(body, target_slug);
        std::fs::write(&dst, rewritten).map_err(|source| BuilderError::Io {
            path: dst.clone(),
            source,
        })?;
        tracing::info!(file = %dst.display(), "seeded default loop spec");
        written.push(dst);
    }
    Ok(written)
}

/// Substitute the upstream `jdmiranda/phantom` slug used by every template
/// header for the user's `target_slug`.
///
/// Strategy: we look for the literal TOML lines that pin the upstream slug
/// (`repo = "jdmiranda/phantom"` on a `[source]` table, and the comments that
/// mention the same string) and rewrite them. The replacement uses
/// `str::replace` rather than a full TOML parse because:
///
/// 1. Round-tripping through `toml_edit` would drop blank lines and comments.
/// 2. Each template's commentary block intentionally documents the original
///    upstream context; rewriting the slug everywhere (including comments)
///    keeps the on-disk file self-explanatory for the new target.
fn rewrite_repo_field(body: &str, target_slug: &str) -> String {
    body.replace("jdmiranda/phantom", target_slug)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn templates_embed_at_compile_time() {
        // Sanity: every template must have non-empty content and at least one
        // `repo = "..."` line.
        for (filename, body) in TEMPLATES {
            assert!(!body.is_empty(), "{filename} is empty");
            assert!(
                body.contains("jdmiranda/phantom"),
                "{filename} does not pin the upstream slug — the substitution will silently no-op",
            );
        }
    }

    #[test]
    fn rewrite_substitutes_target_slug() {
        let original = "[source]\nrepo = \"jdmiranda/phantom\"\n";
        let out = rewrite_repo_field(original, "other/repo");
        assert!(out.contains("other/repo"));
        assert!(!out.contains("jdmiranda/phantom"));
    }

    #[test]
    fn rewrite_preserves_comments_and_blank_lines() {
        let original = "# heading\n\n[source]\nrepo = \"jdmiranda/phantom\"\n# trailer\n";
        let out = rewrite_repo_field(original, "x/y");
        assert!(out.contains("# heading"));
        assert!(out.contains("# trailer"));
        assert!(out.contains("\n\n[source]"));
    }

    #[test]
    fn write_default_specs_seeds_all_four_files_when_missing() {
        let tmp = tempdir().unwrap();
        let written = write_default_specs(tmp.path(), "alice/proj").unwrap();
        assert_eq!(written.len(), 4);
        for path in &written {
            assert!(path.exists(), "{} was not created", path.display());
            let body = std::fs::read_to_string(path).unwrap();
            assert!(body.contains("alice/proj"));
            assert!(!body.contains("jdmiranda/phantom"));
        }
    }

    #[test]
    fn write_default_specs_skips_existing_files() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join(".phantom").join("loops");
        std::fs::create_dir_all(&dir).unwrap();
        let preexisting = dir.join("reviewer.toml");
        std::fs::write(&preexisting, "# hand-crafted, do not overwrite").unwrap();

        let _ = write_default_specs(tmp.path(), "alice/proj").unwrap();

        let body = std::fs::read_to_string(&preexisting).unwrap();
        assert_eq!(body, "# hand-crafted, do not overwrite");
    }

    #[test]
    fn write_default_specs_creates_parent_directories() {
        let tmp = tempdir().unwrap();
        let nested = tmp.path().join("nested").join("deeper");
        std::fs::create_dir_all(&nested).unwrap();
        let written = write_default_specs(&nested, "ax/by").unwrap();
        assert_eq!(written.len(), 4);
        assert!(nested.join(".phantom").join("loops").exists());
    }
}
