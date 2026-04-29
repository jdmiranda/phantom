//! Issue #91 — Capture pipeline end-to-end smoke tests.
//!
//! Exercises all four layers of the capture pipeline in sequence:
//!
//! 1. [`dedup_identical_frames_detected`] — two byte-identical RGBA buffers
//!    hash to the same dHash and hamming distance 0; the fast SAD gate also
//!    returns 0 for identical input.
//! 2. [`bundle_assembler_round_trip`] — `BundleAssembler` produces a sealed
//!    `Bundle` that survives a JSON serialize → deserialize cycle with all
//!    fields intact.
//! 3. [`store_insert_and_retrieve_by_id`] — a `Bundle` written to
//!    `BundleStore` can be retrieved by its UUID; all scalar and collection
//!    fields match the original.
//! 4. [`embedding_self_similarity_is_one`] — a vector inserted into
//!    `InMemoryStore` and queried with itself scores ≈ 1.0 (cosine similarity
//!    of a vector with itself is exactly 1.0 for any non-zero vector).
//!
//! All tests are hermetic: the vision and embedding tests use only in-memory
//! data; the store tests use `tempfile::TempDir` for ephemeral SQLite files.

use phantom_bundle_store::{
    BundleEmbeddings,
    testing::{deterministic_master_key, open_at},
};
use phantom_bundles::{AudioRef, FrameRef, TranscriptWord, assembler::BundleAssembler};
use phantom_embeddings::{
    Embedding, cosine_similarity,
    store::{EmbeddingStore, InMemoryStore},
};
use phantom_vision::{dhash, downsample_to_64x64_gray, fast_diff_gate, hamming_distance};
use tempfile::TempDir;

// ── helpers ───────────────────────────────────────────────────────────────────

/// Build an RGBA buffer of `width × height` pixels filled with a single color.
fn solid_rgba(width: u32, height: u32, r: u8, g: u8, b: u8) -> Vec<u8> {
    let n = (width as usize) * (height as usize);
    let mut buf = Vec::with_capacity(n * 4);
    for _ in 0..n {
        buf.extend_from_slice(&[r, g, b, 255]);
    }
    buf
}

/// Construct an [`Embedding`] from a plain `Vec<f32>`.
fn embedding(vec: Vec<f32>) -> Embedding {
    let dim = vec.len();
    Embedding {
        vec,
        dim,
        model: "test".into(),
    }
}

/// Open a fresh [`BundleStore`] in a temporary directory.
fn open_tmp() -> (TempDir, phantom_bundle_store::BundleStore) {
    let tmp = TempDir::new().expect("tempdir");
    let key = deterministic_master_key(0xDE);
    let store = open_at(tmp.path(), key).expect("open store");
    (tmp, store)
}

// ── 1. Dedup smoke test ───────────────────────────────────────────────────────

/// Two identical RGBA frames must produce the same dHash (hamming distance 0)
/// and a SAD of 0 through the fast diff gate.
///
/// The "duplicate detected → store only one" enforcement is intentionally left
/// to the caller: phantom-vision is a pure computation module with no state.
/// What we verify here is that the primitives correctly identify a duplicate.
#[test]
fn dedup_identical_frames_detected() {
    // Two independently allocated buffers with the same pixel content.
    let frame_a = solid_rgba(128, 128, 42, 100, 200);
    let frame_b = solid_rgba(128, 128, 42, 100, 200);

    // --- dHash path ---
    let hash_a = dhash(&frame_a, 128, 128).expect("dhash a");
    let hash_b = dhash(&frame_b, 128, 128).expect("dhash b");

    // Identical content must hash identically.
    assert_eq!(
        hash_a, hash_b,
        "identical frames must produce the same dHash"
    );

    // Hamming distance 0 → confirmed duplicate.
    let dist = hamming_distance(hash_a, hash_b);
    assert_eq!(dist, 0, "hamming distance for identical frames must be 0");
    assert!(dist <= 5, "threshold check: duplicate if dist <= 5");

    // --- fast SAD gate path ---
    let reference = downsample_to_64x64_gray(&frame_a, 128, 128).expect("downsample reference");
    let sad = fast_diff_gate(&frame_b, &reference, 128, 128).expect("fast_diff_gate");

    assert_eq!(sad, 0, "SAD for identical frames must be 0");

    // Confirm that a distinctly different frame scores much higher on the gate.
    // White (255,255,255) vs the reference color (42,100,200): luma difference
    // per pixel is ~161 over the 64×64 = 4096 cell grid → SAD ≈ 659_456.
    // Any significantly different solid color should produce SAD > 100_000.
    let frame_white = solid_rgba(128, 128, 255, 255, 255);
    let sad_diff =
        fast_diff_gate(&frame_white, &reference, 128, 128).expect("fast_diff_gate different");
    assert!(
        sad_diff > 100_000,
        "SAD for very different frames must be substantially above zero (got {sad_diff})"
    );
}

// ── 2. Bundle round-trip ──────────────────────────────────────────────────────

/// `BundleAssembler` → sealed `Bundle` → JSON → deserialize; all fields match.
#[test]
fn bundle_assembler_round_trip() {
    let pane_id = 99_u64;
    let mut asm = BundleAssembler::new(pane_id);
    asm.set_start_ns(5_000_000_000);

    asm.push_frame(FrameRef {
        t_offset_ns: 0,
        sha: "cafebabe".into(),
        blob_path: "frames/0.png".into(),
        dhash: 0xDEAD_BEEF,
        width: 1920,
        height: 1080,
    });
    asm.push_frame(FrameRef {
        t_offset_ns: 33_000_000,
        sha: "deadbeef".into(),
        blob_path: "frames/1.png".into(),
        dhash: 0xCAFE_BABE,
        width: 1920,
        height: 1080,
    });
    asm.push_audio(AudioRef {
        t_offset_ns: 0,
        duration_ns: 20_000_000,
        blob_path: "audio/0.opus".into(),
        sample_rate: 48_000,
        channels: 2,
    });
    asm.push_word(TranscriptWord {
        t_offset_ns: 0,
        t_end_ns: 500_000_000,
        text: "cargo".into(),
        speaker: Some("user".into()),
        confidence: 0.97,
    });
    asm.push_word(TranscriptWord {
        t_offset_ns: 500_000_001,
        t_end_ns: 1_000_000_000,
        text: "test".into(),
        speaker: Some("user".into()),
        confidence: 0.95,
    });

    let original = asm
        .finish(
            Some("ci-check".into()),
            vec!["rust".into(), "smoke".into()],
            0.88,
        )
        .expect("assembler finish");

    // Basic invariants on the sealed bundle.
    assert!(original.sealed, "bundle must be sealed after finish");
    assert_eq!(original.source_pane_id, pane_id);
    assert_eq!(original.t_start_ns, 5_000_000_000);
    assert_eq!(original.frames.len(), 2);
    assert_eq!(original.audio_chunks.len(), 1);
    assert_eq!(original.transcript_words.len(), 2);

    // Serialize → deserialize.
    let json = serde_json::to_string(&original).expect("serialize");
    let restored: phantom_bundles::Bundle = serde_json::from_str(&json).expect("deserialize");

    // Field-level equality across the round trip.
    assert_eq!(restored.id, original.id, "id must survive round-trip");
    assert_eq!(restored.source_pane_id, original.source_pane_id, "pane_id");
    assert_eq!(restored.t_start_ns, original.t_start_ns, "t_start_ns");
    assert_eq!(restored.frames.len(), 2, "frame count");
    assert_eq!(restored.frames[0].sha, "cafebabe", "frame[0].sha");
    assert_eq!(
        restored.frames[1].t_offset_ns, 33_000_000,
        "frame[1].t_offset_ns"
    );
    assert_eq!(restored.frames[1].dhash, 0xCAFE_BABE_u64, "frame[1].dhash");
    assert_eq!(restored.audio_chunks.len(), 1, "audio count");
    assert_eq!(restored.audio_chunks[0].sample_rate, 48_000, "sample_rate");
    assert_eq!(restored.transcript_words.len(), 2, "word count");
    assert_eq!(restored.transcript_words[0].text, "cargo", "word[0]");
    assert_eq!(restored.transcript_words[1].text, "test", "word[1]");
    assert_eq!(restored.intent.as_deref(), Some("ci-check"), "intent");
    assert_eq!(restored.tags, vec!["rust", "smoke"], "tags");
    assert!((restored.importance - 0.88).abs() < 1e-5, "importance");
    assert!(restored.sealed, "sealed");
    assert_eq!(
        restored.schema_version,
        phantom_bundles::SCHEMA_VERSION,
        "schema_version"
    );
}

// ── 3. Store + retrieve ───────────────────────────────────────────────────────

/// Insert a bundle into `BundleStore` and retrieve it by its UUID.
/// All scalar and collection fields must match the original.
#[test]
fn store_insert_and_retrieve_by_id() {
    let (_tmp, store) = open_tmp();

    // Build a rich, sealed bundle.
    let mut bundle = phantom_bundles::Bundle::new(42_u64);
    bundle.t_start_ns = 1_000_000;
    bundle.t_wall_unix_ms = 1_700_000_000_000;
    bundle.add_frame(FrameRef {
        t_offset_ns: 0,
        sha: "sha-frame-0".into(),
        blob_path: "frames/0.png".into(),
        dhash: 0,
        width: 1280,
        height: 720,
    });
    bundle.add_frame(FrameRef {
        t_offset_ns: 16_666_666,
        sha: "sha-frame-1".into(),
        blob_path: "frames/1.png".into(),
        dhash: 1,
        width: 1280,
        height: 720,
    });
    bundle.add_audio(AudioRef {
        t_offset_ns: 0,
        duration_ns: 20_000_000,
        blob_path: "audio/0.opus".into(),
        sample_rate: 44_100,
        channels: 1,
    });
    bundle.add_word(TranscriptWord {
        t_offset_ns: 0,
        t_end_ns: 800_000_000,
        text: "build".into(),
        speaker: Some("operator".into()),
        confidence: 0.91,
    });
    bundle.add_word(TranscriptWord {
        t_offset_ns: 800_000_001,
        t_end_ns: 1_600_000_000,
        text: "ok".into(),
        speaker: Some("operator".into()),
        confidence: 0.99,
    });
    bundle.seal(
        Some("build-success".into()),
        vec!["green".into(), "ci".into()],
        0.75,
    );

    let original_id = bundle.id;

    let emb = BundleEmbeddings {
        modality: "text".into(),
        embedding: embedding(vec![1.0, 0.0, 0.0, 0.0]),
    };
    store.write_bundle(&bundle, &[emb]).expect("write bundle");

    // Retrieve by the same UUID.
    let retrieved = store.read_bundle(original_id).expect("read bundle");

    assert_eq!(retrieved.id, original_id, "id");
    assert_eq!(retrieved.source_pane_id, 42, "source_pane_id");
    assert_eq!(retrieved.t_start_ns, 1_000_000, "t_start_ns");
    assert_eq!(
        retrieved.t_wall_unix_ms, 1_700_000_000_000,
        "t_wall_unix_ms"
    );

    assert_eq!(retrieved.frames.len(), 2, "frame count");
    assert_eq!(retrieved.frames[0].sha, "sha-frame-0", "frame[0].sha");
    assert_eq!(
        retrieved.frames[1].t_offset_ns, 16_666_666,
        "frame[1].t_offset_ns"
    );
    assert_eq!(retrieved.frames[0].width, 1280, "width");
    assert_eq!(retrieved.frames[0].height, 720, "height");

    assert_eq!(retrieved.audio_chunks.len(), 1, "audio count");
    assert_eq!(retrieved.audio_chunks[0].sample_rate, 44_100, "sample_rate");
    assert_eq!(retrieved.audio_chunks[0].channels, 1, "channels");

    assert_eq!(retrieved.transcript_words.len(), 2, "word count");
    assert_eq!(retrieved.transcript_words[0].text, "build", "word[0]");
    assert_eq!(retrieved.transcript_words[1].text, "ok", "word[1]");
    assert_eq!(
        retrieved.transcript_words[0].speaker.as_deref(),
        Some("operator"),
        "speaker"
    );

    assert_eq!(retrieved.intent.as_deref(), Some("build-success"), "intent");
    assert_eq!(
        retrieved.tags,
        vec!["green".to_string(), "ci".to_string()],
        "tags"
    );
    assert!((retrieved.importance - 0.75).abs() < 1e-5, "importance");
    assert!(retrieved.sealed, "sealed");
}

// ── 4. Embedding round-trip ───────────────────────────────────────────────────

/// A vector inserted into `InMemoryStore` and queried with itself must score
/// ≈ 1.0 (cosine similarity of any non-zero vector with itself is exactly 1.0).
#[test]
fn embedding_self_similarity_is_one() {
    let mut store = InMemoryStore::new();
    let id = uuid::Uuid::from_u128(0x0102_0304_0506_0708_090A_0B0C_0D0E_0F10_u128);

    let vector = vec![0.6_f32, 0.8, 0.0, 0.0]; // |v| = 1.0
    store
        .insert(id, vector.clone(), std::collections::HashMap::new())
        .expect("insert embedding");

    // Query with the exact same vector; k=1.
    let hits = store
        .query(&vector, 1, None)
        .expect("query embedding store");

    assert_eq!(hits.len(), 1, "expected exactly one hit");
    let hit = &hits[0];
    assert_eq!(hit.id(), id, "hit id must match inserted id");
    assert!(
        (hit.score() - 1.0).abs() < 1e-5,
        "cosine similarity of a vector with itself must be ≈ 1.0, got {}",
        hit.score()
    );

    // Also verify the standalone cosine_similarity helper is consistent.
    let sim = cosine_similarity(&vector, &vector);
    assert!(
        (sim - 1.0).abs() < 1e-5,
        "cosine_similarity(&v, &v) must be ≈ 1.0, got {sim}"
    );
}

// ── 5. End-to-end pipeline ────────────────────────────────────────────────────

/// Full pipeline: dedup frames with vision → assemble bundle → store → retrieve
/// → vector search hits the stored bundle.
///
/// This exercises all four crates in a single linear chain, which is the
/// scenario captured by issue #91.
#[test]
fn end_to_end_capture_pipeline() {
    // --- Step 1: Vision dedup ---
    // Two identical capture frames.  Only frame_a "survives" into the bundle.
    let frame_a = solid_rgba(64, 64, 20, 40, 80);
    let frame_b = solid_rgba(64, 64, 20, 40, 80);

    let hash_a = dhash(&frame_a, 64, 64).expect("dhash a");
    let hash_b = dhash(&frame_b, 64, 64).expect("dhash b");
    assert_eq!(
        hamming_distance(hash_a, hash_b),
        0,
        "dedup: b is a duplicate of a"
    );

    // --- Step 2: Bundle assembly ---
    let mut asm = BundleAssembler::new(7_u64);
    asm.push_frame(FrameRef {
        t_offset_ns: 0,
        sha: "sha-unique-frame".into(),
        blob_path: "frames/0.png".into(),
        dhash: hash_a,
        width: 64,
        height: 64,
    });
    // frame_b is a duplicate and is intentionally NOT pushed.
    asm.push_word(TranscriptWord {
        t_offset_ns: 0,
        t_end_ns: 1_000_000_000,
        text: "phantom smoke test".into(),
        speaker: None,
        confidence: 0.99,
    });

    let bundle = asm
        .finish(Some("e2e-smoke".into()), vec!["pipeline".into()], 0.5)
        .expect("assemble bundle");

    assert_eq!(
        bundle.frames.len(),
        1,
        "only the non-duplicate frame is stored"
    );
    assert_eq!(bundle.frames[0].dhash, hash_a);
    assert!(bundle.sealed);

    // --- Step 3: Store ---
    let (_tmp, store) = open_tmp();
    let bundle_id = bundle.id;
    let query_vec = vec![0.0_f32, 1.0, 0.0]; // "text" direction

    store
        .write_bundle(
            &bundle,
            &[BundleEmbeddings {
                modality: "text".into(),
                embedding: embedding(query_vec.clone()),
            }],
        )
        .expect("write bundle to store");

    // --- Step 4: Retrieve by ID ---
    let retrieved = store.read_bundle(bundle_id).expect("read bundle");
    assert_eq!(retrieved.id, bundle_id, "retrieved id matches");
    assert_eq!(retrieved.frames.len(), 1, "retrieved frame count");
    assert_eq!(
        retrieved.frames[0].dhash, hash_a,
        "dhash preserved in store"
    );
    assert_eq!(retrieved.transcript_words[0].text, "phantom smoke test");

    // --- Step 5: Vector search finds the bundle ---
    let hits = store
        .search_vectors(&phantom_bundle_store::VectorQuery {
            modality: "text".into(),
            vector: query_vec.clone(),
            limit: 5,
        })
        .expect("vector search");

    assert!(
        !hits.is_empty(),
        "vector search must return at least one hit"
    );
    assert_eq!(hits[0].bundle_id, bundle_id, "top hit must be our bundle");
    assert!(
        (hits[0].similarity - 1.0).abs() < 1e-5,
        "self-query similarity must be ≈ 1.0, got {}",
        hits[0].similarity
    );
}
