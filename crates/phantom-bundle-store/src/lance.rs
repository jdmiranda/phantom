//! Vector-index abstraction.
//!
//! In v1 we ship only the [`InMemoryVectorIndex`] backend. It is enough to
//! exercise the rest of the bundle store (atomicity, recovery, search) while
//! the Lance/Arrow Rust ecosystem stabilizes. A future `vector_search_lancedb`
//! feature will substitute a real Lance backend behind the same
//! [`VectorIndex`] trait.
//!
//! The index is keyed by (`modality`, [`BundleId`]). Each modality is a
//! separate "table" with its own dimension that is fixed by the first vector
//! upserted into it. Subsequent upserts that disagree on dimension are
//! rejected with [`StoreError::Vector`] — this is the failure surface the
//! atomicity test exploits to force a rollback.
//!
//! Search is exact (no ANN). Cosine similarity is reused from
//! [`phantom_embeddings::cosine_similarity`].

use std::collections::HashMap;
use std::sync::Mutex;

use phantom_bundles::BundleId;
use phantom_embeddings::cosine_similarity;
use serde::{Deserialize, Serialize};

use crate::StoreError;

/// One hit returned by [`VectorIndex::search`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorHit {
    /// Bundle whose vector matched.
    pub bundle_id: BundleId,
    /// Cosine similarity in `[-1.0, 1.0]`. Higher is more similar.
    pub similarity: f32,
}

/// Storage backend for embedding vectors.
///
/// Implementations must be `Send + Sync` because they live behind the
/// store's outer mutex and are reachable across threads.
pub trait VectorIndex: Send + Sync {
    /// Insert or replace a vector under `(modality, bundle_id)`.
    fn upsert(&self, bundle_id: BundleId, modality: &str, vector: &[f32])
    -> Result<(), StoreError>;

    /// Remove a vector. Idempotent — missing entries return `Ok(())`.
    fn remove(&self, bundle_id: BundleId, modality: &str) -> Result<(), StoreError>;

    /// Top-`limit` nearest vectors under `modality`, ordered by descending
    /// similarity. Empty modality returns `Ok(vec![])`.
    fn search(
        &self,
        modality: &str,
        query: &[f32],
        limit: usize,
    ) -> Result<Vec<VectorHit>, StoreError>;

    /// All bundle ids currently held under `modality`. Used by the recovery
    /// sweep to detect leaked rows.
    fn ids_for_modality(&self, modality: &str) -> Vec<BundleId>;

    /// All known modality names.
    fn modalities(&self) -> Vec<String>;
}

/// Default in-memory implementation.
///
/// Cheap to clone via `Default`. Internally a `Mutex<HashMap<modality,
/// ModalityTable>>` — fine because writes are serialized at the store level
/// already and reads are short.
#[derive(Default)]
pub struct InMemoryVectorIndex {
    inner: Mutex<HashMap<String, ModalityTable>>,
}

#[derive(Default)]
struct ModalityTable {
    /// First-write-wins dimension. None until the first upsert.
    dim: Option<usize>,
    vectors: HashMap<BundleId, Vec<f32>>,
}

impl InMemoryVectorIndex {
    /// Construct an empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Direct vector lookup by `(bundle_id, modality)`.
    ///
    /// Returns `None` if the entry is absent or the lock is poisoned.
    /// This is intentionally not part of the [`VectorIndex`] trait — it is
    /// used only by the migration export path in [`crate::migrate`].
    #[must_use]
    pub fn get_vector(&self, bundle_id: BundleId, modality: &str) -> Option<Vec<f32>> {
        let guard = self.inner.lock().ok()?;
        guard
            .get(modality)
            .and_then(|t| t.vectors.get(&bundle_id))
            .cloned()
    }
}

impl VectorIndex for InMemoryVectorIndex {
    fn upsert(
        &self,
        bundle_id: BundleId,
        modality: &str,
        vector: &[f32],
    ) -> Result<(), StoreError> {
        if vector.is_empty() {
            return Err(StoreError::Vector("empty vector".into()));
        }
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| StoreError::Vector(format!("lock poisoned: {e}")))?;
        let table = guard.entry(modality.to_string()).or_default();
        match table.dim {
            None => table.dim = Some(vector.len()),
            Some(d) if d != vector.len() => {
                return Err(StoreError::Vector(format!(
                    "dim mismatch under modality {modality:?}: have {d}, got {}",
                    vector.len()
                )));
            }
            Some(_) => {}
        }
        table.vectors.insert(bundle_id, vector.to_vec());
        Ok(())
    }

    fn remove(&self, bundle_id: BundleId, modality: &str) -> Result<(), StoreError> {
        let mut guard = self
            .inner
            .lock()
            .map_err(|e| StoreError::Vector(format!("lock poisoned: {e}")))?;
        if let Some(table) = guard.get_mut(modality) {
            table.vectors.remove(&bundle_id);
        }
        Ok(())
    }

    fn search(
        &self,
        modality: &str,
        query: &[f32],
        limit: usize,
    ) -> Result<Vec<VectorHit>, StoreError> {
        let guard = self
            .inner
            .lock()
            .map_err(|e| StoreError::Vector(format!("lock poisoned: {e}")))?;
        let Some(table) = guard.get(modality) else {
            return Ok(Vec::new());
        };
        let mut hits: Vec<VectorHit> = table
            .vectors
            .iter()
            .map(|(id, v)| VectorHit {
                bundle_id: *id,
                similarity: cosine_similarity(query, v),
            })
            .collect();
        hits.sort_by(|a, b| {
            b.similarity
                .partial_cmp(&a.similarity)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(limit);
        Ok(hits)
    }

    fn ids_for_modality(&self, modality: &str) -> Vec<BundleId> {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        guard
            .get(modality)
            .map(|t| t.vectors.keys().copied().collect())
            .unwrap_or_default()
    }

    fn modalities(&self) -> Vec<String> {
        let guard = match self.inner.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        guard.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn upsert_then_search_orders_by_similarity() {
        let idx = InMemoryVectorIndex::new();
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let c = Uuid::from_u128(3);
        idx.upsert(a, "text", &[1.0, 0.0, 0.0]).unwrap();
        idx.upsert(b, "text", &[0.0, 1.0, 0.0]).unwrap();
        idx.upsert(c, "text", &[0.0, 0.0, 1.0]).unwrap();
        let hits = idx.search("text", &[0.0, 1.0, 0.0], 3).unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0].bundle_id, b);
        assert!((hits[0].similarity - 1.0).abs() < 1e-6);
    }

    #[test]
    fn dim_mismatch_is_rejected() {
        let idx = InMemoryVectorIndex::new();
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        idx.upsert(a, "text", &[1.0, 2.0, 3.0]).unwrap();
        let err = idx.upsert(b, "text", &[1.0, 2.0]).unwrap_err();
        assert!(matches!(err, StoreError::Vector(_)));
    }

    #[test]
    fn empty_modality_search_returns_empty() {
        let idx = InMemoryVectorIndex::new();
        let hits = idx.search("missing", &[1.0, 2.0, 3.0], 5).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn remove_is_idempotent() {
        let idx = InMemoryVectorIndex::new();
        let a = Uuid::from_u128(1);
        idx.upsert(a, "text", &[1.0]).unwrap();
        idx.remove(a, "text").unwrap();
        idx.remove(a, "text").unwrap(); // second remove is fine
        idx.remove(a, "absent_modality").unwrap();
    }

    #[test]
    fn search_truncates_to_limit() {
        let idx = InMemoryVectorIndex::new();
        for i in 0_u32..10 {
            let id = Uuid::from_u128(u128::from(i + 1));
            idx.upsert(id, "text", &[f32::from(i as u16), 0.0]).unwrap();
        }
        let hits = idx.search("text", &[1.0, 0.0], 3).unwrap();
        assert_eq!(hits.len(), 3);
    }
}
