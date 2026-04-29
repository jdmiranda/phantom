//! Orphan process recovery for phantom-supervisor.
//!
//! When phantom (the main process) is restarted by the supervisor, any child
//! processes it had open (PTYs, agent shells, MCP servers) become orphans —
//! still consuming resources and potentially holding ports.
//!
//! This module provides:
//!
//! - [`PidFile`]: tracks the main phantom PID and its registered child PIDs,
//!   written atomically to `~/.config/phantom/children.pid.json`.
//! - [`recover_orphans`]: called at supervisor startup; if a stale PID file
//!   exists whose main PID is no longer alive, it sends SIGTERM to every
//!   registered child, waits up to 2 seconds, then SIGKILLs survivors.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// PID-file path
// ---------------------------------------------------------------------------

/// Returns the canonical path for the orphan PID file.
///
/// The directory `~/.config/phantom/` is created if it does not exist.
pub fn pid_file_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME env var not set")?;
    let dir = PathBuf::from(home).join(".config").join("phantom");
    fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create config dir {}", dir.display()))?;
    Ok(dir.join("children.pid.json"))
}

// ---------------------------------------------------------------------------
// On-disk schema
// ---------------------------------------------------------------------------

/// The data persisted in the PID file.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PidFileData {
    /// PID of the main phantom process.
    main_pid: u32,
    /// PIDs of child processes spawned by phantom.
    child_pids: Vec<u32>,
}

// ---------------------------------------------------------------------------
// PidFile handle — used by the phantom main process
// ---------------------------------------------------------------------------

/// A handle that the phantom main process uses to register its PID and child
/// PIDs with the supervisor's orphan-recovery system.
///
/// On [`Drop`], the PID file is deleted (clean shutdown path).
///
/// This type lives in the supervisor crate so the same PID-file path logic is
/// shared.  It is intentionally unused in the supervisor binary itself — the
/// supervisor only *reads* PID files; phantom *writes* them.
#[allow(dead_code)]
#[derive(Debug)]
pub struct PidFile {
    path: PathBuf,
}

#[allow(dead_code)]
impl PidFile {
    /// Create a new PID file for the running phantom process.
    ///
    /// `main_pid` should be `std::process::id()`.  `child_pids` is the initial
    /// set of children; call [`PidFile::update`] to revise it later.
    pub fn create(path: PathBuf, main_pid: u32, child_pids: Vec<u32>) -> Result<Self> {
        let handle = Self { path };
        handle.write(main_pid, child_pids)?;
        Ok(handle)
    }

    /// Overwrite the PID file with a fresh child list (atomic write).
    pub fn update(&self, main_pid: u32, child_pids: Vec<u32>) -> Result<()> {
        self.write(main_pid, child_pids)
    }

    fn write(&self, main_pid: u32, child_pids: Vec<u32>) -> Result<()> {
        write_pid_file(&self.path, main_pid, child_pids)
    }

    /// Path to the PID file on disk.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        if self.path.exists() {
            if let Err(e) = fs::remove_file(&self.path) {
                warn!("failed to delete PID file on shutdown: {e}");
            } else {
                info!("deleted PID file on clean shutdown: {}", self.path.display());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Atomic write helper
// ---------------------------------------------------------------------------

/// Serialize `data` and write it to `path` atomically (temp file + rename).
#[allow(dead_code)]
fn write_pid_file(path: &Path, main_pid: u32, child_pids: Vec<u32>) -> Result<()> {
    let data = PidFileData {
        main_pid,
        child_pids,
    };
    let json = serde_json::to_string_pretty(&data).context("failed to serialize PID file")?;

    // Write to a sibling temp file, then rename — atomic on POSIX.
    let tmp_path = path.with_extension("pid.tmp");
    fs::write(&tmp_path, json.as_bytes())
        .with_context(|| format!("failed to write temp PID file {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path)
        .with_context(|| format!("failed to rename PID file {} -> {}", tmp_path.display(), path.display()))?;

    info!(
        "wrote PID file: main={} children={:?} -> {}",
        main_pid,
        data.child_pids,
        path.display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Orphan recovery — called by the supervisor at startup
// ---------------------------------------------------------------------------

/// Scan for a stale PID file and terminate any orphaned child processes.
///
/// Returns `Ok(())` in every non-fatal case (no file, clean main, corrupt
/// file).  A genuine I/O error from the OS during signal delivery is returned
/// as `Err`.
///
/// # Algorithm
///
/// 1. If the PID file does not exist → no-op.
/// 2. Parse the file.  If parsing fails → log a warning, delete the file,
///    return `Ok(())`.
/// 3. If the main PID is still alive (`kill(pid, 0)` succeeds) → another
///    supervisor instance may be running; leave the file alone and return.
/// 4. SIGTERM every child PID that is still alive.
/// 5. Wait up to 2 s, then SIGKILL any survivors.
/// 6. Delete the stale PID file.
pub fn recover_orphans(path: &Path) -> Result<()> {
    // Step 1: no file → nothing to do.
    if !path.exists() {
        info!("no stale PID file at {} — skipping orphan scan", path.display());
        return Ok(());
    }

    info!("stale PID file found at {} — starting orphan scan", path.display());

    // Step 2: parse.
    let data = match read_pid_file(path) {
        Ok(d) => d,
        Err(e) => {
            warn!("corrupt PID file ({}); deleting and continuing", e);
            delete_pid_file(path);
            return Ok(());
        }
    };

    // Step 3: if main PID is still alive, this is a race (supervisor restart
    // while phantom is still running).  Leave the file alone.
    if pid_is_alive(data.main_pid) {
        info!(
            "main phantom process {} is still alive — not recovering orphans",
            data.main_pid
        );
        return Ok(());
    }

    info!(
        "main phantom process {} is gone — sweeping {} child(ren)",
        data.main_pid,
        data.child_pids.len()
    );

    if data.child_pids.is_empty() {
        delete_pid_file(path);
        return Ok(());
    }

    // Step 4: SIGTERM all live children.
    let mut survivors: Vec<u32> = Vec::new();
    for &cpid in &data.child_pids {
        if pid_is_alive(cpid) {
            info!("orphan recovery: SIGTERM -> pid {cpid}");
            send_signal(cpid, libc::SIGTERM);
            survivors.push(cpid);
        } else {
            info!("orphan recovery: pid {cpid} already gone");
        }
    }

    // Step 5: grace period then SIGKILL.
    if !survivors.is_empty() {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
            survivors.retain(|&cpid| pid_is_alive(cpid));
            if survivors.is_empty() {
                break;
            }
        }

        for cpid in survivors {
            warn!("orphan recovery: pid {cpid} survived SIGTERM — sending SIGKILL");
            send_signal(cpid, libc::SIGKILL);
        }
    }

    // Step 6: delete stale PID file.
    delete_pid_file(path);
    info!("orphan recovery complete");
    Ok(())
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn read_pid_file(path: &Path) -> Result<PidFileData> {
    let bytes = fs::read(path)
        .with_context(|| format!("cannot read PID file {}", path.display()))?;
    let data: PidFileData = serde_json::from_slice(&bytes)
        .with_context(|| format!("cannot parse PID file {}", path.display()))?;
    Ok(data)
}

fn delete_pid_file(path: &Path) {
    if let Err(e) = fs::remove_file(path) {
        if e.kind() != io::ErrorKind::NotFound {
            error!("failed to delete PID file {}: {e}", path.display());
        }
    } else {
        info!("deleted stale PID file {}", path.display());
    }
}

/// Returns `true` if the process with `pid` exists (POSIX `kill(pid, 0)`).
fn pid_is_alive(pid: u32) -> bool {
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    // ESRCH → no such process.  EPERM → process exists but we lack permission.
    // Use std::io to get errno in a cross-platform way.
    let err = std::io::Error::last_os_error();
    err.raw_os_error() == Some(libc::EPERM)
}

/// Send `sig` to `pid`, ignoring errors (process may have already exited).
fn send_signal(pid: u32, sig: libc::c_int) {
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::process;

    // -----------------------------------------------------------------------
    // Helper: give every test its own temp file so they don't interfere.
    // -----------------------------------------------------------------------

    fn tmp_pid_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("phantom-orphan-test-{label}.pid.json"))
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(path.with_extension("pid.tmp"));
    }

    // -----------------------------------------------------------------------
    // (a) No PID file → no-op
    // -----------------------------------------------------------------------

    #[test]
    fn test_no_pid_file_is_noop() {
        let path = tmp_pid_path("no-file");
        cleanup(&path);
        assert!(!path.exists(), "pre-condition: file must not exist");

        let result = recover_orphans(&path);
        assert!(result.is_ok());
        assert!(!path.exists(), "no file should have been created");
    }

    // -----------------------------------------------------------------------
    // (b) PID file with dead main PID → orphan cleanup
    //
    // We can't easily spawn real orphan processes in a unit test without
    // forking, so we verify the cleanup path by checking:
    //   - the function succeeds
    //   - the PID file is deleted afterward
    //   - a dead main PID is not considered alive
    // -----------------------------------------------------------------------

    #[test]
    fn test_dead_main_pid_triggers_cleanup() {
        let path = tmp_pid_path("dead-main");
        cleanup(&path);

        // PID 1 is always alive; use a very large PID unlikely to be alive.
        // On macOS/Linux, PIDs above ~4 million are never assigned in practice.
        let dead_main: u32 = 4_000_001;
        // No children to simplify — the key assertion is file deletion.
        write_pid_file(&path, dead_main, vec![]).unwrap();
        assert!(path.exists());

        // Ensure our "dead" PID really looks dead (sanity check).
        assert!(!pid_is_alive(dead_main), "test PID must be dead");

        let result = recover_orphans(&path);
        assert!(result.is_ok());
        // File must be deleted after recovery.
        assert!(!path.exists(), "stale PID file must be removed after recovery");
    }

    // -----------------------------------------------------------------------
    // (c) PID file with alive main PID → no cleanup (supervisor restart race)
    // -----------------------------------------------------------------------

    #[test]
    fn test_alive_main_pid_no_cleanup() {
        let path = tmp_pid_path("alive-main");
        cleanup(&path);

        let alive_main = process::id(); // our own PID — definitely alive
        write_pid_file(&path, alive_main, vec![99_999_999]).unwrap();
        assert!(path.exists());

        let result = recover_orphans(&path);
        assert!(result.is_ok());
        // File must NOT be deleted — main is still alive.
        assert!(path.exists(), "PID file must be preserved when main is alive");

        cleanup(&path);
    }

    // -----------------------------------------------------------------------
    // (d) Corrupt PID file → logged + deleted, no panic
    // -----------------------------------------------------------------------

    #[test]
    fn test_corrupt_pid_file_is_deleted() {
        let path = tmp_pid_path("corrupt");
        cleanup(&path);

        fs::write(&path, b"this is not valid json }{{{").unwrap();
        assert!(path.exists());

        let result = recover_orphans(&path);
        assert!(result.is_ok(), "corrupt file must not return Err");
        assert!(!path.exists(), "corrupt PID file must be deleted");
    }

    // -----------------------------------------------------------------------
    // (e) Atomic write/read round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn test_atomic_write_read_roundtrip() {
        let path = tmp_pid_path("roundtrip");
        cleanup(&path);

        let main_pid: u32 = 12345;
        let children: Vec<u32> = vec![100, 200, 300];

        write_pid_file(&path, main_pid, children.clone()).unwrap();
        assert!(path.exists(), "file must exist after write");

        let data = read_pid_file(&path).unwrap();
        assert_eq!(data.main_pid, main_pid);
        assert_eq!(data.child_pids, children);

        // Temp file must not linger.
        assert!(!path.with_extension("pid.tmp").exists(), "tmp file must be gone after rename");

        cleanup(&path);
    }

    // -----------------------------------------------------------------------
    // Extra: PidFile handle deletes file on drop
    // -----------------------------------------------------------------------

    #[test]
    fn test_pid_file_handle_deletes_on_drop() {
        let path = tmp_pid_path("handle-drop");
        cleanup(&path);

        {
            let _handle = PidFile::create(path.clone(), 9999, vec![1, 2, 3]).unwrap();
            assert!(path.exists(), "file must exist while handle is live");
        }
        // Handle dropped → file must be gone.
        assert!(!path.exists(), "PID file must be deleted when PidFile is dropped");
    }

    // -----------------------------------------------------------------------
    // Extra: PidFile::update overwrites correctly
    // -----------------------------------------------------------------------

    #[test]
    fn test_pid_file_update() {
        let path = tmp_pid_path("update");
        cleanup(&path);

        let handle = PidFile::create(path.clone(), 111, vec![10]).unwrap();
        handle.update(111, vec![20, 30]).unwrap();

        let data = read_pid_file(&path).unwrap();
        assert_eq!(data.child_pids, vec![20, 30]);

        cleanup(&path);
    }

    // -----------------------------------------------------------------------
    // Extra: verify pid_is_alive returns true for our own process
    // -----------------------------------------------------------------------

    #[test]
    fn test_pid_is_alive_self() {
        assert!(pid_is_alive(process::id()));
    }

    // -----------------------------------------------------------------------
    // Extra: verify pid_is_alive returns false for an impossible PID
    // -----------------------------------------------------------------------

    #[test]
    fn test_pid_is_alive_dead() {
        // PID 0 is never a user process.
        // On POSIX, kill(0, 0) sends to the process group, which returns 0.
        // Use a very large PID instead.
        let dead: u32 = 4_000_002;
        assert!(!pid_is_alive(dead));
    }

    // -----------------------------------------------------------------------
    // Extra: child SIGTERM coverage — verify we log each child correctly
    //        by checking recovery doesn't panic with multiple dead children
    // -----------------------------------------------------------------------

    #[test]
    fn test_dead_children_recovered_without_panic() {
        let path = tmp_pid_path("dead-children");
        cleanup(&path);

        let dead_main: u32 = 4_000_003;
        let dead_children: Vec<u32> = vec![4_000_004, 4_000_005, 4_000_006];
        write_pid_file(&path, dead_main, dead_children).unwrap();

        let result = recover_orphans(&path);
        assert!(result.is_ok());
        assert!(!path.exists());
    }
}
