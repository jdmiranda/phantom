//! Video decoder: spawns ffmpeg to decode video into raw RGBA frames
//! on a background thread. Frames are pushed into a ring buffer for
//! the render loop to consume.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use log::{info, warn};

/// Open a native macOS file picker for video files. Blocks until user selects or cancels.
/// Returns the selected path as a String, or None if cancelled.
pub(crate) fn pick_video_file() -> Option<String> {
    let output = Command::new("osascript")
        .args([
            "-e",
            r#"POSIX path of (choose file of type {"public.movie", "public.mpeg-4", "com.apple.quicktime-movie", "public.avi"} with prompt "Select a video to play")"#,
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None; // user cancelled
    }

    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() { None } else { Some(path) }
}

/// Find ffmpeg/ffprobe binary. Checks PATH first, then common brew locations.
fn find_binary(name: &str) -> String {
    // Check if it's on PATH.
    if let Ok(output) = Command::new("which").arg(name).output() {
        if output.status.success() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() {
                return path;
            }
        }
    }
    // Common homebrew paths.
    for dir in &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
        let full = format!("{dir}/{name}");
        if std::path::Path::new(&full).exists() {
            return full;
        }
    }
    // Fallback: hope it's on PATH.
    name.to_string()
}

/// A decoded RGBA frame ready for GPU upload.
pub(crate) struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Video playback state.
pub(crate) struct VideoPlayback {
    /// Shared frame buffer (latest decoded frame).
    latest_frame: Arc<Mutex<Option<VideoFrame>>>,
    /// Signal the decoder thread to stop.
    alive: Arc<AtomicBool>,
    /// Decoder thread handle.
    thread: Option<std::thread::JoinHandle<()>>,
    /// Audio playback process (separate ffmpeg piping to CoreAudio).
    audio_process: Option<std::process::Child>,
    /// Video dimensions.
    pub width: u32,
    pub height: u32,
    /// Target FPS.
    pub fps: f32,
    /// Whether playback has finished.
    pub finished: bool,
    /// Frame timer for pacing.
    last_frame_time: std::time::Instant,
}

impl VideoPlayback {
    /// Start decoding a video file. Returns None if ffmpeg isn't available
    /// or the file doesn't exist.
    pub fn start(path: &Path, max_width: u32, max_height: u32) -> Option<Self> {
        if !path.exists() {
            warn!("Video file not found: {}", path.display());
            return None;
        }

        // Probe video dimensions with ffprobe.
        let (orig_w, orig_h, fps) = probe_video(path)?;

        // Scale to fit within max dimensions while preserving aspect ratio.
        // Allow upscaling so small videos fill the screen.
        let scale = (max_width as f32 / orig_w as f32).min(max_height as f32 / orig_h as f32);
        let width = ((orig_w as f32 * scale) as u32) & !1; // must be even for ffmpeg
        let height = ((orig_h as f32 * scale) as u32) & !1;

        info!(
            "Video: {}x{} @ {fps}fps → scaled to {width}x{height}",
            orig_w, orig_h
        );

        let frame_buf = Arc::new(Mutex::new(None::<VideoFrame>));
        let frame_buf_clone = Arc::clone(&frame_buf);
        let alive = Arc::new(AtomicBool::new(true));
        let alive_clone = Arc::clone(&alive);

        // Spawn audio playback — separate ffmpeg piping to macOS CoreAudio.
        let audio_process = Command::new(find_binary("ffmpeg"))
            .args([
                "-i",
                &path.to_string_lossy(),
                "-vn", // no video
                "-f",
                "audiotoolbox", // macOS CoreAudio output
                "-v",
                "error",
                "-",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok();

        let path_owned = path.to_path_buf();
        let thread = std::thread::Builder::new()
            .name("video-decoder".into())
            .spawn(move || {
                decode_loop(path_owned, width, height, fps, frame_buf_clone, alive_clone);
            })
            .ok()?;

        Some(Self {
            latest_frame: frame_buf,
            alive,
            thread: Some(thread),
            audio_process,
            width,
            height,
            fps,
            finished: false,
            last_frame_time: std::time::Instant::now(),
        })
    }

    /// Take the latest decoded frame if available. Returns None if no new
    /// frame is ready or playback hasn't started.
    pub fn take_frame(&mut self) -> Option<VideoFrame> {
        // Pace frame delivery to target FPS.
        let interval = std::time::Duration::from_secs_f32(1.0 / self.fps);
        if self.last_frame_time.elapsed() < interval {
            return None;
        }

        let mut lock = self.latest_frame.lock().ok()?;
        let frame = lock.take()?;
        self.last_frame_time = std::time::Instant::now();
        Some(frame)
    }

    /// Check if the decoder thread has finished.
    pub fn poll_finished(&mut self) {
        if let Some(ref thread) = self.thread {
            if thread.is_finished() {
                self.finished = true;
            }
        }
    }

    /// Stop playback.
    pub fn stop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        if let Some(ref mut child) = self.audio_process {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.audio_process = None;
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for VideoPlayback {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Probe video dimensions and FPS using ffprobe.
fn probe_video(path: &Path) -> Option<(u32, u32, f32)> {
    let output = Command::new(find_binary("ffprobe"))
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,r_frame_rate",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .ok()?;

    if !output.status.success() {
        warn!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }

    let text = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<&str> = text.trim().split(',').collect();
    if parts.len() < 3 {
        warn!("ffprobe unexpected output: {text}");
        return None;
    }

    let w: u32 = parts[0].parse().ok()?;
    let h: u32 = parts[1].parse().ok()?;

    // r_frame_rate is like "30/1" or "24000/1001".
    let fps = if let Some((num, den)) = parts[2].split_once('/') {
        let n: f32 = num.parse().ok()?;
        let d: f32 = den.parse().ok()?;
        if d > 0.0 { n / d } else { 30.0 }
    } else {
        parts[2].parse().unwrap_or(30.0)
    };

    Some((w, h, fps))
}

/// Background decoder loop: runs ffmpeg and reads raw RGBA frames.
/// Paces delivery at the video's native FPS so frames arrive at the right rate.
fn decode_loop(
    path: std::path::PathBuf,
    width: u32,
    height: u32,
    fps: f32,
    frame_buf: Arc<Mutex<Option<VideoFrame>>>,
    alive: Arc<AtomicBool>,
) {
    let mut child = match Command::new(find_binary("ffmpeg"))
        .args([
            "-i",
            &path.to_string_lossy(),
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "-s",
            &format!("{width}x{height}"),
            "-v",
            "error",
            "-",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to spawn ffmpeg: {e}");
            return;
        }
    };

    let frame_size = (width * height * 4) as usize;
    let mut buf = vec![0u8; frame_size];
    let mut stdout = match child.stdout.take() {
        Some(s) => s,
        None => return,
    };

    info!(
        "Video decoder started: {}x{}, frame_size={frame_size}, fps={fps}",
        width, height
    );

    let frame_interval = std::time::Duration::from_secs_f64(1.0 / fps as f64);
    let start = std::time::Instant::now();
    let mut frame_num: u64 = 0;

    while alive.load(Ordering::Relaxed) {
        // Read exactly one frame.
        match stdout.read_exact(&mut buf) {
            Ok(()) => {}
            Err(_) => {
                info!("Video decode complete ({frame_num} frames)");
                break;
            }
        }

        let frame = VideoFrame {
            width,
            height,
            data: buf.clone(),
        };

        // Overwrite the latest frame.
        if let Ok(mut lock) = frame_buf.lock() {
            *lock = Some(frame);
        }

        frame_num += 1;

        // Pace delivery: sleep until it's time for the next frame.
        let target_time = start + frame_interval * frame_num as u32;
        let now = std::time::Instant::now();
        if target_time > now {
            std::thread::sleep(target_time - now);
        }
    }

    let _ = child.kill();
    let _ = child.wait();
}
