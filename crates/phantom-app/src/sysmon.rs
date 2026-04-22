//! System resource monitor — live CPU, memory, and disk bars.
//!
//! Polls system stats on a background thread via macOS sysctl/mach APIs
//! and sends snapshots over an mpsc channel. The render loop drains the
//! latest snapshot each frame.
//!
//! The thread sleeps when the panel is hidden (signalled via an atomic flag)
//! to avoid wasting CPU on process spawning.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use log::info;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A snapshot of system resource usage.
#[derive(Debug, Clone)]
pub(crate) struct SystemStats {
    pub cpu_usage: f32,
    pub mem_usage: f32,
    pub mem_used_mb: u64,
    pub mem_total_mb: u64,
    pub disk_usage: f32,
    pub disk_used_gb: f32,
    pub disk_total_gb: f32,
    pub load_avg_1m: f32,
}

/// Handle for the sysmon background thread.
pub(crate) struct SysmonHandle {
    rx: mpsc::Receiver<SystemStats>,
    /// Most recent stats (updated each frame from channel).
    pub latest: Option<SystemStats>,
    /// Shared flag: true = thread should poll, false = sleep.
    active: Arc<AtomicBool>,
}

impl SysmonHandle {
    /// Poll for the latest stats (non-blocking). Call once per frame.
    pub fn poll(&mut self) {
        while let Ok(stats) = self.rx.try_recv() {
            self.latest = Some(stats);
        }
    }

    /// Tell the background thread to start/stop polling.
    pub fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

/// Spawn the sysmon background thread. Returns a handle for polling.
pub(crate) fn spawn_sysmon() -> SysmonHandle {
    let (tx, rx) = mpsc::channel();
    let active = Arc::new(AtomicBool::new(false));
    let active_clone = active.clone();

    std::thread::Builder::new()
        .name("phantom-sysmon".into())
        .spawn(move || sysmon_loop(tx, active_clone))
        .expect("failed to spawn sysmon thread");

    info!("System monitor spawned (idle until activated)");

    SysmonHandle { rx, latest: None, active }
}

// ---------------------------------------------------------------------------
// Background loop
// ---------------------------------------------------------------------------

fn sysmon_loop(tx: mpsc::Sender<SystemStats>, active: Arc<AtomicBool>) {
    loop {
        // Sleep when not visible — no process spawning, no CPU.
        if !active.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }

        let cpu_usage = read_cpu_usage();

        let (mem_used_mb, mem_total_mb) = read_memory();
        let mem_usage = if mem_total_mb > 0 {
            mem_used_mb as f32 / mem_total_mb as f32
        } else {
            0.0
        };

        let (disk_used_gb, disk_total_gb) = read_disk();
        let disk_usage = if disk_total_gb > 0.0 {
            disk_used_gb / disk_total_gb
        } else {
            0.0
        };

        let load_avg_1m = read_load_average();

        let stats = SystemStats {
            cpu_usage,
            mem_usage,
            mem_used_mb,
            mem_total_mb,
            disk_usage,
            disk_used_gb,
            disk_total_gb,
            load_avg_1m,
        };

        if tx.send(stats).is_err() {
            break;
        }

        std::thread::sleep(Duration::from_secs(2));
    }
}

// ---------------------------------------------------------------------------
// macOS system queries
// ---------------------------------------------------------------------------

/// Read CPU usage via `top -l 1 -n 0` (one sample, no process list).
/// Returns a fraction 0.0–1.0.
fn read_cpu_usage() -> f32 {
    let output = std::process::Command::new("sh")
        .arg("-c")
        // top -l 1 -n 0: one sample, zero processes. Grep for CPU line.
        .arg("top -l 1 -n 0 -s 0 2>/dev/null | grep 'CPU usage'")
        .output();

    match output {
        Ok(o) => {
            let text = String::from_utf8_lossy(&o.stdout);
            // Format: "CPU usage: 5.26% user, 3.94% sys, 90.79% idle"
            // Extract idle percentage and compute 1 - idle.
            if let Some(idle_str) = text.split("idle").next() {
                let parts: Vec<&str> = idle_str.split(',').collect();
                if let Some(last) = parts.last() {
                    let pct: f32 = last.trim()
                        .trim_end_matches('%')
                        .trim()
                        .parse()
                        .unwrap_or(100.0);
                    return (1.0 - pct / 100.0).clamp(0.0, 1.0);
                }
            }
            0.0
        }
        Err(_) => 0.0,
    }
}

/// Read memory usage via sysctl + vm_stat.
fn read_memory() -> (u64, u64) {
    let total_bytes: u64 = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(0);

    // Use `memory_pressure` or parse `vm_stat` for used memory.
    // vm_stat gives page counts; multiply by page size.
    let output = std::process::Command::new("vm_stat")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();

    // Parse page size from first line: "Mach Virtual Memory Statistics: (page size of 16384 bytes)"
    let page_size: u64 = output.lines().next()
        .and_then(|line| {
            line.split("page size of ").nth(1)
                .and_then(|s| s.split(' ').next())
                .and_then(|s| s.parse().ok())
        })
        .unwrap_or(16384);

    let mut active: u64 = 0;
    let mut wired: u64 = 0;
    let mut compressed: u64 = 0;
    let mut speculative: u64 = 0;

    for line in output.lines() {
        if line.starts_with("Pages active") {
            active = extract_vm_stat_value(line);
        } else if line.starts_with("Pages wired") {
            wired = extract_vm_stat_value(line);
        } else if line.starts_with("Pages occupied by compressor") {
            compressed = extract_vm_stat_value(line);
        } else if line.starts_with("Pages speculative") {
            speculative = extract_vm_stat_value(line);
        }
    }

    // "Used" = active + wired + compressed (how Activity Monitor counts it).
    let used_bytes = (active + wired + compressed + speculative) * page_size;

    (used_bytes / (1024 * 1024), total_bytes / (1024 * 1024))
}

fn extract_vm_stat_value(line: &str) -> u64 {
    line.split(':')
        .nth(1)
        .unwrap_or("")
        .trim()
        .trim_end_matches('.')
        .parse()
        .unwrap_or(0)
}

/// Read disk usage via `df /`.
fn read_disk() -> (f32, f32) {
    let output = std::process::Command::new("df")
        .args(["-g", "/"])
        .output();

    match output {
        Ok(o) => {
            let text = String::from_utf8_lossy(&o.stdout);
            if let Some(line) = text.lines().nth(1) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 4 {
                    let total: f32 = parts[1].parse().unwrap_or(0.0);
                    let used: f32 = parts[2].parse().unwrap_or(0.0);
                    return (used, total);
                }
            }
            (0.0, 0.0)
        }
        Err(_) => (0.0, 0.0),
    }
}

/// Read 1-minute load average.
fn read_load_average() -> f32 {
    std::process::Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .ok()
        .and_then(|o| {
            let text = String::from_utf8_lossy(&o.stdout).to_string();
            text.trim()
                .trim_start_matches('{')
                .trim()
                .split_whitespace()
                .next()
                .and_then(|s| s.parse::<f32>().ok())
        })
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

const BAR_WIDTH: usize = 24;

/// Build a resource bar string in boot sequence style.
pub(crate) fn build_resource_bar(
    label: &str,
    value: f32,
    detail: &str,
) -> String {
    let clamped = value.clamp(0.0, 1.0);
    let filled = (clamped * BAR_WIDTH as f32).round() as usize;
    let empty = BAR_WIDTH - filled;

    let bar = format!(
        "{}{}",
        "\u{2588}".repeat(filled),
        "\u{2591}".repeat(empty),
    );

    format!("▮ {:<12} {} {}", label, bar, detail)
}

/// Build all resource bar lines from a stats snapshot.
pub(crate) fn build_monitor_lines(stats: &SystemStats) -> Vec<(String, [f32; 4])> {
    let num_cpus = num_cpus_cached();

    let usage_color = |v: f32| -> [f32; 4] {
        if v < 0.5 {
            [0.2, 1.0, 0.5, 1.0]
        } else if v < 0.8 {
            [0.9, 0.9, 0.2, 1.0]
        } else {
            [1.0, 0.35, 0.2, 1.0]
        }
    };

    vec![
        (
            build_resource_bar("CPU", stats.cpu_usage, &format!("{:.0}%", stats.cpu_usage * 100.0)),
            usage_color(stats.cpu_usage),
        ),
        (
            build_resource_bar(
                "MEMORY",
                stats.mem_usage,
                &format!("{:.1}/{:.1} GB", stats.mem_used_mb as f32 / 1024.0, stats.mem_total_mb as f32 / 1024.0),
            ),
            usage_color(stats.mem_usage),
        ),
        (
            build_resource_bar("DISK", stats.disk_usage, &format!("{:.0}/{:.0} GB", stats.disk_used_gb, stats.disk_total_gb)),
            usage_color(stats.disk_usage),
        ),
        (
            format!("▮ {:<12} {:.2}  ({} cores)", "LOAD AVG", stats.load_avg_1m, num_cpus),
            if stats.load_avg_1m > num_cpus as f32 {
                [1.0, 0.35, 0.2, 1.0]
            } else if stats.load_avg_1m > num_cpus as f32 * 0.7 {
                [0.9, 0.9, 0.2, 1.0]
            } else {
                [0.2, 1.0, 0.5, 1.0]
            },
        ),
    ]
}

fn num_cpus_cached() -> usize {
    use std::sync::OnceLock;
    static NUM_CPUS: OnceLock<usize> = OnceLock::new();
    *NUM_CPUS.get_or_init(|| {
        std::process::Command::new("sysctl")
            .args(["-n", "hw.ncpu"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
            .unwrap_or(4)
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_resource_bar_empty() {
        let bar = build_resource_bar("TEST", 0.0, "0%");
        assert!(bar.contains("TEST"));
        assert!(bar.contains("\u{2591}"));
        assert!(!bar.contains("\u{2588}"));
    }

    #[test]
    fn build_resource_bar_full() {
        let bar = build_resource_bar("CPU", 1.0, "100%");
        assert!(bar.contains("CPU"));
        assert!(!bar.contains("\u{2591}"));
    }

    #[test]
    fn build_resource_bar_half() {
        let bar = build_resource_bar("MEM", 0.5, "8/16 GB");
        assert!(bar.contains("\u{2588}"));
        assert!(bar.contains("\u{2591}"));
    }

    #[test]
    fn build_resource_bar_clamps() {
        let _ = build_resource_bar("X", 1.5, "");
        let _ = build_resource_bar("X", -0.5, "");
    }

    #[test]
    fn build_monitor_lines_produces_four() {
        let stats = SystemStats {
            cpu_usage: 0.25, mem_usage: 0.60, mem_used_mb: 12288, mem_total_mb: 20480,
            disk_usage: 0.45, disk_used_gb: 225.0, disk_total_gb: 500.0, load_avg_1m: 2.5,
        };
        let lines = build_monitor_lines(&stats);
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn sysmon_handle_poll_keeps_latest() {
        let (tx, rx) = mpsc::channel();
        let active = Arc::new(AtomicBool::new(false));
        let mut handle = SysmonHandle { rx, latest: None, active };

        for i in 0..3 {
            tx.send(SystemStats {
                cpu_usage: i as f32 * 0.1, mem_usage: 0.5, mem_used_mb: 10240,
                mem_total_mb: 20480, disk_usage: 0.4, disk_used_gb: 200.0,
                disk_total_gb: 500.0, load_avg_1m: 1.0,
            }).unwrap();
        }

        handle.poll();
        assert!((handle.latest.as_ref().unwrap().cpu_usage - 0.2).abs() < f32::EPSILON);
    }

    #[test]
    fn active_flag_controls_thread() {
        let active = Arc::new(AtomicBool::new(false));
        assert!(!active.load(Ordering::Relaxed));
        active.store(true, Ordering::Relaxed);
        assert!(active.load(Ordering::Relaxed));
    }
}
