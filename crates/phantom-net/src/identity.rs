//! Ed25519 keypair identity with OS keyring persistence.
//!
//! Each Phantom instance owns a stable [`Identity`].  The private key is kept
//! in the OS keyring (macOS Keychain / libsecret / Windows Credential Store)
//! so it survives restarts without writing a plaintext key file to disk.
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
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, Result};
use bs58;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Per-process Identity cache
// ---------------------------------------------------------------------------
//
// `Identity::load_or_generate` reads the OS keyring on every invocation by
// default.  On macOS each read is gated by an ACL prompt unless the user has
// previously clicked "Always Allow" for the exact signed binary.  Because the
// same Phantom process has multiple independent callers (relay client, hub
// listener, inspector), repeated reads turn into prompt spam.
//
// To fix this we memoize the resolved `Identity` per service string in a
// process-wide cache.  The cache lives only in heap memory — it never
// persists keys to disk.  Cloning an `Identity` is cheap because the inner
// signing material lives behind an `Arc`.
static IDENTITY_CACHE: LazyLock<Mutex<HashMap<String, Identity>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

// Test-only counter that observes how many times the keyring was actually
// read for each distinct service string.  The cache test asserts that
// repeated `load_or_generate` calls for the same service do not increment
// the per-service count.  Keying by service string lets the test run in
// parallel with other identity tests without false positives.
#[cfg(test)]
static KEYRING_READ_COUNTS: LazyLock<Mutex<HashMap<String, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
fn record_keyring_read(service: &str) {
    let mut counts = KEYRING_READ_COUNTS
        .lock()
        .expect("keyring read count mutex poisoned");
    *counts.entry(service.to_owned()).or_insert(0) += 1;
}

#[cfg(not(test))]
fn record_keyring_read(_service: &str) {}

#[cfg(test)]
fn keyring_read_count_for(service: &str) -> usize {
    let counts = KEYRING_READ_COUNTS
        .lock()
        .expect("keyring read count mutex poisoned");
    counts.get(service).copied().unwrap_or(0)
}

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
// Identity
// ---------------------------------------------------------------------------

/// Ed25519 signing identity with a stable, OS-keyring-backed private key.
///
/// Cheap to clone: the underlying signing key lives behind an `Arc`, so a
/// clone is just a refcount bump.  This makes it safe to hand the same
/// `Identity` to multiple subsystems without re-reading the OS keyring.
#[derive(Clone)]
pub struct Identity {
    keypair: Arc<SigningKey>,
    /// The public peer identifier derived from this keypair.
    pub peer_id: PeerId,
}

impl Identity {
    // Keyring service name format: "phantom-net/{service}".
    // The account name is "signing-key".
    const ACCOUNT: &'static str = "signing-key";

    fn keyring_entry(service: &str) -> Result<keyring::Entry> {
        let service_key = format!("phantom-net/{service}");
        keyring::Entry::new(&service_key, Self::ACCOUNT)
            .context("failed to open keyring entry")
    }

    /// Load an existing keypair from the OS keyring, or generate and store a
    /// new one.
    ///
    /// `service` is a short string used to namespace the keyring entry
    /// (e.g. `"phantom"` or `"phantom-test"`).  Different services get
    /// separate keys.
    ///
    /// The result is memoized in a process-wide cache keyed by `service`, so
    /// repeated calls within the same process touch the OS keyring at most
    /// once per distinct service string.  This prevents repeated macOS
    /// Keychain ACL prompts when multiple subsystems each load the same
    /// identity.
    ///
    /// # Errors
    /// Returns an error if the keyring is unavailable or the stored bytes are
    /// corrupt.
    pub fn load_or_generate(service: &str) -> Result<Self> {
        // Fast path — already cached.
        {
            let guard = IDENTITY_CACHE
                .lock()
                .expect("identity cache mutex poisoned");
            if let Some(cached) = guard.get(service) {
                return Ok(cached.clone());
            }
        }

        // Slow path — read the keyring (or generate a fresh key).  Done
        // outside the cache lock so concurrent loads of *different* services
        // do not serialize on this mutex during the keyring round-trip.
        let identity = Self::load_or_generate_uncached(service)?;

        // Insert into the cache.  If another thread raced us and inserted
        // first, prefer the existing entry so every caller observes the same
        // `Identity` value for a given service in this process.
        let mut guard = IDENTITY_CACHE
            .lock()
            .expect("identity cache mutex poisoned");
        let entry = guard
            .entry(service.to_owned())
            .or_insert_with(|| identity.clone());
        Ok(entry.clone())
    }

    /// Internal: actually read the OS keyring.  Bypasses the per-process
    /// cache.  Callers that want the cached behavior should use
    /// [`Identity::load_or_generate`].
    fn load_or_generate_uncached(service: &str) -> Result<Self> {
        let entry = Self::keyring_entry(service)?;

        record_keyring_read(service);

        let signing_key = match entry.get_password() {
            Ok(hex) => {
                // Stored as 64 hex chars (32 bytes).
                let bytes = hex::decode_32(&hex)
                    .context("stored signing key is corrupt — regenerating")?;
                SigningKey::from_bytes(&bytes)
            }
            Err(keyring::Error::NoEntry) => {
                let key = SigningKey::generate(&mut OsRng);
                let hex = hex::encode_32(key.to_bytes());
                entry
                    .set_password(&hex)
                    .context("failed to persist new signing key to keyring")?;
                key
            }
            Err(e) => {
                anyhow::bail!("keyring error: {e}");
            }
        };

        let peer_id = PeerId::from_verifying_key(&signing_key.verifying_key());
        Ok(Self {
            keypair: Arc::new(signing_key),
            peer_id,
        })
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

    /// Generate a fresh throwaway identity without touching the keyring.
    ///
    /// Used in unit tests via a mock keyring backend.
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
// Internal hex helpers (avoids pulling in the `hex` crate)
// ---------------------------------------------------------------------------

mod hex {
    use anyhow::{bail, Result};

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

    /// Identity persists across instances — tested via the mock keyring that
    /// `keyring` uses in test builds when no OS keyring is present.  Both calls
    /// to `load_or_generate` should produce the same `PeerId`.
    #[test]
    fn identity_persists_across_instances() {
        // Use a unique service name per test run to avoid cross-test pollution.
        let service = format!("phantom-test-{}", uuid_short());

        let id1 = Identity::load_or_generate(&service).unwrap();
        let id2 = Identity::load_or_generate(&service).unwrap();

        assert_eq!(
            id1.peer_id, id2.peer_id,
            "peer_id must be stable across load_or_generate calls"
        );
    }

    fn uuid_short() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        format!("{ns:08x}")
    }

    /// Repeated calls to `load_or_generate` for the same service must read
    /// the OS keyring at most once per process.  Distinct service strings
    /// each hit the keyring exactly once.  We track read counts per service
    /// (rather than as a single global) so this test stays deterministic
    /// even when other identity tests run concurrently.
    #[test]
    fn cache_skips_keyring_on_repeat_call() {
        let service_a = format!("phantom-cache-a-{}", uuid_short());
        let service_b = format!("phantom-cache-b-{}", uuid_short());

        // First load for service A — must hit the keyring exactly once.
        let id_a1 = Identity::load_or_generate(&service_a).unwrap();
        assert_eq!(
            keyring_read_count_for(&service_a),
            1,
            "first load_or_generate must read the keyring exactly once"
        );

        // Repeat loads for service A — must NOT touch the keyring again.
        let id_a2 = Identity::load_or_generate(&service_a).unwrap();
        let id_a3 = Identity::load_or_generate(&service_a).unwrap();
        assert_eq!(
            keyring_read_count_for(&service_a),
            1,
            "repeated load_or_generate for the same service must not re-read the keyring"
        );
        assert_eq!(id_a1.peer_id, id_a2.peer_id);
        assert_eq!(id_a1.peer_id, id_a3.peer_id);

        // A different service string must hit the keyring exactly once.
        let id_b1 = Identity::load_or_generate(&service_b).unwrap();
        assert_eq!(
            keyring_read_count_for(&service_b),
            1,
            "load_or_generate for a new service must read the keyring exactly once"
        );
        assert_eq!(
            keyring_read_count_for(&service_a),
            1,
            "loading a different service must not re-read service A"
        );
        assert_ne!(
            id_a1.peer_id, id_b1.peer_id,
            "distinct service strings must yield distinct identities"
        );

        // Subsequent loads of B are also cached.
        let _id_b2 = Identity::load_or_generate(&service_b).unwrap();
        assert_eq!(
            keyring_read_count_for(&service_b),
            1,
            "repeat load for service B must also be served from cache"
        );
    }

    /// The cached `Identity` returned to two callers shares its inner
    /// signing material via `Arc`.  Cloning is therefore cheap, and both
    /// clones produce identical signatures over the same input.
    #[test]
    fn cache_returns_shared_signing_material() {
        let service = format!("phantom-cache-share-{}", uuid_short());
        let a = Identity::load_or_generate(&service).unwrap();
        let b = Identity::load_or_generate(&service).unwrap();

        // Same peer_id, same public key, same signature for the same input.
        assert_eq!(a.peer_id, b.peer_id);
        let msg = b"shared cache signature check";
        let sig_a = a.sign(msg);
        let sig_b = b.sign(msg);
        assert_eq!(sig_a.to_bytes(), sig_b.to_bytes());
    }
}
