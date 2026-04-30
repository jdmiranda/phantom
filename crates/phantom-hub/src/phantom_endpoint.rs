//! Phantom-facing endpoints.
//!
//! - `POST /auth/register` — challenge–response registration; issues a JWT.
//! - `GET  /phantom/connect` — WSS dial-in with JWT validation (stub, #396).
//!
//! # Registration flow
//!
//! ```text
//! 1. Phantom generates a random nonce locally (v1 simplification; issue #399
//!    may add a server-side challenge round-trip).
//! 2. Phantom signs (nonce_bytes || peer_id_bytes) with its Ed25519 key.
//! 3. Phantom POSTs { peer_id, public_key_hex, nonce_hex, signature_hex }.
//! 4. Hub verifies the signature.
//! 5. Hub calls NonceCache::try_claim — returns 409 Conflict if nonce was
//!    already used (replay protection, issue #398).
//! 6. Hub issues a JWT bound to peer_id.
//! 7. Phantom stores the JWT via `phantom_net::DeviceCredentials`.
//! ```
//!
//! # WSS connect
//!
//! `GET /phantom/connect` is stubbed (returns 501) until issue #396 adds the
//! full WebSocket upgrade logic.  Auth enforcement is live — the handler
//! extracts and validates the JWT so that #396 only needs to add the upgrade.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::AppState;
use crate::auth::{self, AuthError};

// ---------------------------------------------------------------------------
// POST /auth/register
// ---------------------------------------------------------------------------

/// Request body for `POST /auth/register`.
#[derive(Debug, Serialize, Deserialize)]
pub struct RegisterRequest {
    /// The Phantom's stable peer_id (base58 SHA-256 of public key).
    pub peer_id: String,
    /// The Phantom's Ed25519 public key, hex-encoded (64 hex chars = 32 bytes).
    pub public_key_hex: String,
    /// Nonce hex-encoded by the client.
    ///
    /// v1: the client generates the nonce itself and includes it in the
    /// request.  The hub verifies the signature over (nonce || peer_id) which
    /// proves key ownership, then records the nonce in [`auth::NonceCache`] to
    /// prevent replay attacks.
    pub nonce_hex: String,
    /// Ed25519 signature over `(nonce_bytes || peer_id_bytes)`, hex-encoded
    /// (128 hex chars = 64 bytes).
    pub signature_hex: String,
}

/// Response body for a successful `POST /auth/register`.
#[derive(Debug, Serialize, Deserialize)]
pub struct RegisterResponse {
    /// The signed JWT.  Do not log this value.
    pub device_token: String,
    /// Token expiry as a Unix timestamp (seconds).
    pub exp: u64,
    /// The `peer_id` echoed back for the caller's convenience.
    pub phantom_id: String,
}

/// Handler for `POST /auth/register`.
///
/// Verifies the Ed25519 registration signature, enforces nonce single-use via
/// [`auth::NonceCache`], and issues a JWT device token.
///
/// Returns `409 Conflict` when the nonce has already been claimed — this is the
/// replay-rejection path.
pub async fn register(
    State(state): State<AppState>,
    Json(body): Json<RegisterRequest>,
) -> impl IntoResponse {
    // Decode the hex fields.
    let pubkey_bytes = match hex_decode_exact::<32>(&body.public_key_hex) {
        Ok(b) => b,
        Err(()) => {
            warn!(phantom_id = %body.peer_id, "register: invalid public_key_hex");
            return (
                StatusCode::BAD_REQUEST,
                "public_key_hex must be 64 hex chars (32 bytes)",
            )
                .into_response();
        }
    };

    let nonce_bytes_raw = match hex_decode_vec(&body.nonce_hex) {
        Ok(b) => b,
        Err(()) => {
            warn!(phantom_id = %body.peer_id, "register: invalid nonce_hex");
            return (StatusCode::BAD_REQUEST, "nonce_hex is not valid hex").into_response();
        }
    };
    let nonce_str = match String::from_utf8(nonce_bytes_raw) {
        Ok(s) => s,
        Err(_) => {
            warn!(phantom_id = %body.peer_id, "register: nonce_hex does not decode to UTF-8");
            return (StatusCode::BAD_REQUEST, "nonce must be a UTF-8 string").into_response();
        }
    };

    let sig_bytes = match hex_decode_exact::<64>(&body.signature_hex) {
        Ok(b) => b,
        Err(()) => {
            warn!(phantom_id = %body.peer_id, "register: invalid signature_hex");
            return (
                StatusCode::BAD_REQUEST,
                "signature_hex must be 128 hex chars (64 bytes)",
            )
                .into_response();
        }
    };

    // Verify the registration signature BEFORE claiming the nonce so that an
    // attacker cannot burn nonces without possessing a valid signing key.
    if let Err(e) = auth::verify_registration_signature(
        &body.peer_id,
        &nonce_str,
        &pubkey_bytes,
        &sig_bytes,
    ) {
        warn!(phantom_id = %body.peer_id, "register: {e} — auth_failure");
        return (StatusCode::UNAUTHORIZED, "signature verification failed").into_response();
    }

    // Claim the nonce.  try_claim is atomic (single Mutex acquisition —
    // check + insert happen together with no window between them).
    // Returns false when the nonce was already used within the TTL window.
    if !state.nonce_cache.try_claim(&nonce_str) {
        warn!(phantom_id = %body.peer_id, "register: nonce already used — replay_rejected");
        return (
            StatusCode::CONFLICT,
            "nonce already used — registration request must not be replayed",
        )
            .into_response();
    }

    // Issue the JWT.
    let token = match state.jwt.issue(&body.peer_id) {
        Ok(t) => t,
        Err(e) => {
            warn!(phantom_id = %body.peer_id, "register: JWT issuance failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to issue device token",
            )
                .into_response();
        }
    };

    // Decode just to read the exp claim for the response.
    let exp = state.jwt.verify(&token).map(|c| c.exp).unwrap_or(0);

    info!(phantom_id = %body.peer_id, "register: device token issued");
    Json(RegisterResponse {
        device_token: token,
        exp,
        phantom_id: body.peer_id,
    })
    .into_response()
}

// ---------------------------------------------------------------------------
// GET /phantom/connect
// ---------------------------------------------------------------------------

/// Handler for `GET /phantom/connect`.
///
/// Validates the device JWT from `Authorization: Bearer <jwt>`.  Returns 501
/// until issue #396 adds the full WebSocket upgrade logic.
pub async fn connect(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> impl IntoResponse {
    let token = match auth::extract_bearer(&headers) {
        Some(t) => t,
        None => {
            warn!("phantom/connect: missing Authorization header — auth_failure");
            return (
                StatusCode::UNAUTHORIZED,
                "Authorization: Bearer <device_token> required",
            )
                .into_response();
        }
    };

    match state.jwt.verify(&token) {
        Ok(claims) => {
            info!(
                phantom_id = %claims.sub,
                "phantom/connect: JWT valid — WSS upgrade pending (#396)"
            );
        }
        Err(AuthError::Expired) => {
            warn!("phantom/connect: expired JWT — auth_failure");
            return (StatusCode::UNAUTHORIZED, "device token expired").into_response();
        }
        Err(_) => {
            warn!("phantom/connect: invalid JWT — auth_failure");
            return (StatusCode::UNAUTHORIZED, "invalid device token").into_response();
        }
    }

    (
        StatusCode::NOT_IMPLEMENTED,
        "phantom/connect: WSS upgrade not yet implemented (issue #396)",
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Hex decode helpers
// ---------------------------------------------------------------------------

fn hex_decode_exact<const N: usize>(s: &str) -> Result<[u8; N], ()> {
    if s.len() != N * 2 {
        return Err(());
    }
    let mut out = [0u8; N];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = from_hex_digit(chunk[0]).ok_or(())?;
        let lo = from_hex_digit(chunk[1]).ok_or(())?;
        out[i] = (hi << 4) | lo;
    }
    Ok(out)
}

fn hex_decode_vec(s: &str) -> Result<Vec<u8>, ()> {
    if !s.len().is_multiple_of(2) {
        return Err(());
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in s.as_bytes().chunks(2) {
        let hi = from_hex_digit(chunk[0]).ok_or(())?;
        let lo = from_hex_digit(chunk[1]).ok_or(())?;
        out.push((hi << 4) | lo);
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{ApiKeyStore, JwtAuthority, NonceCache};
    use axum::body::Body;
    use axum::http::{Method, Request};
    use std::sync::Arc;
    use tower::ServiceExt;

    const TEST_SECRET: &[u8] = b"phantom-hub-test-secret-for-endpoint-tests";

    fn test_state() -> AppState {
        AppState {
            jwt: Arc::new(JwtAuthority::from_secret(TEST_SECRET)),
            api_keys: Arc::new(ApiKeyStore::default()),
            nonce_cache: Arc::new(NonceCache::new()),
        }
    }

    /// Build a valid RegisterRequest for `peer_id` using a freshly generated
    /// Ed25519 keypair and the provided `nonce`.
    fn make_register_body_with_nonce(peer_id: &str, nonce: &str) -> RegisterRequest {
        use ed25519_dalek::{Signer, SigningKey};
        use rand::rngs::OsRng;

        let signing_key = SigningKey::generate(&mut OsRng);

        let mut msg = Vec::new();
        msg.extend_from_slice(nonce.as_bytes());
        msg.extend_from_slice(peer_id.as_bytes());
        let sig = signing_key.sign(&msg);

        let pubkey_hex: String = signing_key
            .verifying_key()
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let nonce_hex: String = nonce.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
        let sig_hex: String = sig.to_bytes().iter().map(|b| format!("{b:02x}")).collect();

        RegisterRequest {
            peer_id: peer_id.to_owned(),
            public_key_hex: pubkey_hex,
            nonce_hex,
            signature_hex: sig_hex,
        }
    }

    fn make_register_body(peer_id: &str) -> RegisterRequest {
        make_register_body_with_nonce(peer_id, "test-nonce-12345")
    }

    // -----------------------------------------------------------------------
    // POST /auth/register — valid signature → 200 + JWT
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn register_valid_signature_returns_jwt() {
        let state = test_state();
        let app = crate::build_router(state);
        let body = make_register_body("test-peer-valid");

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/auth/register")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let r: RegisterResponse = serde_json::from_slice(&bytes).unwrap();
        assert!(!r.device_token.is_empty());
        assert_eq!(r.phantom_id, "test-peer-valid");
        assert!(r.exp > 0);
    }

    // -----------------------------------------------------------------------
    // POST /auth/register — tampered signature → 401
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn register_tampered_signature_returns_401() {
        let state = test_state();
        let app = crate::build_router(state);
        let mut body = make_register_body("test-peer-tampered");
        body.signature_hex = "aa".repeat(64); // 64 bytes of garbage

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/auth/register")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // -----------------------------------------------------------------------
    // P0 regression: replayed nonce returns 409 Conflict
    //
    // Both requests share the same AppState (same NonceCache).  The first
    // succeeds (200); the second is rejected (409) because the nonce was
    // already claimed.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn register_replayed_nonce_returns_409() {
        let state = test_state();
        // Use separate apps but the same state so both share the NonceCache.
        let body = make_register_body_with_nonce("replay-peer", "replay-nonce-xyz");

        let first_resp = crate::build_router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/auth/register")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Replay the identical request (same nonce, same signature).
        let second_resp = crate::build_router(state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/auth/register")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            first_resp.status(),
            StatusCode::OK,
            "first registration must succeed"
        );
        assert_eq!(
            second_resp.status(),
            StatusCode::CONFLICT,
            "replayed nonce must return 409 Conflict"
        );
    }

    // -----------------------------------------------------------------------
    // P0 regression: two distinct nonces both succeed
    //
    // Two different valid nonces signed by two different keypairs must both
    // receive 200 — the cache must not block legitimate concurrent registrations.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn register_distinct_nonces_both_succeed() {
        let state = test_state();

        let body_a = make_register_body_with_nonce("peer-alpha", "nonce-alpha-unique-001");
        let body_b = make_register_body_with_nonce("peer-beta", "nonce-beta-unique-002");

        let resp_a = crate::build_router(state.clone())
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/auth/register")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body_a).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        let resp_b = crate::build_router(state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/auth/register")
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&body_b).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            resp_a.status(),
            StatusCode::OK,
            "first distinct nonce registration must succeed"
        );
        assert_eq!(
            resp_b.status(),
            StatusCode::OK,
            "second distinct nonce registration must also succeed"
        );
    }

    // -----------------------------------------------------------------------
    // P0 regression: LRU eviction — oldest entry is evicted when cache is full
    //
    // Uses NonceCache::with_capacity_and_ttl directly (unit-level) to verify
    // that capacity-driven eviction makes the oldest nonce re-claimable while
    // more-recently-used entries remain blocked.
    //
    // Note: This test operates on NonceCache directly (not via HTTP) because
    // driving LRU eviction via the register handler would require
    // NONCE_CACHE_CAPACITY (10 000) round-trips through axum.  The handler
    // integration path is covered by register_replayed_nonce_returns_409.
    //
    // Two-stage design: re-inserting the evicted nonce in the same cache would
    // cascade-evict the next-oldest entry.  Stage 1 verifies the remaining
    // entries are blocked; Stage 2 (fresh cache) verifies the evicted entry is
    // re-claimable.
    // -----------------------------------------------------------------------

    #[test]
    fn nonce_cache_eviction_lru_oldest() {
        use std::time::Duration;
        // ---- Stage 1: middle entries blocked after one eviction ----
        let cache = NonceCache::with_capacity_and_ttl(4, Duration::from_secs(3600));

        // Fill: A(LRU) → B → C → D(MRU).
        assert!(cache.try_claim("evict-nonce-A"), "A: initial claim (1/4)");
        assert!(cache.try_claim("evict-nonce-B"), "B: initial claim (2/4)");
        assert!(cache.try_claim("evict-nonce-C"), "C: initial claim (3/4)");
        assert!(cache.try_claim("evict-nonce-D"), "D: initial claim (4/4)");

        // E evicts A (the LRU).  Cache: B(LRU), C, D, E(MRU).
        assert!(cache.try_claim("evict-nonce-E"), "E: claim evicts A");

        // B, C, D, E still present — must be replay-rejected.
        assert!(!cache.try_claim("evict-nonce-B"), "B still cached — replay");
        assert!(!cache.try_claim("evict-nonce-C"), "C still cached — replay");
        assert!(!cache.try_claim("evict-nonce-D"), "D still cached — replay");
        assert!(!cache.try_claim("evict-nonce-E"), "E still cached — replay");

        // ---- Stage 2: evicted entry is re-claimable (fresh cache) ----
        let cache2 = NonceCache::with_capacity_and_ttl(4, Duration::from_secs(3600));
        cache2.try_claim("evict-nonce-A");
        cache2.try_claim("evict-nonce-B");
        cache2.try_claim("evict-nonce-C");
        cache2.try_claim("evict-nonce-D");
        cache2.try_claim("evict-nonce-E"); // evicts A

        assert!(
            cache2.try_claim("evict-nonce-A"),
            "evicted nonce-A must be re-claimable after LRU eviction"
        );
    }

    // -----------------------------------------------------------------------
    // GET /phantom/connect — no JWT → 401
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn connect_without_jwt_returns_401() {
        let state = test_state();
        let app = crate::build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/phantom/connect")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // -----------------------------------------------------------------------
    // GET /phantom/connect — garbage JWT → 401
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn connect_invalid_jwt_returns_401() {
        let state = test_state();
        let app = crate::build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/phantom/connect")
                    .header("Authorization", "Bearer not.a.valid.jwt")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // -----------------------------------------------------------------------
    // GET /phantom/connect — valid JWT → 501 (WSS stub)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn connect_valid_jwt_returns_501_stub() {
        let state = test_state();
        let token = state.jwt.issue("stub-peer").unwrap();
        let app = crate::build_router(state);

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/phantom/connect")
                    .header("Authorization", format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }

    // -----------------------------------------------------------------------
    // hex helpers
    // -----------------------------------------------------------------------

    #[test]
    fn hex_decode_exact_correct_length() {
        let hex = "deadbeef".repeat(8); // 32 bytes = 64 hex chars
        assert!(hex_decode_exact::<32>(&hex).is_ok());
    }

    #[test]
    fn hex_decode_exact_wrong_length_errors() {
        assert!(hex_decode_exact::<32>("deadbeef").is_err());
    }
}
