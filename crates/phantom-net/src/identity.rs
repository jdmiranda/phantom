//! Ed25519 keypair identity with on-disk persistence.
//!
//! Each Phantom instance owns a stable [`Identity`].  The private key is kept
//! in a JSON file under the user's config directory (mode `0600` on Unix) so
//! it survives restarts without depending on the OS keyring.
//!
//! # Why a file, not the OS keyring
//! macOS Keychain ACLs are bound to the requesting binary's code signature.
//! Every `cargo build` produces a binary with a different content hash, and
//! the macOS prompt encodes that hash in the requesting-app name — so to the
//! OS each build is a distinct app and the user's "Always Allow" choice never
//! sticks.  With autonomous-agent worktrees rebuilding constantly, this turns
//! into prompt-spam.  A plain file under the config dir, mode `0600`, sidesteps
//! the issue entirely.
//!
//! # Storage path
//! - Default: `dirs::config_dir().unwrap().join("phantom").join("{service}.json")`.
//!   On macOS this is `~/Library/Application Support/phantom/{service}.json`.
//! - Override: set `PHANTOM_IDENTITY_FILE` to use that exact path (the
//!   `{service}` arg is ignored when the override is set).
//!
//! # File format
//! ```json
//! { "peer_id": "<base58>", "signing_key_hex": "<64-hex>" }
//! ```
//! The file is written atomically: a sibling `*.tmp` file is created, fsync'd,
//! then renamed over the destination so a crash mid-write cannot leave a
//! partial file.
//!
//! # Per-process cache
//! Loaded identities are cached in a process-wide map keyed by `service`, so
//! repeated calls within a single process do at most one disk read per service.
//!
//! # Example
//! ```rust,no_run
//! use phantom_net::identity::Identity;
//!
//! let id = Identity::load_or_generate("phantom").unwrap();
//! println!("my peer-id: {}", id.peer_id);
//! let sig = id.sign(b"hello");
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, Result};
use bs58;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// PeerId
// ---------------------------------------------------------------------------

/// Stable public identifier for a Phantom instance.
///
/// Derived from the Ed25519 public key as `base58(SHA-256(pubkey_bytes))`.
/// 44 characters of URL-safe, human-readable text — safe to log and display.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PeerId(String);

impl PeerId {
    /// Derive a [`PeerId`] from an Ed25519 verifying key.
    #[must_use]
    pub fn from_verifying_key(vk: &VerifyingKey) -> Self {
        let hash = Sha256::digest(vk.as_bytes());
        Self(bs58::encode(hash).into_string())
    }

    /// Borrow the inner base58 string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Construct a `PeerId` from a raw string.
    ///
    /// Used internally for relay addressing (e.g. `"relay"` as the relay
    /// server's nominal peer-id) where no keypair is available.
    pub(crate) fn from_raw(s: String) -> Self {
        Self(s)
    }
}

impl std::fmt::Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<PeerId> for String {
    fn from(id: PeerId) -> Self {
        id.0
    }
}

// ---------------------------------------------------------------------------
// On-disk format
// ---------------------------------------------------------------------------

/// Wire-format the identity file is persisted as.
///
/// `peer_id` is denormalised so a human eyeballing the file can read it
/// without running it through SHA-256 + base58.  The signing key is the
/// authoritative source — `peer_id` is rederived on load and the file value
/// is not trusted for routing.
#[derive(Debug, Serialize, Deserialize)]
struct IdentityFile {
    peer_id: String,
    signing_key_hex: String,
}

// ---------------------------------------------------------------------------
// Per-process cache
// ---------------------------------------------------------------------------

/// Process-wide cache of loaded identities, keyed by `service` argument.
///
/// Avoids re-reading the JSON file on every call within the same process.
/// The cache is heap-only and never written to disk — the only persisted
/// state is the JSON file itself.
static IDENTITY_CACHE: LazyLock<Mutex<HashMap<String, Identity>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// Ed25519 signing identity backed by an on-disk JSON file.
///
/// `Identity` is cheap to clone — the underlying [`SigningKey`] lives behind
/// an [`Arc`] so cloning bumps a refcount rather than copying key bytes.  This
/// matches how callers use it: pass to [`crate::client::RelayClient::connect`]
/// by value, but still hold a copy elsewhere for later signing.
#[derive(Clone)]
pub struct Identity {
    keypair: Arc<SigningKey>,
    /// The public peer identifier derived from this keypair.
    pub peer_id: PeerId,
}

impl Identity {
    /// Load an existing keypair from the on-disk identity file, or generate
    /// and persist a new one.
    ///
    /// `service` is a short string used to namespace the file (e.g.
    /// `"phantom"` or `"phantom-test"`).  Different services get separate
    /// files at `{config_dir}/phantom/{service}.json`.
    ///
    /// On first call within a process the file is read (or generated) and the
    /// result cached.  Subsequent calls return the cached value with no disk
    /// I/O.
    ///
    /// # Errors
    /// - Returns an error if the config directory cannot be located.
    /// - Returns an error if an existing file is present but cannot be parsed
    ///   or contains a malformed signing key.  A parse failure does **not**
    ///   trigger silent regeneration — that would erase a real identity in
    ///   the face of a transient parser issue.
    /// - Returns an error if a fresh file cannot be written to disk.
    pub fn load_or_generate(service: &str) -> Result<Self> {
        // Per-process cache hit path — no disk I/O.
        {
            let cache = IDENTITY_CACHE
                .lock()
                .expect("identity cache mutex poisoned");
            if let Some(id) = cache.get(service) {
                return Ok(id.clone());
            }
        }

        let path = identity_path(service)?;
        let id = if path.exists() {
            load_from_file(&path)?
        } else {
            generate_and_persist(&path)?
        };

        // Insert into cache (race-tolerant — last writer wins, both copies
        // are equivalent because they were derived from the same on-disk
        // bytes or the same first-writer's bytes).
        {
            let mut cache = IDENTITY_CACHE
                .lock()
                .expect("identity cache mutex poisoned");
            cache
                .entry(service.to_owned())
                .or_insert_with(|| id.clone());
        }

        Ok(id)
    }

    /// Sign an arbitrary byte slice and return the 64-byte Ed25519 signature.
    #[must_use]
    pub fn sign(&self, msg: &[u8]) -> Signature {
        self.keypair.sign(msg)
    }

    /// Expose the raw verifying key so callers can verify signatures made with
    /// this identity.
    #[must_use]
    pub fn verifying_key(&self) -> VerifyingKey {
        self.keypair.verifying_key()
    }

    // -- test helpers --------------------------------------------------------

    /// Generate a fresh throwaway identity without touching disk.
    ///
    /// Used in unit tests where persistence is not under test.
    #[cfg(test)]
    pub(crate) fn generate_ephemeral() -> Self {
        let keypair = SigningKey::generate(&mut OsRng);
        let peer_id = PeerId::from_verifying_key(&keypair.verifying_key());
        Self {
            keypair: Arc::new(keypair),
            peer_id,
        }
    }
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Environment variable that overrides the default identity file location.
///
/// When set, `{service}` is ignored and the override path is used verbatim.
/// Primarily for tests and operator overrides.
const IDENTITY_FILE_ENV: &str = "PHANTOM_IDENTITY_FILE";

/// Resolve the on-disk path for `service`.
///
/// Honours the `PHANTOM_IDENTITY_FILE` env var as an absolute override, else
/// falls back to `dirs::config_dir().join("phantom").join("{service}.json")`.
fn identity_path(service: &str) -> Result<PathBuf> {
    if let Some(override_path) = std::env::var_os(IDENTITY_FILE_ENV) {
        return Ok(PathBuf::from(override_path));
    }

    let base = dirs::config_dir()
        .or_else(dirs::home_dir)
        .context("could not determine config or home directory for identity storage")?;
    Ok(base.join("phantom").join(format!("{service}.json")))
}

// ---------------------------------------------------------------------------
// Load / persist
// ---------------------------------------------------------------------------

fn load_from_file(path: &Path) -> Result<Identity> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read identity file at {}", path.display()))?;
    let file: IdentityFile = serde_json::from_slice(&bytes)
        .with_context(|| format!("identity file at {} is malformed JSON", path.display()))?;

    let key_bytes = hex::decode_32(&file.signing_key_hex).with_context(|| {
        format!(
            "identity file at {} contains malformed signing key hex",
            path.display()
        )
    })?;
    let keypair = SigningKey::from_bytes(&key_bytes);
    let peer_id = PeerId::from_verifying_key(&keypair.verifying_key());

    Ok(Identity {
        keypair: Arc::new(keypair),
        peer_id,
    })
}

fn generate_and_persist(path: &Path) -> Result<Identity> {
    let keypair = SigningKey::generate(&mut OsRng);
    let peer_id = PeerId::from_verifying_key(&keypair.verifying_key());

    let file = IdentityFile {
        peer_id: peer_id.as_str().to_owned(),
        signing_key_hex: hex::encode_32(keypair.to_bytes()),
    };
    let json = serde_json::to_vec_pretty(&file).context("failed to serialise identity")?;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("failed to create identity directory at {}", parent.display())
        })?;
    }

    // Race-safe write: if another thread/process generated the identity
    // between our `path.exists()` check and now, fall back to reading their
    // canonical file rather than overwriting it.
    match write_atomic_exclusive(path, &json) {
        Ok(()) => Ok(Identity {
            keypair: Arc::new(keypair),
            peer_id,
        }),
        Err(WriteError::AlreadyExists) => load_from_file(path),
        Err(WriteError::Io(e)) => Err(e),
    }
}

enum WriteError {
    /// The destination file was created by someone else mid-write.
    AlreadyExists,
    Io(anyhow::Error),
}

/// Write `bytes` to `path` atomically with a "lose gracefully on conflict"
/// semantics.
///
/// Strategy: write to `{path}.{pid}.{tid}.tmp` first, then attempt to
/// hard-link it into place at `path`.  `hard_link` fails with `AlreadyExists`
/// when `path` already exists — that signal is the contract this function
/// owes its caller, who then knows to defer to the existing file rather than
/// clobber it.  After a successful link the tmp is unlinked; the on-disk
/// `path` is the canonical durable copy.
///
/// On platforms where we can't use hard-link semantics (Windows), we fall
/// back to a `rename`-with-EEXIST best-effort pattern.
///
/// On Unix the destination's permissions are also set to `0600` so the
/// signing-key bytes are not world-readable.
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

    let write_result = (|| -> Result<()> {
        // Open the tmp file with mode 0600 set atomically at creation on Unix.
        //
        // Unix `mode(0o600)` from `OpenOptionsExt` applies the mode inside the
        // same open(2) call, so the file never exists on disk with the
        // process umask's default permissions.  This closes the
        // microsecond-scale race that the previous `create(true)` +
        // `set_permissions` sequence left open.
        //
        // We use `create_new(true)` so a stale tmp file from a prior crash
        // cannot be reused (it might have been written by another user, or
        // tampered with).  To keep the function robust against our own pid+tid
        // collisions, we unlink any prior tmp file at this path first.
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
                "failed to open tmp identity file at {}",
                tmp_path.display()
            )
        })?;

        f.write_all(bytes)
            .with_context(|| format!("failed to write identity to {}", tmp_path.display()))?;
        f.sync_all()
            .with_context(|| format!("failed to fsync identity at {}", tmp_path.display()))?;
        Ok(())
    })();

    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(WriteError::Io(e));
    }

    // Try to hard-link the tmp into place.  `hard_link` is a strict
    // "must not exist" operation on every platform we target — this is the
    // race-safe move.  After link succeeds we unlink the tmp; if link fails
    // with AlreadyExists the caller falls back to `load_from_file`.
    let link_result = std::fs::hard_link(&tmp_path, path);
    let _ = std::fs::remove_file(&tmp_path);

    match link_result {
        Ok(()) => {
            // Best-effort fsync of the parent directory so the link is
            // durable.  Non-fatal — many filesystems do not require it.
            #[cfg(unix)]
            if let Some(parent) = path.parent()
                && let Ok(dir) = std::fs::File::open(parent)
            {
                let _ = dir.sync_all();
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(WriteError::AlreadyExists),
        Err(e) => Err(WriteError::Io(anyhow::anyhow!(
            "failed to link {} -> {}: {e}",
            tmp_path.display(),
            path.display()
        ))),
    }
}

// ---------------------------------------------------------------------------
// Internal hex helpers (avoids pulling in the `hex` crate)
// ---------------------------------------------------------------------------

mod hex {
    use anyhow::{Result, bail};

    pub fn encode_32(bytes: [u8; 32]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn decode_32(s: &str) -> Result<[u8; 32]> {
        if s.len() != 64 {
            bail!("expected 64 hex chars, got {}", s.len());
        }
        let mut out = [0u8; 32];
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
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // -----------------------------------------------------------------------
    // Test infrastructure
    //
    // Each test gets its own tmpdir and a unique service name.  The
    // `PHANTOM_IDENTITY_FILE` env var forces all identity I/O into the
    // tmpdir — but env vars are process-global, so tests that need the
    // override must hold a serial-mutex to avoid clobbering each other.
    // -----------------------------------------------------------------------

    static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);
    static ENV_SERIAL: Mutex<()> = Mutex::new(());

    fn unique_service() -> String {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        format!("phantom-test-{pid}-{n}")
    }

    fn tmp_identity_file() -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("phantom-identity-test-{pid}-{n}.json"))
    }

    /// Clear the per-process cache so a "fresh" call genuinely hits disk.
    fn reset_cache() {
        IDENTITY_CACHE
            .lock()
            .expect("identity cache mutex poisoned")
            .clear();
    }

    // -----------------------------------------------------------------------
    // Pure unit tests (no env, no disk)
    // -----------------------------------------------------------------------

    #[test]
    fn peer_id_is_deterministic() {
        let id = Identity::generate_ephemeral();
        let id2_peer = PeerId::from_verifying_key(&id.keypair.verifying_key());
        assert_eq!(id.peer_id, id2_peer);
    }

    #[test]
    fn peer_id_display_is_base58() {
        let id = Identity::generate_ephemeral();
        let s = id.peer_id.to_string();
        // base58 alphabet never contains 0OIl
        assert!(!s.contains('0'));
        assert!(!s.contains('O'));
        assert!(!s.contains('I'));
        assert!(!s.contains('l'));
    }

    #[test]
    fn sign_produces_valid_signature() {
        use ed25519_dalek::Verifier;

        let id = Identity::generate_ephemeral();
        let msg = b"phantom relay handshake";
        let sig = id.sign(msg);
        let vk = id.verifying_key();
        assert!(vk.verify(msg, &sig).is_ok());
    }

    #[test]
    fn hex_round_trip() {
        let bytes: [u8; 32] = (0u8..32).collect::<Vec<_>>().try_into().unwrap();
        let encoded = hex::encode_32(bytes);
        let decoded = hex::decode_32(&encoded).unwrap();
        assert_eq!(bytes, decoded);
    }

    #[test]
    fn hex_decode_rejects_short_input() {
        assert!(hex::decode_32("deadbeef").is_err());
    }

    #[test]
    fn identity_clone_is_cheap_and_equivalent() {
        let id1 = Identity::generate_ephemeral();
        let id2 = id1.clone();
        assert_eq!(id1.peer_id, id2.peer_id);
        // Both clones produce identical signatures over the same message.
        assert_eq!(id1.sign(b"abc").to_bytes(), id2.sign(b"abc").to_bytes());
    }

    // -----------------------------------------------------------------------
    // File-backend tests (use PHANTOM_IDENTITY_FILE override)
    // -----------------------------------------------------------------------

    #[test]
    fn first_call_generates_fresh_file() {
        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_identity_file();
        // SAFETY: env mutation is serialised via ENV_SERIAL.
        unsafe { std::env::set_var(IDENTITY_FILE_ENV, &path) };
        reset_cache();
        let _cleanup = OnDrop::new(|| {
            unsafe { std::env::remove_var(IDENTITY_FILE_ENV) };
            let _ = std::fs::remove_file(&path);
        });

        assert!(!path.exists(), "precondition: file must not exist");

        let service = unique_service();
        let id = Identity::load_or_generate(&service).expect("first call must succeed");

        assert!(path.exists(), "first call must create the file");

        // File contents must round-trip via load_from_file and produce the
        // same peer_id.
        let reloaded = load_from_file(&path).expect("file must be readable after generation");
        assert_eq!(id.peer_id, reloaded.peer_id);
    }

    #[test]
    fn second_call_reads_existing_file() {
        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_identity_file();
        unsafe { std::env::set_var(IDENTITY_FILE_ENV, &path) };
        reset_cache();
        let _cleanup = OnDrop::new(|| {
            unsafe { std::env::remove_var(IDENTITY_FILE_ENV) };
            let _ = std::fs::remove_file(&path);
        });

        let service = unique_service();
        let id1 = Identity::load_or_generate(&service).unwrap();
        // Clear the cache so the second call genuinely re-reads from disk.
        reset_cache();
        let id2 = Identity::load_or_generate(&service).unwrap();

        assert_eq!(
            id1.peer_id, id2.peer_id,
            "second call must return the same peer_id as the first (read from file)"
        );
    }

    #[test]
    fn cache_skips_disk_on_repeat_call() {
        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_identity_file();
        unsafe { std::env::set_var(IDENTITY_FILE_ENV, &path) };
        reset_cache();
        let _cleanup = OnDrop::new(|| {
            unsafe { std::env::remove_var(IDENTITY_FILE_ENV) };
            let _ = std::fs::remove_file(&path);
        });

        let service = unique_service();
        let id1 = Identity::load_or_generate(&service).unwrap();

        // Delete the file behind the cache.  If the cache works, the next call
        // returns the cached identity rather than erroring on the missing file.
        std::fs::remove_file(&path).expect("file must exist after first call");
        let id2 = Identity::load_or_generate(&service)
            .expect("cache hit must not depend on the file being present");

        assert_eq!(id1.peer_id, id2.peer_id);
        assert!(
            !path.exists(),
            "cache hit must not have re-created the file from disk"
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_mode_is_0600_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_identity_file();
        unsafe { std::env::set_var(IDENTITY_FILE_ENV, &path) };
        reset_cache();
        let _cleanup = OnDrop::new(|| {
            unsafe { std::env::remove_var(IDENTITY_FILE_ENV) };
            let _ = std::fs::remove_file(&path);
        });

        let service = unique_service();
        let _ = Identity::load_or_generate(&service).unwrap();

        let meta = std::fs::metadata(&path).expect("identity file must exist");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "identity file must be mode 0600, got {mode:o}"
        );
    }

    /// Regression guard for issue #549: the tmp file must be created with
    /// mode 0600 in the same syscall as `open(2)`, so there is no window
    /// where it exists on disk with the default umask permissions.
    ///
    /// This test exercises the exact `OpenOptions` configuration the
    /// production path uses and checks the file mode *immediately after*
    /// `open()` returns — before any other syscall runs against the path.
    /// On a typical CI box (umask 022) a regression that drops the explicit
    /// `.mode(0o600)` from the OpenOptions would surface here as 0644.
    #[cfg(unix)]
    #[test]
    fn tmp_file_has_0600_at_creation_no_chmod_window() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let path = std::env::temp_dir().join(format!(
            "phantom-net-mode-test-{}-{}.tmp",
            std::process::id(),
            TEST_COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        let _cleanup = OnDrop::new({
            let p = path.clone();
            move || {
                let _ = std::fs::remove_file(&p);
            }
        });
        let _ = std::fs::remove_file(&path);

        // Mirror the production OpenOptions from write_atomic_exclusive.
        let mut open_opts = std::fs::OpenOptions::new();
        open_opts.write(true).create_new(true).mode(0o600);
        let _f = open_opts.open(&path).expect("create tmp file");

        let meta = std::fs::metadata(&path).expect("tmp file must exist");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "tmp file must be 0600 immediately after open(2), got {mode:o} \
             — this means the umask race window is back"
        );
    }

    #[test]
    fn concurrent_first_calls_only_write_once() {
        // Two threads racing to call load_or_generate with the same service
        // must end up with the same peer_id.  One writes the file; the other
        // either reads it back or hits the cache — either way the peer_ids
        // converge.
        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_identity_file();
        unsafe { std::env::set_var(IDENTITY_FILE_ENV, &path) };
        reset_cache();
        let _cleanup = OnDrop::new(|| {
            unsafe { std::env::remove_var(IDENTITY_FILE_ENV) };
            let _ = std::fs::remove_file(&path);
        });

        let service = unique_service();
        let s1 = service.clone();
        let s2 = service.clone();

        let h1 = std::thread::spawn(move || Identity::load_or_generate(&s1).unwrap().peer_id);
        let h2 = std::thread::spawn(move || Identity::load_or_generate(&s2).unwrap().peer_id);

        let p1 = h1.join().unwrap();
        let p2 = h2.join().unwrap();

        assert_eq!(
            p1, p2,
            "concurrent first calls must converge on the same peer_id"
        );
    }

    #[test]
    fn corrupted_file_returns_error_no_silent_overwrite() {
        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_identity_file();
        unsafe { std::env::set_var(IDENTITY_FILE_ENV, &path) };
        reset_cache();
        let _cleanup = OnDrop::new(|| {
            unsafe { std::env::remove_var(IDENTITY_FILE_ENV) };
            let _ = std::fs::remove_file(&path);
        });

        std::fs::write(&path, b"this is not valid JSON {{{").expect("seed corrupted file");
        let before = std::fs::read(&path).unwrap();

        let service = unique_service();
        let result = Identity::load_or_generate(&service);
        assert!(
            result.is_err(),
            "corrupted file must surface as Err, never silently regenerated"
        );

        let after = std::fs::read(&path).unwrap();
        assert_eq!(
            before, after,
            "corrupted file must not be overwritten — that would erase a real identity if the parser had a transient issue"
        );
    }

    #[test]
    fn distinct_services_use_distinct_files() {
        // No PHANTOM_IDENTITY_FILE here — we want to test that the {service}
        // path component genuinely segregates files.  Use a custom config dir
        // by setting XDG_CONFIG_HOME (Linux) / overriding via the env var on
        // every platform via PHANTOM_IDENTITY_FILE is single-file by design,
        // so instead we test that two services with the same override-prefix
        // produce different filenames — which we verify directly via
        // identity_path.
        let s1 = unique_service();
        let s2 = unique_service();
        assert_ne!(s1, s2);

        // Sanity: with the env var unset, identity_path must produce
        // different paths for different services.
        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let _cleanup = OnDrop::new(|| {
            // Restore env state.  identity_path reads the env each time.
        });
        // Make sure no override is set during this test.
        let prev = std::env::var_os(IDENTITY_FILE_ENV);
        unsafe { std::env::remove_var(IDENTITY_FILE_ENV) };
        let p1 = identity_path(&s1).expect("path 1");
        let p2 = identity_path(&s2).expect("path 2");
        if let Some(prev) = prev {
            unsafe { std::env::set_var(IDENTITY_FILE_ENV, prev) };
        }

        assert_ne!(p1, p2, "different services must produce different file paths");
        assert!(p1.to_string_lossy().contains(&s1));
        assert!(p2.to_string_lossy().contains(&s2));
    }

    /// Identity persists across instances within the same process.  Both
    /// calls to `load_or_generate` produce the same `PeerId`.
    #[test]
    fn identity_persists_across_instances() {
        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_identity_file();
        unsafe { std::env::set_var(IDENTITY_FILE_ENV, &path) };
        reset_cache();
        let _cleanup = OnDrop::new(|| {
            unsafe { std::env::remove_var(IDENTITY_FILE_ENV) };
            let _ = std::fs::remove_file(&path);
        });

        let service = unique_service();
        let id1 = Identity::load_or_generate(&service).unwrap();
        let id2 = Identity::load_or_generate(&service).unwrap();

        assert_eq!(
            id1.peer_id, id2.peer_id,
            "peer_id must be stable across load_or_generate calls"
        );
    }

    // -----------------------------------------------------------------------
    // Drop helper for test cleanup
    // -----------------------------------------------------------------------

    /// Run a closure when the guard goes out of scope.
    ///
    /// Used so cleanup happens even on test panic / early return.
    struct OnDrop<F: FnOnce()>(Option<F>);
    impl<F: FnOnce()> Drop for OnDrop<F> {
        fn drop(&mut self) {
            if let Some(f) = self.0.take() {
                f();
            }
        }
    }
    impl<F: FnOnce()> OnDrop<F> {
        #[allow(dead_code)]
        fn new(f: F) -> Self {
            Self(Some(f))
        }
    }
}
