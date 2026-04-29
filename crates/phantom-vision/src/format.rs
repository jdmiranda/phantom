//! Canonical screenshot format for phantom-vision.
//!
//! A [`Screenshot`] carries raw PNG bytes plus structured metadata sidecar. The
//! sidecar is stored as a JSON string in a `tEXt` chunk keyed `"phantom-meta"`
//! so it travels with the file and survives round-trips through any PNG-aware
//! tool.
//!
//! # Wire schema
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "width": 1920,
//!   "height": 1080,
//!   "captured_at_ms": 1714300800000,
//!   "source": { "type": "FullDesktop" },
//!   "dhash": 12345678901234567890
//! }
//! ```
//!
//! # Round-trip guarantee
//!
//! `Screenshot::encode` → `Screenshot::decode` preserves all fields exactly.
//! The `dhash` field is computed from the raw pixel buffer at construction time
//! so it is always consistent with the image content.

use serde::{Deserialize, Serialize};

use crate::VisionError;

/// Bump this whenever the JSON sidecar schema changes in a breaking way.
pub const SCHEMA_VERSION: u8 = 1;

/// PNG `tEXt` chunk keyword that holds the JSON metadata sidecar.
const META_KEYWORD: &str = "phantom-meta";

/// Where a screenshot was captured from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ScreenshotSource {
    /// The full desktop / primary display.
    FullDesktop,
    /// A specific application window identified by the adapter registry.
    Window { app_id: String },
    /// A single pane inside the Phantom UI.
    Pane { adapter_id: String, pane_kind: String },
}

/// A captured screenshot with all associated metadata.
///
/// Construct via [`Screenshot::new`]. Encode to PNG bytes (with embedded
/// metadata sidecar) via [`Screenshot::encode`]. Recover from those bytes
/// via [`Screenshot::decode`].
#[derive(Debug, Clone)]
pub struct Screenshot {
    /// Raw PNG bytes. Always valid PNG — guaranteed at construction.
    png_bytes: Vec<u8>,
    /// Width and height in pixels.
    dimensions: (u32, u32),
    /// Milliseconds since the Unix epoch when the frame was captured.
    captured_at_ms: u64,
    /// Origin of the screenshot.
    source: ScreenshotSource,
    /// Perceptual difference hash (dHash) of the original RGBA buffer.
    dhash: u64,
}

impl Screenshot {
    /// Raw PNG bytes (always valid, guaranteed at construction).
    #[must_use]
    pub fn png_bytes(&self) -> &[u8] {
        &self.png_bytes
    }

    /// Width and height in pixels as `(width, height)`.
    #[must_use]
    pub fn dimensions(&self) -> (u32, u32) {
        self.dimensions
    }

    /// Milliseconds since the Unix epoch when the frame was captured.
    #[must_use]
    pub fn captured_at_ms(&self) -> u64 {
        self.captured_at_ms
    }

    /// Origin of the screenshot.
    #[must_use]
    pub fn source(&self) -> &ScreenshotSource {
        &self.source
    }

    /// Perceptual difference hash (dHash) of the original RGBA buffer.
    #[must_use]
    pub fn dhash(&self) -> u64 {
        self.dhash
    }

    /// Build a [`Screenshot`] from a raw RGBA pixel buffer.
    ///
    /// The buffer must be exactly `width * height * 4` bytes. The dHash is
    /// computed here so it is always consistent with `png_bytes`.
    ///
    /// # Errors
    ///
    /// - [`VisionError::ZeroDim`] if `width` or `height` is zero.
    /// - [`VisionError::SizeMismatch`] if the buffer length is wrong.
    /// - [`VisionError::Encode`] if PNG encoding fails.
    pub fn new(
        rgba: &[u8],
        width: u32,
        height: u32,
        captured_at_ms: u64,
        source: ScreenshotSource,
    ) -> Result<Self, VisionError> {
        let dhash = crate::dhash(rgba, width, height)?;
        let png_bytes = encode_rgba_to_png(rgba, width, height, captured_at_ms, &source, dhash)?;
        Ok(Self {
            png_bytes,
            dimensions: (width, height),
            captured_at_ms,
            source,
            dhash,
        })
    }

    /// Encode this screenshot as PNG bytes with a `tEXt` sidecar chunk.
    ///
    /// The returned bytes are self-contained: `Screenshot::decode` can recover
    /// all fields from them without any external state.
    ///
    /// # Errors
    ///
    /// Returns [`VisionError::Encode`] if the PNG encoder fails.
    pub fn encode(&self) -> Result<Vec<u8>, VisionError> {
        // Our png_bytes already carry the sidecar from `new`. Return a clone.
        Ok(self.png_bytes.clone())
    }

    /// Recover a [`Screenshot`] from PNG bytes produced by [`Self::encode`].
    ///
    /// # Errors
    ///
    /// - [`VisionError::Decode`] if the bytes are not valid PNG or the sidecar
    ///   is missing / malformed.
    pub fn decode(png_bytes: &[u8]) -> Result<Self, VisionError> {
        let (meta, _rgba, width, height) = decode_png_with_meta(png_bytes)?;
        Ok(Self {
            png_bytes: png_bytes.to_vec(),
            dimensions: (width, height),
            captured_at_ms: meta.captured_at_ms,
            source: meta.source,
            dhash: meta.dhash,
        })
    }
}

// ── Internal PNG helpers ──────────────────────────────────────────────────────

/// JSON sidecar that lives in the PNG `tEXt` chunk.
#[derive(Serialize, Deserialize)]
struct ScreenshotMeta {
    schema_version: u8,
    width: u32,
    height: u32,
    captured_at_ms: u64,
    source: ScreenshotSource,
    dhash: u64,
}

/// Encode RGBA pixel data to PNG bytes and embed the metadata sidecar.
fn encode_rgba_to_png(
    rgba: &[u8],
    width: u32,
    height: u32,
    captured_at_ms: u64,
    source: &ScreenshotSource,
    dhash: u64,
) -> Result<Vec<u8>, VisionError> {
    let meta = ScreenshotMeta {
        schema_version: SCHEMA_VERSION,
        width,
        height,
        captured_at_ms,
        source: source.clone(),
        dhash,
    };
    let meta_json =
        serde_json::to_string(&meta).map_err(|e| VisionError::Encode(e.to_string()))?;

    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);

        // Embed the JSON sidecar as a tEXt chunk before the image data.
        encoder
            .add_text_chunk(META_KEYWORD.to_string(), meta_json)
            .map_err(|e| VisionError::Encode(e.to_string()))?;

        let mut writer = encoder
            .write_header()
            .map_err(|e| VisionError::Encode(e.to_string()))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| VisionError::Encode(e.to_string()))?;
    }
    Ok(out)
}

/// Decode PNG bytes produced by [`encode_rgba_to_png`].
///
/// Returns `(meta, rgba_pixels, width, height)`.
fn decode_png_with_meta(
    png_bytes: &[u8],
) -> Result<(ScreenshotMeta, Vec<u8>, u32, u32), VisionError> {
    let decoder = png::Decoder::new(png_bytes);
    let mut reader = decoder
        .read_info()
        .map_err(|e| VisionError::Decode(e.to_string()))?;

    // Pull the tEXt sidecar out of the ancillary chunks collected during
    // header parsing.
    let meta_json = reader
        .info()
        .uncompressed_latin1_text
        .iter()
        .find(|chunk| chunk.keyword == META_KEYWORD)
        .map(|chunk| chunk.text.clone())
        .ok_or_else(|| VisionError::Decode("missing phantom-meta tEXt chunk".to_string()))?;

    let meta: ScreenshotMeta = serde_json::from_str(&meta_json)
        .map_err(|e| VisionError::Decode(format!("malformed sidecar JSON: {e}")))?;

    let info = reader.info();
    let width = info.width;
    let height = info.height;
    let buf_size = reader.output_buffer_size();

    let mut rgba = vec![0u8; buf_size];
    let frame = reader
        .next_frame(&mut rgba)
        .map_err(|e| VisionError::Decode(e.to_string()))?;
    rgba.truncate(frame.buffer_size());

    Ok((meta, rgba, width, height))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_rgba(width: u32, height: u32, colour: [u8; 4]) -> Vec<u8> {
        let n = (width as usize) * (height as usize);
        let mut buf = Vec::with_capacity(n * 4);
        for _ in 0..n {
            buf.extend_from_slice(&colour);
        }
        buf
    }

    fn gradient_rgba(width: u32, height: u32) -> Vec<u8> {
        let mut buf = Vec::with_capacity((width as usize) * (height as usize) * 4);
        for y in 0..height {
            for x in 0..width {
                let v = ((x + y) % 256) as u8;
                buf.extend_from_slice(&[v, v.wrapping_add(30), v.wrapping_add(60), 255]);
            }
        }
        buf
    }

    #[test]
    fn round_trip_preserves_dimensions() {
        let rgba = solid_rgba(64, 48, [200, 100, 50, 255]);
        let s = Screenshot::new(&rgba, 64, 48, 1_000_000, ScreenshotSource::FullDesktop).unwrap();
        let encoded = s.encode().unwrap();
        let decoded = Screenshot::decode(&encoded).unwrap();
        assert_eq!(decoded.dimensions(), (64, 48));
    }

    #[test]
    fn round_trip_preserves_captured_at_ms() {
        let rgba = solid_rgba(32, 32, [10, 20, 30, 255]);
        let ts = 1_714_300_800_000u64;
        let s = Screenshot::new(&rgba, 32, 32, ts, ScreenshotSource::FullDesktop).unwrap();
        let decoded = Screenshot::decode(&s.encode().unwrap()).unwrap();
        assert_eq!(decoded.captured_at_ms(), ts);
    }

    #[test]
    fn round_trip_preserves_source_full_desktop() {
        let rgba = solid_rgba(16, 16, [255, 0, 0, 255]);
        let s = Screenshot::new(&rgba, 16, 16, 42, ScreenshotSource::FullDesktop).unwrap();
        let decoded = Screenshot::decode(&s.encode().unwrap()).unwrap();
        assert_eq!(*decoded.source(), ScreenshotSource::FullDesktop);
    }

    #[test]
    fn round_trip_preserves_source_window() {
        let rgba = solid_rgba(16, 16, [0, 255, 0, 255]);
        let src = ScreenshotSource::Window {
            app_id: "com.example.app".to_string(),
        };
        let s = Screenshot::new(&rgba, 16, 16, 99, src.clone()).unwrap();
        let decoded = Screenshot::decode(&s.encode().unwrap()).unwrap();
        assert_eq!(*decoded.source(), src);
    }

    #[test]
    fn round_trip_preserves_source_pane() {
        let rgba = solid_rgba(16, 16, [0, 0, 255, 255]);
        let src = ScreenshotSource::Pane {
            adapter_id: "uuid-1234".to_string(),
            pane_kind: "terminal".to_string(),
        };
        let s = Screenshot::new(&rgba, 16, 16, 7, src.clone()).unwrap();
        let decoded = Screenshot::decode(&s.encode().unwrap()).unwrap();
        assert_eq!(*decoded.source(), src);
    }

    #[test]
    fn round_trip_preserves_dhash() {
        let rgba = gradient_rgba(64, 64);
        let s = Screenshot::new(&rgba, 64, 64, 0, ScreenshotSource::FullDesktop).unwrap();
        let h_before = s.dhash();
        let decoded = Screenshot::decode(&s.encode().unwrap()).unwrap();
        assert_eq!(decoded.dhash(), h_before);
    }

    #[test]
    fn dhash_collision_similar_frames_within_hamming_4() {
        let mut a = gradient_rgba(64, 64);
        let b = a.clone();
        a[0] = a[0].saturating_add(10);
        a[1] = a[1].saturating_add(10);
        a[2] = a[2].saturating_add(10);
        let sa = Screenshot::new(&a, 64, 64, 0, ScreenshotSource::FullDesktop).unwrap();
        let sb = Screenshot::new(&b, 64, 64, 0, ScreenshotSource::FullDesktop).unwrap();
        let dist = crate::hamming_distance(sa.dhash(), sb.dhash());
        assert!(dist <= 4, "similar frames: hamming distance {dist}, expected <= 4");
    }

    #[test]
    fn dhash_very_different_frames_exceed_hamming_4() {
        let mut a = Vec::with_capacity(64 * 64 * 4);
        let mut b = Vec::with_capacity(64 * 64 * 4);
        for _y in 0..64u32 {
            for x in 0..64u32 {
                let v = (x * 4).min(255) as u8;
                a.extend_from_slice(&[v, v, v, 255]);
                let inv = 255 - v;
                b.extend_from_slice(&[inv, inv, inv, 255]);
            }
        }
        let sa = Screenshot::new(&a, 64, 64, 0, ScreenshotSource::FullDesktop).unwrap();
        let sb = Screenshot::new(&b, 64, 64, 0, ScreenshotSource::FullDesktop).unwrap();
        let dist = crate::hamming_distance(sa.dhash(), sb.dhash());
        assert!(dist > 4, "mirrored gradient should exceed hamming 4, got {dist}");
    }

    #[test]
    fn new_rejects_zero_dimensions() {
        let err =
            Screenshot::new(&[], 0, 0, 0, ScreenshotSource::FullDesktop).unwrap_err();
        assert!(matches!(err, VisionError::ZeroDim));
    }

    #[test]
    fn new_rejects_size_mismatch() {
        let bad = vec![0u8; 10];
        let err =
            Screenshot::new(&bad, 4, 4, 0, ScreenshotSource::FullDesktop).unwrap_err();
        assert!(matches!(err, VisionError::SizeMismatch { .. }));
    }

    #[test]
    fn decode_rejects_non_png() {
        let err = Screenshot::decode(b"not a png").unwrap_err();
        assert!(matches!(err, VisionError::Decode(_)));
    }

    #[test]
    fn schema_version_is_one() {
        assert_eq!(SCHEMA_VERSION, 1);
    }
}
