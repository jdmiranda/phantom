//! System resource monitor — live hardware metrics.
//!
//! Polls CPU, memory, disk, network, battery, disk I/O, GPU, and thermals
//! on a background thread via macOS commands. Sends snapshots over mpsc.
//! Thread sleeps when the panel is hidden.

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
    // -- CPU / Load --
    pub cpu_usage: f32,
    pub load_avg_1m: f32,

    // -- Memory --
    pub mem_usage: f32,
    pub mem_used_mb: u64,
    pub mem_total_mb: u64,

    // -- Disk space --
    pub disk_usage: f32,
    pub disk_used_gb: f32,
    pub disk_total_gb: f32,

    // -- Disk I/O (KB/s) --
    pub disk_read_kbs: f32,
    pub disk_write_kbs: f32,

    // -- Network throughput (KB/s) --
    pub net_rx_kbs: f32,
    pub net_tx_kbs: f32,

    // -- Battery --
    pub battery_pct: Option<f32>,
    pub battery_charging: bool,
    pub battery_time_remaining: Option<String>,

    // -- Thermal --
    pub cpu_temp_c: Option<f32>,
    pub gpu_temp_c: Option<f32>,

    // -- GPU --
    pub gpu_usage: Option<f32>,

    // -- Network connections --
    pub net_connections: u32,
}

/// Handle for the sysmon background thread.
pub(crate) struct SysmonHandle {
    rx: mpsc::Receiver<SystemStats>,
    pub latest: Option<SystemStats>,
    active: Arc<AtomicBool>,
}

impl SysmonHandle {
    pub fn poll(&mut self) {
        while let Ok(stats) = self.rx.try_recv() {
            self.latest = Some(stats);
        }
    }

    pub fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Relaxed);
    }

    /// Test-only constructor for creating a handle with an injected channel.
    #[cfg(test)]
    pub(crate) fn for_test(rx: mpsc::Receiver<SystemStats>) -> Self {
        Self {
            rx,
            latest: None,
            active: Arc::new(AtomicBool::new(false)),
        }
    }
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

pub(crate) fn spawn_sysmon() -> SysmonHandle {
    let (tx, rx) = mpsc::channel();
    let active = Arc::new(AtomicBool::new(false));
    let active_clone = active.clone();

    match std::thread::Builder::new()
        .name("phantom-sysmon".into())
        .spawn(move || sysmon_loop(tx, active_clone))
    {
        Ok(_) => info!("System monitor spawned (idle until activated)"),
        Err(e) => log::warn!("Failed to spawn sysmon thread: {e} — monitor disabled"),
    }

    SysmonHandle { rx, latest: None, active }
}

// ---------------------------------------------------------------------------
// Background loop
// ---------------------------------------------------------------------------

fn sysmon_loop(tx: mpsc::Sender<SystemStats>, active: Arc<AtomicBool>) {
    let mut prev_net = NetCounters::read();
    let mut prev_disk_io = DiskIoCounters::read();

    loop {
        if !active.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }

        // Fast metrics first (< 50ms each) so the panel shows data immediately.
        let (mem_used_mb, mem_total_mb) = read_memory();
        let mem_usage = if mem_total_mb > 0 { mem_used_mb as f32 / mem_total_mb as f32 } else { 0.0 };
        let (disk_used_gb, disk_total_gb) = read_disk_space();
        let disk_usage = if disk_total_gb > 0.0 { disk_used_gb / disk_total_gb } else { 0.0 };
        let load_avg_1m = read_load_average();
        let (battery_pct, battery_charging, battery_time_remaining) = read_battery();
        let net_connections = read_net_connections();

        // Disk I/O delta.
        let curr_disk_io = DiskIoCounters::read();
        let (disk_read_kbs, disk_write_kbs) = DiskIoCounters::throughput(&prev_disk_io, &curr_disk_io, 2.0);
        prev_disk_io = curr_disk_io;

        // Network delta.
        let curr_net = NetCounters::read();
        let (net_rx_kbs, net_tx_kbs) = NetCounters::throughput(&prev_net, &curr_net, 2.0);
        prev_net = curr_net;

        // Send fast metrics immediately so the panel populates without waiting for CPU.
        let fast_stats = SystemStats {
            cpu_usage: 0.0, // placeholder — updated after slow poll
            load_avg_1m,
            mem_usage, mem_used_mb, mem_total_mb,
            disk_usage, disk_used_gb, disk_total_gb,
            disk_read_kbs, disk_write_kbs,
            net_rx_kbs, net_tx_kbs,
            battery_pct, battery_charging, battery_time_remaining: battery_time_remaining.clone(),
            cpu_temp_c: None, gpu_temp_c: None, gpu_usage: None,
            net_connections,
        };
        let _ = tx.send(fast_stats);

        // Slow metrics (top takes 1-2s, powermetrics needs sudo).
        let cpu_usage = read_cpu_usage();
        let (cpu_temp_c, gpu_temp_c, gpu_usage) = read_powermetrics();

        let stats = SystemStats {
            cpu_usage, load_avg_1m,
            mem_usage, mem_used_mb, mem_total_mb,
            disk_usage, disk_used_gb, disk_total_gb,
            disk_read_kbs, disk_write_kbs,
            net_rx_kbs, net_tx_kbs,
            battery_pct, battery_charging, battery_time_remaining,
            cpu_temp_c, gpu_temp_c, gpu_usage,
            net_connections,
        };

        if tx.send(stats).is_err() { break; }
        std::thread::sleep(Duration::from_secs(2));
    }
}

// ---------------------------------------------------------------------------
// CPU
// ---------------------------------------------------------------------------

/// Run a shell command with a 3-second timeout. Returns stdout or empty string.
///
/// Spawns the command directly and reads stdout with a deadline, killing the
/// child if it exceeds 3 seconds. Does not rely on GNU `timeout` (unavailable
/// on macOS by default).
fn shell_with_timeout(cmd: &str) -> String {
    let Ok(child) = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return String::new();
    };

    // Move child into a thread that waits for output; main thread enforces timeout.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = child.wait_with_output();
        let _ = tx.send(result);
    });

    match rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(output)) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).into_owned()
        }
        _ => String::new(),
    }
}

fn read_cpu_usage() -> f32 {
    let text = shell_with_timeout("top -l 1 -n 0 -s 0 | grep 'CPU usage'");
    if let Some(idle_str) = text.split("idle").next() {
        let parts: Vec<&str> = idle_str.split(',').collect();
        if let Some(last) = parts.last() {
            let pct: f32 = last.trim().trim_end_matches('%').trim().parse().unwrap_or(100.0);
            return (1.0 - pct / 100.0).clamp(0.0, 1.0);
        }
    }
    0.0
}

fn read_load_average() -> f32 {
    let text = shell_with_timeout("sysctl -n vm.loadavg");
    text.trim().trim_start_matches('{').trim()
        .split_whitespace().next()
        .and_then(|s| s.parse::<f32>().ok())
        .unwrap_or(0.0)
}

fn num_cpus_cached() -> usize {
    use std::sync::OnceLock;
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        shell_with_timeout("sysctl -n hw.ncpu").trim().parse().unwrap_or(4)
    })
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

fn read_memory() -> (u64, u64) {
    let total_bytes: u64 = shell_with_timeout("sysctl -n hw.memsize")
        .trim().parse().unwrap_or(0);

    let output = shell_with_timeout("vm_stat");

    let page_size: u64 = output.lines().next()
        .and_then(|l| l.split("page size of ").nth(1).and_then(|s| s.split(' ').next().and_then(|s| s.parse().ok())))
        .unwrap_or(16384);

    let mut active: u64 = 0;
    let mut wired: u64 = 0;
    let mut compressed: u64 = 0;
    let mut speculative: u64 = 0;

    for line in output.lines() {
        if line.starts_with("Pages active") { active = vm_val(line); }
        else if line.starts_with("Pages wired") { wired = vm_val(line); }
        else if line.starts_with("Pages occupied by compressor") { compressed = vm_val(line); }
        else if line.starts_with("Pages speculative") { speculative = vm_val(line); }
    }

    let used = (active + wired + compressed + speculative) * page_size;
    (used / (1024 * 1024), total_bytes / (1024 * 1024))
}

fn vm_val(line: &str) -> u64 {
    line.split(':').nth(1).unwrap_or("").trim().trim_end_matches('.').parse().unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Disk space
// ---------------------------------------------------------------------------

fn read_disk_space() -> (f32, f32) {
    let text = shell_with_timeout("df -g /");
    text.lines().nth(1)
        .and_then(|l| {
            let p: Vec<&str> = l.split_whitespace().collect();
            if p.len() >= 4 {
                Some((p[2].parse().unwrap_or(0.0), p[1].parse().unwrap_or(0.0)))
            } else { None }
        })
        .unwrap_or((0.0, 0.0))
}

// ---------------------------------------------------------------------------
// Disk I/O
// ---------------------------------------------------------------------------

struct DiskIoCounters { read_kb: u64, write_kb: u64 }

impl DiskIoCounters {
    fn read() -> Self {
        let text = shell_with_timeout("iostat -I -d | tail -1");
        let parts: Vec<&str> = text.trim().split_whitespace().collect();
        if parts.len() >= 3 {
            let mb: f64 = parts[2].parse().unwrap_or(0.0);
            let kb = (mb * 1024.0) as u64;
            Self { read_kb: kb / 2, write_kb: kb / 2 }
        } else {
            Self { read_kb: 0, write_kb: 0 }
        }
    }

    fn throughput(prev: &Self, curr: &Self, interval_secs: f32) -> (f32, f32) {
        let dr = curr.read_kb.saturating_sub(prev.read_kb) as f32 / interval_secs;
        let dw = curr.write_kb.saturating_sub(prev.write_kb) as f32 / interval_secs;
        (dr, dw)
    }
}

// ---------------------------------------------------------------------------
// Network throughput
// ---------------------------------------------------------------------------

struct NetCounters { rx_bytes: u64, tx_bytes: u64 }

impl NetCounters {
    fn read() -> Self {
        let text = shell_with_timeout("netstat -ib | grep -E '^en[0-9]' | head -1");
        let parts: Vec<&str> = text.trim().split_whitespace().collect();
        if parts.len() >= 10 {
            let rx: u64 = parts[6].parse().unwrap_or(0);
            let tx: u64 = parts[9].parse().unwrap_or(0);
            Self { rx_bytes: rx, tx_bytes: tx }
        } else {
            Self { rx_bytes: 0, tx_bytes: 0 }
        }
    }

    fn throughput(prev: &Self, curr: &Self, interval_secs: f32) -> (f32, f32) {
        let rx_kb = curr.rx_bytes.saturating_sub(prev.rx_bytes) as f32 / 1024.0 / interval_secs;
        let tx_kb = curr.tx_bytes.saturating_sub(prev.tx_bytes) as f32 / 1024.0 / interval_secs;
        (rx_kb, tx_kb)
    }
}

fn read_net_connections() -> u32 {
    shell_with_timeout("netstat -an | grep -c ESTABLISHED")
        .trim().parse().unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Battery
// ---------------------------------------------------------------------------

fn read_battery() -> (Option<f32>, bool, Option<String>) {
    let text = shell_with_timeout("pmset -g batt");
    let mut pct = None;
    let mut charging = false;
    let mut remaining = None;

    for line in text.lines() {
        if line.contains('%') {
            if let Some(p) = line.split('%').next() {
                let num_str: String = p.chars().rev()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .chars().rev().collect();
                if let Ok(v) = num_str.parse::<f32>() {
                    pct = Some(v);
                }
            }
            charging = line.contains("charging") && !line.contains("discharging");
            if let Some(idx) = line.find("remaining") {
                let before = &line[..idx];
                let time_str = before.rsplit(';').next().unwrap_or("").trim();
                if !time_str.is_empty() && time_str != "(no estimate)" {
                    remaining = Some(time_str.to_string());
                }
            }
        }
    }
    (pct, charging, remaining)
}

// ---------------------------------------------------------------------------
// Thermals + GPU (powermetrics — may need sudo, graceful fallback)
// ---------------------------------------------------------------------------

fn read_powermetrics() -> (Option<f32>, Option<f32>, Option<f32>) {
    let text = shell_with_timeout("powermetrics --samplers smc,gpu_power -n 1 -i 1000");
    if text.is_empty() {
        return (None, None, None);
    }

    let mut cpu_temp = None;
    let mut gpu_temp = None;
    let mut gpu_usage = None;

    for line in text.lines() {
        if line.contains("CPU die temperature") {
            cpu_temp = extract_temp(line);
        }
        if line.contains("GPU die temperature") {
            gpu_temp = extract_temp(line);
        }
        if line.contains("GPU") && (line.contains("Active") || line.contains("active residency")) {
            if let Some(pct_str) = line.split(':').nth(1) {
                let cleaned = pct_str.trim().trim_end_matches('%').trim();
                if let Ok(v) = cleaned.parse::<f32>() {
                    gpu_usage = Some(v / 100.0);
                }
            }
        }
    }
    (cpu_temp, gpu_temp, gpu_usage)
}

fn extract_temp(line: &str) -> Option<f32> {
    line.split(':').nth(1)
        .and_then(|s| s.trim().split_whitespace().next())
        .and_then(|s| s.parse::<f32>().ok())
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

const BAR_WIDTH: usize = 20;

pub(crate) fn build_resource_bar(label: &str, value: f32, detail: &str) -> String {
    let clamped = value.clamp(0.0, 1.0);
    let filled = (clamped * BAR_WIDTH as f32).round() as usize;
    let empty = BAR_WIDTH - filled;
    format!("▮ {:<12} {}{} {}", label, "\u{2588}".repeat(filled), "\u{2591}".repeat(empty), detail)
}

fn usage_color(v: f32) -> [f32; 4] {
    if v < 0.5 { [0.2, 1.0, 0.5, 1.0] }
    else if v < 0.8 { [0.9, 0.9, 0.2, 1.0] }
    else { [1.0, 0.35, 0.2, 1.0] }
}

fn format_throughput(kbs: f32) -> String {
    if kbs >= 1024.0 { format!("{:.1} MB/s", kbs / 1024.0) }
    else { format!("{:.0} KB/s", kbs) }
}

pub(crate) fn build_monitor_lines(stats: &SystemStats) -> Vec<(String, [f32; 4])> {
    let num_cpus = num_cpus_cached();
    let cyan = [0.0, 0.8, 0.9, 1.0];
    let dim = [0.4, 0.7, 0.5, 0.7];

    let mut lines = vec![
        // CPU.
        (build_resource_bar("CPU", stats.cpu_usage, &format!("{:.0}%", stats.cpu_usage * 100.0)),
         usage_color(stats.cpu_usage)),
        // Memory.
        (build_resource_bar("MEMORY", stats.mem_usage,
            &format!("{:.1}/{:.1} GB", stats.mem_used_mb as f32 / 1024.0, stats.mem_total_mb as f32 / 1024.0)),
         usage_color(stats.mem_usage)),
        // Disk space.
        (build_resource_bar("DISK", stats.disk_usage,
            &format!("{:.0}/{:.0} GB", stats.disk_used_gb, stats.disk_total_gb)),
         usage_color(stats.disk_usage)),
        // Load average.
        (format!("▮ {:<12} {:.2}  ({} cores)", "LOAD AVG", stats.load_avg_1m, num_cpus),
         if stats.load_avg_1m > num_cpus as f32 { [1.0, 0.35, 0.2, 1.0] }
         else if stats.load_avg_1m > num_cpus as f32 * 0.7 { [0.9, 0.9, 0.2, 1.0] }
         else { [0.2, 1.0, 0.5, 1.0] }),
    ];

    // Network throughput.
    let net_bar_val = ((stats.net_rx_kbs + stats.net_tx_kbs) / 10240.0).clamp(0.0, 1.0); // 10MB/s = full
    lines.push((
        build_resource_bar("NETWORK", net_bar_val,
            &format!("↓{} ↑{}", format_throughput(stats.net_rx_kbs), format_throughput(stats.net_tx_kbs))),
        cyan,
    ));

    // Disk I/O.
    let io_bar_val = ((stats.disk_read_kbs + stats.disk_write_kbs) / 102400.0).clamp(0.0, 1.0); // 100MB/s = full
    lines.push((
        build_resource_bar("DISK I/O", io_bar_val,
            &format!("R:{} W:{}", format_throughput(stats.disk_read_kbs), format_throughput(stats.disk_write_kbs))),
        dim,
    ));

    // Network connections.
    lines.push((
        format!("▮ {:<12} {} established", "CONNECTIONS", stats.net_connections),
        dim,
    ));

    // Battery (if available).
    if let Some(pct) = stats.battery_pct {
        let status = if stats.battery_charging { "⚡" } else { "🔋" };
        let time = stats.battery_time_remaining.as_deref().unwrap_or("");
        let batt_frac = pct / 100.0;
        lines.push((
            build_resource_bar("BATTERY", batt_frac, &format!("{:.0}% {status} {time}", pct)),
            usage_color(1.0 - batt_frac), // invert: low battery = red
        ));
    }

    // GPU (if available).
    if let Some(gpu) = stats.gpu_usage {
        lines.push((
            build_resource_bar("GPU", gpu, &format!("{:.0}%", gpu * 100.0)),
            usage_color(gpu),
        ));
    }

    // Temperatures (if available).
    if stats.cpu_temp_c.is_some() || stats.gpu_temp_c.is_some() {
        let cpu_t = stats.cpu_temp_c.map(|t| format!("CPU:{:.0}°C", t)).unwrap_or_default();
        let gpu_t = stats.gpu_temp_c.map(|t| format!("GPU:{:.0}°C", t)).unwrap_or_default();
        let max_temp = stats.cpu_temp_c.unwrap_or(0.0).max(stats.gpu_temp_c.unwrap_or(0.0));
        let temp_frac = (max_temp / 100.0).clamp(0.0, 1.0); // 100°C = full
        lines.push((
            build_resource_bar("THERMAL", temp_frac, &format!("{cpu_t} {gpu_t}")),
            if max_temp > 90.0 { [1.0, 0.35, 0.2, 1.0] }
            else if max_temp > 70.0 { [0.9, 0.9, 0.2, 1.0] }
            else { [0.2, 1.0, 0.5, 1.0] },
        ));
    }

    lines
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn full_stats() -> SystemStats {
        SystemStats {
            cpu_usage: 0.25, load_avg_1m: 2.5,
            mem_usage: 0.60, mem_used_mb: 12288, mem_total_mb: 20480,
            disk_usage: 0.45, disk_used_gb: 225.0, disk_total_gb: 500.0,
            disk_read_kbs: 5120.0, disk_write_kbs: 2048.0,
            net_rx_kbs: 1500.0, net_tx_kbs: 300.0,
            battery_pct: Some(72.0), battery_charging: true,
            battery_time_remaining: Some("1:30".into()),
            cpu_temp_c: Some(55.0), gpu_temp_c: Some(48.0),
            gpu_usage: Some(0.15), net_connections: 42,
        }
    }

    #[test]
    fn build_monitor_lines_all_metrics() {
        let lines = build_monitor_lines(&full_stats());
        // CPU + Memory + Disk + Load + Network + Disk I/O + Connections + Battery + GPU + Thermal = 10
        assert_eq!(lines.len(), 10);
        assert!(lines[0].0.contains("CPU"));
        assert!(lines[4].0.contains("NETWORK"));
        assert!(lines[7].0.contains("BATTERY"));
        assert!(lines[8].0.contains("GPU"));
        assert!(lines[9].0.contains("THERMAL"));
    }

    #[test]
    fn build_monitor_lines_no_optional_metrics() {
        let mut s = full_stats();
        s.battery_pct = None;
        s.gpu_usage = None;
        s.cpu_temp_c = None;
        s.gpu_temp_c = None;
        let lines = build_monitor_lines(&s);
        // CPU + Memory + Disk + Load + Network + Disk I/O + Connections = 7
        assert_eq!(lines.len(), 7);
    }

    #[test]
    fn format_throughput_kb() {
        assert_eq!(format_throughput(500.0), "500 KB/s");
    }

    #[test]
    fn format_throughput_mb() {
        assert_eq!(format_throughput(2048.0), "2.0 MB/s");
    }

    #[test]
    fn battery_color_inverted() {
        // Low battery (20%) should be red (inverted: usage_color(0.8) = red).
        let mut s = full_stats();
        s.battery_pct = Some(20.0);
        let lines = build_monitor_lines(&s);
        let batt_line = lines.iter().find(|(t, _)| t.contains("BATTERY")).unwrap();
        assert_eq!(batt_line.1[0], 1.0); // red channel
    }

    #[test]
    fn build_resource_bar_formats() {
        let bar = build_resource_bar("TEST", 0.5, "50%");
        assert!(bar.contains("TEST"));
        assert!(bar.contains("50%"));
        assert!(bar.contains("\u{2588}"));
        assert!(bar.contains("\u{2591}"));
    }

    #[test]
    fn sysmon_handle_poll() {
        let (tx, rx) = mpsc::channel();
        let active = Arc::new(AtomicBool::new(false));
        let mut handle = SysmonHandle { rx, latest: None, active };
        tx.send(full_stats()).unwrap();
        handle.poll();
        assert!(handle.latest.is_some());
    }
}
