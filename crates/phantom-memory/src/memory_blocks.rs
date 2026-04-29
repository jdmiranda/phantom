//! Letta-inspired shared K/V memory blocks for multi-agent coordination.
//!
//! Multiple agents share read access to typed K/V `Block`s identified by a
//! `BlockKey` (`namespace` + `label`). Write access is gated by the
//! `Permission` granted on the `BlockHandle` returned from
//! [`MemoryStore::attach`]. Each block has a monotonically-increasing
//! revision and a `tokio::sync::watch` channel that broadcasts revision
//! bumps to subscribers.
//!
//! # Locking strategy
//!
//! Storage is a `dashmap::DashMap<BlockKey, Arc<Block>>`. The dashmap shards
//! its internal locks by key, so two operations on different blocks never
//! contend. Each `Block` then carries its own `RwLock<String>` for the value
//! payload — chosen over a single global lock so that disjoint agents
//! reading/writing different blocks proceed without serialization. The
//! revision counter is an `AtomicU64`, and the watch sender is read directly
//! (it is internally synchronized).
//!
//! # Race-condition notes
//!
//! - `write` is *not* compare-and-swap: two concurrent writers will both
//!   succeed, each bumping the revision exactly once. Last-writer-wins on
//!   value; subscribers see two distinct revision bumps. If atomic
//!   read-modify-write semantics are needed, a future API addition would
//!   take a closure under the value lock.
//! - `attach` uses `entry().or_insert_with(...)`. If two threads race to
//!   create the same key, only one block is constructed and stored.
//! - `read_only` is set at block creation and is immutable for the block's
//!   lifetime: a later `attach` cannot escalate a read-only block, even if
//!   it requests `Permission::ReadWrite`.
//! - `drop_block` removes the entry from the map but does not invalidate
//!   handles already held by other agents — those keep working against the
//!   `Arc<Block>` they captured. A subsequent `attach` with the same key
//!   creates a *new* block with a fresh revision counter.

use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use tokio::sync::watch;

/// Identifies a memory block by namespace and label.
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct BlockKey {
    pub namespace: String,
    pub label: String,
}

impl BlockKey {
    pub fn new(namespace: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
            label: label.into(),
        }
    }
}

/// Permission granted by a [`BlockHandle`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Read,
    ReadWrite,
}

/// A shared K/V block with a revision counter and change broadcaster.
pub struct Block {
    value: RwLock<String>,
    revision: AtomicU64,
    read_only: bool,
    notifier: watch::Sender<u64>,
}

impl std::fmt::Debug for Block {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Block")
            .field("revision", &self.revision.load(Ordering::Acquire))
            .field("read_only", &self.read_only)
            .finish_non_exhaustive()
    }
}

impl Block {
    fn new(read_only: bool) -> Self {
        let (notifier, _rx) = watch::channel(0);
        Self {
            value: RwLock::new(String::new()),
            revision: AtomicU64::new(0),
            read_only,
            notifier,
        }
    }
}

/// A capability handle to a [`Block`].
///
/// Cloneable so the same capability can be shared among cooperating tasks
/// without re-attaching to the store.
#[derive(Clone)]
pub struct BlockHandle {
    block: Arc<Block>,
    perm: Permission,
}

impl std::fmt::Debug for BlockHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockHandle")
            .field("perm", &self.perm)
            .field("revision", &self.block.revision.load(Ordering::Acquire))
            .field("read_only", &self.block.read_only)
            .finish()
    }
}

impl BlockHandle {
    /// Read the current value.
    #[must_use]
    pub fn read(&self) -> String {
        self.block
            .value
            .read()
            .expect("block value lock poisoned")
            .clone()
    }

    /// Current revision counter. Bumps by exactly 1 per successful write.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.block.revision.load(Ordering::Acquire)
    }

    /// Replace the value, bumping the revision and notifying watchers.
    ///
    /// # Errors
    ///
    /// - [`BlockError::PermissionDenied`] if this handle was attached with
    ///   [`Permission::Read`].
    /// - [`BlockError::ReadOnly`] if the underlying block was created with
    ///   `read_only: true`, regardless of handle permission.
    pub fn write(&self, value: String) -> Result<(), BlockError> {
        if self.perm == Permission::Read {
            return Err(BlockError::PermissionDenied);
        }
        if self.block.read_only {
            return Err(BlockError::ReadOnly);
        }

        {
            let mut guard = self.block.value.write().expect("block value lock poisoned");
            *guard = value;
        }
        let new_rev = self.block.revision.fetch_add(1, Ordering::AcqRel) + 1;
        // send_replace ignores receiver count so the broadcast still succeeds
        // when no one is listening yet.
        self.block.notifier.send_replace(new_rev);
        Ok(())
    }

    /// Subscribe to revision-bump notifications.
    ///
    /// The receiver's initial value is the current revision. Each successful
    /// `write` bumps it; subscribers can `await rx.changed()` to be woken.
    #[must_use]
    pub fn watch_revision(&self) -> watch::Receiver<u64> {
        self.block.notifier.subscribe()
    }

    /// The permission this handle was granted.
    #[must_use]
    pub fn permission(&self) -> Permission {
        self.perm
    }
}

/// Sharded multi-agent memory store.
#[derive(Default)]
pub struct MemoryStore {
    blocks: DashMap<BlockKey, Arc<Block>>,
}

impl MemoryStore {
    #[must_use]
    pub fn new() -> Self {
        Self {
            blocks: DashMap::new(),
        }
    }

    /// Get or create a block at `key`.
    ///
    /// `read_only` only takes effect on the first `attach` for a given
    /// key — for any subsequent attach (even with a different `read_only`
    /// argument) the existing block's `read_only` setting is preserved.
    /// `perm` controls *this handle's* capability and is independent of
    /// other handles to the same block.
    pub fn attach(&self, key: BlockKey, perm: Permission, read_only: bool) -> BlockHandle {
        let block = self
            .blocks
            .entry(key)
            .or_insert_with(|| Arc::new(Block::new(read_only)))
            .clone();
        BlockHandle { block, perm }
    }

    /// Look up an existing block. Does not create.
    #[must_use]
    pub fn get(&self, key: &BlockKey, perm: Permission) -> Option<BlockHandle> {
        self.blocks.get(key).map(|entry| BlockHandle {
            block: entry.value().clone(),
            perm,
        })
    }

    /// Remove a block from the store.
    ///
    /// Existing handles continue to function against their `Arc<Block>`,
    /// but no new lookup via `get` will find this key.
    pub fn drop_block(&self, key: &BlockKey) {
        self.blocks.remove(key);
    }

    /// Number of blocks currently stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Whether the store has no blocks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

/// Errors returned by [`BlockHandle::write`].
#[derive(Debug, thiserror::Error)]
pub enum BlockError {
    #[error("block is read-only")]
    ReadOnly,
    #[error("permission denied: handle is Read but write was attempted")]
    PermissionDenied,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(ns: &str, label: &str) -> BlockKey {
        BlockKey::new(ns, label)
    }

    #[test]
    fn attach_same_key_returns_shared_block() {
        let store = MemoryStore::new();
        let k = key("agent.shared", "scratch");

        let h1 = store.attach(k.clone(), Permission::ReadWrite, false);
        let h2 = store.attach(k.clone(), Permission::ReadWrite, false);

        assert_eq!(h1.revision(), 0);
        assert_eq!(h2.revision(), 0);

        h1.write("from h1".to_string()).unwrap();

        // Both handles observe the same underlying block: revision and value.
        assert_eq!(h2.revision(), 1, "h2 must see h1's write");
        assert_eq!(h2.read(), "from h1");
        assert_eq!(store.len(), 1, "no duplicate block created");
    }

    #[test]
    fn read_after_write_returns_new_value() {
        let store = MemoryStore::new();
        let h = store.attach(key("ns", "k"), Permission::ReadWrite, false);

        assert_eq!(h.read(), "");
        h.write("hello".to_string()).unwrap();
        assert_eq!(h.read(), "hello");
        h.write("world".to_string()).unwrap();
        assert_eq!(h.read(), "world");
    }

    #[test]
    fn write_bumps_revision_by_exactly_one() {
        let store = MemoryStore::new();
        let h = store.attach(key("ns", "k"), Permission::ReadWrite, false);

        assert_eq!(h.revision(), 0);
        h.write("a".to_string()).unwrap();
        assert_eq!(h.revision(), 1);
        h.write("b".to_string()).unwrap();
        assert_eq!(h.revision(), 2);
        h.write("c".to_string()).unwrap();
        assert_eq!(h.revision(), 3);
    }

    #[test]
    fn write_on_read_handle_returns_permission_denied() {
        let store = MemoryStore::new();
        // Create the block with a writer first so it exists.
        let _writer = store.attach(key("ns", "k"), Permission::ReadWrite, false);
        let reader = store.attach(key("ns", "k"), Permission::Read, false);

        let err = reader.write("nope".to_string()).unwrap_err();
        assert!(matches!(err, BlockError::PermissionDenied));
        assert_eq!(reader.revision(), 0, "failed write must not bump revision");
        assert_eq!(reader.read(), "", "failed write must not change value");
    }

    #[test]
    fn write_on_read_only_block_returns_read_only_even_with_readwrite_perm() {
        let store = MemoryStore::new();
        // Create as read-only — subsequent attach can't escalate it.
        let h = store.attach(key("ns", "frozen"), Permission::ReadWrite, true);

        let err = h.write("nope".to_string()).unwrap_err();
        assert!(matches!(err, BlockError::ReadOnly));
        assert_eq!(h.revision(), 0);
        assert_eq!(h.read(), "");
    }

    #[test]
    fn read_only_sticks_for_block_lifetime() {
        let store = MemoryStore::new();
        // First attach pins read_only = true.
        let _frozen = store.attach(key("ns", "k"), Permission::ReadWrite, true);
        // Second attach asks for read_only = false but block stays read-only.
        let later = store.attach(key("ns", "k"), Permission::ReadWrite, false);

        assert!(matches!(
            later.write("x".to_string()).unwrap_err(),
            BlockError::ReadOnly
        ));
    }

    #[tokio::test]
    async fn watch_revision_subscriber_sees_bump() {
        let store = MemoryStore::new();
        let writer = store.attach(key("ns", "k"), Permission::ReadWrite, false);
        let reader = store.attach(key("ns", "k"), Permission::Read, false);

        let mut rx = reader.watch_revision();
        // Initial value is 0 (the current revision).
        assert_eq!(*rx.borrow(), 0);

        writer.write("update".to_string()).unwrap();

        // Wait for the bump notification.
        rx.changed()
            .await
            .expect("watch sender must still be alive");
        assert_eq!(*rx.borrow(), 1);
        assert_eq!(reader.read(), "update");
    }

    #[test]
    fn drop_block_removes_and_get_returns_none() {
        let store = MemoryStore::new();
        let k = key("ns", "ephemeral");
        let _h = store.attach(k.clone(), Permission::ReadWrite, false);

        assert!(store.get(&k, Permission::Read).is_some());
        assert_eq!(store.len(), 1);

        store.drop_block(&k);

        assert!(store.get(&k, Permission::Read).is_none());
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn get_on_missing_key_returns_none() {
        let store = MemoryStore::new();
        let k = key("ns", "ghost");
        assert!(store.get(&k, Permission::Read).is_none());
    }

    #[test]
    fn len_reflects_stored_blocks() {
        let store = MemoryStore::new();
        assert_eq!(store.len(), 0);
        assert!(store.is_empty());

        let _a = store.attach(key("ns", "a"), Permission::ReadWrite, false);
        assert_eq!(store.len(), 1);

        let _b = store.attach(key("ns", "b"), Permission::ReadWrite, false);
        let _c = store.attach(key("other", "a"), Permission::Read, true);
        assert_eq!(store.len(), 3);

        // Re-attaching an existing key does not grow len.
        let _a2 = store.attach(key("ns", "a"), Permission::Read, false);
        assert_eq!(store.len(), 3);

        store.drop_block(&key("ns", "b"));
        assert_eq!(store.len(), 2);
    }

    #[test]
    fn block_key_distinguishes_namespace_and_label() {
        let store = MemoryStore::new();
        let h1 = store.attach(key("ns1", "x"), Permission::ReadWrite, false);
        let h2 = store.attach(key("ns2", "x"), Permission::ReadWrite, false);
        let h3 = store.attach(key("ns1", "y"), Permission::ReadWrite, false);

        h1.write("one".to_string()).unwrap();
        h2.write("two".to_string()).unwrap();
        h3.write("three".to_string()).unwrap();

        assert_eq!(h1.read(), "one");
        assert_eq!(h2.read(), "two");
        assert_eq!(h3.read(), "three");
        assert_eq!(store.len(), 3);
    }

    #[test]
    fn handles_held_after_drop_block_continue_to_work() {
        // Documents the documented race-condition behavior: an old handle
        // keeps its Arc<Block> alive even after the store removes the entry.
        let store = MemoryStore::new();
        let k = key("ns", "k");
        let h = store.attach(k.clone(), Permission::ReadWrite, false);
        h.write("kept".to_string()).unwrap();

        store.drop_block(&k);
        assert!(store.get(&k, Permission::Read).is_none());

        // The dropped-from-store handle still functions.
        assert_eq!(h.read(), "kept");
        h.write("still works".to_string()).unwrap();
        assert_eq!(h.revision(), 2);

        // A fresh attach creates a *new* block with revision 0.
        let fresh = store.attach(k, Permission::ReadWrite, false);
        assert_eq!(fresh.revision(), 0);
        assert_eq!(fresh.read(), "");
    }
}
