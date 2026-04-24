//! Unified resource manager with GUID registry and ref-counting.
//!
//! Single loader for shaders, fonts, videos, themes, plugin WASM, and
//! agent system prompts. One copy per path, ref-counted across sessions.

use std::any::Any;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

/// Unique resource identifier, derived from the canonical path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResourceId(pub u64);

impl ResourceId {
    /// Create a [`ResourceId`] from a path string by hashing it.
    pub fn from_path(path: &str) -> Self {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut hasher);
        Self(hasher.finish())
    }
}

/// A loaded resource. Implemented by each resource type.
pub trait Resource: Send + Sync + 'static {
    /// Short human-readable kind tag (e.g. `"shader"`, `"font"`).
    fn kind(&self) -> &'static str;

    /// Estimated memory footprint in bytes.
    fn size_hint(&self) -> usize;

    /// Downcast helper.
    fn as_any(&self) -> &dyn Any;
}

/// Status of a resource load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadStatus {
    /// Not yet started or unknown.
    NotLoaded,
    /// Loading in progress.
    Loading,
    /// Successfully loaded.
    Ready,
    /// Load failed.
    Failed,
}

/// Internal entry for a managed resource.
struct ResourceEntry {
    resource: Option<Arc<dyn Resource>>,
    status: LoadStatus,
    ref_count: AtomicUsize,
    #[allow(dead_code)]
    path: String,
}

/// The central resource manager.
pub struct ResourceManager {
    resources: Mutex<HashMap<ResourceId, ResourceEntry>>,
}

impl ResourceManager {
    /// Create an empty resource manager.
    pub fn new() -> Self {
        Self {
            resources: Mutex::new(HashMap::new()),
        }
    }

    /// Load a resource synchronously. Returns the existing handle when the
    /// same path has already been loaded (deduplication by path).
    pub fn load_or_insert(
        &self,
        path: &str,
        loader: impl FnOnce() -> anyhow::Result<Arc<dyn Resource>>,
    ) -> anyhow::Result<ResourceHandle> {
        let id = ResourceId::from_path(path);

        let mut resources = self
            .resources
            .lock()
            .map_err(|_| anyhow::anyhow!("lock poisoned"))?;

        if let Some(entry) = resources.get(&id) {
            if entry.status == LoadStatus::Ready {
                entry.ref_count.fetch_add(1, Ordering::Relaxed);
                return Ok(ResourceHandle { id });
            }
        }

        let resource = loader()?;
        resources.insert(
            id,
            ResourceEntry {
                resource: Some(resource),
                status: LoadStatus::Ready,
                ref_count: AtomicUsize::new(1),
                path: path.to_string(),
            },
        );

        Ok(ResourceHandle { id })
    }

    /// Register a placeholder for a resource that will be loaded
    /// asynchronously in the background.
    pub fn register_pending(&self, path: &str) -> ResourceHandle {
        let id = ResourceId::from_path(path);

        if let Ok(mut resources) = self.resources.lock() {
            if let Some(entry) = resources.get(&id) {
                entry.ref_count.fetch_add(1, Ordering::Relaxed);
                return ResourceHandle { id };
            }

            resources.insert(
                id,
                ResourceEntry {
                    resource: None,
                    status: LoadStatus::Loading,
                    ref_count: AtomicUsize::new(1),
                    path: path.to_string(),
                },
            );
        }

        ResourceHandle { id }
    }

    /// Complete an async load by inserting the loaded resource data.
    pub fn complete_load(
        &self,
        id: ResourceId,
        resource: Arc<dyn Resource>,
    ) -> anyhow::Result<()> {
        let mut resources = self
            .resources
            .lock()
            .map_err(|_| anyhow::anyhow!("lock poisoned"))?;

        let Some(entry) = resources.get_mut(&id) else {
            anyhow::bail!("resource {id:?} not registered");
        };
        entry.resource = Some(resource);
        entry.status = LoadStatus::Ready;
        Ok(())
    }

    /// Mark an async load as failed.
    pub fn fail_load(&self, id: ResourceId) {
        if let Ok(mut resources) = self.resources.lock() {
            if let Some(entry) = resources.get_mut(&id) {
                entry.status = LoadStatus::Failed;
            }
        }
    }

    /// Try to get a loaded resource. Returns `None` if not ready.
    pub fn try_get(&self, id: ResourceId) -> Option<Arc<dyn Resource>> {
        let resources = self.resources.lock().ok()?;
        let entry = resources.get(&id)?;
        if entry.status == LoadStatus::Ready {
            entry.resource.clone()
        } else {
            None
        }
    }

    /// Get the load status of a resource.
    pub fn status(&self, id: ResourceId) -> LoadStatus {
        self.resources
            .lock()
            .ok()
            .and_then(|r| r.get(&id).map(|e| e.status))
            .unwrap_or(LoadStatus::NotLoaded)
    }

    /// Release a reference. If ref count hits zero, resource stays loaded
    /// until [`gc`](Self::gc) is called.
    pub fn release(&self, id: ResourceId) {
        if let Ok(resources) = self.resources.lock() {
            if let Some(entry) = resources.get(&id) {
                let prev = entry.ref_count.load(Ordering::Relaxed);
                if prev == 0 {
                    log::error!("ResourceManager: release() called on {id:?} with ref_count already 0");
                    debug_assert!(false, "ref-count underflow on resource {id:?}");
                    return;
                }
                entry.ref_count.fetch_sub(1, Ordering::Relaxed);
            }
        }
    }

    /// Remove all resources with zero references. Returns the number of
    /// entries removed.
    pub fn gc(&self) -> usize {
        let Ok(mut resources) = self.resources.lock() else {
            return 0;
        };
        let before = resources.len();
        resources.retain(|_, entry| entry.ref_count.load(Ordering::Relaxed) > 0);
        before - resources.len()
    }

    /// Total count of tracked resources (loaded or pending).
    pub fn count(&self) -> usize {
        self.resources.lock().map(|r| r.len()).unwrap_or(0)
    }

    /// Total memory usage estimate across all loaded resources.
    pub fn memory_usage(&self) -> usize {
        let Ok(resources) = self.resources.lock() else {
            return 0;
        };
        resources
            .values()
            .filter_map(|e| e.resource.as_ref())
            .map(|r| r.size_hint())
            .sum()
    }
}

impl Default for ResourceManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Handle to a loaded resource. Dropping does **not** auto-release; call
/// [`ResourceManager::release`] explicitly when done.
pub struct ResourceHandle {
    pub id: ResourceId,
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestResource {
        #[allow(dead_code)]
        data: String,
        size: usize,
    }

    impl Resource for TestResource {
        fn kind(&self) -> &'static str {
            "test"
        }
        fn size_hint(&self) -> usize {
            self.size
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn make_test_resource(data: &str, size: usize) -> Arc<dyn Resource> {
        Arc::new(TestResource {
            data: data.to_string(),
            size,
        })
    }

    #[test]
    fn load_returns_handle() {
        let mgr = ResourceManager::new();
        let handle = mgr.load_or_insert("test.txt", || Ok(make_test_resource("hello", 5)));
        assert!(handle.is_ok());
        assert_eq!(mgr.count(), 1);
    }

    #[test]
    fn same_path_returns_same_id() {
        let mgr = ResourceManager::new();
        let h1 = mgr
            .load_or_insert("test.txt", || Ok(make_test_resource("a", 1)))
            .unwrap();
        let h2 = mgr
            .load_or_insert("test.txt", || Ok(make_test_resource("b", 1)))
            .unwrap();
        assert_eq!(h1.id, h2.id);
        assert_eq!(mgr.count(), 1); // not duplicated
    }

    #[test]
    fn try_get_returns_resource() {
        let mgr = ResourceManager::new();
        let handle = mgr
            .load_or_insert("test.txt", || Ok(make_test_resource("data", 4)))
            .unwrap();
        let resource = mgr.try_get(handle.id);
        assert!(resource.is_some());
        assert_eq!(resource.unwrap().kind(), "test");
    }

    #[test]
    fn release_and_gc() {
        let mgr = ResourceManager::new();
        let handle = mgr
            .load_or_insert("test.txt", || Ok(make_test_resource("data", 4)))
            .unwrap();
        let id = handle.id;
        mgr.release(id);
        let removed = mgr.gc();
        assert_eq!(removed, 1);
        assert_eq!(mgr.count(), 0);
    }

    #[test]
    fn gc_keeps_referenced() {
        let mgr = ResourceManager::new();
        let _h = mgr
            .load_or_insert("test.txt", || Ok(make_test_resource("data", 4)))
            .unwrap();
        // handle still alive, ref_count = 1
        let removed = mgr.gc();
        assert_eq!(removed, 0);
        assert_eq!(mgr.count(), 1);
    }

    #[test]
    fn async_load_flow() {
        let mgr = ResourceManager::new();
        let handle = mgr.register_pending("video.mp4");

        assert_eq!(mgr.status(handle.id), LoadStatus::Loading);
        assert!(mgr.try_get(handle.id).is_none());

        mgr.complete_load(handle.id, make_test_resource("video_data", 1024))
            .unwrap();

        assert_eq!(mgr.status(handle.id), LoadStatus::Ready);
        assert!(mgr.try_get(handle.id).is_some());
    }

    #[test]
    fn failed_load() {
        let mgr = ResourceManager::new();
        let handle = mgr.register_pending("missing.txt");
        mgr.fail_load(handle.id);
        assert_eq!(mgr.status(handle.id), LoadStatus::Failed);
    }

    #[test]
    fn memory_usage_sums() {
        let mgr = ResourceManager::new();
        mgr.load_or_insert("a.txt", || Ok(make_test_resource("a", 100)))
            .unwrap();
        mgr.load_or_insert("b.txt", || Ok(make_test_resource("b", 200)))
            .unwrap();
        assert_eq!(mgr.memory_usage(), 300);
    }

    #[test]
    fn resource_id_deterministic() {
        let id1 = ResourceId::from_path("shaders/crt.wgsl");
        let id2 = ResourceId::from_path("shaders/crt.wgsl");
        assert_eq!(id1, id2);
    }
}
