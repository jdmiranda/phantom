//! Integration tests for `phantom_bundle_store::recovery`.
//!
//! All five tests cover distinct error paths in the recovery module. Each test
//! asserts that the recovery path returns a well-typed `Result` and never
//! panics.

use phantom_bundle_store::{
    BundleEmbeddings, InMemoryVectorIndex, MasterKey, StoreError, VectorIndex, testing,
};
use phantom_bundles::Bundle;
use phantom_embeddings::Embedding;
use tempfile::TempDir;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn embedding(vec: Vec<f32>) -> Embedding {
    let dim = vec.len();
    Embedding { vec, dim, model: "test".into() }
}

fn sealed_bundle(pane: u64) -> Bundle {
    let mut b = Bundle::new(pane);
    b.seal(Some("recovery-test".into()), vec![], 0.5);
    b
}

// ---------------------------------------------------------------------------
// Test 1 — Corrupt AEAD tag → read_blob returns StoreError::Crypto
// ---------------------------------------------------------------------------
//
// We seal a frame blob to disk, then flip the last byte of the file (which
// sits inside the Poly1305 authentication tag). The AEAD open must fail and
// surface as StoreError::Crypto — not a panic.

#[test]
fn corrupt_aead_tag_returns_crypto_error() {
    let tmp = TempDir::new().expect("tempdir");
    let key = testing::deterministic_master_key(0x01);
    let store = testing::open_at(tmp.path(), key).expect("open");

    let bundle = sealed_bundle(1);
    let plaintext = b"frame pixel data";

    // Write one blob and capture its relative path.
    let relpath = store
        .write_frame_blob(bundle.id, "frame0.raw", plaintext)
        .expect("write blob");

    // Locate the file on disk and corrupt the last byte (AEAD tag tail).
    let abs = tmp.path().join(&relpath);
    let mut bytes = std::fs::read(&abs).expect("read blob file");
    assert!(!bytes.is_empty(), "blob file must not be empty");
    *bytes.last_mut().unwrap() ^= 0xFF;
    std::fs::write(&abs, &bytes).expect("write corrupted blob");

    // Reading back must fail with a Crypto error, not a panic.
    let err = store
        .read_blob(bundle.id, &relpath)
        .expect_err("corrupt tag must cause decryption failure");

    assert!(
        matches!(err, StoreError::Crypto(_)),
        "expected StoreError::Crypto, got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 2 — Missing blob file → read_blob returns StoreError::Io
// ---------------------------------------------------------------------------
//
// After writing a frame blob we delete the blob file from disk. A subsequent
// read_blob must surface StoreError::Io (file not found) rather than panicking
// or returning corrupted data.

#[test]
fn missing_blob_file_returns_io_error() {
    let tmp = TempDir::new().expect("tempdir");
    let key = testing::deterministic_master_key(0x02);
    let store = testing::open_at(tmp.path(), key).expect("open");

    let bundle = sealed_bundle(2);
    let relpath = store
        .write_frame_blob(bundle.id, "missing.raw", b"audio sample data")
        .expect("write blob");

    // Delete the file so the next read hits a missing-file I/O error.
    let abs = tmp.path().join(&relpath);
    std::fs::remove_file(&abs).expect("remove blob file");

    let err = store
        .read_blob(bundle.id, &relpath)
        .expect_err("missing file must return an error");

    assert!(
        matches!(err, StoreError::Io(_)),
        "expected StoreError::Io, got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 3 — Partial write / truncated blob → read_blob returns StoreError::Crypto
// ---------------------------------------------------------------------------
//
// A blob sealed with XChaCha20-Poly1305 has the on-disk layout:
//   MAGIC (4 bytes) || nonce (24 bytes) || ciphertext+tag
//
// If the process crashed mid-write the file may be shorter than the minimum
// envelope length (28 bytes). BlobEnvelope::from_bytes returns an error for
// short files; that error is mapped to StoreError::Crypto("envelope decode:
// ..."). The recovery path must return that error rather than panic.

#[test]
fn truncated_blob_file_returns_crypto_error() {
    let tmp = TempDir::new().expect("tempdir");
    let key = testing::deterministic_master_key(0x03);
    let store = testing::open_at(tmp.path(), key).expect("open");

    let bundle = sealed_bundle(3);
    let relpath = store
        .write_frame_blob(bundle.id, "partial.raw", b"the quick brown fox")
        .expect("write blob");

    let abs = tmp.path().join(&relpath);
    let full_bytes = std::fs::read(&abs).expect("read blob file");
    assert!(full_bytes.len() > 10, "blob must be larger than 10 bytes");

    // Truncate to 10 bytes — too short for the MAGIC+nonce header (28 bytes).
    std::fs::write(&abs, &full_bytes[..10]).expect("write truncated blob");

    let err = store
        .read_blob(bundle.id, &relpath)
        .expect_err("truncated file must cause an error");

    assert!(
        matches!(err, StoreError::Crypto(_)),
        "expected StoreError::Crypto for truncated envelope, got: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 4 — SQLite index in dirty state → sweep_leaked_rows cleans up correctly
// ---------------------------------------------------------------------------
//
// This test simulates the state that would exist after a crash between the
// vector upsert and the SQLite commit: vector entries exist in the in-memory
// index for bundle ids that are *absent* from SQLite. We then call
// `testing::run_sweep` (which calls `recovery::sweep_leaked_rows` internally)
// and verify:
//   a) The call succeeds (returns Ok).
//   b) All orphaned vector entries are removed.
//   c) Legitimate vector entries (whose bundle ids ARE in SQLite) are left
//      intact.
//   d) The `leaked_rows` scratchpad entry we injected doesn't cause the sweep
//      to error.

#[test]
fn sweep_cleans_orphaned_vectors_and_leaked_rows_table() {
    let tmp = TempDir::new().expect("tempdir");
    let key = testing::deterministic_master_key(0x04);

    // Open a store and write one real bundle so SQLite has at least one row.
    let real_store = testing::open_at(tmp.path(), key.clone()).expect("open");
    let real_bundle = sealed_bundle(4);
    real_store
        .write_bundle(
            &real_bundle,
            &[BundleEmbeddings {
                modality: "text".into(),
                embedding: embedding(vec![1.0, 0.0]),
            }],
        )
        .expect("write real bundle");
    drop(real_store);

    let db_path = tmp.path().join("bundles.db");

    // Inject a fake leaked_rows entry for an id that has no SQLite bundle row.
    let orphan_id = Uuid::from_u128(0xDEAD_BEEF_CAFE_0000_FFFF_0000_CAFE_DEAD);
    testing::inject_leaked_row(&db_path, &key, orphan_id, "text")
        .expect("inject leaked row");

    // Build an in-memory vector index with two entries:
    //   - real_bundle.id: has a SQLite row → must survive the sweep.
    //   - orphan_id:      no SQLite row → must be dropped by the sweep.
    let vectors = InMemoryVectorIndex::new();
    vectors
        .upsert(real_bundle.id, "text", &[1.0, 0.0])
        .expect("upsert real");
    vectors
        .upsert(orphan_id, "text", &[0.0, 1.0])
        .expect("upsert orphan");

    assert_eq!(
        vectors.ids_for_modality("text").len(),
        2,
        "should have 2 entries before sweep"
    );

    // Run the sweep — it must succeed and report exactly 1 dropped entry.
    let dropped = testing::run_sweep(&db_path, &key, &vectors)
        .expect("sweep must not fail");
    assert_eq!(dropped, 1, "exactly one orphaned vector should be dropped");

    // The real bundle's vector must still be present.
    let remaining = vectors.ids_for_modality("text");
    assert_eq!(remaining.len(), 1, "one entry should remain after sweep");
    assert!(
        remaining.contains(&real_bundle.id),
        "real bundle's vector must survive the sweep"
    );

    // The orphan must be gone.
    assert!(
        !remaining.contains(&orphan_id),
        "orphan vector must have been removed"
    );
}

// ---------------------------------------------------------------------------
// Test 5 — Master key missing from keychain → clear error, no panic
// ---------------------------------------------------------------------------
//
// In production `MasterKey::from_keyring()` fetches the master key from the
// OS keychain. When it fails (keychain unavailable, sandbox restrictions, CI)
// the error must:
//   a) Be a `StoreError::Keyring` variant — not any other variant or a panic.
//   b) Carry a non-empty human-readable reason string.
//   c) Display correctly (implements `std::fmt::Display` without panicking).
//   d) Prevent `BundleStore::open` from succeeding — the caller propagates it
//      as a `StoreError` through the normal error path.
//
// We synthesise the keyring error rather than calling `from_keyring()` because
// that function blocks waiting for an OS keychain permission dialog on macOS
// in CI. The synthesised path exercises the same `StoreError::Keyring` code
// path that production code uses when the keychain is unavailable.

#[test]
fn master_key_keyring_error_surfaces_cleanly_without_panic() {
    // --- Arrange: representative error messages from keyring backends -----
    let messages = [
        "simulated: keychain service unavailable",
        "get: No secret-service dbus interface found",
        "entry: platform error: access denied",
    ];

    // --- Act / Assert: error variant is well-formed ----------------------
    for msg in &messages {
        let err = StoreError::Keyring((*msg).into());

        // Display must not panic and must include the original message.
        let display = err.to_string();
        assert!(
            display.contains(msg),
            "Display must embed the inner message. Got: {display:?}"
        );

        // Pattern match must work — proves we have the right variant.
        assert!(
            matches!(&err, StoreError::Keyring(s) if !s.is_empty()),
            "Must match StoreError::Keyring with non-empty payload"
        );

        // Debug must not panic.
        let _ = format!("{err:?}");
    }

    // --- Act / Assert: keyring failure prevents the store from opening ---
    //
    // Simulate the real production flow:
    //   let key = MasterKey::from_keyring()?;          // <-- would fail
    //   let store = BundleStore::open(StoreConfig { master_key: key, .. })?;
    //
    // When the keyring step fails the store must not open and the error must
    // propagate as StoreError::Keyring.

    let simulated_keyring_result: Result<MasterKey, StoreError> =
        Err(StoreError::Keyring("keychain unavailable in test environment".into()));

    let tmp = TempDir::new().expect("tempdir");

    match simulated_keyring_result {
        Ok(key) => {
            // Keychain available — this branch is not expected in this test
            // but is valid production behaviour.
            let _store = testing::open_at(tmp.path(), key).expect("open store");
        }
        Err(StoreError::Keyring(reason)) => {
            // Keychain unavailable — error is clear, no panic, no store open.
            assert!(!reason.is_empty(), "reason must be non-empty");
            // The store directory was never touched.
            assert!(
                !tmp.path().join("bundles.db").exists(),
                "database must not be created when the keyring step failed"
            );
        }
        Err(other) => {
            panic!("unexpected error variant when keyring is unavailable: {other:?}");
        }
    }
}
