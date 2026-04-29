//! Kitty graphics protocol image handler.
//!
//! Bridges the PTY layer (which intercepts Kitty APC escape sequences) with
//! the renderer layer (which uploads decoded images to the GPU).
//!
//! Handles chunked transfers, format decoding, and produces `DecodedImage`
//! values ready for `ImageManager::place_image`.

use std::collections::HashMap;

use phantom_renderer::images::DecodedImage;
use phantom_terminal::kitty::{KittyAction, KittyCommand, KittyFormat};

// ---------------------------------------------------------------------------
// ImageHandler
// ---------------------------------------------------------------------------

/// Handles Kitty graphics protocol commands intercepted from PTY output.
pub struct ImageHandler {
    /// Accumulates payload chunks for multi-part transfers keyed by image ID.
    pending_chunks: HashMap<u32, Vec<u8>>,
}

impl ImageHandler {
    /// Create a new image handler.
    pub fn new() -> Self {
        Self {
            pending_chunks: HashMap::new(),
        }
    }

    /// Process a Kitty command. Returns a decoded image if one is ready to
    /// display, along with the requested display columns and rows (defaulting
    /// to 0 if unspecified, meaning "use image native size").
    pub fn handle_command(&mut self, cmd: KittyCommand) -> Option<(DecodedImage, u32, u32)> {
        match cmd.action {
            KittyAction::Transmit | KittyAction::TransmitAndDisplay => self.handle_transmit(cmd),
            KittyAction::Delete => {
                // If there is a pending chunked transfer for this ID, discard it.
                if let Some(id) = cmd.image_id {
                    self.pending_chunks.remove(&id);
                }
                None
            }
            KittyAction::Display | KittyAction::Query => {
                // Display of previously-uploaded images and query responses are
                // handled at a higher layer that has access to the ImageManager.
                None
            }
        }
    }

    /// Handle a transmit (or transmit-and-display) command.
    fn handle_transmit(&mut self, cmd: KittyCommand) -> Option<(DecodedImage, u32, u32)> {
        let image_id = cmd.image_id.unwrap_or(0);

        if cmd.more_chunks {
            // Accumulate this chunk; the image is not yet complete.
            let entry = self.pending_chunks.entry(image_id).or_default();
            entry.extend_from_slice(&cmd.payload);
            return None;
        }

        // Final chunk (or a single non-chunked transfer).
        let data = if let Some(mut accumulated) = self.pending_chunks.remove(&image_id) {
            accumulated.extend_from_slice(&cmd.payload);
            accumulated
        } else {
            cmd.payload
        };

        if data.is_empty() {
            return None;
        }

        let decoded = Self::decode_image(&data, cmd.format, cmd.width, cmd.height)?;

        let display_cols = cmd.display_cols.unwrap_or(0);
        let display_rows = cmd.display_rows.unwrap_or(0);

        Some((decoded, display_cols, display_rows))
    }

    /// Decode image data based on the Kitty pixel format.
    fn decode_image(
        data: &[u8],
        format: KittyFormat,
        width: Option<u32>,
        height: Option<u32>,
    ) -> Option<DecodedImage> {
        match format {
            KittyFormat::Png => DecodedImage::from_png(data),
            KittyFormat::Rgba => {
                let w = width?;
                let h = height?;
                DecodedImage::from_rgba(w, h, data.to_vec())
            }
            KittyFormat::Rgb => {
                let w = width?;
                let h = height?;
                DecodedImage::from_rgb(w, h, data)
            }
        }
    }
}

impl Default for ImageHandler {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use phantom_terminal::kitty::{KittyFormat, KittyTransmission};

    /// Helper to build a minimal KittyCommand for testing.
    fn make_cmd(
        action: KittyAction,
        image_id: Option<u32>,
        format: KittyFormat,
        width: Option<u32>,
        height: Option<u32>,
        payload: Vec<u8>,
        more_chunks: bool,
    ) -> KittyCommand {
        KittyCommand {
            action,
            image_id,
            image_number: None,
            width,
            height,
            format,
            transmission: KittyTransmission::Direct,
            more_chunks,
            display_cols: None,
            display_rows: None,
            x_offset: None,
            y_offset: None,
            z_index: None,
            compression: None,
            payload,
        }
    }

    #[test]
    fn single_rgba_transmit() {
        let mut handler = ImageHandler::new();
        // 1x1 red RGBA pixel.
        let pixel = vec![255, 0, 0, 255];
        let cmd = make_cmd(
            KittyAction::TransmitAndDisplay,
            Some(1),
            KittyFormat::Rgba,
            Some(1),
            Some(1),
            pixel.clone(),
            false,
        );
        let result = handler.handle_command(cmd);
        assert!(result.is_some());
        let (img, _, _) = result.unwrap();
        assert_eq!(img.width, 1);
        assert_eq!(img.height, 1);
        assert_eq!(img.data, pixel);
    }

    #[test]
    fn chunked_transfer_assembles_correctly() {
        let mut handler = ImageHandler::new();

        // 1x1 RGBA = 4 bytes, split across two chunks.
        let chunk1 = vec![255, 0];
        let chunk2 = vec![0, 255];

        // First chunk: more_chunks=true.
        let cmd1 = make_cmd(
            KittyAction::Transmit,
            Some(42),
            KittyFormat::Rgba,
            Some(1),
            Some(1),
            chunk1,
            true,
        );
        assert!(handler.handle_command(cmd1).is_none());

        // Second chunk: more_chunks=false (final).
        let cmd2 = make_cmd(
            KittyAction::TransmitAndDisplay,
            Some(42),
            KittyFormat::Rgba,
            Some(1),
            Some(1),
            chunk2,
            false,
        );
        let result = handler.handle_command(cmd2);
        assert!(result.is_some());
        let (img, _, _) = result.unwrap();
        assert_eq!(img.data, vec![255, 0, 0, 255]);
    }

    #[test]
    fn png_format_decoding() {
        let mut handler = ImageHandler::new();

        // Create a valid 1x1 PNG in memory.
        let mut png_data = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png_data, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[0, 255, 0, 255]).unwrap();
        }

        let cmd = make_cmd(
            KittyAction::TransmitAndDisplay,
            Some(1),
            KittyFormat::Png,
            None,
            None,
            png_data,
            false,
        );
        let result = handler.handle_command(cmd);
        assert!(result.is_some());
        let (img, _, _) = result.unwrap();
        assert_eq!(img.width, 1);
        assert_eq!(img.height, 1);
        assert_eq!(img.data, vec![0, 255, 0, 255]);
    }

    #[test]
    fn rgb_format_decoding() {
        let mut handler = ImageHandler::new();
        // 1x1 RGB pixel (no alpha).
        let pixel = vec![128, 64, 32];
        let cmd = make_cmd(
            KittyAction::TransmitAndDisplay,
            Some(1),
            KittyFormat::Rgb,
            Some(1),
            Some(1),
            pixel,
            false,
        );
        let result = handler.handle_command(cmd);
        assert!(result.is_some());
        let (img, _, _) = result.unwrap();
        // from_rgb adds alpha=255.
        assert_eq!(img.data, vec![128, 64, 32, 255]);
    }

    #[test]
    fn delete_clears_pending_chunks() {
        let mut handler = ImageHandler::new();

        // Start a chunked transfer.
        let cmd1 = make_cmd(
            KittyAction::Transmit,
            Some(99),
            KittyFormat::Rgba,
            Some(1),
            Some(1),
            vec![1, 2],
            true,
        );
        handler.handle_command(cmd1);
        assert!(handler.pending_chunks.contains_key(&99));

        // Delete it.
        let del = make_cmd(
            KittyAction::Delete,
            Some(99),
            KittyFormat::Rgba,
            None,
            None,
            vec![],
            false,
        );
        handler.handle_command(del);
        assert!(!handler.pending_chunks.contains_key(&99));
    }
}
