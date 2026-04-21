//! Foreground process name detection for PTY sessions.
//!
//! Uses `TIOCGPGRP` to get the foreground process group of a PTY, then reads
//! the process name via platform-specific APIs. On macOS this uses
//! `proc_name()`; on Linux it reads `/proc/{pid}/comm`.

use std::os::unix::io::AsRawFd;

/// Get the foreground process name for the given PTY file descriptor.
///
/// Returns `None` if the process name cannot be determined (unsupported
/// platform, invalid fd, zombie process, etc.).
pub fn foreground_process_name<F: AsRawFd>(pty_fd: &F) -> Option<String> {
    let fd = pty_fd.as_raw_fd();

    // Get the foreground process group ID via TIOCGPGRP.
    let mut pgid: libc::pid_t = 0;
    // SAFETY: TIOCGPGRP is safe on a valid PTY master fd.
    let res = unsafe { libc::ioctl(fd, libc::TIOCGPGRP, &mut pgid as *mut _) };
    if res < 0 || pgid <= 0 {
        return None;
    }

    process_name(pgid)
}

/// Get the process name for a given PID.
#[cfg(target_os = "macos")]
fn process_name(pid: libc::pid_t) -> Option<String> {
    // On macOS, use proc_name from libproc.
    // PROC_PIDPATHINFO_MAXSIZE is 4096 but we only need the short name.
    let mut buf = [0u8; 256];
    // SAFETY: proc_name is safe with a valid PID and correctly sized buffer.
    let len = unsafe {
        libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32)
    };
    if len <= 0 {
        return None;
    }
    let name = std::str::from_utf8(&buf[..len as usize]).ok()?;
    Some(name.to_string())
}

/// Get the process name for a given PID.
#[cfg(target_os = "linux")]
fn process_name(pid: libc::pid_t) -> Option<String> {
    let comm_path = format!("/proc/{pid}/comm");
    std::fs::read_to_string(&comm_path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// Fallback for other platforms.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_name(_pid: libc::pid_t) -> Option<String> {
    None
}
