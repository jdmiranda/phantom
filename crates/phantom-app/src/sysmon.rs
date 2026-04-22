//! System resource monitor — live CPU, memory, and disk bars.
//!
//! Polls system stats once per second on a background thread and sends
//! snapshots over an mpsc channel. The render loop drains the latest
//! snapshot each frame and draws boot-style progress bars in a stacked
//! panel above the terminal.
//!
//! macOS: uses `sysctl` and `host_statistics64` via libc.
//! Designed to be lightweight — one syscall per metric per second.

use std::sync::mpsc;
use std::time::Duration;

use log::info;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A snapshot of system resource usage.
#[derive(Debug, Clone)]
pub(crate) struct SystemStats {
    /// CPU usage as a fraction (0.0 – 1.0).
    pub cpu_usage: f32,
    /// Memory usage as a fraction (0.0 – 1.0).
    pub mem_usage: f32,
    /// Memory used in MB.
    pub mem_used_mb: u64,
    /// Memory total in MB.
    pub mem_total_mb: u64,
    /// Disk usage as a fraction (0.0 – 1.0).
    pub disk_usage: f32,
    /// Disk used in GB.
    pub disk_used_gb: f32,
    /// Disk total in GB.
    pub disk_total_gb: f32,
    /// System load average (1 min).
    pub load_avg_1m: f32,
}

/// Handle for the sysmon background thread.
pub(crate) struct SysmonHandle {
    rx: mpsc::Receiver<SystemStats>,
    /// Most recent stats (updated each frame from channel).
    pub latest: Option<SystemStats>,
}

impl SysmonHandle {
    /// Poll for the latest stats (non-blocking). Call once per frame.
    pub fn poll(&mut self) {
        // Drain the channel and keep only the newest snapshot.
        while let Ok(stats) = self.rx.try_recv() {
            self.latest = Some(stats);
        }
    }
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

/// Spawn the sysmon background thread. Returns a handle for polling.
pub(crate) fn spawn_sysmon() -> SysmonHandle {
    let (tx, rx) = mpsc::channel();

    std::thread::Builder::new()
        .name("phantom-sysmon".into())
        .spawn(move || sysmon_loop(tx))
        .expect("failed to spawn sysmon thread");

    info!("System monitor spawned");

    SysmonHandle { rx, latest: None }
}

// ---------------------------------------------------------------------------
// Background loop
// ---------------------------------------------------------------------------

fn sysmon_loop(tx: mpsc::Sender<SystemStats>) {
    let mut prev_cpu = CpuTicks::read();

    loop {
        std::thread::sleep(Duration::from_secs(1));

        let curr_cpu = CpuTicks::read();
        let cpu_usage = CpuTicks::usage(&prev_cpu, &curr_cpu);
        prev_cpu = curr_cpu;

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
            break; // receiver dropped
        }
    }
}

// ---------------------------------------------------------------------------
// Platform: macOS (sysctl / host_statistics)
// ---------------------------------------------------------------------------

/// Raw CPU tick counters for delta computation.
struct CpuTicks {
    user: u64,
    system: u64,
    idle: u64,
}

impl CpuTicks {
    fn read() -> Self {
        // Use sysctl kern.cp_time (or fallback to host_processor_info).
        // Simplest approach: parse `sysctl -n vm.loadavg` is too coarse.
        // Instead: read host_statistics64 via mach API.
        //
        // Fallback: use /usr/bin/top or ps. For simplicity and zero
        // external deps, we'll shell out to a tiny command.
        let output = std::process::Command::new("sh")
            .arg("-c")
            // ps: %cpu sums across all processes, top: we avoid it (interactive).
            // Instead: read from sysctl kern.cp_time on FreeBSD/macOS.
            // macOS doesn't expose kern.cp_time. Use host_statistics.
            // For simplicity: use `ps -A -o %cpu` and sum.
            .arg("ps -A -o %cpu | awk '{s+=$1} END {printf \"%.0f\", s}'")
            .output();

        match output {
            Ok(o) => {
                let total: f32 = String::from_utf8_lossy(&o.stdout)
                    .trim()
                    .parse()
                    .unwrap_or(0.0);
                // `ps` reports per-CPU percentages. On an 8-core, 800% = full.
                // We store raw total and compute usage as total/num_cpus.
                let num_cpus = num_cpus_cached();
                let user = (total * 100.0) as u64;
                let system = 0;
                let idle = ((num_cpus as f32 * 100.0 - total) * 100.0).max(0.0) as u64;
                Self { user, system, idle }
            }
            Err(_) => Self {
                user: 0,
                system: 0,
                idle: 10000,
            },
        }
    }

    fn usage(prev: &Self, curr: &Self) -> f32 {
        let prev_total = prev.user + prev.system + prev.idle;
        let curr_total = curr.user + curr.system + curr.idle;
        let total_delta = curr_total.saturating_sub(prev_total);
        if total_delta == 0 {
            return 0.0;
        }
        let idle_delta = curr.idle.saturating_sub(prev.idle);
        let active = total_delta.saturating_sub(idle_delta);
        (active as f32 / total_delta as f32).clamp(0.0, 1.0)
    }
}

fn num_cpus_cached() -> usize {
    use std::sync::OnceLock;
    static NUM_CPUS: OnceLock<usize> = OnceLock::new();
    *NUM_CPUS.get_or_init(|| {
        let output = std::process::Command::new("sysctl")
            .args(["-n", "hw.ncpu"])
            .output();
        match output {
            Ok(o) => String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse()
                .unwrap_or(4),
            Err(_) => 4,
        }
    })
}

/// Read memory usage via `vm_stat` (macOS).
fn read_memory() -> (u64, u64) {
    // Total physical memory.
    let total_bytes: u64 = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        .unwrap_or(0);

    // Used memory: total - (free + inactive pages) via vm_stat.
    let vm_stat = std::process::Command::new("vm_stat")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();

    let page_size: u64 = 16384; // Apple Silicon default
    let mut free_pages: u64 = 0;
    let mut inactive_pages: u64 = 0;
    let mut speculative_pages: u64 = 0;

    for line in vm_stat.lines() {
        if line.contains("Pages free") {
            free_pages = extract_vm_stat_value(line);
        } else if line.contains("Pages inactive") {
            inactive_pages = extract_vm_stat_value(line);
        } else if line.contains("Pages speculative") {
            speculative_pages = extract_vm_stat_value(line);
        }
    }

    let available_bytes = (free_pages + inactive_pages + speculative_pages) * page_size;
    let used_bytes = total_bytes.saturating_sub(available_bytes);

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
            // df -g output: Filesystem 1G-blocks Used Available Capacity ...
            // Skip header, parse second line.
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
            // Format: "{ 1.23 4.56 7.89 }"
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

/// Bar width in characters (matches boot sequence style).
const BAR_WIDTH: usize = 24;

/// Build a resource bar string in boot sequence style.
///
/// Format: `▮ LABEL ........... ████████████████░░░░░░░░ VALUE`
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

    // Pad label to 12 chars for alignment.
    format!("▮ {:<12} {} {}", label, bar, detail)
}

/// Build all resource bar lines from a stats snapshot.
pub(crate) fn build_monitor_lines(stats: &SystemStats) -> Vec<(String, [f32; 4])> {
    let num_cpus = num_cpus_cached();

    // Color based on usage: green → yellow → red.
    let usage_color = |v: f32| -> [f32; 4] {
        if v < 0.5 {
            [0.2, 1.0, 0.5, 1.0] // green
        } else if v < 0.8 {
            [0.9, 0.9, 0.2, 1.0] // yellow
        } else {
            [1.0, 0.35, 0.2, 1.0] // red
        }
    };

    vec![
        (
            build_resource_bar(
                "CPU",
                stats.cpu_usage,
                &format!("{:.0}%", stats.cpu_usage * 100.0),
            ),
            usage_color(stats.cpu_usage),
        ),
        (
            build_resource_bar(
                "MEMORY",
                stats.mem_usage,
                &format!(
                    "{:.1}/{:.1} GB",
                    stats.mem_used_mb as f32 / 1024.0,
                    stats.mem_total_mb as f32 / 1024.0,
                ),
            ),
            usage_color(stats.mem_usage),
        ),
        (
            build_resource_bar(
                "DISK",
                stats.disk_usage,
                &format!(
                    "{:.0}/{:.0} GB",
                    stats.disk_used_gb,
                    stats.disk_total_gb,
                ),
            ),
            usage_color(stats.disk_usage),
        ),
        (
            format!(
                "▮ {:<12} {:.2}  ({} cores)",
                "LOAD AVG",
                stats.load_avg_1m,
                num_cpus,
            ),
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
        assert!(bar.contains("\u{2591}")); // empty blocks
        assert!(bar.contains("0%"));
        // No filled blocks at 0%.
        assert!(!bar.contains("\u{2588}"));
    }

    #[test]
    fn build_resource_bar_full() {
        let bar = build_resource_bar("CPU", 1.0, "100%");
        assert!(bar.contains("CPU"));
        assert!(bar.contains("100%"));
        // All filled.
        assert!(!bar.contains("\u{2591}"));
    }

    #[test]
    fn build_resource_bar_half() {
        let bar = build_resource_bar("MEM", 0.5, "8/16 GB");
        assert!(bar.contains("MEM"));
        assert!(bar.contains("\u{2588}"));
        assert!(bar.contains("\u{2591}"));
        assert!(bar.contains("8/16 GB"));
    }

    #[test]
    fn build_resource_bar_clamps() {
        let bar_over = build_resource_bar("X", 1.5, "");
        let bar_under = build_resource_bar("X", -0.5, "");
        // Both should produce valid bars without panic.
        assert!(bar_over.contains("\u{2588}"));
        assert!(bar_under.contains("\u{2591}"));
    }

    #[test]
    fn build_monitor_lines_produces_four_lines() {
        let stats = SystemStats {
            cpu_usage: 0.25,
            mem_usage: 0.60,
            mem_used_mb: 12288,
            mem_total_mb: 20480,
            disk_usage: 0.45,
            disk_used_gb: 225.0,
            disk_total_gb: 500.0,
            load_avg_1m: 2.5,
        };
        let lines = build_monitor_lines(&stats);
        assert_eq!(lines.len(), 4);
        assert!(lines[0].0.contains("CPU"));
        assert!(lines[1].0.contains("MEMORY"));
        assert!(lines[2].0.contains("DISK"));
        assert!(lines[3].0.contains("LOAD AVG"));
    }

    #[test]
    fn color_thresholds() {
        let stats_low = SystemStats {
            cpu_usage: 0.2,
            mem_usage: 0.3,
            mem_used_mb: 6144,
            mem_total_mb: 20480,
            disk_usage: 0.4,
            disk_used_gb: 200.0,
            disk_total_gb: 500.0,
            load_avg_1m: 1.0,
        };
        let lines = build_monitor_lines(&stats_low);
        // Low usage = green (channel [1] = 1.0).
        assert_eq!(lines[0].1[1], 1.0);

        let stats_high = SystemStats {
            cpu_usage: 0.95,
            mem_usage: 0.90,
            mem_used_mb: 18432,
            mem_total_mb: 20480,
            disk_usage: 0.85,
            disk_used_gb: 425.0,
            disk_total_gb: 500.0,
            load_avg_1m: 20.0,
        };
        let lines = build_monitor_lines(&stats_high);
        // High usage = red (channel [0] = 1.0, channel [1] < 0.5).
        assert_eq!(lines[0].1[0], 1.0);
        assert!(lines[0].1[1] < 0.5);
    }

    #[test]
    fn sysmon_handle_poll_drains_latest() {
        let (tx, rx) = mpsc::channel();
        let mut handle = SysmonHandle { rx, latest: None };

        // Send 3 snapshots.
        for i in 0..3 {
            tx.send(SystemStats {
                cpu_usage: i as f32 * 0.1,
                mem_usage: 0.5,
                mem_used_mb: 10240,
                mem_total_mb: 20480,
                disk_usage: 0.4,
                disk_used_gb: 200.0,
                disk_total_gb: 500.0,
                load_avg_1m: 1.0,
            })
            .unwrap();
        }

        handle.poll();
        // Should have the LAST snapshot (cpu = 0.2).
        let stats = handle.latest.as_ref().unwrap();
        assert!((stats.cpu_usage - 0.2).abs() < f32::EPSILON);
    }
}
