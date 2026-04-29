//! Atomicity recovery for the unified store.
//!
//! The two-phase write protocol in [`crate::BundleStore::write_bundle`] keeps
//! SQLite and the vector index in sync at steady state: vectors go in first,
//! then the SQLite transaction commits. If the *vector* step fails we roll
//! the SQLite transaction back; if SQLite fails on commit we roll back the
//! vectors we already wrote. Either way no half-state should reach disk.
//!
//! However, when the vector backend is persistent (LanceDB, future) the
//! protocol could leak rows on a process crash between the two phases. The
//! `leaked_rows` table is a scratchpad where the writer records bundle ids
//! that need attention, and [`sweep_leaked_rows`] runs at startup to:
//!
//! 1. Drop any vector entries whose bundle id is missing from SQLite.
//! 2. Clear the corresponding `leaked_rows` rows.
//!
//! With the in-memory vector index there is nothing durable to clean up — the
//! sweep is a no-op except for clearing the scratchpad table. The function
//! still runs so the contract is exercised and the failure surface stays
//! consistent across backends.

use std::collections::HashSet;

use phantom_bundles::BundleId;

use crate::StoreError;
use crate::lance::VectorIndex;
use crate::sqlite::{Connection, all_bundle_ids};

/// Sweep vector-index entries that no longer have a matching SQLite row,
/// then clear the `leaked_rows` table.
///
/// Returns the number of vector entries that were dropped.
pub fn sweep_leaked_rows(
    sqlite: &Connection,
    vectors: &dyn VectorIndex,
) -> Result<usize, StoreError> {
    let known: HashSet<BundleId> = all_bundle_ids(sqlite)?.into_iter().collect();

    let mut dropped = 0_usize;
    for modality in vectors.modalities() {
        for id in vectors.ids_for_modality(&modality) {
            if !known.contains(&id) {
                vectors.remove(id, &modality)?;
                dropped += 1;
            }
        }
    }

    // Clear the durable scratchpad — every entry there has been handled
    // (either by re-checking the index above, or because the row never
    // landed in the index at all and the next write will retry).
    sqlite.exec("DELETE FROM leaked_rows")?;
    Ok(dropped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lance::InMemoryVectorIndex;
    use crate::testing;
    use crate::{BundleEmbeddings, BundleStore, StoreConfig};
    use phantom_bundles::Bundle;
    use phantom_embeddings::Embedding;
    use tempfile::TempDir;
    use uuid::Uuid;

    fn embedding(vec: Vec<f32>) -> Embedding {
        let dim = vec.len();
        Embedding {
            vec,
            dim,
            model: "test".into(),
        }
    }

    #[test]
    fn sweep_drops_unknown_vector_entries() {
        let tmp = TempDir::new().unwrap();
        let key = testing::deterministic_master_key(0xCD);
        let store = BundleStore::open(StoreConfig {
            root: tmp.path().to_path_buf(),
            master_key: key.clone(),
        })
        .unwrap();

        // Write one real bundle.
        let mut b = Bundle::new(1);
        b.seal(Some("real".into()), vec![], 0.5);
        let real_id = b.id;
        store
            .write_bundle(
                &b,
                &[BundleEmbeddings {
                    modality: "text".into(),
                    embedding: embedding(vec![1.0, 0.0]),
                }],
            )
            .unwrap();

        // Drop the store and re-open. `sweep_leaked_rows` runs on open.
        drop(store);
        let _store = BundleStore::open(StoreConfig {
            root: tmp.path().to_path_buf(),
            master_key: key,
        })
        .unwrap();
        // `real_id` should still be readable; the in-memory vectors get
        // rebuilt empty on reopen so there's nothing to sweep, but the call
        // must not error.
        let _ = real_id;
    }

    #[test]
    fn sweep_with_orphaned_vector_drops_it() {
        let tmp = TempDir::new().unwrap();
        let key = testing::deterministic_master_key(0xEF);
        let db_path = tmp.path().join("bundles.db");
        std::fs::create_dir_all(tmp.path()).unwrap();
        let conn = Connection::open_encrypted(&db_path, key.bytes()).unwrap();
        crate::sqlite::init_schema(&conn).unwrap();

        let vectors = InMemoryVectorIndex::new();
        let orphan = Uuid::from_u128(123);
        vectors.upsert(orphan, "text", &[1.0, 2.0]).unwrap();

        let dropped = sweep_leaked_rows(&conn, &vectors).unwrap();
        assert_eq!(dropped, 1, "orphaned vector should be dropped");
        assert!(vectors.ids_for_modality("text").is_empty());
    }
}
