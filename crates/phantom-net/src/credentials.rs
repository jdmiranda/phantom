//! Device credentials — JWT device token paired with the hub URL.
//!
//! After `phantom auth register --hub <url>` succeeds the hub issues a JWT.
//! This module persists that token in a file alongside the signing
//! [`crate::identity::Identity`] so that every subsequent startup can load
//! and present it without a network call.
//!
//! # Why a file, not the OS keyring
//! The same code-signature ACL problem that plagued [`Identity`] applies
//! here — every fresh `cargo build` is treated as a different requesting
//! app on macOS and "Always Allow" never sticks.  Storing the token in a
//! `0600`-mode JSON file under the user's config directory sidesteps the
//! prompt-spam.
//!
//! # Storage layout
//! Default path: `{config_dir}/phantom/credentials/{namespace}.json`.
//! Override via `PHANTOM_CREDENTIALS_FILE` (used by tests).
//!
//! # Threat model
//! The JWT is an opaque bearer token.  Whoever holds it can impersonate this
//! Phantom to the hub until the token expires (30-day window per issue #398).
//! `0600` restricts access to the current user, which is the same trust
//! boundary as the macOS Keychain login keychain in practice.  The hub URL
//! is not secret but is stored alongside the JWT for convenience.
//!
//! # Example
//! ```rust,no_run
//! use phantom_net::credentials::DeviceCredentials;
//!
//! // Store after a successful `phantom auth register` call.
//! DeviceCredentials::store("phantom", "wss://hub.example.com", "eyJ...").unwrap();
//!
//! // Load on every startup.
//! if let Some(creds) = DeviceCredentials::load("phantom").unwrap() {
//!     println!("hub: {}, jwt len: {}", creds.hub_url, creds.jwt.len());
//! }
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

const CREDENTIALS_FILE_ENV: &str = "PHANTOM_CREDENTIALS_FILE";

fn credentials_path(namespace: &str) -> Result<PathBuf> {
    if let Some(override_path) = std::env::var_os(CREDENTIALS_FILE_ENV) {
        return Ok(PathBuf::from(override_path));
    }
    let base = dirs::config_dir()
        .or_else(dirs::home_dir)
        .context("could not determine config or home directory for credentials storage")?;
    Ok(base
        .join("phantom")
        .join("credentials")
        .join(format!("{namespace}.json")))
}

// ---------------------------------------------------------------------------
// On-disk format
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize)]
struct CredentialsFile {
    hub_url: String,
    jwt: String,
}

// ---------------------------------------------------------------------------
// DeviceCredentials
// ---------------------------------------------------------------------------

/// A pair of (hub URL, JWT device token) stored on disk.
///
/// This is separate from [`crate::identity::Identity`] so that `Identity`
/// remains pure (keypair only) and `DeviceCredentials` carries the
/// registration result.
#[derive(Debug, Clone)]
pub struct DeviceCredentials {
    /// The hub WebSocket URL this token was issued for.
    pub hub_url: String,
    /// The raw JWT string.  Do not log this value.
    pub jwt: String,
}

impl DeviceCredentials {
    /// Persist a newly received JWT and hub URL to disk.
    ///
    /// Overwrites any previously stored credentials for `namespace`.
    /// The file is written atomically (`*.tmp` + rename) and chmod'd `0600`
    /// on Unix before any bytes are flushed.
    ///
    /// `namespace` must match the `Identity` namespace used for this instance
    /// (usually `"phantom"` for production, a unique string for tests).
    ///
    /// # Errors
    /// Returns an error if the file cannot be written.
    pub fn store(namespace: &str, hub_url: &str, jwt: &str) -> Result<()> {
        let path = credentials_path(namespace)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create credentials directory at {}",
                    parent.display()
                )
            })?;
        }
        let payload = CredentialsFile {
            hub_url: hub_url.to_owned(),
            jwt: jwt.to_owned(),
        };
        let json = serde_json::to_vec_pretty(&payload).context("failed to serialise credentials")?;
        write_atomic(&path, &json)
    }

    /// Load previously persisted credentials from disk.
    ///
    /// Returns `Ok(None)` when no credentials have been stored yet (file
    /// does not exist).  A malformed file surfaces as `Err` and is **not**
    /// silently overwritten.
    ///
    /// # Errors
    /// Returns an error if the file is present but unreadable or malformed.
    pub fn load(namespace: &str) -> Result<Option<Self>> {
        let path = credentials_path(namespace)?;
        if !path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&path)
            .with_context(|| format!("failed to read credentials file at {}", path.display()))?;
        let file: CredentialsFile = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "credentials file at {} is malformed JSON",
                path.display()
            )
        })?;
        Ok(Some(Self {
            hub_url: file.hub_url,
            jwt: file.jwt,
        }))
    }

    /// Delete stored credentials from disk.
    ///
    /// Returns `Ok(())` even when no credentials were stored — idempotent.
    ///
    /// # Errors
    /// Returns an error if the file exists but cannot be removed.
    pub fn delete(namespace: &str) -> Result<()> {
        let path = credentials_path(namespace)?;
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| {
                format!("failed to delete credentials file at {}", path.display())
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Atomic write (mirrors identity::write_atomic)
// ---------------------------------------------------------------------------

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;

    // Per-(pid, thread) tmp suffix so concurrent writers cannot trample
    // each other's tmp file.
    let tid = std::thread::current().id();
    let suffix = format!("{}.{:?}.tmp", std::process::id(), tid);
    let tmp_path = match path.extension() {
        Some(ext) => {
            let mut s = ext.to_os_string();
            s.push(".");
            s.push(&suffix);
            path.with_extension(s)
        }
        None => path.with_extension(&suffix),
    };

    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)
            .with_context(|| {
                format!(
                    "failed to open tmp credentials file at {}",
                    tmp_path.display()
                )
            })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&tmp_path, perms).with_context(|| {
                format!(
                    "failed to set 0600 mode on tmp credentials file at {}",
                    tmp_path.display()
                )
            })?;
        }

        f.write_all(bytes).with_context(|| {
            format!("failed to write credentials to {}", tmp_path.display())
        })?;
        f.sync_all()
            .with_context(|| format!("failed to fsync credentials at {}", tmp_path.display()))?;
    }

    std::fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "failed to rename {} -> {}",
            tmp_path.display(),
            path.display()
        )
    })?;

    #[cfg(unix)]
    if let Some(parent) = path.parent()
        && let Ok(dir) = std::fs::File::open(parent)
    {
        let _ = dir.sync_all();
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Serialises tests that mutate the `PHANTOM_CREDENTIALS_FILE` env var.
    static ENV_SERIAL: Mutex<()> = Mutex::new(());
    static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn unique_ns() -> String {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        format!("phantom-test-creds-{pid}-{n}")
    }

    fn tmp_creds_file() -> PathBuf {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("phantom-creds-test-{pid}-{n}.json"))
    }

    /// Cleanup guard — removes env var and tmp file on drop.
    struct CleanupGuard {
        path: PathBuf,
    }
    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            // SAFETY: env mutation is serialised via ENV_SERIAL.
            unsafe { std::env::remove_var(CREDENTIALS_FILE_ENV) };
            let _ = std::fs::remove_file(&self.path);
        }
    }

    #[test]
    fn store_and_load_round_trip() {
        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_creds_file();
        unsafe { std::env::set_var(CREDENTIALS_FILE_ENV, &path) };
        let _cleanup = CleanupGuard { path: path.clone() };

        let ns = unique_ns();
        let hub_url = "wss://hub.example.com";
        let jwt = "eyJ.test.token";

        DeviceCredentials::store(&ns, hub_url, jwt).expect("store must succeed");
        let loaded = DeviceCredentials::load(&ns)
            .expect("load must succeed")
            .expect("credentials must be present after store");

        assert_eq!(loaded.hub_url, hub_url);
        assert_eq!(loaded.jwt, jwt);
    }

    #[test]
    fn load_returns_none_when_not_stored() {
        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_creds_file();
        unsafe { std::env::set_var(CREDENTIALS_FILE_ENV, &path) };
        let _cleanup = CleanupGuard { path: path.clone() };

        let ns = unique_ns();
        let result = DeviceCredentials::load(&ns).expect("load must not error");
        assert!(
            result.is_none(),
            "load must return None when nothing is stored"
        );
    }

    #[test]
    fn delete_is_idempotent() {
        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_creds_file();
        unsafe { std::env::set_var(CREDENTIALS_FILE_ENV, &path) };
        let _cleanup = CleanupGuard { path: path.clone() };

        let ns = unique_ns();
        DeviceCredentials::delete(&ns).expect("delete on missing file must be OK");
    }

    #[test]
    fn store_overwrites_previous() {
        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_creds_file();
        unsafe { std::env::set_var(CREDENTIALS_FILE_ENV, &path) };
        let _cleanup = CleanupGuard { path: path.clone() };

        let ns = unique_ns();
        DeviceCredentials::store(&ns, "wss://v1.example.com", "jwt-v1").unwrap();
        DeviceCredentials::store(&ns, "wss://v2.example.com", "jwt-v2").unwrap();

        let loaded = DeviceCredentials::load(&ns).unwrap().unwrap();
        assert_eq!(loaded.hub_url, "wss://v2.example.com");
        assert_eq!(loaded.jwt, "jwt-v2");
    }

    #[cfg(unix)]
    #[test]
    fn store_creates_0600_file_on_unix() {
        use std::os::unix::fs::PermissionsExt;

        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_creds_file();
        unsafe { std::env::set_var(CREDENTIALS_FILE_ENV, &path) };
        let _cleanup = CleanupGuard { path: path.clone() };

        let ns = unique_ns();
        DeviceCredentials::store(&ns, "wss://hub.example.com", "jwt-test").unwrap();

        let meta = std::fs::metadata(&path).expect("credentials file must exist");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "credentials file must be mode 0600, got {mode:o}");
    }

    #[test]
    fn corrupted_file_returns_error_no_silent_overwrite() {
        let _serial = ENV_SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let path = tmp_creds_file();
        unsafe { std::env::set_var(CREDENTIALS_FILE_ENV, &path) };
        let _cleanup = CleanupGuard { path: path.clone() };

        std::fs::write(&path, b"not valid json {{{").expect("seed corrupted file");
        let before = std::fs::read(&path).unwrap();

        let ns = unique_ns();
        let result = DeviceCredentials::load(&ns);
        assert!(result.is_err(), "corrupted file must surface as Err");

        let after = std::fs::read(&path).unwrap();
        assert_eq!(before, after, "corrupted file must not be overwritten");
    }
}
