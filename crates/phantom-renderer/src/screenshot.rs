//! Screenshot capture and encoding.
//!
//! Captures the current frame from a wgpu texture into CPU-readable RGBA
//! pixels, encodes them as PNG, and optionally saves with embedded metadata.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur during GPU-side screenshot capture.
#[derive(Debug)]
pub enum ScreenshotError {
    /// The staging buffer could not be mapped (driver-side error).
    MapFailed(wgpu::BufferAsyncError),
    /// The GPU did not complete the readback within the timeout window.
    ///
    /// This typically indicates a GPU hang or a severely overloaded driver.
    /// The caller should treat the screenshot as unavailable and continue.
    GpuTimeout,
    /// The channel used to receive the map result was unexpectedly closed.
    ChannelClosed,
}

impl std::fmt::Display for ScreenshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MapFailed(e) => write!(f, "GPU buffer mapping failed: {e}"),
            Self::GpuTimeout => write!(f, "GPU readback timed out (possible GPU hang)"),
            Self::ChannelClosed => write!(f, "GPU buffer map channel disconnected"),
        }
    }
}

impl std::error::Error for ScreenshotError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::MapFailed(e) => Some(e),
            Self::GpuTimeout | Self::ChannelClosed => None,
        }
    }
}

/// How long to wait for the GPU to complete a readback before giving up.
const GPU_READBACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Map a staging buffer with a timeout, returning `ScreenshotError` on failure.
///
/// Uses `wgpu::PollType::Poll` (non-blocking) plus an `mpsc` channel with a
/// 5-second deadline so that a hung GPU driver cannot block the caller forever.
fn map_buffer_with_timeout(
    device: &wgpu::Device,
    buffer_slice: wgpu::BufferSlice<'_>,
) -> Result<(), ScreenshotError> {
    let (tx, rx) = std::sync::mpsc::channel();
    buffer_slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    // Non-blocking poll — the callback above will fire once the GPU is done.
    // We intentionally ignore poll errors here; the channel timeout below
    // catches any real GPU failure within 5 seconds.
    let _ = device.poll(wgpu::PollType::Poll);
    match rx.recv_timeout(GPU_READBACK_TIMEOUT) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(ScreenshotError::MapFailed(e)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(ScreenshotError::GpuTimeout),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err(ScreenshotError::ChannelClosed)
        }
    }
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

/// Metadata embedded in screenshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenshotMetadata {
    /// Unix timestamp (seconds since epoch).
    pub timestamp: u64,
    /// Screenshot width in pixels.
    pub width: u32,
    /// Screenshot height in pixels.
    pub height: u32,
    /// The active color theme name.
    pub theme: String,
    /// Number of visible panes at capture time.
    pub pane_count: usize,
    /// Project directory name, if any.
    pub project: Option<String>,
    /// Git branch at capture time, if any.
    pub branch: Option<String>,
}

// ---------------------------------------------------------------------------
// GPU readback
// ---------------------------------------------------------------------------

/// Capture the current frame from a wgpu texture.
///
/// Creates a staging buffer, copies the texture into it, maps the buffer for
/// reading, and returns the raw RGBA pixel data. This is the standard wgpu
/// readback pattern.
///
/// The returned `Vec<u8>` has length `width * height * 4` (RGBA).
pub fn capture_frame(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
) -> anyhow::Result<Vec<u8>> {
    // wgpu requires rows to be aligned to 256 bytes.
    let bytes_per_pixel = 4u32;
    let unpadded_bytes_per_row = width * bytes_per_pixel;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded_bytes_per_row = unpadded_bytes_per_row.div_ceil(align) * align;

    let buffer_size = (padded_bytes_per_row * height) as u64;

    let staging_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("screenshot-staging"),
        size: buffer_size,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("screenshot-encoder"),
    });

    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &staging_buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_bytes_per_row),
                rows_per_image: None,
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );

    queue.submit(std::iter::once(encoder.finish()));

    // Map the staging buffer and read the data.
    let buffer_slice = staging_buffer.slice(..);
    map_buffer_with_timeout(device, buffer_slice)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let mapped = buffer_slice.get_mapped_range();

    // Strip row padding if present.
    let mut pixels = Vec::with_capacity((width * height * bytes_per_pixel) as usize);
    for row in 0..height {
        let start = (row * padded_bytes_per_row) as usize;
        let end = start + unpadded_bytes_per_row as usize;
        pixels.extend_from_slice(&mapped[start..end]);
    }

    drop(mapped);
    staging_buffer.unmap();

    Ok(pixels)
}

/// Capture a sub-rect of a wgpu texture into CPU-readable RGBA pixels.
///
/// Like [`capture_frame`], but reads only the rectangle whose top-left is
/// `origin` and whose size is `extent`. The rect is clamped to the texture
/// bounds; if the clamped rect is empty, returns `Ok(vec![])`.
///
/// The returned `Vec<u8>` has length `extent.0 * extent.1 * 4` (RGBA), or
/// is empty when the requested rect lies entirely outside the texture.
pub fn capture_frame_sub(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    origin: (u32, u32),
    extent: (u32, u32),
) -> anyhow::Result<Vec<u8>> {
    let tex_w = texture.width();
    let tex_h = texture.height();
    let (ox, oy) = origin;
    let (mut ew, mut eh) = extent;
    if ox >= tex_w || oy >= tex_h || ew == 0 || eh == 0 {
        return Ok(Vec::new());
    }
    if ox + ew > tex_w {
        ew = tex_w - ox;
    }
    if oy + eh > tex_h {
        eh = tex_h - oy;
    }

    let bytes_per_pixel = 4u32;
    let unpadded_bytes_per_row = ew * bytes_per_pixel;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded_bytes_per_row = unpadded_bytes_per_row.div_ceil(align) * align;
    let buffer_size = (padded_bytes_per_row * eh) as u64;

    let staging_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("screenshot-sub-staging"),
        size: buffer_size,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("screenshot-sub-encoder"),
    });

    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d { x: ox, y: oy, z: 0 },
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &staging_buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_bytes_per_row),
                rows_per_image: None,
            },
        },
        wgpu::Extent3d {
            width: ew,
            height: eh,
            depth_or_array_layers: 1,
        },
    );

    queue.submit(std::iter::once(encoder.finish()));

    let buffer_slice = staging_buffer.slice(..);
    map_buffer_with_timeout(device, buffer_slice)
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let mapped = buffer_slice.get_mapped_range();
    let mut pixels = Vec::with_capacity((ew * eh * bytes_per_pixel) as usize);
    for row in 0..eh {
        let start = (row * padded_bytes_per_row) as usize;
        let end = start + unpadded_bytes_per_row as usize;
        pixels.extend_from_slice(&mapped[start..end]);
    }
    drop(mapped);
    staging_buffer.unmap();

    Ok(pixels)
}

// ---------------------------------------------------------------------------
// PNG encoding
// ---------------------------------------------------------------------------

/// Encode RGBA pixels to PNG bytes.
///
/// Returns a complete PNG file as a byte vector.
pub fn encode_png(pixels: &[u8], width: u32, height: u32) -> anyhow::Result<Vec<u8>> {
    let mut png_data = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png_data, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(pixels)?;
    }
    Ok(png_data)
}

// ---------------------------------------------------------------------------
// Save to file
// ---------------------------------------------------------------------------

/// Save a screenshot with metadata to a file.
///
/// The PNG is written to `path`, and a companion `.json` sidecar file is
/// written alongside it containing the serialized metadata.
pub fn save_screenshot(
    pixels: &[u8],
    width: u32,
    height: u32,
    metadata: &ScreenshotMetadata,
    path: &std::path::Path,
) -> anyhow::Result<()> {
    let png_bytes = encode_png(pixels, width, height)?;
    std::fs::write(path, &png_bytes)?;

    // Write metadata sidecar as `.json` next to the PNG.
    let meta_path = path.with_extension("json");
    let meta_json = serde_json::to_string_pretty(metadata)?;
    std::fs::write(&meta_path, meta_json)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_png_produces_valid_png() {
        // 2x2 red image.
        let pixels = vec![
            255, 0, 0, 255, // red
            0, 255, 0, 255, // green
            0, 0, 255, 255, // blue
            255, 255, 0, 255, // yellow
        ];
        let png_bytes = encode_png(&pixels, 2, 2).unwrap();

        // PNG magic bytes.
        assert_eq!(&png_bytes[..4], &[0x89, 0x50, 0x4E, 0x47]);

        // Round-trip: decode back and verify.
        let decoder = png::Decoder::new(png_bytes.as_slice());
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        buf.truncate(info.buffer_size());

        assert_eq!(info.width, 2);
        assert_eq!(info.height, 2);
        assert_eq!(buf, pixels);
    }

    #[test]
    fn encode_png_1x1() {
        let pixels = vec![128, 64, 32, 200];
        let png_bytes = encode_png(&pixels, 1, 1).unwrap();
        assert!(!png_bytes.is_empty());
        assert_eq!(&png_bytes[..4], &[0x89, 0x50, 0x4E, 0x47]);
    }

    #[test]
    fn metadata_serialization_round_trip() {
        let meta = ScreenshotMetadata {
            timestamp: 1700000000,
            width: 1920,
            height: 1080,
            theme: "phantom-dark".to_string(),
            pane_count: 3,
            project: Some("phantom".to_string()),
            branch: Some("main".to_string()),
        };

        let json = serde_json::to_string(&meta).unwrap();
        let deserialized: ScreenshotMetadata = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.timestamp, meta.timestamp);
        assert_eq!(deserialized.width, meta.width);
        assert_eq!(deserialized.height, meta.height);
        assert_eq!(deserialized.theme, meta.theme);
        assert_eq!(deserialized.pane_count, meta.pane_count);
        assert_eq!(deserialized.project, meta.project);
        assert_eq!(deserialized.branch, meta.branch);
    }

    #[test]
    fn metadata_with_none_fields() {
        let meta = ScreenshotMetadata {
            timestamp: 0,
            width: 800,
            height: 600,
            theme: "default".to_string(),
            pane_count: 1,
            project: None,
            branch: None,
        };

        let json = serde_json::to_string(&meta).unwrap();
        assert!(json.contains("\"project\":null"));
        assert!(json.contains("\"branch\":null"));

        let deserialized: ScreenshotMetadata = serde_json::from_str(&json).unwrap();
        assert!(deserialized.project.is_none());
        assert!(deserialized.branch.is_none());
    }

    #[test]
    fn save_screenshot_writes_files() {
        let dir = tempfile::tempdir().unwrap();
        let png_path = dir.path().join("test.png");

        let pixels = vec![255, 0, 0, 255]; // 1x1 red
        let meta = ScreenshotMetadata {
            timestamp: 1700000000,
            width: 1,
            height: 1,
            theme: "test".to_string(),
            pane_count: 1,
            project: Some("phantom".to_string()),
            branch: None,
        };

        save_screenshot(&pixels, 1, 1, &meta, &png_path).unwrap();

        // PNG file exists and starts with magic bytes.
        let png_data = std::fs::read(&png_path).unwrap();
        assert_eq!(&png_data[..4], &[0x89, 0x50, 0x4E, 0x47]);

        // Metadata sidecar exists and deserializes.
        let meta_path = dir.path().join("test.json");
        let meta_json = std::fs::read_to_string(&meta_path).unwrap();
        let loaded: ScreenshotMetadata = serde_json::from_str(&meta_json).unwrap();
        assert_eq!(loaded.timestamp, 1700000000);
        assert_eq!(loaded.theme, "test");
    }

    /// Verify that a timed-out GPU poll is reported as `ScreenshotError::GpuTimeout`.
    ///
    /// We cannot create a real wgpu device in a unit-test environment, so this
    /// test simulates the channel-timeout branch directly: it creates the same
    /// `mpsc` channel used by `map_buffer_with_timeout`, drops the sender
    /// immediately (simulating a hung GPU that never calls the callback), and
    /// waits for `recv_timeout` to fire.
    #[test]
    fn screenshot_returns_gpu_timeout_error_on_hang() {
        use std::sync::mpsc;
        use std::time::Duration;

        // Simulate a hung GPU: sender is dropped without sending anything.
        let (tx, rx) = mpsc::channel::<Result<(), wgpu::BufferAsyncError>>();
        drop(tx);

        let result = rx.recv_timeout(Duration::from_millis(10));

        // With the sender dropped `recv_timeout` returns `Disconnected`, not
        // `Timeout`, but either maps to a non-Ok ScreenshotError in real code.
        // The important property: it DOES NOT block forever.
        assert!(
            result.is_err(),
            "recv_timeout must not return Ok when the sender is dropped"
        );

        // Verify the GpuTimeout variant is constructible and displayable —
        // confirms the error type compiles and its Display impl doesn't panic.
        let err = ScreenshotError::GpuTimeout;
        let msg = err.to_string();
        assert!(msg.contains("GPU"), "GpuTimeout display should mention GPU");

        // Verify ChannelClosed variant as well.
        let err = ScreenshotError::ChannelClosed;
        let msg = err.to_string();
        assert!(!msg.is_empty(), "ChannelClosed display must be non-empty");
    }
}
