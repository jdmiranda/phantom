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
//! confidentiality via X25519 ECDH → HKDF-SHA256 → ChaCha20-Poly1305.  The
//! raw X25519 shared secret is *not* used as an AEAD key directly; it is
//! passed through HKDF-SHA256 with a fixed `info` string for domain
//! separation.  This defends against low-order-point inputs that could
//! otherwise produce predictable shared secrets.  See [`derive_aead_key`].
//!
//! AEAD additional data binds `from || to || nonce` to the ciphertext, so a
//! ciphertext lifted out of one envelope cannot be replayed inside an
//! envelope addressed to a different recipient even if the relay is hostile.
//!
//! The wire format for an encrypted payload is:
//!
//! ```text
//! 12-byte nonce || ChaCha20-Poly1305 ciphertext (includes 16-byte tag)
//! ```
//!
//! Callers use [`Envelope::with_encrypted_payload`] to build an envelope whose
//! payload is encrypted, and [`Envelope::decrypt`] to recover the plaintext.
//! [`Envelope::decrypt`] returns [`Result`] — there is no silent fallback to
//! raw payload bytes.  Callers that expect either ciphertext or plaintext
//! must distinguish at a higher protocol layer.
//!
//! # Security limitations
//!
//! This implementation uses **static-static X25519** (the sender's long-lived
//! [`x25519_dalek::StaticSecret`] and the recipient's long-lived
//! [`x25519_dalek::PublicKey`]).  It does *not* provide forward secrecy:
//! compromise of either long-lived secret retroactively decrypts past
//! envelopes.  A follow-up that extends the envelope schema with an ephemeral
//! sender public key would provide forward secrecy; that work is out of
//! scope for this change and tracked separately.

use anyhow::{Context, Result};
use chacha20poly1305::{
    AeadCore, AeadInPlace, ChaCha20Poly1305, Key, KeyInit, Nonce,
};
use ed25519_dalek::{Signature, SignatureError};
use hkdf::Hkdf;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use thiserror::Error;
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

use crate::identity::{Identity, PeerId};

/// HKDF `info` string for the AEAD key derivation.  Domain-separates the
/// derived key from any other use of an X25519 shared secret in this
/// codebase.  Bump the version suffix if the wire format changes.
const HKDF_INFO: &[u8] = b"phantom-relay-envelope-v1 aead key";

// ---------------------------------------------------------------------------
// EnvelopeError
// ---------------------------------------------------------------------------

/// Errors produced by envelope crypto operations.
#[derive(Debug, Error)]
pub enum EnvelopeError {
    /// AEAD encryption failed.  In practice only happens on catastrophic RNG
    /// failure or a buffer-allocation panic.
    #[error("payload encryption failed")]
    EncryptionFailed,

    /// AEAD decryption failed (wrong key, truncated ciphertext, mismatched
    /// associated data, or corruption).
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
    /// ECDH + HKDF-SHA256 + ChaCha20-Poly1305.
    ///
    /// `from`, `to`, and `nonce` are bound into the AEAD associated data so
    /// the ciphertext is not transferable between identities at the AEAD
    /// layer.  The signature covers the *encrypted* payload bytes, so the
    /// relay can validate authenticity without being able to read the
    /// contents.
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
        let from = identity.peer_id.to_string();
        let to_str = to.to_string();
        let aad = aead_associated_data(&from, &to_str, nonce);
        let ciphertext = encrypt_payload_with_aad(
            plaintext,
            recipient_public_key,
            sender_secret_key,
            &aad,
        )
        .context("failed to encrypt envelope payload")?;
        Ok(Self::new(identity, to, ciphertext, nonce))
    }

    /// Decrypt this envelope's payload using the sender's X25519 public key
    /// and the recipient's X25519 secret key.
    ///
    /// The envelope's `from`, `to`, and `nonce` fields are re-bound into the
    /// AEAD associated data — a ciphertext spliced out of a different
    /// envelope will fail authentication.
    ///
    /// # Errors
    ///
    /// - [`EnvelopeError::CiphertextTooShort`] if the payload is shorter than
    ///   the 12-byte nonce prefix.
    /// - [`EnvelopeError::DecryptionFailed`] if the AEAD tag does not verify
    ///   (wrong key, corrupted bytes, mismatched associated data, replay
    ///   with the wrong nonce).
    ///
    /// There is no silent fallback to raw payload bytes; callers that need
    /// to handle a mix of encrypted and plaintext senders must distinguish
    /// at a higher protocol layer.
    pub fn decrypt(
        &self,
        sender_public_key: &[u8; 32],
        recipient_secret_key: &[u8; 32],
    ) -> Result<Vec<u8>, EnvelopeError> {
        let aad = aead_associated_data(&self.from, &self.to, self.nonce);
        decrypt_payload_with_aad(
            &self.payload,
            sender_public_key,
            recipient_secret_key,
            &aad,
        )
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

/// Encrypt `payload` using X25519 ECDH → HKDF-SHA256 → ChaCha20-Poly1305.
///
/// Convenience wrapper around [`encrypt_payload_with_aad`] with empty
/// associated data.  Prefer the AAD variant whenever there is contextual
/// metadata (peer identifiers, nonce) to bind to the ciphertext.
///
/// # Wire format
///
/// ```text
/// 12-byte nonce || ChaCha20-Poly1305 ciphertext (plaintext + 16-byte tag)
/// ```
///
/// # Errors
///
/// Returns [`EnvelopeError::EncryptionFailed`] if AEAD encryption fails.
/// In practice this only occurs on catastrophic RNG failure.
pub fn encrypt_payload(
    payload: &[u8],
    recipient_public_key: &[u8; 32],
    sender_secret_key: &[u8; 32],
) -> Result<Vec<u8>, EnvelopeError> {
    encrypt_payload_with_aad(payload, recipient_public_key, sender_secret_key, b"")
}

/// Encrypt `payload` with explicit AEAD associated data.
///
/// The associated data is authenticated but not encrypted; decryption must
/// be performed with byte-identical AAD or the tag check will fail.
///
/// # Errors
///
/// Returns [`EnvelopeError::EncryptionFailed`] on AEAD failure.
pub fn encrypt_payload_with_aad(
    payload: &[u8],
    recipient_public_key: &[u8; 32],
    sender_secret_key: &[u8; 32],
    associated_data: &[u8],
) -> Result<Vec<u8>, EnvelopeError> {
    let aead_key = derive_aead_key(sender_secret_key, recipient_public_key);
    let key = Key::from_slice(&aead_key);
    let cipher = ChaCha20Poly1305::new(key);

    let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);

    // Encrypt in-place on a copy of the payload.
    let mut buf = payload.to_vec();
    cipher
        .encrypt_in_place(&nonce, associated_data, &mut buf)
        .map_err(|_| EnvelopeError::EncryptionFailed)?;

    // Prepend the 12-byte nonce.
    let mut out = Vec::with_capacity(12 + buf.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&buf);
    Ok(out)
}

/// Decrypt `ciphertext` (nonce || ciphertext) using X25519 ECDH → HKDF-SHA256
/// → ChaCha20-Poly1305.
///
/// Convenience wrapper around [`decrypt_payload_with_aad`] with empty
/// associated data.
///
/// # Errors
///
/// - [`EnvelopeError::CiphertextTooShort`] if `ciphertext` is less than 12 bytes.
/// - [`EnvelopeError::DecryptionFailed`] if the AEAD tag does not verify
///   (wrong key, corrupted bytes, mismatched AAD, or replay with wrong nonce).
pub fn decrypt_payload(
    ciphertext: &[u8],
    sender_public_key: &[u8; 32],
    recipient_secret_key: &[u8; 32],
) -> Result<Vec<u8>, EnvelopeError> {
    decrypt_payload_with_aad(ciphertext, sender_public_key, recipient_secret_key, b"")
}

/// Decrypt with explicit AEAD associated data.
///
/// # Errors
///
/// Same as [`decrypt_payload`].
pub fn decrypt_payload_with_aad(
    ciphertext: &[u8],
    sender_public_key: &[u8; 32],
    recipient_secret_key: &[u8; 32],
    associated_data: &[u8],
) -> Result<Vec<u8>, EnvelopeError> {
    const NONCE_LEN: usize = 12;
    if ciphertext.len() < NONCE_LEN {
        return Err(EnvelopeError::CiphertextTooShort(ciphertext.len()));
    }

    let nonce = Nonce::from_slice(&ciphertext[..NONCE_LEN]);
    let aead_key = derive_aead_key(recipient_secret_key, sender_public_key);
    let key = Key::from_slice(&aead_key);
    let cipher = ChaCha20Poly1305::new(key);

    let mut buf = ciphertext[NONCE_LEN..].to_vec();
    cipher
        .decrypt_in_place(nonce, associated_data, &mut buf)
        .map_err(|_| EnvelopeError::DecryptionFailed)?;

    Ok(buf)
}

// ---------------------------------------------------------------------------
// X25519 ECDH + HKDF helper
// ---------------------------------------------------------------------------

/// Derive a 32-byte ChaCha20-Poly1305 key from an X25519 shared secret via
/// HKDF-SHA256.
///
/// X25519 produces a 32-byte point coordinate, not uniformly random bytes,
/// and pathological peer public keys (low-order points on Curve25519) can
/// produce an all-zero shared secret.  HKDF-SHA256 extracts uniformly
/// distributed key material from the raw output and domain-separates it
/// from any other use of the same shared secret elsewhere in the codebase
/// via [`HKDF_INFO`].
///
/// The function is symmetric: Alice's `(alice_sk, bob_pk)` and Bob's
/// `(bob_sk, alice_pk)` derive the same key.
fn derive_aead_key(
    secret_key_bytes: &[u8; 32],
    peer_public_key_bytes: &[u8; 32],
) -> [u8; 32] {
    let secret = StaticSecret::from(*secret_key_bytes);
    let public = X25519PublicKey::from(*peer_public_key_bytes);
    let shared = secret.diffie_hellman(&public);

    // No salt — the static-static DH output is itself the IKM, and the
    // info string provides domain separation.
    let hk = Hkdf::<Sha256>::new(None, shared.as_bytes());
    let mut okm = [0u8; 32];
    hk.expand(HKDF_INFO, &mut okm)
        .expect("HKDF-SHA256 expand of 32 bytes never fails");
    okm
}

// ---------------------------------------------------------------------------
// AEAD associated-data helper
// ---------------------------------------------------------------------------

/// Encode `from || to || nonce` as length-prefixed AEAD associated data.
///
/// Reusing the same length-prefixed layout as [`canonical_signed_bytes`]
/// (sans `payload`, which is what we are encrypting) ensures the AAD is
/// unambiguously bound to a specific `(from, to, nonce)` triple.
fn aead_associated_data(from: &str, to: &str, nonce: u64) -> Vec<u8> {
    let from_b = from.as_bytes();
    let to_b = to.as_bytes();
    let mut out = Vec::with_capacity(4 + from_b.len() + 4 + to_b.len() + 8);
    out.extend_from_slice(&(from_b.len() as u32).to_le_bytes());
    out.extend_from_slice(from_b);
    out.extend_from_slice(&(to_b.len() as u32).to_le_bytes());
    out.extend_from_slice(to_b);
    out.extend_from_slice(&nonce.to_le_bytes());
    out
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
fn canonical_signed_bytes(from: &str, to: &str, nonce: u64, payload: &[u8]) -> Vec<u8> {
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
        let recovered = env.decrypt(&alice_pk, &bob_sk).expect("decryption must succeed");
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn envelope_decrypt_returns_error_on_plaintext_payload() {
        // A receiver that calls decrypt() on a plaintext envelope must get a
        // hard error, not a silent fallback to the raw bytes — the previous
        // fallback was a downgrade oracle.
        let a = alice();
        let b = bob();
        let plaintext = b"unencrypted payload";

        let env = Envelope::new(&a, &b.peer_id, plaintext.to_vec(), 0);

        let (_, alice_pk) = x25519_keypair();
        let (bob_sk, _) = x25519_keypair();

        let result = env.decrypt(&alice_pk, &bob_sk);
        assert!(
            result.is_err(),
            "decrypting a plaintext payload must return an error, not the raw bytes"
        );
    }

    #[test]
    fn envelope_decrypt_aad_binds_from_to_nonce() {
        // A ciphertext crafted for envelope (from=A, to=B, nonce=N) must not
        // be decryptable when spliced into an envelope addressed differently:
        // the AEAD associated data binds from/to/nonce so a relay-side splice
        // attack fails the AEAD tag check.
        let a = alice();
        let b = bob();
        let c = alice(); // third identity, different from b

        let (alice_sk, alice_pk) = x25519_keypair();
        let (bob_sk, bob_pk) = x25519_keypair();

        let env_to_b =
            Envelope::with_encrypted_payload(&a, &b.peer_id, b"secret", 1, &bob_pk, &alice_sk)
                .unwrap();

        // Splice: keep the encrypted payload but pretend it was sent to C.
        let mut spliced = env_to_b.clone();
        spliced.to = c.peer_id.to_string();

        let result = spliced.decrypt(&alice_pk, &bob_sk);
        assert!(
            result.is_err(),
            "splicing ciphertext into an envelope with a different recipient must fail AEAD auth"
        );

        // And mutating `nonce` similarly fails.
        let mut spliced_nonce = env_to_b.clone();
        spliced_nonce.nonce = 999;
        let result = spliced_nonce.decrypt(&alice_pk, &bob_sk);
        assert!(
            result.is_err(),
            "splicing ciphertext into an envelope with a different nonce must fail AEAD auth"
        );
    }

    #[test]
    fn derive_aead_key_is_symmetric() {
        // Alice's (alice_sk, bob_pk) and Bob's (bob_sk, alice_pk) must derive
        // the same AEAD key after HKDF — otherwise no message could ever be
        // decrypted by the intended recipient.
        let (alice_sk, alice_pk) = x25519_keypair();
        let (bob_sk, bob_pk) = x25519_keypair();

        let k1 = derive_aead_key(&alice_sk, &bob_pk);
        let k2 = derive_aead_key(&bob_sk, &alice_pk);

        assert_eq!(k1, k2, "HKDF-derived AEAD key must be symmetric");
    }

    #[test]
    fn derive_aead_key_low_order_public_key_does_not_panic() {
        // The all-zero X25519 public key is a low-order point that yields an
        // all-zero raw DH output.  HKDF over that zero IKM is well-defined
        // (it just produces a deterministic but unique-to-this-context key)
        // and must not panic; without HKDF the raw zero bytes would have been
        // used directly as an AEAD key, which is catastrophic.
        let (alice_sk, _alice_pk) = x25519_keypair();
        let low_order_pk = [0u8; 32];

        let key = derive_aead_key(&alice_sk, &low_order_pk);
        // Sanity: the derived bytes are not the raw all-zero DH output —
        // HKDF mixes in the info string.
        assert_ne!(key, [0u8; 32]);
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
