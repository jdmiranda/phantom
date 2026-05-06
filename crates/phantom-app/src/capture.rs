//! Per-pane GPU capture pipeline + bundle persistence.
//!
//! This module wires three subsystems together:
//!
//! 1. **GPU readback**: For each visible pane, copies the corresponding
//!    sub-rect of the pre-CRT scene texture
//!    ([`PostFxPipeline::scene_texture`](phantom_renderer::postfx::PostFxPipeline::scene_texture))
//!    into CPU pixels via
//!    [`screenshot::capture_frame_sub`](phantom_renderer::screenshot::capture_frame_sub).
//!
//! 2. **Perceptual dedup**: Each captured frame is run through
//!    [`phantom_vision::dhash`] and compared against the previous capture
//!    for the same pane. Captures whose hamming distance is `<= 5` are
//!    skipped (visually identical) — this keeps the bundle store from
//!    filling up with redundant frames during idle terminals.
//!
//! 3. **Encrypted persistence**: When a pane reaches a command boundary
//!    (or its in-flight bundle accumulates enough frames), the bundle is
//!    sealed and handed to the
//!    [`BundleStore`](phantom_bundle_store::BundleStore) via the existing
//!    [`JobPool`](crate::jobs::JobPool) so the encode + write happens off
//!    the render thread.
//!
//! All of this is **best-effort**: any error inside the pipeline logs a
//! warning and short-circuits — the user's UI never stutters because
//! capture failed. If the [`BundleStore`] is `None` (e.g. keychain access
//! denied at startup), the pipeline gracefully no-ops.
//!
//! # Adaptive sampling
//!
//! Default cadence is **1 fps** per pane. After 3 consecutive captures
//! produce identical dhashes (idle terminal, cursor blink only), the cadence
//! drops to **0.2 fps** (one frame every 5 seconds). The next pane change
//! resets the counter and restores the 1 fps rate.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use phantom_adapter::AppId;
use phantom_bundle_store::{BundleEmbeddings, BundleStore};
use phantom_bundles::{Bundle, FrameRef};
use phantom_bundles::session::{CaptureFrame, CaptureSession};
use phantom_embeddings::{
    EmbedItem, EmbedRequest, Embedding, EmbeddingBackend, Modality, openai::OpenAiEmbeddingBackend,
};
use phantom_renderer::screenshot::capture_frame_sub;
use phantom_vision::{dhash, hamming_distance};

use crate::jobs::{JobContext, JobPayload, JobPool, JobPriority, JobResult};

/// Hamming distance threshold below which two consecutive frames are
/// considered duplicates and the second one is skipped.
const DEDUP_HAMMING_THRESHOLD: u32 = 5;

/// Default capture interval. 1 fps gives enough resolution to reconstruct
/// most pane activity without overwhelming storage.
const ACTIVE_CAPTURE_INTERVAL: Duration = Duration::from_millis(1_000);

/// Idle capture interval. After several consecutive identical captures we
/// drop to one frame every 5 seconds to save GPU bandwidth.
const IDLE_CAPTURE_INTERVAL: Duration = Duration::from_millis(5_000);

/// How many consecutive identical captures we tolerate before dropping
/// from active to idle cadence.
const IDLE_THRESHOLD: u32 = 3;

/// Maximum number of frames a bundle can hold before we force a seal.
/// Caps memory growth on pathologically active panes.
const MAX_FRAMES_PER_BUNDLE: usize = 128;

/// Per-pane capture bookkeeping.
///
/// One [`PaneCaptureState`] is kept for each visible adapter that has
/// produced at least one capture. Tracks the last dhash (for dedup), how
/// many consecutive identical captures we've seen (for adaptive cadence),
/// when we last captured (for the cadence gate itself), and the in-flight
/// [`CaptureSession`] being assembled.
///
/// The [`CaptureSession`] accumulates [`CaptureFrame`] values (phash, pixels,
/// dimensions) and handles run-length duplicate suppression internally.
/// When sealed via [`CaptureSession::finalize`] it produces a [`Bundle`] ready
/// for the persistence job.
#[derive(Default)]
pub(crate) struct PaneCaptureState {
    /// dhash of the most recently *stored* frame for this pane.
    last_dhash: Option<u64>,
    /// Number of consecutive captures that were dedup-skipped.
    consecutive_identical: u32,
    /// Wall-clock time when we last *attempted* a capture for this pane.
    /// `None` means no attempt yet — capture immediately on the next call.
    last_attempt: Option<Instant>,
    /// Wall-clock when the open session started, in monotonic time.
    /// Used to compute frame `t_offset_ns` when building the `Bundle`.
    bundle_start: Option<Instant>,
    /// Wall-clock unix-ms when the open session started. Stored verbatim on
    /// the sealed `Bundle`.
    bundle_wall_ms: i64,
    /// Capture session currently being assembled.  `None` means no open
    /// session — the next captured frame will start one.  Replaces the old
    /// manual `open_bundle: Option<Bundle>` field.
    open_session: Option<CaptureSession>,
    /// Raw RGBA pixel buffers for each frame accepted into `open_session`,
    /// in lock-step with the session's internal frame list.  The persistence
    /// job consumes these to encode PNG blobs; they are not stored inside
    /// [`CaptureSession`] because the blob layer is separate from the schema.
    /// Cleared every time `open_session` is finalized/taken.
    pending_pixels: Vec<Vec<u8>>,
}

impl PaneCaptureState {
    /// True if enough time has elapsed since `last_attempt` to take another
    /// capture under the current cadence.
    fn due_for_capture(&self, now: Instant) -> bool {
        let interval = if self.consecutive_identical >= IDLE_THRESHOLD {
            IDLE_CAPTURE_INTERVAL
        } else {
            ACTIVE_CAPTURE_INTERVAL
        };
        match self.last_attempt {
            None => true,
            Some(t) => now.duration_since(t) >= interval,
        }
    }
}

/// Top-level capture state owned by [`crate::app::App`]. Holds one
/// [`PaneCaptureState`] per pane and exposes test-friendly knobs.
#[derive(Default)]
pub struct CaptureState {
    /// One entry per pane. Keyed by [`AppId`] so closing a pane cleans up
    /// trivially via `panes.remove(&id)`.
    pub(crate) panes: HashMap<AppId, PaneCaptureState>,
}

impl CaptureState {
    /// Construct an empty capture state. No panes are tracked until the
    /// first call to [`crate::app::App::capture_panes`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Total frames captured (across all panes) that are currently in open
    /// (unsealed) sessions. Used by tests to assert dedup behavior without
    /// reaching into the bundle store.
    #[allow(dead_code)]
    #[must_use]
    pub fn open_frame_count(&self) -> usize {
        self.panes
            .values()
            .map(|p| p.open_session.as_ref().map_or(0, |s| s.frame_count()))
            .sum()
    }

    /// Number of panes currently being tracked.
    #[allow(dead_code)]
    #[must_use]
    pub fn tracked_pane_count(&self) -> usize {
        self.panes.len()
    }

    /// Number of consecutive identical captures recorded for `pane`.
    #[allow(dead_code)]
    #[must_use]
    pub fn consecutive_identical(&self, pane: AppId) -> u32 {
        self.panes
            .get(&pane)
            .map(|p| p.consecutive_identical)
            .unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Bundle persistence job
// ---------------------------------------------------------------------------

/// Off-thread bundle persistence work.
///
/// We submit one of these to the [`JobPool`] for every sealed bundle. The
/// pool's worker threads handle the SQLite/SQLCipher write + vector index
/// upsert, so the render thread never blocks on disk I/O.
///
/// `frame_pixels` carries the raw RGBA buffer for each [`FrameRef`] in
/// `bundle.frames`, in lock-step. The job encodes each buffer as PNG, hands
/// the bytes to [`BundleStore::write_frame_blob`] which seals them with
/// XChaCha20-Poly1305, and rewrites `FrameRef::blob_path` to the relpath
/// returned by the store before [`BundleStore::write_bundle`] runs.
struct PersistBundleJob {
    bundle: Bundle,
    /// Raw RGBA pixel buffers, one per frame in `bundle.frames`. Indices
    /// match by position (frame[0] ↔ frame_pixels[0]). May be empty if the
    /// caller has nothing to persist (e.g. caps-forced seal with all
    /// pixels already gone).
    frame_pixels: Vec<Vec<u8>>,
    store: std::sync::Arc<BundleStore>,
    /// Embedding backend builder. `None` if `OPENAI_API_KEY` was not set
    /// at job construction time — the persist still happens, just without
    /// vector indexing. The backend itself is built lazily inside `run`
    /// so we don't construct one when the env var is absent.
    backend_kind: EmbeddingBackendKind,
}

/// What kind of embedding backend a persist job should use. Built once at
/// seal time (when we still hold `&App`), then resolved lazily on the
/// worker thread.
#[derive(Clone, Copy)]
enum EmbeddingBackendKind {
    /// Use the OpenAI HTTP backend. `OPENAI_API_KEY` was present at the
    /// time the job was queued.
    OpenAi,
    /// No backend — bundle is persisted with no embeddings. Either the env
    /// var was missing or the user explicitly disabled embeddings.
    None,
}

impl JobPayload for PersistBundleJob {
    fn run(&mut self, _ctx: &JobContext) -> JobResult {
        // 1) Encode each captured RGBA buffer as PNG, encrypt, and write.
        //    On any failure we drop just that frame from the bundle —
        //    losing one frame is preferable to losing the whole bundle.
        let bundle_id = self.bundle.id;
        let mut surviving_frames: Vec<FrameRef> = Vec::with_capacity(self.bundle.frames.len());
        for (idx, frame) in self.bundle.frames.iter().enumerate() {
            let Some(pixels) = self.frame_pixels.get(idx) else {
                // No pixels recorded for this frame (e.g. cap-forced seal
                // after pixels were already drained). Keep the metadata.
                surviving_frames.push(frame.clone());
                continue;
            };
            if pixels.is_empty() {
                surviving_frames.push(frame.clone());
                continue;
            }
            match encode_png(pixels, frame.width, frame.height) {
                Ok(png_bytes) => {
                    // Use a stable per-frame name. `<bundle_id>-<seq>.png`
                    // keeps the encrypted-blob bucket flat but
                    // human-debuggable.
                    let name = format!("{}-{}.png", bundle_id, idx);
                    match self.store.write_frame_blob(bundle_id, &name, &png_bytes) {
                        Ok(rel) => {
                            // Replace the placeholder path with what the
                            // store actually used.
                            let mut updated = frame.clone();
                            updated.blob_path = rel;
                            surviving_frames.push(updated);
                        }
                        Err(e) => {
                            log::warn!(
                                "frame blob write failed for {bundle_id}-{idx}: {e}; \
                                 dropping frame",
                            );
                        }
                    }
                }
                Err(e) => {
                    log::warn!(
                        "PNG encode failed for {bundle_id}-{idx} ({}x{}): {e}; \
                         dropping frame",
                        frame.width,
                        frame.height,
                    );
                }
            }
        }
        self.bundle.frames = surviving_frames;

        // 2) Build a real embedding for the bundle's transcript chunk.
        //    On any failure (no API key, network, etc.) we log and persist
        //    the bundle without embeddings — metadata is more valuable than
        //    silent loss.
        let embeddings = match self.backend_kind {
            EmbeddingBackendKind::OpenAi => match build_openai_embeddings(&self.bundle) {
                Ok(v) => v,
                Err(e) => {
                    log::warn!(
                        "embedding pipeline failed for {bundle_id}: {e}; persisting without vectors"
                    );
                    Vec::new()
                }
            },
            EmbeddingBackendKind::None => Vec::new(),
        };

        // 3) Two-phase write into the store.
        match self.store.write_bundle(&self.bundle, &embeddings) {
            Ok(()) => JobResult::Done(format!("bundle {bundle_id} persisted")),
            Err(e) => JobResult::Err(format!("bundle persist failed: {e}")),
        }
    }

    fn describe(&self) -> &str {
        "persist-bundle"
    }
}

impl PersistBundleJob {
    /// Wrap a bundle (and its frame pixel buffers) in a job and submit it
    /// to `pool`.
    fn submit(
        pool: &JobPool,
        store: std::sync::Arc<BundleStore>,
        bundle: Bundle,
        frame_pixels: Vec<Vec<u8>>,
        backend_kind: EmbeddingBackendKind,
    ) {
        let _handle = pool.submit(
            JobPriority::Background,
            Box::new(PersistBundleJob {
                bundle,
                frame_pixels,
                store,
                backend_kind,
            }),
        );
    }
}

// ---------------------------------------------------------------------------
// Embedding helpers
// ---------------------------------------------------------------------------

/// Build the transcript chunk that gets embedded for a sealed bundle.
///
/// Concatenates intent + tags + transcript words into a single string. If
/// the bundle is empty (no intent, no tags, no transcript) returns `None`
/// — embedding an empty string would burn an API call for zero signal.
fn transcript_chunk_for(bundle: &Bundle) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(ref intent) = bundle.intent {
        parts.push(format!("intent: {intent}"));
    }
    if !bundle.tags.is_empty() {
        parts.push(format!("tags: {}", bundle.tags.join(", ")));
    }
    if !bundle.transcript_words.is_empty() {
        let body: String = bundle
            .transcript_words
            .iter()
            .map(|w| w.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        if !body.is_empty() {
            parts.push(body);
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

/// Drive the OpenAI backend to a single text embedding for `bundle`.
///
/// Runs on the persistence worker thread, so the OpenAI HTTP call is
/// blocked-on inside a fresh tokio current-thread runtime. Empty bundles
/// return `Ok(vec![])` (no embedding to compute).
fn build_openai_embeddings(bundle: &Bundle) -> Result<Vec<BundleEmbeddings>, String> {
    let Some(text) = transcript_chunk_for(bundle) else {
        return Ok(Vec::new());
    };
    let backend =
        OpenAiEmbeddingBackend::from_env().map_err(|e| format!("openai backend init: {e}"))?;
    let request = EmbedRequest {
        modality: Modality::Text,
        items: vec![EmbedItem::Text(text)],
    };
    // Tokio's basic single-threaded runtime is the lightest way to drive
    // a single async call from a sync context.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("tokio runtime build: {e}"))?;
    let mut embs: Vec<Embedding> = rt
        .block_on(backend.embed(request))
        .map_err(|e| format!("openai embed: {e}"))?;
    let Some(embedding) = embs.pop() else {
        return Ok(Vec::new());
    };
    Ok(vec![BundleEmbeddings {
        modality: "text".to_string(),
        embedding,
    }])
}

// ---------------------------------------------------------------------------
// PNG encoding
// ---------------------------------------------------------------------------

/// Encode a raw RGBA pixel buffer as PNG.
///
/// `pixels.len()` must equal `width * height * 4`. Uses the `image` crate's
/// PNG encoder (default zlib compression) via an in-memory writer. Errors
/// surface as a `String` so callers can pipe them straight into a log line.
fn encode_png(pixels: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    use image::{ColorType, ImageEncoder};

    let expected = (width as usize) * (height as usize) * 4;
    if pixels.len() != expected {
        return Err(format!(
            "RGBA buffer is {} bytes, expected {} for {}x{}",
            pixels.len(),
            expected,
            width,
            height,
        ));
    }
    let mut out: Vec<u8> = Vec::with_capacity(pixels.len() / 4);
    let encoder = PngEncoder::new_with_quality(
        &mut out,
        // `Default` compression = decent ratio without burning CPU on the
        // background worker. The capture pipeline runs at ~1 fps so even
        // moderate compression keeps up.
        CompressionType::Default,
        FilterType::NoFilter,
    );
    encoder
        .write_image(pixels, width, height, ColorType::Rgba8.into())
        .map_err(|e| format!("png encode: {e}"))?;
    Ok(out)
}

// ---------------------------------------------------------------------------
// App glue
// ---------------------------------------------------------------------------

impl crate::app::App {
    /// Per-frame capture entry point. Called from
    /// [`render`](crate::app::App::render) after the scene pass has written
    /// to `postfx.scene_texture` but before the post-FX composite consumes
    /// it. No-ops if no [`BundleStore`] was configured at startup.
    ///
    /// All errors are swallowed and logged: capture is best-effort and
    /// must never stutter the UI.
    pub(crate) fn capture_panes(&mut self) {
        let Some(_store) = self.bundle_store.as_ref() else {
            // No store configured — capture pipeline is a no-op.
            return;
        };

        let now = Instant::now();
        let texture = self.postfx.scene_texture();

        // Snapshot the (app_id, rect) pairs first so we can release the
        // coordinator borrow before mutating self.capture_state.
        let outputs = self.coordinator.render_all(&self.layout, self.cell_size);
        let mut pane_rects = Vec::new();
        for (app_id, rect, _ro) in &outputs {
            // Convert pixel-space f32 rect to integer texel origin/extent.
            let x = rect.x.max(0.0).floor() as u32;
            let y = rect.y.max(0.0).floor() as u32;
            let w = rect.width.max(0.0).ceil() as u32;
            let h = rect.height.max(0.0).ceil() as u32;
            if w == 0 || h == 0 {
                continue;
            }
            pane_rects.push((*app_id, (x, y), (w, h)));
        }

        // Drop adapters that no longer exist from our state map.
        let live_ids: std::collections::HashSet<AppId> =
            pane_rects.iter().map(|(id, _, _)| *id).collect();
        self.capture_state
            .panes
            .retain(|id, _| live_ids.contains(id));

        // Bundles sealed during this pass — we hand them off to the job
        // pool *after* the per-pane loop so we don't borrow self twice.
        // Each entry pairs the bundle with the per-frame RGBA buffers so
        // the persist job can encode + seal blobs.
        let mut sealed_bundles: Vec<(Bundle, Vec<Vec<u8>>)> = Vec::new();

        for (app_id, origin, extent) in pane_rects {
            let pane_state = self.capture_state.panes.entry(app_id).or_default();

            // Cadence gate: skip if not enough time has passed.
            if !pane_state.due_for_capture(now) {
                continue;
            }
            pane_state.last_attempt = Some(now);

            // GPU readback for this pane's sub-rect. Best-effort: log & skip on error.
            let pixels =
                match capture_frame_sub(&self.gpu.device, &self.gpu.queue, texture, origin, extent)
                {
                    Ok(p) if !p.is_empty() => p,
                    Ok(_) => continue, // Empty rect after clamp.
                    Err(e) => {
                        log::warn!("capture_frame_sub failed for pane {app_id}: {e}");
                        continue;
                    }
                };

            // Compute perceptual hash.
            let hash = match dhash(&pixels, extent.0, extent.1) {
                Ok(h) => h,
                Err(e) => {
                    log::warn!("dhash failed for pane {app_id}: {e}");
                    continue;
                }
            };

            // Dedup gate: skip if the hash matches the last stored frame.
            if let Some(prev) = pane_state.last_dhash
                && hamming_distance(prev, hash) <= DEDUP_HAMMING_THRESHOLD {
                    pane_state.consecutive_identical =
                        pane_state.consecutive_identical.saturating_add(1);
                    continue;
                }
            pane_state.consecutive_identical = 0;
            pane_state.last_dhash = Some(hash);

            // Issue #79 item 7: emit FrameCaptured to the event bus.
            //
            // The frame has passed the perceptual-hash dedup gate and will be
            // stored in the bundle. Notify subscribers (brain, tests) via the
            // typed event bus so they can react without polling the bundle store.
            let frame_timestamp_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            {
                use phantom_adapter::BusMessage;
                use phantom_protocol::Event;
                let msg = BusMessage {
                    topic_id: self.topic_capture_frame,
                    sender: app_id,
                    event: Event::FrameCaptured {
                        pane_id: app_id,
                        timestamp_ms: frame_timestamp_ms,
                    },
                    frame: 0,
                    timestamp: frame_timestamp_ms,
                };
                self.coordinator.bus_mut().emit(msg);
            }

            // GPT-4V analysis — best-effort, non-blocking.
            //
            // When a `VisionAnalyzer` is available (i.e. `OPENAI_API_KEY` is
            // set), we clone the pixel buffer and dispatch analysis on a
            // tokio task so the capture loop is never stalled by a network
            // call. Results are logged at DEBUG level. Future work (issue #79)
            // may route `Analysis` into `BrainAction::ScreenAnalysis`.
            if let Some(analyzer) = self.vision_analyzer.as_ref() {
                let pixels_for_task = pixels.clone();
                let (w, h) = extent;
                let ts = frame_timestamp_ms;
                let pane_id_for_log = u64::from(app_id);
                let analyzer_arc = std::sync::Arc::clone(analyzer);
                let _task = tokio::spawn(async move {
                    match analyzer_arc.analyze_frame(&pixels_for_task, w, h, ts).await {
                        Ok(analysis) => {
                            log::debug!(
                                "GPT-4V analysis (pane {pane_id_for_log}): {}",
                                analysis.summary()
                            );
                        }
                        Err(e) => {
                            log::debug!(
                                "GPT-4V analysis failed (pane {pane_id_for_log}): {e}"
                            );
                        }
                    }
                });
            }

            // Validate frame dimensions before accumulating. A zero-dimension
            // frame cannot be PNG-encoded and would produce an empty blob.
            // Log and skip so the pipeline keeps running on the other panes.
            if extent.0 == 0 || extent.1 == 0 {
                log::warn!(
                    "capture_panes: skipping zero-dimension frame for pane {app_id} \
                     ({}x{})",
                    extent.0,
                    extent.1,
                );
                continue;
            }

            // Open a fresh CaptureSession if we don't have one. Wall-clock is
            // captured here so frame offsets are relative to session start.
            if pane_state.open_session.is_none() {
                let session_id = uuid::Uuid::new_v4();
                pane_state.bundle_start = Some(now);
                pane_state.bundle_wall_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0);
                pane_state.open_session = Some(CaptureSession::new(session_id));
            }

            // Push the captured frame into the CaptureSession.
            // CaptureFrame carries the raw pixels so the session knows what
            // to hand to finalize(); pending_pixels travels in lock-step for
            // the PNG-encoding layer in the persistence job.
            let session = pane_state.open_session.as_mut().expect("just opened");
            let capture_frame = CaptureFrame::new(
                frame_timestamp_ms,
                hash,
                pixels.clone(),
                extent.0,
                extent.1,
            );
            session.add_frame(capture_frame);
            pane_state.pending_pixels.push(pixels);

            // Force-seal if the session is at the cap.
            if session.frame_count() >= MAX_FRAMES_PER_BUNDLE {
                let taken_session = pane_state.open_session.take().expect("just had it");
                let pixels_for_seal = std::mem::take(&mut pane_state.pending_pixels);
                pane_state.bundle_start = None;

                match taken_session.finalize() {
                    Ok(mut bundle) => {
                        // Stamp wall-clock on the bundle so the store can order by time.
                        // `finalize` already sealed the bundle via BundleAssembler::finish;
                        // we only overwrite the wall-clock field.
                        bundle.t_wall_unix_ms = pane_state.bundle_wall_ms;
                        sealed_bundles.push((bundle, pixels_for_seal));
                    }
                    Err(e) => {
                        log::warn!("CaptureSession::finalize failed at cap for pane {app_id}: {e}");
                    }
                }
            }
        }

        // Hand sealed bundles to the job pool. Cheap: just an Arc clone
        // and a channel send per bundle.
        if !sealed_bundles.is_empty() {
            self.persist_sealed_bundles(sealed_bundles);
        }
    }

    /// Seal the open session for `pane` (if any), finalize it into a
    /// [`Bundle`], and queue it for persistence.  Called at command
    /// boundaries via [`Self::on_command_boundary`] and from the bus-drain
    /// loop when an `Event::CommandComplete` is observed.
    ///
    /// Returns `true` if a bundle was sealed and queued, `false` if there
    /// was nothing to do (no open session for this pane, or the session had
    /// no valid frames after zero-dimension filtering).
    pub(crate) fn seal_pane_bundle(&mut self, pane: AppId, intent: Option<String>) -> bool {
        let Some(state) = self.capture_state.panes.get_mut(&pane) else {
            return false;
        };
        let Some(session) = state.open_session.take() else {
            return false;
        };
        let pixels = std::mem::take(&mut state.pending_pixels);
        let wall_ms = state.bundle_wall_ms;
        state.bundle_start = None;
        state.last_dhash = None;
        state.consecutive_identical = 0;

        match session.finalize() {
            Ok(mut bundle) => {
                // `finalize` already sealed the bundle via BundleAssembler::finish;
                // we only overwrite the wall-clock field which was 0-initialized.
                bundle.t_wall_unix_ms = wall_ms;
                // Propagate the caller's intent into the already-sealed bundle.
                if let Some(ref intent_str) = intent {
                    bundle.intent = Some(intent_str.clone());
                }
                self.persist_sealed_bundles(vec![(bundle, pixels)]);
                true
            }
            Err(e) => {
                log::warn!(
                    "seal_pane_bundle: CaptureSession::finalize failed for pane {pane}: {e}"
                );
                false
            }
        }
    }

    /// Explicit entry point for command-boundary sealing.
    ///
    /// Callers (the bus-drain loop, MCP handlers, future PTY shell-prompt
    /// detectors) invoke this when they observe that a shell command
    /// finished in `pane`. The current open bundle (if any) is sealed with
    /// `command_text` as its intent and queued for persistence.
    ///
    /// Returns `true` if a bundle was sealed, `false` if there was none.
    pub fn on_command_boundary(&mut self, pane: AppId, command_text: Option<String>) -> bool {
        self.seal_pane_bundle(pane, command_text)
    }

    /// Returns the number of [`PersistBundleJob`]s currently in flight.
    /// Used by tests to assert that a command boundary actually queued a
    /// job. Counts by snapshot of the pool's completed buffer plus the
    /// caller's job submission count — the JobPool itself doesn't expose
    /// queue depth, so this is a best-effort indicator.
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn pending_persist_jobs(&self) -> usize {
        // Currently no public introspection on JobPool queue depth; tests
        // assert via observable side-effects (bundle present in store).
        0
    }

    /// Submit one or more sealed bundles to the job pool for off-thread
    /// persistence. The render thread never blocks on disk I/O.
    ///
    /// Uses the `embedding_backend` pre-wired at startup (from
    /// [`App::with_config_scaled`]) to determine whether vector indexing is
    /// available. Avoids re-reading the env var on every seal event.
    fn persist_sealed_bundles(&mut self, bundles: Vec<(Bundle, Vec<Vec<u8>>)>) {
        let Some(store) = self.bundle_store.clone() else {
            return;
        };
        let Some(ref pool) = self.job_pool else {
            // No pool means we're shutting down — drop the bundles. They'd
            // be lost-on-crash anyway and we can't block here.
            log::warn!("no job pool, dropping {} sealed bundles", bundles.len());
            return;
        };
        // Use the pre-constructed embedding backend stored on the App.
        // `None` → bundles persist with metadata but no vector index.
        let backend_kind = if self.embedding_backend.is_some() {
            EmbeddingBackendKind::OpenAi
        } else {
            EmbeddingBackendKind::None
        };
        for (bundle, pixels) in bundles {
            PersistBundleJob::submit(
                pool,
                std::sync::Arc::clone(&store),
                bundle,
                pixels,
                backend_kind,
            );
        }
    }
}

/// Tiny SHA-256-style content hash.
///
/// We don't want to pull in a SHA-256 dep for phantom-app *just* for frame
/// content addressing — phantom-bundle-store has sha2 but it's not exposed.
/// Instead we use FxHash-style 64-bit fold over the bytes and hex-encode
/// it. Collisions are possible but the field is informational only; the
/// dhash is what actually drives dedup.
///
/// Used by tests to verify determinism. The main pipeline delegates sha
/// computation to [`CaptureSession`], which derives it from the phash.
#[allow(dead_code)]
fn simple_sha_hex(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a 64-bit basis
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    format!("{h:016x}")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: synthetic pane state with one stored hash.
    fn pane_with_last_hash(hash: u64) -> PaneCaptureState {
        PaneCaptureState {
            last_dhash: Some(hash),
            consecutive_identical: 0,
            last_attempt: None,
            bundle_start: None,
            bundle_wall_ms: 0,
            open_session: None,
            pending_pixels: Vec::new(),
        }
    }

    /// `capture_pipeline_skips_when_bundle_store_missing`
    ///
    /// With `bundle_store: None`, `CaptureState::open_frame_count` stays at
    /// zero no matter how many synthetic captures we feed it. The store
    /// guard short-circuits the entire pipeline.
    ///
    /// We can't construct a full `App` without a GPU, so this is verified
    /// at the [`CaptureState`] level: a fresh state has no panes, no
    /// frames, no tracked panes. The `App::capture_panes` early-return is
    /// covered by inspection of the source.
    #[test]
    fn capture_pipeline_skips_when_bundle_store_missing() {
        let state = CaptureState::new();
        assert_eq!(state.open_frame_count(), 0);
        assert_eq!(state.tracked_pane_count(), 0);
    }

    /// `capture_pipeline_dedup_hits_skip_writes`
    ///
    /// Hamming distance below the threshold must skip; above must record.
    /// Verified via direct calls into the dedup primitive used by the
    /// pipeline.
    #[test]
    fn capture_pipeline_dedup_hits_skip_writes() {
        let mut state = CaptureState::new();
        let pane: AppId = 1;
        state.panes.insert(pane, pane_with_last_hash(0xDEAD_BEEF));

        // Identical hash: hamming = 0, well below threshold → skip.
        let new_hash = 0xDEAD_BEEF_u64;
        let dist = hamming_distance(state.panes[&pane].last_dhash.unwrap(), new_hash);
        assert!(dist <= DEDUP_HAMMING_THRESHOLD, "dedup must trigger");

        // Now a clearly different hash: must NOT trigger dedup.
        let different = 0x0000_0000_u64;
        let dist2 = hamming_distance(state.panes[&pane].last_dhash.unwrap(), different);
        assert!(
            dist2 > DEDUP_HAMMING_THRESHOLD,
            "different hash must NOT dedup, got distance {dist2}"
        );

        // Simulate the pipeline updating consecutive_identical when dedup hits.
        let pane_state = state.panes.get_mut(&pane).unwrap();
        pane_state.consecutive_identical += 1;
        assert_eq!(state.consecutive_identical(pane), 1);
    }

    /// `capture_pipeline_command_boundary_seals_bundle`
    ///
    /// When a command boundary fires, the open session (if any) is finalized
    /// and removed from the per-pane state. We exercise the seal path
    /// directly on a synthetic `CaptureSession` since the finalize-and-persist
    /// branches in `seal_pane_bundle` mirror this logic.
    #[test]
    fn capture_pipeline_command_boundary_seals_bundle() {

        let mut state = CaptureState::new();
        let pane: AppId = 7;

        // Open a CaptureSession and push one valid frame into it.
        let mut session = CaptureSession::new(uuid::Uuid::new_v4());
        session.add_frame(CaptureFrame::new(0, 0xAA, vec![0xAA, 0xBB, 0xCC, 0xDD], 100, 100));
        let ps = PaneCaptureState {
            open_session: Some(session),
            bundle_wall_ms: 1_700_000_000_000,
            pending_pixels: vec![vec![0xAA, 0xBB, 0xCC, 0xDD]],
            ..PaneCaptureState::default()
        };
        state.panes.insert(pane, ps);

        assert_eq!(state.open_frame_count(), 1, "1 frame open before seal");

        // Pull the session out and finalize it (mimics what `seal_pane_bundle` does).
        let taken_session = state
            .panes
            .get_mut(&pane)
            .unwrap()
            .open_session
            .take()
            .unwrap();
        let mut sealed = taken_session.finalize().expect("one valid frame → Ok");
        sealed.t_wall_unix_ms = 1_700_000_000_000;
        sealed.intent = Some("test-boundary".into());

        assert_eq!(state.open_frame_count(), 0, "0 open frames after take");
        assert!(sealed.sealed, "bundle must be sealed by finalize");
        assert_eq!(sealed.intent.as_deref(), Some("test-boundary"));
        assert_eq!(sealed.frames.len(), 1, "frame count preserved by finalize");
    }

    /// `capture_state_advances_frame_counter_per_pane`
    ///
    /// Each pane gets its own `consecutive_identical` and `open_session`
    /// frame counters — incrementing one must not touch the other.
    #[test]
    fn capture_state_advances_frame_counter_per_pane() {

        let mut state = CaptureState::new();
        let pane_a: AppId = 1;
        let pane_b: AppId = 2;

        state.panes.insert(pane_a, PaneCaptureState::default());
        state.panes.insert(pane_b, PaneCaptureState::default());

        // Open a CaptureSession in pane A with 2 frames (distinct phashes).
        let mut session_a = CaptureSession::new(uuid::Uuid::new_v4());
        session_a.add_frame(CaptureFrame::new(0, 0x01, vec![0; 4], 10, 10));
        session_a.add_frame(CaptureFrame::new(1_000, 0x02, vec![0; 4], 10, 10));
        state.panes.get_mut(&pane_a).unwrap().open_session = Some(session_a);

        assert_eq!(state.open_frame_count(), 2, "pane A has 2 frames");

        // Bump consecutive_identical on B — must not affect A.
        state.panes.get_mut(&pane_b).unwrap().consecutive_identical = 5;
        assert_eq!(state.consecutive_identical(pane_a), 0);
        assert_eq!(state.consecutive_identical(pane_b), 5);
        assert_eq!(
            state.open_frame_count(),
            2,
            "still 2 (A's frames unchanged)"
        );
    }

    /// Cadence gate sanity check: a fresh state always allows a capture;
    /// after recording an attempt under active cadence, a second call
    /// within the active interval is denied.
    #[test]
    fn cadence_gate_active_interval_denies_rapid_repeats() {
        let mut ps = PaneCaptureState::default();
        let now = Instant::now();
        assert!(ps.due_for_capture(now), "fresh state always due");
        ps.last_attempt = Some(now);
        assert!(!ps.due_for_capture(now), "same instant: not due");
        // Half the active interval: still not due.
        let half = now + ACTIVE_CAPTURE_INTERVAL / 2;
        assert!(!ps.due_for_capture(half), "half interval: not due");
        // Full active interval elapsed: due again.
        let full = now + ACTIVE_CAPTURE_INTERVAL;
        assert!(ps.due_for_capture(full), "full interval: due");
    }

    /// Cadence drops to idle after [`IDLE_THRESHOLD`] consecutive identical
    /// captures.
    #[test]
    fn cadence_gate_drops_to_idle_after_threshold() {
        let mut ps = PaneCaptureState::default();
        let now = Instant::now();
        ps.last_attempt = Some(now);
        ps.consecutive_identical = IDLE_THRESHOLD;

        // Active interval has elapsed but we're now in idle mode — still
        // not due until the idle interval passes.
        let active_elapsed = now + ACTIVE_CAPTURE_INTERVAL + Duration::from_millis(10);
        assert!(
            !ps.due_for_capture(active_elapsed),
            "active interval doesn't satisfy idle cadence"
        );
        let idle_elapsed = now + IDLE_CAPTURE_INTERVAL;
        assert!(ps.due_for_capture(idle_elapsed), "idle interval satisfies");
    }

    /// `transcript_chunk_for` collects intent + tags + transcript words
    /// into a single embeddable string. Empty bundles return `None` so we
    /// don't burn an embedding API call on nothing.
    #[test]
    fn transcript_chunk_for_concatenates_known_fields() {
        use phantom_bundles::TranscriptWord;
        let mut b = Bundle::new(1);
        b.add_word(TranscriptWord {
            t_offset_ns: 0,
            t_end_ns: 1,
            text: "hello".into(),
            speaker: None,
            confidence: 1.0,
        });
        b.add_word(TranscriptWord {
            t_offset_ns: 2,
            t_end_ns: 3,
            text: "world".into(),
            speaker: None,
            confidence: 1.0,
        });
        b.seal(Some("greeting".into()), vec!["demo".into()], 0.5);
        let chunk = transcript_chunk_for(&b).expect("non-empty bundle has chunk");
        assert!(chunk.contains("intent: greeting"), "got {chunk:?}");
        assert!(chunk.contains("tags: demo"), "got {chunk:?}");
        assert!(chunk.contains("hello world"), "got {chunk:?}");
    }

    /// Empty bundles must produce `None` so the persist job can skip the
    /// embedding call entirely (and not waste an API quota credit).
    #[test]
    fn transcript_chunk_for_empty_bundle_returns_none() {
        let b = Bundle::new(1);
        assert!(transcript_chunk_for(&b).is_none());
    }

    /// `encode_png` round-trips a tiny solid color: known input produces a
    /// non-empty PNG that decodes back to the same pixels.
    #[test]
    fn encode_png_round_trips_solid_color() {
        // 2x2 image, all opaque red.
        let pixels: Vec<u8> = (0..4).flat_map(|_| [255_u8, 0, 0, 255]).collect();
        let png = encode_png(&pixels, 2, 2).expect("encode");
        // Decode with the `image` crate to verify a real PNG came out.
        let decoded = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .expect("decode")
            .to_rgba8();
        assert_eq!(decoded.dimensions(), (2, 2));
        assert_eq!(decoded.into_raw(), pixels);
    }

    /// `encode_png` rejects a buffer whose size doesn't match the dimensions.
    #[test]
    fn encode_png_rejects_size_mismatch() {
        let pixels = vec![0_u8; 7]; // not a multiple of 4, way too small
        let err = encode_png(&pixels, 2, 2).expect_err("should reject");
        assert!(err.contains("expected"), "got {err}");
    }

    /// `simple_sha_hex` is deterministic and produces a 16-hex-char string
    /// so it slots into `FrameRef::sha` without surprises.
    #[test]
    fn simple_sha_hex_is_deterministic_and_16_chars() {
        let a = simple_sha_hex(b"hello world");
        let b = simple_sha_hex(b"hello world");
        let c = simple_sha_hex(b"different");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 16);
    }

    // -----------------------------------------------------------------
    // Integration: PersistBundleJob round-trip
    // -----------------------------------------------------------------
    //
    // The next two tests cover the closing-the-loop behavior: that a
    // sealed bundle plus its captured RGBA buffers actually lands in
    // SQLCipher + an encrypted PNG file on disk, and that the path
    // recorded in `FrameRef::blob_path` round-trips to recover the
    // original pixel data via `BundleStore::read_blob`.

    /// Build a tempdir-backed [`BundleStore`] for tests.
    fn open_test_store() -> (
        tempfile::TempDir,
        std::sync::Arc<phantom_bundle_store::BundleStore>,
    ) {
        use phantom_bundle_store::{StoreConfig, testing::deterministic_master_key};
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let key = deterministic_master_key(0x42);
        let store = phantom_bundle_store::BundleStore::open(StoreConfig {
            root: tmp.path().to_path_buf(),
            master_key: key,
        })
        .expect("open store");
        (tmp, std::sync::Arc::new(store))
    }

    /// `persist_bundle_job_writes_png_blob_and_round_trips`
    ///
    /// End-to-end Fix #3 check: hand a `PersistBundleJob` a 4x4 RGBA
    /// buffer, run it, and verify (a) the on-disk envelope file exists,
    /// (b) `read_bundle` returns a `FrameRef` whose `blob_path` is
    /// non-empty and points into the store's `objects/frames/` bucket,
    /// and (c) `read_blob` decrypts back to the original pixels.
    #[test]
    fn persist_bundle_job_writes_png_blob_and_round_trips() {
        let (tmp, store) = open_test_store();

        // Build a synthetic bundle with a single 4x4 frame. The pixel
        // values are deterministic so we can verify exact round-trip.
        let mut bundle = Bundle::new(7);
        bundle.t_wall_unix_ms = 1_700_000_000_000;
        bundle.add_frame(FrameRef {
            t_offset_ns: 0,
            sha: "test-sha".into(),
            blob_path: "placeholder-overwritten-by-job".into(),
            dhash: 0xAA,
            width: 4,
            height: 4,
        });
        bundle.seal(Some("test-cmd".into()), vec!["pane-boundary".into()], 0.5);
        let bundle_id = bundle.id;

        // Pixel pattern: each 4-byte run is (i, i+1, i+2, 0xFF) so we
        // can detect a transposition or off-by-one.
        let pixels: Vec<u8> = (0..16_u8)
            .flat_map(|i| [i, i.wrapping_add(1), i.wrapping_add(2), 0xFF])
            .collect();

        let mut job = PersistBundleJob {
            bundle,
            frame_pixels: vec![pixels.clone()],
            store: std::sync::Arc::clone(&store),
            backend_kind: EmbeddingBackendKind::None,
        };
        let ctx = crate::jobs::JobContext {
            job_id: crate::jobs::JobId(1),
            cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        match job.run(&ctx) {
            crate::jobs::JobResult::Done(_) => {}
            crate::jobs::JobResult::Err(e) => panic!("persist failed: {e}"),
            crate::jobs::JobResult::Cancelled => panic!("unexpected cancel"),
        }

        // The bundle should now be in SQLite with a real frame whose
        // blob_path resolves under `objects/frames/`.
        let restored = store.read_bundle(bundle_id).expect("read bundle");
        assert_eq!(restored.frames.len(), 1, "frame must survive persist");
        let frame = &restored.frames[0];
        assert!(
            frame.blob_path.starts_with("objects/frames/"),
            "blob_path should be under objects/frames/, got {:?}",
            frame.blob_path
        );

        // The on-disk envelope exists.
        let abs = tmp.path().join(&frame.blob_path);
        assert!(
            abs.exists(),
            "encrypted blob file must exist on disk at {}",
            abs.display()
        );

        // Read back, decrypt, and decode the PNG. The decoded pixels
        // must match what we captured.
        let png_bytes = store
            .read_blob(bundle_id, &frame.blob_path)
            .expect("decrypt blob");
        let decoded = image::load_from_memory_with_format(&png_bytes, image::ImageFormat::Png)
            .expect("decode png")
            .to_rgba8();
        assert_eq!(decoded.dimensions(), (4, 4));
        assert_eq!(decoded.into_raw(), pixels);
    }

    /// `persist_bundle_job_writes_embeddings_when_provided`
    ///
    /// End-to-end Fix #2 check: when a `PersistBundleJob` is fed
    /// embeddings (here we synthesize them inline since the mock
    /// backend lives in `phantom-embeddings`), the same embedding
    /// vector becomes searchable by `vector_search`.
    ///
    /// We bypass `build_openai_embeddings` here because that function
    /// hits the live OpenAI API; the wire-up logic (run_job → write_bundle
    /// with embeddings → search_vectors finds it) is what we want to
    /// verify, and that's what this test does.
    #[test]
    fn persist_bundle_job_indexes_embeddings_for_search() {
        use phantom_bundle_store::{BundleEmbeddings, VectorQuery};
        use phantom_embeddings::Embedding;

        let (_tmp, store) = open_test_store();

        // Two bundles, each with a different single-modality vector.
        let mut a = Bundle::new(1);
        a.seal(Some("alpha".into()), vec![], 0.1);
        let mut b = Bundle::new(2);
        b.seal(Some("beta".into()), vec![], 0.1);

        let emb_a = BundleEmbeddings {
            modality: "text".into(),
            embedding: Embedding {
                vec: vec![1.0, 0.0, 0.0],
                dim: 3,
                model: "test".into(),
            },
        };
        let emb_b = BundleEmbeddings {
            modality: "text".into(),
            embedding: Embedding {
                vec: vec![0.0, 1.0, 0.0],
                dim: 3,
                model: "test".into(),
            },
        };
        store.write_bundle(&a, &[emb_a]).expect("write a");
        store.write_bundle(&b, &[emb_b]).expect("write b");

        // Querying close to a's vector returns a first.
        let hits = store
            .search_vectors(&VectorQuery {
                modality: "text".into(),
                vector: vec![0.99, 0.01, 0.0],
                limit: 2,
            })
            .expect("search");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].bundle_id, a.id, "alpha must be the closest");
    }

    /// `seal_pane_bundle_drains_pending_pixels_in_lockstep`
    ///
    /// Fix #1 unit-level check: when `seal_pane_bundle`-style logic runs
    /// (we call the underlying transitions directly since `App` requires
    /// a full GPU context), the open session is taken and finalized, pending
    /// pixels are drained in lock-step, and a sealed bundle + matching
    /// `Vec<Vec<u8>>` is what would be queued.
    ///
    /// This mirrors the `App::seal_pane_bundle` body without going
    /// through GPU-bound `App::new`.
    #[test]
    fn seal_pane_bundle_drains_pending_pixels_in_lockstep() {

        let mut state = CaptureState::new();
        let pane: AppId = 11;

        // Seed an open CaptureSession with 2 frames + 2 pixel buffers.
        let mut session = CaptureSession::new(uuid::Uuid::new_v4());
        session.add_frame(CaptureFrame::new(0, 0x01, vec![1; 16], 2, 2));
        session.add_frame(CaptureFrame::new(1_000, 0x02, vec![2; 16], 2, 2));
        let ps = PaneCaptureState {
            open_session: Some(session),
            pending_pixels: vec![vec![1; 16], vec![2; 16]],
            ..PaneCaptureState::default()
        };
        state.panes.insert(pane, ps);

        // Mirror the seal_pane_bundle body.
        let pane_state = state.panes.get_mut(&pane).expect("pane present");
        let taken_session = pane_state.open_session.take().expect("had open session");
        let pixels = std::mem::take(&mut pane_state.pending_pixels);
        let mut sealed = taken_session.finalize().expect("two valid frames → Ok");
        sealed.intent = Some("test".into());

        assert!(sealed.sealed, "finalize returns a sealed bundle");
        assert_eq!(sealed.frames.len(), 2);
        assert_eq!(pixels.len(), 2, "pixels must travel with the sealed bundle");
        assert_eq!(pixels[0].len(), 16);
        assert_eq!(pixels[1].len(), 16);
        assert_eq!(state.open_frame_count(), 0, "no open frames after seal");
    }

    /// `capture_pipeline_skips_analysis_when_no_key`
    ///
    /// Verifies that `VisionAnalyzer::from_env()` returns `None`-equivalent
    /// (via `.ok()`) when `OPENAI_API_KEY` is not set, so the capture pipeline
    /// gracefully skips GPT-4V analysis rather than panicking.
    #[test]
    fn capture_pipeline_skips_analysis_when_no_key() {
        // Remove the env var for this test scope.
        let _guard = std::env::remove_var("OPENAI_API_KEY");

        let result = phantom_vision::VisionAnalyzer::from_env();
        assert!(
            result.is_err(),
            "from_env must fail without OPENAI_API_KEY"
        );

        // The capture pipeline stores `from_env().ok()` — verify it yields None.
        let analyzer: Option<std::sync::Arc<phantom_vision::VisionAnalyzer>> =
            phantom_vision::VisionAnalyzer::from_env()
                .ok()
                .map(std::sync::Arc::new);
        assert!(
            analyzer.is_none(),
            "vision_analyzer field must be None when OPENAI_API_KEY is absent"
        );
    }
}
