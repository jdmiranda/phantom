//! `phantom auth` CLI surface — register / status / clear (issue #563).
//!
//! This module owns the user-facing commands that move a Phantom instance
//! from "unregistered" to "holding a hub-issued JWT in the credentials
//! store".  It is the local half of the registration handshake whose remote
//! half lives in `phantom-hub::phantom_endpoint::register`.
//!
//! # Wire predicate
//!
//! The signed challenge **must** be produced byte-for-byte the same way the
//! hub will verify it.  The hub's verifier is
//! [`phantom_hub::auth::verify_registration_signature`] which builds the
//! signed message as:
//!
//! ```text
//! msg = nonce.as_bytes() || peer_id.as_bytes()
//! ```
//!
//! (see `crates/phantom-hub/src/auth.rs` near line 575).  Any deviation —
//! reversing the order, inserting a domain-separation prefix, hashing
//! first — fails verification and the hub returns `401`.  This module
//! mirrors that predicate exactly in [`build_register_payload`] which is
//! the single place the message bytes are constructed.
//!
//! # Hex encodings on the wire
//!
//! The `RegisterRequest` schema (see
//! `crates/phantom-hub/src/phantom_endpoint.rs`) uses three hex fields:
//!
//! | Field             | Bytes hex-encoded                                          |
//! |-------------------|-----------------------------------------------------------|
//! | `public_key_hex`  | 32 raw Ed25519 public-key bytes → 64 hex chars            |
//! | `nonce_hex`       | UTF-8 bytes of the nonce string → variable hex length     |
//! | `signature_hex`   | 64 raw Ed25519 signature bytes → 128 hex chars            |
//!
//! The nonce is a freshly generated UUID-v4 string; the hub decodes its
//! UTF-8 bytes back to a string and uses it both for replay protection
//! and as the first half of the signed message.
//!
//! # Storage
//!
//! On success the JWT is written via
//! [`phantom_net::DeviceCredentials::store`] which already provides
//! atomic-create + mode-`0600` semantics (post-#555).  We do not roll our
//! own file write.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use ed25519_dalek::Signature;
use phantom_net::{DeviceCredentials, Identity};
use serde::{Deserialize, Serialize};

/// Default namespace for both the [`Identity`] file and
/// [`DeviceCredentials`] file when the user does not pass `--service`.
pub const DEFAULT_SERVICE: &str = "phantom";

// ---------------------------------------------------------------------------
// Wire types — mirror `phantom_hub::phantom_endpoint::{Register{Request,Response}}`
// ---------------------------------------------------------------------------
//
// We duplicate the wire shape rather than depending on `phantom-hub` so that
// `phantom-hub`'s heavy axum/tracing/jsonwebtoken transitive dep tree is not
// pulled into the GUI binary.  The two sides are validated against each
// other in `tests/auth_cli.rs` which imports the real hub verifier and
// asserts the signature this module produces is accepted.

#[derive(Debug, Serialize)]
struct RegisterRequest {
    peer_id: String,
    public_key_hex: String,
    nonce_hex: String,
    signature_hex: String,
}

#[derive(Debug, Deserialize)]
struct RegisterResponse {
    device_token: String,
    exp: u64,
    phantom_id: String,
}

// ---------------------------------------------------------------------------
// Payload builder — the byte-level contract with the hub
// ---------------------------------------------------------------------------

/// Inputs to the registration payload, kept separate from network concerns
/// so the byte-construction step is unit-testable in isolation.
pub struct RegisterPayload {
    pub peer_id: String,
    pub public_key_hex: String,
    pub nonce_hex: String,
    pub signature_hex: String,
}

/// Build the JSON body for `POST /auth/register` from an [`Identity`] and a
/// nonce string.
///
/// The signed message is constructed byte-for-byte the same way as the
/// hub's verifier (`phantom_hub::auth::verify_registration_signature`):
///
/// ```text
/// msg = nonce.as_bytes() || peer_id.as_bytes()
/// ```
///
/// Any deviation here causes the hub to return `401 Unauthorized`.
#[must_use]
pub fn build_register_payload(identity: &Identity, nonce: &str) -> RegisterPayload {
    let peer_id = identity.peer_id.as_str().to_owned();
    let public_key_hex = encode_hex(identity.verifying_key().as_bytes());

    // Mirror the hub predicate exactly — see auth.rs:576-578.
    let mut msg = Vec::with_capacity(nonce.len() + peer_id.len());
    msg.extend_from_slice(nonce.as_bytes());
    msg.extend_from_slice(peer_id.as_bytes());

    let signature: Signature = identity.sign(&msg);
    let signature_hex = encode_hex(&signature.to_bytes());
    let nonce_hex = encode_hex(nonce.as_bytes());

    RegisterPayload {
        peer_id,
        public_key_hex,
        nonce_hex,
        signature_hex,
    }
}

// ---------------------------------------------------------------------------
// Public commands — wired into clap subcommands in main.rs
// ---------------------------------------------------------------------------

/// `phantom auth register --hub <url> [--service <name>]`.
///
/// 1. Loads (or generates) the on-disk identity for `service`.
/// 2. Builds the registration payload and POSTs it to `<hub_url>/auth/register`.
/// 3. Persists the returned JWT via [`DeviceCredentials::store`].
/// 4. Prints peer-id, expiry (ISO-8601), and the credentials file path.
pub fn register(hub_url: &str, service: &str) -> Result<()> {
    let hub_url = hub_url.trim().trim_end_matches('/');
    if hub_url.is_empty() {
        bail!("--hub URL must not be empty");
    }

    let identity =
        Identity::load_or_generate(service).context("failed to load or generate identity")?;
    let nonce = uuid::Uuid::new_v4().to_string();
    let payload = build_register_payload(&identity, &nonce);

    let body = RegisterRequest {
        peer_id: payload.peer_id.clone(),
        public_key_hex: payload.public_key_hex,
        nonce_hex: payload.nonce_hex,
        signature_hex: payload.signature_hex,
    };

    let endpoint = format!("{hub_url}/auth/register");
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to construct HTTP client")?;

    let response = client
        .post(&endpoint)
        .json(&body)
        .send()
        .with_context(|| format!("POST {endpoint} failed"))?;

    let status = response.status();
    if !status.is_success() {
        // Body may contain a server-side reason; surface it but never the JWT.
        let server_msg = response.text().unwrap_or_default();
        bail!("hub rejected registration: {status} {server_msg}");
    }

    let parsed: RegisterResponse = response
        .json()
        .context("hub response was not valid RegisterResponse JSON")?;

    if parsed.phantom_id != payload.peer_id {
        bail!(
            "hub echoed peer_id {} but we sent {}",
            parsed.phantom_id,
            payload.peer_id
        );
    }

    DeviceCredentials::store(service, hub_url, &parsed.device_token)
        .context("failed to persist device credentials")?;

    let creds_path = credentials_path_for_display(service);
    println!("Registered with hub at {hub_url}");
    println!("  peer_id : {}", payload.peer_id);
    println!("  expires : {} (unix={})", format_expiry(parsed.exp), parsed.exp);
    println!("  stored  : {creds_path}");

    Ok(())
}

/// `phantom auth status [--service <name>]`.
///
/// Prints the stored credentials' peer-id and expiry without verifying the
/// JWT signature.  When no credentials are stored, prints a friendly
/// "not registered" message and exits successfully (status is informational,
/// not a probe).
pub fn status(service: &str) -> Result<()> {
    let Some(creds) =
        DeviceCredentials::load(service).context("failed to load credentials")?
    else {
        println!("No credentials stored for service '{service}' — run `phantom auth register --hub <url>`");
        return Ok(());
    };

    let claims = decode_jwt_claims(&creds.jwt)?;
    println!("Credentials present for service '{service}':");
    println!("  hub_url : {}", creds.hub_url);
    if let Some(peer_id) = &claims.sub {
        println!("  peer_id : {peer_id}");
    }
    println!("  expires : {} (unix={})", format_expiry(claims.exp), claims.exp);

    Ok(())
}

/// `phantom auth clear [--service <name>]`.
///
/// Deletes the on-disk credentials file.  Idempotent — succeeds even when
/// no credentials are stored, mirroring `DeviceCredentials::delete`.
pub fn clear(service: &str) -> Result<()> {
    DeviceCredentials::delete(service).context("failed to delete credentials")?;
    println!("Cleared credentials for service '{service}'");
    Ok(())
}

// ---------------------------------------------------------------------------
// JWT decoding (no verification)
// ---------------------------------------------------------------------------

/// The two JWT claims `phantom auth status` cares about.
///
/// The hub signs additional fields (issuer, audience, …) but we deliberately
/// do not import the hub's claims struct — `auth status` is a local diagnostic
/// and must work without the hub's signing key.
#[derive(Debug, Deserialize)]
struct DisplayClaims {
    /// Subject — the `peer_id` the JWT was issued for.
    #[serde(default)]
    sub: Option<String>,
    /// Expiry as a Unix timestamp (seconds).
    exp: u64,
}

fn decode_jwt_claims(jwt: &str) -> Result<DisplayClaims> {
    // Insecure decode is correct here: we are not making a trust decision
    // off these claims, only displaying them to the user.  The hub
    // re-validates on every request.
    let mut validation = jsonwebtoken::Validation::default();
    validation.insecure_disable_signature_validation();
    validation.validate_exp = false;
    validation.required_spec_claims.clear();

    let dummy_key = jsonwebtoken::DecodingKey::from_secret(b"unused");
    let data = jsonwebtoken::decode::<DisplayClaims>(jwt, &dummy_key, &validation)
        .map_err(|e| anyhow!("could not decode JWT for display: {e}"))?;
    Ok(data.claims)
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn encode_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(hex_digit(b >> 4));
        s.push(hex_digit(b & 0x0f));
    }
    s
}

fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        10..=15 => (b'a' + (nibble - 10)) as char,
        _ => unreachable!("nibble out of range: {nibble}"),
    }
}

fn format_expiry(unix_ts: u64) -> String {
    use chrono::{DateTime, Utc};
    let secs = i64::try_from(unix_ts).unwrap_or(i64::MAX);
    DateTime::<Utc>::from_timestamp(secs, 0)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| format!("unix={unix_ts}"))
}

/// Best-effort display path for the credentials file.
///
/// The real storage logic in `phantom_net::credentials` honours
/// `PHANTOM_CREDENTIALS_FILE` first, then falls back to
/// `{config_dir}/phantom/credentials/{service}.json`.  We replicate that
/// resolution here for display only — `DeviceCredentials::store` is what
/// actually performs the write.
fn credentials_path_for_display(service: &str) -> String {
    if let Some(p) = std::env::var_os("PHANTOM_CREDENTIALS_FILE") {
        return std::path::PathBuf::from(p).display().to_string();
    }
    let base = dirs::config_dir().or_else(dirs::home_dir);
    match base {
        Some(p) => p
            .join("phantom")
            .join("credentials")
            .join(format!("{service}.json"))
            .display()
            .to_string(),
        None => format!("<config_dir>/phantom/credentials/{service}.json"),
    }
}

// ---------------------------------------------------------------------------
// Pure unit tests — hex round-trip only
//
// Tests that need an `Identity` live in `tests/auth_cli.rs` so they can use
// the `PHANTOM_IDENTITY_FILE` env-override against a tempfile.  Doing that
// from in-crate `cfg(test)` is risky because env vars are process-global
// and would clobber other in-crate tests in the same binary.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_hex_lower_case_zero_padded() {
        assert_eq!(encode_hex(&[]), "");
        assert_eq!(encode_hex(&[0x00]), "00");
        assert_eq!(encode_hex(&[0xab, 0xcd, 0xef]), "abcdef");
        assert_eq!(encode_hex(&[0x01, 0x0f, 0xf0]), "010ff0");
    }

    #[test]
    fn format_expiry_handles_well_known_timestamp() {
        // 2026-01-01T00:00:00Z = 1767225600
        let s = format_expiry(1767225600);
        assert!(s.starts_with("2026-01-01T"), "got {s}");
    }
}
