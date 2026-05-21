//! Local checkout management for builder targets.
//!
//! Two paths:
//!
//! 1. **Override** — when [`ensure_local_checkout`] receives `Some(override_path)`,
//!    the path is used verbatim. The function only checks the path exists and is
//!    a directory; it does NOT validate that the remote URL matches the target
//!    slug, because in practice `--repo-path /Users/me/work/already-cloned-repo`
//!    is the power-user override.
//! 2. **Default** — otherwise the checkout lives at
//!    `~/.phantom/builds/<owner>-<repo>`. If the directory already exists, the
//!    builder refreshes it with `git fetch && git checkout origin/main`. If it
//!    does not, the builder clones from `https://github.com/<owner>/<repo>.git`.
//!
//! Both paths return an absolute [`PathBuf`] to the working directory.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{BuilderError, parse_slug};

/// Resolve a working-copy path for the target slug.
///
/// See module docs for the override / default split.
///
/// # Errors
///
/// - [`BuilderError::InvalidSlug`] if `slug` is malformed.
/// - [`BuilderError::NoHomeDirectory`] when the default location is selected
///   but `$HOME` cannot be resolved.
/// - [`BuilderError::Git`] when the clone or refresh shells out non-zero.
/// - [`BuilderError::Io`] when the parent directory cannot be created.
pub fn ensure_local_checkout(
    slug: &str,
    override_path: Option<&Path>,
) -> Result<PathBuf, BuilderError> {
    let (owner, repo) = parse_slug(slug)?;

    if let Some(path) = override_path {
        if !path.exists() {
            return Err(BuilderError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "--repo-path does not exist",
                ),
            });
        }
        if !path.is_dir() {
            return Err(BuilderError::Io {
                path: path.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "--repo-path is not a directory",
                ),
            });
        }
        // Canonicalize so downstream filesystem ops always see an absolute
        // path. If canonicalize fails (unusual on an existing path), fall
        // back to the as-given form rather than failing the operation —
        // the existence and directory checks above already cover the common
        // error cases.
        return Ok(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()));
    }

    let default = default_checkout_path(owner, repo)?;
    if default.exists() {
        refresh_existing(&default)?;
    } else {
        clone_fresh(slug, &default)?;
    }
    Ok(default.canonicalize().unwrap_or(default))
}

/// Compute the default checkout path: `~/.phantom/builds/<owner>-<repo>`.
///
/// # Errors
///
/// Returns [`BuilderError::NoHomeDirectory`] when `$HOME` cannot be resolved.
pub fn default_checkout_path(owner: &str, repo: &str) -> Result<PathBuf, BuilderError> {
    let home = dirs::home_dir().ok_or(BuilderError::NoHomeDirectory)?;
    Ok(home
        .join(".phantom")
        .join("builds")
        .join(format!("{owner}-{repo}")))
}

/// Run `git fetch && git checkout origin/main` against an existing clone.
///
/// We tolerate failures of `git checkout origin/main` because some repos use
/// `master` or another default branch. In that case the directory stays on
/// whatever the working tree had — the loop runner will pick up the issue
/// and the agent will deal with it.
fn refresh_existing(path: &Path) -> Result<(), BuilderError> {
    tracing::info!(path = %path.display(), "refreshing existing builder checkout");
    let fetch = Command::new("git")
        .args(["fetch", "--quiet", "--all", "--prune"])
        .current_dir(path)
        .output()
        .map_err(|e| BuilderError::Git(format!("git fetch failed to spawn: {e}")))?;
    if !fetch.status.success() {
        return Err(BuilderError::Git(format!(
            "git fetch returned {}: {}",
            fetch.status,
            String::from_utf8_lossy(&fetch.stderr).trim()
        )));
    }
    // Best-effort checkout to origin/main. A non-zero exit is logged but not
    // fatal — see the function header comment.
    let checkout = Command::new("git")
        .args(["checkout", "--quiet", "origin/main"])
        .current_dir(path)
        .output();
    match checkout {
        Ok(out) if out.status.success() => {
            tracing::debug!("git checkout origin/main succeeded");
        }
        Ok(out) => {
            tracing::warn!(
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "git checkout origin/main returned non-zero — leaving worktree as-is",
            );
        }
        Err(e) => {
            tracing::warn!("git checkout origin/main failed to spawn: {e}");
        }
    }
    Ok(())
}

/// Run `git clone https://github.com/<slug>.git <dest>`.
fn clone_fresh(slug: &str, dest: &Path) -> Result<(), BuilderError> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|source| BuilderError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let url = format!("https://github.com/{slug}.git");
    tracing::info!(url = %url, dest = %dest.display(), "cloning builder target");
    let out = Command::new("git")
        .args(["clone", "--quiet", &url])
        .arg(dest)
        .output()
        .map_err(|e| BuilderError::Git(format!("git clone failed to spawn: {e}")))?;
    if !out.status.success() {
        return Err(BuilderError::Git(format!(
            "git clone {url} returned {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn default_path_layout_uses_owner_dash_repo() {
        // Skip when $HOME is unset (rare in CI but possible).
        if dirs::home_dir().is_none() {
            return;
        }
        let path = default_checkout_path("jdmiranda", "phantom").unwrap();
        let last = path.components().next_back().unwrap();
        assert_eq!(last.as_os_str(), "jdmiranda-phantom");
        let parent = path.parent().unwrap();
        assert_eq!(parent.file_name().unwrap(), "builds");
        let grand = parent.parent().unwrap();
        assert_eq!(grand.file_name().unwrap(), ".phantom");
    }

    #[test]
    fn override_path_returns_absolute_directory_when_it_exists() {
        let tmp = tempdir().unwrap();
        let path = ensure_local_checkout("foo/bar", Some(tmp.path())).unwrap();
        assert!(path.is_absolute());
        assert!(path.exists());
    }

    #[test]
    fn override_path_rejects_missing_directory() {
        let tmp = tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let err = ensure_local_checkout("foo/bar", Some(&missing)).unwrap_err();
        assert!(matches!(err, BuilderError::Io { .. }));
    }

    #[test]
    fn override_path_rejects_file_instead_of_directory() {
        let tmp = tempdir().unwrap();
        let file = tmp.path().join("a-file");
        std::fs::write(&file, "not a directory").unwrap();
        let err = ensure_local_checkout("foo/bar", Some(&file)).unwrap_err();
        assert!(matches!(err, BuilderError::Io { .. }));
    }

    #[test]
    fn invalid_slug_short_circuits_before_filesystem_work() {
        let tmp = tempdir().unwrap();
        let err = ensure_local_checkout("not-a-slug", Some(tmp.path())).unwrap_err();
        assert!(matches!(err, BuilderError::InvalidSlug(_)));
    }
}
