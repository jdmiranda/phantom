//! Authentication — device token and API key verification.
//!
//! # SCAFFOLD — issue #398 fills this in
//!
//! Phase 1: type definitions only. No tokens are validated; every extraction
//! returns a placeholder. Issue #398 wires real JWT verification and key
//! lookup.
//!
//! # Token model (planned)
//!
//! Two principal types:
//!
//! - **Device token** — long-lived JWT issued to a Phantom instance at
//!   registration. Presented in `Authorization: Bearer <token>` on the
//!   `GET /phantom/connect` WebSocket upgrade.
//!
//! - **API key** — short-lived or static key issued to a Claude session
//!   (operator). Presented in `X-Api-Key: <key>` or
//!   `Authorization: Bearer <key>` on MCP endpoints.

use axum::http::HeaderMap;

/// The authenticated identity of a Phantom device.
///
/// SCAFFOLD: fields are present but `phantom_id` is always a placeholder.
/// Issue #398 populates from a verified JWT claim.
#[derive(Debug, Clone)]
pub struct DeviceIdentity {
    /// Phantom identifier, matches `PhantomId` in the registry.
    pub phantom_id: String,
    // TODO(#398): add `issued_at: std::time::SystemTime`
    // TODO(#398): add `scopes: Vec<String>`
}

/// The authenticated identity of a Claude session (MCP caller).
///
/// SCAFFOLD: fields are present but unvalidated. Issue #398 adds lookup.
#[derive(Debug, Clone)]
pub struct SessionIdentity {
    /// Opaque API key string (will become a key-id reference post-#398).
    pub api_key: String,
    // TODO(#398): add `allowed_phantom_ids: Option<Vec<String>>` for scoped access
}

/// Extract a bearer token from `Authorization` header.
///
/// SCAFFOLD: returns the raw header value, no signature verification.
/// Issue #398 adds JWT parsing and validation.
pub fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get("Authorization")?.to_str().ok()?;
    value.strip_prefix("Bearer ").map(str::to_owned)
}

/// Validate a device token and return the identity.
///
/// SCAFFOLD: always returns `Err` — no validation yet. Issue #398 wires
/// Ed25519/JWT verification.
pub fn validate_device_token(_token: &str) -> Result<DeviceIdentity, AuthError> {
    Err(AuthError::NotImplemented)
}

/// Validate an API key and return the session identity.
///
/// SCAFFOLD: always returns `Err` — no lookup yet. Issue #398 wires
/// the key store.
pub fn validate_api_key(_key: &str) -> Result<SessionIdentity, AuthError> {
    Err(AuthError::NotImplemented)
}

/// Authentication errors.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("not implemented (issue #398)")]
    NotImplemented,
    #[error("missing or malformed Authorization header")]
    MissingToken,
    #[error("token signature invalid")]
    InvalidSignature,
    #[error("token expired")]
    Expired,
    #[error("unknown API key")]
    UnknownKey,
}
