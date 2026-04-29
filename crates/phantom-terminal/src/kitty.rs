// Kitty graphics protocol parser.
//
// The Kitty graphics protocol transmits image data via APC escape sequences:
//
//     ESC _G <key>=<value>,<key>=<value>;base64data ESC \
//
// This module parses the key=value header and decodes the base64 payload.
// It does NOT handle the escape sequence framing — the caller is expected to
// strip the `\e_G` prefix and `\e\` suffix before calling `parse_kitty_command`.
//
// Reference: https://sw.kovidgoyal.net/kitty/graphics-protocol/

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// The action requested by a Kitty graphics command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyAction {
    /// `a=t` — Transmit image data (upload, don't display yet).
    Transmit,
    /// `a=T` — Transmit and display immediately.
    TransmitAndDisplay,
    /// `a=p` — Display a previously transmitted image.
    Display,
    /// `a=d` — Delete an image.
    Delete,
    /// `a=q` — Query support (terminal responds but takes no visible action).
    Query,
}

/// The pixel format of the image payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyFormat {
    /// `f=32` — 32-bit RGBA (default).
    Rgba,
    /// `f=24` — 24-bit RGB.
    Rgb,
    /// `f=100` — PNG compressed.
    Png,
}

/// How the image data is transmitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyTransmission {
    /// `t=d` — Direct: payload is the image data (base64-encoded).
    Direct,
    /// `t=f` — File: payload is a file path.
    File,
    /// `t=t` — Temporary file: payload is a path, delete after reading.
    TempFile,
    /// `t=s` — Shared memory object name.
    SharedMemory,
}

/// A parsed Kitty graphics command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyCommand {
    /// The action to perform.
    pub action: KittyAction,
    /// Image ID (`i=`), if specified.
    pub image_id: Option<u32>,
    /// Image number (`I=`), if specified. Used for deduplication.
    pub image_number: Option<u32>,
    /// Pixel width of the source image (`s=`).
    pub width: Option<u32>,
    /// Pixel height of the source image (`v=`).
    pub height: Option<u32>,
    /// Pixel format.
    pub format: KittyFormat,
    /// Transmission medium.
    pub transmission: KittyTransmission,
    /// Whether more chunks follow (`m=1`).
    pub more_chunks: bool,
    /// Display columns (`c=`) — how many terminal columns the image should span.
    pub display_cols: Option<u32>,
    /// Display rows (`r=`) — how many terminal rows the image should span.
    pub display_rows: Option<u32>,
    /// X offset within the cell (`X=`) in pixels.
    pub x_offset: Option<u32>,
    /// Y offset within the cell (`Y=`) in pixels.
    pub y_offset: Option<u32>,
    /// Z-index (`z=`) for layering.
    pub z_index: Option<i32>,
    /// Compression (`o=`). `z` for zlib.
    pub compression: Option<char>,
    /// Decoded payload data (from base64). Empty if no payload was present.
    pub payload: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Parse a Kitty graphics escape sequence payload.
///
/// The input should be the content between `\e_G` and `\e\` (or `\x07`).
/// Format: `key=value,key=value;base64data`
///
/// Returns `None` if the payload is malformed.
#[must_use]
pub fn parse_kitty_command(payload: &str) -> Option<KittyCommand> {
    // Split header from base64 data at the first semicolon.
    let (header, data) = match payload.find(';') {
        Some(idx) => (&payload[..idx], &payload[idx + 1..]),
        None => {
            // No semicolon: entire string is the header with no data.
            (payload, "")
        }
    };

    // Parse key=value pairs from the comma-separated header.
    let kvs = parse_header(header);

    // Decode the action.
    let action = match kvs.get("a").map(|s| s.as_str()) {
        Some("t") => KittyAction::Transmit,
        Some("T") => KittyAction::TransmitAndDisplay,
        Some("p") => KittyAction::Display,
        Some("d") => KittyAction::Delete,
        Some("q") => KittyAction::Query,
        // Default action is transmit-and-display when data is present,
        // or transmit when just uploading.
        None => {
            if data.is_empty() {
                KittyAction::Display
            } else {
                KittyAction::TransmitAndDisplay
            }
        }
        Some(_) => return None,
    };

    // Format.
    let format = match kvs.get("f").map(|s| s.as_str()) {
        Some("32") | None => KittyFormat::Rgba,
        Some("24") => KittyFormat::Rgb,
        Some("100") => KittyFormat::Png,
        Some(_) => return None,
    };

    // Transmission type.
    let transmission = match kvs.get("t").map(|s| s.as_str()) {
        Some("d") | None => KittyTransmission::Direct,
        Some("f") => KittyTransmission::File,
        Some("t") => KittyTransmission::TempFile,
        Some("s") => KittyTransmission::SharedMemory,
        Some(_) => return None,
    };

    // Decode base64 payload.
    let payload_bytes = if data.is_empty() {
        Vec::new()
    } else {
        base64_decode(data)?
    };

    Some(KittyCommand {
        action,
        image_id: kvs.get("i").and_then(|v| v.parse().ok()),
        image_number: kvs.get("I").and_then(|v| v.parse().ok()),
        width: kvs.get("s").and_then(|v| v.parse().ok()),
        height: kvs.get("v").and_then(|v| v.parse().ok()),
        format,
        transmission,
        more_chunks: kvs.get("m").is_some_and(|v| v == "1"),
        display_cols: kvs.get("c").and_then(|v| v.parse().ok()),
        display_rows: kvs.get("r").and_then(|v| v.parse().ok()),
        x_offset: kvs.get("X").and_then(|v| v.parse().ok()),
        y_offset: kvs.get("Y").and_then(|v| v.parse().ok()),
        z_index: kvs.get("z").and_then(|v| v.parse().ok()),
        compression: kvs.get("o").and_then(|v| v.chars().next()),
        payload: payload_bytes,
    })
}

/// Parse the comma-separated `key=value` header into a map.
fn parse_header(header: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();

    if header.is_empty() {
        return map;
    }

    for pair in header.split(',') {
        if let Some(eq_idx) = pair.find('=') {
            let key = &pair[..eq_idx];
            let value = &pair[eq_idx + 1..];
            if !key.is_empty() {
                map.insert(key.to_string(), value.to_string());
            }
        }
    }

    map
}

/// Minimal base64 decoder (standard alphabet, with optional padding).
///
/// We avoid pulling in a full base64 crate for this single use case.
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    const DECODE_TABLE: [u8; 256] = {
        let mut table = [0xFFu8; 256];
        let mut i = 0u8;
        // A-Z -> 0..25
        while i < 26 {
            table[(b'A' + i) as usize] = i;
            i += 1;
        }
        // a-z -> 26..51
        i = 0;
        while i < 26 {
            table[(b'a' + i) as usize] = 26 + i;
            i += 1;
        }
        // 0-9 -> 52..61
        i = 0;
        while i < 10 {
            table[(b'0' + i) as usize] = 52 + i;
            i += 1;
        }
        table[b'+' as usize] = 62;
        table[b'/' as usize] = 63;
        table
    };

    // Strip padding and whitespace.
    let clean: Vec<u8> = input
        .bytes()
        .filter(|&b| b != b'=' && b != b'\n' && b != b'\r' && b != b' ')
        .collect();

    let mut output = Vec::with_capacity(clean.len() * 3 / 4);
    let mut i = 0;

    while i + 3 < clean.len() {
        let a = DECODE_TABLE[clean[i] as usize];
        let b = DECODE_TABLE[clean[i + 1] as usize];
        let c = DECODE_TABLE[clean[i + 2] as usize];
        let d = DECODE_TABLE[clean[i + 3] as usize];

        if a == 0xFF || b == 0xFF || c == 0xFF || d == 0xFF {
            return None;
        }

        let triple = ((a as u32) << 18) | ((b as u32) << 12) | ((c as u32) << 6) | (d as u32);
        output.push((triple >> 16) as u8);
        output.push((triple >> 8) as u8);
        output.push(triple as u8);

        i += 4;
    }

    // Handle remaining 2 or 3 characters.
    let remaining = clean.len() - i;
    if remaining == 2 {
        let a = DECODE_TABLE[clean[i] as usize];
        let b = DECODE_TABLE[clean[i + 1] as usize];
        if a == 0xFF || b == 0xFF {
            return None;
        }
        output.push(((a as u16) << 2 | (b as u16) >> 4) as u8);
    } else if remaining == 3 {
        let a = DECODE_TABLE[clean[i] as usize];
        let b = DECODE_TABLE[clean[i + 1] as usize];
        let c = DECODE_TABLE[clean[i + 2] as usize];
        if a == 0xFF || b == 0xFF || c == 0xFF {
            return None;
        }
        let triple = ((a as u32) << 18) | ((b as u32) << 12) | ((c as u32) << 6);
        output.push((triple >> 16) as u8);
        output.push((triple >> 8) as u8);
    } else if remaining == 1 {
        // Invalid base64: a single leftover character is malformed.
        return None;
    }

    Some(output)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_transmit_and_display() {
        // Simulate: \e_Ga=T,f=100,s=200,v=100,i=1;iVBOR...\e\
        let payload_b64 = base64_encode(&[0x89, 0x50, 0x4E, 0x47]); // PNG magic
        let input = format!("a=T,f=100,s=200,v=100,i=1;{payload_b64}");

        let cmd = parse_kitty_command(&input).unwrap();
        assert_eq!(cmd.action, KittyAction::TransmitAndDisplay);
        assert_eq!(cmd.format, KittyFormat::Png);
        assert_eq!(cmd.width, Some(200));
        assert_eq!(cmd.height, Some(100));
        assert_eq!(cmd.image_id, Some(1));
        assert_eq!(cmd.payload, vec![0x89, 0x50, 0x4E, 0x47]);
    }

    #[test]
    fn parse_transmit_only() {
        let cmd = parse_kitty_command("a=t,i=42,s=10,v=10,f=32;AQID").unwrap();
        assert_eq!(cmd.action, KittyAction::Transmit);
        assert_eq!(cmd.format, KittyFormat::Rgba);
        assert_eq!(cmd.image_id, Some(42));
        assert_eq!(cmd.payload, vec![1, 2, 3]); // base64 "AQID" = [1, 2, 3]
    }

    #[test]
    fn parse_display_previously_sent() {
        let cmd = parse_kitty_command("a=p,i=42").unwrap();
        assert_eq!(cmd.action, KittyAction::Display);
        assert_eq!(cmd.image_id, Some(42));
        assert!(cmd.payload.is_empty());
    }

    #[test]
    fn parse_delete() {
        let cmd = parse_kitty_command("a=d,i=42").unwrap();
        assert_eq!(cmd.action, KittyAction::Delete);
        assert_eq!(cmd.image_id, Some(42));
    }

    #[test]
    fn parse_query() {
        let cmd = parse_kitty_command("a=q,i=1,s=1,v=1;AAAA").unwrap();
        assert_eq!(cmd.action, KittyAction::Query);
    }

    #[test]
    fn parse_default_action_with_data() {
        // No `a=` key, but data is present: default is TransmitAndDisplay.
        let cmd = parse_kitty_command("f=100,s=1,v=1;AAAA").unwrap();
        assert_eq!(cmd.action, KittyAction::TransmitAndDisplay);
    }

    #[test]
    fn parse_default_action_no_data() {
        // No `a=` key and no data: default is Display.
        let cmd = parse_kitty_command("i=1").unwrap();
        assert_eq!(cmd.action, KittyAction::Display);
    }

    #[test]
    fn parse_format_rgb() {
        let cmd = parse_kitty_command("a=t,f=24,s=1,v=1;AAAA").unwrap();
        assert_eq!(cmd.format, KittyFormat::Rgb);
    }

    #[test]
    fn parse_format_default_rgba() {
        let cmd = parse_kitty_command("a=t,s=1,v=1;AAAA").unwrap();
        assert_eq!(cmd.format, KittyFormat::Rgba);
    }

    #[test]
    fn parse_more_chunks() {
        let cmd = parse_kitty_command("a=t,m=1,i=1;AAAA").unwrap();
        assert!(cmd.more_chunks);
    }

    #[test]
    fn parse_no_more_chunks() {
        let cmd = parse_kitty_command("a=t,m=0,i=1;AAAA").unwrap();
        assert!(!cmd.more_chunks);
    }

    #[test]
    fn parse_display_dimensions() {
        let cmd = parse_kitty_command("a=T,c=40,r=20,i=1;AAAA").unwrap();
        assert_eq!(cmd.display_cols, Some(40));
        assert_eq!(cmd.display_rows, Some(20));
    }

    #[test]
    fn parse_offsets() {
        let cmd = parse_kitty_command("a=T,X=5,Y=3,i=1;AAAA").unwrap();
        assert_eq!(cmd.x_offset, Some(5));
        assert_eq!(cmd.y_offset, Some(3));
    }

    #[test]
    fn parse_z_index() {
        let cmd = parse_kitty_command("a=T,z=-1,i=1;AAAA").unwrap();
        assert_eq!(cmd.z_index, Some(-1));
    }

    #[test]
    fn parse_compression() {
        let cmd = parse_kitty_command("a=t,o=z,i=1;AAAA").unwrap();
        assert_eq!(cmd.compression, Some('z'));
    }

    #[test]
    fn parse_transmission_file() {
        let path_b64 = base64_encode(b"/tmp/image.png");
        let input = format!("a=t,t=f,f=100;{path_b64}");
        let cmd = parse_kitty_command(&input).unwrap();
        assert_eq!(cmd.transmission, KittyTransmission::File);
        assert_eq!(cmd.payload, b"/tmp/image.png");
    }

    #[test]
    fn parse_transmission_temp_file() {
        let cmd = parse_kitty_command("a=t,t=t,f=100;AAAA").unwrap();
        assert_eq!(cmd.transmission, KittyTransmission::TempFile);
    }

    #[test]
    fn parse_invalid_action() {
        let cmd = parse_kitty_command("a=x,i=1");
        assert!(cmd.is_none());
    }

    #[test]
    fn parse_invalid_format() {
        let cmd = parse_kitty_command("a=t,f=99,i=1;AAAA");
        assert!(cmd.is_none());
    }

    #[test]
    fn parse_invalid_transmission() {
        let cmd = parse_kitty_command("a=t,t=x,i=1;AAAA");
        assert!(cmd.is_none());
    }

    #[test]
    fn parse_empty_payload() {
        let cmd = parse_kitty_command("a=t,i=1").unwrap();
        assert!(cmd.payload.is_empty());
    }

    #[test]
    fn parse_image_number() {
        let cmd = parse_kitty_command("a=T,i=5,I=42;AAAA").unwrap();
        assert_eq!(cmd.image_id, Some(5));
        assert_eq!(cmd.image_number, Some(42));
    }

    #[test]
    fn base64_round_trip() {
        let data = b"Hello, Kitty!";
        let encoded = base64_encode(data);
        let decoded = base64_decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn base64_empty() {
        let decoded = base64_decode("").unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn base64_padding() {
        // "A" (1 byte) -> base64 "QQ==" (with padding)
        let decoded = base64_decode("QQ==").unwrap();
        assert_eq!(decoded, vec![65]);

        // "AB" (2 bytes) -> base64 "QUI=" (with padding)
        let decoded = base64_decode("QUI=").unwrap();
        assert_eq!(decoded, vec![65, 66]);
    }

    #[test]
    fn base64_no_padding() {
        // Same as above but without padding characters.
        let decoded = base64_decode("QQ").unwrap();
        assert_eq!(decoded, vec![65]);

        let decoded = base64_decode("QUI").unwrap();
        assert_eq!(decoded, vec![65, 66]);
    }

    #[test]
    fn base64_invalid_chars() {
        // Non-base64 character.
        let decoded = base64_decode("@@@@");
        assert!(decoded.is_none());
    }

    #[test]
    fn base64_single_char_invalid() {
        // A single leftover character is invalid base64.
        let decoded = base64_decode("A");
        assert!(decoded.is_none());
    }

    #[test]
    fn header_parsing() {
        let kvs = parse_header("a=T,f=100,s=200,v=100,i=1");
        assert_eq!(kvs.get("a").unwrap(), "T");
        assert_eq!(kvs.get("f").unwrap(), "100");
        assert_eq!(kvs.get("s").unwrap(), "200");
        assert_eq!(kvs.get("v").unwrap(), "100");
        assert_eq!(kvs.get("i").unwrap(), "1");
    }

    #[test]
    fn header_empty() {
        let kvs = parse_header("");
        assert!(kvs.is_empty());
    }

    #[test]
    fn header_no_equals() {
        let kvs = parse_header("abc,def");
        assert!(kvs.is_empty());
    }

    // Helper: minimal base64 encoder for tests.
    fn base64_encode(data: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut output = String::with_capacity((data.len() + 2) / 3 * 4);
        let mut i = 0;

        while i + 2 < data.len() {
            let triple =
                ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8) | (data[i + 2] as u32);
            output.push(TABLE[((triple >> 18) & 0x3F) as usize] as char);
            output.push(TABLE[((triple >> 12) & 0x3F) as usize] as char);
            output.push(TABLE[((triple >> 6) & 0x3F) as usize] as char);
            output.push(TABLE[(triple & 0x3F) as usize] as char);
            i += 3;
        }

        let remaining = data.len() - i;
        if remaining == 1 {
            let val = (data[i] as u32) << 16;
            output.push(TABLE[((val >> 18) & 0x3F) as usize] as char);
            output.push(TABLE[((val >> 12) & 0x3F) as usize] as char);
            output.push('=');
            output.push('=');
        } else if remaining == 2 {
            let val = ((data[i] as u32) << 16) | ((data[i + 1] as u32) << 8);
            output.push(TABLE[((val >> 18) & 0x3F) as usize] as char);
            output.push(TABLE[((val >> 12) & 0x3F) as usize] as char);
            output.push(TABLE[((val >> 6) & 0x3F) as usize] as char);
            output.push('=');
        }

        output
    }
}
