//! Device credentials — JWT device token paired with the hub URL.
//!
//! After `phantom auth register --hub <url>` succeeds the hub issues a JWT.
//! This module persists that token in the OS keyring alongside the signing
//! [`Identity`] so that every subsequent startup can load and present it
//! without a network call.
//!
//! # Keyring layout
//!
//! | service key                        | account     | value                          |
//! |------------------------------------|-------------|--------------------------------|
//! | `phantom-net/device-creds/default` | `jwt`       | raw JWT string                 |
//! | `phantom-net/device-creds/default` | `hub-url`   | hub WebSocket URL              |
//!
//! The `namespace` parameter lets QA / dev isolate their credentials from
//! each other and from production.
//!
//! # Threat model
//!
//! The JWT is an opaque bearer token.  Whoever holds it can impersonate this
//! Phantom to the hub until the token expires (30-day window per issue #398).
//! Storing it in the OS keyring is substantially safer than a plain config
//! file; it is protected by the OS credential store's ACL/keychain permissions.
//! The hub URL is not secret but is stored alongside the JWT for convenience.
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

use anyhow::{Context, Result};
use keyring::Entry;

// ---------------------------------------------------------------------------
// Keyring helpers
// ---------------------------------------------------------------------------

fn jwt_entry(namespace: &str) -> Result<Entry> {
    let service = format!("phantom-net/device-creds/{namespace}");
    Entry::new(&service, "jwt").context("failed to open JWT keyring entry")
}

fn hub_url_entry(namespace: &str) -> Result<Entry> {
    let service = format!("phantom-net/device-creds/{namespace}");
    Entry::new(&service, "hub-url").context("failed to open hub-url keyring entry")
}

// ---------------------------------------------------------------------------
// DeviceCredentials
// ---------------------------------------------------------------------------

/// A pair of (hub URL, JWT device token) stored in the OS keyring.
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
    /// Persist a newly received JWT and hub URL to the OS keyring.
    ///
    /// Overwrites any previously stored credentials for `namespace`.
    ///
    /// `namespace` must match the `Identity` namespace used for this instance
    /// (usually `"phantom"` for production, a unique string for tests).
    ///
    /// # Errors
    ///
    /// Returns an error if the keyring is unavailable or write fails.
    pub fn store(namespace: &str, hub_url: &str, jwt: &str) -> Result<()> {
        jwt_entry(namespace)?
            .set_password(jwt)
            .context("failed to persist JWT to keyring")?;
        hub_url_entry(namespace)?
            .set_password(hub_url)
            .context("failed to persist hub URL to keyring")?;
        Ok(())
    }

    /// Load previously persisted credentials from the OS keyring.
    ///
    /// Returns `Ok(None)` when no credentials have been stored yet.
    ///
    /// # Errors
    ///
    /// Returns an error if the keyring is unavailable or the stored bytes are
    /// corrupt.
    pub fn load(namespace: &str) -> Result<Option<Self>> {
        let jwt = match jwt_entry(namespace)?.get_password() {
            Ok(s) => s,
            Err(keyring::Error::NoEntry) => return Ok(None),
            Err(e) => anyhow::bail!("keyring error reading JWT: {e}"),
        };
        let hub_url = match hub_url_entry(namespace)?.get_password() {
            Ok(s) => s,
            Err(keyring::Error::NoEntry) => return Ok(None),
            Err(e) => anyhow::bail!("keyring error reading hub URL: {e}"),
        };
        Ok(Some(Self { hub_url, jwt }))
    }

    /// Delete stored credentials from the OS keyring.
    ///
    /// Returns `Ok(())` even when no credentials were stored.
    ///
    /// # Errors
    ///
    /// Returns an error if the keyring is unavailable or delete fails
    /// for a reason other than "no entry".
    pub fn delete(namespace: &str) -> Result<()> {
        let jwt_del = jwt_entry(namespace)?.delete_credential();
        let hub_del = hub_url_entry(namespace)?.delete_credential();

        // Treat NoEntry as a success — idempotent delete.
        match jwt_del {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(e) => anyhow::bail!("failed to delete JWT keyring entry: {e}"),
        }
        match hub_del {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(e) => anyhow::bail!("failed to delete hub-url keyring entry: {e}"),
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_ns() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        format!("phantom-test-creds-{ns}")
    }

    #[test]
    fn store_and_load_round_trip() {
        let ns = unique_ns();
        let hub_url = "wss://hub.example.com";
        let jwt = "eyJ.test.token";

        DeviceCredentials::store(&ns, hub_url, jwt).expect("store must succeed");
        let loaded = DeviceCredentials::load(&ns)
            .expect("load must succeed")
            .expect("credentials must be present after store");

        assert_eq!(loaded.hub_url, hub_url);
        assert_eq!(loaded.jwt, jwt);

        // Cleanup.
        DeviceCredentials::delete(&ns).expect("delete must succeed");
    }

    #[test]
    fn load_returns_none_when_not_stored() {
        let ns = unique_ns();
        let result = DeviceCredentials::load(&ns).expect("load must not error");
        assert!(
            result.is_none(),
            "load must return None when nothing is stored"
        );
    }

    #[test]
    fn delete_is_idempotent() {
        let ns = unique_ns();
        // Deleting when nothing is stored must not error.
        DeviceCredentials::delete(&ns).expect("delete on empty keyring must be OK");
    }

    #[test]
    fn store_overwrites_previous() {
        let ns = unique_ns();
        DeviceCredentials::store(&ns, "wss://v1.example.com", "jwt-v1").unwrap();
        DeviceCredentials::store(&ns, "wss://v2.example.com", "jwt-v2").unwrap();

        let loaded = DeviceCredentials::load(&ns).unwrap().unwrap();
        assert_eq!(loaded.hub_url, "wss://v2.example.com");
        assert_eq!(loaded.jwt, "jwt-v2");

        DeviceCredentials::delete(&ns).unwrap();
    }
}
