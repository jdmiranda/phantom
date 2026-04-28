//! Perceptual-hash dedup for frame storage.
//!
//! Self-contained dHash + cheap pixel-difference gate. No image-decoding deps;
//! callers pass an 8-bit RGBA buffer of known dimensions.
//!
//! Pipeline: `fast_diff_gate` (cheap SAD on 64x64 luma) rejects near-identical
//! frames before the more expensive `dhash` is computed.

#![forbid(unsafe_code)]

/// Errors returned by the vision module.
#[derive(Debug, thiserror::Error)]
pub enum VisionError {
    #[error("image dimensions zero")]
    ZeroDim,
    #[error("buffer size mismatch: expected {expected}, got {got}")]
    SizeMismatch { expected: usize, got: usize },
}

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
///
/// Each target cell averages the luma of all source pixels whose coordinates
/// fall into that cell. Cheap, branch-free, and stable under sub-pixel shifts.
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

/// Hamming distance between two dhashes. <= 5 typically means duplicate.
#[must_use]
pub fn hamming_distance(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Downsample RGBA to a 64x64 grayscale buffer (4096 bytes). Used as the
/// reference in [`fast_diff_gate`].
///
/// # Errors
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

/// Cheap pixel-difference gate: downsample to 64x64 grayscale, compute
/// sum-of-absolute-differences against the reference. Returns the SAD.
///
/// Used as a fast first-pass before computing the full dhash. Max SAD for
/// fully inverted 64x64 gray = 4096 * 255 = 1_044_480.
///
/// # Errors
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `width x height` RGBA buffer filled with one color.
    fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Vec<u8> {
        let n = (width as usize) * (height as usize);
        let mut buf = Vec::with_capacity(n * 4);
        for _ in 0..n {
            buf.extend_from_slice(&rgba);
        }
        buf
    }

    /// Build a 9x8 RGBA buffer with varied (non-monotonic) pixels so some
    /// horizontal diffs go negative and the dhash sets at least one bit.
    fn gradient_9x8() -> Vec<u8> {
        let mut buf = Vec::with_capacity(9 * 8 * 4);
        for y in 0..8u32 {
            for x in 0..9u32 {
                // Checkerboard-ish: alternates high/low across the row.
                let v = if (x + y) % 2 == 0 { 200u8 } else { 50 };
                buf.extend_from_slice(&[v, v, v, 255]);
            }
        }
        buf
    }

    #[test]
    fn dhash_returns_nonzero_for_real_image() {
        let img = gradient_9x8();
        let h = dhash(&img, 9, 8).unwrap();
        assert_ne!(h, 0, "gradient image should not hash to zero");
    }

    #[test]
    fn dhash_zero_for_uniform_image() {
        // All pixels equal -> every diff is 0 -> no bit ever set -> hash = 0.
        let img = solid(16, 16, [128, 128, 128, 255]);
        let h = dhash(&img, 16, 16).unwrap();
        assert_eq!(h, 0, "uniform image must hash to zero");
    }

    #[test]
    fn dhash_stable_under_minor_changes() {
        // Two near-identical 64x64 gradients, one pixel nudged.
        let mut a = Vec::with_capacity(64 * 64 * 4);
        for y in 0..64u32 {
            for x in 0..64u32 {
                let v = ((x * 4) + y) as u8;
                a.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let mut b = a.clone();
        // Flip one pixel hard.
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
        // Black-to-white gradient vs white-to-black gradient: every diff flips.
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
}
