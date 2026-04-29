//! Phantom vision: perceptual-hash dedup + GPT-4V analysis pipeline.
//!
//! # Modules
//!
//! - [`format`] — [`Screenshot`] canonical capture format with PNG sidecar metadata.
//! - [`analysis`] — [`VisionBackend`] trait, [`OpenAiVisionBackend`] (GPT-4V), [`MockVisionBackend`].
//!
//! # Frame types
//!
//! [`VisionFrame`] is a lightweight, allocation-friendly capture buffer used by
//! the analysis pipeline. [`format::Screenshot`] is the richer, PNG-encoded
//! format used for storage and dedup.
//!
//! # Dedup pipeline
//!
//! `fast_diff_gate` (cheap SAD on 64×64 luma) rejects near-identical frames
//! before the more expensive `dhash` is computed.

#![forbid(unsafe_code)]

pub mod analysis;
pub mod format;

// Re-export commonly used types.
pub use analysis::{
    Analysis, MockVisionBackend, OpenAiVisionBackend, PromptTemplate, UiElement, VisionBackend,
    MAX_IMAGE_BYTES,
};
pub use format::{Screenshot, ScreenshotSource};

/// Type alias matching the issue #71 spec name for the OpenAI backend.
pub type Gpt4VBackend = OpenAiVisionBackend;

// ── Errors ────────────────────────────────────────────────────────────────────

/// All errors returned by the phantom-vision module.
#[derive(Debug, thiserror::Error)]
pub enum VisionError {
    /// Image dimensions are zero.
    #[error("image dimensions zero")]
    ZeroDim,
    /// Buffer size does not match the declared dimensions.
    #[error("buffer size mismatch: expected {expected}, got {got}")]
    SizeMismatch { expected: usize, got: usize },
    /// PNG image byte length exceeds the backend cost-guard limit.
    #[error("image too large: {size} bytes exceeds limit of {limit} bytes")]
    ImageTooLarge { size: usize, limit: usize },
    /// PNG encoding failure.
    #[error("PNG encode error: {0}")]
    Encode(String),
    /// PNG decoding failure.
    #[error("PNG decode error: {0}")]
    Decode(String),
    /// Vision backend (network / API) failure.
    #[error("vision backend error: {0}")]
    Backend(String),
}

// ── VisionFrame ───────────────────────────────────────────────────────────────

/// Lightweight raw frame buffer for the analysis pipeline.
///
/// Prefer this over [`Screenshot`] when PNG encoding or the dHash sidecar are
/// not needed — it avoids unnecessary allocations in hot paths.
///
/// # Construction
///
/// ```
/// use phantom_vision::VisionFrame;
///
/// let rgba = vec![0u8; 4 * 4 * 4]; // 4×4 solid black
/// let frame = VisionFrame::new(4, 4, rgba, 0).unwrap();
/// assert_eq!(frame.width(), 4);
/// assert_eq!(frame.height(), 4);
/// assert_eq!(frame.pixels().len(), 64);
/// assert_eq!(frame.timestamp_ms(), 0);
/// ```
#[derive(Debug, Clone)]
pub struct VisionFrame {
    width: u32,
    height: u32,
    /// Raw RGBA pixel data, row-major.
    pixels: Vec<u8>,
    timestamp_ms: u64,
}

impl VisionFrame {
    /// Build a new frame, validating that `pixels.len() == width * height * 4`.
    ///
    /// # Errors
    ///
    /// - [`VisionError::ZeroDim`] if `width` or `height` is zero.
    /// - [`VisionError::SizeMismatch`] if the buffer length is wrong.
    pub fn new(
        width: u32,
        height: u32,
        pixels: Vec<u8>,
        timestamp_ms: u64,
    ) -> Result<Self, VisionError> {
        if width == 0 || height == 0 {
            return Err(VisionError::ZeroDim);
        }
        let expected = (width as usize)
            .checked_mul(height as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or(VisionError::SizeMismatch {
                expected: usize::MAX,
                got: pixels.len(),
            })?;
        if pixels.len() != expected {
            return Err(VisionError::SizeMismatch {
                expected,
                got: pixels.len(),
            });
        }
        Ok(Self {
            width,
            height,
            pixels,
            timestamp_ms,
        })
    }

    /// Frame width in pixels.
    #[must_use]
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Frame height in pixels.
    #[must_use]
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Raw RGBA pixel data, row-major.
    #[must_use]
    pub fn pixels(&self) -> &[u8] {
        &self.pixels
    }

    /// Milliseconds since the Unix epoch when the frame was captured.
    #[must_use]
    pub fn timestamp_ms(&self) -> u64 {
        self.timestamp_ms
    }

    /// Convert to a [`Screenshot`] (PNG-encodes the pixel data with dHash sidecar).
    ///
    /// # Errors
    ///
    /// Propagates [`VisionError::Encode`] if PNG encoding fails.
    pub fn to_screenshot(&self) -> Result<Screenshot, VisionError> {
        Screenshot::new(
            &self.pixels,
            self.width,
            self.height,
            self.timestamp_ms,
            ScreenshotSource::FullDesktop,
        )
    }
}

// ── VisionAnalysis ────────────────────────────────────────────────────────────

/// Compact result returned after analysing a [`VisionFrame`].
///
/// Matches the field names specified in issue #71 (`description`, `anomalies`,
/// `confidence`). Use [`Analysis`] from [`analysis`] for the richer format
/// produced by the full [`VisionBackend`] trait.
#[derive(Debug, Clone)]
pub struct VisionAnalysis {
    description: String,
    anomalies: Vec<String>,
    confidence: f32,
}

impl VisionAnalysis {
    /// Build a new analysis result.
    #[must_use]
    pub fn new(description: String, anomalies: Vec<String>, confidence: f32) -> Self {
        Self {
            description,
            anomalies,
            confidence,
        }
    }

    /// Human-readable description of what is visible in the frame.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Detected anomalies (errors, crashes, unexpected output).
    #[must_use]
    pub fn anomalies(&self) -> &[String] {
        &self.anomalies
    }

    /// Overall confidence in the analysis, in `[0.0, 1.0]`.
    #[must_use]
    pub fn confidence(&self) -> f32 {
        self.confidence
    }
}

// ── BT.601 luma / dedup helpers ───────────────────────────────────────────────

/// BT.601 luma weights, scaled to integer math (sum = 1024).
const LUMA_R: u32 = 306; // ~0.299 * 1024
const LUMA_G: u32 = 601; // ~0.587 * 1024
const LUMA_B: u32 = 117; // ~0.114 * 1024

/// Convert one RGBA pixel to 8-bit grayscale.
#[inline]
fn rgba_to_luma(r: u8, g: u8, b: u8) -> u8 {
    let y = (LUMA_R * r as u32 + LUMA_G * g as u32 + LUMA_B * b as u32) >> 10;
    y as u8
}

/// Validate buffer size for an RGBA image of `width` x `height`.
fn check_rgba(rgba: &[u8], width: u32, height: u32) -> Result<(), VisionError> {
    if width == 0 || height == 0 {
        return Err(VisionError::ZeroDim);
    }
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or(VisionError::SizeMismatch {
            expected: usize::MAX,
            got: rgba.len(),
        })?;
    if rgba.len() != expected {
        return Err(VisionError::SizeMismatch {
            expected,
            got: rgba.len(),
        });
    }
    Ok(())
}

/// Box-filter downsample an RGBA image to a `target_w x target_h` grayscale buffer.
fn box_downsample_gray(
    rgba: &[u8],
    width: u32,
    height: u32,
    target_w: u32,
    target_h: u32,
) -> Vec<u8> {
    let tw = target_w as usize;
    let th = target_h as usize;
    let w = width as usize;
    let h = height as usize;

    let mut sum = vec![0u32; tw * th];
    let mut count = vec![0u32; tw * th];

    for y in 0..h {
        let ty = (y * th) / h;
        let row = ty * tw;
        let pix_row = y * w * 4;
        for x in 0..w {
            let tx = (x * tw) / w;
            let i = pix_row + x * 4;
            let luma = rgba_to_luma(rgba[i], rgba[i + 1], rgba[i + 2]);
            let cell = row + tx;
            sum[cell] += u32::from(luma);
            count[cell] += 1;
        }
    }

    let mut out = vec![0u8; tw * th];
    for i in 0..(tw * th) {
        out[i] = if count[i] == 0 {
            0
        } else {
            (sum[i] / count[i]) as u8
        };
    }
    out
}

/// Compute a 64-bit difference hash (dHash) of an 8-bit RGBA image.
///
/// Algorithm: downsample to 9x8 grayscale, compute differences between
/// adjacent horizontal pixels, set bit if left > right.
///
/// # Errors
///
/// - [`VisionError::ZeroDim`] if `width` or `height` is zero.
/// - [`VisionError::SizeMismatch`] if `rgba.len()` is not `width * height * 4`.
pub fn dhash(rgba: &[u8], width: u32, height: u32) -> Result<u64, VisionError> {
    check_rgba(rgba, width, height)?;
    let small = box_downsample_gray(rgba, width, height, 9, 8);

    let mut hash: u64 = 0;
    for row in 0..8 {
        let base = row * 9;
        for col in 0..8 {
            let left = small[base + col];
            let right = small[base + col + 1];
            let bit = u64::from(left > right);
            hash = (hash << 1) | bit;
        }
    }
    Ok(hash)
}

/// Hamming distance between two dhashes. A distance of <= 5 typically means duplicate.
#[must_use]
pub fn hamming_distance(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Downsample RGBA to a 64x64 grayscale buffer (4096 bytes).
///
/// Used as the reference in [`fast_diff_gate`].
///
/// # Errors
///
/// - [`VisionError::ZeroDim`] if `width` or `height` is zero.
/// - [`VisionError::SizeMismatch`] if `rgba.len()` is not `width * height * 4`.
pub fn downsample_to_64x64_gray(
    rgba: &[u8],
    width: u32,
    height: u32,
) -> Result<Vec<u8>, VisionError> {
    check_rgba(rgba, width, height)?;
    Ok(box_downsample_gray(rgba, width, height, 64, 64))
}

/// Cheap pixel-difference gate: SAD on 64x64 luma vs a reference buffer.
///
/// Used as a fast first-pass before computing the full dhash.
/// Max SAD for fully inverted 64x64 gray = 4096 * 255 = 1_044_480.
///
/// # Errors
///
/// - [`VisionError::ZeroDim`] if `width` or `height` is zero.
/// - [`VisionError::SizeMismatch`] if `rgba.len()` is not `width * height * 4`,
///   or `reference_64x64.len() != 4096`.
pub fn fast_diff_gate(
    rgba: &[u8],
    reference_64x64: &[u8],
    width: u32,
    height: u32,
) -> Result<u32, VisionError> {
    check_rgba(rgba, width, height)?;
    if reference_64x64.len() != 4096 {
        return Err(VisionError::SizeMismatch {
            expected: 4096,
            got: reference_64x64.len(),
        });
    }
    let small = box_downsample_gray(rgba, width, height, 64, 64);
    let mut sad: u32 = 0;
    for i in 0..4096 {
        sad += (i32::from(small[i]) - i32::from(reference_64x64[i])).unsigned_abs();
    }
    Ok(sad)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        let n = (width as usize) * (height as usize);
        let mut buf = Vec::with_capacity(n * 4);
        for _ in 0..n {
            buf.extend_from_slice(&rgba);
        }
        buf
    }

    fn gradient_9x8() -> Vec<u8> {
        let mut buf = Vec::with_capacity(9 * 8 * 4);
        for y in 0..8u32 {
            for x in 0..9u32 {
                let v = if (x + y) % 2 == 0 { 200u8 } else { 50 };
                buf.extend_from_slice(&[v, v, v, 255]);
            }
        }
        buf
    }

    // ── dHash / dedup ─────────────────────────────────────────────────────────

    #[test]
    fn dhash_returns_nonzero_for_real_image() {
        let img = gradient_9x8();
        let h = dhash(&img, 9, 8).unwrap();
        assert_ne!(h, 0, "gradient image should not hash to zero");
    }

    #[test]
    fn dhash_zero_for_uniform_image() {
        let img = solid(16, 16, [128, 128, 128, 255]);
        let h = dhash(&img, 16, 16).unwrap();
        assert_eq!(h, 0, "uniform image must hash to zero");
    }

    #[test]
    fn dhash_stable_under_minor_changes() {
        let mut a = Vec::with_capacity(64 * 64 * 4);
        for y in 0..64u32 {
            for x in 0..64u32 {
                let v = ((x * 4) + y) as u8;
                a.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let mut b = a.clone();
        let i = (32 * 64 + 32) * 4;
        b[i] = 0;
        b[i + 1] = 0;
        b[i + 2] = 0;
        let ha = dhash(&a, 64, 64).unwrap();
        let hb = dhash(&b, 64, 64).unwrap();
        let d = hamming_distance(ha, hb);
        assert!(d <= 2, "minor change yielded distance {d}, expected <= 2");
    }

    #[test]
    fn dhash_changes_for_inverted_image() {
        let mut a = Vec::with_capacity(64 * 64 * 4);
        let mut b = Vec::with_capacity(64 * 64 * 4);
        for y in 0..64u32 {
            for x in 0..64u32 {
                let v = ((x * 4) as u8).saturating_add(y as u8);
                a.extend_from_slice(&[v, v, v, 255]);
                let inv = 255 - v;
                b.extend_from_slice(&[inv, inv, inv, 255]);
            }
        }
        let ha = dhash(&a, 64, 64).unwrap();
        let hb = dhash(&b, 64, 64).unwrap();
        let d = hamming_distance(ha, hb);
        assert!(d > 32, "inversion yielded distance {d}, expected > 32");
    }

    #[test]
    fn hamming_distance_self_zero() {
        assert_eq!(hamming_distance(0xDEAD_BEEF_CAFE_BABE, 0xDEAD_BEEF_CAFE_BABE), 0);
    }

    #[test]
    fn hamming_distance_full_64() {
        assert_eq!(hamming_distance(0u64, !0u64), 64);
    }

    #[test]
    fn fast_diff_gate_zero_for_identical() {
        let img = solid(128, 128, [50, 100, 150, 255]);
        let reference = downsample_to_64x64_gray(&img, 128, 128).unwrap();
        let sad = fast_diff_gate(&img, &reference, 128, 128).unwrap();
        assert_eq!(sad, 0, "identical image SAD should be zero");
    }

    #[test]
    fn fast_diff_gate_high_for_different() {
        let black = solid(128, 128, [0, 0, 0, 255]);
        let white = solid(128, 128, [255, 255, 255, 255]);
        let reference = downsample_to_64x64_gray(&black, 128, 128).unwrap();
        let sad = fast_diff_gate(&white, &reference, 128, 128).unwrap();
        assert!(sad > 1_000_000, "black vs white SAD = {sad}, expected > 1_000_000");
    }

    #[test]
    fn downsample_returns_4096_bytes() {
        for (w, h) in [(64u32, 64), (128, 128), (1920, 1080), (33, 17)] {
            let img = solid(w, h, [200, 150, 100, 255]);
            let out = downsample_to_64x64_gray(&img, w, h).unwrap();
            assert_eq!(out.len(), 4096, "{w}x{h} should downsample to 4096 bytes");
        }
    }

    #[test]
    fn downsample_rejects_zero_dims() {
        let img: Vec<u8> = Vec::new();
        let err = downsample_to_64x64_gray(&img, 0, 0).unwrap_err();
        assert!(matches!(err, VisionError::ZeroDim));
        let err = downsample_to_64x64_gray(&img, 64, 0).unwrap_err();
        assert!(matches!(err, VisionError::ZeroDim));
        let err = downsample_to_64x64_gray(&img, 0, 64).unwrap_err();
        assert!(matches!(err, VisionError::ZeroDim));
    }

    #[test]
    fn dhash_rejects_size_mismatch() {
        let bad = vec![0u8; 10];
        let err = dhash(&bad, 4, 4).unwrap_err();
        assert!(matches!(err, VisionError::SizeMismatch { expected: 64, got: 10 }));
    }

    // ── VisionFrame ───────────────────────────────────────────────────────────

    #[test]
    fn vision_frame_accessors_round_trip() {
        let pixels = vec![128u8; 8 * 8 * 4];
        let frame = VisionFrame::new(8, 8, pixels.clone(), 42_000).unwrap();
        assert_eq!(frame.width(), 8);
        assert_eq!(frame.height(), 8);
        assert_eq!(frame.pixels(), pixels.as_slice());
        assert_eq!(frame.timestamp_ms(), 42_000);
    }

    #[test]
    fn vision_frame_rejects_zero_width() {
        let err = VisionFrame::new(0, 8, vec![], 0).unwrap_err();
        assert!(matches!(err, VisionError::ZeroDim));
    }

    #[test]
    fn vision_frame_rejects_zero_height() {
        let err = VisionFrame::new(8, 0, vec![], 0).unwrap_err();
        assert!(matches!(err, VisionError::ZeroDim));
    }

    #[test]
    fn vision_frame_rejects_wrong_buffer_size() {
        let err = VisionFrame::new(4, 4, vec![0u8; 10], 0).unwrap_err();
        assert!(matches!(err, VisionError::SizeMismatch { expected: 64, got: 10 }));
    }

    #[test]
    fn vision_frame_to_screenshot_succeeds() {
        let pixels = vec![200u8; 16 * 16 * 4];
        let frame = VisionFrame::new(16, 16, pixels, 1_000).unwrap();
        let shot = frame.to_screenshot().unwrap();
        assert_eq!(shot.dimensions(), (16, 16));
        assert_eq!(shot.captured_at_ms(), 1_000);
    }

    // ── VisionAnalysis ────────────────────────────────────────────────────────

    #[test]
    fn vision_analysis_accessors() {
        let a = VisionAnalysis::new(
            "Terminal showing cargo build".into(),
            vec!["error: unknown field".into()],
            0.92,
        );
        assert_eq!(a.description(), "Terminal showing cargo build");
        assert_eq!(a.anomalies().len(), 1);
        assert!((a.confidence() - 0.92).abs() < 1e-6);
    }

    #[test]
    fn vision_analysis_no_anomalies() {
        let a = VisionAnalysis::new("Clean terminal prompt".into(), vec![], 1.0);
        assert!(a.anomalies().is_empty());
    }
}
