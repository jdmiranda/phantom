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
//! The master key itself is fetched from the OS keychain in production via
//! [`MasterKey::from_keyring`]. Tests pass an explicit key via
//! [`MasterKey::from_bytes`].
//!
//! Sensitive material is wrapped in [`zeroize::Zeroizing`] so it scrubs on
//! drop.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use phantom_bundles::BundleId;
use rand::RngCore;
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::StoreError;

/// Service name registered in the OS keychain.
const KEYRING_SERVICE: &str = "phantom-bundle-store";
/// Username/account for the master key entry. There is exactly one per host.
const KEYRING_ACCOUNT: &str = "master-key-v1";
/// HKDF info string. Bumping this string is equivalent to a key rotation
/// for everything sealed under the previous string.
const HKDF_INFO_BLOB: &[u8] = b"phantom-bundle-store/blob/v1";
/// Length of the XChaCha20-Poly1305 nonce, in bytes.
const XNONCE_LEN: usize = 24;
/// Magic bytes prefixing a serialized [`BlobEnvelope`]. Lets us version the
/// on-disk envelope format independently of anything else.
const ENVELOPE_MAGIC: &[u8; 4] = b"PBE1";

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
    /// after pulling from the keychain.
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// Borrow the raw key bytes. Used to feed SQLCipher's `PRAGMA key`.
    #[must_use]
    pub fn bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Load the master key from the OS keychain, generating and storing a
    /// fresh random key if no entry yet exists.
    ///
    /// Errors surface as [`StoreError::Keyring`].
    pub fn from_keyring() -> Result<Self, StoreError> {
        let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT)
            .map_err(|e| StoreError::Keyring(format!("entry: {e}")))?;
        match entry.get_password() {
            Ok(b64) => {
                let bytes = decode_b64_32(&b64)
                    .map_err(|e| StoreError::Keyring(format!("decode: {e}")))?;
                Ok(Self::from_bytes(bytes))
            }
            Err(keyring::Error::NoEntry) => {
                let mut bytes = [0_u8; 32];
                rand::thread_rng().fill_bytes(&mut bytes);
                let b64 = encode_b64_32(&bytes);
                entry
                    .set_password(&b64)
                    .map_err(|e| StoreError::Keyring(format!("set: {e}")))?;
                Ok(Self::from_bytes(bytes))
            }
            Err(e) => Err(StoreError::Keyring(format!("get: {e}"))),
        }
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
// Tiny base64 (no_std style) — avoids a heavy dep just to round-trip the
// 32-byte master key through the keychain (which expects a string).
// ---------------------------------------------------------------------------

const B64_ALPHA: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn encode_b64_32(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(44);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n = (u32::from(bytes[i]) << 16) | (u32::from(bytes[i + 1]) << 8) | u32::from(bytes[i + 2]);
        out.push(B64_ALPHA[((n >> 18) & 0x3F) as usize] as char);
        out.push(B64_ALPHA[((n >> 12) & 0x3F) as usize] as char);
        out.push(B64_ALPHA[((n >> 6) & 0x3F) as usize] as char);
        out.push(B64_ALPHA[(n & 0x3F) as usize] as char);
        i += 3;
    }
    // 32 bytes = 30 + 2 leftover.
    if i < bytes.len() {
        let rem = bytes.len() - i;
        let b0 = bytes[i];
        let b1 = if rem > 1 { bytes[i + 1] } else { 0 };
        let n = (u32::from(b0) << 16) | (u32::from(b1) << 8);
        out.push(B64_ALPHA[((n >> 18) & 0x3F) as usize] as char);
        out.push(B64_ALPHA[((n >> 12) & 0x3F) as usize] as char);
        if rem == 2 {
            out.push(B64_ALPHA[((n >> 6) & 0x3F) as usize] as char);
            out.push('=');
        } else {
            out.push('=');
            out.push('=');
        }
    }
    out
}

fn decode_b64_32(s: &str) -> Result<[u8; 32], String> {
    let bytes = s.as_bytes();
    if bytes.len() != 44 {
        return Err(format!("expected 44 b64 chars, got {}", bytes.len()));
    }
    let lookup = |c: u8| -> Result<u32, String> {
        match c {
            b'A'..=b'Z' => Ok(u32::from(c - b'A')),
            b'a'..=b'z' => Ok(u32::from(c - b'a') + 26),
            b'0'..=b'9' => Ok(u32::from(c - b'0') + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            b'=' => Ok(0),
            _ => Err(format!("bad b64 char: {c}")),
        }
    };
    let mut out = [0_u8; 32];
    let mut o = 0;
    let mut i = 0;
    while i < 40 {
        let n = (lookup(bytes[i])? << 18)
            | (lookup(bytes[i + 1])? << 12)
            | (lookup(bytes[i + 2])? << 6)
            | lookup(bytes[i + 3])?;
        out[o] = ((n >> 16) & 0xFF) as u8;
        out[o + 1] = ((n >> 8) & 0xFF) as u8;
        out[o + 2] = (n & 0xFF) as u8;
        o += 3;
        i += 4;
    }
    // 32 bytes mod 3 == 2 leftover, so the final group encodes as
    // `XXX=` (three significant chars + one pad). The encoder above
    // produces exactly that pattern.
    if bytes[43] != b'=' || bytes[42] == b'=' {
        return Err("expected trailing 'XXX=' for 32-byte b64".into());
    }
    let n = (lookup(bytes[40])? << 18)
        | (lookup(bytes[41])? << 12)
        | (lookup(bytes[42])? << 6);
    out[30] = ((n >> 16) & 0xFF) as u8;
    out[31] = ((n >> 8) & 0xFF) as u8;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn b64_round_trip_32_bytes() {
        let mut bytes = [0_u8; 32];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(17).wrapping_add(3);
        }
        let s = encode_b64_32(&bytes);
        assert_eq!(s.len(), 44);
        let back = decode_b64_32(&s).expect("decode");
        assert_eq!(bytes, back);
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
        assert_ne!(dek_a.0.as_ref(), dek_c.0.as_ref(), "different bundles get different DEKs");
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
}
