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

use anyhow::{Context, Result};
use bs58;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;
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
// Identity
// ---------------------------------------------------------------------------

/// Ed25519 signing identity with a stable, OS-keyring-backed private key.
pub struct Identity {
    keypair: SigningKey,
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
    /// # Errors
    /// Returns an error if the keyring is unavailable or the stored bytes are
    /// corrupt.
    pub fn load_or_generate(service: &str) -> Result<Self> {
        let entry = Self::keyring_entry(service)?;

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
            keypair: signing_key,
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
        Self { keypair, peer_id }
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
}
