//! Opaque message envelope carrying a signed, nonce-stamped payload.
//!
//! Every message exchanged over the relay is wrapped in an [`Envelope`].
//! The envelope carries:
//!
//! - **`from`** — sender [`PeerId`]
//! - **`to`**   — recipient [`PeerId`] (or the relay itself for control messages)
//! - **`nonce`** — monotonically increasing u64; used for ordering and
//!   replay-attack prevention
//! - **`payload`** — opaque bytes; the relay does not inspect these
//! - **`sig`** — Ed25519 signature over the canonical signed bytes
//!
//! The relay validates the signature before forwarding.  Receivers SHOULD
//! also verify the signature using the sender's known public key.
//!
//! # Payload encryption
//!
//! [`encrypt_payload`] and [`decrypt_payload`] provide authenticated
//! confidentiality via X25519 ECDH + ChaCha20-Poly1305.  The wire format for
//! an encrypted payload is:
//!
//! ```text
//! 12-byte nonce || ChaCha20-Poly1305 ciphertext (includes 16-byte tag)
//! ```
//!
//! Callers use [`Envelope::with_encrypted_payload`] to build an envelope whose
//! payload is encrypted, and [`Envelope::decrypt`] to recover the plaintext.
//! For a gradual rollout, [`Envelope::decrypt`] falls back to returning the
//! raw payload bytes if AEAD decryption fails — callers should migrate senders
//! before removing the fallback.

use anyhow::{Context, Result};
use chacha20poly1305::{
    AeadCore, AeadInPlace, ChaCha20Poly1305, Key, KeyInit, Nonce,
};
use ed25519_dalek::{Signature, SignatureError};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

use crate::identity::{Identity, PeerId};

// ---------------------------------------------------------------------------
// EnvelopeError
// ---------------------------------------------------------------------------

/// Errors produced by envelope crypto operations.
#[derive(Debug, Error)]
pub enum EnvelopeError {
    /// AEAD decryption failed (wrong key, truncated ciphertext, or corruption).
    #[error("payload decryption failed: authentication tag mismatch or truncated ciphertext")]
    DecryptionFailed,

    /// Ciphertext is too short to contain the 12-byte nonce prefix.
    #[error("ciphertext too short: need at least 12 bytes for nonce prefix, got {0}")]
    CiphertextTooShort(usize),
}

// ---------------------------------------------------------------------------
// Envelope
// ---------------------------------------------------------------------------

/// A signed, nonce-stamped message envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    /// Sender peer identifier.
    pub from: String,
    /// Recipient peer identifier.
    pub to: String,
    /// Opaque payload bytes (application-defined; may be encrypted).
    #[serde(with = "serde_bytes_base64")]
    pub payload: Vec<u8>,
    /// 64-byte Ed25519 signature over the canonical signed bytes.
    #[serde(with = "serde_bytes_base64")]
    pub sig: Vec<u8>,
    /// Monotonic nonce — prevents replay; used for ordering.
    pub nonce: u64,
}

impl Envelope {
    /// Build and sign a new envelope with a **plaintext** payload.
    ///
    /// The `nonce` should be unique per sender session; callers typically
    /// maintain a `u64` counter starting at 0.
    #[must_use]
    pub fn new(identity: &Identity, to: &PeerId, payload: Vec<u8>, nonce: u64) -> Self {
        let from = identity.peer_id.to_string();
        let to_str = to.to_string();
        let bytes = canonical_signed_bytes(&from, &to_str, nonce, &payload);
        let sig = identity.sign(&bytes);

        Self {
            from,
            to: to_str,
            payload,
            sig: sig.to_bytes().to_vec(),
            nonce,
        }
    }

    /// Build and sign an envelope whose payload is **encrypted** via X25519
    /// ECDH + ChaCha20-Poly1305.
    ///
    /// The signature covers the *encrypted* payload bytes, so the relay can
    /// validate authenticity without being able to read the contents.
    ///
    /// # Errors
    ///
    /// Returns [`EnvelopeError`] (wrapped in [`anyhow::Error`]) if encryption
    /// fails, which in practice only happens on RNG failure.
    pub fn with_encrypted_payload(
        identity: &Identity,
        to: &PeerId,
        plaintext: &[u8],
        nonce: u64,
        recipient_public_key: &[u8; 32],
        sender_secret_key: &[u8; 32],
    ) -> Result<Self> {
        let ciphertext = encrypt_payload(plaintext, recipient_public_key, sender_secret_key)
            .context("failed to encrypt envelope payload")?;
        Ok(Self::new(identity, to, ciphertext, nonce))
    }

    /// Decrypt this envelope's payload using the sender's X25519 public key
    /// and the recipient's X25519 secret key.
    ///
    /// On success returns the plaintext bytes.
    ///
    /// **Fallback behaviour**: if AEAD decryption fails the raw payload bytes
    /// are returned unchanged, allowing a gradual rollout where some senders
    /// have not yet enabled encryption.  Remove this fallback once all senders
    /// are upgraded.
    #[must_use]
    pub fn decrypt(
        &self,
        sender_public_key: &[u8; 32],
        recipient_secret_key: &[u8; 32],
    ) -> Vec<u8> {
        match decrypt_payload(&self.payload, sender_public_key, recipient_secret_key) {
            Ok(plaintext) => plaintext,
            // Decryption failed — treat as unencrypted plaintext (gradual rollout).
            Err(_) => self.payload.clone(),
        }
    }

    /// Verify the envelope's signature against the provided verifying key.
    ///
    /// Returns `Ok(())` on success.
    pub fn verify(&self, vk: &ed25519_dalek::VerifyingKey) -> Result<(), SignatureError> {
        let bytes = canonical_signed_bytes(&self.from, &self.to, self.nonce, &self.payload);
        let sig_bytes: [u8; 64] = self.sig.as_slice().try_into().map_err(|_| {
            // Signature byte-length mismatch — treat as invalid signature.
            SignatureError::new()
        })?;
        let sig = Signature::from_bytes(&sig_bytes);
        use ed25519_dalek::Verifier;
        vk.verify(&bytes, &sig)
    }

    /// Serialize the envelope to JSON bytes (wire format).
    pub fn to_wire(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self).context("envelope serialization failed")
    }

    /// Deserialize an envelope from JSON bytes.
    pub fn from_wire(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).context("envelope deserialization failed")
    }
}

// ---------------------------------------------------------------------------
// Payload encryption / decryption
// ---------------------------------------------------------------------------

/// Encrypt `payload` using X25519 ECDH + ChaCha20-Poly1305.
///
/// Derives a shared secret from `recipient_public_key` and
/// `sender_secret_key` via X25519, uses it as the ChaCha20-Poly1305 key,
/// and encrypts the payload with a fresh random 12-byte nonce.
///
/// # Wire format
///
/// ```text
/// 12-byte nonce || ChaCha20-Poly1305 ciphertext (plaintext + 16-byte tag)
/// ```
///
/// # Errors
///
/// Returns [`EnvelopeError::DecryptionFailed`] if AEAD encryption fails.
/// In practice this only occurs on catastrophic RNG failure.
pub fn encrypt_payload(
    payload: &[u8],
    recipient_public_key: &[u8; 32],
    sender_secret_key: &[u8; 32],
) -> Result<Vec<u8>, EnvelopeError> {
    let shared_secret = ecdh_shared_secret(sender_secret_key, recipient_public_key);
    let key = Key::from_slice(&shared_secret);
    let cipher = ChaCha20Poly1305::new(key);

    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);

    // Encrypt in-place on a copy of the payload.
    let mut buf = payload.to_vec();
    cipher
        .encrypt_in_place(&nonce, b"", &mut buf)
        .map_err(|_| EnvelopeError::DecryptionFailed)?;

    // Prepend the 12-byte nonce.
    let mut out = Vec::with_capacity(12 + buf.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&buf);
    Ok(out)
}

/// Decrypt `ciphertext` (nonce || ciphertext) using X25519 ECDH +
/// ChaCha20-Poly1305.
///
/// Derives the shared secret from `sender_public_key` and
/// `recipient_secret_key`, then decrypts and authenticates the payload.
///
/// # Errors
///
/// - [`EnvelopeError::CiphertextTooShort`] if `ciphertext` is less than 12 bytes.
/// - [`EnvelopeError::DecryptionFailed`] if the AEAD tag does not verify
///   (wrong key, corrupted bytes, or replay with wrong nonce).
pub fn decrypt_payload(
    ciphertext: &[u8],
    sender_public_key: &[u8; 32],
    recipient_secret_key: &[u8; 32],
) -> Result<Vec<u8>, EnvelopeError> {
    const NONCE_LEN: usize = 12;
    if ciphertext.len() < NONCE_LEN {
        return Err(EnvelopeError::CiphertextTooShort(ciphertext.len()));
    }

    let nonce = Nonce::from_slice(&ciphertext[..NONCE_LEN]);
    let shared_secret = ecdh_shared_secret(recipient_secret_key, sender_public_key);
    let key = Key::from_slice(&shared_secret);
    let cipher = ChaCha20Poly1305::new(key);

    let mut buf = ciphertext[NONCE_LEN..].to_vec();
    cipher
        .decrypt_in_place(nonce, b"", &mut buf)
        .map_err(|_| EnvelopeError::DecryptionFailed)?;

    Ok(buf)
}

// ---------------------------------------------------------------------------
// X25519 ECDH helper
// ---------------------------------------------------------------------------

/// Perform X25519 Diffie-Hellman and return the 32-byte raw shared secret.
///
/// The caller is responsible for using this secret as a key directly (here:
/// as a ChaCha20-Poly1305 key).  The shared secret is the output of the
/// X25519 field operation and is 32 bytes of high-entropy uniformly random
/// material suitable for direct use as an AEAD key.
fn ecdh_shared_secret(secret_key_bytes: &[u8; 32], peer_public_key_bytes: &[u8; 32]) -> [u8; 32] {
    let secret = StaticSecret::from(*secret_key_bytes);
    let public = X25519PublicKey::from(*peer_public_key_bytes);
    secret.diffie_hellman(&public).to_bytes()
}

// ---------------------------------------------------------------------------
// Canonical signed bytes  (length-prefixed — no NUL-separator ambiguity)
// ---------------------------------------------------------------------------

/// Produce the byte string that is actually signed / verified.
///
/// # Encoding
///
/// Each variable-length field (`from`, `to`, `payload`) is prefixed by its
/// length as a little-endian `u32`.  The `nonce` is encoded as a
/// little-endian `u64`.
///
/// ```text
/// u32le(len(from)) || from_utf8
/// u32le(len(to))   || to_utf8
/// u64le(nonce)
/// u32le(len(payload)) || payload
/// ```
///
/// This avoids the NUL-separator ambiguity of the previous format: a `PeerId`
/// containing a NUL byte (unlikely but possible for raw-string identifiers)
/// could produce the same signed bytes as a different pair of identifiers
/// under the old concatenate-with-NUL scheme.
pub(crate) fn canonical_signed_bytes(from: &str, to: &str, nonce: u64, payload: &[u8]) -> Vec<u8> {
    let from_b = from.as_bytes();
    let to_b = to.as_bytes();
    let mut out = Vec::with_capacity(
        4 + from_b.len() + 4 + to_b.len() + 8 + 4 + payload.len(),
    );

    // from
    out.extend_from_slice(&(from_b.len() as u32).to_le_bytes());
    out.extend_from_slice(from_b);

    // to
    out.extend_from_slice(&(to_b.len() as u32).to_le_bytes());
    out.extend_from_slice(to_b);

    // nonce
    out.extend_from_slice(&nonce.to_le_bytes());

    // payload
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);

    out
}

// ---------------------------------------------------------------------------
// serde helper: base64-encode Vec<u8>
// ---------------------------------------------------------------------------

mod serde_bytes_base64 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], ser: S) -> Result<S::Ok, S::Error> {
        // Use standard base64 alphabet.
        let s = BASE64.encode(bytes);
        ser.serialize_str(&s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(de: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(de)?;
        BASE64
            .decode(s.as_bytes())
            .map_err(serde::de::Error::custom)
    }

    static BASE64: &base64_impl::Engine = &base64_impl::ENGINE;

    mod base64_impl {
        pub struct Engine;
        pub static ENGINE: Engine = Engine;

        impl Engine {
            pub fn encode(&self, input: &[u8]) -> String {
                // Pure-Rust base64 without an extra dependency.
                static CHARS: &[u8; 64] =
                    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
                let mut out = Vec::with_capacity(input.len().div_ceil(3) * 4);
                for chunk in input.chunks(3) {
                    let b0 = chunk[0];
                    let b1 = if chunk.len() > 1 { chunk[1] } else { 0 };
                    let b2 = if chunk.len() > 2 { chunk[2] } else { 0 };
                    out.push(CHARS[(b0 >> 2) as usize]);
                    out.push(CHARS[((b0 & 3) << 4 | b1 >> 4) as usize]);
                    if chunk.len() > 1 {
                        out.push(CHARS[((b1 & 15) << 2 | b2 >> 6) as usize]);
                    } else {
                        out.push(b'=');
                    }
                    if chunk.len() > 2 {
                        out.push(CHARS[(b2 & 63) as usize]);
                    } else {
                        out.push(b'=');
                    }
                }
                String::from_utf8(out).unwrap()
            }

            pub fn decode(&self, input: &[u8]) -> Result<Vec<u8>, &'static str> {
                fn val(c: u8) -> Result<u8, &'static str> {
                    match c {
                        b'A'..=b'Z' => Ok(c - b'A'),
                        b'a'..=b'z' => Ok(c - b'a' + 26),
                        b'0'..=b'9' => Ok(c - b'0' + 52),
                        b'+' => Ok(62),
                        b'/' => Ok(63),
                        b'=' => Ok(0),
                        _ => Err("invalid base64 character"),
                    }
                }
                // Strip padding and whitespace for robustness.
                let clean: Vec<u8> =
                    input.iter().copied().filter(|&c| c != b'\n' && c != b'\r').collect();
                if !clean.len().is_multiple_of(4) {
                    return Err("base64 input length not a multiple of 4");
                }
                let mut out = Vec::with_capacity(clean.len() / 4 * 3);
                for chunk in clean.chunks(4) {
                    let v0 = val(chunk[0])?;
                    let v1 = val(chunk[1])?;
                    let v2 = val(chunk[2])?;
                    let v3 = val(chunk[3])?;
                    out.push(v0 << 2 | v1 >> 4);
                    if chunk[2] != b'=' {
                        out.push((v1 & 15) << 4 | v2 >> 2);
                    }
                    if chunk[3] != b'=' {
                        out.push((v2 & 3) << 6 | v3);
                    }
                }
                Ok(out)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Generate a random X25519 static keypair and return (secret_bytes, public_bytes).
    fn x25519_keypair() -> ([u8; 32], [u8; 32]) {
        let secret = StaticSecret::random_from_rng(OsRng);
        let public = X25519PublicKey::from(&secret);
        (secret.to_bytes(), public.to_bytes())
    }

    fn alice() -> Identity {
        Identity::generate_ephemeral()
    }

    fn bob() -> Identity {
        Identity::generate_ephemeral()
    }

    // -----------------------------------------------------------------------
    // Existing envelope tests (preserved / adapted for new canonical_signed_bytes)
    // -----------------------------------------------------------------------

    #[test]
    fn envelope_round_trip() {
        let a = alice();
        let b = bob();
        let payload = b"hello from alice".to_vec();

        let env = Envelope::new(&a, &b.peer_id, payload.clone(), 0);
        let wire = env.to_wire().unwrap();
        let decoded = Envelope::from_wire(&wire).unwrap();

        assert_eq!(decoded.from, a.peer_id.to_string());
        assert_eq!(decoded.to, b.peer_id.to_string());
        assert_eq!(decoded.payload, payload);
        assert_eq!(decoded.nonce, 0);
    }

    #[test]
    fn envelope_signature_verifies() {
        let a = alice();
        let b = bob();
        let env = Envelope::new(&a, &b.peer_id, b"data".to_vec(), 42);

        assert!(
            env.verify(&a.verifying_key()).is_ok(),
            "signature should verify with sender's key"
        );
    }

    #[test]
    fn envelope_wrong_key_fails() {
        let a = alice();
        let b = bob();
        let env = Envelope::new(&a, &b.peer_id, b"data".to_vec(), 1);

        assert!(
            env.verify(&b.verifying_key()).is_err(),
            "signature must not verify with a different key"
        );
    }

    #[test]
    fn envelope_tampered_payload_fails() {
        let a = alice();
        let b = bob();
        let mut env = Envelope::new(&a, &b.peer_id, b"original".to_vec(), 2);
        env.payload = b"tampered".to_vec();

        assert!(
            env.verify(&a.verifying_key()).is_err(),
            "tampered payload must invalidate signature"
        );
    }

    #[test]
    fn envelope_nonce_increments() {
        let a = alice();
        let b = bob();
        let e0 = Envelope::new(&a, &b.peer_id, vec![], 0);
        let e1 = Envelope::new(&a, &b.peer_id, vec![], 1);
        assert_ne!(e0.nonce, e1.nonce);
        // Different nonces produce different signatures.
        assert_ne!(e0.sig, e1.sig);
    }

    // -----------------------------------------------------------------------
    // encrypt/decrypt roundtrip
    // -----------------------------------------------------------------------

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let (alice_sk, alice_pk) = x25519_keypair();
        let (bob_sk, bob_pk) = x25519_keypair();

        let plaintext = b"secret message from alice to bob";

        // Alice encrypts for Bob.
        let ciphertext = encrypt_payload(plaintext, &bob_pk, &alice_sk)
            .expect("encryption must succeed");

        // Ciphertext must differ from plaintext.
        assert_ne!(ciphertext.as_slice(), plaintext.as_slice());
        // Wire format is nonce (12) + ciphertext_with_tag (plaintext_len + 16).
        assert_eq!(ciphertext.len(), 12 + plaintext.len() + 16);

        // Bob decrypts with Alice's public key and his own secret key.
        let recovered = decrypt_payload(&ciphertext, &alice_pk, &bob_sk)
            .expect("decryption must succeed");

        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn encrypt_decrypt_roundtrip_empty_payload() {
        let (alice_sk, alice_pk) = x25519_keypair();
        let (bob_sk, bob_pk) = x25519_keypair();

        let ciphertext = encrypt_payload(b"", &bob_pk, &alice_sk).unwrap();
        // Empty plaintext: 12 nonce + 16 tag.
        assert_eq!(ciphertext.len(), 28);

        let recovered = decrypt_payload(&ciphertext, &alice_pk, &bob_sk).unwrap();
        assert!(recovered.is_empty());
    }

    #[test]
    fn decrypt_fails_with_wrong_key() {
        let (alice_sk, _alice_pk) = x25519_keypair();
        let (_bob_sk, bob_pk) = x25519_keypair();
        let (eve_sk, _eve_pk) = x25519_keypair();

        let ciphertext = encrypt_payload(b"secret", &bob_pk, &alice_sk).unwrap();

        // Eve uses her own secret key but presents Bob's public key — wrong shared secret.
        let result = decrypt_payload(&ciphertext, &bob_pk, &eve_sk);
        assert!(
            result.is_err(),
            "decryption with wrong secret key must fail"
        );
    }

    #[test]
    fn decrypt_fails_with_truncated_ciphertext() {
        let result = decrypt_payload(&[0u8; 5], &[0u8; 32], &[0u8; 32]);
        assert!(matches!(result, Err(EnvelopeError::CiphertextTooShort(5))));
    }

    #[test]
    fn decrypt_fails_with_corrupted_tag() {
        let (alice_sk, alice_pk) = x25519_keypair();
        let (bob_sk, bob_pk) = x25519_keypair();

        let mut ciphertext = encrypt_payload(b"hello", &bob_pk, &alice_sk).unwrap();
        // Corrupt the last byte of the authentication tag.
        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 0xFF;

        let result = decrypt_payload(&ciphertext, &alice_pk, &bob_sk);
        assert!(
            result.is_err(),
            "corrupted AEAD tag must cause decryption failure"
        );
    }

    // -----------------------------------------------------------------------
    // Envelope-level encrypt/decrypt integration
    // -----------------------------------------------------------------------

    #[test]
    fn envelope_with_encrypted_payload_roundtrip() {
        let a = alice();
        let b = bob();

        let (alice_sk, alice_pk) = x25519_keypair();
        let (bob_sk, bob_pk) = x25519_keypair();

        let plaintext = b"encrypted envelope payload";

        let env = Envelope::with_encrypted_payload(&a, &b.peer_id, plaintext, 7, &bob_pk, &alice_sk)
            .expect("envelope creation must succeed");

        // Signature covers the encrypted bytes.
        assert!(env.verify(&a.verifying_key()).is_ok());

        // Payload on wire is not the plaintext.
        assert_ne!(env.payload.as_slice(), plaintext.as_slice());

        // Bob decrypts.
        let recovered = env.decrypt(&alice_pk, &bob_sk);
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn envelope_decrypt_fallback_on_plaintext() {
        // Simulate receiving an unencrypted envelope (from an old sender).
        let a = alice();
        let b = bob();
        let plaintext = b"unencrypted payload";

        let env = Envelope::new(&a, &b.peer_id, plaintext.to_vec(), 0);

        let (_, alice_pk) = x25519_keypair();
        let (bob_sk, _) = x25519_keypair();

        // decrypt() should fall back and return the raw payload unchanged.
        let result = env.decrypt(&alice_pk, &bob_sk);
        assert_eq!(result, plaintext);
    }

    // -----------------------------------------------------------------------
    // canonical_signed_bytes — length-prefixed encoding
    // -----------------------------------------------------------------------

    #[test]
    fn canonical_signed_bytes_length_prefixed_no_nul_confusion() {
        // Under the old NUL-separator scheme, these two pairs of (from, to)
        // would produce identical canonical bytes because NUL is a valid
        // character and the field boundary would be ambiguous:
        //   from="a\x00b", to="c"   →  "a\x00b\x00c\x00..."
        //   from="a",      to="\x00bc"  →  "a\x00\x00bc\x00..."
        // Under length-prefixed encoding they must be distinct.
        let payload = b"payload";
        let nonce = 0u64;

        let b1 = canonical_signed_bytes("a\x00b", "c", nonce, payload);
        let b2 = canonical_signed_bytes("a", "\x00bc", nonce, payload);

        assert_ne!(
            b1, b2,
            "length-prefixed encoding must distinguish NUL-containing peer IDs"
        );
    }

    #[test]
    fn nul_in_peer_id_doesnt_bypass_signature() {
        // An attacker crafting a PeerId with embedded NUL bytes must not be
        // able to forge a valid signature for a different (from, to) pair.
        let a = alice();
        let b = bob();

        let env = Envelope::new(&a, &b.peer_id, b"data".to_vec(), 0);

        // Fabricate a fake "from" that would collide under NUL-separator scheme.
        // Under length-prefixed encoding the signed bytes are unambiguous, so
        // mutating `from` must invalidate the signature regardless of NUL presence.
        let mut forged = env.clone();
        forged.from = format!("{}\x00extra", env.from);

        assert!(
            forged.verify(&a.verifying_key()).is_err(),
            "NUL-containing forged from field must not verify with original key"
        );
    }

    #[test]
    fn canonical_signed_bytes_different_nonces_differ() {
        let from = "peer-a";
        let to = "peer-b";
        let payload = b"hello";
        let b0 = canonical_signed_bytes(from, to, 0, payload);
        let b1 = canonical_signed_bytes(from, to, 1, payload);
        assert_ne!(b0, b1);
    }

    #[test]
    fn canonical_signed_bytes_different_payloads_differ() {
        let from = "peer-a";
        let to = "peer-b";
        let b0 = canonical_signed_bytes(from, to, 0, b"aaa");
        let b1 = canonical_signed_bytes(from, to, 0, b"bbb");
        assert_ne!(b0, b1);
    }
}
