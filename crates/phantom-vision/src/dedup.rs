//! Perceptual-hash deduplication for the frame capture pipeline.
//!
//! Implements the two-stage gate described in issue #70:
//!
//! 1. [`DHash`] — 64-bit difference hash stored as `[u8; 8]`, with Hamming
//!    distance for near-duplicate detection.
//! 2. [`SadGate`] — Sum of Absolute Differences on two equally-sized grayscale
//!    buffers, expressed as a mean per-pixel value in `[0.0, 255.0]`.
//! 3. [`FrameDedup`] — stateful gate that combines both signals to decide
//!    whether a frame is novel enough to forward to GPT-4V analysis.
//!
//! # Pipeline
//!
//! ```text
//! capture frame
//!     │
//!     ▼
//! FrameDedup::should_process
//!     ├── first frame ever        → true  (always forward)
//!     ├── hamming_distance > thr  → true  (visually distinct)
//!     ├── sad > thr               → true  (pixel-level distinct)
//!     └── both ≤ thr              → false (near-duplicate, skip)
//! ```

use crate::{box_downsample_gray, check_rgba, VisionError};

// ── DHash ─────────────────────────────────────────────────────────────────────

/// 64-bit difference hash (dHash) of a frame, stored as eight bytes.
///
/// Algorithm: downsample the input RGBA image to 9×8 grayscale, then for each
/// row compare adjacent horizontal pixels — set the corresponding bit if the
/// left pixel is brighter than the right. The resulting 64 bits are packed into
/// `[u8; 8]` in big-endian order (most-significant bit first).
///
/// # Example
///
/// ```
/// use phantom_vision::dedup::DHash;
///
/// let pixels = vec![128u8; 16 * 16 * 4]; // 16×16 uniform grey
/// let hash = DHash::compute(&pixels, 16, 16).unwrap();
/// // Uniform image → no gradient → all-zero hash.
/// assert_eq!(hash, [0u8; 8]);
/// ```
pub struct DHash;

impl DHash {
    /// Compute the dHash of an RGBA image.
    ///
    /// Downsamples `pixels` to 9×8 grayscale (box filter) then derives 64
    /// gradient bits, packed into `[u8; 8]` big-endian.
    ///
    /// # Errors
    ///
    /// - [`VisionError::ZeroDim`] if `width` or `height` is zero.
    /// - [`VisionError::SizeMismatch`] if `pixels.len() != width * height * 4`.
    pub fn compute(pixels: &[u8], width: u32, height: u32) -> Result<[u8; 8], VisionError> {
        check_rgba(pixels, width, height)?;
        let small = box_downsample_gray(pixels, width, height, 9, 8);

        let mut bits = 0u64;
        for row in 0..8usize {
            let base = row * 9;
            for col in 0..8usize {
                let left = small[base + col];
                let right = small[base + col + 1];
                bits = (bits << 1) | u64::from(left > right);
            }
        }

        Ok(bits.to_be_bytes())
    }

    /// Count the number of differing bits between two dHash values.
    ///
    /// A distance of 0 means identical; ≤ 10 typically indicates a near-duplicate.
    #[must_use]
    pub fn hamming_distance(a: &[u8; 8], b: &[u8; 8]) -> u32 {
        let a_val = u64::from_be_bytes(*a);
        let b_val = u64::from_be_bytes(*b);
        (a_val ^ b_val).count_ones()
    }
}

// ── SadGate ───────────────────────────────────────────────────────────────────

/// Sum of Absolute Differences gate for grayscale pixel buffers.
///
/// Computes the mean per-pixel absolute difference between two equal-length
/// grayscale (single-channel) buffers. The result is in `[0.0, 255.0]`.
///
/// Use this as an independent signal inside [`FrameDedup`] or directly as a
/// cheap pre-filter before a more expensive comparison step.
pub struct SadGate;

impl SadGate {
    /// Compute mean SAD between two equal-length grayscale buffers.
    ///
    /// Returns the mean per-pixel absolute difference in `[0.0, 255.0]`.
    ///
    /// # Panics
    ///
    /// Panics if `a.len() != b.len()` or if either slice is empty.
    #[must_use]
    pub fn compute_sad(a: &[u8], b: &[u8]) -> f32 {
        assert_eq!(a.len(), b.len(), "SadGate::compute_sad: slice lengths differ");
        assert!(!a.is_empty(), "SadGate::compute_sad: slices must not be empty");

        let total: u64 = a
            .iter()
            .zip(b.iter())
            .map(|(&pa, &pb)| u64::from((i16::from(pa) - i16::from(pb)).unsigned_abs()))
            .sum();

        total as f32 / a.len() as f32
    }
}

// ── FrameDedup ────────────────────────────────────────────────────────────────

/// Width of the stored thumbnail used for SAD comparisons.
const THUMB_W: u32 = 64;
/// Height of the stored thumbnail used for SAD comparisons.
const THUMB_H: u32 = 48;

/// Stateful two-stage frame deduplication gate.
///
/// Keeps the dHash and a downsampled 64×48 grayscale thumbnail of the last
/// forwarded frame. On each call to [`FrameDedup::should_process`] it checks
/// both the Hamming distance between the stored and incoming dHash, **and** the
/// mean SAD between the stored and incoming 64×48 thumbnails. The frame is
/// considered novel (returns `true`) if **either** signal exceeds its threshold.
///
/// The first frame always returns `true` regardless of thresholds.
///
/// # Defaults
///
/// | Parameter           | Default  |
/// |---------------------|----------|
/// | `hamming_threshold` | `10`     |
/// | `sad_threshold`     | `15.0`   |
///
/// # Example
///
/// ```
/// use phantom_vision::dedup::FrameDedup;
///
/// let mut gate = FrameDedup::new(10, 15.0);
/// let frame = vec![128u8; 64 * 48 * 4];
///
/// // First call always forwards.
/// assert!(gate.should_process(&frame, 64, 48));
/// // Identical second frame is deduplicated.
/// assert!(!gate.should_process(&frame, 64, 48));
/// ```
pub struct FrameDedup {
    hamming_threshold: u32,
    sad_threshold: f32,
    /// dHash of the last forwarded frame, or `None` before any frame.
    last_hash: Option<[u8; 8]>,
    /// 64×48 grayscale thumbnail of the last forwarded frame (3 072 bytes).
    last_thumb: Option<Vec<u8>>,
}

impl FrameDedup {
    /// Build a new [`FrameDedup`] gate.
    ///
    /// - `hamming_threshold` — frames with Hamming distance ≤ this value are
    ///   candidates for deduplication (typical default: `10`).
    /// - `sad_threshold` — frames with mean SAD ≤ this value are candidates for
    ///   deduplication (typical default: `15.0`).
    #[must_use]
    pub fn new(hamming_threshold: u32, sad_threshold: f32) -> Self {
        Self {
            hamming_threshold,
            sad_threshold,
            last_hash: None,
            last_thumb: None,
        }
    }

    /// Decide whether `frame` should be forwarded to analysis.
    ///
    /// Returns `true` (forward) when:
    /// - this is the very first frame, **or**
    /// - the Hamming distance between this frame's dHash and the stored hash
    ///   exceeds `hamming_threshold`, **or**
    /// - the mean SAD between this frame's 64×48 thumbnail and the stored
    ///   thumbnail exceeds `sad_threshold`.
    ///
    /// When the frame is forwarded the internal state is updated to this frame.
    /// When the frame is deduplicated the internal state is left unchanged.
    ///
    /// # Panics
    ///
    /// Panics if `width == 0`, `height == 0`, or
    /// `frame.len() != width * height * 4`. These conditions indicate a
    /// programming error at the call site.
    pub fn should_process(&mut self, frame: &[u8], width: u32, height: u32) -> bool {
        assert!(width > 0 && height > 0, "FrameDedup: dimensions must be non-zero");
        assert_eq!(
            frame.len(),
            width as usize * height as usize * 4,
            "FrameDedup: frame buffer length does not match dimensions"
        );

        let hash = DHash::compute(frame, width, height)
            .expect("dimensions already validated above");
        let thumb = box_downsample_gray(frame, width, height, THUMB_W, THUMB_H);

        let novel = match (&self.last_hash, &self.last_thumb) {
            (None, _) | (_, None) => true,
            (Some(last_hash), Some(last_thumb)) => {
                let hamming = DHash::hamming_distance(last_hash, &hash);
                let sad = SadGate::compute_sad(last_thumb, &thumb);
                hamming > self.hamming_threshold || sad > self.sad_threshold
            }
        };

        if novel {
            self.last_hash = Some(hash);
            self.last_thumb = Some(thumb);
        }

        novel
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── helpers ───────────────────────────────────────────────────────────────

    fn solid(width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
        let n = width as usize * height as usize;
        let mut buf = Vec::with_capacity(n * 4);
        for _ in 0..n {
            buf.extend_from_slice(&color);
        }
        buf
    }

    // Left-to-right descending brightness so dHash produces many set bits.
    fn gradient_rgba(width: u32, height: u32) -> Vec<u8> {
        let mut buf = Vec::with_capacity(width as usize * height as usize * 4);
        let denom = (width - 1).max(1);
        for _y in 0..height {
            for x in 0..width {
                let v = (255u32 * (width - 1 - x) / denom) as u8;
                buf.extend_from_slice(&[v, v, v, 255]);
            }
        }
        buf
    }

    // ── DHash ─────────────────────────────────────────────────────────────────

    #[test]
    fn hamming_distance_zero_for_same_hash() {
        let hash = [0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0xBA, 0xBE];
        assert_eq!(DHash::hamming_distance(&hash, &hash), 0);
    }

    #[test]
    fn hamming_distance_correct_for_known_inputs() {
        // All-zero vs all-ones → 64 differing bits.
        let all_zero = [0u8; 8];
        let all_ones = [0xFFu8; 8];
        assert_eq!(DHash::hamming_distance(&all_zero, &all_ones), 64);

        // Single bit differs in byte 0.
        let a = [0x01u8, 0, 0, 0, 0, 0, 0, 0];
        let b = [0x00u8, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(DHash::hamming_distance(&a, &b), 1);
    }

    #[test]
    fn dhash_uniform_image_is_zero() {
        let pixels = solid(32, 32, [100, 100, 100, 255]);
        let hash = DHash::compute(&pixels, 32, 32).unwrap();
        assert_eq!(hash, [0u8; 8], "uniform image must hash to all-zero");
    }

    #[test]
    fn dhash_non_uniform_image_is_nonzero() {
        let pixels = gradient_rgba(64, 64);
        let hash = DHash::compute(&pixels, 64, 64).unwrap();
        assert_ne!(hash, [0u8; 8], "gradient image should produce non-zero hash");
    }

    #[test]
    fn dhash_matches_existing_free_function() {
        // DHash::compute must agree with the module-level `dhash` free function.
        let pixels = gradient_rgba(48, 32);
        let via_struct = DHash::compute(&pixels, 48, 32).unwrap();
        let via_fn = crate::dhash(&pixels, 48, 32).unwrap();
        assert_eq!(via_struct, via_fn.to_be_bytes());
    }

    // ── SadGate ───────────────────────────────────────────────────────────────

    #[test]
    fn sad_zero_for_identical() {
        let buf = vec![128u8; 3072]; // 64×48 grayscale
        assert_eq!(SadGate::compute_sad(&buf, &buf), 0.0);
    }

    #[test]
    fn sad_correct_for_known_delta() {
        // All-zero vs all-100 → mean SAD = 100.0 exactly.
        let a = vec![0u8; 3072];
        let b = vec![100u8; 3072];
        let sad = SadGate::compute_sad(&a, &b);
        assert!(
            (sad - 100.0).abs() < 0.01,
            "expected mean SAD ≈ 100.0, got {sad}"
        );
    }

    #[test]
    fn sad_max_value_is_255() {
        let zeros = vec![0u8; 256];
        let full = vec![255u8; 256];
        let sad = SadGate::compute_sad(&zeros, &full);
        assert!((sad - 255.0).abs() < 0.01, "max SAD should be 255.0, got {sad}");
    }

    // ── FrameDedup ────────────────────────────────────────────────────────────

    #[test]
    fn first_frame_always_processes() {
        let mut gate = FrameDedup::new(10, 15.0);
        let frame = solid(64, 48, [50, 100, 150, 255]);
        assert!(gate.should_process(&frame, 64, 48), "first frame must always return true");
    }

    #[test]
    fn identical_frames_are_deduped() {
        let mut gate = FrameDedup::new(10, 15.0);
        let frame = solid(64, 48, [50, 100, 150, 255]);
        assert!(gate.should_process(&frame, 64, 48)); // first: always true
        assert!(!gate.should_process(&frame, 64, 48), "identical second frame must be deduplicated");
        assert!(!gate.should_process(&frame, 64, 48), "identical third frame must be deduplicated");
    }

    #[test]
    fn different_frames_pass_through() {
        let mut gate = FrameDedup::new(10, 15.0);
        let black = solid(64, 48, [0, 0, 0, 255]);
        let white = solid(64, 48, [255, 255, 255, 255]);
        assert!(gate.should_process(&black, 64, 48)); // first
        assert!(gate.should_process(&white, 64, 48), "black→white must pass through");
        assert!(gate.should_process(&black, 64, 48), "white→black must pass through");
    }

    #[test]
    fn state_updates_on_novel_frame() {
        let mut gate = FrameDedup::new(10, 15.0);
        let black = solid(64, 48, [0, 0, 0, 255]);
        let white = solid(64, 48, [255, 255, 255, 255]);

        gate.should_process(&black, 64, 48); // initialise with black
        gate.should_process(&white, 64, 48); // update state to white

        // Another copy of white should now be deduplicated (state is white).
        assert!(!gate.should_process(&white, 64, 48), "white after white should be deduplicated");
    }

    #[test]
    fn very_tight_thresholds_let_distinct_frame_pass() {
        // hamming_threshold=0, sad_threshold=0.0 → any detectable difference triggers.
        let mut gate = FrameDedup::new(0, 0.0);
        let frame_a = solid(64, 48, [100, 100, 100, 255]);
        let mut frame_b = frame_a.clone();
        frame_b[0] = 255;
        frame_b[1] = 255;
        frame_b[2] = 255;
        assert!(gate.should_process(&frame_a, 64, 48));
        assert!(gate.should_process(&frame_b, 64, 48), "one-pixel change with threshold=0 must pass");
    }

    #[test]
    fn very_loose_thresholds_dedup_black_vs_white() {
        // hamming_threshold=64, sad_threshold=255.0 → nothing ever passes after first.
        let mut gate = FrameDedup::new(64, 255.0);
        let black = solid(64, 48, [0, 0, 0, 255]);
        let white = solid(64, 48, [255, 255, 255, 255]);
        assert!(gate.should_process(&black, 64, 48)); // first always true
        assert!(!gate.should_process(&white, 64, 48), "max threshold: white after black is deduplicated");
    }
}
