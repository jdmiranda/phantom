//! `POST /mcp` and `GET /mcp/sse` — Claude-side MCP transport.
//!
//! # Auth (issue #398)
//!
//! Both endpoints require `Authorization: Bearer <api-key>` where the key is
//! a `phk_<base64url>` value loaded from `HUB_API_KEYS` at startup.  The hub
//! stores SHA-256 hashes; comparison is constant-time.  An absent or invalid
//! key returns 401 immediately.
//!
//! # Routing (issue #396)
//!
//! After auth passes, both handlers currently return 501.  Issue #396 fills in
//! the JSON-RPC frame routing; issue #397 adds the SSE streaming transport.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use tracing::warn;

use crate::AppState;
use crate::auth;

// ---------------------------------------------------------------------------
// POST /mcp
// ---------------------------------------------------------------------------

/// Handler for `POST /mcp` — Claude-side JSON-RPC 2.0 endpoint.
///
/// Validates the API key from `Authorization: Bearer <key>`.
/// Returns 401 on missing/invalid key, 501 until issue #396 adds routing.
pub async fn handle_jsonrpc(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(resp) = require_api_key(&state, &headers, "POST /mcp") {
        return *resp;
    }

    (
        StatusCode::NOT_IMPLEMENTED,
        "mcp: routing not yet implemented (issue #396)",
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// GET /mcp/sse
// ---------------------------------------------------------------------------

/// Handler for `GET /mcp/sse` — Claude-side SSE transport.
///
/// Validates the API key from `Authorization: Bearer <key>`.
/// Returns 401 on missing/invalid key, 501 until issue #397 adds SSE.
pub async fn handle_sse(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(resp) = require_api_key(&state, &headers, "GET /mcp/sse") {
        return *resp;
    }

    (
        StatusCode::NOT_IMPLEMENTED,
        "mcp/sse: SSE transport not yet implemented (issue #397)",
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Shared API key guard
// ---------------------------------------------------------------------------

/// Extract and validate the API key from headers.
///
/// Returns `Ok(())` when the key is present and valid.
/// Returns `Err(impl IntoResponse)` (a 401) otherwise.
fn require_api_key(
    state: &AppState,
    headers: &HeaderMap,
    endpoint: &str,
) -> Result<(), Box<axum::response::Response>> {
    let key = match auth::extract_bearer(headers) {
        Some(k) => k,
        None => {
            warn!("{endpoint}: missing Authorization header — auth_failure");
            return Err(Box::new(
                (
                    StatusCode::UNAUTHORIZED,
                    "Authorization: Bearer <api-key> required",
                )
                    .into_response(),
            ));
        }
    };

    match auth::validate_api_key(&key, &state.api_keys) {
        Ok(_session) => Ok(()),
        Err(_) => {
            warn!("{endpoint}: invalid or unknown API key — auth_failure");
            Err(Box::new(
                (StatusCode::UNAUTHORIZED, "invalid or unknown API key").into_response(),
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{ApiKeyStore, JwtAuthority};
    use axum::body::Body;
    use axum::http::{Method, Request};
    use std::sync::Arc;
    use tower::ServiceExt;

    const TEST_SECRET: &[u8] = b"phantom-hub-test-secret-for-mcp-endpoint-tests";
    const TEST_API_KEY: &str = "phk_test-api-key-for-unit-tests";

    fn test_state_with_key(key: &str) -> crate::AppState {
        crate::AppState {
            jwt: Arc::new(JwtAuthority::from_secret(TEST_SECRET)),
            api_keys: Arc::new(ApiKeyStore::from_raw_keys(std::iter::once(key))),
            nonce_cache: Arc::new(crate::auth::NonceCache::new()),
            registry: crate::registry::new_shared(),
        }
    }

    fn test_state_no_keys() -> crate::AppState {
        crate::AppState {
            jwt: Arc::new(JwtAuthority::from_secret(TEST_SECRET)),
            api_keys: Arc::new(ApiKeyStore::default()),
            nonce_cache: Arc::new(crate::auth::NonceCache::new()),
            registry: crate::registry::new_shared(),
        }
    }

    // -----------------------------------------------------------------------
    // POST /mcp — no API key → 401
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn mcp_no_api_key_returns_401() {
        let app = crate::build_router(test_state_no_keys());

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // -----------------------------------------------------------------------
    // POST /mcp — wrong API key → 401
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn mcp_wrong_api_key_returns_401() {
        let app = crate::build_router(test_state_with_key(TEST_API_KEY));

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .header("Authorization", "Bearer phk_wrong-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // -----------------------------------------------------------------------
    // POST /mcp — valid API key → 501 (routing stub)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn mcp_valid_api_key_returns_501_stub() {
        let app = crate::build_router(test_state_with_key(TEST_API_KEY));

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/mcp")
                    .header("Authorization", format!("Bearer {TEST_API_KEY}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }

    // -----------------------------------------------------------------------
    // GET /mcp/sse — no API key → 401
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn mcp_sse_no_api_key_returns_401() {
        let app = crate::build_router(test_state_no_keys());

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/mcp/sse")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    // -----------------------------------------------------------------------
    // GET /mcp/sse — valid API key → 501 (SSE stub)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn mcp_sse_valid_api_key_returns_501_stub() {
        let app = crate::build_router(test_state_with_key(TEST_API_KEY));

        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/mcp/sse")
                    .header("Authorization", format!("Bearer {TEST_API_KEY}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    }
}
