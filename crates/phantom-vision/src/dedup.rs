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
//!     ├── hamming_distance > thr  → true  (visually distinct, SAD skipped)
//!     ├── sad > thr               → true  (pixel-level distinct)
//!     └── both ≤ thr              → false (near-duplicate, skip)
//! ```

use crate::{box_downsample_gray, box_downsample_gray_from_gray, dhash_pack_from_9x8, VisionError};

// ── DHash ─────────────────────────────────────────────────────────────────────

/// 64-bit difference hash (dHash) of a frame, stored as eight bytes.
///
/// `DHash` is an ergonomic namespace over the crate's authoritative dHash
/// implementation in [`crate::dhash`]. [`DHash::compute`] delegates to that
/// function and packs the result big-endian; the two paths cannot drift.
///
/// Algorithm: downsample the input RGBA image to 9×8 grayscale, then for each
/// row compare adjacent horizontal pixels — set the corresponding bit when the
/// left pixel is brighter than the right. The 64 bits are packed into
/// `[u8; 8]` MSB-first.
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
    /// Delegates to [`crate::dhash`] and returns the 64-bit result packed as
    /// `[u8; 8]` big-endian (MSB first).
    ///
    /// # Errors
    ///
    /// - [`VisionError::ZeroDim`] if `width` or `height` is zero.
    /// - [`VisionError::SizeMismatch`] if `pixels.len() != width * height * 4`.
    pub fn compute(pixels: &[u8], width: u32, height: u32) -> Result<[u8; 8], VisionError> {
        crate::dhash(pixels, width, height).map(u64::to_be_bytes)
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
    /// Prefer [`SadGate::try_compute_sad`] when the caller does not control
    /// both sides of the comparison and cannot guarantee equal, non-empty
    /// slices.
    ///
    /// # Panics
    ///
    /// Panics if `a.len() != b.len()` or if either slice is empty.
    #[must_use]
    pub fn compute_sad(a: &[u8], b: &[u8]) -> f32 {
        assert_eq!(a.len(), b.len(), "SadGate::compute_sad: slice lengths differ");
        assert!(!a.is_empty(), "SadGate::compute_sad: slices must not be empty");
        Self::sad_unchecked(a, b)
    }

    /// Fallible variant of [`SadGate::compute_sad`] that returns `None`
    /// instead of panicking on mismatched or empty input.
    ///
    /// Returns `None` when `a.len() != b.len()` or when both slices are empty,
    /// otherwise the mean per-pixel absolute difference in `[0.0, 255.0]`.
    #[must_use]
    pub fn try_compute_sad(a: &[u8], b: &[u8]) -> Option<f32> {
        if a.len() != b.len() || a.is_empty() {
            return None;
        }
        Some(Self::sad_unchecked(a, b))
    }

    /// Inner SAD body. Caller is responsible for verifying `a.len() == b.len()`
    /// and `!a.is_empty()`.
    fn sad_unchecked(a: &[u8], b: &[u8]) -> f32 {
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

/// Default Hamming-distance threshold for the dedup gate.
const DEFAULT_HAMMING_THRESHOLD: u32 = 10;
/// Default mean SAD threshold for the dedup gate.
const DEFAULT_SAD_THRESHOLD: f32 = 15.0;

/// Stateful two-stage frame deduplication gate.
///
/// Keeps the dHash and a downsampled 64×48 grayscale thumbnail of the last
/// forwarded frame as a single atomic tuple. On each call to
/// [`FrameDedup::should_process`] it computes the 64×48 thumbnail once, derives
/// the 9×8 dHash thumb from it (avoiding a second pass over the full RGBA
/// source frame), then evaluates the gate:
///
/// 1. Compare Hamming distance against `hamming_threshold`. If it exceeds the
///    threshold the frame is forwarded immediately and the SAD comparison is
///    skipped.
/// 2. Otherwise compute mean SAD on the 64×48 thumb and compare against
///    `sad_threshold`.
///
/// The frame is considered novel (returns `true`) if **either** signal exceeds
/// its threshold. The first frame always returns `true` regardless of
/// thresholds.
///
/// # Defaults
///
/// | Parameter           | Default  |
/// |---------------------|----------|
/// | `hamming_threshold` | `10`     |
/// | `sad_threshold`     | `15.0`   |
///
/// Use [`FrameDedup::default`] to obtain a gate with these values.
///
/// # Example
///
/// ```
/// use phantom_vision::dedup::FrameDedup;
///
/// let mut gate = FrameDedup::default();
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
    /// dHash + 64×48 thumbnail of the last forwarded frame.
    ///
    /// Kept as a single `Option` so the two halves of the dedup state cannot
    /// disagree about whether a previous frame exists.
    last: Option<([u8; 8], Vec<u8>)>,
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
            last: None,
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
    /// programming error at the call site. Validation happens once inside the
    /// shared `check_rgba` path used by [`crate::dhash`].
    pub fn should_process(&mut self, frame: &[u8], width: u32, height: u32) -> bool {
        // Downsample the full RGBA frame to the 64×48 SAD thumb **once**.
        // The dHash thumb is derived from this buffer instead of re-traversing
        // the multi-megapixel source. `box_downsample_gray` validates the
        // buffer/dimensions invariants implicitly via the index arithmetic; we
        // assert the surface contract here so a malformed call panics with a
        // clear message instead of producing a wrong-shaped output.
        assert!(width > 0 && height > 0, "FrameDedup: dimensions must be non-zero");
        assert_eq!(
            frame.len(),
            width as usize * height as usize * 4,
            "FrameDedup: frame buffer length does not match dimensions"
        );

        let thumb = box_downsample_gray(frame, width, height, THUMB_W, THUMB_H);
        let dhash_thumb = box_downsample_gray_from_gray(&thumb, THUMB_W, THUMB_H, 9, 8);
        let hash = dhash_pack_from_9x8(&dhash_thumb).to_be_bytes();

        let novel = match &self.last {
            None => true,
            Some((last_hash, last_thumb)) => {
                let hamming = DHash::hamming_distance(last_hash, &hash);
                if hamming > self.hamming_threshold {
                    // Cheap signal already decisive; skip the SAD pass.
                    true
                } else {
                    SadGate::compute_sad(last_thumb, &thumb) > self.sad_threshold
                }
            }
        };

        if novel {
            self.last = Some((hash, thumb));
        }

        novel
    }
}

impl Default for FrameDedup {
    /// Build a [`FrameDedup`] gate with the documented default thresholds
    /// (`hamming_threshold = 10`, `sad_threshold = 15.0`).
    fn default() -> Self {
        Self::new(DEFAULT_HAMMING_THRESHOLD, DEFAULT_SAD_THRESHOLD)
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
        // DHash::compute must agree with the module-level `dhash` free function
        // (since the former delegates to the latter, this enforces the
        // delegation contract rather than two independent implementations).
        let pixels = gradient_rgba(48, 32);
        let via_struct = DHash::compute(&pixels, 48, 32).unwrap();
        let via_fn = crate::dhash(&pixels, 48, 32).unwrap();
        assert_eq!(via_struct, via_fn.to_be_bytes());
    }

    #[test]
    fn dhash_compute_rejects_zero_dim() {
        let err = DHash::compute(&[], 0, 16).unwrap_err();
        assert!(matches!(err, VisionError::ZeroDim));
        let err = DHash::compute(&[], 16, 0).unwrap_err();
        assert!(matches!(err, VisionError::ZeroDim));
    }

    #[test]
    fn dhash_compute_rejects_size_mismatch() {
        let bad = vec![0u8; 10];
        let err = DHash::compute(&bad, 4, 4).unwrap_err();
        assert!(matches!(
            err,
            VisionError::SizeMismatch { expected: 64, got: 10 }
        ));
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

    #[test]
    fn try_compute_sad_returns_none_on_length_mismatch() {
        let a = vec![0u8; 16];
        let b = vec![0u8; 32];
        assert!(SadGate::try_compute_sad(&a, &b).is_none());
    }

    #[test]
    fn try_compute_sad_returns_none_on_empty_input() {
        assert!(SadGate::try_compute_sad(&[], &[]).is_none());
    }

    #[test]
    fn try_compute_sad_matches_compute_sad_for_valid_input() {
        let a = vec![10u8; 64];
        let b = vec![40u8; 64];
        let panicking = SadGate::compute_sad(&a, &b);
        let fallible = SadGate::try_compute_sad(&a, &b).unwrap();
        assert!((panicking - fallible).abs() < 1e-6);
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

    #[test]
    fn default_uses_documented_thresholds() {
        // FrameDedup::default() must match the thresholds documented in the
        // rustdoc table (hamming=10, sad=15.0). Verify by behaviour: a small
        // perturbation that would pass `new(0, 0.0)` must be deduplicated by
        // the loose default thresholds.
        let mut default_gate = FrameDedup::default();
        let frame = solid(64, 48, [100, 100, 100, 255]);
        let mut tweaked = frame.clone();
        // Flip a single pixel: hamming distance from a uniform image will be 0
        // (no gradient change), and mean SAD across 64×48 = 3072 pixels is
        // ~255/3072 ≈ 0.08, well below the default 15.0 threshold.
        tweaked[0] = 255;
        tweaked[1] = 255;
        tweaked[2] = 255;
        assert!(default_gate.should_process(&frame, 64, 48));
        assert!(
            !default_gate.should_process(&tweaked, 64, 48),
            "single-pixel change must be deduplicated under default thresholds"
        );
    }

    #[test]
    fn handles_minimum_valid_1x1_frame() {
        // 1×1 input still satisfies width > 0 && height > 0 and len = 4.
        // box_downsample_gray will broadcast the single luma value across the
        // 64×48 thumb, and dHash of a uniform image is all-zero — so the gate
        // must accept the first frame and deduplicate the second.
        let mut gate = FrameDedup::default();
        let frame = vec![123u8, 45, 67, 255];
        assert!(gate.should_process(&frame, 1, 1));
        assert!(!gate.should_process(&frame, 1, 1));
    }
}
