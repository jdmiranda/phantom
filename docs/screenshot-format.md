# Screenshot Format

> **Issue #70** — Canonical screenshot format for `phantom-vision`.

`phantom-vision` stores every captured frame as a **self-describing PNG**: the
image data lives in the standard IDAT chunks, and all metadata lives in a
`tEXt` ancillary chunk with the keyword `phantom-meta`. No sidecar files; no
databases required to recover a screenshot's provenance.

---

## JSON sidecar schema

The value of the `phantom-meta` tEXt chunk is a UTF-8 JSON object:

| Field            | Type   | Description                                                |
|------------------|--------|------------------------------------------------------------|
| `schema_version` | u8     | Always `1` for the current format. Bump on breaking changes. |
| `width`          | u32    | Image width in pixels.                                     |
| `height`         | u32    | Image height in pixels.                                    |
| `captured_at_ms` | u64    | Unix epoch milliseconds when the frame was captured.       |
| `source`         | object | See [`ScreenshotSource`](#screenshotsource) below.         |
| `dhash`          | u64    | 64-bit perceptual difference hash (see [dHash](#dhash)).   |

Example sidecar:

```json
{
  "schema_version": 1,
  "width": 1920,
  "height": 1080,
  "captured_at_ms": 1714300800000,
  "source": { "type": "FullDesktop" },
  "dhash": 12345678901234567890
}
```

---

## ScreenshotSource

The `source` field is a tagged union (`serde` `tag = "type"`):

| Variant       | Extra fields                              | Meaning                                  |
|---------------|-------------------------------------------|------------------------------------------|
| `FullDesktop` | —                                         | Full primary display.                    |
| `Window`      | `app_id: String`                          | A specific app window from the registry. |
| `Pane`        | `adapter_id: String`, `pane_kind: String` | A single pane inside Phantom.            |

---

## dHash

The perceptual difference hash is computed at construction time:

1. Downsample the RGBA buffer to **9x8 grayscale** using a box filter with
   BT.601 luma weights (`R=0.299, G=0.587, B=0.114`).
2. For each of the 8 rows, compare each adjacent pair of pixels (9 columns ->
   8 comparisons). Set bit if `left > right`.
3. Pack the 64 bits (MSB-first, row-major) into a `u64`.

Two frames with Hamming distance <= 5 are considered near-duplicates.

---

## SAD gate

Before computing dHash, callers can use `fast_diff_gate` as a cheap
pre-filter:

1. Downsample the candidate frame to **64x64 grayscale**.
2. Compute the Sum of Absolute Differences (SAD) against a stored 64x64
   reference.
3. If SAD <= threshold (e.g. 50 000), skip the full dHash.

Maximum possible SAD for a 64x64 image = 4 096 x 255 = 1 044 480.

---

## Round-trip guarantee

```
Screenshot::new(rgba, w, h, ts, source)
  -> embeds sidecar in PNG bytes
  -> Screenshot::encode()   // returns png_bytes (clone)
  -> Screenshot::decode()   // recovers all fields exactly
```

All fields (dimensions, `captured_at_ms`, `source`, `dhash`) survive a
full encode-decode round trip with no loss.

---

## Versioning

`SCHEMA_VERSION` is a `u8` constant set to `1`. Increment it whenever the
JSON sidecar schema changes in a backward-incompatible way. Decoders should
reject or warn on unknown schema versions.
