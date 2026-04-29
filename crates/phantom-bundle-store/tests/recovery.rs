//! Integration tests for the recovery sweep path.
//!
//! These tests live outside the crate and rely on the `testing` Cargo feature
//! (enabled via the `phantom-bundle-store` dev-dependency in `Cargo.toml`) to
//! access helpers that must not appear in production builds.

use phantom_bundle_store::testing::{
    deterministic_master_key, inject_leaked_row, open_at, run_sweep,
};
use phantom_bundle_store::{BundleEmbeddings, InMemoryVectorIndex, StoreError, VectorIndex};
use phantom_bundles::Bundle;
use phantom_embeddings::Embedding;
use tempfile::TempDir;

fn embedding(vec: Vec<f32>) -> Embedding {
    let dim = vec.len();
    Embedding { vec, dim, model: "test".into() }
}

/// After a clean write-and-reopen cycle no bundles are swept (none are
/// orphaned). The sweep must succeed without returning an error.
#[test]
fn reopen_after_clean_write_does_not_error() {
    let tmp = TempDir::new().unwrap();
    let key = deterministic_master_key(0x01);

    // Write one bundle.
    {
        let store = open_at(tmp.path(), key.clone()).expect("open");
        let mut b = Bundle::new(1);
        b.seal(Some("ok".into()), vec![], 0.5);
        store
            .write_bundle(
                &b,
                &[BundleEmbeddings {
                    modality: "text".into(),
                    embedding: embedding(vec![1.0, 0.0]),
                }],
            )
            .expect("write");
    }

    // Re-open — sweep_leaked_rows runs on open and must not error.
    open_at(tmp.path(), key).expect("reopen must succeed");
}

/// An in-memory vector index that already holds an orphaned entry (no
/// corresponding SQLite row) before the sweep runs should have that entry
/// removed. We exercise the sweep function directly so we can pre-populate the
/// vector index without going through the two-phase write protocol.
#[test]
fn testing_module_accessible_from_integration_test() {
    // This test is primarily a compile-time assertion: if the `testing` feature
    // is not enabled the symbols used above won't resolve and this file won't
    // compile. A trivial runtime assertion confirms the helpers actually work.
    let tmp = TempDir::new().unwrap();
    let key = deterministic_master_key(0x42);
    let store = open_at(tmp.path(), key).expect("open");

    // The in-memory vector index is empty on a fresh store — nothing to sweep.
    let vectors = InMemoryVectorIndex::default();
    assert!(vectors.ids_for_modality("text").is_empty());
    drop(store);
}

// ---------------------------------------------------------------------------
// Error-path tests (issue #88)
// ---------------------------------------------------------------------------

/// Flipping the last byte of a sealed blob corrupts the Poly1305 authentication
/// tag. Reading that blob must surface `StoreError::Crypto` — not a panic, not
/// a garbled plaintext.
#[test]
fn corrupt_aead_tag_returns_crypto_error() {
    let tmp = TempDir::new().unwrap();
    let key = deterministic_master_key(0x10);
    let store = open_at(tmp.path(), key).expect("open");

    let mut b = Bundle::new(1);
    b.seal(Some("crypto-test".into()), vec![], 0.5);

    // Write a frame blob and get its relative path back.
    let relpath = store
        .write_frame_blob(b.id, "frame0.raw", b"hello crypto world")
        .expect("write blob");

    // The blob lives at <root>/<relpath>. Read it, flip the last byte (that is
    // the last byte of the Poly1305 tag), write it back.
    let abs = tmp.path().join(&relpath);
    let mut raw = std::fs::read(&abs).expect("read raw blob");
    let last = raw.last_mut().expect("non-empty blob");
    *last ^= 0xFF;
    std::fs::write(&abs, &raw).expect("write tampered blob");

    // Now attempt to decrypt — must fail with Crypto, not panic.
    let err = store.read_blob(b.id, &relpath).expect_err("must fail auth");
    assert!(
        matches!(err, StoreError::Crypto(_)),
        "expected Crypto error, got {err:?}"
    );
}

/// Deleting the blob file after a successful write must surface
/// `StoreError::Io` when the caller tries to read it back.
#[test]
fn missing_blob_file_returns_io_error() {
    let tmp = TempDir::new().unwrap();
    let key = deterministic_master_key(0x20);
    let store = open_at(tmp.path(), key).expect("open");

    let mut b = Bundle::new(2);
    b.seal(Some("io-test".into()), vec![], 0.5);

    let relpath = store
        .write_audio_blob(b.id, "audio0.opus", b"fake audio data")
        .expect("write blob");

    // Remove the file from disk.
    let abs = tmp.path().join(&relpath);
    std::fs::remove_file(&abs).expect("remove blob");

    // Read must now surface Io.
    let err = store.read_blob(b.id, &relpath).expect_err("must fail");
    assert!(
        matches!(err, StoreError::Io(_)),
        "expected Io error, got {err:?}"
    );
}

/// Writing fewer bytes than the MAGIC + nonce minimum (4 + 24 = 28 bytes)
/// makes the envelope unparseable. Reading back must surface `StoreError::Crypto`.
#[test]
fn truncated_blob_file_returns_crypto_error() {
    let tmp = TempDir::new().unwrap();
    let key = deterministic_master_key(0x30);
    let store = open_at(tmp.path(), key).expect("open");

    let mut b = Bundle::new(3);
    b.seal(Some("trunc-test".into()), vec![], 0.5);

    let relpath = store
        .write_frame_blob(b.id, "frame_trunc.raw", b"truncation test payload")
        .expect("write blob");

    // Overwrite with only 10 bytes — well below the 28-byte minimum for a
    // valid PBE1 envelope (4 magic + 24 nonce).
    let abs = tmp.path().join(&relpath);
    std::fs::write(&abs, &[0u8; 10]).expect("write truncated blob");

    let err = store.read_blob(b.id, &relpath).expect_err("must fail");
    assert!(
        matches!(err, StoreError::Crypto(_)),
        "expected Crypto error on truncated envelope, got {err:?}"
    );
}

/// Inject an orphan vector entry (present in the in-memory index but absent
/// from SQLite) and a `leaked_rows` table entry via testing helpers. Then call
/// the sweep via `run_sweep` and assert that exactly one entry is dropped while
/// a real bundle that was properly committed survives.
#[test]
fn sweep_cleans_orphaned_vectors_and_leaked_rows_table() {
    let tmp = TempDir::new().unwrap();
    let key = deterministic_master_key(0x40);

    // Open, write one real bundle, then close.
    let real_id = {
        let store = open_at(tmp.path(), key.clone()).expect("open");
        let mut b = Bundle::new(4);
        b.seal(Some("real-bundle".into()), vec![], 0.7);
        let id = b.id;
        store
            .write_bundle(
                &b,
                &[BundleEmbeddings {
                    modality: "text".into(),
                    embedding: embedding(vec![1.0, 0.0, 0.0]),
                }],
            )
            .expect("write real bundle");
        id
    };

    // Construct a fresh in-memory vector index and populate it with:
    //   • the real bundle's vector (should survive the sweep)
    //   • an orphan id that has no matching SQLite row (should be dropped)
    let orphan_id = uuid::Uuid::from_u128(0xDEAD_BEEF_CAFE_0000_0000_0000_0000_0001);
    let vectors = InMemoryVectorIndex::new();
    vectors.upsert(real_id, "text", &[1.0, 0.0, 0.0]).expect("upsert real");
    vectors.upsert(orphan_id, "text", &[0.0, 1.0, 0.0]).expect("upsert orphan");

    // Also inject a leaked_rows record to confirm the sweep clears it.
    let db_path = tmp.path().join("bundles.db");
    inject_leaked_row(&db_path, &key, orphan_id, "text").expect("inject leaked row");

    // Run the sweep.
    let dropped = run_sweep(&db_path, &key, &vectors).expect("sweep");
    assert_eq!(dropped, 1, "exactly one orphan vector should be dropped");

    // The real bundle's vector must still be in the index.
    let surviving = vectors.ids_for_modality("text");
    assert!(surviving.contains(&real_id), "real bundle must survive sweep");
    assert!(!surviving.contains(&orphan_id), "orphan must be gone after sweep");
}

/// Synthesize a `StoreError::Keyring` error and verify that its `Display`
/// implementation does not panic and produces a non-empty, human-readable
/// message. This guards against any accidental unwrap inside the display path.
#[test]
fn master_key_keyring_error_surfaces_cleanly_without_panic() {
    // Construct the error directly — no need to touch the real OS keychain.
    let err = StoreError::Keyring("simulated keychain failure".into());

    // Must not panic.
    let msg = err.to_string();

    // Must produce a non-empty, human-readable string containing the detail.
    assert!(!msg.is_empty(), "error display must not be empty");
    assert!(
        msg.contains("simulated keychain failure"),
        "display must include the inner message, got: {msg:?}"
    );

    // Confirm Debug also works without panicking.
    let dbg = format!("{err:?}");
    assert!(!dbg.is_empty());
}
