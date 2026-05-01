//! Integration tests for `phantom auth` (issue #563).
//!
//! These tests live outside the bin module so they can:
//!   - import `phantom::auth_cli` through the small lib surface in `lib.rs`,
//!   - import `phantom_hub::auth::verify_registration_signature` to
//!     cross-validate the wire signature against the actual hub verifier,
//!   - drive `PHANTOM_IDENTITY_FILE` and `PHANTOM_CREDENTIALS_FILE` against
//!     per-test tempfiles so we never touch the user's real config dir.
//!
//! Env mutation is process-global so every test that touches an env var
//! holds the `ENV_SERIAL` mutex for its duration.  The mutex is poison-tolerant.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use phantom::auth_cli;
use phantom_net::{DeviceCredentials, Identity};

static ENV_SERIAL: Mutex<()> = Mutex::new(());
static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Unique service name per test so the per-process Identity cache cannot
/// leak state between cases.
fn unique_service() -> String {
    let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    format!("phantom-563-{pid}-{n}")
}

fn tmp_path(label: &str) -> PathBuf {
    let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("phantom-563-{label}-{pid}-{n}"))
}

/// RAII guard that restores env state and removes a tempfile on drop.
/// Cleanup runs even if the test panics.
struct EnvGuard {
    var: &'static str,
    file: PathBuf,
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: env mutation is serialised by ENV_SERIAL.
        unsafe { std::env::remove_var(self.var) };
        let _ = std::fs::remove_file(&self.file);
    }
}

fn set_identity_override(path: &PathBuf) -> EnvGuard {
    // SAFETY: env mutation is serialised by ENV_SERIAL.
    unsafe { std::env::set_var("PHANTOM_IDENTITY_FILE", path) };
    EnvGuard {
        var: "PHANTOM_IDENTITY_FILE",
        file: path.clone(),
    }
}

fn set_credentials_override(path: &PathBuf) -> EnvGuard {
    unsafe { std::env::set_var("PHANTOM_CREDENTIALS_FILE", path) };
    EnvGuard {
        var: "PHANTOM_CREDENTIALS_FILE",
        file: path.clone(),
    }
}

// ---------------------------------------------------------------------------
// 1. Signature contract — wire payload must verify against the hub's verifier
// ---------------------------------------------------------------------------

#[test]
fn register_payload_includes_correct_signature() {
    let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let id_file = tmp_path("id");
    let _id_guard = set_identity_override(&id_file);

    let service = unique_service();
    let identity = Identity::load_or_generate(&service).expect("identity must load");

    let nonce = "uuid-v4-style-nonce-AB12cd";
    let payload = auth_cli::build_register_payload(&identity, nonce);

    // Decode the hex fields back into raw bytes — these are the exact
    // bytes the hub will see on the wire after `hex_decode_exact`/`hex_decode_vec`.
    let pubkey_bytes = decode_hex(&payload.public_key_hex).expect("pubkey hex");
    let signature_bytes = decode_hex(&payload.signature_hex).expect("sig hex");
    let nonce_bytes = decode_hex(&payload.nonce_hex).expect("nonce hex");
    let nonce_str = std::str::from_utf8(&nonce_bytes).expect("nonce decodes to UTF-8");
    assert_eq!(nonce_str, nonce, "nonce hex must round-trip via UTF-8");

    // The byte-level contract: hand exactly what the hub will hand its
    // verifier and assert success.  This is the critical guard.
    phantom_hub::auth::verify_registration_signature(
        &payload.peer_id,
        nonce_str,
        &pubkey_bytes,
        &signature_bytes,
    )
    .expect("hub verifier MUST accept the CLI-built signature");

    // Sanity: a different nonce must NOT verify under the same signature.
    let bad = phantom_hub::auth::verify_registration_signature(
        &payload.peer_id,
        "different-nonce",
        &pubkey_bytes,
        &signature_bytes,
    );
    assert!(
        bad.is_err(),
        "verifier must reject signature when the nonce changes"
    );

    // Sanity: a different peer_id must NOT verify either.
    let bad2 = phantom_hub::auth::verify_registration_signature(
        "not-the-real-peer-id",
        nonce_str,
        &pubkey_bytes,
        &signature_bytes,
    );
    assert!(
        bad2.is_err(),
        "verifier must reject signature when the peer_id changes"
    );
}

// ---------------------------------------------------------------------------
// 2. status with no credentials prints "not registered" and exits Ok
// ---------------------------------------------------------------------------

#[test]
fn status_reports_no_credentials_when_file_missing() {
    let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let creds_file = tmp_path("creds");
    let _creds_guard = set_credentials_override(&creds_file);
    assert!(
        !creds_file.exists(),
        "precondition: creds file must not exist"
    );

    let service = unique_service();
    // status returns Ok(()) even when nothing is stored — it's a probe.
    auth_cli::status(&service).expect("status must succeed when nothing is stored");
}

// ---------------------------------------------------------------------------
// 3. clear removes the credentials file (and is idempotent)
// ---------------------------------------------------------------------------

#[test]
fn clear_removes_credentials_file() {
    let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let creds_file = tmp_path("creds");
    let _creds_guard = set_credentials_override(&creds_file);

    let service = unique_service();
    DeviceCredentials::store(&service, "https://hub.example.com", "fake-jwt")
        .expect("seed credentials");
    assert!(creds_file.exists(), "credentials file must exist after store");

    auth_cli::clear(&service).expect("clear must succeed");
    assert!(
        !creds_file.exists(),
        "credentials file must be removed after clear"
    );

    // Idempotent: a second clear is a no-op.
    auth_cli::clear(&service).expect("second clear must also succeed");
}

// ---------------------------------------------------------------------------
// 4. register against an in-process axum mock writes credentials on 200
// ---------------------------------------------------------------------------

#[test]
fn register_writes_credentials_on_200_response() {
    use axum::Json;
    use axum::Router;
    use axum::routing::post;

    let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
    let id_file = tmp_path("id");
    let creds_file = tmp_path("creds");
    let _id_guard = set_identity_override(&id_file);
    let _creds_guard = set_credentials_override(&creds_file);

    let service = unique_service();
    // Pre-load the identity so we know the peer_id the hub-mock should echo.
    let identity = Identity::load_or_generate(&service).expect("identity must load");
    let expected_peer_id = identity.peer_id.as_str().to_owned();
    drop(identity);

    // Build a tokio runtime to run the hub mock.  Spawn the mock on a fresh
    // background thread — the `register` call we make below uses
    // `reqwest::blocking`, so we cannot share a runtime with it.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.set_nonblocking(true).expect("set nonblocking");
    let local_addr = listener.local_addr().expect("local addr");
    let hub_url = format!("http://{local_addr}");

    // `auth_cli::register` only stores the JWT — it never decodes it.
    // A simple opaque string is sufficient for this test.
    let mock_jwt = "mock-device-jwt-issued-by-test-hub".to_owned();
    let mock_jwt_for_handler = mock_jwt.clone();
    let echo_peer_id = expected_peer_id.clone();

    let server_thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        rt.block_on(async move {
            let app = Router::new().route(
                "/auth/register",
                post(move |Json(_body): Json<serde_json::Value>| {
                    let device_token = mock_jwt_for_handler.clone();
                    let phantom_id = echo_peer_id.clone();
                    async move {
                        Json(serde_json::json!({
                            "device_token": device_token,
                            "exp": 4_102_444_800u64,
                            "phantom_id": phantom_id,
                        }))
                    }
                }),
            );
            let listener = tokio::net::TcpListener::from_std(listener).expect("from_std");
            // Serve a single connection then exit.
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    // Stay alive long enough for the test client to finish.
                    tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                })
                .await;
        });
    });

    // Give the server a beat to actually bind.
    std::thread::sleep(std::time::Duration::from_millis(50));

    auth_cli::register(&hub_url, &service).expect("register must succeed against the mock hub");

    // The credentials file must now exist and contain the JWT we returned.
    let loaded = DeviceCredentials::load(&service)
        .expect("load must succeed")
        .expect("credentials must be present after register");
    assert_eq!(loaded.jwt, mock_jwt, "stored JWT must match server response");
    assert!(
        loaded.hub_url.starts_with("http://127.0.0.1:"),
        "stored hub URL must match the mock hub: got {}",
        loaded.hub_url
    );

    // Best-effort cleanup — let the server's graceful_shutdown timeout run out.
    drop(server_thread);
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn decode_hex(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("odd hex length".into());
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for chunk in s.as_bytes().chunks(2) {
        let hi = from_hex_digit(chunk[0])?;
        let lo = from_hex_digit(chunk[1])?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn from_hex_digit(b: u8) -> Result<u8, String> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        _ => Err(format!("bad hex digit: {}", b as char)),
    }
}

