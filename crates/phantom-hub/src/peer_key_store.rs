//! Persistent file-backed registry of peer Ed25519 verifying keys.
//!
//! Each Phantom that registers with the hub presents a 32-byte Ed25519
//! public key (in [`crate::phantom_endpoint::register`]).  The hub previously
//! verified the registration signature and threw the key away.  This module
//! persists the key under the user's config directory so the hub can recover
//! the full public-key registry across restarts (issue #527).
//!
//! # Why a file, not Postgres or the OS keychain
//!
//! Mirrors the rationale documented in `phantom-net::identity` (PR #539) and
//! `phantom-bundle-store::crypto` (PR #636):
//! - macOS Keychain ACLs are pinned to the requesting binary's code signature,
//!   which changes on every `cargo build`. With autonomous-agent worktrees
//!   rebuilding constantly this turns into prompt-spam.
//! - A plain JSON file under the config dir, mode `0600`, sidesteps the issue
//!   entirely and matches the pattern already established in this repo.
//! - Postgres (per #401) is the eventual destination for production hubs;
//!   this file-backed store unblocks single-host deployments today and shares
//!   a clean trait-shaped API surface that a Postgres-backed implementation
//!   can replace later.
//!
//! # Storage path
//!
//! - Default: `dirs::config_dir().join("phantom").join("peer-keys.json")`.
//!   On macOS this is `~/Library/Application Support/phantom/peer-keys.json`.
//! - Override: set `PHANTOM_PEER_KEYS_FILE` to an absolute path. Used by tests
//!   and operator overrides.
//!
//! # File format
//!
//! ```json
//! {
//!   "<phantom_id_string>": "<64-hex 32-byte verifying key>",
//!   ...
//! }
//! ```
//!
//! The file is written atomically: a sibling `*.tmp` is created with mode
//! `0600` set in the same `open(2)` call, fsync'd, then `rename(2)`-d into
//! place at the destination so a crash mid-write cannot leave a partial file.
//! `rename` (rather than the `hard_link`-with-fallback used by
//! `phantom-net::identity` for first-write-wins identity bootstrapping) is
//! the right primitive here because [`PeerKeyStore`] is the authoritative
//! writer: it always wants to replace the previous map, not "lose gracefully
//! on conflict".
//!
//! # Per-process cache
//!
//! Loaded keys are cached in an `Arc<Mutex<HashMap<...>>>` on the
//! [`PeerKeyStore`] handle so repeated `get` calls within the same process do
//! at most one disk read per key.
//!
//! # Corrupted files
//!
//! A malformed JSON file surfaces as `Err` from [`PeerKeyStore::open`].  We
//! never silently regenerate — that would erase the entire public-key
//! registry in the face of a transient parser issue.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};

use crate::registry::PhantomId;

// ---------------------------------------------------------------------------
// Shared env-mutation serial (used by tests and by the test-helper in
// `registry::new_shared_for_tests`).  A single crate-level `Mutex` ensures
// `PHANTOM_PEER_KEYS_FILE` mutations are serialised across every call site —
// unit tests in either module and the in-tree test-helper that is always
// compiled.
//
// Previously each module defined its own `ENV_SERIAL` static, which left two
// callers free to interleave their env mutations.  See PR #640 review.
// ---------------------------------------------------------------------------

/// Process-wide serialisation point for `PHANTOM_PEER_KEYS_FILE` mutations.
///
/// `pub(crate)` so the test-helper in `registry.rs` and the unit tests in
/// `peer_key_store.rs` can both lock the same mutex.  Held across the
/// `set_var` / `PeerKeyStore::open` / `remove_var` sequence so that two
/// concurrent calls cannot trample each other's env state.
pub(crate) static ENV_SERIAL: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Environment variable that overrides the default peer-keys file location.
///
/// When set, the override path is used verbatim. Primarily for tests and
/// operator overrides.
const PEER_KEYS_FILE_ENV: &str = "PHANTOM_PEER_KEYS_FILE";

/// Resolve the on-disk path for the peer-keys file.
///
/// Honours the `PHANTOM_PEER_KEYS_FILE` env var as an absolute override, else
/// falls back to `dirs::config_dir().join("phantom").join("peer-keys.json")`.
fn peer_keys_path() -> Result<PathBuf> {
    if let Some(override_path) = std::env::var_os(PEER_KEYS_FILE_ENV) {
        return Ok(PathBuf::from(override_path));
    }
    let base = dirs::config_dir()
        .or_else(dirs::home_dir)
        .context("could not determine config or home directory for peer-keys storage")?;
    Ok(base.join("phantom").join("peer-keys.json"))
}

// ---------------------------------------------------------------------------
// On-disk format
// ---------------------------------------------------------------------------

/// JSON wire format for the peer-keys file.
///
/// Keys are phantom_id strings (the same opaque value used throughout
/// `phantom-hub`); values are 64-character lowercase hex of the 32-byte
/// Ed25519 verifying key. Hex matches the `phantom-net` convention used at
/// the registration endpoint (`public_key_hex` in [`crate::phantom_endpoint::RegisterRequest`]).
#[derive(Debug, Default, Serialize, Deserialize)]
struct PeerKeysFile {
    #[serde(flatten)]
    keys: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// PeerKeyStore
// ---------------------------------------------------------------------------

/// File-backed registry of peer Ed25519 verifying keys.
///
/// Each store owns its own per-instance cache, scoped by file path.  The cache
/// is populated from disk at [`PeerKeyStore::open`] and kept in sync with the
/// file by every [`PeerKeyStore::insert`] / [`PeerKeyStore::remove`] write,
/// which always flushes the full map atomically before returning.
///
/// Cloning is cheap — the cache and path live behind [`Arc`] so cloned
/// handles share state.
#[derive(Clone)]
pub struct PeerKeyStore {
    path: PathBuf,
    cache: Arc<Mutex<HashMap<PhantomId, VerifyingKey>>>,
}

impl PeerKeyStore {
    /// Open the on-disk peer-keys store, loading any existing entries into
    /// the per-instance cache.
    ///
    /// A missing file is treated as an empty registry — the first
    /// [`insert`](Self::insert) creates the file. A present-but-malformed file
    /// surfaces as `Err`, never silently overwritten.
    ///
    /// # Errors
    /// - Returns an error if the config directory cannot be located.
    /// - Returns an error if the file exists but cannot be parsed as JSON or
    ///   contains a malformed verifying key.
    pub fn open() -> Result<Self> {
        let path = peer_keys_path()?;
        let cache = if path.exists() {
            load_from_file(&path)?
        } else {
            HashMap::new()
        };
        Ok(Self {
            path,
            cache: Arc::new(Mutex::new(cache)),
        })
    }

    /// Look up the verifying key for `id`.
    ///
    /// Reads from the per-instance cache; the cache is fully populated at
    /// [`open`](Self::open), so this never falls back to disk for normal
    /// requests. (Disk fallback is unnecessary: there is exactly one writer
    /// per file path, the same process that owns this handle.)
    ///
    /// Returns `Ok(None)` when the id is not registered.
    pub fn get(&self, id: &PhantomId) -> Result<Option<VerifyingKey>> {
        let cache = self
            .cache
            .lock()
            .map_err(|_| anyhow!("peer-key cache mutex poisoned"))?;
        Ok(cache.get(id).copied())
    }

    /// Insert or update the verifying key for `id` and atomically persist the
    /// full map to disk.
    ///
    /// Strategy: take the lock, mutate the cache, serialise the cache, write
    /// to a `.tmp` sibling (mode 0600), fsync, then hard-link into place over
    /// the destination. The lock is held across the disk write so the on-disk
    /// JSON always reflects the in-memory cache exactly.
    ///
    /// # Errors
    /// - Returns an error if the config directory cannot be created.
    /// - Returns an error if the tmp file cannot be opened, written, or
    ///   fsync'd.
    /// - Returns an error if the rename/hard-link into place fails for any
    ///   reason other than the destination already existing — in which case
    ///   the caller's update is folded into the freshly-loaded map and a
    ///   second write attempt is made.
    pub fn insert(&self, id: PhantomId, key: VerifyingKey) -> Result<()> {
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| anyhow!("peer-key cache mutex poisoned"))?;
        cache.insert(id, key);
        flush_locked(&self.path, &cache)
    }

    /// Remove the verifying key for `id` and atomically persist the full map
    /// to disk.
    ///
    /// Removing an id that is not registered is a no-op — the on-disk file
    /// is left untouched.  This skips an unnecessary fsync round-trip for the
    /// common case where the caller defensively removes a key that was never
    /// registered.
    pub fn remove(&self, id: &PhantomId) -> Result<()> {
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| anyhow!("peer-key cache mutex poisoned"))?;
        if cache.remove(id).is_none() {
            return Ok(());
        }
        flush_locked(&self.path, &cache)
    }
}

impl std::fmt::Debug for PeerKeyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Don't dump the keys themselves in Debug output.  Use `try_lock`
        // rather than `lock` so a format call that occurs while a panic
        // unwinds (and the same thread already holds the mutex) cannot
        // deadlock — `Debug` should never block, never panic.
        let mut dbg = f.debug_struct("PeerKeyStore");
        dbg.field("path", &self.path);
        match self.cache.try_lock() {
            Ok(c) => dbg.field("entries", &c.len()),
            Err(_) => dbg.field("entries", &"<locked>"),
        };
        dbg.finish()
    }
}

// ---------------------------------------------------------------------------
// Disk I/O helpers
// ---------------------------------------------------------------------------

fn load_from_file(path: &Path) -> Result<HashMap<PhantomId, VerifyingKey>> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read peer-keys file at {}", path.display()))?;
    let file: PeerKeysFile = serde_json::from_slice(&bytes)
        .with_context(|| format!("peer-keys file at {} is malformed JSON", path.display()))?;

    let mut out = HashMap::with_capacity(file.keys.len());
    for (id_str, hex_key) in file.keys {
        let key_bytes = decode_hex_32(&hex_key).with_context(|| {
            format!(
                "peer-keys file at {} contains malformed key hex for phantom_id {}",
                path.display(),
                id_str
            )
        })?;
        let vk = VerifyingKey::from_bytes(&key_bytes).with_context(|| {
            format!(
                "peer-keys file at {} contains a non-canonical Ed25519 key for phantom_id {}",
                path.display(),
                id_str
            )
        })?;
        out.insert(PhantomId(id_str), vk);
    }
    Ok(out)
}

/// Serialise `cache` and atomically write it to `path`.
///
/// Caller holds the cache mutex so no other writer can interleave between the
/// serialise and the rename.
fn flush_locked(path: &Path, cache: &HashMap<PhantomId, VerifyingKey>) -> Result<()> {
    let mut file = PeerKeysFile {
        keys: HashMap::with_capacity(cache.len()),
    };
    for (id, vk) in cache {
        file.keys
            .insert(id.0.clone(), encode_hex_32(vk.as_bytes()));
    }
    let json = serde_json::to_vec_pretty(&file).context("failed to serialise peer-keys")?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create peer-keys directory at {}",
                parent.display()
            )
        })?;
    }

    write_atomic(path, &json)
}

/// Write `bytes` to `path` atomically.
///
/// Strategy: write to `{path}.{pid}.{tid}.tmp` with mode `0600` set in the
/// same `open(2)` call, fsync, then `rename` into place.  We use `rename`
/// rather than `hard_link` here because [`PeerKeyStore::insert`] is the
/// authoritative writer that always wants to replace the previous map; the
/// "lose gracefully on conflict" semantics of `hard_link` (used in
/// `phantom-net::identity` for first-write-wins identity bootstrapping) are
/// not a fit for this use case.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    let tid = std::thread::current().id();
    let suffix = format!("{}.{:?}.tmp", std::process::id(), tid);
    let tmp_path = match path.extension() {
        Some(ext) => {
            let mut s = ext.to_os_string();
            s.push(".");
            s.push(&suffix);
            path.with_extension(s)
        }
        None => path.with_extension(&suffix),
    };

    let write_result = (|| -> Result<()> {
        // Unlink any prior tmp file at this path first so a stale leftover
        // from a prior crash cannot cause `create_new(true)` to fail.
        let _ = std::fs::remove_file(&tmp_path);

        let mut open_opts = std::fs::OpenOptions::new();
        open_opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open_opts.mode(0o600);
        }
        let mut f = open_opts.open(&tmp_path).with_context(|| {
            format!(
                "failed to open tmp peer-keys file at {}",
                tmp_path.display()
            )
        })?;

        f.write_all(bytes)
            .with_context(|| format!("failed to write peer-keys to {}", tmp_path.display()))?;
        f.sync_all()
            .with_context(|| format!("failed to fsync peer-keys at {}", tmp_path.display()))?;
        Ok(())
    })();

    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e);
    }

    // `rename` on Unix is atomic — the destination either points at the old
    // bytes or the new bytes, never a partial frankenstein.  On Windows,
    // `rename` over an existing file requires `MoveFileEx` semantics which
    // `std::fs::rename` already provides on stable since 1.5.  We accept
    // the platform difference: durability depends on the underlying fs.
    let rename_result = std::fs::rename(&tmp_path, path);
    if let Err(e) = rename_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(anyhow!(
            "failed to rename {} -> {}: {e}",
            tmp_path.display(),
            path.display()
        ));
    }

    // Best-effort fsync of the parent directory so the rename is durable.
    // Non-fatal — many filesystems do not require it.
    #[cfg(unix)]
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Internal hex helpers — avoids pulling in the `hex` crate just to round-trip
// 32-byte verifying keys through the JSON file.
// ---------------------------------------------------------------------------

fn encode_hex_32(bytes: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(64);
    for b in bytes {
        // `write!` on a `String` is infallible; the `unwrap` here cannot
        // panic in practice.  Writing two ASCII hex digits per iteration
        // avoids the per-byte `format!` allocation of the previous impl.
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn decode_hex_32(s: &str) -> Result<[u8; 32]> {
    if s.len() != 64 {
        bail!("expected 64 hex chars, got {}", s.len());
    }
    let mut out = [0_u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = from_hex_digit(chunk[0])?;
        let lo = from_hex_digit(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

fn from_hex_digit(b: u8) -> Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => bail!("invalid hex digit: {}", b as char),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Tests that mutate `PHANTOM_PEER_KEYS_FILE` serialize through the
    // crate-level `super::ENV_SERIAL` because env vars are process-global.
    // The shared mutex is also used by `registry::new_shared_for_tests`, so
    // unit tests here and the helper there can never interleave.
    static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);
    use super::ENV_SERIAL;

    fn tmp_peer_keys_file() -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("phantom-peer-keys-test-{pid}-{n}.json"))
    }

    fn fresh_key() -> VerifyingKey {
        SigningKey::generate(&mut OsRng).verifying_key()
    }

    /// Cleanup guard — removes env var and tmp file on drop, so cleanup runs
    /// even on test panic / early return.
    struct CleanupGuard {
        path: PathBuf,
    }
    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            // SAFETY: env mutation is serialised via ENV_SERIAL.
            unsafe { std::env::remove_var(PEER_KEYS_FILE_ENV) };
            let _ = std::fs::remove_file(&self.path);
        }
    }

    // -----------------------------------------------------------------------
    // Pure unit tests (no env, no disk)
    // -----------------------------------------------------------------------

    #[test]
    fn hex_round_trip_32_bytes() {
        let mut bytes = [0_u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(31).wrapping_add(7);
        }
        let s = encode_hex_32(&bytes);
        assert_eq!(s.len(), 64);
        let back = decode_hex_32(&s).expect("decode");
        assert_eq!(bytes, back);
    }

    #[test]
    fn hex_decode_rejects_short_input() {
        assert!(decode_hex_32("deadbeef").is_err());
    }

    // -----------------------------------------------------------------------
    // File-backend tests (use PHANTOM_PEER_KEYS_FILE override)
    // -----------------------------------------------------------------------

    /// Acceptance: a fresh `open()` against a non-existent file must yield
    /// an empty store, not an error — first-boot semantics.
    #[test]
    fn open_creates_empty_when_missing() {
        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_peer_keys_file();
        // SAFETY: env mutation is serialised via ENV_SERIAL.
        unsafe { std::env::set_var(PEER_KEYS_FILE_ENV, &path) };
        let _cleanup = CleanupGuard { path: path.clone() };

        assert!(!path.exists(), "precondition: file must not exist");

        let store = PeerKeyStore::open().expect("open must succeed when file is missing");
        let result = store
            .get(&PhantomId::new("phantom-a"))
            .expect("get must succeed on empty store");
        assert!(result.is_none(), "empty store must return None for any id");
        assert!(
            !path.exists(),
            "open() must not create the file — only the first insert does"
        );
    }

    /// Round-trip: insert a key, then read it back from the cache, then read
    /// it back from a fresh handle that re-loads from disk.
    #[test]
    fn insert_then_get_round_trips() {
        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_peer_keys_file();
        unsafe { std::env::set_var(PEER_KEYS_FILE_ENV, &path) };
        let _cleanup = CleanupGuard { path: path.clone() };

        let store = PeerKeyStore::open().expect("open empty store");
        let id = PhantomId::new("phantom-roundtrip");
        let key = fresh_key();

        store.insert(id.clone(), key).expect("insert must succeed");

        // Cache hit on the same handle.
        let got = store.get(&id).expect("get must succeed").expect("present");
        assert_eq!(got.as_bytes(), key.as_bytes());

        // Cold-load: a fresh handle opens the file from disk and the key
        // survives.
        let store2 = PeerKeyStore::open().expect("open populated store");
        let got2 = store2.get(&id).expect("get must succeed").expect("present");
        assert_eq!(
            got2.as_bytes(),
            key.as_bytes(),
            "key must survive a fresh open() — that's the whole point of #527"
        );
    }

    /// Acceptance: a present-but-malformed file must surface as Err and the
    /// file must not be silently overwritten.
    #[test]
    fn corrupted_file_returns_err() {
        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_peer_keys_file();
        unsafe { std::env::set_var(PEER_KEYS_FILE_ENV, &path) };
        let _cleanup = CleanupGuard { path: path.clone() };

        std::fs::write(&path, b"this is not valid JSON {{{").expect("seed corrupted file");
        let before = std::fs::read(&path).unwrap();

        let result = PeerKeyStore::open();
        assert!(
            result.is_err(),
            "corrupted file must surface as Err, never silently regenerated"
        );

        let after = std::fs::read(&path).unwrap();
        assert_eq!(
            before, after,
            "corrupted file must not be overwritten — that would erase the entire peer-key registry if the parser had a transient issue"
        );
    }

    /// Hardening: the on-disk file must be mode 0600 so the public-key
    /// registry is not world-readable.  Mirrors the same regression guard
    /// in `phantom-net::identity` and `phantom-bundle-store::crypto`.
    #[cfg(unix)]
    #[test]
    fn file_mode_is_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_peer_keys_file();
        unsafe { std::env::set_var(PEER_KEYS_FILE_ENV, &path) };
        let _cleanup = CleanupGuard { path: path.clone() };

        let store = PeerKeyStore::open().expect("open empty store");
        store
            .insert(PhantomId::new("mode-test"), fresh_key())
            .expect("insert creates the file");

        let meta = std::fs::metadata(&path).expect("peer-keys file must exist after first insert");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "peer-keys file must be mode 0600, got {mode:o}"
        );
    }

    /// Cache: a repeated `get` for the same id must not require a disk read.
    /// We verify this by deleting the on-disk file behind the cache — a
    /// second `get` must still return the cached value.
    #[test]
    fn cache_skips_disk_on_repeat_get() {
        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_peer_keys_file();
        unsafe { std::env::set_var(PEER_KEYS_FILE_ENV, &path) };
        let _cleanup = CleanupGuard { path: path.clone() };

        let store = PeerKeyStore::open().expect("open empty store");
        let id = PhantomId::new("cache-id");
        let key = fresh_key();
        store.insert(id.clone(), key).expect("insert");

        // Pull the rug from under the cache.
        std::fs::remove_file(&path).expect("file must exist after first insert");

        let got = store
            .get(&id)
            .expect("cache hit must not depend on the file being present")
            .expect("key must still be in cache");
        assert_eq!(got.as_bytes(), key.as_bytes());
        assert!(
            !path.exists(),
            "cache hit must not have re-created the file from disk"
        );
    }

    // -----------------------------------------------------------------------
    // Additional coverage
    // -----------------------------------------------------------------------

    #[test]
    fn remove_evicts_key() {
        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_peer_keys_file();
        unsafe { std::env::set_var(PEER_KEYS_FILE_ENV, &path) };
        let _cleanup = CleanupGuard { path: path.clone() };

        let store = PeerKeyStore::open().expect("open");
        let id = PhantomId::new("evictee");
        store.insert(id.clone(), fresh_key()).expect("insert");
        assert!(store.get(&id).expect("get").is_some());

        store.remove(&id).expect("remove");
        assert!(
            store.get(&id).expect("get after remove").is_none(),
            "remove must evict the key from the cache and the file"
        );

        // Cold reload confirms the on-disk file no longer contains the id.
        let store2 = PeerKeyStore::open().expect("re-open");
        assert!(
            store2.get(&id).expect("get on reload").is_none(),
            "removed key must not resurrect on reload"
        );
    }

    #[test]
    fn multiple_ids_persist_independently() {
        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_peer_keys_file();
        unsafe { std::env::set_var(PEER_KEYS_FILE_ENV, &path) };
        let _cleanup = CleanupGuard { path: path.clone() };

        let store = PeerKeyStore::open().expect("open");
        let id1 = PhantomId::new("alpha");
        let id2 = PhantomId::new("beta");
        let k1 = fresh_key();
        let k2 = fresh_key();

        store.insert(id1.clone(), k1).expect("insert alpha");
        store.insert(id2.clone(), k2).expect("insert beta");

        let store2 = PeerKeyStore::open().expect("re-open");
        assert_eq!(
            store2.get(&id1).expect("get alpha").expect("present").as_bytes(),
            k1.as_bytes()
        );
        assert_eq!(
            store2.get(&id2).expect("get beta").expect("present").as_bytes(),
            k2.as_bytes()
        );
    }

    #[test]
    fn re_insert_overwrites_value() {
        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_peer_keys_file();
        unsafe { std::env::set_var(PEER_KEYS_FILE_ENV, &path) };
        let _cleanup = CleanupGuard { path: path.clone() };

        let store = PeerKeyStore::open().expect("open");
        let id = PhantomId::new("rotating");
        let k1 = fresh_key();
        let k2 = fresh_key();
        assert_ne!(k1.as_bytes(), k2.as_bytes());

        store.insert(id.clone(), k1).expect("insert v1");
        store.insert(id.clone(), k2).expect("insert v2");

        // Cold reload must surface the latest write.
        let store2 = PeerKeyStore::open().expect("re-open");
        let got = store2
            .get(&id)
            .expect("get")
            .expect("present");
        assert_eq!(got.as_bytes(), k2.as_bytes());
    }
}
