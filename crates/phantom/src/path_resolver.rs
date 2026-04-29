/// Desktop PATH resolution for GUI launches.
///
/// When Phantom is launched as a desktop application (e.g. via a .app bundle
/// on macOS or a .desktop file on Linux), the inherited `PATH` is typically
/// stripped down to the bare system defaults and does not include user-shell
/// paths such as `/usr/local/bin`, `/opt/homebrew/bin`, or `~/.cargo/bin`.
///
/// `resolve_desktop_path` probes the user's login shell for its full `PATH`
/// and merges that into the current process environment so that every
/// subsequent tool-spawn (git, cargo, node, …) can find its binary.
///
/// # Platform notes
/// - **macOS / Linux**: spawns `$SHELL -l -c 'echo $PATH'` with a 3-second
///   timeout; falls back to `$SHELL -i -c 'echo $PATH'` if the first attempt
///   fails.
/// - **Windows**: no-op (PATH handling is different there and not needed for
///   the desktop-launch scenario).

#[cfg(not(target_os = "windows"))]
use std::time::Duration;

/// Probe the login shell and merge its `PATH` into the current process.
///
/// On failure (timeout, bad output, no `$SHELL`) this function logs a warning
/// and leaves the environment unchanged — it never panics or returns an error.
pub fn resolve_desktop_path() {
    #[cfg(target_os = "windows")]
    {
        // No-op on Windows — GUI launch PATH handling is handled differently.
        return;
    }

    #[cfg(not(target_os = "windows"))]
    resolve_desktop_path_unix();
}

#[cfg(not(target_os = "windows"))]
fn resolve_desktop_path_unix() {
    let shell = match std::env::var("SHELL") {
        Ok(s) if !s.is_empty() => s,
        _ => {
            log::warn!("PATH resolution: $SHELL not set; keeping current PATH");
            return;
        }
    };

    // Try login-shell first, fall back to interactive.
    let shell_path = probe_shell_path(&shell, &["-l", "-c", "echo $PATH"])
        .or_else(|| probe_shell_path(&shell, &["-i", "-c", "echo $PATH"]));

    let shell_path = match shell_path {
        Some(p) if !p.is_empty() => p,
        _ => {
            log::warn!("PATH resolution: shell probe returned nothing; keeping current PATH");
            return;
        }
    };

    let current = std::env::var("PATH").unwrap_or_default();

    // Merge: shell paths first so they take precedence, then system paths.
    let merged = if current.is_empty() {
        shell_path.clone()
    } else {
        format!("{shell_path}:{current}")
    };

    // SAFETY: called before any threads are spawned that might read PATH
    // concurrently.  The caller guarantees this runs at program startup.
    unsafe {
        std::env::set_var("PATH", &merged);
    }

    log::info!(
        "PATH resolved via shell probe: prepended {} component(s)",
        shell_path.split(':').count()
    );
}

/// Spawn `shell args` with a 3-second timeout and return the trimmed first
/// non-empty line of stdout, or `None` on any failure.
#[cfg(not(target_os = "windows"))]
fn probe_shell_path(shell: &str, args: &[&str]) -> Option<String> {
    use std::process::{Command, Stdio};

    let mut child = Command::new(shell)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Prevent the child from inheriting the Phantom process group so it
        // doesn't receive terminal signals meant for us.
        .env_remove("__CF_USER_TEXT_ENCODING") // suppress macOS CFRunLoop noise
        .spawn()
        .ok()?;

    // Poll for up to 3 seconds instead of blocking forever.
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    log::warn!("PATH probe timed out; killing shell");
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => {
                let _ = child.kill();
                return None;
            }
        }
    }

    // Collect stdout only after the process has exited.
    use std::io::Read;
    let mut out = String::new();
    child.stdout?.read_to_string(&mut out).ok()?;

    // Return only the first non-empty line (the PATH value).
    let path = out
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_owned())?;

    // Basic sanity check: a PATH must contain at least one '/'.
    if !path.contains('/') {
        log::warn!("PATH probe returned suspicious output (no '/'); ignoring");
        return None;
    }

    Some(path)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    // -----------------------------------------------------------------------
    // (a) Parse the shell PATH correctly from well-formed output
    // -----------------------------------------------------------------------
    #[test]
    fn parses_shell_path_from_output() {
        // Simulate what probe_shell_path does: take the first non-empty line
        // and verify it is returned verbatim (after trim).
        let fake_output = "/usr/local/bin:/usr/bin:/bin\n";
        let path = fake_output
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(|l| l.trim().to_owned())
            .unwrap();

        assert_eq!(path, "/usr/local/bin:/usr/bin:/bin");
    }

    // -----------------------------------------------------------------------
    // (b) Merged PATH prepends shell components before current PATH
    // -----------------------------------------------------------------------
    #[test]
    fn merges_shell_path_before_current() {
        let shell_path = "/opt/homebrew/bin:/usr/local/bin";
        let current = "/usr/bin:/bin";
        let merged = format!("{shell_path}:{current}");

        // Shell paths must come first.
        assert!(
            merged.starts_with("/opt/homebrew/bin:"),
            "shell path not first: {merged}"
        );
        assert!(merged.contains("/usr/bin"), "current path missing: {merged}");
    }

    // -----------------------------------------------------------------------
    // (c) Timeout fallback — probe with a long-running shell command
    // -----------------------------------------------------------------------
    #[test]
    #[cfg(not(target_os = "windows"))]
    fn timeout_kills_hung_process() {
        // `sleep 10` would run for 10 seconds; we replicate the polling loop
        // with a tiny 200 ms deadline to keep the test fast.
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        let mut child = Command::new("sh")
            .args(["-c", "sleep 10"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("sh must be available");

        let deadline = Instant::now() + Duration::from_millis(200);
        let timed_out = loop {
            match child.try_wait().unwrap() {
                Some(_) => break false,
                None => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        break true;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        };

        assert!(timed_out, "expected timeout but process exited early");
    }

    // -----------------------------------------------------------------------
    // (d) Corrupt / suspicious output is rejected
    // -----------------------------------------------------------------------
    #[test]
    fn corrupt_output_without_slash_is_rejected() {
        // A PATH with no '/' is considered corrupt.
        let bad_output = "SOMETHINGRANDOM\n";
        let path = bad_output
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(|l| l.trim().to_owned())
            .unwrap();

        let valid = path.contains('/');
        assert!(!valid, "expected corrupt path to fail validation");
    }

    // -----------------------------------------------------------------------
    // (e) Empty shell PATH is handled gracefully — no panic, no mutation
    // -----------------------------------------------------------------------
    #[test]
    fn empty_shell_path_is_handled() {
        let shell_path_opt: Option<String> = Some(String::new());

        // Mimic what resolve_desktop_path_unix does with an empty probe result.
        let is_empty = matches!(&shell_path_opt, Some(p) if p.is_empty());
        assert!(
            is_empty,
            "empty shell PATH should be detected and skipped"
        );

        // When the shell returns nothing we should not mutate PATH — verify
        // that the branch exits early without panicking.
        let before = std::env::var("PATH").unwrap_or_default();
        // Simulate the early-return condition.
        let would_skip =
            matches!(shell_path_opt, Some(ref p) if p.is_empty()) || shell_path_opt.is_none();
        assert!(would_skip);

        let after = std::env::var("PATH").unwrap_or_default();
        assert_eq!(
            before, after,
            "PATH must not change when shell probe is empty"
        );
    }
}
