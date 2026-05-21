//! Startup pre-flight checks for `phantom loop run`.
//!
//! Before a [`crate::LoopRunner`] is allowed to start, the CLI runs a set of
//! environment gates so the user gets a clean diagnostic *before* an agent
//! is spawned or a `gh` call is attempted in anger. The four gates are:
//!
//! 1. **`gh` binary present.** `gh --version` exits zero. Refuse to start if
//!    the GitHub CLI is missing.
//! 2. **`gh auth status` returns authenticated.** Refuse otherwise.
//! 3. **File lock at `<repo>/.phantom/loops/.runlock`.** A best-effort
//!    advisory lock prevents two `phantom loop run` invocations from racing
//!    on the same repo's loops. The lock is freed by [`RunLock::drop`].
//! 4. **MCP collision check.** No registered MCP tool may shadow the
//!    lifecycle tool names `complete_task` or `abort_task` — those are the
//!    runner's exit channel and an MCP override would silently break loop
//!    termination.
//!
//! Each gate returns a typed [`PreflightError`] on failure so the CLI can
//! pick its own error-rendering strategy.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Reserved tool names the runner depends on. An MCP server that registers
/// a tool by either of these names would shadow the lifecycle tool and
/// break loop completion.
pub const RESERVED_TOOL_NAMES: &[&str] = &["complete_task", "abort_task"];

/// Errors a pre-flight gate can produce.
#[derive(Debug, thiserror::Error)]
pub enum PreflightError {
    /// `gh --version` could not be executed (missing binary).
    #[error(
        "GitHub CLI (`gh`) is not installed or not in PATH — install it from \
         https://cli.github.com/ before running `phantom loop run`"
    )]
    GhMissing,

    /// `gh --version` ran but exited non-zero.
    #[error("`gh --version` exited non-zero — stderr: {stderr}")]
    GhBroken { stderr: String },

    /// `gh auth status` reports the user is not authenticated.
    #[error(
        "GitHub CLI is not authenticated — run `gh auth login` to authenticate, \
         then re-run `phantom loop run`"
    )]
    GhNotAuthenticated,

    /// `gh auth status` failed for a non-auth reason (network, panic, etc.).
    #[error("`gh auth status` failed: {detail}")]
    GhAuthStatusFailed { detail: String },

    /// The repo's `.phantom/loops/.runlock` is held by another process.
    #[error(
        "another `phantom loop run` invocation is already running on this repo \
         — its lock file is at {path}"
    )]
    LockHeld { path: PathBuf },

    /// Could not write to the lock-file path (permissions or missing dir).
    #[error("could not create runlock at {path}: {source}")]
    LockIoError {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// An MCP server has registered a tool that shadows a reserved name.
    #[error(
        "MCP tool name {name} collides with a runner-reserved lifecycle tool \
         — disable that MCP server or rename the tool before running loops"
    )]
    McpCollision { name: String },
}

/// Verify `gh --version` works.
///
/// # Errors
///
/// [`PreflightError::GhMissing`] if the binary is absent;
/// [`PreflightError::GhBroken`] if it ran but exited non-zero.
pub fn check_gh_binary() -> Result<(), PreflightError> {
    let out = Command::new("gh")
        .arg("--version")
        .output()
        .map_err(|_| PreflightError::GhMissing)?;
    if !out.status.success() {
        return Err(PreflightError::GhBroken {
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        });
    }
    Ok(())
}

/// Verify `gh auth status` reports the user is authenticated.
///
/// `gh auth status` exits 0 when authenticated, 1 otherwise. Stderr carries
/// the human-readable reason in both cases.
///
/// # Errors
///
/// [`PreflightError::GhNotAuthenticated`] when not authenticated;
/// [`PreflightError::GhAuthStatusFailed`] for non-zero exit with non-auth
/// reasons (very rare).
pub fn check_gh_auth() -> Result<(), PreflightError> {
    let out = Command::new("gh")
        .args(["auth", "status"])
        .output()
        .map_err(|e| PreflightError::GhAuthStatusFailed {
            detail: format!("could not exec `gh`: {e}"),
        })?;
    if out.status.success() {
        return Ok(());
    }
    // gh exits 1 when not authenticated. Distinguish from "binary broken"
    // by checking the stderr signature.
    Err(PreflightError::GhNotAuthenticated)
}

/// Verify no registered MCP tool shadows a reserved lifecycle tool name.
///
/// `tool_names` should come from
/// [`phantom_mcp::registry::McpToolRegistry::tool_names`]. Passes an empty
/// iterator when no registry is configured (the CLI's typical case).
///
/// # Errors
///
/// [`PreflightError::McpCollision`] for the first reserved name that
/// appears in `tool_names`.
pub fn check_mcp_collisions<I, S>(tool_names: I) -> Result<(), PreflightError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    for name in tool_names {
        let n = name.as_ref();
        if RESERVED_TOOL_NAMES.contains(&n) {
            return Err(PreflightError::McpCollision {
                name: n.to_string(),
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// RunLock — file-backed advisory lock
// ---------------------------------------------------------------------------

/// RAII guard for `<repo>/.phantom/loops/.runlock`.
///
/// Created exclusively (`O_CREAT | O_EXCL`) so a second invocation
/// targeting the same repo fails fast. Removed on drop; if the process
/// crashes the file persists, but a fresh `phantom loop run` invocation
/// produces a clear error pointing at the stale lock path which the user
/// can `rm`.
#[derive(Debug)]
#[must_use = "drop the RunLock to release the .runlock file"]
pub struct RunLock {
    path: PathBuf,
}

impl RunLock {
    /// Acquire the runlock for `repo_root`. Creates the parent directory
    /// (`<repo>/.phantom/loops/`) if needed.
    ///
    /// # Errors
    ///
    /// [`PreflightError::LockHeld`] if the lockfile already exists;
    /// [`PreflightError::LockIoError`] for any other I/O failure
    /// (permission denied, missing repo root, etc.).
    pub fn acquire(repo_root: &Path) -> Result<Self, PreflightError> {
        let dir = repo_root.join(".phantom").join("loops");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            return Err(PreflightError::LockIoError {
                path: dir,
                source: e,
            });
        }
        let path = dir.join(".runlock");
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(PreflightError::LockHeld { path });
            }
            Err(e) => {
                return Err(PreflightError::LockIoError { path, source: e });
            }
        };
        // Write the pid + start time for diagnostic value. Failures here
        // are non-fatal — the *existence* of the file is the lock.
        let pid = std::process::id();
        let _ = writeln!(
            file,
            "pid={pid} acquired_at_unix_ms={}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
        Ok(Self { path })
    }

    /// Path of the underlying lock file. For tests and diagnostics.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        // Best-effort. Files left behind on crash are recoverable via `rm`.
        let _ = std::fs::remove_file(&self.path);
    }
}

// ---------------------------------------------------------------------------
// Helper for test factories
// ---------------------------------------------------------------------------

/// Touch the runlock file directly without acquiring a [`RunLock`]. Used
/// by tests that want to simulate a stale or concurrently-held lock.
#[cfg(test)]
pub(crate) fn touch_runlock_for_test(repo_root: &Path) -> std::io::Result<PathBuf> {
    let dir = repo_root.join(".phantom").join("loops");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(".runlock");
    std::fs::File::create(&path)?;
    Ok(path)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_mcp_collisions_passes_empty() {
        let names: Vec<&str> = vec![];
        check_mcp_collisions(names).expect("empty iter must pass");
    }

    #[test]
    fn check_mcp_collisions_passes_non_reserved() {
        let names = vec!["fs.read_file", "http.get", "weather.forecast"];
        check_mcp_collisions(names).expect("non-reserved names must pass");
    }

    #[test]
    fn check_mcp_collisions_rejects_complete_task() {
        let names = vec!["something", "complete_task"];
        let err = check_mcp_collisions(names).expect_err("must reject");
        match err {
            PreflightError::McpCollision { name } => assert_eq!(name, "complete_task"),
            other => panic!("expected McpCollision, got {other:?}"),
        }
    }

    #[test]
    fn check_mcp_collisions_rejects_abort_task() {
        let names = vec!["abort_task"];
        let err = check_mcp_collisions(names).expect_err("must reject");
        match err {
            PreflightError::McpCollision { name } => assert_eq!(name, "abort_task"),
            other => panic!("expected McpCollision, got {other:?}"),
        }
    }

    #[test]
    fn run_lock_acquire_then_drop_round_trips() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let lock = RunLock::acquire(tmp.path()).expect("acquire");
        let lock_path = lock.path().to_path_buf();
        assert!(lock_path.exists(), "lock file must exist while held");
        drop(lock);
        assert!(!lock_path.exists(), "lock file must be removed on drop");
    }

    #[test]
    fn run_lock_double_acquire_fails_with_lock_held() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _first = RunLock::acquire(tmp.path()).expect("first acquire");
        let err = RunLock::acquire(tmp.path()).expect_err("second must fail");
        assert!(matches!(err, PreflightError::LockHeld { .. }));
    }

    #[test]
    fn run_lock_acquire_fails_when_stale_file_exists() {
        let tmp = tempfile::tempdir().expect("tempdir");
        touch_runlock_for_test(tmp.path()).expect("touch");
        let err = RunLock::acquire(tmp.path()).expect_err("must fail");
        assert!(matches!(err, PreflightError::LockHeld { .. }));
    }
}
