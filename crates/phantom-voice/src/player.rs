//! Audio output for synthesized speech chunks.
//!
//! [`AudioPlayer`] wraps a `rodio` output stream and exposes two ergonomic
//! play surfaces:
//!
//! * [`AudioPlayer::play_bytes`] — decode and play a raw byte slice (e.g.
//!   MP3 or WAV bytes returned by a batch TTS backend).
//! * [`AudioPlayer::play_f32_chunks`] — receive streamed [`SynthAudioChunk`]
//!   values from an [`mpsc::Receiver`] and queue them into the `rodio` sink as
//!   they arrive.  Designed for the streaming OpenAI backend path.
//!
//! The `rodio` `OutputStream` must live as long as the player; we keep it in
//! the struct (prefixed with `_` to suppress the unused-field lint) so the
//! stream is kept alive for the lifetime of `AudioPlayer`.

use std::io::Cursor;

use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink};
use tokio::sync::mpsc;

use crate::SynthAudioChunk;

// ── Error type ────────────────────────────────────────────────────────────────

/// Errors produced by [`AudioPlayer`].
#[derive(Debug, thiserror::Error)]
pub enum PlayerError {
    /// The platform audio device could not be opened.
    #[error("failed to open audio output stream: {0}")]
    StreamOpen(String),
    /// A [`Sink`] could not be constructed from the stream handle.
    #[error("failed to create audio sink: {0}")]
    SinkCreate(String),
    /// The byte buffer could not be decoded as audio.
    #[error("failed to decode audio bytes: {0}")]
    Decode(String),
}

// ── AudioPlayer ───────────────────────────────────────────────────────────────

/// Platform audio output backed by `rodio`.
///
/// Holds the `rodio` output stream alive for the lifetime of this value.
/// Cheap to clone if you need multiple sinks — each clone shares the same
/// underlying `OutputStreamHandle`.
pub struct AudioPlayer {
    /// Keep the stream alive; dropping it would silence all sinks.
    _stream: OutputStream,
    stream_handle: OutputStreamHandle,
}

impl AudioPlayer {
    /// Open the default system audio output device.
    ///
    /// # Errors
    ///
    /// Returns [`PlayerError::StreamOpen`] when no audio output is available
    /// (e.g. headless CI environment or no sound card).
    pub fn new() -> Result<Self, PlayerError> {
        let (_stream, stream_handle) = OutputStream::try_default()
            .map_err(|e| PlayerError::StreamOpen(e.to_string()))?;
        Ok(Self {
            _stream,
            stream_handle,
        })
    }

    /// Play raw audio bytes (MP3, WAV, OGG, FLAC, etc.) through the default
    /// audio output.
    ///
    /// Decodes `audio` with `rodio::Decoder` and blocks the current thread
    /// until the clip finishes playing (unless the `Sink` is dropped early).
    ///
    /// # Errors
    ///
    /// Returns [`PlayerError::SinkCreate`] when the `rodio` sink cannot be
    /// constructed, or [`PlayerError::Decode`] when the byte buffer is not a
    /// recognised audio format.
    pub fn play_bytes(&self, audio: &[u8]) -> Result<(), PlayerError> {
        let sink = Sink::try_new(&self.stream_handle)
            .map_err(|e| PlayerError::SinkCreate(e.to_string()))?;

        let cursor = Cursor::new(audio.to_vec());
        let source = Decoder::new(cursor).map_err(|e| PlayerError::Decode(e.to_string()))?;
        sink.append(source);
        sink.sleep_until_end();
        Ok(())
    }

    /// Spawn a Tokio task that receives [`SynthAudioChunk`] values from `rx`
    /// and queues them into a `rodio` sink for sequential playback.
    ///
    /// Each chunk's `f32` samples are wrapped in `rodio::buffer::SamplesBuffer`
    /// so they are played back at the sample rate reported by the chunk.
    /// The task exits when the sender is dropped (channel closed) or when a
    /// chunk with `is_final == true` has been queued.
    ///
    /// Errors are logged rather than propagated — if the sink cannot be
    /// created the task returns immediately.
    pub fn play_f32_chunks(&self, mut rx: mpsc::Receiver<SynthAudioChunk>) {
        let handle = self.stream_handle.clone();
        tokio::spawn(async move {
            let sink = match Sink::try_new(&handle) {
                Ok(s) => s,
                Err(e) => {
                    log::error!("[phantom-voice] AudioPlayer: failed to create sink: {e}");
                    return;
                }
            };

            while let Some(chunk) = rx.recv().await {
                let is_final = chunk.is_final;
                let source = rodio::buffer::SamplesBuffer::new(
                    1, // mono
                    chunk.sample_rate,
                    chunk.samples,
                );
                sink.append(source);
                if is_final {
                    break;
                }
            }

            // Let `sink` drop here; it will drain its internal queue before
            // being freed, so playback finishes naturally.
            sink.sleep_until_end();
        });
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // Minimal valid 44-byte WAV: 1 channel, 8-bit PCM, 8000 Hz, 1 sample of silence.
    // Constructed from the RIFF/WAVE spec by hand so the test has zero external deps.
    fn silent_wav_bytes() -> Vec<u8> {
        let mut wav: Vec<u8> = Vec::new();
        // RIFF header
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&36u32.to_le_bytes()); // chunk size = 36 + data size
        wav.extend_from_slice(b"WAVE");
        // fmt sub-chunk
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes()); // sub-chunk size (PCM = 16)
        wav.extend_from_slice(&1u16.to_le_bytes()); // audio format = PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // num channels = 1
        wav.extend_from_slice(&8000u32.to_le_bytes()); // sample rate = 8000 Hz
        wav.extend_from_slice(&8000u32.to_le_bytes()); // byte rate = 8000 * 1 * 1
        wav.extend_from_slice(&1u16.to_le_bytes()); // block align = 1
        wav.extend_from_slice(&8u16.to_le_bytes()); // bits per sample = 8
        // data sub-chunk
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&1u32.to_le_bytes()); // data size = 1 byte
        wav.push(128u8); // one sample of 8-bit silence (unsigned centre = 128)
        wav
    }

    /// Verify that the WAV fixture parses as a valid rodio `Decoder` — this
    /// exercises the actual format detection path without requiring a sound card.
    #[test]
    fn silent_wav_bytes_are_decodable_by_rodio() {
        let wav = silent_wav_bytes();
        let cursor = std::io::Cursor::new(wav);
        Decoder::new(cursor).expect("silent WAV must be decodable by rodio");
    }

    /// `AudioPlayer::new()` may fail in headless CI environments (no audio
    /// device), so we only assert that — when it succeeds — the player
    /// produces a valid value, and we skip the test gracefully otherwise.
    #[test]
    fn audio_player_plays_silent_wav() {
        let player = match AudioPlayer::new() {
            Ok(p) => p,
            Err(e) => {
                // No audio device available (CI / headless). Treat as skip.
                eprintln!("audio_player_plays_silent_wav: skipping — {e}");
                return;
            }
        };

        let wav = silent_wav_bytes();
        player.play_bytes(&wav).expect("silent WAV must play without error");
    }
}
