//! Encryption-at-rest primitives.
//!
//! Two layers of crypto live here:
//!
//! 1. **SQLCipher** uses the master key directly as the database passphrase.
//!    See [`MasterKey::bytes`] — it is fed verbatim into a `PRAGMA key`.
//! 2. **Frame and audio blobs** are sealed with XChaCha20-Poly1305. The
//!    per-blob key is derived as `HKDF-SHA256(master_key, salt = bundle_id,
//!    info = "phantom-bundle-store/blob/v1")`. Each seal generates a fresh
//!    24-byte XNonce.
//!
//! The master key itself is persisted in a JSON file under the user's config
//! directory (mode `0600` on Unix) by [`MasterKey::load_or_generate`]. Tests
//! pass an explicit key via [`MasterKey::from_bytes`].
//!
//! # Why a file, not the OS keychain
//! macOS Keychain ACLs are bound to the requesting binary's code signature.
//! Every `cargo build` produces a binary with a different content hash, so
//! "Always Allow" never sticks across rebuilds. With autonomous-agent
//! worktrees rebuilding constantly this turned into prompt-spam. A plain
//! file under the config dir, mode `0600`, sidesteps the issue entirely.
//! Mirrors the file-store rationale in `phantom-net::identity` (see PR
//! #539; this change closes the parallel keychain entry-point flagged by
//! issue #565).
//!
//! # Storage path
//! - Default: `dirs::config_dir().join("phantom").join("bundle-store").join("master-key.json")`.
//!   On macOS this is
//!   `~/Library/Application Support/phantom/bundle-store/master-key.json`.
//! - Override: set `PHANTOM_BUNDLE_STORE_MASTER_KEY_FILE` to use that exact
//!   path verbatim (used by tests).
//!
//! # File format
//! ```json
//! { "master_key_hex": "<64-hex>" }
//! ```
//! The file is written atomically: a sibling `*.tmp` file is created with
//! mode `0600` set in the same `open(2)` call, fsync'd, then renamed over
//! the destination so a crash mid-write cannot leave a partial file.
//!
//! # Per-process cache
//! Loaded master keys are cached in a process-wide map keyed by file path,
//! so repeated calls within a single process do at most one disk read per
//! path.
//!
//! Sensitive material is wrapped in [`zeroize::Zeroizing`] so it scrubs on
//! drop.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use phantom_bundles::BundleId;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::StoreError;

/// HKDF info string. Bumping this string is equivalent to a key rotation
/// for everything sealed under the previous string.
const HKDF_INFO_BLOB: &[u8] = b"phantom-bundle-store/blob/v1";
/// Length of the XChaCha20-Poly1305 nonce, in bytes.
const XNONCE_LEN: usize = 24;
/// Magic bytes prefixing a serialized [`BlobEnvelope`]. Lets us version the
/// on-disk envelope format independently of anything else.
const ENVELOPE_MAGIC: &[u8; 4] = b"PBE1";

/// Environment variable that overrides the default master-key file location.
///
/// When set, the override path is used verbatim. Primarily for tests and
/// operator overrides.
const MASTER_KEY_FILE_ENV: &str = "PHANTOM_BUNDLE_STORE_MASTER_KEY_FILE";

/// Process-wide cache of loaded master keys, keyed by the resolved file path.
///
/// Avoids re-reading the JSON file on every call within the same process.
/// The cache is heap-only and never written to disk — the only persisted
/// state is the JSON file itself.
static MASTER_KEY_CACHE: LazyLock<Mutex<HashMap<PathBuf, MasterKey>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 32-byte master key. The single root of trust for this store.
///
/// Cloning is cheap (memcpy of 32 bytes) and intentionally allowed so the
/// key can be shared across [`BundleStore`](crate::BundleStore) handles.
/// Drops zero the buffer.
#[derive(Clone)]
pub struct MasterKey(Zeroizing<[u8; 32]>);

impl std::fmt::Debug for MasterKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never log key bytes.
        f.debug_struct("MasterKey").field("len", &32_usize).finish()
    }
}

impl MasterKey {
    /// Construct from raw bytes. Used in tests and as the in-memory form
    /// after pulling from disk.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// Borrow the raw key bytes. Used to feed SQLCipher's `PRAGMA key`.
    #[must_use]
    pub fn bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Load the master key from the on-disk JSON file, generating and
    /// persisting a fresh random key if no file yet exists.
    ///
    /// On first call within a process the file is read (or generated) and
    /// the result cached. Subsequent calls return the cached value with no
    /// disk I/O.
    ///
    /// # Errors
    /// - Returns [`StoreError::MasterKey`] if the config directory cannot
    ///   be located.
    /// - Returns [`StoreError::MasterKey`] if an existing file is present
    ///   but cannot be parsed or contains a malformed key. A parse failure
    ///   does **not** trigger silent regeneration — that would erase a
    ///   real master key in the face of a transient parser issue.
    /// - Returns [`StoreError::MasterKey`] if a fresh file cannot be
    ///   written to disk.
    pub fn load_or_generate() -> Result<Self, StoreError> {
        let path = master_key_path()?;

        // Per-process cache hit path — no disk I/O.
        {
            let cache = MASTER_KEY_CACHE
                .lock()
                .expect("master key cache mutex poisoned");
            if let Some(key) = cache.get(&path) {
                return Ok(key.clone());
            }
        }

        let key = if path.exists() {
            load_from_file(&path)?
        } else {
            generate_and_persist(&path)?
        };

        // Insert into cache (race-tolerant — last writer wins, both copies
        // are equivalent because they were derived from the same on-disk
        // bytes or the same first-writer's bytes).
        {
            let mut cache = MASTER_KEY_CACHE
                .lock()
                .expect("master key cache mutex poisoned");
            cache.entry(path).or_insert_with(|| key.clone());
        }

        Ok(key)
    }

    /// Derive a per-bundle data-encryption key with HKDF-SHA256.
    ///
    /// Salt is the bundle id bytes; info pins the protocol version. Output
    /// is 32 bytes (the natural XChaCha20-Poly1305 key length).
    pub fn derive_bundle_dek(&self, bundle_id: BundleId) -> Result<DataEncryptionKey, StoreError> {
        let hk = Hkdf::<Sha256>::new(Some(bundle_id.as_bytes()), &self.0[..]);
        let mut okm = Zeroizing::new([0_u8; 32]);
        hk.expand(HKDF_INFO_BLOB, &mut okm[..])
            .map_err(|e| StoreError::Crypto(format!("hkdf expand: {e}")))?;
        Ok(DataEncryptionKey(okm))
    }
}

/// Per-bundle data-encryption key. Zeroized on drop.
pub struct DataEncryptionKey(Zeroizing<[u8; 32]>);

impl DataEncryptionKey {
    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new(self.0.as_ref().into())
    }
}

/// Sealed blob envelope: nonce + ciphertext (with appended authentication tag).
///
/// On-disk layout: `MAGIC (4) || nonce (24) || ciphertext (N + 16)`.
#[derive(Debug, Clone)]
pub struct BlobEnvelope {
    /// Per-seal random 24-byte XNonce.
    pub nonce: [u8; XNONCE_LEN],
    /// Ciphertext including the trailing 16-byte Poly1305 auth tag.
    pub ciphertext: Vec<u8>,
}

impl BlobEnvelope {
    /// Serialize to the on-disk byte layout.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(ENVELOPE_MAGIC.len() + XNONCE_LEN + self.ciphertext.len());
        out.extend_from_slice(ENVELOPE_MAGIC);
        out.extend_from_slice(&self.nonce);
        out.extend_from_slice(&self.ciphertext);
        out
    }

    /// Parse the on-disk byte layout back into an envelope.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < ENVELOPE_MAGIC.len() + XNONCE_LEN {
            return Err("envelope too short".into());
        }
        let (magic, rest) = bytes.split_at(ENVELOPE_MAGIC.len());
        if magic != ENVELOPE_MAGIC {
            return Err(format!(
                "bad envelope magic: {:02x?} (expected {ENVELOPE_MAGIC:02x?})",
                magic
            ));
        }
        let (nonce_bytes, ciphertext) = rest.split_at(XNONCE_LEN);
        let mut nonce = [0_u8; XNONCE_LEN];
        nonce.copy_from_slice(nonce_bytes);
        Ok(Self {
            nonce,
            ciphertext: ciphertext.to_vec(),
        })
    }
}

/// Seal `plaintext` under `dek`. Generates a fresh nonce.
pub(crate) fn seal_blob(dek: &DataEncryptionKey, plaintext: &[u8]) -> Result<BlobEnvelope, String> {
    let mut nonce = [0_u8; XNONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce);
    let cipher = dek.cipher();
    let xnonce = XNonce::from_slice(&nonce);
    let ciphertext = cipher
        .encrypt(
            xnonce,
            Payload {
                msg: plaintext,
                aad: ENVELOPE_MAGIC,
            },
        )
        .map_err(|e| format!("encrypt: {e}"))?;
    Ok(BlobEnvelope { nonce, ciphertext })
}

/// Open a sealed envelope under `dek`. Authenticates with the same AAD.
pub(crate) fn open_blob(dek: &DataEncryptionKey, env: &BlobEnvelope) -> Result<Vec<u8>, String> {
    let cipher = dek.cipher();
    let xnonce = XNonce::from_slice(&env.nonce);
    let pt = cipher
        .decrypt(
            xnonce,
            Payload {
                msg: &env.ciphertext,
                aad: ENVELOPE_MAGIC,
            },
        )
        .map_err(|e| format!("decrypt: {e}"))?;
    Ok(pt)
}

// ---------------------------------------------------------------------------
// On-disk format & path resolution
// ---------------------------------------------------------------------------

/// Wire-format the master-key file is persisted as.
#[derive(Debug, Serialize, Deserialize)]
struct MasterKeyFile {
    master_key_hex: String,
}

/// Resolve the on-disk path for the master-key file.
///
/// Honours the `PHANTOM_BUNDLE_STORE_MASTER_KEY_FILE` env var as an absolute
/// override, else falls back to
/// `dirs::config_dir().join("phantom").join("bundle-store").join("master-key.json")`.
fn master_key_path() -> Result<PathBuf, StoreError> {
    if let Some(override_path) = std::env::var_os(MASTER_KEY_FILE_ENV) {
        return Ok(PathBuf::from(override_path));
    }
    let base = dirs::config_dir().or_else(dirs::home_dir).ok_or_else(|| {
        StoreError::MasterKey(
            "could not determine config or home directory for master-key storage".into(),
        )
    })?;
    Ok(base
        .join("phantom")
        .join("bundle-store")
        .join("master-key.json"))
}

fn load_from_file(path: &Path) -> Result<MasterKey, StoreError> {
    let bytes = std::fs::read(path).map_err(|e| {
        StoreError::MasterKey(format!(
            "failed to read master-key file at {}: {e}",
            path.display()
        ))
    })?;
    let file: MasterKeyFile = serde_json::from_slice(&bytes).map_err(|e| {
        StoreError::MasterKey(format!(
            "master-key file at {} is malformed JSON: {e}",
            path.display()
        ))
    })?;
    let key_bytes = decode_hex_32(&file.master_key_hex).map_err(|e| {
        StoreError::MasterKey(format!(
            "master-key file at {} contains malformed key hex: {e}",
            path.display()
        ))
    })?;
    Ok(MasterKey::from_bytes(key_bytes))
}

fn generate_and_persist(path: &Path) -> Result<MasterKey, StoreError> {
    let mut bytes = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);

    let file = MasterKeyFile {
        master_key_hex: encode_hex_32(&bytes),
    };
    let json = serde_json::to_vec_pretty(&file)
        .map_err(|e| StoreError::MasterKey(format!("failed to serialise master-key: {e}")))?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            StoreError::MasterKey(format!(
                "failed to create master-key directory at {}: {e}",
                parent.display()
            ))
        })?;
    }

    // Race-safe write: if another thread/process generated the master key
    // between our `path.exists()` check and now, fall back to reading their
    // canonical file rather than overwriting it.
    match write_atomic_exclusive(path, &json) {
        Ok(()) => Ok(MasterKey::from_bytes(bytes)),
        Err(WriteError::AlreadyExists) => load_from_file(path),
        Err(WriteError::Io(e)) => Err(e),
    }
}

enum WriteError {
    /// The destination file was created by someone else mid-write.
    AlreadyExists,
    Io(StoreError),
}

/// Write `bytes` to `path` atomically with "lose gracefully on conflict"
/// semantics.
///
/// Strategy: write to `{path}.{pid}.{tid}.tmp` with mode 0600 set in the
/// same `open(2)` call, fsync, then `hard_link` it into place at `path`.
/// `hard_link` fails with `AlreadyExists` when `path` already exists — that
/// signal is the contract this function owes its caller, who then knows to
/// defer to the existing file rather than clobber it. After a successful
/// link the tmp is unlinked; the on-disk `path` is the canonical durable
/// copy.
///
/// The 0600 mode is set inside the same `open(2)` call so the file never
/// exists on disk with the process umask's default permissions — closing
/// the umask race documented for [`phantom-net::identity`] in #555.
fn write_atomic_exclusive(path: &Path, bytes: &[u8]) -> Result<(), WriteError> {
    use std::io::Write;

    // Per-(pid, thread) tmp suffix so racing writers do not trample each
    // other's tmp file.
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

    let write_result = (|| -> Result<(), StoreError> {
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
        let mut f = open_opts.open(&tmp_path).map_err(|e| {
            StoreError::MasterKey(format!(
                "failed to open tmp master-key file at {}: {e}",
                tmp_path.display()
            ))
        })?;

        f.write_all(bytes).map_err(|e| {
            StoreError::MasterKey(format!(
                "failed to write master-key to {}: {e}",
                tmp_path.display()
            ))
        })?;
        f.sync_all().map_err(|e| {
            StoreError::MasterKey(format!(
                "failed to fsync master-key at {}: {e}",
                tmp_path.display()
            ))
        })?;
        Ok(())
    })();

    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(WriteError::Io(e));
    }

    // Try to hard-link the tmp into place. `hard_link` is a strict
    // "must not exist" operation on every platform we target — this is the
    // race-safe move.
    let link_result = std::fs::hard_link(&tmp_path, path);
    let _ = std::fs::remove_file(&tmp_path);

    match link_result {
        Ok(()) => {
            // Best-effort fsync of the parent directory so the link is
            // durable. Non-fatal — many filesystems do not require it.
            #[cfg(unix)]
            if let Some(parent) = path.parent()
                && let Ok(dir) = std::fs::File::open(parent)
            {
                let _ = dir.sync_all();
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(WriteError::AlreadyExists),
        Err(e) => Err(WriteError::Io(StoreError::MasterKey(format!(
            "failed to link {} -> {}: {e}",
            tmp_path.display(),
            path.display()
        )))),
    }
}

// ---------------------------------------------------------------------------
// Tiny hex helpers — avoids pulling in the `hex` crate just to round-trip
// the 32-byte master key through the JSON file.
// ---------------------------------------------------------------------------

fn encode_hex_32(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn decode_hex_32(s: &str) -> Result<[u8; 32], String> {
    if s.len() != 64 {
        return Err(format!("expected 64 hex chars, got {}", s.len()));
    }
    let mut out = [0_u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = from_hex_digit(chunk[0])?;
        let lo = from_hex_digit(chunk[1])?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

fn from_hex_digit(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(format!("invalid hex digit: {}", b as char)),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use uuid::Uuid;

    // -----------------------------------------------------------------------
    // Test infrastructure
    //
    // Tests that mutate `PHANTOM_BUNDLE_STORE_MASTER_KEY_FILE` serialize
    // through ENV_SERIAL because env vars are process-global.
    // -----------------------------------------------------------------------

    static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);
    static ENV_SERIAL: Mutex<()> = Mutex::new(());

    fn tmp_master_key_file() -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("phantom-master-key-test-{pid}-{n}.json"))
    }

    /// Clear the per-process cache so a "fresh" call genuinely hits disk.
    fn reset_cache() {
        MASTER_KEY_CACHE
            .lock()
            .expect("master key cache mutex poisoned")
            .clear();
    }

    /// Cleanup guard — removes env var and tmp file on drop.
    struct CleanupGuard {
        path: PathBuf,
    }
    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            // SAFETY: env mutation is serialised via ENV_SERIAL.
            unsafe { std::env::remove_var(MASTER_KEY_FILE_ENV) };
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn dek_derivation_is_deterministic_per_bundle_id() {
        let mk = MasterKey::from_bytes([7_u8; 32]);
        let id = Uuid::from_u128(0xDEAD_BEEF_CAFE_F00D_0123_4567_89AB_CDEF);
        let dek_a = mk.derive_bundle_dek(id).unwrap();
        let dek_b = mk.derive_bundle_dek(id).unwrap();
        assert_eq!(dek_a.0.as_ref(), dek_b.0.as_ref());

        let other = Uuid::from_u128(0x1111_1111_1111_1111_1111_1111_1111_1111);
        let dek_c = mk.derive_bundle_dek(other).unwrap();
        assert_ne!(
            dek_a.0.as_ref(),
            dek_c.0.as_ref(),
            "different bundles get different DEKs"
        );
    }

    #[test]
    fn seal_open_round_trip() {
        let mk = MasterKey::from_bytes([0xAB_u8; 32]);
        let id = Uuid::from_u128(1);
        let dek = mk.derive_bundle_dek(id).unwrap();
        let plaintext = b"the quick brown fox jumps over the lazy dog";
        let env = seal_blob(&dek, plaintext).unwrap();
        assert_ne!(env.ciphertext.as_slice(), plaintext);
        let opened = open_blob(&dek, &env).unwrap();
        assert_eq!(opened, plaintext);
    }

    #[test]
    fn envelope_bytes_round_trip() {
        let mk = MasterKey::from_bytes([0x33_u8; 32]);
        let dek = mk.derive_bundle_dek(Uuid::nil()).unwrap();
        let env = seal_blob(&dek, b"hello").unwrap();
        let bytes = env.to_bytes();
        let parsed = BlobEnvelope::from_bytes(&bytes).expect("parse");
        assert_eq!(parsed.nonce, env.nonce);
        assert_eq!(parsed.ciphertext, env.ciphertext);
    }

    #[test]
    fn tampered_ciphertext_fails_to_open() {
        let mk = MasterKey::from_bytes([0x55_u8; 32]);
        let dek = mk.derive_bundle_dek(Uuid::nil()).unwrap();
        let mut env = seal_blob(&dek, b"secret payload").unwrap();
        // Flip a bit somewhere in the ciphertext.
        env.ciphertext[0] ^= 0x01;
        let err = open_blob(&dek, &env).expect_err("must fail auth");
        assert!(err.contains("decrypt"));
    }

    #[test]
    fn hex_round_trip_32_bytes() {
        let mut bytes = [0_u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(17).wrapping_add(3);
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
    // File-backend tests (use PHANTOM_BUNDLE_STORE_MASTER_KEY_FILE override)
    // -----------------------------------------------------------------------

    #[test]
    fn load_or_generate_creates_fresh_file_on_first_call() {
        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_master_key_file();
        // SAFETY: env mutation is serialised via ENV_SERIAL.
        unsafe { std::env::set_var(MASTER_KEY_FILE_ENV, &path) };
        reset_cache();
        let _cleanup = CleanupGuard { path: path.clone() };

        assert!(!path.exists(), "precondition: file must not exist");

        let key = MasterKey::load_or_generate().expect("first call must succeed");
        assert!(path.exists(), "first call must create the file");

        // File contents must round-trip via load_from_file and produce the
        // same key bytes.
        reset_cache();
        let reloaded = load_from_file(&path).expect("file must be readable after generation");
        assert_eq!(key.bytes(), reloaded.bytes());
    }

    #[test]
    fn load_or_generate_reads_existing_file_on_second_call() {
        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_master_key_file();
        unsafe { std::env::set_var(MASTER_KEY_FILE_ENV, &path) };
        reset_cache();
        let _cleanup = CleanupGuard { path: path.clone() };

        let k1 = MasterKey::load_or_generate().unwrap();
        // Clear the cache so the second call genuinely re-reads from disk.
        reset_cache();
        let k2 = MasterKey::load_or_generate().unwrap();

        assert_eq!(
            k1.bytes(),
            k2.bytes(),
            "second call must return the same key as the first (read from file)"
        );
    }

    #[test]
    fn corrupted_file_returns_error_no_silent_overwrite() {
        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_master_key_file();
        unsafe { std::env::set_var(MASTER_KEY_FILE_ENV, &path) };
        reset_cache();
        let _cleanup = CleanupGuard { path: path.clone() };

        std::fs::write(&path, b"this is not valid JSON {{{").expect("seed corrupted file");
        let before = std::fs::read(&path).unwrap();

        let result = MasterKey::load_or_generate();
        assert!(
            result.is_err(),
            "corrupted file must surface as Err, never silently regenerated"
        );

        let after = std::fs::read(&path).unwrap();
        assert_eq!(
            before, after,
            "corrupted file must not be overwritten — that would erase a real master key if the parser had a transient issue"
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_mode_is_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_master_key_file();
        unsafe { std::env::set_var(MASTER_KEY_FILE_ENV, &path) };
        reset_cache();
        let _cleanup = CleanupGuard { path: path.clone() };

        let _ = MasterKey::load_or_generate().unwrap();

        let meta = std::fs::metadata(&path).expect("master-key file must exist");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "master-key file must be mode 0600, got {mode:o}"
        );
    }

    /// Regression guard: the tmp file must be created with mode 0600 in the
    /// same syscall as `open(2)`, so there is no window where it exists on
    /// disk with the default umask permissions. Mirrors PR #555 hardening
    /// in `phantom-net::identity`.
    #[cfg(unix)]
    #[test]
    fn tmp_file_has_0600_at_creation_no_chmod_window() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let path = std::env::temp_dir().join(format!(
            "phantom-bundle-store-mode-test-{}-{}.tmp",
            std::process::id(),
            TEST_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_file(&path);

        // Mirror the production OpenOptions from write_atomic_exclusive.
        let mut open_opts = std::fs::OpenOptions::new();
        open_opts.write(true).create_new(true).mode(0o600);
        let _f = open_opts.open(&path).expect("create tmp file");

        let meta = std::fs::metadata(&path).expect("tmp file must exist");
        let mode = meta.permissions().mode() & 0o777;
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            mode, 0o600,
            "tmp file must be 0600 immediately after open(2), got {mode:o} \
             — this means the umask race window is back"
        );
    }

    #[test]
    fn cache_skips_disk_on_repeat_call() {
        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_master_key_file();
        unsafe { std::env::set_var(MASTER_KEY_FILE_ENV, &path) };
        reset_cache();
        let _cleanup = CleanupGuard { path: path.clone() };

        let k1 = MasterKey::load_or_generate().unwrap();

        // Delete the file behind the cache. If the cache works, the next
        // call returns the cached key rather than erroring on the missing
        // file.
        std::fs::remove_file(&path).expect("file must exist after first call");
        let k2 = MasterKey::load_or_generate()
            .expect("cache hit must not depend on the file being present");

        assert_eq!(k1.bytes(), k2.bytes());
        assert!(
            !path.exists(),
            "cache hit must not have re-created the file from disk"
        );
    }
}
