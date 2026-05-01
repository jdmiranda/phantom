//! Ed25519 envelope signing and verification for relay-internal calls.
//!
//! Every envelope constructed by [`crate::agent_route::route_agent_envelope`]
//! is signed with the sender's Ed25519 [`SigningKey`] before it is handed to
//! the router. Recipients SHOULD verify the signature using the sender's known
//! [`VerifyingKey`] before acting on the payload.
//!
//! # Canonical signed bytes
//!
//! The bytes that are actually signed are:
//!
//! ```text
//! from_str || 0x00 || to_str || 0x00 || nonce_uuid_bytes(16, RFC 4122 big-endian) || payload_json_bytes
//! ```
//!
//! The 16 nonce bytes come from [`uuid::Uuid::as_bytes`], which returns the
//! UUID in RFC 4122 network (big-endian) byte order.
//!
//! Note: this layout is NOT wire-compatible with `phantom-net::envelope`,
//! which uses an 8-byte little-endian `u64` nonce rather than a 16-byte UUID.
//!
//! # Signature encoding
//!
//! Signatures are stored in the [`crate::envelope::Envelope::sig`] field as
//! 128 lowercase hex characters (64 bytes × 2 hex digits each).

use ed25519_dalek::{Signature, SignatureError, Signer, SigningKey, VerifyingKey};

use crate::envelope::Envelope;

// ---------------------------------------------------------------------------
// Canonical bytes
// ---------------------------------------------------------------------------

/// Produce the byte string that is signed and verified.
///
/// Layout: `from || NUL || to || NUL || nonce_be16 || payload_json`
/// where `nonce_be16` is the 16-byte RFC 4122 big-endian UUID returned by
/// [`uuid::Uuid::as_bytes`].
pub(crate) fn canonical_bytes(env: &Envelope) -> Vec<u8> {
    let payload_json = serde_json::to_vec(&env.payload).unwrap_or_default();
    let mut buf = Vec::with_capacity(
        env.from.0.len() + 1 + env.to.0.len() + 1 + 16 + payload_json.len(),
    );
    buf.extend_from_slice(env.from.0.as_bytes());
    buf.push(0);
    buf.extend_from_slice(env.to.0.as_bytes());
    buf.push(0);
    buf.extend_from_slice(env.nonce.as_bytes()); // 16 bytes, RFC 4122 big-endian UUID
    buf.extend_from_slice(&payload_json);
    buf
}

// ---------------------------------------------------------------------------
// Hex helpers (avoids pulling in the `hex` crate)
// ---------------------------------------------------------------------------

/// Encode 64 bytes as 128 lowercase hex characters.
fn encode_hex_128(bytes: &[u8; 64]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decode 128 hex characters back to 64 bytes.
///
/// Returns an error if the input is not exactly 128 hex characters.
fn decode_hex_128(s: &str) -> Result<[u8; 64], &'static str> {
    if s.len() != 128 {
        return Err("expected 128 hex chars for a 64-byte Ed25519 signature");
    }
    let mut out = [0u8; 64];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = from_hex_digit(chunk[0]).ok_or("invalid hex digit")?;
        let lo = from_hex_digit(chunk[1]).ok_or("invalid hex digit")?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

fn from_hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Sign `envelope` with `key` and store the hex-encoded signature in
/// `envelope.sig`.
///
/// Call this immediately before handing the envelope to the router so the
/// signature covers the finalised `from`, `to`, `nonce`, and `payload`.
pub fn sign_envelope(envelope: &mut Envelope, key: &SigningKey) {
    let bytes = canonical_bytes(envelope);
    let sig: Signature = key.sign(&bytes);
    envelope.sig = encode_hex_128(&sig.to_bytes());
}

/// Verify the Ed25519 signature in `envelope.sig` against `verifying_key`.
///
/// Returns `Ok(())` on success, or a [`SignatureError`] if the signature is
/// absent, malformed, or does not match the envelope's canonical bytes.
///
/// # Errors
///
/// - [`SignatureError`] if `envelope.sig` is not 128 hex chars, contains
///   invalid characters, or the cryptographic check fails.
pub fn verify_envelope(
    envelope: &Envelope,
    verifying_key: &VerifyingKey,
) -> Result<(), SignatureError> {
    let raw = decode_hex_128(&envelope.sig).map_err(|_| SignatureError::new())?;
    let sig = Signature::from_bytes(&raw);
    let bytes = canonical_bytes(envelope);
    use ed25519_dalek::Verifier;
    verifying_key.verify(&bytes, &sig)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use rand::rngs::OsRng;
    use uuid::Uuid;

    use super::*;
    use crate::envelope::{Envelope, PeerId};

    /// Generate a fresh throwaway signing key for tests.
    pub(crate) fn make_signing_key() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    fn make_envelope(from: &str, to: &str) -> Envelope {
        Envelope {
            from: PeerId(from.into()),
            to: PeerId(to.into()),
            payload: serde_json::json!({"msg": "hello"}),
            sig: String::new(),
            nonce: Uuid::new_v4(),
        }
    }

    #[test]
    fn sign_then_verify_succeeds() {
        let key = make_signing_key();
        let vk = key.verifying_key();
        let mut env = make_envelope("alice", "bob");

        sign_envelope(&mut env, &key);
        assert!(!env.sig.is_empty(), "sig must be populated after signing");
        assert_eq!(env.sig.len(), 128, "sig must be 128 hex chars");

        assert!(
            verify_envelope(&env, &vk).is_ok(),
            "valid signature must verify"
        );
    }

    #[test]
    fn tampered_payload_fails_verification() {
        let key = make_signing_key();
        let vk = key.verifying_key();
        let mut env = make_envelope("alice", "bob");
        sign_envelope(&mut env, &key);

        // Tamper with the payload after signing.
        env.payload = serde_json::json!({"msg": "tampered"});

        assert!(
            verify_envelope(&env, &vk).is_err(),
            "tampered payload must invalidate signature"
        );
    }

    #[test]
    fn tampered_from_field_fails_verification() {
        let key = make_signing_key();
        let vk = key.verifying_key();
        let mut env = make_envelope("alice", "bob");
        sign_envelope(&mut env, &key);

        // Tamper with the sender field.
        env.from = PeerId("mallory".into());

        assert!(
            verify_envelope(&env, &vk).is_err(),
            "tampered from field must invalidate signature"
        );
    }

    #[test]
    fn wrong_key_fails_verification() {
        let alice_key = make_signing_key();
        let bob_key = make_signing_key();
        let mut env = make_envelope("alice", "bob");
        sign_envelope(&mut env, &alice_key);

        // Verify with the wrong key.
        assert!(
            verify_envelope(&env, &bob_key.verifying_key()).is_err(),
            "wrong key must not verify signature"
        );
    }

    #[test]
    fn empty_sig_fails_verification() {
        let key = make_signing_key();
        let env = make_envelope("alice", "bob");
        // sig is still empty — never signed.
        assert!(
            verify_envelope(&env, &key.verifying_key()).is_err(),
            "empty sig must fail verification"
        );
    }

    #[test]
    fn tampered_sig_fails_verification() {
        let key = make_signing_key();
        let vk = key.verifying_key();
        let mut env = make_envelope("alice", "bob");
        sign_envelope(&mut env, &key);

        // Flip the first byte of the hex sig.
        let mut sig_chars: Vec<char> = env.sig.chars().collect();
        sig_chars[0] = if sig_chars[0] == '0' { '1' } else { '0' };
        env.sig = sig_chars.into_iter().collect();

        assert!(
            verify_envelope(&env, &vk).is_err(),
            "corrupted sig bytes must fail verification"
        );
    }

    #[test]
    fn canonical_bytes_differ_with_different_nonce() {
        let mut env1 = make_envelope("alice", "bob");
        env1.nonce = Uuid::new_v4();
        let mut env2 = env1.clone();
        env2.nonce = Uuid::new_v4();

        // Different nonces must produce different canonical byte strings to
        // prevent replay attacks.
        assert_ne!(
            canonical_bytes(&env1),
            canonical_bytes(&env2),
            "different nonces must produce different canonical bytes"
        );
    }
}
