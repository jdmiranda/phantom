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
//!   (`from || to || nonce_le || payload`)
//!
//! The relay validates the signature before forwarding.  Receivers SHOULD
//! also verify the signature using the sender's known public key.

use anyhow::{Context, Result};
use ed25519_dalek::{Signature, SignatureError};
use serde::{Deserialize, Serialize};

use crate::identity::{Identity, PeerId};

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
    /// Opaque payload bytes (application-defined).
    #[serde(with = "serde_bytes_base64")]
    pub payload: Vec<u8>,
    /// 64-byte Ed25519 signature over the canonical signed bytes.
    #[serde(with = "serde_bytes_base64")]
    pub sig: Vec<u8>,
    /// Monotonic nonce — prevents replay; used for ordering.
    pub nonce: u64,
}

impl Envelope {
    /// Build and sign a new envelope.
    ///
    /// The `nonce` should be unique per sender session; callers typically
    /// maintain a `u64` counter starting at 0.
    #[must_use]
    pub fn new(identity: &Identity, to: &PeerId, payload: Vec<u8>, nonce: u64) -> Self {
        let from = identity.peer_id.to_string();
        let to_str = to.to_string();
        let bytes = canonical_bytes(&from, &to_str, nonce, &payload);
        let sig = identity.sign(&bytes);

        Self {
            from,
            to: to_str,
            payload,
            sig: sig.to_bytes().to_vec(),
            nonce,
        }
    }

    /// Verify the envelope's signature against the provided verifying key.
    ///
    /// Returns `Ok(())` on success.
    pub fn verify(&self, vk: &ed25519_dalek::VerifyingKey) -> Result<(), SignatureError> {
        let bytes = canonical_bytes(&self.from, &self.to, self.nonce, &self.payload);
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
// Canonical signed bytes
// ---------------------------------------------------------------------------

/// Produce the byte string that is actually signed / verified.
///
/// Layout: `from_utf8 || NUL || to_utf8 || NUL || nonce_le8 || payload`
fn canonical_bytes(from: &str, to: &str, nonce: u64, payload: &[u8]) -> Vec<u8> {
    let mut buf =
        Vec::with_capacity(from.len() + 1 + to.len() + 1 + 8 + payload.len());
    buf.extend_from_slice(from.as_bytes());
    buf.push(0);
    buf.extend_from_slice(to.as_bytes());
    buf.push(0);
    buf.extend_from_slice(&nonce.to_le_bytes());
    buf.extend_from_slice(payload);
    buf
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

    fn alice() -> Identity {
        Identity::generate_ephemeral()
    }

    fn bob() -> Identity {
        Identity::generate_ephemeral()
    }

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
}
