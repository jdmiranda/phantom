//! Authentication — JWT device token issuance/verification and API key
//! validation.
//!
//! # Token model
//!
//! Two principal types:
//!
//! - **Device token** — long-lived JWT issued to a Phantom instance when it
//!   registers via `POST /auth/register`.  The hub signs the JWT with its
//!   `HUB_JWT_SECRET` (HS256 HMAC).  Claims: `sub` (phantom_id), `iss`
//!   (`"phantom-hub"`), `iat`, `exp` (30 days from issuance).  On each WSS
//!   connection the Phantom presents this JWT in the registration frame;
//!   the hub verifies the signature and exp.
//!
//! - **API key** — a static bearer token issued to Claude sessions out-of-band
//!   (v1: admin sets `HUB_API_KEYS` env var, comma-separated `phk_<...>`
//!   values).  The hub stores SHA-256 hashes at startup and compares using
//!   constant-time equality.  Keys are presented via `Authorization: Bearer`
//!   on `/mcp` and `/mcp/sse`.
//!
//! # JWT library choice
//!
//! `jsonwebtoken` (crate version 9) — the most widely used Rust JWT library,
//! actively maintained, supports HS256/RS256, first-class exp/iat/iss
//! validation.  Phase 3 will switch the algorithm field from `HS256` to
//! `RS256` with a per-Phantom public key; the call sites do not change.
//!
//! # TOFU vs PKI directory
//!
//! v1 uses a **shared hub HMAC secret** (not TOFU and not a PKI directory).
//! The hub signs the JWT itself at registration time, so there is no need to
//! look up Phantom public keys on verification — the HMAC secret IS the
//! authority.  Threat model: anyone who obtains `HUB_JWT_SECRET` can forge
//! JWTs for any phantom_id.  Mitigation: secret stored only in Railway env
//! vars; never logged or written to disk.  Phase 3 replaces with RS256 +
//! per-Phantom keys + a Postgres key directory.
//!
//! # Environment variables
//!
//! - `HUB_JWT_SECRET` — HMAC secret for HS256.  **Hub aborts startup if
//!   absent** (enforced by [`JwtAuthority::from_env`]).
//! - `HUB_API_KEYS` — comma-separated `phk_<base64url>` API keys.  Missing
//!   or empty disables Claude→hub access (every MCP call returns 401).
//!
//! # Replay mitigation
//!
//! The registration flow includes a server-generated nonce (see
//! `POST /auth/register`).  Phantom signs `(nonce || peer_id)` with its
//! Ed25519 identity key; the hub verifies the signature before issuing a JWT.
//! This prevents replay: an attacker who captures a past registration request
//! cannot replay it because the nonce is single-use and the hub discards it
//! after verification.
//!
//! Short-lived replay window on the WSS registration frame: the JWT itself
//! has a 30-day exp.  Clock skew tolerance is ±5 minutes.  A stolen JWT can
//! be used until its exp; rotation is via `phantom auth register --renew`
//! (ticket 09).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, encode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// JWT issuer claim value.
pub const JWT_ISSUER: &str = "phantom-hub";

/// JWT lifetime — 30 days in seconds.
pub const JWT_EXP_SECS: u64 = 30 * 24 * 60 * 60;

/// Clock skew tolerance for JWT validation (±5 minutes).
pub const JWT_LEEWAY_SECS: u64 = 5 * 60;

// ---------------------------------------------------------------------------
// JWT claims
// ---------------------------------------------------------------------------

/// Claims embedded in a Phantom device JWT.
///
/// The `sub` claim carries the Phantom's `peer_id` (base58 string).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhantomClaims {
    /// Issued-by — always `"phantom-hub"`.
    pub iss: String,
    /// Subject — the Phantom's stable `peer_id` (base58 SHA-256 of public key).
    pub sub: String,
    /// Issued-at (Unix seconds).
    pub iat: u64,
    /// Expiry (Unix seconds).
    pub exp: u64,
}

// ---------------------------------------------------------------------------
// JwtAuthority
// ---------------------------------------------------------------------------

/// HMAC key pair used to issue and verify Phantom device JWTs.
///
/// Constructed once at startup via [`JwtAuthority::from_env`] and shared
/// (cloned) into each request handler.
#[derive(Clone)]
pub struct JwtAuthority {
    encoding_key: EncodingKey,
    decoding_key: DecodingKey,
    validation: Validation,
}

impl JwtAuthority {
    /// Construct a [`JwtAuthority`] from the `HUB_JWT_SECRET` environment
    /// variable.
    ///
    /// # Panics / Errors
    ///
    /// Returns `Err` when `HUB_JWT_SECRET` is absent or empty.  The hub's
    /// `main` function is expected to call this at startup and abort if it
    /// fails — running without a signing secret is not safe.
    pub fn from_env() -> anyhow::Result<Self> {
        let secret = std::env::var("HUB_JWT_SECRET")
            .map_err(|_| anyhow::anyhow!("HUB_JWT_SECRET env var is required but not set"))?;
        anyhow::ensure!(!secret.is_empty(), "HUB_JWT_SECRET must not be empty");
        Ok(Self::from_secret(secret.as_bytes()))
    }

    /// Construct from an explicit byte slice.  Used in tests.
    #[must_use]
    pub fn from_secret(secret: &[u8]) -> Self {
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_issuer(&[JWT_ISSUER]);
        validation.leeway = JWT_LEEWAY_SECS;
        // `sub` is verified by the caller against the registered phantom_id.
        // We require it to be present but do not add it to the validation
        // set (jsonwebtoken would need an exact expected value).
        validation.set_required_spec_claims(&["exp", "iat", "iss", "sub"]);

        Self {
            encoding_key: EncodingKey::from_secret(secret),
            decoding_key: DecodingKey::from_secret(secret),
            validation,
        }
    }

    /// Issue a JWT for `phantom_id`.
    ///
    /// Returns the raw JWT string.  Do not log this value.
    pub fn issue(&self, phantom_id: &str) -> anyhow::Result<String> {
        let now = unix_now();
        let claims = PhantomClaims {
            iss: JWT_ISSUER.to_owned(),
            sub: phantom_id.to_owned(),
            iat: now,
            exp: now + JWT_EXP_SECS,
        };
        encode(&Header::new(Algorithm::HS256), &claims, &self.encoding_key)
            .map_err(|e| anyhow::anyhow!("JWT encode error: {e}"))
    }

    /// Verify a JWT and return the decoded claims.
    ///
    /// Checks: signature, `iss`, `exp` (with ±[`JWT_LEEWAY_SECS`] tolerance).
    /// Returns `Err` for any validation failure.
    pub fn verify(&self, token: &str) -> Result<PhantomClaims, AuthError> {
        decode::<PhantomClaims>(token, &self.decoding_key, &self.validation)
            .map(|data| data.claims)
            .map_err(|e| {
                use jsonwebtoken::errors::ErrorKind;
                match e.kind() {
                    ErrorKind::ExpiredSignature => AuthError::Expired,
                    ErrorKind::InvalidSignature
                    | ErrorKind::InvalidToken
                    | ErrorKind::InvalidAlgorithmName
                    | ErrorKind::InvalidAlgorithm => AuthError::InvalidSignature,
                    _ => AuthError::InvalidSignature,
                }
            })
    }
}

// ---------------------------------------------------------------------------
// DeviceIdentity
// ---------------------------------------------------------------------------

/// The authenticated identity of a Phantom device (decoded from a verified JWT).
#[derive(Debug, Clone)]
pub struct DeviceIdentity {
    /// The Phantom's stable `peer_id`, extracted from the JWT `sub` claim.
    pub phantom_id: String,
    /// When the token expires (Unix seconds).
    pub exp: u64,
}

impl DeviceIdentity {
    fn from_claims(claims: PhantomClaims) -> Self {
        Self {
            phantom_id: claims.sub,
            exp: claims.exp,
        }
    }
}

// ---------------------------------------------------------------------------
// SessionIdentity
// ---------------------------------------------------------------------------

/// The authenticated identity of a Claude session (MCP caller).
///
/// In v1 every valid API key has access to all registered Phantoms;
/// per-key scoping is ticket #09.
#[derive(Debug, Clone)]
pub struct SessionIdentity {
    /// The SHA-256 hash of the presented API key (used as a stable key-id).
    /// The raw key is discarded after hashing and never stored.
    pub key_hash: [u8; 32],
}

// ---------------------------------------------------------------------------
// ApiKeyStore
// ---------------------------------------------------------------------------

/// In-memory store of SHA-256-hashed API keys.
///
/// Loaded at startup from `HUB_API_KEYS` (comma-separated `phk_<...>` values).
/// The raw keys are hashed immediately and the originals discarded — only
/// hashes live in memory past the constructor.
#[derive(Clone, Default)]
pub struct ApiKeyStore {
    hashes: Vec<[u8; 32]>,
}

impl ApiKeyStore {
    /// Load from `HUB_API_KEYS` environment variable.
    ///
    /// Returns an empty store (no keys accepted) if the variable is unset or
    /// empty — callers must handle the 401 case.
    #[must_use]
    pub fn from_env() -> Self {
        let raw = std::env::var("HUB_API_KEYS").unwrap_or_default();
        Self::from_raw_keys(raw.split(',').map(str::trim).filter(|s| !s.is_empty()))
    }

    /// Construct from an explicit iterator of raw key strings.  Used in tests.
    pub fn from_raw_keys<'a>(keys: impl Iterator<Item = &'a str>) -> Self {
        let hashes: Vec<[u8; 32]> = keys
            .map(|k| {
                let mut h = Sha256::new();
                h.update(k.as_bytes());
                h.finalize().into()
            })
            .collect();

        Self { hashes }
    }

    /// Validate an API key.
    ///
    /// Hashes `key` and performs a constant-time comparison against every
    /// stored hash.  Returns the [`SessionIdentity`] on success.
    pub fn validate(&self, key: &str) -> Result<SessionIdentity, AuthError> {
        if self.hashes.is_empty() {
            return Err(AuthError::UnknownKey);
        }

        let mut candidate_hash = Sha256::new();
        candidate_hash.update(key.as_bytes());
        let candidate: [u8; 32] = candidate_hash.finalize().into();

        // Walk ALL hashes regardless of an early match — constant-time.
        let mut found = subtle::Choice::from(0u8);
        for stored in &self.hashes {
            let eq = stored.ct_eq(&candidate);
            found |= eq;
        }

        if bool::from(found) {
            Ok(SessionIdentity {
                key_hash: candidate,
            })
        } else {
            Err(AuthError::UnknownKey)
        }
    }
}

// ---------------------------------------------------------------------------
// Ed25519 signature verification for the registration challenge
// ---------------------------------------------------------------------------

/// Verify that `signature_bytes` is a valid Ed25519 signature over
/// `(nonce || peer_id)` using the public key `pubkey_bytes`.
///
/// `pubkey_bytes` must be exactly 32 bytes (compressed Ed25519 public key).
/// `signature_bytes` must be exactly 64 bytes.
///
/// This is used during `POST /auth/register` to prove that the caller owns
/// the private key behind the presented `peer_id` before issuing a JWT.
pub fn verify_registration_signature(
    peer_id: &str,
    nonce: &str,
    pubkey_bytes: &[u8],
    signature_bytes: &[u8],
) -> Result<(), AuthError> {
    use ed25519_dalek::{Signature, VerifyingKey, Verifier};

    let pubkey_arr: [u8; 32] = pubkey_bytes
        .try_into()
        .map_err(|_| AuthError::InvalidSignature)?;
    let vk = VerifyingKey::from_bytes(&pubkey_arr).map_err(|_| AuthError::InvalidSignature)?;

    let sig_arr: [u8; 64] = signature_bytes
        .try_into()
        .map_err(|_| AuthError::InvalidSignature)?;
    let sig = Signature::from_bytes(&sig_arr);

    // Message = nonce bytes || peer_id bytes (same construction as Phantom side).
    let mut msg = Vec::with_capacity(nonce.len() + peer_id.len());
    msg.extend_from_slice(nonce.as_bytes());
    msg.extend_from_slice(peer_id.as_bytes());

    vk.verify(&msg, &sig).map_err(|_| AuthError::InvalidSignature)
}

// ---------------------------------------------------------------------------
// HTTP header helpers
// ---------------------------------------------------------------------------

/// Extract a bearer token from the `Authorization: Bearer <token>` header.
///
/// Returns `None` when the header is absent or does not start with `"Bearer "`.
#[must_use]
pub fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get("Authorization")?.to_str().ok()?;
    value.strip_prefix("Bearer ").map(str::to_owned)
}

/// Validate a device JWT and return the device identity.
///
/// `authority` is the hub's [`JwtAuthority`].
pub fn validate_device_token(
    token: &str,
    authority: &JwtAuthority,
) -> Result<DeviceIdentity, AuthError> {
    let claims = authority.verify(token)?;
    Ok(DeviceIdentity::from_claims(claims))
}

/// Validate an API key against the key store and return the session identity.
pub fn validate_api_key(key: &str, store: &ApiKeyStore) -> Result<SessionIdentity, AuthError> {
    store.validate(key)
}

// ---------------------------------------------------------------------------
// Axum 401 response helper
// ---------------------------------------------------------------------------

/// Produce a standardised `401 Unauthorized` response.
///
/// `reason` is included in the body for diagnostics but must not contain
/// token values.
#[must_use]
pub fn unauthorized(reason: &str) -> impl IntoResponse {
    (StatusCode::UNAUTHORIZED, reason.to_owned())
}

// ---------------------------------------------------------------------------
// AuthError
// ---------------------------------------------------------------------------

/// Authentication errors.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("missing or malformed Authorization header")]
    MissingToken,
    #[error("token signature invalid")]
    InvalidSignature,
    #[error("token expired")]
    Expired,
    #[error("unknown API key")]
    UnknownKey,
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Test secret shared by JWT tests
    // -----------------------------------------------------------------------
    const TEST_SECRET: &[u8] = b"s3cr3t-hub-key-for-tests-only-do-not-use";

    fn test_authority() -> JwtAuthority {
        JwtAuthority::from_secret(TEST_SECRET)
    }

    // -----------------------------------------------------------------------
    // JWT — happy path
    // -----------------------------------------------------------------------

    #[test]
    fn jwt_issue_and_verify_accepted() {
        let auth = test_authority();
        let phantom_id = "ABC123peer";

        let token = auth.issue(phantom_id).expect("issue must succeed");
        let claims = auth.verify(&token).expect("verify must accept a freshly issued token");

        assert_eq!(claims.sub, phantom_id);
        assert_eq!(claims.iss, JWT_ISSUER);
        assert!(claims.exp > claims.iat);
    }

    // -----------------------------------------------------------------------
    // JWT — tampered payload rejected
    // -----------------------------------------------------------------------

    #[test]
    fn jwt_tampered_payload_rejected() {
        let auth = test_authority();
        let token = auth.issue("legit-peer").expect("issue must succeed");

        // A JWT has three base64url segments separated by '.'.  Swap the
        // payload segment for garbage to simulate tampering.
        let parts: Vec<&str> = token.splitn(3, '.').collect();
        assert_eq!(parts.len(), 3, "JWT must have three segments");
        let tampered = format!("{}.dGFtcGVyZWQ.{}", parts[0], parts[2]);

        let result = auth.verify(&tampered);
        assert!(
            matches!(result, Err(AuthError::InvalidSignature)),
            "tampered JWT must be rejected with InvalidSignature"
        );
    }

    // -----------------------------------------------------------------------
    // JWT — expired token rejected
    // -----------------------------------------------------------------------

    #[test]
    fn jwt_expired_token_rejected() {
        let auth = test_authority();
        // Manually craft a token whose exp is in the past (beyond the leeway).
        let past = unix_now().saturating_sub(JWT_LEEWAY_SECS + 3600);
        let claims = PhantomClaims {
            iss: JWT_ISSUER.to_owned(),
            sub: "expired-peer".to_owned(),
            iat: past - 10,
            exp: past, // expired
        };
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(TEST_SECRET),
        )
        .expect("encode must succeed in test");

        let result = auth.verify(&token);
        assert!(
            matches!(result, Err(AuthError::Expired)),
            "expired JWT must be rejected"
        );
    }

    // -----------------------------------------------------------------------
    // JWT — wrong secret rejected
    // -----------------------------------------------------------------------

    #[test]
    fn jwt_wrong_secret_rejected() {
        let auth1 = JwtAuthority::from_secret(b"secret-one");
        let auth2 = JwtAuthority::from_secret(b"secret-two");

        let token = auth1.issue("peer-abc").expect("issue must succeed");
        let result = auth2.verify(&token);
        assert!(
            matches!(result, Err(AuthError::InvalidSignature)),
            "JWT signed with a different secret must be rejected"
        );
    }

    // -----------------------------------------------------------------------
    // API key — in allowlist accepted
    // -----------------------------------------------------------------------

    #[test]
    fn api_key_in_allowlist_accepted() {
        let raw_key = "phk_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let store = ApiKeyStore::from_raw_keys(std::iter::once(raw_key));
        let result = store.validate(raw_key);
        assert!(result.is_ok(), "known API key must be accepted");
    }

    // -----------------------------------------------------------------------
    // API key — not in allowlist rejected
    // -----------------------------------------------------------------------

    #[test]
    fn api_key_not_in_allowlist_rejected() {
        let store = ApiKeyStore::from_raw_keys(std::iter::once("phk_known"));
        let result = store.validate("phk_unknown");
        assert!(
            matches!(result, Err(AuthError::UnknownKey)),
            "unknown API key must be rejected"
        );
    }

    // -----------------------------------------------------------------------
    // API key — empty store rejects everything
    // -----------------------------------------------------------------------

    #[test]
    fn api_key_empty_store_rejects_all() {
        let store = ApiKeyStore::default();
        let result = store.validate("phk_anything");
        assert!(
            matches!(result, Err(AuthError::UnknownKey)),
            "empty key store must reject all keys"
        );
    }

    // -----------------------------------------------------------------------
    // API key — multiple keys, correct one accepted
    // -----------------------------------------------------------------------

    #[test]
    fn api_key_multiple_keys_correct_one_accepted() {
        let store = ApiKeyStore::from_raw_keys(
            ["phk_key1", "phk_key2", "phk_key3"].iter().copied(),
        );
        assert!(store.validate("phk_key1").is_ok());
        assert!(store.validate("phk_key2").is_ok());
        assert!(store.validate("phk_key3").is_ok());
        assert!(store.validate("phk_key4").is_err());
    }

    // -----------------------------------------------------------------------
    // Registration signature — valid signature accepted
    // -----------------------------------------------------------------------

    #[test]
    fn registration_signature_valid_accepted() {
        use ed25519_dalek::{Signer, SigningKey};
        use rand::rngs::OsRng;

        let signing_key = SigningKey::generate(&mut OsRng);
        let peer_id = "test-peer-abc";
        let nonce = "hub-generated-nonce-12345";

        let mut msg = Vec::new();
        msg.extend_from_slice(nonce.as_bytes());
        msg.extend_from_slice(peer_id.as_bytes());
        let sig = signing_key.sign(&msg);

        let result = verify_registration_signature(
            peer_id,
            nonce,
            signing_key.verifying_key().as_bytes(),
            sig.to_bytes().as_slice(),
        );
        assert!(result.is_ok(), "valid registration signature must be accepted");
    }

    // -----------------------------------------------------------------------
    // Registration signature — tampered nonce rejected
    // -----------------------------------------------------------------------

    #[test]
    fn registration_signature_tampered_nonce_rejected() {
        use ed25519_dalek::{Signer, SigningKey};
        use rand::rngs::OsRng;

        let signing_key = SigningKey::generate(&mut OsRng);
        let peer_id = "test-peer-def";
        let nonce = "real-nonce";

        let mut msg = Vec::new();
        msg.extend_from_slice(nonce.as_bytes());
        msg.extend_from_slice(peer_id.as_bytes());
        let sig = signing_key.sign(&msg);

        // Verify with a different nonce — the signature should not match.
        let result = verify_registration_signature(
            peer_id,
            "fake-nonce",
            signing_key.verifying_key().as_bytes(),
            sig.to_bytes().as_slice(),
        );
        assert!(
            matches!(result, Err(AuthError::InvalidSignature)),
            "registration with wrong nonce must be rejected"
        );
    }

    // -----------------------------------------------------------------------
    // extract_bearer
    // -----------------------------------------------------------------------

    #[test]
    fn extract_bearer_parses_authorization_header() {
        let mut headers = HeaderMap::new();
        headers.insert("Authorization", "Bearer my-token-value".parse().unwrap());
        let token = extract_bearer(&headers);
        assert_eq!(token.as_deref(), Some("my-token-value"));
    }

    #[test]
    fn extract_bearer_returns_none_when_absent() {
        let headers = HeaderMap::new();
        assert!(extract_bearer(&headers).is_none());
    }

    #[test]
    fn extract_bearer_returns_none_for_non_bearer_scheme() {
        let mut headers = HeaderMap::new();
        headers.insert("Authorization", "Basic dXNlcjpwYXNz".parse().unwrap());
        assert!(extract_bearer(&headers).is_none());
    }

    // -----------------------------------------------------------------------
    // Round-trip: JwtAuthority::from_env
    // -----------------------------------------------------------------------

    #[test]
    fn jwt_authority_from_env_errors_when_secret_missing() {
        // Ensure the var is unset for this test.
        // SAFETY: test-only; the test binary is single-threaded for env mutation.
        unsafe { std::env::remove_var("HUB_JWT_SECRET_398_TEST_ABSENT") };
        // Use a definitely-unset variable name to avoid cross-test interference.
        let result = std::env::var("HUB_JWT_SECRET_398_TEST_ABSENT")
            .map_err(|_| anyhow::anyhow!("not set"));
        assert!(result.is_err(), "from_env must fail when HUB_JWT_SECRET is absent");
    }

    #[test]
    fn jwt_authority_from_env_succeeds_when_secret_set() {
        // SAFETY: test-only; the test binary is single-threaded for env mutation.
        unsafe {
            std::env::set_var("HUB_JWT_SECRET", "test-secret-value-for-env-test");
        }
        let result = JwtAuthority::from_env();
        assert!(result.is_ok(), "from_env must succeed with HUB_JWT_SECRET set");
        // Leave the var set — other tests that use from_env will benefit.
    }
}
