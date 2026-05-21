//! Piper local TTS backend.
//!
//! [Piper](https://github.com/rhasspy/piper) is an offline, low-latency
//! text-to-speech engine. This backend spawns the `piper` binary as a
//! subprocess, writes the input text to its stdin, and reads raw 16-bit
//! little-endian mono PCM from its stdout. The PCM stream is decoded into
//! `f32` samples at 22 050 Hz and returned as a single [`SynthAudioChunk`]
//! with `is_final == true`.
//!
//! # Configuration
//!
//! Use [`PiperVoiceBackend::from_env`] to construct a backend from the
//! `PIPER_MODEL` environment variable, which must be the path to a `.onnx`
//! VITS model file (e.g. `en_US-lessac-medium.onnx`).
//!
//! # Subprocess contract
//!
//! The backend invokes:
//! ```text
//! piper --model <model_path> --output-raw
//! ```
//! and pipes text to stdin. Piper writes raw little-endian signed 16-bit
//! mono PCM to stdout. The binary must be present on `PATH`; if it is not,
//! [`VoiceError::Backend`] is returned.

use std::io::Write as _;
use std::process::{Command, Stdio};

use crate::{BoxedVoiceStream, SynthAudioChunk, VoiceError, VoiceProfile, VoiceStyle, VoiceSynth};

/// Sample rate of Piper's raw PCM output.
const PIPER_SAMPLE_RATE: u32 = 22_050;

/// Local Piper TTS backend.
///
/// Construct with [`PiperVoiceBackend::from_env`] (reads `PIPER_MODEL`) or
/// [`PiperVoiceBackend::new`] for an explicit model path.
#[derive(Debug, Clone)]
pub struct PiperVoiceBackend {
    model_path: String,
}

impl PiperVoiceBackend {
    /// Build a backend with an explicit model path.
    #[must_use]
    pub fn new(model_path: String) -> Self {
        Self { model_path }
    }

    /// Build a backend, reading the model path from `PIPER_MODEL`.
    ///
    /// # Errors
    ///
    /// Returns [`VoiceError::NotConfigured`] if the env var is missing or
    /// empty.
    pub fn from_env() -> Result<Self, VoiceError> {
        let model_path = std::env::var("PIPER_MODEL").map_err(|_| {
            VoiceError::NotConfigured("PIPER_MODEL env var not set".to_string())
        })?;
        if model_path.trim().is_empty() {
            return Err(VoiceError::NotConfigured(
                "PIPER_MODEL env var is empty".to_string(),
            ));
        }
        Ok(Self::new(model_path))
    }

    /// The configured model path.
    #[must_use]
    pub fn model_path(&self) -> &str {
        &self.model_path
    }
}

#[async_trait::async_trait]
impl VoiceSynth for PiperVoiceBackend {
    fn name(&self) -> &'static str {
        "piper-tts"
    }

    async fn list_voices(&self) -> Result<Vec<VoiceProfile>, VoiceError> {
        Ok(vec![VoiceProfile {
            voice_id: self.model_path.clone(),
            label: self.model_path.clone(),
            language: "en-US".to_string(),
            style: VoiceStyle::Neutral,
        }])
    }

    async fn synthesize(
        &self,
        text: String,
        _voice: &VoiceProfile,
    ) -> Result<BoxedVoiceStream, VoiceError> {
        let model_path = self.model_path.clone();

        // Spawn blocking synthesis on a dedicated thread so we don't block
        // the async executor during subprocess I/O.
        let chunk = tokio::task::spawn_blocking(move || synthesize_blocking(&model_path, &text))
            .await
            .map_err(|e| VoiceError::Backend(format!("piper task panicked: {e}")))?;

        let chunk = chunk?;
        let stream = crate::single_chunk_stream(chunk);
        Ok(Box::pin(stream))
    }
}

/// Runs `piper` synchronously: writes `text` to stdin, drains stdout, returns
/// a single final [`SynthAudioChunk`].
fn synthesize_blocking(model_path: &str, text: &str) -> Result<SynthAudioChunk, VoiceError> {
    let mut child = Command::new("piper")
        .args(["--model", model_path, "--output-raw"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                VoiceError::Backend("piper not found on PATH".to_string())
            } else {
                VoiceError::Backend(format!("failed to spawn piper: {e}"))
            }
        })?;

    // Write text to stdin and close the handle so piper sees EOF.
    {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| VoiceError::Backend("could not open piper stdin".to_string()))?;
        let mut stdin = stdin;
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| VoiceError::Backend(format!("write to piper stdin failed: {e}")))?;
        // `stdin` drops here, closing the write end.
    }

    // Collect stdout (raw PCM bytes).
    let output = child
        .wait_with_output()
        .map_err(|e| VoiceError::Backend(format!("piper wait failed: {e}")))?;

    if !output.status.success() {
        return Err(VoiceError::Backend(format!(
            "piper exited with status {}",
            output.status
        )));
    }

    let raw = &output.stdout;
    let samples = decode_i16_le(raw);

    Ok(SynthAudioChunk {
        samples,
        sample_rate: PIPER_SAMPLE_RATE,
        timestamp_ms: 0,
        is_final: true,
    })
}

/// Decode raw little-endian signed 16-bit PCM into f32 samples in `[-1.0, 1.0]`.
fn decode_i16_le(raw: &[u8]) -> Vec<f32> {
    raw.chunks_exact(2)
        .map(|pair| {
            let s16 = i16::from_le_bytes([pair[0], pair[1]]);
            f32::from(s16) / 32_768.0
        })
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    use super::*;

    /// Serializes tests that mutate `PIPER_MODEL` in the process environment.
    fn env_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    fn clear_env() {
        // SAFETY: callers hold `env_lock()`.
        unsafe { std::env::remove_var("PIPER_MODEL") }
    }

    fn set_env(value: &str) {
        // SAFETY: callers hold `env_lock()`.
        unsafe { std::env::set_var("PIPER_MODEL", value) }
    }

    #[test]
    fn from_env_returns_not_configured_when_missing() {
        let _guard = env_lock();
        clear_env();
        let err = PiperVoiceBackend::from_env().expect_err("must error when env missing");
        assert!(
            matches!(err, VoiceError::NotConfigured(_)),
            "expected NotConfigured, got {err:?}"
        );
    }

    #[test]
    fn from_env_returns_not_configured_when_empty() {
        let _guard = env_lock();
        set_env("");
        let err = PiperVoiceBackend::from_env().expect_err("must error when env empty");
        assert!(
            matches!(err, VoiceError::NotConfigured(_)),
            "expected NotConfigured, got {err:?}"
        );
        clear_env();
    }

    #[test]
    fn piper_not_on_path_returns_backend_error() {
        // Patch PATH so `piper` cannot be found.
        let backend = PiperVoiceBackend::new("/some/model.onnx".to_string());

        // Run a synthesize call via the blocking helper directly so we don't
        // need a full tokio runtime in this test, and so we can control the
        // PATH.
        let result = {
            // Override PATH to an empty or nonexistent directory.
            let _guard = env_lock();
            // SAFETY: we hold env_lock and restore PATH immediately after.
            let original_path = std::env::var("PATH").unwrap_or_default();
            unsafe { std::env::set_var("PATH", "/nonexistent-path-for-tests") };
            let r = synthesize_blocking(backend.model_path(), "hello");
            unsafe { std::env::set_var("PATH", &original_path) };
            r
        };

        let err = result.expect_err("must error when piper binary is absent");
        assert!(
            matches!(err, VoiceError::Backend(ref msg) if msg.contains("piper not found on PATH")),
            "expected Backend(piper not found on PATH), got {err:?}"
        );
    }

    #[tokio::test]
    async fn list_voices_returns_model_path() {
        let model = "/models/en_US-lessac-medium.onnx".to_string();
        let backend = PiperVoiceBackend::new(model.clone());
        let voices = backend.list_voices().await.expect("list_voices");
        assert_eq!(voices.len(), 1);
        assert_eq!(voices[0].voice_id, model);
        assert_eq!(voices[0].label, model);
    }

    #[test]
    fn name_returns_piper_tts() {
        let backend = PiperVoiceBackend::new("/any/model.onnx".to_string());
        assert_eq!(backend.name(), "piper-tts");
    }

    #[test]
    fn decode_i16_le_maps_extremes() {
        // i16::MIN (-32768) → -1.0
        let raw = i16::MIN.to_le_bytes();
        let samples = decode_i16_le(&raw);
        assert!((samples[0] - (-1.0_f32)).abs() < 1e-6);

        // i16::MAX (32767) → ~0.9999…
        let raw = i16::MAX.to_le_bytes();
        let samples = decode_i16_le(&raw);
        assert!(samples[0] > 0.999 && samples[0] < 1.0);

        // 0 → 0.0
        let raw = 0_i16.to_le_bytes();
        let samples = decode_i16_le(&raw);
        assert!((samples[0] - 0.0_f32).abs() < 1e-6);
    }

    #[test]
    fn decode_i16_le_ignores_trailing_odd_byte() {
        // Three bytes → one complete i16 + one orphan byte ignored.
        let raw = [0x00_u8, 0x40_u8, 0xFF_u8]; // only [0x00, 0x40] is a full sample
        let samples = decode_i16_le(&raw);
        assert_eq!(samples.len(), 1);
    }
}
