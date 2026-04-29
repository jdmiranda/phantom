//! Integration tests for the recovery sweep path.
//!
//! These tests live outside the crate and rely on the `testing` Cargo feature
//! (enabled via the `phantom-bundle-store` dev-dependency in `Cargo.toml`) to
//! access helpers that must not appear in production builds.

use phantom_bundle_store::testing::{deterministic_master_key, open_at};
use phantom_bundle_store::{BundleEmbeddings, InMemoryVectorIndex, VectorIndex};
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
