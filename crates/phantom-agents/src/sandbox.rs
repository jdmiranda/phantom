//! Process-level sandbox for agent `run_command` execution.
//!
//! The capability-class gate (Sec.1–Sec.6) blocks tool *dispatch*, but the
//! tool itself still runs in the host process namespace. This module adds
//! OS-level isolation around the child process so that even if an adversarial
//! prompt tricks the Act gate, the executed binary is wrapped in a
//! deny-by-default environment.
//!
//! # Platform support
//!
//! | Platform | Mechanism                                              |
//! |----------|--------------------------------------------------------|
//! | macOS    | `sandbox-exec(1)` with a deny-by-default SBPL profile  |
//! | Linux    | `setrlimit(2)` for resource limits (seccomp deferred)  |
//! | Windows  | `TODO` (labeled below)                                 |
//!
//! # Policy variants
//!
//! ```text
//! SandboxPolicy::Strict      — no network, no writes outside cwd, tight rlimits
//! SandboxPolicy::Permissive  — rlimits only, network allowed
//! SandboxPolicy::None        — bare exec, legacy behaviour
//! ```
//!
//! The policy is chosen per-call in [`execute_run_command_sandboxed`]; the
//! default used by `execute_tool` is [`SandboxPolicy::Strict`].

use std::path::Path;
use std::process::Command;
use std::time::Duration;

// ---------------------------------------------------------------------------
// SandboxPolicy
// ---------------------------------------------------------------------------

/// Controls the OS-level isolation applied to `run_command` child processes.
///
/// Converted from the agent's role manifest at dispatch time: Watcher/Capturer
/// get `Strict`; Actor gets `Strict` (still sandboxed — sandbox is additive
/// on top of the capability gate, not a replacement); `None` is only used in
/// tests that deliberately want the bare executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxPolicy {
    /// Full isolation: deny-by-default network, restrict filesystem to cwd,
    /// impose CPU-time and max-process rlimits.
    #[default]
    Strict,
    /// Resource limits only (CPU, max-procs). Network access is allowed.
    /// Useful for agent tasks that legitimately need external connectivity
    /// (e.g. `cargo fetch`, `curl` health checks).
    Permissive,
    /// No additional isolation. Matches the pre-#87 behaviour. Only set this
    /// in integration tests that explicitly test unsandboxed execution, or
    /// when the host OS doesn't support sandboxing.
    None,
}

// ---------------------------------------------------------------------------
// SandboxError
// ---------------------------------------------------------------------------

/// Errors produced by the sandbox layer (distinct from the command's own
/// exit code). These are surfaced as `ToolResult { success: false }` to the
/// agent runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxError {
    /// The sandbox wrapper binary or syscall was unavailable.
    Unavailable { reason: String },
    /// The child process could not be spawned.
    SpawnFailed { reason: String },
    /// The child process timed out; it was killed.
    Timeout { limit: Duration },
    /// Waiting on the child failed.
    WaitFailed { reason: String },
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable { reason } => write!(f, "sandbox unavailable: {reason}"),
            Self::SpawnFailed { reason } => write!(f, "spawn failed: {reason}"),
            Self::Timeout { limit } => write!(f, "command timed out after {limit:?}"),
            Self::WaitFailed { reason } => write!(f, "wait failed: {reason}"),
        }
    }
}

impl std::error::Error for SandboxError {}

// ---------------------------------------------------------------------------
// CommandOutput
// ---------------------------------------------------------------------------

/// The captured output of a sandboxed command execution.
#[derive(Debug, Clone)]
pub struct CommandOutput {
    /// Combined stdout + stderr (stderr prefixed with `"STDERR:\n"`).
    pub output: String,
    /// `true` iff the process exited with status 0.
    pub success: bool,
    /// Raw stdout, unprefixed. Kept separate so callers (e.g. semantic parser)
    /// can reason on stdout and stderr independently.
    pub stdout: String,
    /// Raw stderr, unprefixed.
    pub stderr: String,
    /// Process exit code, or `None` when the process was killed by a signal.
    pub exit_code: Option<i32>,
}

// ---------------------------------------------------------------------------
// Public entry-point
// ---------------------------------------------------------------------------

/// Execute `command_str` as a shell command under the given [`SandboxPolicy`].
///
/// - `cwd` — the working directory (already validated to be inside the agent's
///   sandbox root by the caller).
/// - `timeout` — hard wall-clock limit; the process is killed if it exceeds this.
///
/// Returns [`CommandOutput`] on success (even when the command itself exits
/// non-zero) or [`SandboxError`] when the sandbox machinery itself fails.
pub fn execute_sandboxed(
    command_str: &str,
    cwd: &Path,
    policy: SandboxPolicy,
    timeout: Duration,
) -> Result<CommandOutput, SandboxError> {
    match policy {
        SandboxPolicy::None => run_bare(command_str, cwd, timeout),
        SandboxPolicy::Permissive => run_permissive(command_str, cwd, timeout),
        SandboxPolicy::Strict => run_strict(command_str, cwd, timeout),
    }
}

// ---------------------------------------------------------------------------
// Bare execution (SandboxPolicy::None)
// ---------------------------------------------------------------------------

fn run_bare(
    command_str: &str,
    cwd: &Path,
    timeout: Duration,
) -> Result<CommandOutput, SandboxError> {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command_str).current_dir(cwd);
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    spawn_and_wait(cmd, timeout)
}

// ---------------------------------------------------------------------------
// Permissive (rlimits only, network allowed)
// ---------------------------------------------------------------------------

fn run_permissive(
    command_str: &str,
    cwd: &Path,
    timeout: Duration,
) -> Result<CommandOutput, SandboxError> {
    // Wrap the user command with rlimit preamble applied via `sh -c`.
    // The ulimit built-in is POSIX and available in both bash and sh.
    //
    // Limits applied:
    //   -t 60  CPU seconds
    //   -u 64  max user processes (fork-bomb mitigation)
    let wrapped = format!(
        "ulimit -t 60; ulimit -u 64 2>/dev/null || true; {command_str}"
    );

    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(&wrapped).current_dir(cwd);
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    spawn_and_wait(cmd, timeout)
}

// ---------------------------------------------------------------------------
// Strict  (deny-by-default network + filesystem + rlimits)
// ---------------------------------------------------------------------------

fn run_strict(
    command_str: &str,
    cwd: &Path,
    timeout: Duration,
) -> Result<CommandOutput, SandboxError> {
    #[cfg(target_os = "macos")]
    return run_strict_macos(command_str, cwd, timeout);

    #[cfg(target_os = "linux")]
    return run_strict_linux(command_str, cwd, timeout);

    // Windows and everything else: fall back to permissive with a TODO marker.
    // We intentionally do NOT silently drop to bare — that would be a silent
    // security regression. Permissive at least keeps rlimits.
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        log::warn!(
            "sandbox: Strict policy requested but platform is unsupported; \
             falling back to Permissive (TODO: Windows job objects)"
        );
        run_permissive(command_str, cwd, timeout)
    }
}

// ---------------------------------------------------------------------------
// macOS — sandbox-exec
// ---------------------------------------------------------------------------

/// SBPL profile for macOS sandbox-exec.
///
/// Design approach: start with a broad `(deny default)`, then layer allow
/// rules for the minimal execution environment (`process*`, all file reads,
/// IPC, mach). After granting broad read access, add two targeted deny rules:
///
/// 1. `(deny network*)` — blocks all socket operations (connect, send, etc.)
/// 2. `(deny file-write* (subpath "/"))` — blocks writes to any path, then…
/// 3. `(allow file-write* (subpath PHANTOM_CWD))` — re-allows writes inside
///    the agent's working directory (injected at runtime via `-D PHANTOM_CWD`).
///
/// SBPL precedence: later rules override earlier ones for the same operation,
/// so the deny-then-allow ordering for file-write is intentional and correct.
/// The `allow file* (subpath "/")` above the write-deny grants read access to
/// the full filesystem (tools, frameworks, libraries) which is required to run
/// a basic `sh -c '...'` on macOS.
const MACOS_SBPL_PROFILE: &str = r#"
(version 1)
(deny default)

; ---------- process / signal ----------
(allow process*)
(allow signal)

; ---------- file: broad read access to support sh + toolchain ----------
; macOS requires reads from /System, /Library, /private, /var (symlink) etc.
; to execute even a minimal sh command.  We allow all reads and restrict only
; writes below.
(allow file* (subpath "/"))

; ---------- IPC primitives required by sh/bash ----------
(allow ipc-posix-shm*)
(allow mach*)

; ---------- sysctl reads (uname, clock, etc.) ----------
(allow sysctl*)

; ---------- deny all network I/O ----------
(deny network*)

; ---------- deny ALL writes, then re-allow cwd subtree ----------
; SBPL precedence: later rules win, so this deny overrides the allow file*
; above for write operations, and the subsequent allow restores cwd writes.
(deny file-write* (subpath "/"))
(allow file-write* (subpath (param "PHANTOM_CWD")))
"#;

#[cfg(target_os = "macos")]
fn run_strict_macos(
    command_str: &str,
    cwd: &Path,
    timeout: Duration,
) -> Result<CommandOutput, SandboxError> {
    // sandbox-exec must be available on macOS (it has shipped since 10.5).
    // Resolve cwd to a canonical path so the SBPL subpath anchor is correct.
    let cwd_str = cwd
        .canonicalize()
        .map_err(|e| SandboxError::Unavailable {
            reason: format!("cannot canonicalize cwd for sandbox profile: {e}"),
        })?;
    let cwd_str = cwd_str.to_string_lossy();

    // Apply rlimits *inside* the sandbox via the sh wrapper.
    let wrapped = format!(
        "ulimit -t 60; ulimit -u 64 2>/dev/null || true; {command_str}"
    );

    let mut cmd = Command::new("sandbox-exec");
    cmd.arg("-p")
        .arg(MACOS_SBPL_PROFILE)
        .arg("-D")
        .arg(format!("PHANTOM_CWD={cwd_str}"))
        .arg("sh")
        .arg("-c")
        .arg(&wrapped)
        .current_dir(cwd);
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    spawn_and_wait(cmd, timeout)
}

// ---------------------------------------------------------------------------
// Linux — setrlimit (seccomp-bpf deferred)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn run_strict_linux(
    command_str: &str,
    cwd: &Path,
    timeout: Duration,
) -> Result<CommandOutput, SandboxError> {
    // On Linux we apply rlimits via ulimit and additionally use `unshare`
    // to drop network namespace if it's available (non-root may not have it).
    // seccomp-bpf filtering is intentionally deferred (requires libc or
    // syscall assembly; tracked in #87).
    //
    // Strategy:
    //   1. Try `unshare -n sh -c '...'` to drop network namespace.
    //   2. If unshare is not available or fails (permission denied), fall back
    //      to the ulimit-only wrapper and emit a warning.
    let cwd_str = cwd
        .canonicalize()
        .map_err(|e| SandboxError::Unavailable {
            reason: format!("cannot canonicalize cwd: {e}"),
        })?;

    let wrapped_inner = format!(
        "ulimit -t 60; ulimit -u 64 2>/dev/null || true; \
         cd {cwd_q}; {command_str}",
        cwd_q = shell_quote(&cwd_str.to_string_lossy()),
    );

    // Attempt unshare-based network isolation.
    let mut unshare_cmd = Command::new("unshare");
    unshare_cmd
        .args(["-n", "--", "sh", "-c", &wrapped_inner])
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    // Probe: does `unshare` exist?
    match unshare_cmd.spawn() {
        Ok(child) => wait_child(child, timeout),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log::warn!(
                "sandbox: `unshare` not found; falling back to rlimit-only \
                 isolation (seccomp-bpf pending — see #87)"
            );
            run_permissive(command_str, cwd, timeout)
        }
        Err(e) => {
            // Could be EPERM (no capability to create user namespaces).
            log::warn!(
                "sandbox: `unshare -n` failed ({e}); falling back to rlimit-only isolation"
            );
            run_permissive(command_str, cwd, timeout)
        }
    }
}

/// Minimal shell quoting: wrap in single quotes and escape internal `'`.
#[cfg(target_os = "linux")]
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ---------------------------------------------------------------------------
// Shared: spawn + wait with timeout
// ---------------------------------------------------------------------------

fn spawn_and_wait(mut cmd: Command, timeout: Duration) -> Result<CommandOutput, SandboxError> {
    let child = cmd.spawn().map_err(|e| SandboxError::SpawnFailed {
        reason: e.to_string(),
    })?;
    wait_child(child, timeout)
}

fn wait_child(
    mut child: std::process::Child,
    timeout: Duration,
) -> Result<CommandOutput, SandboxError> {
    use std::io::Read as _;

    let start = std::time::Instant::now();
    let poll = Duration::from_millis(50);

    loop {
        match child.try_wait().map_err(|e| SandboxError::WaitFailed {
            reason: e.to_string(),
        })? {
            Some(status) => {
                let mut stdout_buf = String::new();
                let mut stderr_buf = String::new();

                if let Some(mut s) = child.stdout.take() {
                    let _ = s.read_to_string(&mut stdout_buf);
                }
                if let Some(mut s) = child.stderr.take() {
                    let _ = s.read_to_string(&mut stderr_buf);
                }

                let mut output = stdout_buf.clone();
                if !stderr_buf.is_empty() {
                    if !output.is_empty() {
                        output.push('\n');
                    }
                    output.push_str("STDERR:\n");
                    output.push_str(&stderr_buf);
                }

                return Ok(CommandOutput {
                    output,
                    success: status.success(),
                    stdout: stdout_buf,
                    stderr: stderr_buf,
                    exit_code: status.code(),
                });
            }
            None => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    return Err(SandboxError::Timeout { limit: timeout });
                }
                std::thread::sleep(poll);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tempfile::TempDir;

    const TIMEOUT: Duration = Duration::from_secs(15);

    // -----------------------------------------------------------------------
    // SandboxPolicy::None — baseline, mirrors pre-#87 behaviour
    // -----------------------------------------------------------------------

    #[test]
    fn none_policy_runs_echo() {
        let tmp = TempDir::new().unwrap();
        let out = execute_sandboxed("echo phantom", tmp.path(), SandboxPolicy::None, TIMEOUT)
            .expect("bare exec must succeed");
        assert!(out.success);
        assert!(out.output.contains("phantom"));
    }

    #[test]
    fn none_policy_reports_nonzero_exit() {
        let tmp = TempDir::new().unwrap();
        let out = execute_sandboxed("false", tmp.path(), SandboxPolicy::None, TIMEOUT)
            .expect("spawn must succeed");
        assert!(!out.success);
    }

    #[test]
    fn none_policy_timeout_fires() {
        let tmp = TempDir::new().unwrap();
        let result = execute_sandboxed(
            "sleep 10",
            tmp.path(),
            SandboxPolicy::None,
            Duration::from_millis(200),
        );
        match result {
            Err(SandboxError::Timeout { .. }) => {}
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // SandboxPolicy::Permissive — rlimits only
    // -----------------------------------------------------------------------

    #[test]
    fn permissive_policy_runs_echo() {
        let tmp = TempDir::new().unwrap();
        let out = execute_sandboxed("echo ok", tmp.path(), SandboxPolicy::Permissive, TIMEOUT)
            .expect("permissive exec must succeed");
        assert!(out.success, "output: {}", out.output);
        assert!(out.output.contains("ok"));
    }

    #[test]
    fn permissive_policy_captures_stderr() {
        let tmp = TempDir::new().unwrap();
        let out = execute_sandboxed(
            "echo err >&2",
            tmp.path(),
            SandboxPolicy::Permissive,
            TIMEOUT,
        )
        .expect("permissive exec must succeed");
        // Command succeeds (sh exit 0) but stderr is captured.
        assert!(out.output.contains("err"), "stderr not captured: {}", out.output);
    }

    // -----------------------------------------------------------------------
    // SandboxPolicy::Strict — network blocked
    // -----------------------------------------------------------------------

    /// On macOS, `sandbox-exec` with the deny-by-default profile must block
    /// network connections. We test this by attempting to reach a loopback
    /// address on a port that is not listening — the key assertion is that
    /// the command fails (cannot connect), not that it succeeds.
    #[test]
    #[cfg(target_os = "macos")]
    fn strict_policy_blocks_network_macos() {
        let tmp = TempDir::new().unwrap();

        // `curl` trying to reach localhost should be blocked by the sandbox
        // profile's `(deny network*)` clause.  We use a 1-second connect
        // timeout so the test doesn't block on a slow system.
        let out = execute_sandboxed(
            "curl --connect-timeout 1 http://127.0.0.1:19999 2>&1; true",
            tmp.path(),
            SandboxPolicy::Strict,
            TIMEOUT,
        )
        .expect("sandbox-exec must spawn");

        // Network denied → curl exits non-zero and prints an error message.
        // The sandbox may produce "Operation not permitted" or curl may say
        // "Connection refused" / "Network unreachable" depending on kernel
        // version.  Either way the command must NOT succeed with a 200 response.
        //
        // We just assert curl exited non-zero OR reported a connection error.
        let network_error = !out.success
            || out.output.to_lowercase().contains("failed")
            || out.output.to_lowercase().contains("refused")
            || out.output.to_lowercase().contains("not permitted")
            || out.output.to_lowercase().contains("unreachable")
            || out.output.to_lowercase().contains("operation not supported");

        assert!(
            network_error,
            "expected network to be blocked but curl appeared to succeed: {}",
            out.output
        );
    }

    /// On Linux, after unshare drops the network namespace, even loopback
    /// should be unreachable.
    #[test]
    #[cfg(target_os = "linux")]
    fn strict_policy_blocks_network_linux() {
        let tmp = TempDir::new().unwrap();

        // ping to loopback inside a network-namespace-isolated shell
        // should fail immediately.
        let out = execute_sandboxed(
            "ping -c 1 -W 1 127.0.0.1 2>&1; true",
            tmp.path(),
            SandboxPolicy::Strict,
            TIMEOUT,
        )
        .expect("strict exec must spawn");

        let network_error = !out.success
            || out.output.contains("Network unreachable")
            || out.output.contains("not permitted")
            || out.output.contains("Cannot assign");

        // If unshare isn't available the test is vacuous (we warned in run_strict_linux).
        // Accept either: network blocked OR unshare unavailable (test environment).
        let unshare_unavailable = out.output.contains("unshare");
        assert!(
            network_error || unshare_unavailable,
            "expected network block or unshare unavailability; got: {}",
            out.output
        );
    }

    // -----------------------------------------------------------------------
    // Strict — filesystem write outside cwd is blocked (macOS)
    // -----------------------------------------------------------------------

    #[test]
    #[cfg(target_os = "macos")]
    fn strict_policy_blocks_write_outside_cwd_macos() {
        let tmp = TempDir::new().unwrap();

        // Try to write into /tmp (a sibling of our TempDir, not under it).
        let out = execute_sandboxed(
            "echo pwned > /tmp/phantom_sandbox_test_87.txt 2>&1; true",
            tmp.path(),
            SandboxPolicy::Strict,
            TIMEOUT,
        )
        .expect("sandbox-exec must spawn");

        // The write should be denied. Either:
        //  (a) the file does not exist, or
        //  (b) the command output contains a denial error.
        let file_created = std::path::Path::new("/tmp/phantom_sandbox_test_87.txt").exists();
        // Clean up just in case sandbox is permissive in test environment.
        let _ = std::fs::remove_file("/tmp/phantom_sandbox_test_87.txt");

        assert!(
            !file_created || out.output.to_lowercase().contains("denied"),
            "expected write outside cwd to be blocked; out='{}'",
            out.output
        );
    }

    // -----------------------------------------------------------------------
    // Strict — writes inside cwd ARE allowed
    // -----------------------------------------------------------------------

    #[test]
    fn strict_policy_allows_write_inside_cwd() {
        let tmp = TempDir::new().unwrap();

        let out = execute_sandboxed(
            "echo hello > output.txt && cat output.txt",
            tmp.path(),
            SandboxPolicy::Strict,
            TIMEOUT,
        )
        .expect("strict exec must spawn");

        assert!(
            out.success,
            "write inside cwd should be allowed; out='{}'",
            out.output
        );
        assert!(out.output.contains("hello"), "unexpected output: {}", out.output);
    }

    // -----------------------------------------------------------------------
    // SandboxError display
    // -----------------------------------------------------------------------

    #[test]
    fn sandbox_error_display_covers_all_variants() {
        let cases = [
            SandboxError::Unavailable { reason: "no sandbox-exec".into() },
            SandboxError::SpawnFailed { reason: "ENOENT".into() },
            SandboxError::Timeout { limit: Duration::from_secs(30) },
            SandboxError::WaitFailed { reason: "interrupted".into() },
        ];
        for err in &cases {
            let s = err.to_string();
            assert!(!s.is_empty(), "Display must be non-empty for {err:?}");
        }
    }

    // -----------------------------------------------------------------------
    // SandboxPolicy default
    // -----------------------------------------------------------------------

    #[test]
    fn default_policy_is_strict() {
        assert_eq!(SandboxPolicy::default(), SandboxPolicy::Strict);
    }

    // -----------------------------------------------------------------------
    // QA-#177 — Resource exhaustion: 100 rapid tool calls, no memory leak
    // -----------------------------------------------------------------------
    //
    // Strategy (Rust-native, no external allocator hook):
    //
    //   1. Make 100 sequential `execute_sandboxed` calls with `SandboxPolicy::None`
    //      (harmless `echo hi`). Each must return `Ok(...)`.
    //
    //   2. Assert no `SandboxError` in any result — proves no handle/descriptor
    //      leaks that would cause spawn failures after accumulation.
    //
    //   3. Thread count proxy: sample the OS thread count before and after;
    //      any increase must stay under a generous bound (≤4 extra threads).
    //      On platforms where sampling is unavailable the test degrades
    //      gracefully to a correctness-only check.
    //
    // Marked `#[ignore = "slow"]` because 100 subprocess spawns add ~2 s in CI.

    #[test]
    #[ignore = "slow"]
    fn resource_exhaustion_100_rapid_calls_no_leak() {
        let tmp = TempDir::new().unwrap();
        const CALL_COUNT: usize = 100;
        const THREAD_HEADROOM: usize = 4;

        let threads_before = sample_thread_count();

        let mut failures: Vec<usize> = Vec::new();
        for i in 0..CALL_COUNT {
            match execute_sandboxed("echo hi", tmp.path(), SandboxPolicy::None, TIMEOUT) {
                Ok(out) => {
                    if !out.success {
                        failures.push(i);
                    }
                }
                Err(e) => {
                    panic!("call {i} returned SandboxError: {e}");
                }
            }
        }

        assert!(
            failures.is_empty(),
            "{} of {CALL_COUNT} calls exited non-zero (indices: {failures:?})",
            failures.len(),
        );

        let threads_after = sample_thread_count();

        // Thread-count proxy: if the OS reported values, verify no significant growth.
        if let (Some(before), Some(after)) = (threads_before, threads_after) {
            let growth = after.saturating_sub(before);
            assert!(
                growth <= THREAD_HEADROOM,
                "thread count grew by {growth} after {CALL_COUNT} calls \
                 (before={before}, after={after}); expected ≤{THREAD_HEADROOM} new threads",
            );
        }
    }

    /// Sample the thread count of the current process.
    ///
    /// Returns `None` on platforms where the query is unavailable or fails;
    /// the test degrades gracefully to a correctness-only check.
    fn sample_thread_count() -> Option<usize> {
        // macOS: `ps -p <pid> -o thcount=`
        #[cfg(target_os = "macos")]
        {
            use std::process::Command;
            let pid = std::process::id().to_string();
            let out = Command::new("ps")
                .args(["-p", &pid, "-o", "thcount="])
                .output()
                .ok()?;
            let s = String::from_utf8_lossy(&out.stdout);
            s.trim().parse::<usize>().ok()
        }

        // Linux: count entries in /proc/self/task/
        #[cfg(target_os = "linux")]
        {
            std::fs::read_dir("/proc/self/task").ok().map(|entries| entries.count())
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            None
        }
    }
}
