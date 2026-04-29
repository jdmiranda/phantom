//! LanceDB-backed persistent vector index.
//!
//! Implements [`VectorIndex`] against a real LanceDB columnar store. Enabled
//! by the `lancedb` Cargo feature. The in-memory stub in [`lance`] is used
//! when the feature is absent.
//!
//! ## Table schema
//!
//! Each LanceDB table is named after its modality (e.g. `"text"`, `"intent"`).
//! Rows follow this Arrow schema:
//!
//! | column          | type            | notes                              |
//! |-----------------|-----------------|------------------------------------|
//! | `bundle_id`     | `Utf8`          | UUID string                        |
//! | `embedding`     | `FixedSizeList` | dim fixed on first upsert          |
//! | `metadata`      | `Utf8`          | JSON blob (reserved for future use)|
//! | `created_at_ms` | `Int64`         | Unix epoch milliseconds            |
//!
//! ## Async bridging
//!
//! LanceDB 0.27 is fully async. `VectorIndex` is synchronous because it lives
//! inside a coarse `Mutex` and is called from synchronous store methods. We
//! bridge the two worlds with a single-threaded `tokio::runtime::Runtime`
//! embedded in the index. Each trait-method call spawns a blocking task on
//! that runtime via `rt.block_on(...)`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use arrow_array::{
    Array, FixedSizeListArray, Int64Array, RecordBatch, RecordBatchReader, StringArray,
};
use arrow_schema::{DataType, Field, Schema};
use futures::TryStreamExt;
use lancedb::connection::Connection as LanceConnection;
use lancedb::query::{ExecutableQuery, QueryBase, Select};
use phantom_bundles::BundleId;
use tokio::runtime::Runtime;

use crate::lance::VectorIndex;
use crate::{StoreError, VectorHit};

// ---------------------------------------------------------------------------
// Dim cache — first-write-wins per modality, kept in memory.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct DimCache(HashMap<String, usize>);

impl DimCache {
    fn check_or_register(&mut self, modality: &str, dim: usize) -> Result<(), StoreError> {
        match self.0.get(modality) {
            None => {
                self.0.insert(modality.to_string(), dim);
                Ok(())
            }
            Some(&stored) if stored != dim => Err(StoreError::Vector(format!(
                "dim mismatch under modality {modality:?}: have {stored}, got {dim}"
            ))),
            Some(_) => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// LanceDbIndex
// ---------------------------------------------------------------------------

/// Persistent vector index backed by LanceDB.
///
/// Each modality maps to a separate LanceDB table in the same on-disk
/// database directory (`<store_root>/vectors/`).
pub struct LanceDbIndex {
    /// Shared async runtime — one per index, reused across calls.
    rt: Arc<Runtime>,
    /// Async LanceDB connection (an `Arc` internally, so cheap to clone).
    conn: LanceConnection,
    /// In-memory dim cache to avoid a round-trip to LanceDB on every upsert.
    dims: Mutex<DimCache>,
}

impl LanceDbIndex {
    /// Open (or create) a LanceDB database at `db_path`.
    ///
    /// This is a synchronous constructor: it blocks until the async
    /// [`lancedb::connect`] call resolves.
    pub fn open(db_path: &std::path::Path) -> Result<Self, StoreError> {
        std::fs::create_dir_all(db_path)?;
        let rt =
            Runtime::new().map_err(|e| StoreError::Vector(format!("tokio runtime init: {e}")))?;
        let path_str = db_path
            .to_str()
            .ok_or_else(|| StoreError::Vector("db path is not valid UTF-8".into()))?
            .to_string();
        let conn = rt.block_on(async {
            lancedb::connect(&path_str)
                .execute()
                .await
                .map_err(|e| StoreError::Vector(format!("lancedb connect: {e}")))
        })?;

        // Pre-populate dim cache from any tables that already exist.
        let dims = rt.block_on(async {
            let mut cache = DimCache::default();
            let tables = conn
                .table_names()
                .execute()
                .await
                .map_err(|e| StoreError::Vector(format!("table_names: {e}")))?;
            for tbl_name in tables {
                if let Ok(tbl) = conn.open_table(&tbl_name).execute().await {
                    if let Ok(schema) = tbl.schema().await {
                        if let Ok(dim) = embedding_dim_from_schema(&schema) {
                            cache.0.insert(tbl_name, dim);
                        }
                    }
                }
            }
            Ok::<_, StoreError>(cache)
        })?;

        Ok(Self {
            rt: Arc::new(rt),
            conn,
            dims: Mutex::new(dims),
        })
    }

    // ---------------------------------------------------------------------------
    // Private helpers
    // ---------------------------------------------------------------------------

    /// Open or create the LanceDB table for `modality` with `dim`-dimensional
    /// fixed-size-list embedding column.
    async fn open_or_create_table(
        &self,
        modality: &str,
        dim: usize,
    ) -> Result<lancedb::Table, StoreError> {
        match self.conn.open_table(modality).execute().await {
            Ok(tbl) => Ok(tbl),
            Err(_) => {
                // Create with an empty initial batch so the schema is committed.
                let schema = Arc::new(table_schema(dim));
                let empty = empty_batch(&schema, dim);
                let reader: Box<dyn RecordBatchReader + Send> = Box::new(
                    arrow_array::RecordBatchIterator::new(vec![Ok(empty)], schema),
                );
                self.conn
                    .create_table(modality, reader)
                    .execute()
                    .await
                    .map_err(|e| StoreError::Vector(format!("create_table {modality:?}: {e}")))
            }
        }
    }
}

impl VectorIndex for LanceDbIndex {
    fn upsert(
        &self,
        bundle_id: BundleId,
        modality: &str,
        vector: &[f32],
    ) -> Result<(), StoreError> {
        if vector.is_empty() {
            return Err(StoreError::Vector("empty vector".into()));
        }
        let dim = vector.len();
        {
            let mut cache = self
                .dims
                .lock()
                .map_err(|e| StoreError::Vector(format!("dim cache lock poisoned: {e}")))?;
            cache.check_or_register(modality, dim)?;
        }

        let modality = modality.to_string();
        let vector = vector.to_vec();
        let rt = Arc::clone(&self.rt);

        rt.block_on(async {
            let tbl = self.open_or_create_table(&modality, dim).await?;

            // Delete any existing row for this bundle_id so upsert is idempotent.
            let id_str = bundle_id.to_string();
            let _ = tbl.delete(&format!("bundle_id = '{id_str}'")).await;

            let schema = Arc::new(table_schema(dim));
            let batch = single_row_batch(&schema, bundle_id, &vector, dim)?;
            let reader: Box<dyn RecordBatchReader + Send> = Box::new(
                arrow_array::RecordBatchIterator::new(vec![Ok(batch)], schema),
            );
            tbl.add(reader)
                .execute()
                .await
                .map_err(|e| StoreError::Vector(format!("add to {modality:?}: {e}")))?;
            Ok(())
        })
    }

    fn remove(&self, bundle_id: BundleId, modality: &str) -> Result<(), StoreError> {
        let modality = modality.to_string();
        let rt = Arc::clone(&self.rt);
        rt.block_on(async {
            let tbl = match self.conn.open_table(&modality).execute().await {
                Ok(t) => t,
                Err(_) => return Ok(()), // table doesn't exist → nothing to remove
            };
            let id_str = bundle_id.to_string();
            tbl.delete(&format!("bundle_id = '{id_str}'"))
                .await
                .map_err(|e| StoreError::Vector(format!("delete from {modality:?}: {e}")))?;
            Ok(())
        })
    }

    fn search(
        &self,
        modality: &str,
        query: &[f32],
        limit: usize,
    ) -> Result<Vec<VectorHit>, StoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let modality = modality.to_string();
        let query_vec = query.to_vec();
        let rt = Arc::clone(&self.rt);
        rt.block_on(async {
            let tbl = match self.conn.open_table(&modality).execute().await {
                Ok(t) => t,
                Err(_) => return Ok(Vec::new()),
            };

            let row_count = tbl.count_rows(None).await.unwrap_or(0);
            if row_count == 0 {
                return Ok(Vec::new());
            }

            let results = tbl
                .vector_search(query_vec)
                .map_err(|e| StoreError::Vector(format!("vector_search build: {e}")))?
                .limit(limit)
                .execute()
                .await
                .map_err(|e| StoreError::Vector(format!("vector_search execute: {e}")))?;

            let batches: Vec<RecordBatch> = results
                .try_collect()
                .await
                .map_err(|e| StoreError::Vector(format!("collect results: {e}")))?;

            let mut hits = Vec::new();
            for batch in &batches {
                let id_col = batch
                    .column_by_name("bundle_id")
                    .ok_or_else(|| StoreError::Vector("bundle_id column missing".into()))?;
                let dist_col = batch
                    .column_by_name("_distance")
                    .ok_or_else(|| StoreError::Vector("_distance column missing".into()))?;

                let ids = id_col
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .ok_or_else(|| StoreError::Vector("bundle_id is not Utf8".into()))?;
                let dists = dist_col
                    .as_any()
                    .downcast_ref::<arrow_array::Float32Array>()
                    .ok_or_else(|| StoreError::Vector("_distance is not Float32".into()))?;

                for i in 0..batch.num_rows() {
                    let id_str = ids.value(i);
                    let bundle_id: BundleId = id_str
                        .parse()
                        .map_err(|e| StoreError::Vector(format!("uuid parse: {e}")))?;
                    // LanceDB returns L2 distance; convert to a similarity in (0,1].
                    // Distance 0 → similarity 1.0.
                    let dist = dists.value(i);
                    let similarity = 1.0_f32 / (1.0 + dist);
                    hits.push(VectorHit {
                        bundle_id,
                        similarity,
                    });
                }
            }

            // Sort descending by similarity (nearest first).
            hits.sort_by(|a, b| {
                b.similarity
                    .partial_cmp(&a.similarity)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            hits.truncate(limit);
            Ok(hits)
        })
    }

    fn ids_for_modality(&self, modality: &str) -> Vec<BundleId> {
        let modality = modality.to_string();
        let rt = Arc::clone(&self.rt);
        rt.block_on(async {
            let tbl = match self.conn.open_table(&modality).execute().await {
                Ok(t) => t,
                Err(_) => return Vec::new(),
            };
            let results = match tbl
                .query()
                .select(Select::Columns(vec!["bundle_id".into()]))
                .execute()
                .await
            {
                Ok(r) => r,
                Err(_) => return Vec::new(),
            };
            let batches: Vec<RecordBatch> = match results.try_collect().await {
                Ok(b) => b,
                Err(_) => return Vec::new(),
            };
            let mut ids = Vec::new();
            for batch in &batches {
                if let Some(col) = batch.column_by_name("bundle_id") {
                    if let Some(arr) = col.as_any().downcast_ref::<StringArray>() {
                        for i in 0..arr.len() {
                            if let Ok(id) = arr.value(i).parse::<BundleId>() {
                                ids.push(id);
                            }
                        }
                    }
                }
            }
            ids
        })
    }

    fn modalities(&self) -> Vec<String> {
        let rt = Arc::clone(&self.rt);
        rt.block_on(async { self.conn.table_names().execute().await.unwrap_or_default() })
    }
}

// ---------------------------------------------------------------------------
// Arrow schema helpers
// ---------------------------------------------------------------------------

fn table_schema(dim: usize) -> Schema {
    Schema::new(vec![
        Field::new("bundle_id", DataType::Utf8, false),
        Field::new(
            "embedding",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                dim as i32,
            ),
            false,
        ),
        Field::new("metadata", DataType::Utf8, true),
        Field::new("created_at_ms", DataType::Int64, false),
    ])
}

fn embedding_dim_from_schema(schema: &Schema) -> Result<usize, StoreError> {
    let field = schema
        .field_with_name("embedding")
        .map_err(|_| StoreError::Vector("embedding field missing from schema".into()))?;
    match field.data_type() {
        DataType::FixedSizeList(_, dim) => Ok(*dim as usize),
        _ => Err(StoreError::Vector(
            "embedding column is not FixedSizeList".into(),
        )),
    }
}

fn empty_batch(schema: &Arc<Schema>, dim: usize) -> RecordBatch {
    let id_arr = StringArray::from(Vec::<&str>::new());
    let emb_arr = FixedSizeListArray::from_iter_primitive::<arrow_array::types::Float32Type, _, _>(
        std::iter::empty::<Option<Vec<Option<f32>>>>(),
        dim as i32,
    );
    let meta_arr = StringArray::from(Vec::<Option<&str>>::new());
    let ts_arr = Int64Array::from(Vec::<i64>::new());
    RecordBatch::try_new(
        Arc::clone(schema),
        vec![
            Arc::new(id_arr),
            Arc::new(emb_arr),
            Arc::new(meta_arr),
            Arc::new(ts_arr),
        ],
    )
    .expect("empty batch always valid")
}

fn single_row_batch(
    schema: &Arc<Schema>,
    bundle_id: BundleId,
    vector: &[f32],
    dim: usize,
) -> Result<RecordBatch, StoreError> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let id_arr = StringArray::from(vec![bundle_id.to_string()]);
    let items: Vec<Option<f32>> = vector.iter().map(|&v| Some(v)).collect();
    let emb_arr = FixedSizeListArray::from_iter_primitive::<arrow_array::types::Float32Type, _, _>(
        std::iter::once(Some(items)),
        dim as i32,
    );
    let meta_arr = StringArray::from(vec![Option::<&str>::None]);
    let ts_arr = Int64Array::from(vec![now_ms]);

    RecordBatch::try_new(
        Arc::clone(schema),
        vec![
            Arc::new(id_arr),
            Arc::new(emb_arr),
            Arc::new(meta_arr),
            Arc::new(ts_arr),
        ],
    )
    .map_err(|e| StoreError::Vector(format!("build record batch: {e}")))
}

// ---------------------------------------------------------------------------
// Tests (only compiled under the `lancedb` feature)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use uuid::Uuid;

    fn open_tmp() -> (TempDir, LanceDbIndex) {
        let tmp = TempDir::new().expect("tempdir");
        let idx = LanceDbIndex::open(tmp.path()).expect("open lancedb index");
        (tmp, idx)
    }

    /// Basic smoke test — write three vectors, search, confirm ordering.
    #[test]
    fn upsert_then_search_returns_results() {
        let (_tmp, idx) = open_tmp();
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        let c = Uuid::from_u128(3);
        idx.upsert(a, "text", &[1.0, 0.0, 0.0]).unwrap();
        idx.upsert(b, "text", &[0.0, 1.0, 0.0]).unwrap();
        idx.upsert(c, "text", &[0.0, 0.0, 1.0]).unwrap();

        let hits = idx.search("text", &[0.0, 1.0, 0.0], 3).unwrap();
        assert_eq!(hits.len(), 3, "should return 3 hits");
        assert_eq!(hits[0].bundle_id, b, "B should be nearest");
        assert!(hits[0].similarity >= hits[1].similarity);
    }

    #[test]
    fn dim_mismatch_is_rejected() {
        let (_tmp, idx) = open_tmp();
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        idx.upsert(a, "text", &[1.0, 2.0, 3.0]).unwrap();
        let err = idx.upsert(b, "text", &[1.0, 2.0]).unwrap_err();
        assert!(matches!(err, StoreError::Vector(_)));
    }

    #[test]
    fn empty_modality_search_returns_empty() {
        let (_tmp, idx) = open_tmp();
        let hits = idx.search("absent", &[1.0, 2.0], 5).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn remove_is_idempotent() {
        let (_tmp, idx) = open_tmp();
        let a = Uuid::from_u128(10);
        idx.upsert(a, "text", &[1.0, 0.0]).unwrap();
        idx.remove(a, "text").unwrap();
        idx.remove(a, "text").unwrap();
        idx.remove(a, "absent_modality").unwrap();
    }

    #[test]
    fn ids_for_modality_tracks_inserts_and_removes() {
        let (_tmp, idx) = open_tmp();
        let a = Uuid::from_u128(100);
        let b = Uuid::from_u128(200);
        idx.upsert(a, "intent", &[1.0]).unwrap();
        idx.upsert(b, "intent", &[0.5]).unwrap();
        let ids = idx.ids_for_modality("intent");
        assert!(ids.contains(&a));
        assert!(ids.contains(&b));

        idx.remove(a, "intent").unwrap();
        let ids_after = idx.ids_for_modality("intent");
        assert!(!ids_after.contains(&a));
        assert!(ids_after.contains(&b));
    }

    #[test]
    fn modalities_lists_all_tables() {
        let (_tmp, idx) = open_tmp();
        let x = Uuid::from_u128(1);
        idx.upsert(x, "text", &[1.0]).unwrap();
        idx.upsert(x, "intent", &[1.0]).unwrap();
        let mods = idx.modalities();
        assert!(mods.contains(&"text".to_string()));
        assert!(mods.contains(&"intent".to_string()));
    }

    #[test]
    fn persists_across_reopen() {
        let tmp = TempDir::new().expect("tempdir");
        let id = Uuid::from_u128(999);
        {
            let idx = LanceDbIndex::open(tmp.path()).expect("open");
            idx.upsert(id, "text", &[1.0, 0.0]).unwrap();
        }
        // Re-open and confirm the entry is still there.
        let idx2 = LanceDbIndex::open(tmp.path()).expect("reopen");
        let ids = idx2.ids_for_modality("text");
        assert!(ids.contains(&id), "id should survive reopen");
    }
}
